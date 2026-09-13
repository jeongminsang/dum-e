use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub const ANTHROPIC_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const ANTHROPIC_AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
pub const ANTHROPIC_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const OPENAI_CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const OPENAI_CODEX_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const OPENAI_CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const DEFAULT_CALLBACK_PORT: u16 = 53692;
const DEVICE_BASE: &str = "https://auth.openai.com";

pub struct OAuthProviderConfig {
    pub client_id: &'static str,
    pub auth_url: &'static str,
    pub token_url: &'static str,
    pub scope: &'static str,
    pub port: u16,
    pub callback_path: &'static str,
}

impl OAuthProviderConfig {
    pub fn redirect_uri(&self) -> String {
        format!("http://localhost:{}{}", self.port, self.callback_path)
    }
}

pub fn get_oauth_config(provider: &str) -> Option<OAuthProviderConfig> {
    match provider {
        "anthropic" => Some(OAuthProviderConfig {
            client_id: ANTHROPIC_CLIENT_ID,
            auth_url: ANTHROPIC_AUTHORIZE_URL,
            token_url: ANTHROPIC_TOKEN_URL,
            scope: "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload",
            port: DEFAULT_CALLBACK_PORT,
            callback_path: "/callback",
        }),
        "openai-codex" => Some(OAuthProviderConfig {
            client_id: OPENAI_CODEX_CLIENT_ID,
            auth_url: OPENAI_CODEX_AUTHORIZE_URL,
            token_url: OPENAI_CODEX_TOKEN_URL,
            scope: "openid profile email offline_access",
            port: 1455,
            callback_path: "/auth/callback",
        }),
        _ => None,
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct OAuthTokenResponse {
    pub access_token: String,
    pub token_type: Option<String>,
    pub expires_in: Option<i64>,
    pub refresh_token: Option<String>,
    pub scope: Option<String>,
}

pub struct PkceChallenge {
    pub verifier: String,
    pub challenge: String,
}

pub fn generate_state() -> Result<String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("OS randomness unavailable"))?;
    Ok(hex::encode(bytes))
}

pub fn generate_pkce() -> Result<PkceChallenge> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("OS randomness unavailable"))?;
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = compute_code_challenge(&verifier);
    Ok(PkceChallenge {
        verifier,
        challenge,
    })
}

pub fn compute_code_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub fn build_authorization_url(
    config: &OAuthProviderConfig,
    state: &str,
    challenge: &str,
) -> Result<String> {
    let mut url = Url::parse(config.auth_url)?;
    url.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", config.client_id),
        ("redirect_uri", &config.redirect_uri()),
        ("scope", config.scope),
        ("state", state),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
    ]);
    if config.client_id == ANTHROPIC_CLIENT_ID {
        url.query_pairs_mut().append_pair("code", "true");
    } else {
        url.query_pairs_mut().extend_pairs([
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("originator", "pi"),
        ]);
    }
    Ok(url.into())
}

fn authorization_code(url: &Url, expected_state: &str) -> Result<String> {
    let pairs: Vec<_> = url.query_pairs().collect();
    let values = |name: &str| {
        pairs
            .iter()
            .filter(move |(k, _)| k == name)
            .map(|(_, v)| v.as_ref())
            .collect::<Vec<_>>()
    };
    let state = values("state");
    anyhow::ensure!(state == [expected_state], "OAuth state mismatch");
    anyhow::ensure!(values("error").is_empty(), "OAuth authorization denied");
    let codes = values("code");
    anyhow::ensure!(
        codes.len() == 1 && !codes[0].is_empty(),
        "Missing or duplicate authorization code"
    );
    Ok(codes[0].to_owned())
}

pub fn parse_authorization_input(input: &str, state: &str) -> Result<String> {
    let input = input.trim();
    if let Ok(url) = Url::parse(input) {
        return authorization_code(&url, state);
    }
    if let Some((code, supplied_state)) = input.split_once('#') {
        anyhow::ensure!(
            supplied_state == state && !code.is_empty(),
            "Invalid authorization input"
        );
        return Ok(code.to_owned());
    }
    if input.contains("code=") {
        let url = Url::parse(&format!("http://localhost/?{input}"))
            .map_err(|_| anyhow::anyhow!("Invalid authorization input"))?;
        return authorization_code(&url, state);
    }
    anyhow::ensure!(
        !input.is_empty() && !input.chars().any(char::is_whitespace),
        "Missing authorization code"
    );
    Ok(input.to_owned())
}

/// Bind before displaying the authorization URL. Dropping the wait future closes the listener.
pub async fn bind_oauth_callback(port: u16) -> Result<TcpListener> {
    TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .context("Unable to bind OAuth callback listener")
}

pub async fn wait_for_oauth_callback(
    listener: TcpListener,
    path: &str,
    state: &str,
) -> Result<String> {
    tokio::time::timeout(Duration::from_secs(300), async {
        loop {
            let (mut stream, _) = listener.accept().await?;
            let request = tokio::time::timeout(Duration::from_secs(2), async {
                let mut bytes = Vec::new();
                loop {
                    let byte = stream.read_u8().await?;
                    bytes.push(byte);
                    if bytes.ends_with(b"\r\n\r\n") || bytes.len() >= 8192 { break; }
                }
                Ok::<_, std::io::Error>(bytes)
            }).await;
            let Ok(Ok(bytes)) = request else { continue; };
            if !bytes.ends_with(b"\r\n\r\n") { continue; }
            let text = String::from_utf8_lossy(&bytes);
            let mut parts = text.lines().next().unwrap_or_default().split_whitespace();
            let method = parts.next().unwrap_or_default();
            let target = parts.next().unwrap_or_default();
            let version = parts.next().unwrap_or_default();
            let url = Url::parse(&format!("http://localhost{target}"));
            let result = match url {
                Ok(url) if method == "GET" && matches!(version, "HTTP/1.0" | "HTTP/1.1")
                    && target.split('?').next() == Some(path) && url.path() == path => authorization_code(&url, state),
                _ => Err(anyhow::anyhow!("Unrelated callback request")),
            };
            let status = if result.is_ok() { "200 OK" } else { "400 Bad Request" };
            let body = if result.is_ok() { "Authentication Successful. Return to DUM-E." } else { "Invalid OAuth callback." };
            let response = format!("HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            let _ = tokio::time::timeout(Duration::from_secs(2), stream.write_all(response.as_bytes())).await;
            if let Ok(code) = result { return Ok(code); }
        }
    }).await.context("OAuth callback timed out")?
}

fn client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| anyhow::anyhow!("Unable to create OAuth client"))
}

async fn token_request(
    config: &OAuthProviderConfig,
    params: &[(&str, &str)],
) -> Result<OAuthTokenResponse> {
    let request = client()?
        .post(config.token_url)
        .header("Accept", "application/json");
    let request = if config.client_id == ANTHROPIC_CLIENT_ID {
        request.json(
            &params
                .iter()
                .copied()
                .collect::<std::collections::HashMap<_, _>>(),
        )
    } else {
        request.form(params)
    };
    let response = request
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("OAuth token request failed"))?;
    anyhow::ensure!(
        response.status().is_success(),
        "OAuth token request rejected (HTTP {})",
        response.status().as_u16()
    );
    let token: OAuthTokenResponse = response
        .json()
        .await
        .map_err(|_| anyhow::anyhow!("Invalid OAuth token response"))?;
    anyhow::ensure!(
        !token.access_token.trim().is_empty()
            && token
                .expires_in
                .is_some_and(|v| v > 0 && v <= i64::MAX / 1000),
        "Incomplete OAuth token response"
    );
    anyhow::ensure!(
        token
            .refresh_token
            .as_ref()
            .is_none_or(|v| !v.trim().is_empty()),
        "Invalid OAuth refresh token"
    );
    Ok(token)
}

pub async fn exchange_code_for_token(
    config: &OAuthProviderConfig,
    code: &str,
    redirect: &str,
    verifier: &str,
    state: &str,
) -> Result<OAuthTokenResponse> {
    let mut params = vec![
        ("grant_type", "authorization_code"),
        ("client_id", config.client_id),
        ("code", code),
        ("redirect_uri", redirect),
        ("code_verifier", verifier),
    ];
    if config.client_id == ANTHROPIC_CLIENT_ID {
        params.push(("state", state));
    }
    let token = token_request(config, &params).await?;
    anyhow::ensure!(
        token.refresh_token.is_some(),
        "OAuth login response missing refresh token"
    );
    Ok(token)
}

pub async fn refresh_oauth_token(
    config: &OAuthProviderConfig,
    refresh: &str,
) -> Result<OAuthTokenResponse> {
    token_request(
        config,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", config.client_id),
            ("refresh_token", refresh),
        ],
    )
    .await
}

/// Decode routing metadata only; this does not verify JWT authenticity.
pub fn extract_account_id(token: &str) -> Option<String> {
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(parts[1].trim_end_matches('='))
        .ok()?;
    let payload: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    payload
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}

pub async fn login_codex_device(notify: impl FnOnce(&str, &str)) -> Result<OAuthTokenResponse> {
    device_login(DEVICE_BASE, OPENAI_CODEX_TOKEN_URL, notify).await
}

async fn device_login(
    base: &str,
    token_url: &str,
    notify: impl FnOnce(&str, &str),
) -> Result<OAuthTokenResponse> {
    tokio::time::timeout(Duration::from_secs(900), async {
        let client = client()?;
        let response = client
            .post(format!("{base}/api/accounts/deviceauth/usercode"))
            .json(&serde_json::json!({"client_id": OPENAI_CODEX_CLIENT_ID}))
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Device authorization request failed"))?;
        anyhow::ensure!(
            response.status().is_success(),
            "Device authorization unavailable (HTTP {})",
            response.status().as_u16()
        );
        let info: serde_json::Value = response
            .json()
            .await
            .map_err(|_| anyhow::anyhow!("Invalid device authorization response"))?;
        let id = info["device_auth_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("Missing device authorization ID")?;
        let code = info["user_code"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("Missing device user code")?;
        let interval = info["interval"]
            .as_f64()
            .or_else(|| info["interval"].as_str()?.parse().ok())
            .context("Missing device polling interval")?;
        anyhow::ensure!(
            interval.is_finite() && (0.0..=900.0).contains(&interval),
            "Invalid device polling interval"
        );
        let mut interval = interval.max(1.0);
        notify(&format!("{base}/codex/device"), code);
        loop {
            tokio::time::sleep(Duration::from_secs_f64(interval)).await;
            let response = client
                .post(format!("{base}/api/accounts/deviceauth/token"))
                .json(&serde_json::json!({"device_auth_id": id, "user_code": code}))
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("Device polling request failed"))?;
            let status = response.status();
            if status.as_u16() == 403 || status.as_u16() == 404 {
                continue;
            }
            let body: serde_json::Value = response
                .json()
                .await
                .map_err(|_| anyhow::anyhow!("Invalid device polling response"))?;
            if status.is_success() {
                let code = body["authorization_code"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .context("Missing device authorization code")?;
                let verifier = body["code_verifier"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .context("Missing device verifier")?;
                let config = get_oauth_config("openai-codex").unwrap();
                // The test endpoint is borrowed, while production configuration is static.
                let params = [
                    ("grant_type", "authorization_code"),
                    ("client_id", config.client_id),
                    ("code", code),
                    ("code_verifier", verifier),
                    (
                        "redirect_uri",
                        "https://auth.openai.com/deviceauth/callback",
                    ),
                ];
                let response = client
                    .post(token_url)
                    .form(&params)
                    .send()
                    .await
                    .map_err(|_| anyhow::anyhow!("Device token exchange failed"))?;
                anyhow::ensure!(
                    response.status().is_success(),
                    "Device token exchange rejected"
                );
                let token: OAuthTokenResponse = response
                    .json()
                    .await
                    .map_err(|_| anyhow::anyhow!("Invalid device token response"))?;
                anyhow::ensure!(
                    !token.access_token.is_empty()
                        && token.refresh_token.as_ref().is_some_and(|s| !s.is_empty())
                        && token.expires_in.is_some_and(|v| v > 0),
                    "Incomplete device token response"
                );
                return Ok(token);
            }
            match body["error"]
                .as_str()
                .or_else(|| body["error"]["code"].as_str())
            {
                Some("deviceauth_authorization_pending" | "authorization_pending") => {}
                Some("slow_down") => interval = (interval + 5.0).min(900.0),
                _ => anyhow::bail!("Device authorization rejected (HTTP {})", status.as_u16()),
            }
        }
    })
    .await
    .context("Device authorization timed out")?
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn mock_response(
        status: u16,
        body: &'static str,
    ) -> (&'static str, tokio::task::JoinHandle<String>) {
        let listener = bind_oauth_callback(0).await.unwrap();
        let url =
            Box::leak(format!("http://{}/token", listener.local_addr().unwrap()).into_boxed_str());
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                bytes.push(stream.read_u8().await.unwrap());
            }
            let headers = String::from_utf8(bytes.clone()).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            let mut body_bytes = vec![0; length];
            stream.read_exact(&mut body_bytes).await.unwrap();
            bytes.extend(body_bytes);
            stream.write_all(format!("HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            String::from_utf8(bytes).unwrap()
        });
        (url, task)
    }

    #[tokio::test]
    async fn exchange_and_refresh_use_provider_specific_encoding() {
        for provider in ["anthropic", "openai-codex"] {
            let (url, request) = mock_response(
                200,
                r#"{"access_token":"access","refresh_token":"refresh","expires_in":3600}"#,
            )
            .await;
            let mut config = get_oauth_config(provider).unwrap();
            config.token_url = url;
            exchange_code_for_token(
                &config,
                "code+value",
                &config.redirect_uri(),
                "verifier",
                "state",
            )
            .await
            .unwrap();
            let request = request.await.unwrap();
            let body = request.split_once("\r\n\r\n").unwrap().1;
            if provider == "anthropic" {
                assert!(request.contains("application/json"));
                let body: serde_json::Value = serde_json::from_str(body).unwrap();
                assert_eq!(body["state"], "state");
                assert_eq!(body["code"], "code+value");
            } else {
                assert!(request.contains("application/x-www-form-urlencoded"));
                let params = Url::parse(&format!("http://localhost/?{body}")).unwrap();
                assert!(params.query_pairs().any(
                    |(k, v)| k == "redirect_uri" && v == "http://localhost:1455/auth/callback"
                ));
                assert!(!params.query_pairs().any(|(k, _)| k == "state"));
            }
            let (url, request) =
                mock_response(200, r#"{"access_token":"rotated","expires_in":3600}"#).await;
            config.token_url = url;
            let token = refresh_oauth_token(&config, "old+refresh").await.unwrap();
            assert!(token.refresh_token.is_none());
            let request = request.await.unwrap();
            if provider == "anthropic" {
                let body: serde_json::Value =
                    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
                assert_eq!(body["grant_type"], "refresh_token");
                assert_eq!(body["refresh_token"], "old+refresh");
            } else {
                assert!(request.contains("refresh_token=old%2Brefresh"));
            }
        }
    }

    #[tokio::test]
    async fn rejection_does_not_disclose_response_secrets() {
        let (url, request) =
            mock_response(401, r#"{"error":"secret-access-and-refresh-token"}"#).await;
        let mut config = get_oauth_config("anthropic").unwrap();
        config.token_url = url;
        let error = refresh_oauth_token(&config, "secret-refresh")
            .await
            .err()
            .unwrap();
        assert!(!format!("{error:#}").contains("secret"));
        request.await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_callback_releases_port() {
        let listener = bind_oauth_callback(0).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task =
            tokio::spawn(async move { wait_for_oauth_callback(listener, "/callback", "s").await });
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(bind_oauth_callback(port).await.is_ok());
    }

    #[tokio::test]
    async fn device_login_polls_pending_then_exchanges_code() {
        let listener = bind_oauth_callback(0).await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in [
                (
                    200,
                    r#"{"device_auth_id":"device","user_code":"user","interval":"0"}"#,
                ),
                (403, r#"{}"#),
                (
                    200,
                    r#"{"authorization_code":"code","code_verifier":"verifier"}"#,
                ),
                (
                    200,
                    r#"{"access_token":"access","refresh_token":"refresh","expires_in":3600}"#,
                ),
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                while !bytes.ends_with(b"\r\n\r\n") {
                    bytes.push(stream.read_u8().await.unwrap());
                }
                let headers = String::from_utf8(bytes.clone()).unwrap();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                let mut body_bytes = vec![0; length];
                stream.read_exact(&mut body_bytes).await.unwrap();
                bytes.extend(body_bytes);
                requests.push(String::from_utf8(bytes).unwrap());
                stream.write_all(format!("HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
            requests
        });
        let token = device_login(&base, &format!("{base}/oauth/token"), |url, code| {
            assert!(url.ends_with("/codex/device"));
            assert_eq!(code, "user");
        })
        .await
        .unwrap();
        assert_eq!(token.access_token, "access");
        let requests = server.await.unwrap();
        assert!(requests[0].starts_with("POST /api/accounts/deviceauth/usercode "));
        assert!(requests[1].starts_with("POST /api/accounts/deviceauth/token "));
        assert!(requests[3].starts_with("POST /oauth/token "));
        let url = Url::parse(&format!(
            "http://localhost/?{}",
            requests[3].split_once("\r\n\r\n").unwrap().1
        ))
        .unwrap();
        assert!(url.query_pairs().any(
            |(k, v)| k == "redirect_uri" && v == "https://auth.openai.com/deviceauth/callback"
        ));
    }

    #[test]
    fn pkce_and_protocol_parameters() {
        assert_eq!(
            compute_code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let a = generate_pkce().unwrap();
        assert_eq!(a.verifier.len(), 43);
        assert_ne!(a.verifier, generate_pkce().unwrap().verifier);
        let config = get_oauth_config("anthropic").unwrap();
        let url = Url::parse(&build_authorization_url(&config, &a.verifier, &a.challenge).unwrap())
            .unwrap();
        assert!(url.query_pairs().any(|(k, v)| k == "code" && v == "true"));
        assert!(config.scope.contains("user:file_upload"));
        assert!(get_oauth_config("openai").is_none());
        assert_eq!(
            get_oauth_config("openai-codex").unwrap().redirect_uri(),
            "http://localhost:1455/auth/callback"
        );
    }

    #[test]
    fn manual_state_and_metadata() {
        assert_eq!(
            parse_authorization_input("http://localhost/callback?state=s&code=a%2Bb", "s").unwrap(),
            "a+b"
        );
        assert!(parse_authorization_input("code=a&state=wrong", "s").is_err());
        assert!(parse_authorization_input("code=a&state=s&state=s", "s").is_err());
        let payload = URL_SAFE_NO_PAD
            .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct"}}"#);
        assert_eq!(
            extract_account_id(&format!("x.{payload}.x")).as_deref(),
            Some("acct")
        );
        assert!(extract_account_id("not-a-jwt").is_none());
    }

    #[tokio::test]
    async fn callback_survives_unrelated_and_wrong_state_requests() {
        let listener = bind_oauth_callback(0).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task =
            tokio::spawn(
                async move { wait_for_oauth_callback(listener, "/auth/callback", "s").await },
            );
        let client = Client::new();
        for path in ["/favicon.ico", "/auth/callback?state=x&code=a"] {
            assert_eq!(
                client
                    .get(format!("http://127.0.0.1:{port}{path}"))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                400
            );
        }
        assert_eq!(
            client
                .post(format!(
                    "http://127.0.0.1:{port}/auth/callback?state=s&code=a"
                ))
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        client
            .get(format!(
                "http://127.0.0.1:{port}/auth/callback?state=s&code=a%2Bb"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(task.await.unwrap().unwrap(), "a+b");
    }
}

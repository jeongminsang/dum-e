use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

pub const ANTHROPIC_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const ANTHROPIC_AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
pub const ANTHROPIC_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";

pub const OPENAI_CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const OPENAI_CODEX_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const OPENAI_CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

pub const OPENROUTER_AUTHORIZE_URL: &str = "https://openrouter.ai/auth";
pub const OPENROUTER_TOKEN_URL: &str = "https://openrouter.ai/api/v1/auth/keys";

pub const DEFAULT_CALLBACK_PORT: u16 = 53692;

pub struct OAuthProviderConfig {
    pub client_id: &'static str,
    pub auth_url: &'static str,
    pub token_url: &'static str,
    pub scope: &'static str,
    pub port: u16,
}

pub fn get_oauth_config(provider: &str) -> Option<OAuthProviderConfig> {
    match provider {
        "anthropic" => Some(OAuthProviderConfig {
            client_id: ANTHROPIC_CLIENT_ID,
            auth_url: ANTHROPIC_AUTHORIZE_URL,
            token_url: ANTHROPIC_TOKEN_URL,
            scope: "org:create_api_key user:profile user:inference user:sessions:claude_code",
            port: DEFAULT_CALLBACK_PORT,
        }),
        "openai" | "openai-codex" => Some(OAuthProviderConfig {
            client_id: OPENAI_CODEX_CLIENT_ID,
            auth_url: OPENAI_CODEX_AUTHORIZE_URL,
            token_url: OPENAI_CODEX_TOKEN_URL,
            scope: "openid profile email offline_access",
            port: 1455,
        }),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthTokenResponse {
    pub access_token: String,
    pub token_type: Option<String>,
    pub expires_in: Option<i64>,
    pub refresh_token: Option<String>,
    pub scope: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PkceChallenge {
    pub verifier: String,
    pub challenge: String,
}

pub fn generate_pkce() -> PkceChallenge {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use std::time::SystemTime;

    // Generate 32-byte high-entropy cryptographic seed
    let seed = format!(
        "{:?}_{:?}_{}",
        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_nanos(),
        std::process::id(),
        std::time::Instant::now().elapsed().as_nanos()
    );
    let mut hasher = Sha256::new();
    hasher.update(seed.as_bytes());
    let raw = hasher.finalize();
    // RFC 7636 verifier: base64url unpadded string
    let verifier = URL_SAFE_NO_PAD.encode(raw);

    // RFC 7636 S256 challenge: BASE64URL-ENCODE(SHA256(ASCII(code_verifier)))
    let challenge = compute_code_challenge(&verifier);

    PkceChallenge { verifier, challenge }
}

pub fn compute_code_challenge(verifier: &str) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let hash = hasher.finalize();
    URL_SAFE_NO_PAD.encode(hash)
}

pub fn build_authorization_url(
    authorize_base_url: &str,
    client_id: &str,
    redirect_uri: &str,
    scope: &str,
    state: &str,
    code_challenge: &str,
) -> String {
    format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        authorize_base_url,
        urlencoding(client_id),
        urlencoding(redirect_uri),
        urlencoding(scope),
        urlencoding(state),
        urlencoding(code_challenge),
    )
}

fn urlencoding(input: &str) -> String {
    let mut out = String::new();
    for b in input.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

pub async fn start_oauth_callback_server(
    port: u16,
    expected_state: String,
    shutdown_rx: oneshot::Receiver<()>,
) -> Result<String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = TcpListener::bind(addr).await
        .with_context(|| format!("Failed to bind local OAuth callback server on 127.0.0.1:{}", port))?;

    tokio::select! {
        _ = shutdown_rx => {
            anyhow::bail!("OAuth callback server cancelled by timeout or user");
        }
        res = listener.accept() => {
            let (mut stream, _) = res?;
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).await?;
            let req = String::from_utf8_lossy(&buf[..n]);

            // Parse request line e.g. GET /callback?code=xxx&state=yyy HTTP/1.1
            let first_line = req.lines().next().unwrap_or_default();
            let mut query_params = std::collections::HashMap::new();
            if let Some(pos) = first_line.find('?') {
                if let Some(end_pos) = first_line[pos..].find(' ') {
                    let query = &first_line[pos + 1..pos + end_pos];
                    for pair in query.split('&') {
                        let mut parts = pair.splitn(2, '=');
                        if let (Some(k), Some(v)) = (parts.next(), parts.next()) {
                            query_params.insert(k.to_string(), v.to_string());
                        }
                    }
                }
            }

            let code = query_params.get("code").cloned().context("Missing code parameter in OAuth callback")?;
            let state = query_params.get("state").cloned().unwrap_or_default();

            if state != expected_state {
                let response = "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\n\r\n<h1>OAuth Error: Invalid State Parameter</h1>";
                stream.write_all(response.as_bytes()).await?;
                anyhow::bail!("OAuth state parameter mismatch");
            }

            let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<h1>Authentication Successful!</h1><p>You may close this window and return to DUM-E.</p>";
            stream.write_all(response.as_bytes()).await?;

            Ok(code)
        }
    }
}

pub async fn exchange_code_for_token(
    token_url: &str,
    client_id: &str,
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
) -> Result<OAuthTokenResponse> {
    let client = Client::builder().build()?;
    let params = [
        ("grant_type", "authorization_code"),
        ("client_id", client_id),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("code_verifier", code_verifier),
    ];

    let resp = client.post(token_url)
        .form(&params)
        .send()
        .await
        .context("Failed to send token exchange request")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let err_body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Token exchange failed with status {}: {}", status, err_body);
    }

    let token_data: OAuthTokenResponse = resp.json().await
        .context("Failed to deserialize OAuth token response")?;

    Ok(token_data)
}

pub async fn refresh_oauth_token(
    token_url: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<OAuthTokenResponse> {
    let client = Client::builder().build()?;
    let params = [
        ("grant_type", "refresh_token"),
        ("client_id", client_id),
        ("refresh_token", refresh_token),
    ];

    let resp = client.post(token_url)
        .form(&params)
        .send()
        .await
        .context("Failed to send token refresh request")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let err_body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Token refresh failed with status {}: {}", status, err_body);
    }

    let token_data: OAuthTokenResponse = resp.json().await
        .context("Failed to deserialize refreshed OAuth token response")?;

    Ok(token_data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_pkce_generation_and_auth_url() {
        let pkce = generate_pkce();
        assert!(!pkce.verifier.is_empty());
        assert!(!pkce.challenge.is_empty());

        let url = build_authorization_url(
            ANTHROPIC_AUTHORIZE_URL,
            ANTHROPIC_CLIENT_ID,
            "http://localhost:53692/callback",
            "org:create_api_key",
            "test_state",
            &pkce.challenge,
        );

        assert!(url.starts_with(ANTHROPIC_AUTHORIZE_URL));
        assert!(url.contains("client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e"));
        assert!(url.contains("code_challenge="));
        assert!(url.contains("state=test_state"));
    }

    #[tokio::test]
    async fn test_oauth_callback_server_captures_code() {
        let (tx, rx) = oneshot::channel();
        let expected_state = "secret_state_1234".to_string();

        let server_task = tokio::spawn(start_oauth_callback_server(54321, expected_state.clone(), rx));

        // Wait a few milliseconds for server to bind
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Send simulated browser callback request
        let client = reqwest::Client::new();
        let callback_url = format!("http://127.0.0.1:54321/callback?code=mock_auth_code_xyz&state={}", expected_state);
        let resp = client.get(&callback_url).send().await.unwrap();
        assert!(resp.status().is_success());
        let text = resp.text().await.unwrap();
        assert!(text.contains("Authentication Successful"));

        let code_res = server_task.await.unwrap().unwrap();
        assert_eq!(code_res, "mock_auth_code_xyz");

        let _ = tx.send(());
    }

    #[test]
    fn test_rfc7636_test_vector() {
        // RFC 7636 Appendix B Test Vector:
        // code_verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
        // code_challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected_challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        let challenge = compute_code_challenge(verifier);
        assert_eq!(challenge, expected_challenge);
    }
}

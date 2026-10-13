use serde::{Deserialize, Serialize};
use std::fmt;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassifierBackend {
    #[serde(rename = "opencode")]
    OpenCodeZen,
    LocalSystemOne,
}

impl ClassifierBackend {
    pub fn credential_provider(&self) -> &'static str {
        match self {
            Self::OpenCodeZen => "opencode",
            Self::LocalSystemOne => "jev-local-system-one",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassifierConfig {
    pub backend: ClassifierBackend,
    pub base_url: String,
    pub model: String,
}

impl ClassifierConfig {
    pub fn validate(&self) -> Result<(), ClassifierError> {
        if self.model.trim().is_empty() {
            return Err(ClassifierError::new(
                ClassifierErrorKind::InvalidConfiguration,
            ));
        }
        let url = reqwest::Url::parse(self.base_url.trim())
            .map_err(|_| ClassifierError::new(ClassifierErrorKind::InvalidConfiguration))?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.host_str().is_none()
        {
            return Err(ClassifierError::new(
                ClassifierErrorKind::InvalidConfiguration,
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassifierUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Classification {
    pub complex_probability: f64,
    pub usage: Option<ClassifierUsage>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClassifierErrorKind {
    InvalidConfiguration,
    MissingCredential,
    HttpStatus(u16),
    Timeout,
    Cancelled,
    Transport,
    InvalidResponse,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ClassifierError {
    pub kind: ClassifierErrorKind,
    pub usage: Option<ClassifierUsage>,
}

impl ClassifierError {
    fn new(kind: ClassifierErrorKind) -> Self {
        Self { kind, usage: None }
    }
    pub fn reason_code(&self) -> &'static str {
        match &self.kind {
            ClassifierErrorKind::InvalidConfiguration => "invalid_configuration",
            ClassifierErrorKind::MissingCredential => "missing_credential",
            ClassifierErrorKind::HttpStatus(_) => "http_status",
            ClassifierErrorKind::Timeout => "timeout",
            ClassifierErrorKind::Cancelled => "cancelled",
            ClassifierErrorKind::Transport => "transport",
            ClassifierErrorKind::InvalidResponse => "invalid_response",
        }
    }
}

impl fmt::Debug for ClassifierError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClassifierError")
            .field("reason", &self.reason_code())
            .field("usage", &self.usage)
            .finish()
    }
}
impl fmt::Display for ClassifierError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason_code())
    }
}
impl std::error::Error for ClassifierError {}

pub struct SystemOneClassifier {
    config: ClassifierConfig,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl fmt::Debug for SystemOneClassifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SystemOneClassifier")
            .field("config", &self.config)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

impl SystemOneClassifier {
    pub fn new(
        config: &ClassifierConfig,
        api_key: Option<String>,
    ) -> Result<Self, ClassifierError> {
        config.validate()?;
        if config.backend == ClassifierBackend::OpenCodeZen
            && api_key.as_deref().is_none_or(|k| k.trim().is_empty())
        {
            return Err(ClassifierError::new(ClassifierErrorKind::MissingCredential));
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| ClassifierError::new(ClassifierErrorKind::Transport))?;
        Ok(Self {
            config: config.clone(),
            api_key,
            client,
        })
    }

    pub async fn classify(
        &self,
        prompt: &str,
        cancellation: &CancellationToken,
    ) -> Result<Classification, ClassifierError> {
        if cancellation.is_cancelled() {
            return Err(ClassifierError::new(ClassifierErrorKind::Cancelled));
        }
        let mut url = reqwest::Url::parse(self.config.base_url.trim())
            .map_err(|_| ClassifierError::new(ClassifierErrorKind::InvalidConfiguration))?;
        url.path_segments_mut()
            .map_err(|_| ClassifierError::new(ClassifierErrorKind::InvalidConfiguration))?
            .pop_if_empty()
            .push("systemone");
        let prompt: String = prompt.chars().take(16_000).collect();
        let body = serde_json::json!({
            "model": self.config.model,
            "state": {"prompt": prompt},
            "questions": {"complexity": {
                "type": "choice",
                "instructions": "How demanding is the software engineering work requested in `prompt`?",
                "criteria": {"standard": "Ordinary features, fixes, reviews, or questions", "complex": "Subtle design, cross-cutting changes, or hard debugging"}
            }}
        });
        let mut request = self
            .client
            .post(url)
            .timeout(std::time::Duration::from_secs(5))
            .json(&body);
        if let Some(key) = self.api_key.as_deref().filter(|k| !k.trim().is_empty()) {
            request = request.bearer_auth(key);
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let response = tokio::select! { biased;
            _ = cancellation.cancelled() => return Err(ClassifierError::new(ClassifierErrorKind::Cancelled)),
            result = request.send() => match result {
                Err(error) if error.is_timeout() => return Err(ClassifierError::new(ClassifierErrorKind::Timeout)),
                Err(_) => return Err(ClassifierError::new(ClassifierErrorKind::Transport)),
                Ok(response) => response,
            }
        };
        let status = response.status();
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
        loop {
            let next = tokio::select! { biased;
                _ = cancellation.cancelled() => return Err(ClassifierError::new(ClassifierErrorKind::Cancelled)),
                _ = tokio::time::sleep_until(deadline) => return Err(ClassifierError::new(ClassifierErrorKind::Timeout)),
                chunk = futures_util::StreamExt::next(&mut stream) => chunk,
            };
            match next {
                Some(Ok(chunk)) => {
                    if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                        if !status.is_success() {
                            return Err(http_error(status.as_u16(), None));
                        }
                        return Err(ClassifierError::new(ClassifierErrorKind::InvalidResponse));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Some(Err(error)) => {
                    if !status.is_success() {
                        return Err(http_error(status.as_u16(), None));
                    }
                    return Err(ClassifierError::new(if error.is_timeout() {
                        ClassifierErrorKind::Timeout
                    } else {
                        ClassifierErrorKind::Transport
                    }));
                }
                None => break,
            }
        }
        let value = serde_json::from_slice::<serde_json::Value>(&bytes).ok();
        let usage = value.as_ref().and_then(|v| parse_usage(v.get("usage")));
        if !status.is_success() {
            return Err(http_error(status.as_u16(), usage));
        }
        let value =
            value.ok_or_else(|| ClassifierError::new(ClassifierErrorKind::InvalidResponse))?;
        parse_response(&value, usage)
    }
}

fn http_error(status: u16, usage: Option<ClassifierUsage>) -> ClassifierError {
    ClassifierError {
        kind: if status == 401 || status == 403 {
            ClassifierErrorKind::MissingCredential
        } else {
            ClassifierErrorKind::HttpStatus(status)
        },
        usage,
    }
}

fn parse_usage(value: Option<&serde_json::Value>) -> Option<ClassifierUsage> {
    let v = value?;
    Some(ClassifierUsage {
        input_tokens: u64::try_from(v.get("input_tokens")?.as_u64()?).ok()?,
        output_tokens: u64::try_from(v.get("output_tokens")?.as_u64()?).ok()?,
    })
}

fn probability(value: Option<&serde_json::Value>) -> Option<f64> {
    let n = value?.as_f64()?;
    (n.is_finite() && (0.0..=1.0).contains(&n)).then_some(n)
}

fn parse_response(
    value: &serde_json::Value,
    usage: Option<ClassifierUsage>,
) -> Result<Classification, ClassifierError> {
    let answer = value.get("answers").and_then(|v| v.get("complexity"));
    if answer.and_then(|v| v.get("type")).and_then(|v| v.as_str()) != Some("choice")
        || !matches!(
            answer
                .and_then(|v| v.get("choice"))
                .and_then(|v| v.as_str()),
            Some("standard" | "complex")
        )
    {
        return Err(ClassifierError {
            kind: ClassifierErrorKind::InvalidResponse,
            usage,
        });
    }
    let probs = answer.and_then(|v| v.get("probabilities"));
    let standard = probability(probs.and_then(|v| v.get("standard")));
    let complex = probability(probs.and_then(|v| v.get("complex")));
    let confidence = probability(answer.and_then(|v| v.get("confidence")));
    let (Some(standard), Some(complex), Some(_)) = (standard, complex, confidence) else {
        return Err(ClassifierError {
            kind: ClassifierErrorKind::InvalidResponse,
            usage,
        });
    };
    if (standard + complex - 1.0).abs() > 1e-6 {
        return Err(ClassifierError {
            kind: ClassifierErrorKind::InvalidResponse,
            usage,
        });
    }
    Ok(Classification {
        complex_probability: complex,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn server(status: &str, response: &str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = response.to_owned();
        let status = status.to_owned();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 4096];
            let body_start;
            let content_length;
            loop {
                let n = stream.read(&mut chunk).await.unwrap();
                assert!(n > 0, "request ended before headers were complete");
                request.extend_from_slice(&chunk[..n]);
                if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    body_start = pos + 4;
                    let headers = String::from_utf8_lossy(&request[..pos]);
                    content_length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap();
                    break;
                }
            }
            while request.len() - body_start < content_length {
                let n = stream.read(&mut chunk).await.unwrap();
                assert!(n > 0, "request body ended early");
                request.extend_from_slice(&chunk[..n]);
            }
            let request = String::from_utf8(request).unwrap();
            let body = response.as_bytes();
            let header = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes()).await;
            let _ = stream.write_all(body).await;
            request
        });
        (format!("http://{addr}/v1/"), task)
    }

    fn config(base_url: String) -> ClassifierConfig {
        ClassifierConfig {
            backend: ClassifierBackend::OpenCodeZen,
            base_url,
            model: "user-model".into(),
        }
    }
    const GOOD: &str = r#"{"answers":{"complexity":{"type":"choice","choice":"complex","probabilities":{"standard":0.2,"complex":0.8},"confidence":0.8}},"usage":{"input_tokens":12,"output_tokens":3}}"#;

    #[test]
    fn backend_serde_names_match_public_contract() {
        assert_eq!(
            ClassifierBackend::OpenCodeZen.credential_provider(),
            "opencode"
        );
        assert_eq!(
            ClassifierBackend::LocalSystemOne.credential_provider(),
            "jev-local-system-one"
        );
        assert_eq!(
            serde_json::to_string(&ClassifierBackend::OpenCodeZen).unwrap(),
            "\"opencode\""
        );
        assert_eq!(
            serde_json::to_string(&ClassifierBackend::LocalSystemOne).unwrap(),
            "\"local_system_one\""
        );
        assert_eq!(
            serde_json::from_str::<ClassifierBackend>("\"opencode\"").unwrap(),
            ClassifierBackend::OpenCodeZen
        );
        assert_eq!(
            serde_json::from_str::<ClassifierBackend>("\"local_system_one\"").unwrap(),
            ClassifierBackend::LocalSystemOne
        );
    }

    #[tokio::test]
    async fn sends_systemone_path_bearer_and_contract_body() {
        let (url, task) = server("200 OK", GOOD).await;
        let classifier = SystemOneClassifier::new(&config(url), Some("secret-key".into())).unwrap();
        let result = classifier
            .classify("prompt", &CancellationToken::new())
            .await
            .unwrap();
        let request = task.await.unwrap();
        assert!(request.starts_with("POST /v1/systemone HTTP/1.1"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer secret-key")
        );
        assert!(request.contains("\"model\":\"user-model\""));
        assert!(request.contains("\"complexity\""));
        assert_eq!(result.complex_probability, 0.8);
        assert_eq!(result.usage.unwrap().input_tokens, 12);
    }

    #[tokio::test]
    async fn pre_cancel_does_not_send_and_cancel_during_request_wins() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let classifier = SystemOneClassifier::new(&config(url), Some("k".into())).unwrap();
        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(
            classifier.classify("x", &token).await.unwrap_err().kind,
            ClassifierErrorKind::Cancelled
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
        drop(listener);
    }

    #[tokio::test]
    async fn cancellation_during_request_preempts_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 2048];
            let _ = stream.read(&mut buffer).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{")
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        });
        let classifier =
            SystemOneClassifier::new(&config(format!("http://{address}/v1")), Some("k".into()))
                .unwrap();
        let token = CancellationToken::new();
        let cancel = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            cancel.cancel();
        });
        assert_eq!(
            classifier.classify("x", &token).await.unwrap_err().kind,
            ClassifierErrorKind::Cancelled
        );
        server.abort();
    }

    #[tokio::test]
    async fn http_errors_are_classified_without_retry_and_preserve_usage() {
        for (status, expected) in [
            ("401 Unauthorized", ClassifierErrorKind::MissingCredential),
            (
                "500 Internal Server Error",
                ClassifierErrorKind::HttpStatus(500),
            ),
        ] {
            let (url, task) = server(status, GOOD).await;
            let classifier = SystemOneClassifier::new(&config(url), Some("k".into())).unwrap();
            let error = classifier
                .classify("x", &CancellationToken::new())
                .await
                .unwrap_err();
            assert_eq!(error.kind, expected);
            assert_eq!(error.usage.unwrap().input_tokens, 12);
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn plain_http_error_bodies_preserve_status() {
        for (status, expected) in [
            ("401 Unauthorized", ClassifierErrorKind::MissingCredential),
            (
                "500 Internal Server Error",
                ClassifierErrorKind::HttpStatus(500),
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buffer = [0; 4096];
                let _ = stream.read(&mut buffer).await;
                let text = "not-json secret response";
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            });
            let classifier =
                SystemOneClassifier::new(&config(format!("http://{address}/v1")), Some("k".into()))
                    .unwrap();
            assert_eq!(
                classifier
                    .classify("x", &CancellationToken::new())
                    .await
                    .unwrap_err()
                    .kind,
                expected
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn oversized_success_response_is_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 4096];
            let _ = stream.read(&mut buffer).await;
            let body = vec![b'x'; 1024 * 1024 + 1];
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
        });
        let classifier =
            SystemOneClassifier::new(&config(format!("http://{address}/v1")), Some("k".into()))
                .unwrap();
        assert_eq!(
            classifier
                .classify("x", &CancellationToken::new())
                .await
                .unwrap_err()
                .kind,
            ClassifierErrorKind::InvalidResponse
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn redirect_is_not_followed() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 4096];
            let _ = stream.read(&mut buffer).await;
            stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: /other\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
        });
        let classifier =
            SystemOneClassifier::new(&config(format!("http://{address}/v1")), Some("k".into()))
                .unwrap();
        assert_eq!(
            classifier
                .classify("x", &CancellationToken::new())
                .await
                .unwrap_err()
                .kind,
            ClassifierErrorKind::HttpStatus(302)
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn timeout_is_distinguished() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 2048];
            let _ = stream.read(&mut buffer).await;
            tokio::time::sleep(std::time::Duration::from_secs(6)).await;
        });
        let classifier =
            SystemOneClassifier::new(&config(format!("http://{address}/v1")), Some("k".into()))
                .unwrap();
        assert_eq!(
            classifier
                .classify("x", &CancellationToken::new())
                .await
                .unwrap_err()
                .kind,
            ClassifierErrorKind::Timeout
        );
        server.abort();
    }

    #[tokio::test]
    async fn timeout_covers_a_body_that_stalls_after_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 4096];
            let _ = stream.read(&mut buffer).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{")
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(6)).await;
        });
        let classifier =
            SystemOneClassifier::new(&config(format!("http://{address}/v1")), Some("k".into()))
                .unwrap();
        assert_eq!(
            classifier
                .classify("x", &CancellationToken::new())
                .await
                .unwrap_err()
                .kind,
            ClassifierErrorKind::Timeout
        );
        server.abort();
    }

    #[test]
    fn validates_choices_probability_boundaries_and_malformed_usage() {
        let value: serde_json::Value = serde_json::from_str(GOOD).unwrap();
        assert_eq!(
            parse_response(&value, None).unwrap().complex_probability,
            0.8
        );
        let mut boundary = value.clone();
        boundary["answers"]["complexity"]["probabilities"]["standard"] = 0.0.into();
        boundary["answers"]["complexity"]["probabilities"]["complex"] = 1.0.into();
        assert!(parse_response(&boundary, None).is_ok());
        boundary["answers"]["complexity"]["choice"] = "other".into();
        assert!(parse_response(&boundary, None).is_err());
        assert!(
            parse_usage(Some(
                &serde_json::json!({"input_tokens": "bad", "output_tokens": 5})
            ))
            .is_none()
        );
        assert_eq!(
            parse_usage(Some(
                &serde_json::json!({"input_tokens": 1, "output_tokens": 2})
            ))
            .unwrap()
            .output_tokens,
            2
        );
    }

    #[test]
    fn errors_and_debug_never_reveal_secret() {
        let error = ClassifierError::new(ClassifierErrorKind::Transport);
        assert!(!format!("{error:?} {error}").contains("secret-key"));
        let classifier = SystemOneClassifier::new(
            &config("http://localhost/v1".into()),
            Some("secret-key".into()),
        )
        .unwrap();
        assert!(!format!("{classifier:?}").contains("secret-key"));
    }

    #[tokio::test]
    async fn prompt_limit_is_character_safe() {
        let input = "한".repeat(16_001);
        let (url, task) = server("200 OK", GOOD).await;
        let classifier = SystemOneClassifier::new(&config(url), Some("k".into())).unwrap();
        classifier
            .classify(&input, &CancellationToken::new())
            .await
            .unwrap();
        let request = task.await.unwrap();
        let body = request.split_once("\r\n\r\n").unwrap().1;
        let value: serde_json::Value = serde_json::from_str(body).unwrap();
        let prompt = value["state"]["prompt"].as_str().unwrap();
        assert_eq!(prompt.chars().count(), 16_000);
        assert!(prompt.chars().all(|ch| ch == '한'));
    }
}

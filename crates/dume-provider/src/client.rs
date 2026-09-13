use anyhow::{Context, Result};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use std::time::Duration;

#[derive(Clone)]
pub struct LlmClient {
    http: Client,
}

impl Default for LlmClient {
    fn default() -> Self {
        Self::new()
    }
}

impl LlmClient {
    pub fn new() -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self { http }
    }

    pub async fn post_with_retry(
        &self,
        url: &str,
        headers: HeaderMap,
        body: &Value,
        max_retries: usize,
    ) -> Result<reqwest::Response> {
        let mut attempts = 0;
        let mut delay = Duration::from_millis(500);

        loop {
            attempts += 1;
            let resp = self
                .http
                .post(url)
                .headers(headers.clone())
                .json(body)
                .send()
                .await;

            match resp {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        return Ok(response);
                    } else if (status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
                        && attempts <= max_retries
                    {
                        // Check Retry-After header
                        if let Some(retry_after) = response.headers().get("Retry-After") {
                            if let Ok(secs) = retry_after.to_str().unwrap_or("").parse::<u64>() {
                                tokio::time::sleep(Duration::from_secs(secs)).await;
                                continue;
                            }
                        }
                        tokio::time::sleep(delay).await;
                        delay *= 2;
                        continue;
                    } else {
                        let text = response.text().await.unwrap_or_default();
                        anyhow::bail!("HTTP {} error from {}: {}", status, url, text);
                    }
                }
                Err(_e) if attempts <= max_retries => {
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                    continue;
                }
                Err(e) => return Err(e).context(format!("Failed to connect to {}", url)),
            }
        }
    }

    pub fn build_auth_headers(auth_token: &str, is_anthropic: bool) -> Result<HeaderMap> {
        let mut map = HeaderMap::new();
        map.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        if is_anthropic {
            map.insert(
                "x-api-key",
                HeaderValue::from_str(auth_token).context("Invalid API key header")?,
            );
            map.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        } else {
            let auth_val = format!("Bearer {}", auth_token);
            map.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&auth_val).context("Invalid authorization header")?,
            );
        }

        Ok(map)
    }
}

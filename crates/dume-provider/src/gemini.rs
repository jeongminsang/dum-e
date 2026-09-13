use crate::client::LlmClient;
use crate::types::{ChatMessage, Role, StreamEvent, ToolDefinition};
use anyhow::Result;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::header::HeaderMap;
use serde_json::{json, Value};
use tokio::sync::mpsc;

pub struct GeminiProvider {
    client: LlmClient,
    api_key: String,
}

impl GeminiProvider {
    pub fn new(api_key: &str) -> Self {
        Self {
            client: LlmClient::new(),
            api_key: api_key.to_string(),
        }
    }

    pub async fn stream(
        &self,
        model: &str,
        messages: &[ChatMessage],
        _tools: &[ToolDefinition],
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:streamGenerateContent?alt=sse&key={}",
            model, self.api_key
        );

        let contents: Vec<Value> = messages
            .iter()
            .map(|m| {
                let role = match m.role {
                    Role::User => "user",
                    Role::Assistant => "model",
                    Role::System => "user",
                    Role::Tool => "user",
                };
                json!({
                    "role": role,
                    "parts": [{"text": m.content}]
                })
            })
            .collect();

        let body = json!({
            "contents": contents,
        });

        let headers = HeaderMap::new();
        let resp = self.client.post_with_retry(&url, headers, &body, 3).await?;
        let mut stream = resp.bytes_stream().eventsource();

        while let Some(event_res) = stream.next().await {
            match event_res {
                Ok(event) => {
                    if let Ok(val) = serde_json::from_str::<Value>(&event.data) {
                        if let Some(candidates) = val.get("candidates").and_then(|c| c.as_array()) {
                            if let Some(first) = candidates.first() {
                                if let Some(parts) = first.get("content").and_then(|c| c.get("parts")).and_then(|p| p.as_array()) {
                                    for part in parts {
                                        if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                                            let _ = tx.send(StreamEvent::TextDelta(text.to_string())).await;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                    break;
                }
            }
        }

        let _ = tx.send(StreamEvent::Completed { finish_reason: "stop".to_string() }).await;
        Ok(())
    }
}

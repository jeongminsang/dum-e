use crate::client::LlmClient;
use crate::types::{ChatMessage, Role, StreamEvent, ToolDefinition};
use anyhow::Result;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;

pub struct AnthropicProvider {
    client: LlmClient,
    api_key: String,
}

impl AnthropicProvider {
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
        tools: &[ToolDefinition],
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        let url = "https://api.anthropic.com/v1/messages";
        let headers = LlmClient::build_auth_headers(&self.api_key, true);

        let formatted_msgs: Vec<Value> = messages
            .iter()
            .filter(|m| m.role != Role::System)
            .map(|m| {
                json!({
                    "role": match m.role {
                        Role::User | Role::Tool => "user",
                        Role::Assistant => "assistant",
                        Role::System => "user",
                    },
                    "content": m.content
                })
            })
            .collect();

        let system_prompt = messages
            .iter()
            .find(|m| m.role == Role::System)
            .map(|m| m.content.clone());

        let mut body = json!({
            "model": model,
            "messages": formatted_msgs,
            "max_tokens": 4096,
            "stream": true,
        });

        if let Some(sys) = system_prompt {
            body["system"] = json!(sys);
        }

        if !tools.is_empty() {
            let tools_val: Vec<Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters
                    })
                })
                .collect();
            body["tools"] = json!(tools_val);
        }

        let resp = self.client.post_with_retry(url, headers, &body, 3).await?;
        let mut stream = resp.bytes_stream().eventsource();

        while let Some(event_res) = stream.next().await {
            match event_res {
                Ok(event) => {
                    if let Ok(parsed) = serde_json::from_str::<Value>(&event.data) {
                        if let Some(event_type) = parsed.get("type").and_then(|t| t.as_str()) {
                            match event_type {
                                "content_block_delta" => {
                                    if let Some(delta) = parsed.get("delta") {
                                        if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                                            let _ = tx.send(StreamEvent::TextDelta(text.to_string())).await;
                                        }
                                    }
                                }
                                "message_delta" => {
                                    if let Some(stop) = parsed.get("delta").and_then(|d| d.get("stop_reason")).and_then(|s| s.as_str()) {
                                        let _ = tx.send(StreamEvent::Completed { finish_reason: stop.to_string() }).await;
                                    }
                                }
                                "message_stop" => break,
                                _ => {}
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

        Ok(())
    }
}

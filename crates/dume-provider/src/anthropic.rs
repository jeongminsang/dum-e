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
                match m.role {
                    Role::Assistant => {
                        let mut content_blocks = Vec::new();
                        if !m.content.is_empty() {
                            content_blocks.push(json!({
                                "type": "text",
                                "text": m.content
                            }));
                        }
                        if let Some(tool_calls) = &m.tool_calls {
                            for tc in tool_calls {
                                let parsed_input: Value = serde_json::from_str(&tc.arguments).unwrap_or(json!({}));
                                content_blocks.push(json!({
                                    "type": "tool_use",
                                    "id": tc.id,
                                    "name": tc.name,
                                    "input": parsed_input
                                }));
                            }
                        }
                        json!({
                            "role": "assistant",
                            "content": content_blocks
                        })
                    }
                    Role::Tool => {
                        json!({
                            "role": "user",
                            "content": [{
                                "type": "tool_result",
                                "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
                                "content": m.content
                            }]
                        })
                    }
                    _ => {
                        json!({
                            "role": "user",
                            "content": m.content
                        })
                    }
                }
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
                                "content_block_start" => {
                                    let index = parsed.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                                    if let Some(cb) = parsed.get("content_block") {
                                        if cb.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                                            let id = cb.get("id").and_then(|s| s.as_str()).map(|s| s.to_string());
                                            let name = cb.get("name").and_then(|s| s.as_str()).map(|s| s.to_string());
                                            let _ = tx.send(StreamEvent::ToolCallDelta {
                                                index,
                                                id,
                                                name,
                                                arguments_delta: String::new(),
                                            }).await;
                                        }
                                    }
                                }
                                "content_block_delta" => {
                                    let index = parsed.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                                    if let Some(delta) = parsed.get("delta") {
                                        let delta_type = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                        if delta_type == "text_delta" {
                                            if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                                                let _ = tx.send(StreamEvent::TextDelta(text.to_string())).await;
                                            }
                                        } else if delta_type == "input_json_delta" {
                                            if let Some(partial_json) = delta.get("partial_json").and_then(|s| s.as_str()) {
                                                let _ = tx.send(StreamEvent::ToolCallDelta {
                                                    index,
                                                    id: None,
                                                    name: None,
                                                    arguments_delta: partial_json.to_string(),
                                                }).await;
                                            }
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

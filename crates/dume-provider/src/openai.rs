use crate::client::LlmClient;
use crate::types::{ChatMessage, Role, StreamEvent, ToolDefinition};
use anyhow::Result;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;

pub struct OpenAiProvider {
    client: LlmClient,
    api_key: String,
    base_url: String,
}

impl OpenAiProvider {
    pub fn new(api_key: &str) -> Self {
        Self {
            client: LlmClient::new(),
            api_key: api_key.to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
        }
    }

    pub fn with_base_url(mut self, base_url: &str) -> Self {
        self.base_url = base_url.trim_end_matches('/').to_string();
        self
    }

    pub async fn stream(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        let url = format!("{}/chat/completions", self.base_url);
        let headers = LlmClient::build_auth_headers(&self.api_key, false);

        let formatted_msgs: Vec<Value> = messages
            .iter()
            .map(|m| {
                let mut map = serde_json::Map::new();
                map.insert("role".to_string(), json!(match m.role {
                    Role::System => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool => "tool",
                }));
                map.insert("content".to_string(), json!(m.content));

                if let Some(tool_call_id) = &m.tool_call_id {
                    map.insert("tool_call_id".to_string(), json!(tool_call_id));
                }

                if let Some(tool_calls) = &m.tool_calls {
                    let tc_val: Vec<Value> = tool_calls
                        .iter()
                        .map(|tc| {
                            json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {
                                    "name": tc.name,
                                    "arguments": tc.arguments
                                }
                            })
                        })
                        .collect();
                    map.insert("tool_calls".to_string(), json!(tc_val));
                }

                Value::Object(map)
            })
            .collect();

        let mut body = json!({
            "model": model,
            "messages": formatted_msgs,
            "stream": true,
        });

        if !tools.is_empty() {
            let tools_val: Vec<Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters
                        }
                    })
                })
                .collect();
            body["tools"] = json!(tools_val);
        }

        let resp = self.client.post_with_retry(&url, headers, &body, 3).await?;
        let mut stream = resp.bytes_stream().eventsource();

        while let Some(event_res) = stream.next().await {
            match event_res {
                Ok(event) => {
                    if event.data.trim() == "[DONE]" {
                        let _ = tx.send(StreamEvent::Completed { finish_reason: "stop".to_string() }).await;
                        break;
                    }
                    if let Ok(parsed) = serde_json::from_str::<Value>(&event.data) {
                        if let Some(choices) = parsed.get("choices").and_then(|c| c.as_array()) {
                            if let Some(choice) = choices.first() {
                                if let Some(delta) = choice.get("delta") {
                                    if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                                        let _ = tx.send(StreamEvent::TextDelta(content.to_string())).await;
                                    }
                                    if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                                        for tc in tool_calls {
                                            let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                                            let id = tc.get("id").and_then(|s| s.as_str()).map(|s| s.to_string());
                                            let name = tc.get("function").and_then(|f| f.get("name")).and_then(|s| s.as_str()).map(|s| s.to_string());
                                            let args_delta = tc.get("function").and_then(|f| f.get("arguments")).and_then(|s| s.as_str()).unwrap_or_default().to_string();
                                            let _ = tx.send(StreamEvent::ToolCallDelta {
                                                index,
                                                id,
                                                name,
                                                arguments_delta: args_delta,
                                            }).await;
                                        }
                                    }
                                }
                                if let Some(finish) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                                    let _ = tx.send(StreamEvent::Completed { finish_reason: finish.to_string() }).await;
                                    break;
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

        Ok(())
    }
}

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
                json!({
                    "role": match m.role {
                        Role::System => "system",
                        Role::User => "user",
                        Role::Assistant => "assistant",
                        Role::Tool => "tool",
                    },
                    "content": m.content
                })
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

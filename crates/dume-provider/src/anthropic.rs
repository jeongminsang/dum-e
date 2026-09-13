use crate::client::LlmClient;
use crate::types::{ChatMessage, Role, StreamEvent, ToolDefinition};
use anyhow::Result;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};
use tokio::sync::mpsc;

pub struct AnthropicProvider {
    client: LlmClient,
    api_key: String,
    oauth: bool,
    base_url: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolCall;

    #[tokio::test]
    async fn explicit_auth_and_tool_names_round_trip() {
        for oauth in [false, true] {
            let wire_name = if oauth { "Read" } else { "read" };
            let sse = format!(
                "data: {}\n\ndata: {}\n\ndata: {}\n\n",
                json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"next","name":wire_name,"input":{}}}),
                json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}),
            );
            let (url, server) = crate::codex::tests::fixture(sse);
            // OAuth-looking key remains an API key unless explicitly selected.
            let provider = if oauth {
                AnthropicProvider::new_oauth("plain-token")
            } else {
                AnthropicProvider::new("sk-ant-oat-key")
            }
            .with_base_url(&url);
            let messages = vec![
                ChatMessage::system("Custom system"),
                ChatMessage::user("Hi"),
                ChatMessage::assistant_with_tool_calls(
                    "",
                    vec![ToolCall {
                        id: "old".into(),
                        name: "read".into(),
                        arguments: "{}".into(),
                    }],
                ),
                ChatMessage::tool("result", "old"),
            ];
            let tools = vec![ToolDefinition {
                name: "read".into(),
                description: "Read".into(),
                parameters: json!({"type":"object"}),
            }];
            let (tx, mut rx) = mpsc::channel(16);
            provider
                .stream("claude", &messages, &tools, tx)
                .await
                .unwrap();
            let (headers, body) = server.join().unwrap();
            assert!(headers.starts_with("POST /messages "));
            if oauth {
                assert!(headers.contains("authorization: Bearer plain-token"));
                assert!(!headers.contains("x-api-key:"));
                assert!(headers.contains("oauth-2025-04-20"));
                assert!(headers.contains("user-agent: claude-cli/2.1.251"));
                assert!(headers.contains("x-app: cli"));
                assert_eq!(
                    body["system"][0]["text"],
                    "You are Claude Code, Anthropic's official CLI for Claude."
                );
                assert_eq!(body["system"][1]["text"], "Custom system");
            } else {
                assert!(headers.contains("x-api-key: sk-ant-oat-key"));
                assert!(!headers.contains("authorization:"));
                assert!(!headers.contains("anthropic-beta:"));
                assert_eq!(body["system"], "Custom system");
            }
            assert_eq!(body["tools"][0]["name"], wire_name);
            assert_eq!(body["messages"][1]["content"][0]["name"], wire_name);
            assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "old");
            assert_eq!(
                rx.recv().await,
                Some(StreamEvent::ToolCallDelta {
                    index: 1,
                    id: Some("next".into()),
                    name: Some("read".into()),
                    arguments_delta: String::new()
                })
            );
            assert_eq!(
                rx.recv().await,
                Some(StreamEvent::ToolCallDelta {
                    index: 1,
                    id: None,
                    name: None,
                    arguments_delta: "{}".into()
                })
            );
        }
    }

    #[test]
    fn invalid_headers_are_errors_and_unknown_names_are_preserved() {
        assert!(AnthropicProvider::new("bad\nkey").headers().is_err());
        assert!(
            AnthropicProvider::new_oauth("bad\ntoken")
                .headers()
                .is_err()
        );
        assert_eq!(
            AnthropicProvider::new_oauth("token").tool_name("custom_tool"),
            "custom_tool"
        );
        assert_eq!(
            AnthropicProvider::new_oauth("token").tool_name("bAsH"),
            "Bash"
        );
        assert_eq!(AnthropicProvider::new("key").tool_name("bash"), "bash");
    }
}

impl AnthropicProvider {
    pub fn new(api_key: &str) -> Self {
        Self {
            client: LlmClient::new(),
            api_key: api_key.to_string(),
            oauth: false,
            base_url: "https://api.anthropic.com/v1".to_string(),
        }
    }

    pub fn new_oauth(token: &str) -> Self {
        Self {
            oauth: true,
            ..Self::new(token)
        }
    }

    pub fn with_base_url(mut self, base_url: &str) -> Self {
        self.base_url = base_url.trim_end_matches('/').to_string();
        self
    }

    fn headers(&self) -> Result<HeaderMap> {
        let mut headers = LlmClient::build_auth_headers(&self.api_key, !self.oauth)?;
        if self.oauth {
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
            headers.insert(
                "anthropic-beta",
                HeaderValue::from_static("claude-code-20250219,oauth-2025-04-20"),
            );
            headers.insert("user-agent", HeaderValue::from_static("claude-cli/2.1.251"));
            headers.insert("x-app", HeaderValue::from_static("cli"));
            headers.insert("accept", HeaderValue::from_static("application/json"));
            headers.insert(
                "anthropic-dangerous-direct-browser-access",
                HeaderValue::from_static("true"),
            );
        }
        Ok(headers)
    }

    fn tool_name<'a>(&self, name: &'a str) -> &'a str {
        const NAMES: &[&str] = &[
            "Read",
            "Write",
            "Edit",
            "Bash",
            "Grep",
            "Glob",
            "AskUserQuestion",
            "EnterPlanMode",
            "ExitPlanMode",
            "KillShell",
            "NotebookEdit",
            "Skill",
            "Task",
            "TaskOutput",
            "TodoWrite",
            "WebFetch",
            "WebSearch",
        ];
        if self.oauth {
            NAMES
                .iter()
                .copied()
                .find(|n| n.eq_ignore_ascii_case(name))
                .unwrap_or(name)
        } else {
            name
        }
    }

    pub async fn stream(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        let url = format!("{}/messages", self.base_url);
        let headers = self.headers()?;

        let formatted_msgs: Vec<Value> = messages
            .iter()
            .filter(|m| m.role != Role::System)
            .map(|m| match m.role {
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
                            let parsed_input: Value =
                                serde_json::from_str(&tc.arguments).unwrap_or(json!({}));
                            content_blocks.push(json!({
                                "type": "tool_use",
                                "id": tc.id,
                                "name": self.tool_name(&tc.name),
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

        if self.oauth {
            let mut system = vec![
                json!({"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."}),
            ];
            if let Some(sys) = system_prompt {
                system.push(json!({"type": "text", "text": sys}));
            }
            body["system"] = json!(system);
        } else if let Some(sys) = system_prompt {
            body["system"] = json!(sys);
        }

        if !tools.is_empty() {
            let tools_val: Vec<Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "name": self.tool_name(&t.name),
                        "description": t.description,
                        "input_schema": t.parameters
                    })
                })
                .collect();
            body["tools"] = json!(tools_val);
        }

        let resp = self.client.post_with_retry(&url, headers, &body, 3).await?;
        let mut stream = resp.bytes_stream().eventsource();
        let mut completed = false;

        while let Some(event_res) = stream.next().await {
            match event_res {
                Ok(event) => {
                    if let Ok(parsed) = serde_json::from_str::<Value>(&event.data) {
                        if let Some(event_type) = parsed.get("type").and_then(|t| t.as_str()) {
                            match event_type {
                                "content_block_start" => {
                                    let index =
                                        parsed.get("index").and_then(|i| i.as_u64()).unwrap_or(0)
                                            as usize;
                                    if let Some(cb) = parsed.get("content_block") {
                                        if cb.get("type").and_then(|t| t.as_str())
                                            == Some("tool_use")
                                        {
                                            let id = cb
                                                .get("id")
                                                .and_then(|s| s.as_str())
                                                .map(|s| s.to_string());
                                            let name =
                                                cb.get("name").and_then(|s| s.as_str()).map(|s| {
                                                    if self.oauth {
                                                        tools
                                                            .iter()
                                                            .find(|t| {
                                                                t.name.eq_ignore_ascii_case(s)
                                                            })
                                                            .map(|t| t.name.clone())
                                                            .unwrap_or_else(|| s.to_string())
                                                    } else {
                                                        s.to_string()
                                                    }
                                                });
                                            let _ = tx
                                                .send(StreamEvent::ToolCallDelta {
                                                    index,
                                                    id,
                                                    name,
                                                    arguments_delta: String::new(),
                                                })
                                                .await;
                                        }
                                    }
                                }
                                "content_block_delta" => {
                                    let index =
                                        parsed.get("index").and_then(|i| i.as_u64()).unwrap_or(0)
                                            as usize;
                                    if let Some(delta) = parsed.get("delta") {
                                        let delta_type = delta
                                            .get("type")
                                            .and_then(|t| t.as_str())
                                            .unwrap_or("");
                                        if delta_type == "text_delta" {
                                            if let Some(text) =
                                                delta.get("text").and_then(|t| t.as_str())
                                            {
                                                let _ = tx
                                                    .send(StreamEvent::TextDelta(text.to_string()))
                                                    .await;
                                            }
                                        } else if delta_type == "input_json_delta" {
                                            if let Some(partial_json) =
                                                delta.get("partial_json").and_then(|s| s.as_str())
                                            {
                                                let _ = tx
                                                    .send(StreamEvent::ToolCallDelta {
                                                        index,
                                                        id: None,
                                                        name: None,
                                                        arguments_delta: partial_json.to_string(),
                                                    })
                                                    .await;
                                            }
                                        }
                                    }
                                }
                                "message_delta" => {
                                    if let Some(stop) = parsed
                                        .get("delta")
                                        .and_then(|d| d.get("stop_reason"))
                                        .and_then(|s| s.as_str())
                                    {
                                        let _ = tx
                                            .send(StreamEvent::Completed {
                                                finish_reason: stop.to_string(),
                                            })
                                            .await;
                                        completed = true;
                                    }
                                }
                                "error" => {
                                    let message =
                                        format!("Anthropic response error: {}", parsed["error"]);
                                    let _ = tx.send(StreamEvent::Error(message.clone())).await;
                                    anyhow::bail!(message);
                                }
                                "message_stop" => break,
                                _ => {}
                            }
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                    return Err(e.into());
                }
            }
        }

        if !completed {
            let message = "Anthropic stream ended before a stop reason";
            let _ = tx.send(StreamEvent::Error(message.to_string())).await;
            anyhow::bail!(message);
        }
        Ok(())
    }
}

use crate::client::LlmClient;
use crate::types::{ChatMessage, Role, StreamEvent, ToolDefinition};
use anyhow::{Context, Result, bail};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;

pub struct OpenAiProvider {
    client: LlmClient,
    api_key: String,
    base_url: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn api_key_keeps_chat_completions_contract() {
        let sse = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]})
        );
        let (url, server) = crate::codex::tests::fixture(sse);
        let (tx, mut rx) = mpsc::channel(8);
        OpenAiProvider::new("key")
            .with_base_url(&url)
            .stream("gpt", &[ChatMessage::user("Hi")], &[], tx)
            .await
            .unwrap();
        let (headers, body) = server.join().unwrap();
        assert!(headers.starts_with("POST /chat/completions "));
        assert!(headers.contains("authorization: Bearer key"));
        assert!(!headers.contains("chatgpt-account-id:"));
        assert_eq!(body["messages"][0]["content"], "Hi");
        assert!(body.get("input").is_none());
        assert_eq!(rx.recv().await, Some(StreamEvent::TextDelta("ok".into())));
        assert_eq!(
            rx.recv().await,
            Some(StreamEvent::Completed {
                finish_reason: "stop".into()
            })
        );
        assert!(rx.recv().await.is_none());
        assert!(LlmClient::build_auth_headers("bad\nkey", false).is_err());
    }

    #[tokio::test]
    async fn malformed_error_and_eof_never_complete() {
        for sse in [
            "data: invalid\n\n",
            "data: {\"error\":{\"message\":\"denied\"}}\n\n",
            "data: [DONE]\n\n",
            "data: {\"choices\":[{\"delta\":{}}]}\n\n",
            "data: {}\n\n",
            "",
        ] {
            let (url, server) = crate::codex::tests::fixture(sse.into());
            let (tx, mut rx) = mpsc::channel(8);
            assert!(
                OpenAiProvider::new("key")
                    .with_base_url(&url)
                    .stream("gpt", &[], &[], tx)
                    .await
                    .is_err()
            );
            server.join().unwrap();
            assert!(matches!(rx.recv().await, Some(StreamEvent::Error(_))));
            assert!(rx.recv().await.is_none());
        }
    }
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
        let result = self.stream_inner(model, messages, tools, &tx).await;
        if let Err(error) = &result {
            let _ = tx.send(StreamEvent::Error(error.to_string())).await;
        }
        result
    }

    async fn stream_inner(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        tx: &mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        let url = format!("{}/chat/completions", self.base_url);
        let headers = LlmClient::build_auth_headers(&self.api_key, false)?;

        let formatted_msgs: Vec<Value> = messages
            .iter()
            .map(|m| {
                let mut map = serde_json::Map::new();
                map.insert(
                    "role".to_string(),
                    json!(match m.role {
                        Role::System => "system",
                        Role::User => "user",
                        Role::Assistant => "assistant",
                        Role::Tool => "tool",
                    }),
                );
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
                        bail!("OpenAI stream ended without a finish reason");
                    }
                    {
                        let parsed = serde_json::from_str::<Value>(&event.data)
                            .context("Invalid OpenAI SSE JSON")?;
                        if parsed.get("error").is_some() {
                            bail!("OpenAI response error");
                        }
                        anyhow::ensure!(parsed["choices"].is_array(), "Missing OpenAI choices");
                        if let Some(choices) = parsed.get("choices").and_then(|c| c.as_array()) {
                            if let Some(choice) = choices.first() {
                                if let Some(delta) = choice.get("delta") {
                                    if let Some(content) =
                                        delta.get("content").and_then(|c| c.as_str())
                                    {
                                        tx.send(StreamEvent::TextDelta(content.to_string()))
                                            .await
                                            .context("Stream receiver closed")?;
                                    }
                                    if let Some(tool_calls) =
                                        delta.get("tool_calls").and_then(|t| t.as_array())
                                    {
                                        for tc in tool_calls {
                                            let index = tc
                                                .get("index")
                                                .and_then(|i| i.as_u64())
                                                .unwrap_or(0)
                                                as usize;
                                            let id = tc
                                                .get("id")
                                                .and_then(|s| s.as_str())
                                                .map(|s| s.to_string());
                                            let name = tc
                                                .get("function")
                                                .and_then(|f| f.get("name"))
                                                .and_then(|s| s.as_str())
                                                .map(|s| s.to_string());
                                            let args_delta = tc
                                                .get("function")
                                                .and_then(|f| f.get("arguments"))
                                                .and_then(|s| s.as_str())
                                                .unwrap_or_default()
                                                .to_string();
                                            tx.send(StreamEvent::ToolCallDelta {
                                                index,
                                                id,
                                                name,
                                                arguments_delta: args_delta,
                                            })
                                            .await
                                            .context("Stream receiver closed")?;
                                        }
                                    }
                                }
                                if let Some(finish) =
                                    choice.get("finish_reason").and_then(|f| f.as_str())
                                {
                                    tx.send(StreamEvent::Completed {
                                        finish_reason: finish.to_string(),
                                    })
                                    .await
                                    .context("Stream receiver closed")?;
                                    return Ok(());
                                }
                            }
                        }
                    }
                }

                Err(e) => {
                    return Err(e).context("Invalid OpenAI SSE frame");
                }
            }
        }

        bail!("OpenAI stream ended without a finish reason")
    }
}

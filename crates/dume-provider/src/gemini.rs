use crate::client::LlmClient;
use crate::types::{ChatMessage, Role, StreamEvent, TokenUsage, ToolDefinition};
use anyhow::{Context, Result};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};
use std::collections::HashMap;
use tokio::sync::mpsc;

pub struct GeminiProvider {
    client: LlmClient,
    api_key: String,
    base_url: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolCall;

    #[tokio::test]
    async fn system_tools_and_function_roundtrip_use_gemini_schema() {
        let sse = format!(
            "data: {}\n\n",
            json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"read","args":{"path":"x"}},"thoughtSignature":"opaque-signature"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":120,"candidatesTokenCount":8,"totalTokenCount":128,"cachedContentTokenCount":90}})
        );
        let (url, server) = crate::codex::tests::fixture(sse);
        let tool = ToolDefinition {
            name: "read".into(),
            description: "Read file".into(),
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}}}),
        };
        let messages = vec![
            ChatMessage::system("stable rules"),
            ChatMessage::user("read x"),
            ChatMessage::assistant_with_tool_calls(
                "",
                vec![ToolCall {
                    id: "call-1".into(),
                    name: "read".into(),
                    arguments: "{\"path\":\"x\"}".into(),
                }],
            ),
            ChatMessage::tool("content", "call-1"),
        ];
        let (tx, mut rx) = mpsc::channel(16);
        GeminiProvider::new("local-key")
            .with_base_url(&url)
            .stream("gemini-test", &messages, &[tool], tx)
            .await
            .unwrap();
        let (headers, body) = server.join().unwrap();
        assert!(headers.starts_with("POST /models/gemini-test:streamGenerateContent?alt=sse "));
        assert!(headers.contains("x-goog-api-key: local-key"));
        assert_eq!(
            body["systemInstruction"]["parts"][0]["text"],
            "stable rules"
        );
        assert_eq!(body["tools"][0]["functionDeclarations"][0]["name"], "read");
        assert_eq!(
            body["contents"][1]["parts"][0]["functionCall"]["args"]["path"],
            "x"
        );
        assert_eq!(
            body["contents"][2]["parts"][0]["functionResponse"]["name"],
            "read"
        );
        assert!(
            matches!(rx.recv().await, Some(StreamEvent::Usage(usage)) if usage.cache_read_tokens == Some(90))
        );
        let parts = match rx.recv().await {
            Some(StreamEvent::GeminiParts(parts)) => parts,
            other => panic!("unexpected event: {other:?}"),
        };
        let mut replay = ChatMessage::assistant_with_tool_calls(
            "",
            vec![ToolCall {
                id: "local-call".into(),
                name: "read".into(),
                arguments: "{\"path\":\"x\"}".into(),
            }],
        );
        replay.gemini_parts = parts;
        let replay_body = GeminiProvider::request_body(
            &[
                ChatMessage::user("read x"),
                replay,
                ChatMessage::tool("content", "local-call"),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(
            replay_body["contents"][1]["parts"][0]["thoughtSignature"],
            "opaque-signature"
        );
        assert!(
            replay_body["contents"][2]["parts"][0]["functionResponse"]
                .get("id")
                .is_none()
        );
        assert!(
            matches!(rx.recv().await, Some(StreamEvent::ToolCallDelta { name: Some(name), arguments_delta, .. }) if name == "read" && arguments_delta.contains("path"))
        );
        assert_eq!(
            rx.recv().await,
            Some(StreamEvent::Completed {
                finish_reason: "stop".into()
            })
        );
    }
}

impl GeminiProvider {
    pub fn new(api_key: &str) -> Self {
        Self {
            client: LlmClient::new(),
            api_key: api_key.to_string(),
            base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
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
        let url = format!(
            "{}/models/{}:streamGenerateContent?alt=sse",
            self.base_url, model
        );

        let body = Self::request_body(messages, tools)?;

        let mut headers = HeaderMap::new();
        headers.insert(
            "x-goog-api-key",
            HeaderValue::from_str(&self.api_key).context("Invalid Gemini API key header")?,
        );
        let resp = self.client.post_with_retry(&url, headers, &body, 3).await?;
        let mut stream = resp.bytes_stream().eventsource();
        let mut call_index = 0usize;
        let call_prefix = crate::types::StreamRequestContext::new().request_id;

        while let Some(event_res) = stream.next().await {
            match event_res {
                Ok(event) => {
                    if let Ok(val) = serde_json::from_str::<Value>(&event.data) {
                        if let Some(err) = val.get("error") {
                            let message = format!("Google API error: {err}");
                            let _ = tx.send(StreamEvent::Error(message.clone())).await;
                            anyhow::bail!(message);
                        }
                        if let Some(usage) = val.get("usageMetadata") {
                            let input = usage
                                .get("promptTokenCount")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0);
                            let output = usage
                                .get("candidatesTokenCount")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0);
                            let total = usage
                                .get("totalTokenCount")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(input + output);
                            let cache_read = usage
                                .get("cachedContentTokenCount")
                                .and_then(|v| v.as_i64());
                            if input > 0 || output > 0 || total > 0 {
                                let _ = tx
                                    .send(StreamEvent::Usage(TokenUsage {
                                        input_tokens: input,
                                        output_tokens: output,
                                        total_tokens: total,
                                        cache_read_tokens: cache_read,
                                        cache_write_tokens: None,
                                        raw_usage: Some(usage.clone()),
                                        is_complete: true,
                                    }))
                                    .await;
                            }
                        }
                        if let Some(candidates) = val.get("candidates").and_then(|c| c.as_array()) {
                            if let Some(first) = candidates.first() {
                                if let Some(parts) = first
                                    .get("content")
                                    .and_then(|c| c.get("parts"))
                                    .and_then(|p| p.as_array())
                                {
                                    tx.send(StreamEvent::GeminiParts(parts.clone())).await?;
                                    for part in parts {
                                        if let Some(text) =
                                            part.get("text").and_then(|t| t.as_str())
                                        {
                                            let _ = tx
                                                .send(StreamEvent::TextDelta(text.to_string()))
                                                .await;
                                        }
                                        if let Some(call) = part.get("functionCall") {
                                            let name = call
                                                .get("name")
                                                .and_then(Value::as_str)
                                                .context("Gemini function call missing name")?;
                                            let id = call
                                                .get("id")
                                                .and_then(Value::as_str)
                                                .map(str::to_owned)
                                                .unwrap_or_else(|| {
                                                    format!("gemini-{call_prefix}-{call_index}")
                                                });
                                            let arguments_delta = serde_json::to_string(
                                                call.get("args").unwrap_or(&json!({})),
                                            )?;
                                            tx.send(StreamEvent::ToolCallDelta {
                                                index: call_index,
                                                id: Some(id),
                                                name: Some(name.to_string()),
                                                arguments_delta,
                                            })
                                            .await?;
                                            call_index += 1;
                                        }
                                    }
                                }
                                if let Some(finish) =
                                    first.get("finishReason").and_then(|f| f.as_str())
                                {
                                    let reason = match finish {
                                        "STOP" => "stop",
                                        "MAX_TOKENS" => "length",
                                        _ => finish,
                                    };
                                    let _ = tx
                                        .send(StreamEvent::Completed {
                                            finish_reason: reason.to_string(),
                                        })
                                        .await;
                                    return Ok(());
                                }
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

        let message = "Google stream ended prematurely without finish reason";
        let _ = tx.send(StreamEvent::Error(message.to_string())).await;
        anyhow::bail!(message);
    }

    fn request_body(messages: &[ChatMessage], tools: &[ToolDefinition]) -> Result<Value> {
        let mut system = Vec::new();
        let mut contents = Vec::new();
        let mut call_names: HashMap<&str, (&str, Option<&str>)> = HashMap::new();
        for message in messages.iter().filter(|m| m.is_model_visible()) {
            match message.role {
                Role::System => system.push(message.content.as_str()),
                Role::User => {
                    contents.push(json!({"role": "user", "parts": [{"text": message.content}]}))
                }
                Role::Assistant => {
                    if !message.gemini_parts.is_empty() {
                        if let Some(calls) = &message.tool_calls {
                            for (call, part) in calls.iter().zip(
                                message
                                    .gemini_parts
                                    .iter()
                                    .filter_map(|part| part.get("functionCall")),
                            ) {
                                call_names.insert(
                                    call.id.as_str(),
                                    (call.name.as_str(), part.get("id").and_then(Value::as_str)),
                                );
                            }
                        }
                        contents.push(json!({"role": "model", "parts": message.gemini_parts}));
                        continue;
                    }
                    let mut parts = Vec::new();
                    if !message.content.is_empty() {
                        parts.push(json!({"text": message.content}));
                    }
                    if let Some(calls) = &message.tool_calls {
                        for call in calls {
                            call_names.insert(
                                call.id.as_str(),
                                (call.name.as_str(), Some(call.id.as_str())),
                            );
                            let args: Value = serde_json::from_str(&call.arguments)
                                .context("Invalid Gemini tool arguments")?;
                            parts.push(json!({"functionCall": {"name": call.name, "args": args, "id": call.id}}));
                        }
                    }
                    contents.push(json!({"role": "model", "parts": parts}));
                }
                Role::Tool => {
                    let id = message
                        .tool_call_id
                        .as_deref()
                        .context("Gemini tool result missing call ID")?;
                    let (name, provider_id) = call_names
                        .get(id)
                        .context("Gemini tool result has no matching call")?;
                    let mut response =
                        json!({"name": name, "response": {"output": message.content}});
                    if let Some(provider_id) = provider_id {
                        response["id"] = json!(provider_id);
                    }
                    contents
                        .push(json!({"role": "user", "parts": [{"functionResponse": response}]}));
                }
            }
        }
        let mut body = json!({"contents": contents});
        if !system.is_empty() {
            body["systemInstruction"] = json!({"parts": [{"text": system.join("\n\n")}]});
        }
        if !tools.is_empty() {
            let declarations: Vec<Value> = tools.iter().map(|tool| json!({"name": tool.name, "description": tool.description, "parameters": tool.parameters})).collect();
            body["tools"] = json!([{"functionDeclarations": declarations}]);
        }
        Ok(body)
    }
}

use crate::client::LlmClient;
use crate::types::{
    ChatMessage, ProviderFailureKind, Role, StreamEvent, StreamRequestContext, TokenUsage,
    ToolDefinition,
};
use anyhow::{Context, Result};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::header::{HeaderName, HeaderValue, USER_AGENT};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct OpenCodeProfile {
    pub is_free_model: bool,
    pub project_id: String,
}

pub struct OpenAiProvider {
    client: LlmClient,
    api_key: String,
    base_url: String,
    opencode_profile: Option<OpenCodeProfile>,
    deepseek: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct ProviderStreamFailure {
    kind: ProviderFailureKind,
    message: String,
}

fn openai_stream_failure(error: &Value) -> ProviderStreamFailure {
    let detail = error.get("error").unwrap_or(error);
    let code = detail
        .get("code")
        .or_else(|| detail.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let message = detail.get("message").and_then(Value::as_str).unwrap_or("");
    let kind = if crate::types::is_quota_type_or_code(code)
        || crate::types::is_quota_message(message)
    {
        ProviderFailureKind::QuotaExceeded
    } else {
        match code {
            "overloaded_error"
            | "overload"
            | "server_busy"
            | "server_error"
            | "resource_exhausted"
            | "rate_limit_exceeded"
            | "rate_limit_error" => ProviderFailureKind::Transient,
            "invalid_api_key" | "authentication_error" | "unauthorized" | "permission_denied" => {
                ProviderFailureKind::Auth
            }
            "context_length_exceeded" | "maximum_context_length_exceeded" => {
                ProviderFailureKind::Context
            }
            "invalid_request_error" | "bad_request" => ProviderFailureKind::InvalidRequest,
            _ => ProviderFailureKind::Unknown,
        }
    };
    ProviderStreamFailure {
        kind,
        message: error.to_string(),
    }
}

impl OpenAiProvider {
    pub(crate) fn url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }
    pub fn new(api_key: &str) -> Self {
        Self {
            client: LlmClient::new(),
            api_key: api_key.to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            opencode_profile: None,
            deepseek: false,
        }
    }

    pub fn new_deepseek(api_key: &str) -> Self {
        let mut provider = Self::new(api_key).with_base_url("https://api.deepseek.com");
        provider.deepseek = true;
        provider
    }

    pub fn with_base_url(mut self, base_url: &str) -> Self {
        self.base_url = base_url.trim_end_matches('/').to_string();
        self
    }

    pub fn with_opencode_profile(mut self, is_free_model: bool) -> Self {
        let project_id = Self::derive_stable_project_id();
        self.opencode_profile = Some(OpenCodeProfile {
            is_free_model,
            project_id,
        });
        self
    }

    pub fn derive_stable_project_id() -> String {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "global".to_string());
        let hash = Sha256::digest(cwd.as_bytes());
        hex::encode(&hash[..6]) // 12 hex chars
    }

    pub fn format_opencode_session_id(seed: &str) -> String {
        const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
        let mut hasher = Sha256::new();
        hasher.update(b"opencode\0ses_\0");
        hasher.update(seed.as_bytes());
        let hash = hasher.finalize();

        // 36 bits from hash[0..5]
        let ts36 = ((hash[0] as u64) << 28)
            | ((hash[1] as u64) << 20)
            | ((hash[2] as u64) << 12)
            | ((hash[3] as u64) << 4)
            | ((hash[4] >> 4) as u64);

        // descending encoding: ~(ts36 << 12 | 1) & 0xffffffffffff
        let descending_48 = !((ts36 << 12) | 1u64) & 0xffffffffffffu64;
        let time_part = format!("{descending_48:012x}");

        let random_part: String = hash[5..19]
            .iter()
            .map(|&byte| BASE62[(byte as usize) % 62] as char)
            .collect();

        format!("ses_{time_part}{random_part}")
    }

    pub fn format_opencode_request_id(seed: &str) -> String {
        const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
        let mut hasher = Sha256::new();
        hasher.update(b"opencode\0msg_\0");
        hasher.update(seed.as_bytes());
        let hash = hasher.finalize();

        let hex_part = hex::encode(&hash[..6]);
        let random_part: String = hash[6..20]
            .iter()
            .map(|&byte| BASE62[(byte as usize) % 62] as char)
            .collect();

        format!("msg_{hex_part}{random_part}")
    }

    pub async fn stream(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        self.stream_with_context(model, messages, tools, None, tx)
            .await
    }

    pub async fn stream_with_context(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        context: Option<&StreamRequestContext>,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        let result = self
            .stream_inner(model, messages, tools, context, &tx)
            .await;
        if let Err(error) = &result {
            if let Some(failure) = error.downcast_ref::<ProviderStreamFailure>() {
                let _ = tx
                    .send(StreamEvent::ProviderError {
                        kind: failure.kind,
                        message: failure.message.clone(),
                        retry_after_ms: None,
                    })
                    .await;
            } else {
                let _ = tx.send(StreamEvent::Error(error.to_string())).await;
            }
        }
        result
    }

    async fn stream_inner(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        context: Option<&StreamRequestContext>,
        tx: &mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        let is_opencode_free = self
            .opencode_profile
            .as_ref()
            .map(|p| p.is_free_model)
            .unwrap_or(false)
            || std::env::var("DUME_OPENCODE_FREE_COMPAT")
                .map(|v| v.trim() == "1")
                .unwrap_or(false);

        let url = if is_opencode_free && self.base_url == "https://opencode.ai/zen/v1" {
            "https://opencode.ai/inference/v1/chat/completions".to_string()
        } else {
            self.url()
        };
        let mut headers = LlmClient::build_auth_headers(&self.api_key, false)?;

        if let Some(profile) = &self.opencode_profile {
            let default_ctx;
            let ctx = match context {
                Some(ctx) => ctx,
                None => {
                    default_ctx = StreamRequestContext::new();
                    &default_ctx
                }
            };

            let (session_id, request_id) = if is_opencode_free {
                (
                    Self::format_opencode_session_id(&ctx.session_id),
                    Self::format_opencode_request_id(&ctx.request_id),
                )
            } else {
                (ctx.session_id.clone(), ctx.request_id.clone())
            };

            if is_opencode_free {
                let default_ua = "opencode/1.19.0".to_string();
                let opencode_ua = std::env::var("OPENCODE_USER_AGENT").unwrap_or(default_ua);
                headers.insert(
                    USER_AGENT,
                    HeaderValue::from_str(&opencode_ua)
                        .unwrap_or_else(|_| HeaderValue::from_static("opencode/1.19.0")),
                );
                headers.insert(
                    HeaderName::from_static("x-opencode-client"),
                    HeaderValue::from_static("desktop"),
                );
                headers.insert(
                    HeaderName::from_static("x-opencode-project"),
                    HeaderValue::from_static("global"),
                );
            } else {
                let ua = format!("dume/{}", env!("CARGO_PKG_VERSION"));
                headers.insert(
                    USER_AGENT,
                    HeaderValue::from_str(&ua).unwrap_or_else(|_| HeaderValue::from_static("dume")),
                );
                headers.insert(
                    HeaderName::from_static("x-opencode-client"),
                    HeaderValue::from_static("dume"),
                );
                headers.insert(
                    HeaderName::from_static("x-opencode-project"),
                    HeaderValue::from_str(&profile.project_id)
                        .unwrap_or_else(|_| HeaderValue::from_static("global")),
                );
            }

            headers.insert(
                HeaderName::from_static("x-opencode-session"),
                HeaderValue::from_str(&session_id).context("Invalid session ID header")?,
            );
            headers.insert(
                HeaderName::from_static("x-opencode-request"),
                HeaderValue::from_str(&request_id).context("Invalid request ID header")?,
            );
        }

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

                if self.deepseek && m.role == Role::Assistant {
                    map.insert(
                        "reasoning_content".to_string(),
                        json!(m.deepseek_reasoning.as_deref().unwrap_or("")),
                    );
                }

                Value::Object(map)
            })
            .collect();

        let mut body = json!({
            "model": model,
            "messages": formatted_msgs,
            "stream": true,
            "stream_options": {
                "include_usage": true
            }
        });

        if let Some(effort) = context
            .and_then(|ctx| ctx.reasoning_effort)
            .filter(|effort| !effort.is_default())
        {
            if self.deepseek {
                if effort == crate::reasoning::ReasoningEffort::None {
                    body["thinking"] = json!({"type": "disabled"});
                } else {
                    body["thinking"] = json!({"type": "enabled"});
                    body["reasoning_effort"] = json!(effort);
                }
            } else {
                body["reasoning_effort"] = json!(effort);
            }
        }

        let mut tools_val: Vec<Value> = tools
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

        if is_opencode_free {
            let has_read_tool = tools_val.iter().any(|t| {
                t.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    == Some("read")
            });
            if !has_read_tool {
                tools_val.push(json!({
                    "type": "function",
                    "function": {
                        "name": "read",
                        "description": "Read file contents",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "filePath": {
                                    "type": "string"
                                }
                            },
                            "required": ["filePath"]
                        }
                    }
                }));
            }
        }

        if !tools_val.is_empty() {
            body["tools"] = json!(tools_val);
        }

        let resp = self.client.post_once(&url, headers, &body).await?;
        let mut stream = resp.bytes_stream().eventsource();
        let mut finish_reason = None;

        while let Some(event_res) = stream.next().await {
            match event_res {
                Ok(event) => {
                    if event.data.trim() == "[DONE]" {
                        break;
                    }
                    {
                        let parsed = serde_json::from_str::<Value>(&event.data)
                            .context("Invalid OpenAI SSE JSON")?;
                        if let Some(error) = parsed.get("error") {
                            return Err(anyhow::Error::new(openai_stream_failure(error)));
                        }
                        let identity =
                            crate::types::ResponseIdentity::from_response(&parsed, "model", "id");
                        if identity.model.is_some() || identity.response_id.is_some() {
                            tx.send(StreamEvent::ResponseIdentity(identity)).await?;
                        }
                        anyhow::ensure!(parsed["choices"].is_array(), "Missing OpenAI choices");
                        if let Some(usage) = parsed.get("usage").filter(|value| value.is_object()) {
                            let input_tokens = usage["prompt_tokens"].as_i64().unwrap_or(0);
                            let output_tokens = usage["completion_tokens"].as_i64().unwrap_or(0);
                            let total_tokens = usage["total_tokens"]
                                .as_i64()
                                .unwrap_or(input_tokens + output_tokens);
                            tx.send(StreamEvent::Usage(TokenUsage {
                                input_tokens,
                                output_tokens,
                                total_tokens,
                                cache_read_tokens: if self.deepseek {
                                    usage["prompt_cache_hit_tokens"].as_i64()
                                } else {
                                    usage["prompt_tokens_details"]["cached_tokens"].as_i64()
                                },
                                cache_write_tokens: None,
                                raw_usage: Some(usage.clone()),
                                is_complete: true,
                            }))
                            .await
                            .context("Stream receiver closed")?;
                        }
                        if let Some(choices) = parsed.get("choices").and_then(|c| c.as_array()) {
                            if let Some(choice) = choices.first() {
                                if let Some(delta) = choice.get("delta") {
                                    if self.deepseek {
                                        if let Some(reasoning) =
                                            delta.get("reasoning_content").and_then(Value::as_str)
                                        {
                                            tx.send(StreamEvent::DeepSeekReasoning(
                                                reasoning.to_string(),
                                            ))
                                            .await
                                            .context("Stream receiver closed")?;
                                        }
                                    }
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
                                    finish_reason = Some(finish.to_string());
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

        let finish_reason = finish_reason.context("OpenAI stream ended without a finish reason")?;
        tx.send(StreamEvent::Completed { finish_reason })
            .await
            .context("Stream receiver closed")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolCall;

    #[tokio::test]
    async fn effort_and_final_usage_identity_are_not_lost_after_finish_reason() {
        use crate::reasoning::ReasoningEffort;
        for (deepseek, effort) in [
            (false, ReasoningEffort::High),
            (true, ReasoningEffort::None),
            (true, ReasoningEffort::High),
            (true, ReasoningEffort::Max),
        ] {
            let sse = format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}),
                json!({"model":"reported","id":"response","choices":[],"usage":{"prompt_tokens":7,"completion_tokens":2}})
            );
            let (url, server) = crate::codex::tests::fixture(sse);
            let provider = if deepseek {
                OpenAiProvider::new_deepseek("fake")
            } else {
                OpenAiProvider::new("fake")
            };
            let mut context = StreamRequestContext::new();
            context.reasoning_effort = Some(effort);
            let (tx, mut rx) = mpsc::channel(16);
            provider
                .with_base_url(&url)
                .stream_with_context("fixture", &[], &[], Some(&context), tx)
                .await
                .unwrap();
            let (_, body) = server.join().unwrap();
            if deepseek && effort == ReasoningEffort::None {
                assert_eq!(body["thinking"]["type"], "disabled");
                assert!(body.get("reasoning_effort").is_none());
            } else {
                assert_eq!(body["reasoning_effort"], json!(effort));
                if deepseek {
                    assert_eq!(body["thinking"]["type"], "enabled");
                }
            }
            assert!(matches!(rx.recv().await, Some(StreamEvent::TextDelta(_))));
            assert!(
                matches!(rx.recv().await, Some(StreamEvent::ResponseIdentity(identity)) if identity.model.as_deref() == Some("reported"))
            );
            assert!(
                matches!(rx.recv().await, Some(StreamEvent::Usage(usage)) if usage.input_tokens == 7)
            );
            assert!(matches!(
                rx.recv().await,
                Some(StreamEvent::Completed { .. })
            ));
        }
    }

    #[test]
    fn structured_error_formats_classify_only_exact_codes() {
        for response in [
            json!({"error":{"code":"rate_limit_exceeded","message":"busy"}}),
            json!({"error":{"type":"rate_limit_exceeded","message":"busy"}}),
            json!({"error":{"code":"rate_limit_exceeded","message":"busy","provider":"opencode"}}),
        ] {
            assert_eq!(
                openai_stream_failure(&response).kind,
                ProviderFailureKind::Transient
            );
        }
        assert_eq!(
            openai_stream_failure(&json!({"error":{"code":"invalid_api_key","message":"no"}})).kind,
            ProviderFailureKind::Auth
        );
        assert_eq!(
            openai_stream_failure(
                &json!({"error":{"code":"vendor_custom","message":"rate limit"}})
            )
            .kind,
            ProviderFailureKind::Unknown
        );
        assert_eq!(
            openai_stream_failure(
                &json!({"error":{"type":"FreeUsageLimitError","message":"Free usage limit reached"}})
            )
            .kind,
            ProviderFailureKind::QuotaExceeded
        );
        assert_eq!(
            openai_stream_failure(
                &json!({"error":{"code":"insufficient_quota","message":"You exceeded your current quota"}})
            )
            .kind,
            ProviderFailureKind::QuotaExceeded
        );
    }

    #[tokio::test]
    async fn deepseek_replays_reasoning_and_reports_cache_hits() {
        let sse = format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"choices":[{"delta":{"reasoning_content":"think","tool_calls":[{"index":0,"id":"next","function":{"name":"read","arguments":"{}"}}]}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":100,"completion_tokens":5,"total_tokens":105,"prompt_cache_hit_tokens":80,"prompt_cache_miss_tokens":20}})
        );
        let (url, server) = crate::codex::tests::fixture(sse);
        let mut assistant = ChatMessage::assistant_with_tool_calls(
            "",
            vec![ToolCall {
                id: "old".into(),
                name: "read".into(),
                arguments: "{}".into(),
            }],
        );
        assistant.deepseek_reasoning = Some("earlier thought".into());
        let messages = [
            ChatMessage::system("stable"),
            ChatMessage::user("task"),
            assistant,
            ChatMessage::tool("result", "old"),
        ];
        let (tx, mut rx) = mpsc::channel(16);
        OpenAiProvider::new_deepseek("secret")
            .with_base_url(&url)
            .stream("deepseek-v4-flash", &messages, &[], tx)
            .await
            .unwrap();
        let (headers, body) = server.join().unwrap();
        assert!(headers.starts_with("POST /chat/completions "));
        assert_eq!(body["messages"][0]["content"], "stable");
        assert_eq!(body["messages"][2]["reasoning_content"], "earlier thought");
        assert_eq!(body["messages"][3]["tool_call_id"], "old");
        assert_eq!(
            rx.recv().await,
            Some(StreamEvent::DeepSeekReasoning("think".into()))
        );
        assert!(
            matches!(rx.recv().await, Some(StreamEvent::ToolCallDelta { id: Some(id), .. }) if id == "next")
        );
        assert!(
            matches!(rx.recv().await, Some(StreamEvent::Usage(usage)) if usage.cache_read_tokens == Some(80) && usage.input_tokens == 100)
        );
        assert_eq!(
            rx.recv().await,
            Some(StreamEvent::Completed {
                finish_reason: "tool_calls".into()
            })
        );
    }

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
        assert!(!headers.contains("x-opencode-"));
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
            let terminal = rx.recv().await;
            match sse {
                "data: {\"error\":{\"message\":\"denied\"}}\n\n" => assert!(matches!(
                    terminal,
                    Some(StreamEvent::ProviderError {
                        kind: ProviderFailureKind::Unknown,
                        ..
                    })
                )),
                _ => assert!(matches!(terminal, Some(StreamEvent::Error(_)))),
            }
            assert!(rx.recv().await.is_none());
        }
    }

    #[tokio::test]
    async fn opencode_zen_standard_headers_present() {
        let sse = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]})
        );
        let (url, server) = crate::codex::tests::fixture(sse);
        let (tx, mut rx) = mpsc::channel(8);

        let ctx = StreamRequestContext::from_ids("test-session-123", "test-req-456");

        OpenAiProvider::new("zen-key")
            .with_base_url(&url)
            .with_opencode_profile(false)
            .stream_with_context("zen-model", &[ChatMessage::user("Hi")], &[], Some(&ctx), tx)
            .await
            .unwrap();

        let (headers, _body) = server.join().unwrap();
        assert!(headers.contains("authorization: Bearer zen-key"));
        assert!(headers.contains("x-opencode-session: test-session-123"));
        assert!(headers.contains("x-opencode-request: test-req-456"));
        assert!(headers.contains("x-opencode-client: dume"));
        let expected_ua = format!("user-agent: dume/{}", env!("CARGO_PKG_VERSION"));
        assert!(headers.contains(&expected_ua));
        assert!(headers.contains("x-opencode-project: "));
        // Verify project is stable hex, not absolute path
        assert!(!headers.contains("x-opencode-project: /"));
        assert_eq!(rx.recv().await, Some(StreamEvent::TextDelta("ok".into())));
    }

    #[tokio::test]
    async fn opencode_free_model_works_without_compat_env() {
        unsafe {
            std::env::remove_var("DUME_OPENCODE_FREE_COMPAT");
        }
        let sse = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]})
        );
        let (url, server) = crate::codex::tests::fixture(sse);
        let (tx, mut rx) = mpsc::channel(8);

        let ctx = StreamRequestContext::from_ids("free-sess-default", "free-req-default");

        OpenAiProvider::new("zen-key")
            .with_base_url(&url)
            .with_opencode_profile(true)
            .stream_with_context(
                "nemotron-free",
                &[ChatMessage::user("Hi")],
                &[],
                Some(&ctx),
                tx,
            )
            .await
            .unwrap();

        let (headers, body) = server.join().unwrap();
        assert!(headers.contains("user-agent: opencode/1.19.0"));
        assert!(headers.contains("x-opencode-client: desktop"));
        assert!(headers.contains("x-opencode-project: global"));
        let session_header = headers
            .lines()
            .find(|line| line.starts_with("x-opencode-session: "))
            .unwrap();
        assert!(session_header.starts_with("x-opencode-session: ses_"));
        assert_eq!(
            session_header.trim(),
            format!(
                "x-opencode-session: {}",
                OpenAiProvider::format_opencode_session_id("free-sess-default")
            )
        );
        let request_header = headers
            .lines()
            .find(|line| line.starts_with("x-opencode-request: "))
            .unwrap();
        assert!(request_header.starts_with("x-opencode-request: msg_"));
        assert_eq!(
            request_header.trim(),
            format!(
                "x-opencode-request: {}",
                OpenAiProvider::format_opencode_request_id("free-req-default")
            )
        );
        assert_eq!(body["tools"][0]["function"]["name"], "read");
        assert_eq!(
            body["tools"][0]["function"]["parameters"]["properties"]["filePath"]["type"],
            "string"
        );
        assert_eq!(rx.recv().await, Some(StreamEvent::TextDelta("ok".into())));
    }

    #[tokio::test]
    async fn opencode_free_compat_profile_headers() {
        unsafe {
            std::env::set_var("DUME_OPENCODE_FREE_COMPAT", "1");
        }
        let sse = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]})
        );
        let (url, server) = crate::codex::tests::fixture(sse);
        let (tx, mut rx) = mpsc::channel(8);

        let ctx = StreamRequestContext::from_ids("free-sess-789", "free-req-012");

        OpenAiProvider::new("zen-key")
            .with_base_url(&url)
            .with_opencode_profile(true)
            .stream_with_context(
                "nemotron-free",
                &[ChatMessage::user("Hi")],
                &[],
                Some(&ctx),
                tx,
            )
            .await
            .unwrap();

        let (headers, body) = server.join().unwrap();
        assert!(headers.contains("user-agent: opencode/1.19.0"));
        assert!(headers.contains("x-opencode-client: desktop"));
        assert!(headers.contains("x-opencode-project: global"));
        assert_eq!(
            headers
                .lines()
                .find(|line| line.starts_with("x-opencode-session: "))
                .unwrap()
                .trim(),
            format!(
                "x-opencode-session: {}",
                OpenAiProvider::format_opencode_session_id("free-sess-789")
            )
        );
        assert_eq!(
            headers
                .lines()
                .find(|line| line.starts_with("x-opencode-request: "))
                .unwrap()
                .trim(),
            format!(
                "x-opencode-request: {}",
                OpenAiProvider::format_opencode_request_id("free-req-012")
            )
        );
        assert!(headers.contains("authorization: Bearer zen-key"));
        assert_eq!(body["tools"][0]["function"]["name"], "read");
        assert_eq!(rx.recv().await, Some(StreamEvent::TextDelta("ok".into())));

        unsafe {
            std::env::remove_var("DUME_OPENCODE_FREE_COMPAT");
        }
    }

    #[tokio::test]
    async fn opencode_go_requests_include_headers() {
        let sse = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]})
        );
        let (url, server) = crate::codex::tests::fixture(sse);
        let (tx, mut rx) = mpsc::channel(8);

        let ctx = StreamRequestContext::from_ids("go-session-123", "go-req-456");

        OpenAiProvider::new("go-key")
            .with_base_url(&url)
            .with_opencode_profile(false)
            .stream_with_context(
                "kimi-k2-chat",
                &[ChatMessage::user("Hi")],
                &[],
                Some(&ctx),
                tx,
            )
            .await
            .unwrap();

        let (headers, _body) = server.join().unwrap();
        assert!(headers.contains("authorization: Bearer go-key"));
        assert!(headers.contains("x-opencode-session: go-session-123"));
        assert!(headers.contains("x-opencode-request: go-req-456"));
        assert!(headers.contains("x-opencode-client: dume"));
        assert_eq!(rx.recv().await, Some(StreamEvent::TextDelta("ok".into())));
    }

    #[test]
    fn session_and_request_id_lifecycle() {
        let session1 = StreamRequestContext::new();
        let session2 = StreamRequestContext::new();

        // 1. Session IDs are random and opaque, not equal between different sessions
        assert_ne!(session1.session_id, session2.session_id);
        assert_ne!(session1.session_id, session1.request_id);

        // 2. Next request in same session keeps session_id but changes request_id
        let turn1 = session1.next_request();
        assert_eq!(turn1.session_id, session1.session_id);
        assert_ne!(turn1.request_id, session1.request_id);

        let turn2 = session1.next_request();
        assert_eq!(turn2.session_id, session1.session_id);
        assert_ne!(turn2.request_id, turn1.request_id);

        // 3. Retrying retains the same request context
        let retry_ctx = turn1.clone();
        assert_eq!(retry_ctx.session_id, turn1.session_id);
        assert_eq!(retry_ctx.request_id, turn1.request_id);
    }
}

use crate::client::LlmClient;
use crate::types::{
    ChatMessage, Role, StreamEvent, StreamRequestContext, TokenUsage, ToolDefinition,
};
use anyhow::{Context, Result, bail};
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
}

impl OpenAiProvider {
    pub fn new(api_key: &str) -> Self {
        Self {
            client: LlmClient::new(),
            api_key: api_key.to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            opencode_profile: None,
        }
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
            let _ = tx.send(StreamEvent::Error(error.to_string())).await;
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
        let url = format!("{}/chat/completions", self.base_url);
        let mut headers = LlmClient::build_auth_headers(&self.api_key, false)?;

        if let Some(profile) = &self.opencode_profile {
            let free_compat_enabled = std::env::var("DUME_OPENCODE_FREE_COMPAT")
                .map(|v| v.trim() == "1")
                .unwrap_or(false);

            let default_ctx;
            let ctx = match context {
                Some(ctx) => ctx,
                None => {
                    default_ctx = StreamRequestContext::new();
                    &default_ctx
                }
            };

            if profile.is_free_model || free_compat_enabled {
                headers.insert(USER_AGENT, HeaderValue::from_static("opencode"));
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
                HeaderValue::from_str(&ctx.session_id).context("Invalid session ID header")?,
            );
            headers.insert(
                HeaderName::from_static("x-opencode-request"),
                HeaderValue::from_str(&ctx.request_id).context("Invalid request ID header")?,
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
                        if let Some(usage) = parsed.get("usage") {
                            let input_tokens = usage
                                .get("prompt_tokens")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0);
                            let output_tokens = usage
                                .get("completion_tokens")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0);
                            let total_tokens = usage
                                .get("total_tokens")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(input_tokens + output_tokens);
                            if input_tokens > 0 || output_tokens > 0 || total_tokens > 0 {
                                let _ = tx
                                    .send(StreamEvent::Usage(TokenUsage {
                                        input_tokens,
                                        output_tokens,
                                        total_tokens,
                                    }))
                                    .await;
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
            assert!(matches!(rx.recv().await, Some(StreamEvent::Error(_))));
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

        let ctx = StreamRequestContext {
            session_id: "test-session-123".to_string(),
            request_id: "test-req-456".to_string(),
        };

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

        let ctx = StreamRequestContext {
            session_id: "free-sess-default".to_string(),
            request_id: "free-req-default".to_string(),
        };

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

        let (headers, _body) = server.join().unwrap();
        assert!(headers.contains("user-agent: opencode"));
        assert!(headers.contains("x-opencode-client: desktop"));
        assert!(headers.contains("x-opencode-project: global"));
        assert!(headers.contains("x-opencode-session: free-sess-default"));
        assert!(headers.contains("x-opencode-request: free-req-default"));
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

        let ctx = StreamRequestContext {
            session_id: "free-sess-789".to_string(),
            request_id: "free-req-012".to_string(),
        };

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

        let (headers, _body) = server.join().unwrap();
        assert!(headers.contains("user-agent: opencode"));
        assert!(headers.contains("x-opencode-client: desktop"));
        assert!(headers.contains("x-opencode-project: global"));
        assert!(headers.contains("x-opencode-session: free-sess-789"));
        assert!(headers.contains("x-opencode-request: free-req-012"));
        assert!(headers.contains("authorization: Bearer zen-key"));
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

        let ctx = StreamRequestContext {
            session_id: "go-session-123".to_string(),
            request_id: "go-req-456".to_string(),
        };

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

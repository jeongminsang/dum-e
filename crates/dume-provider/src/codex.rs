use crate::client::LlmClient;
use crate::types::{ChatMessage, Role, StreamEvent, ToolDefinition};
use anyhow::{Context, Result, bail};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tokio::sync::mpsc;

pub struct CodexProvider {
    client: LlmClient,
    access_token: String,
    account_id: String,
    base_url: String,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::types::ToolCall;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    pub(crate) fn fixture(sse: String) -> (String, std::thread::JoinHandle<(String, Value)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let header_end = loop {
                let mut chunk = [0; 1024];
                let size = socket.read(&mut chunk).unwrap();
                assert!(size > 0);
                bytes.extend_from_slice(&chunk[..size]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            while bytes.len() < header_end + length {
                let mut chunk = [0; 1024];
                let size = socket.read(&mut chunk).unwrap();
                assert!(size > 0);
                bytes.extend_from_slice(&chunk[..size]);
            }
            let body = serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", sse.len()).unwrap();
            // Deliberately split frames and UTF-8 across writes.
            for chunk in sse.as_bytes().chunks(7) {
                socket.write_all(chunk).unwrap();
            }
            (headers, body)
        });
        (url, task)
    }

    fn sse(events: &[Value]) -> String {
        events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect()
    }

    #[tokio::test]
    async fn reasoning_snapshots_replay_with_tool_result_on_second_request() {
        for terminal_only in [false, true] {
            let reasoning = json!({"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"opaque-secret"});
            let call = json!({"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":"{}"});
            let mut done_reasoning = reasoning.clone();
            if terminal_only {
                done_reasoning
                    .as_object_mut()
                    .unwrap()
                    .remove("encrypted_content");
            }
            let (url, server) = fixture(sse(&[
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[]}}),
                json!({"type":"response.output_item.done","output_index":0,"item":done_reasoning}),
                json!({"type":"response.output_item.done","output_index":1,"item":call}),
                json!({"type":"response.completed","response":{"status":"completed","output":[reasoning,call]}}),
            ]));
            let mut history = vec![ChatMessage::user("read")];
            let (tx, mut rx) = mpsc::channel(16);
            CodexProvider::new("access", "account")
                .with_base_url(&url)
                .stream("gpt-5", &history, &[], tx)
                .await
                .unwrap();
            server.join().unwrap();
            let mut assistant = ChatMessage::assistant_with_tool_calls("", vec![]);
            let mut reasoning_events = 0;
            while let Some(event) = rx.recv().await {
                match event {
                    StreamEvent::CodexReasoning(items) => {
                        reasoning_events += 1;
                        assistant.codex_reasoning = items;
                    }
                    StreamEvent::ToolCallDelta {
                        id,
                        name,
                        arguments_delta,
                        ..
                    } => {
                        assistant.tool_calls.as_mut().unwrap().push(ToolCall {
                            id: id.unwrap(),
                            name: name.unwrap(),
                            arguments: arguments_delta,
                        });
                    }
                    StreamEvent::Completed { .. } => {}
                    _ => panic!("Unexpected event"),
                }
            }
            assert_eq!(reasoning_events, 1);
            assert_eq!(assistant.codex_reasoning, vec![reasoning.clone()]);
            history.push(assistant);
            history.push(ChatMessage::tool("result", "call_1"));
            let (url, server) = fixture(sse(&[
                json!({"type":"response.completed","response":{"status":"completed"}}),
            ]));
            let (tx, _rx) = mpsc::channel(16);
            CodexProvider::new("access", "account")
                .with_base_url(&url)
                .stream("gpt-5", &history, &[], tx)
                .await
                .unwrap();
            let (_, body) = server.join().unwrap();
            assert_eq!(body["input"][1], reasoning);
            assert_eq!(body["input"][2]["type"], "function_call");
            assert_eq!(body["input"][2]["call_id"], "call_1");
            assert_eq!(body["input"][3]["type"], "function_call_output");
            assert_eq!(body["input"][3]["call_id"], "call_1");
            assert_eq!(body["input"][3]["output"], "result");
        }
    }

    #[tokio::test]
    async fn codex_headers_payload_and_interleaved_tool_replay() {
        let (url, server) = fixture(sse(&[
            json!({"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"fc_a","call_id":"call_a","name":"read","arguments":""}}),
            json!({"type":"response.output_item.added","output_index":4,"item":{"type":"function_call","id":"fc_b","call_id":"call_b","name":"read","arguments":""}}),
            json!({"type":"response.function_call_arguments.delta","item_id":"fc_a","delta":"{\"path\":"}),
            json!({"type":"response.function_call_arguments.delta","output_index":4,"item_id":"fc_b","delta":"{}"}),
            json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"안녕"}),
            json!({"type":"response.function_call_arguments.done","item_id":"fc_a","arguments":"{\"path\":\"a\"}"}),
            json!({"type":"response.done","response":{"status":"completed"}}),
        ]));
        let calls = vec![ToolCall {
            id: "previous".into(),
            name: "read".into(),
            arguments: "{}".into(),
        }];
        let messages = vec![
            ChatMessage::system("Instructions"),
            ChatMessage::user("Hi"),
            ChatMessage::assistant_with_tool_calls("Checking", calls),
            ChatMessage::tool("result", "previous"),
        ];
        let tools = vec![ToolDefinition {
            name: "read".into(),
            description: "Read".into(),
            parameters: json!({"type":"object"}),
        }];
        let (tx, mut rx) = mpsc::channel(32);
        CodexProvider::new("access", "account")
            .with_base_url(&url)
            .stream("gpt-5", &messages, &tools, tx)
            .await
            .unwrap();
        let (headers, body) = server.join().unwrap();
        assert!(headers.starts_with("POST /codex/responses "));
        for header in [
            "authorization: Bearer access",
            "chatgpt-account-id: account",
            "openai-beta: responses=experimental",
            "accept: text/event-stream",
            "originator: pi",
            "user-agent: dume/",
        ] {
            assert!(headers.contains(header), "{headers}");
        }
        assert_eq!(body["store"], false);
        assert_eq!(body["instructions"], "Instructions");
        assert_eq!(body["input"][2]["call_id"], "previous");
        assert_eq!(body["input"][3]["type"], "function_call_output");
        assert_eq!(body["input"][3]["call_id"], "previous");
        assert_eq!(body["tools"][0]["type"], "function");
        let mut args = BTreeMap::<usize, String>::new();
        let mut completed = 0;
        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::ToolCallDelta {
                    index,
                    arguments_delta,
                    ..
                } => args.entry(index).or_default().push_str(&arguments_delta),
                StreamEvent::Completed { finish_reason } => {
                    assert_eq!(finish_reason, "tool_calls");
                    completed += 1;
                }
                StreamEvent::Error(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert_eq!(args[&2], "{\"path\":\"a\"}");
        assert_eq!(args[&4], "{}");
        assert_eq!(completed, 1);
    }

    #[tokio::test]
    async fn codex_errors_and_unexpected_eof_never_complete() {
        for body in [
            sse(&[json!({"type":"error","message":"denied"})]),
            sse(&[json!({"type":"response.failed","response":{"error":{"message":"failed"}}})]),
            sse(&[
                json!({"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"content_filter"}}}),
            ]),
            "data: invalid\n\n".into(),
            "data: [DONE]\n\n".into(),
        ] {
            let (url, server) = fixture(body);
            let (tx, mut rx) = mpsc::channel(16);
            assert!(
                CodexProvider::new("access", "account")
                    .with_base_url(&url)
                    .stream("gpt-5", &[], &[], tx)
                    .await
                    .is_err()
            );
            server.join().unwrap();
            assert!(matches!(rx.recv().await, Some(StreamEvent::Error(_))));
            assert!(rx.recv().await.is_none());
        }
    }

    #[test]
    fn final_snapshots_reconcile_without_duplicate_arguments() {
        let mut state = ResponseState::default();
        state.process(&json!({"type":"response.function_call_arguments.delta","output_index":3,"item_id":"fc","delta":"{"})).unwrap();
        let (events, _) = state.process(&json!({"type":"response.output_item.done","output_index":3,"item":{"type":"function_call","id":"fc","call_id":"call","name":"read","arguments":"{}"}})).unwrap();
        assert_eq!(
            events,
            vec![StreamEvent::ToolCallDelta {
                index: 3,
                id: Some("call".into()),
                name: Some("read".into()),
                arguments_delta: "}".into()
            }]
        );
        assert!(state.process(&json!({"type":"response.function_call_arguments.done","item_id":"fc","arguments":"{}"})).unwrap().0.is_empty());
        assert!(state.process(&json!({"type":"response.function_call_arguments.done","item_id":"fc","arguments":"[]"})).is_err());
        let (events, terminal) = state.process(&json!({"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}}})).unwrap();
        assert!(terminal);
        assert_eq!(
            events.last(),
            Some(&StreamEvent::Completed {
                finish_reason: "length".into()
            })
        );
        assert!(
            CodexProvider::new("bad\nvalue", "account")
                .headers()
                .is_err()
        );
        assert!(
            CodexProvider::new("access", "bad\nvalue")
                .headers()
                .is_err()
        );
    }
}

impl CodexProvider {
    pub fn new(access_token: &str, account_id: &str) -> Self {
        Self {
            client: LlmClient::new(),
            access_token: access_token.to_string(),
            account_id: account_id.to_string(),
            base_url: "https://chatgpt.com/backend-api".to_string(),
        }
    }

    pub fn with_base_url(mut self, base_url: &str) -> Self {
        self.base_url = base_url.trim_end_matches('/').to_string();
        self
    }

    fn url(&self) -> String {
        if self.base_url.ends_with("/codex/responses") {
            self.base_url.clone()
        } else if self.base_url.ends_with("/codex") {
            format!("{}/responses", self.base_url)
        } else {
            format!("{}/codex/responses", self.base_url)
        }
    }

    fn headers(&self) -> Result<HeaderMap> {
        let mut headers = LlmClient::build_auth_headers(&self.access_token, false)?;
        headers.insert(
            "chatgpt-account-id",
            HeaderValue::from_str(&self.account_id).context("Invalid account ID header")?,
        );
        headers.insert("originator", HeaderValue::from_static("pi"));
        headers.insert(
            "user-agent",
            HeaderValue::from_static(concat!("dume/", env!("CARGO_PKG_VERSION"))),
        );
        headers.insert(
            "openai-beta",
            HeaderValue::from_static("responses=experimental"),
        );
        headers.insert("accept", HeaderValue::from_static("text/event-stream"));
        Ok(headers)
    }

    fn body(model: &str, messages: &[ChatMessage], tools: &[ToolDefinition]) -> Result<Value> {
        let mut input = Vec::new();
        for message in messages {
            match message.role {
                Role::System => {}
                Role::User => input.push(json!({"role": "user", "content": [{"type": "input_text", "text": message.content}]})),
                Role::Assistant => {
                    input.extend(message.codex_reasoning.iter().cloned());
                    if !message.content.is_empty() {
                        input.push(json!({"role": "assistant", "content": [{"type": "output_text", "text": message.content, "annotations": []}]}));
                    }
                    for call in message.tool_calls.iter().flatten() {
                        input.push(json!({"type": "function_call", "call_id": call.id, "name": call.name, "arguments": call.arguments}));
                    }
                }
                Role::Tool => {
                    let id = message.tool_call_id.as_deref().filter(|id| !id.is_empty()).context("Tool result requires a call ID")?;
                    input.push(json!({"type": "function_call_output", "call_id": id, "output": message.content}));
                }
            }
        }
        let instructions = messages
            .iter()
            .filter(|m| m.role == Role::System)
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut body = json!({
            "model": model, "store": false, "stream": true,
            "instructions": if instructions.is_empty() { "You are a helpful assistant." } else { &instructions },
            "input": input, "tool_choice": "auto", "parallel_tool_calls": true,
            "text": {"verbosity": "low"}, "include": ["reasoning.encrypted_content"]
        });
        if !tools.is_empty() {
            body["tools"] = json!(
                tools
                    .iter()
                    .map(|tool| json!({
                        "type": "function", "name": tool.name, "description": tool.description,
                        "parameters": tool.parameters, "strict": null
                    }))
                    .collect::<Vec<_>>()
            );
        }
        Ok(body)
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
        let response = self
            .client
            .post_with_retry(
                &self.url(),
                self.headers()?,
                &Self::body(model, messages, tools)?,
                0,
            )
            .await?;
        let mut stream = response.bytes_stream().eventsource();
        let mut state = ResponseState::default();
        while let Some(event) = stream.next().await {
            let event = event.context("Invalid Codex SSE frame")?;
            if event.data.trim() == "[DONE]" {
                continue;
            }
            let value: Value =
                serde_json::from_str(&event.data).context("Invalid Codex SSE JSON")?;
            let (events, terminal) = state.process(&value)?;
            for event in events {
                tx.send(event).await.context("Stream receiver closed")?;
            }
            if terminal {
                return Ok(());
            }
        }
        bail!("Codex stream ended before a terminal response event")
    }
}

#[derive(Default)]
struct CallState {
    item_id: Option<String>,
    call_id: Option<String>,
    name: Option<String>,
    arguments: String,
}

#[derive(Default)]
struct ResponseState {
    calls: BTreeMap<usize, CallState>,
    reasoning: BTreeMap<usize, Value>,
    text: BTreeMap<(usize, usize), String>,
    item_indices: BTreeMap<String, usize>,
}

impl ResponseState {
    fn index(&self, event: &Value) -> Result<usize> {
        if let Some(index) = event["output_index"].as_u64() {
            return usize::try_from(index).context("Output index overflow");
        }
        let id = event["item_id"]
            .as_str()
            .context("Missing output index and item ID")?;
        self.item_indices
            .get(id)
            .copied()
            .or_else(|| {
                self.calls
                    .iter()
                    .find(|(_, call)| call.item_id.as_deref() == Some(id))
                    .map(|(index, _)| *index)
            })
            .context("Unknown Codex item ID")
    }

    fn item(&mut self, index: usize, item: &Value, events: &mut Vec<StreamEvent>) -> Result<()> {
        if let Some(id) = item["id"].as_str() {
            if self
                .item_indices
                .insert(id.to_string(), index)
                .is_some_and(|old| old != index)
            {
                bail!("Conflicting Codex output index");
            }
        }
        if item["type"] == "reasoning" {
            let mut snapshot = item.clone();
            // Terminal output may backfill encryption omitted by output_item.done.
            // Conversely, do not discard encryption if a later snapshot omits it.
            if snapshot["encrypted_content"].is_null() {
                if let Some(previous) = self.reasoning.get(&index) {
                    if !previous["encrypted_content"].is_null() {
                        snapshot["encrypted_content"] = previous["encrypted_content"].clone();
                    }
                }
            }
            self.reasoning.insert(index, snapshot);
            return Ok(());
        }
        if item["type"] == "message" {
            if let Some(content) = item["content"].as_array() {
                for (content_index, part) in content.iter().enumerate() {
                    if let Some(full) = part["text"].as_str().or_else(|| part["refusal"].as_str()) {
                        let text = self.text.entry((index, content_index)).or_default();
                        let suffix = full
                            .strip_prefix(text.as_str())
                            .context("Conflicting Codex message text")?;
                        if !suffix.is_empty() {
                            events.push(StreamEvent::TextDelta(suffix.to_string()));
                        }
                        *text = full.to_string();
                    }
                }
            }
        }
        if item["type"] != "function_call" {
            return Ok(());
        }
        let call = self.calls.entry(index).or_default();
        let id = item["call_id"].as_str().map(str::to_string);
        let name = item["name"].as_str().map(str::to_string);
        if call.call_id.is_some() && id.is_some() && call.call_id != id {
            bail!("Conflicting Codex call ID");
        }
        if call.name.is_some() && name.is_some() && call.name != name {
            bail!("Conflicting Codex tool name");
        }
        let new_id = if call.call_id.is_none() {
            id.clone()
        } else {
            None
        };
        let new_name = if call.name.is_none() {
            name.clone()
        } else {
            None
        };
        if let Some(id) = id {
            call.call_id = Some(id);
        }
        if let Some(name) = name {
            call.name = Some(name);
        }
        if let Some(id) = item["id"].as_str() {
            if call.item_id.as_deref().is_some_and(|old| old != id) {
                bail!("Conflicting Codex item ID");
            }
            call.item_id = Some(id.to_string());
        }
        let suffix = if let Some(args) = item["arguments"].as_str().filter(|s| !s.is_empty()) {
            let suffix = args
                .strip_prefix(call.arguments.as_str())
                .context("Conflicting Codex arguments")?
                .to_string();
            call.arguments = args.to_string();
            suffix
        } else {
            String::new()
        };
        if new_id.is_some() || new_name.is_some() || !suffix.is_empty() {
            events.push(StreamEvent::ToolCallDelta {
                index,
                id: new_id,
                name: new_name,
                arguments_delta: suffix,
            });
        }
        Ok(())
    }

    fn process(&mut self, event: &Value) -> Result<(Vec<StreamEvent>, bool)> {
        let mut events = Vec::new();
        let kind = event["type"].as_str().context("Missing Codex event type")?;
        match kind {
            "error" | "response.failed" | "response.cancelled" => {
                bail!("Codex response error ({})", kind)
            }
            "response.output_item.added" | "response.output_item.done" => {
                let index = self.index(event)?;
                self.item(index, &event["item"], &mut events)?;
            }
            "response.function_call_arguments.delta" | "response.function_call_arguments.done" => {
                let index = self.index(event)?;
                let call = self.calls.entry(index).or_default();
                if let Some(id) = event["item_id"].as_str() {
                    if call.item_id.as_deref().is_some_and(|old| old != id) {
                        bail!("Conflicting Codex item ID");
                    }
                    call.item_id = Some(id.to_string());
                }
                let delta = if kind.ends_with(".done") {
                    let full = event["arguments"]
                        .as_str()
                        .context("Missing final arguments")?;
                    full.strip_prefix(call.arguments.as_str())
                        .context("Conflicting final Codex arguments")?
                        .to_string()
                } else {
                    event["delta"]
                        .as_str()
                        .context("Missing argument delta")?
                        .to_string()
                };
                call.arguments.push_str(&delta);
                if !delta.is_empty() {
                    events.push(StreamEvent::ToolCallDelta {
                        index,
                        id: None,
                        name: None,
                        arguments_delta: delta,
                    });
                }
            }
            "response.output_text.delta"
            | "response.refusal.delta"
            | "response.output_text.done"
            | "response.refusal.done" => {
                let index = self.index(event)?;
                let content_index = event["content_index"].as_u64().unwrap_or(0) as usize;
                let text = self.text.entry((index, content_index)).or_default();
                let delta = if kind.ends_with(".done") {
                    let full = event["text"]
                        .as_str()
                        .or_else(|| event["refusal"].as_str())
                        .context("Missing final text")?;
                    full.strip_prefix(text.as_str())
                        .context("Conflicting final Codex text")?
                        .to_string()
                } else {
                    event["delta"]
                        .as_str()
                        .context("Missing text delta")?
                        .to_string()
                };
                text.push_str(&delta);
                if !delta.is_empty() {
                    events.push(StreamEvent::TextDelta(delta));
                }
            }
            "response.completed" | "response.done" | "response.incomplete" => {
                let response = &event["response"];
                let status =
                    response["status"]
                        .as_str()
                        .unwrap_or(if kind == "response.incomplete" {
                            "incomplete"
                        } else {
                            "completed"
                        });
                let finish = match status {
                    "completed" => "stop",
                    "incomplete"
                        if response["incomplete_details"]["reason"] == "max_output_tokens" =>
                    {
                        "length"
                    }
                    _ => bail!("Codex terminal response error ({})", status),
                };
                if let Some(output) = response["output"].as_array() {
                    for (index, item) in output.iter().enumerate() {
                        self.item(index, item, &mut events)?;
                    }
                }
                if finish != "length" {
                    for call in self.calls.values() {
                        if call.call_id.as_deref().is_none_or(str::is_empty)
                            || call.name.as_deref().is_none_or(str::is_empty)
                        {
                            bail!("Incomplete Codex tool identity");
                        }
                        let _: Value = serde_json::from_str(&call.arguments)
                            .context("Invalid final Codex tool arguments")?;
                    }
                }
                if !self.reasoning.is_empty() {
                    events.push(StreamEvent::CodexReasoning(
                        std::mem::take(&mut self.reasoning).into_values().collect(),
                    ));
                }
                events.push(StreamEvent::Completed {
                    finish_reason: if !self.calls.is_empty() && finish == "stop" {
                        "tool_calls"
                    } else {
                        finish
                    }
                    .to_string(),
                });
                return Ok((events, true));
            }
            _ => {}
        }
        Ok((events, false))
    }
}

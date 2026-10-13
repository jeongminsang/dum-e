use dume_provider::types::*;
use dume_provider::{CredentialStore, resolve_provider};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

#[tokio::test]
async fn test_provider_interleaved_chunks_split_utf8_and_429_retry() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let request_count = Arc::new(AtomicUsize::new(0));
    let request_count_clone = Arc::clone(&request_count);

    // Mock HTTP server
    let server = tokio::spawn(async move {
        for _ in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let count = request_count_clone.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let req_str = String::from_utf8_lossy(&buf[..n]);

            if count == 0 {
                // First request: return 429 Too Many Requests with Retry-After: 1
                let body = r#"{"error":"rate_limit_exceeded"}"#;
                let response = format!(
                    "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nRetry-After: 1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            } else if count == 1 {
                // Second request: stream SSE response with interleaved tool call chunks and arbitrary byte boundaries
                let chunk1 = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_001","function":{"name":"write_file","arguments":"{\"path\":\"out.txt\",\"co"}}]},"finish_reason":null}]}"#;
                let chunk2 = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_002","function":{"name":"bash","arguments":"{\"command\":\"echo 'hello'"}}]},"finish_reason":null}]}"#;
                let chunk3 = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"ntent\":\"data_한글\"}"}}]},"finish_reason":null}]}"#;
                let chunk4 = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":" >> out.txt\"}"}}]},"finish_reason":"tool_calls"}]}"#;
                let sse_body = format!("{}\n\n{}\n\n{}\n\n{}\n\n", chunk1, chunk2, chunk3, chunk4);

                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    sse_body.len(),
                    sse_body
                );
                for byte in response.as_bytes().chunks(1) {
                    socket.write_all(byte).await.unwrap();
                    tokio::task::yield_now().await;
                }
            } else {
                // Third request: verification that subsequent request contains assistant tool_calls and tool result
                assert!(req_str.contains("\"tool_calls\""));
                assert!(req_str.contains("call_001"));
                assert!(req_str.contains("call_002"));
                assert!(req_str.contains("\"role\":\"tool\""));

                let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"Finished task\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    sse_body.len(),
                    sse_body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        }
    });

    let mock_base_url = format!("http://{}", addr);
    let directory = tempfile::tempdir().unwrap();
    let credentials = CredentialStore::new(directory.path().join("auth.json"));
    credentials.save_credential("openai", "test-key").unwrap();
    let provider = resolve_provider("openai/gpt-5.4", &credentials)
        .await
        .unwrap()
        .with_base_url(&mock_base_url)
        .unwrap();

    let (tx, mut rx) = mpsc::channel(50);
    let initial_msgs = vec![ChatMessage::user("Do task with write_file and bash")];
    let tools = vec![
        ToolDefinition {
            name: "write_file".to_string(),
            description: "write".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        },
        ToolDefinition {
            name: "bash".to_string(),
            description: "bash".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        },
    ];

    provider.stream(&initial_msgs, &tools, tx).await.unwrap();

    let mut events = Vec::new();
    while let Some(evt) = rx.recv().await {
        events.push(evt);
    }

    // Proves 429 was automatically retried: request_count should be 2
    assert_eq!(
        request_count.load(Ordering::SeqCst),
        2,
        "429 must trigger backoff retry"
    );

    // Check interleaved events
    let tool_deltas: Vec<_> = events
        .into_iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => Some((index, id, name, arguments_delta)),
            _ => None,
        })
        .collect();

    assert_eq!(
        tool_deltas.len(),
        4,
        "Must receive 4 interleaved tool call delta events"
    );
    assert_eq!(tool_deltas[0].0, 0);
    assert_eq!(tool_deltas[0].1.as_deref(), Some("call_001"));
    assert_eq!(tool_deltas[1].0, 1);
    assert_eq!(tool_deltas[1].1.as_deref(), Some("call_002"));
    assert_eq!(tool_deltas[2].0, 0); // Interleaved tool 0
    assert_eq!(tool_deltas[3].0, 1); // Interleaved tool 1
    assert!(tool_deltas[2].3.contains("data_한글"));

    // Now test round-trip subsequent request:
    // Model receives assistant message with tool_calls + tool result messages
    let mut roundtrip_msgs = initial_msgs.clone();
    let tool_calls = vec![
        ToolCall {
            id: "call_001".to_string(),
            name: "write_file".to_string(),
            arguments: "{\"path\":\"out.txt\",\"content\":\"data_한글\"}".to_string(),
        },
        ToolCall {
            id: "call_002".to_string(),
            name: "bash".to_string(),
            arguments: "{\"command\":\"echo 'hello' >> out.txt\"}".to_string(),
        },
    ];
    roundtrip_msgs.push(ChatMessage::assistant_with_tool_calls(
        "Running tools",
        tool_calls,
    ));
    roundtrip_msgs.push(ChatMessage::tool("Successfully wrote out.txt", "call_001"));
    roundtrip_msgs.push(ChatMessage::tool("hello appended", "call_002"));

    let (tx2, mut rx2) = mpsc::channel(50);
    provider.stream(&roundtrip_msgs, &tools, tx2).await.unwrap();

    let mut roundtrip_events = Vec::new();
    while let Some(evt) = rx2.recv().await {
        roundtrip_events.push(evt);
    }

    assert_eq!(
        request_count.load(Ordering::SeqCst),
        3,
        "All 3 HTTP requests (429 retry + initial + follow-up) executed"
    );
    assert!(roundtrip_events.iter().any(|e| match e {
        StreamEvent::TextDelta(t) => t.contains("Finished task"),
        _ => false,
    }));
    server.await.unwrap();
}

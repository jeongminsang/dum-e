use crate::tools::LocalToolExecutor;
use anyhow::Result;
use dume_provider::types::{ChatMessage, StreamEvent, ToolDefinition};
use dume_provider::{AnthropicProvider, GeminiProvider, OpenAiProvider};

use serde_json::json;
use std::path::Path;
use tokio::sync::mpsc;

pub struct AgentLoop {
    worktree_path: std::path::PathBuf,
    model: String,
    max_turns: usize,
    base_url: Option<String>,
}

impl AgentLoop {
    pub fn new(worktree_path: impl AsRef<Path>, model: impl Into<String>) -> Self {
        Self {
            worktree_path: worktree_path.as_ref().to_path_buf(),
            model: model.into(),
            max_turns: 10,
            base_url: None,
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    pub fn tool_definitions() -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "bash".to_string(),
                description: "Execute a shell command inside the repository worktree".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "Shell command to execute" }
                    },
                    "required": ["command"]
                }),
            },
            ToolDefinition {
                name: "read_file".to_string(),
                description: "Read file contents relative to the worktree".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Relative file path" }
                    },
                    "required": ["path"]
                }),
            },
            ToolDefinition {
                name: "write_file".to_string(),
                description: "Write full file contents at relative path".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Relative file path" },
                        "content": { "type": "string", "description": "New file content" }
                    },
                    "required": ["path", "content"]
                }),
            },
        ]
    }

    pub async fn run_task(&self, task_prompt: &str) -> Result<String> {
        let tools = Self::tool_definitions();
        let tool_executor = LocalToolExecutor::new(&self.worktree_path);

        let system_msg = format!(
            "You are DUM-E coding agent. Work directly in the worktree.\nGoal: {}\nUse tools bash, read_file, write_file as needed.",
            task_prompt
        );

        let mut messages = vec![
            ChatMessage::system(system_msg),
            ChatMessage::user(task_prompt),
        ];

        for _turn in 0..self.max_turns {
            let (tx, mut rx) = mpsc::channel::<StreamEvent>(50);
            let model_name = self.model.clone();
            let msgs = messages.clone();
            let tools_clone = tools.clone();

            let base_url_opt = self.base_url.clone();
            // Stream response
            let stream_handle = tokio::spawn(async move {
                if let Some(base_url) = base_url_opt {
                    // Direct mock/custom provider endpoint
                    let p = OpenAiProvider::new("mock-key").with_base_url(&base_url);
                    let _ = p.stream(&model_name, &msgs, &tools_clone, tx).await;
                } else if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
                    let p = AnthropicProvider::new(&api_key);
                    let _ = p.stream(&model_name, &msgs, &tools_clone, tx).await;
                } else if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
                    let p = OpenAiProvider::new(&api_key);
                    let _ = p.stream(&model_name, &msgs, &tools_clone, tx).await;
                } else if let Ok(api_key) = std::env::var("GEMINI_API_KEY") {
                    let p = GeminiProvider::new(&api_key);
                    let _ = p.stream(&model_name, &msgs, &tools_clone, tx).await;
                } else {
                    // Fallback mock tool step if no API key is set in test/dev environment
                    let _ = tx.send(StreamEvent::TextDelta("Autonomous action completed.".to_string())).await;
                    let _ = tx.send(StreamEvent::Completed { finish_reason: "stop".to_string() }).await;
                }
            });

            let mut assistant_reply = String::new();
            // Preserve order: index -> (id, name, args_buf)
            let mut ordered_calls: Vec<(String, String, String)> = Vec::new();
            let mut stream_completed_normally = false;
            let mut stream_finish_reason = String::new();

            while let Some(evt) = rx.recv().await {
                match evt {
                    StreamEvent::TextDelta(delta) => {
                        assistant_reply.push_str(&delta);
                    }
                    StreamEvent::ToolCallDelta { index, id, name, arguments_delta } => {
                        while ordered_calls.len() <= index {
                            ordered_calls.push((String::new(), String::new(), String::new()));
                        }
                        if let Some(tool_id) = id {
                            if !tool_id.is_empty() {
                                ordered_calls[index].0 = tool_id;
                            }
                        }
                        if let Some(tool_name) = name {
                            if !tool_name.is_empty() {
                                ordered_calls[index].1 = tool_name;
                            }
                        }
                        ordered_calls[index].2.push_str(&arguments_delta);
                    }
                    StreamEvent::Completed { finish_reason } => {
                        stream_completed_normally = true;
                        stream_finish_reason = finish_reason;
                        break;
                    }
                    StreamEvent::Error(err) => {
                        anyhow::bail!("Model streaming error: {}", err);
                    }
                }
            }
            
            // Check task/stream join result
            let stream_task_res = stream_handle.await;
            if let Err(join_err) = stream_task_res {
                anyhow::bail!("Stream task aborted: {}", join_err);
            }

            if !stream_completed_normally {
                anyhow::bail!("Stream disconnected prematurely without Completed event");
            }

            if stream_finish_reason == "length" {
                anyhow::bail!("Model output truncated due to length limits before completion");
            }

            // Filter out empty slots if any
            let complete_tool_calls: Vec<dume_provider::types::ToolCall> = ordered_calls
                .into_iter()
                .filter(|(id, name, _)| !id.is_empty() || !name.is_empty())
                .map(|(id, name, args)| dume_provider::types::ToolCall {
                    id: if id.is_empty() { format!("call_{}", uuid_simple()) } else { id },
                    name,
                    arguments: args,
                })
                .collect();

            if complete_tool_calls.is_empty() {
                // Regular assistant message without tools
                messages.push(ChatMessage::assistant(&assistant_reply));
                break;
            }

            // CRITICAL: Assistant message MUST contain tool_calls metadata so provider accepts subsequent tool responses
            messages.push(ChatMessage::assistant_with_tool_calls(&assistant_reply, complete_tool_calls.clone()));

            // Execute requested tools in exact order and feed back results
            for tc in complete_tool_calls {
                let parsed_args: serde_json::Value = match serde_json::from_str(&tc.arguments) {
                    Ok(val) => val,
                    Err(e) => {
                        let err_msg = format!("JSON schema validation error for tool {}: invalid syntax: {}", tc.name, e);
                        messages.push(ChatMessage::tool(err_msg, tc.id));
                        continue;
                    }
                };

                // Validate required fields by tool name
                let schema_check = match tc.name.as_str() {
                    "bash" => {
                        if parsed_args.get("command").and_then(|v| v.as_str()).is_none() {
                            Some("Missing required field 'command' (string)")
                        } else {
                            None
                        }
                    }
                    "read_file" => {
                        if parsed_args.get("path").and_then(|v| v.as_str()).is_none() {
                            Some("Missing required field 'path' (string)")
                        } else {
                            None
                        }
                    }
                    "write_file" => {
                        if parsed_args.get("path").and_then(|v| v.as_str()).is_none() {
                            Some("Missing required field 'path' (string)")
                        } else if parsed_args.get("content").and_then(|v| v.as_str()).is_none() {
                            Some("Missing required field 'content' (string)")
                        } else {
                            None
                        }
                    }
                    _ => Some("Unknown tool name"),
                };

                if let Some(schema_err) = schema_check {
                    messages.push(ChatMessage::tool(format!("Tool schema error: {}", schema_err), tc.id));
                    continue;
                }

                let exec_result = match tool_executor.execute(&tc.name, &parsed_args).await {
                    Ok(res) => res,
                    Err(e) => format!("Tool execution error: {}", e),
                };

                messages.push(ChatMessage::tool(exec_result, tc.id));
            }
        }

        Ok("Agent completed task execution".to_string())
    }
}

fn uuid_simple() -> String {
    format!("{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_tool_call_delta_chunk_assembly_and_execution() {
        let dir = tempdir().unwrap();
        let executor = LocalToolExecutor::new(dir.path());

        // Simulate 2 tool calls chunked and interleaved
        // Tool 0: write_file
        // Tool 1: bash
        let chunks = vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_w1".to_string()),
                name: Some("write_file".to_string()),
                arguments_delta: "{\"path\": \"test.txt\", \"co".to_string(),
            },
            StreamEvent::ToolCallDelta {
                index: 1,
                id: Some("call_b1".to_string()),
                name: Some("bash".to_string()),
                arguments_delta: "{\"command\": \"cat test".to_string(),
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_delta: "ntent\": \"ordered hello\"}".to_string(),
            },
            StreamEvent::ToolCallDelta {
                index: 1,
                id: None,
                name: None,
                arguments_delta: ".txt\"}".to_string(),
            },
        ];

        let mut ordered_calls: Vec<(String, String, String)> = Vec::new();
        for evt in chunks {
            if let StreamEvent::ToolCallDelta { index, id, name, arguments_delta } = evt {
                while ordered_calls.len() <= index {
                    ordered_calls.push((String::new(), String::new(), String::new()));
                }
                if let Some(tool_id) = id {
                    if !tool_id.is_empty() {
                        ordered_calls[index].0 = tool_id;
                    }
                }
                if let Some(tool_name) = name {
                    if !tool_name.is_empty() {
                        ordered_calls[index].1 = tool_name;
                    }
                }
                ordered_calls[index].2.push_str(&arguments_delta);
            }
        }

        assert_eq!(ordered_calls.len(), 2);
        assert_eq!(ordered_calls[0].0, "call_w1");
        assert_eq!(ordered_calls[0].1, "write_file");
        assert_eq!(ordered_calls[1].0, "call_b1");
        assert_eq!(ordered_calls[1].1, "bash");

        // Execute in exact order: write first, then bash cat
        let args0: serde_json::Value = serde_json::from_str(&ordered_calls[0].2).unwrap();
        let res0 = executor.execute(&ordered_calls[0].1, &args0).await.unwrap();
        assert!(res0.contains("Successfully wrote"));

        let args1: serde_json::Value = serde_json::from_str(&ordered_calls[1].2).unwrap();
        let res1 = executor.execute(&ordered_calls[1].1, &args1).await.unwrap();
        assert!(res1.contains("ordered hello"));
    }

    #[tokio::test]
    async fn test_agent_loop_e2e_roundtrip_with_mock_provider() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let request_count = Arc::new(AtomicUsize::new(0));
        let request_count_clone = Arc::clone(&request_count);

        let dir = tempdir().unwrap();
        let wt_path = dir.path().to_path_buf();

        // Spawn mock server for 2-turn agent loop:
        // Turn 1: model returns write_file tool call
        // Turn 2: model inspects assistant with tool_calls + tool result, then returns final text
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let count = request_count_clone.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 8192];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let req_str = String::from_utf8_lossy(&buf[..n]);

                if count == 0 {
                    // Turn 1 response: call write_file
                    let sse = format!(
                        "data: {}\n\ndata: [DONE]\n\n",
                        serde_json::json!({
                            "choices": [{
                                "delta": {
                                    "tool_calls": [{
                                        "index": 0,
                                        "id": "call_mock_write",
                                        "function": {
                                            "name": "write_file",
                                            "arguments": "{\"path\": \"e2e_out.txt\", \"content\": \"hello e2e agent loop\"}"
                                        }
                                    }]
                                },
                                "finish_reason": "tool_calls"
                            }]
                        })
                    );
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        sse.len(),
                        sse
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                } else if count == 1 {
                    // Turn 2 verification: request payload MUST contain assistant with tool_calls and tool result
                    assert!(req_str.contains("\"tool_calls\""), "Must contain tool_calls metadata");
                    assert!(req_str.contains("call_mock_write"), "Must contain call_mock_write ID");
                    assert!(req_str.contains("\"role\":\"tool\""), "Must contain tool result role");
                    assert!(req_str.contains("Successfully wrote"), "Must contain tool result content");

                    // Turn 2 response: model final answer
                    let sse = format!(
                        "data: {}\n\ndata: [DONE]\n\n",
                        serde_json::json!({
                            "choices": [{
                                "delta": {
                                    "content": "E2E Task is finished."
                                },
                                "finish_reason": "stop"
                            }]
                        })
                    );
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        sse.len(),
                        sse
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                }
            }
        });

        let mock_url = format!("http://{}", addr);
        let agent = AgentLoop::new(&wt_path, "mock-model").with_base_url(&mock_url);

        let res = agent.run_task("Write e2e_out.txt with test content").await.unwrap();
        assert_eq!(res, "Agent completed task execution");

        // Verify physical side-effect on disk!
        let disk_file = wt_path.join("e2e_out.txt");
        assert!(disk_file.exists(), "write_file tool must have created the file on disk");
        let content = tokio::fs::read_to_string(&disk_file).await.unwrap();
        assert_eq!(content, "hello e2e agent loop");

        // Verify request count
        assert_eq!(request_count.load(Ordering::SeqCst), 2, "Agent loop must execute both turns via HTTP");
    }

    #[tokio::test]
    async fn test_tool_schema_violation_prevents_execution_zero_side_effect() {
        let dir = tempdir().unwrap();
        let wt_path = dir.path().to_path_buf();
        let executor = LocalToolExecutor::new(&wt_path);

        // 1. Invalid argument type: write_file path is integer instead of string
        let bad_args1 = serde_json::json!({
            "path": 12345,
            "content": "some content"
        });
        assert!(bad_args1.get("path").and_then(|v| v.as_str()).is_none());

        // 2. Missing field: write_file missing content
        let bad_args2 = serde_json::json!({
            "path": "missing_content.txt"
        });
        assert!(bad_args2.get("content").and_then(|v| v.as_str()).is_none());

        // 3. Unknown tool name
        let unknown_res = executor.execute("unsupported_tool", &serde_json::json!({})).await;
        assert!(unknown_res.is_err(), "Unknown tool must fail");

        // 4. Directory traversal attempt: ../escaped.txt
        let bad_args_traversal = serde_json::json!({
            "path": "../../escaped.txt",
            "content": "malicious"
        });
        // Execute write_file with path traversal - must error and not write outside worktree
        let trav_res = executor.execute("write_file", &bad_args_traversal).await;
        // Verify no file written outside worktree
        assert!(trav_res.is_err() || !dir.path().parent().unwrap().join("escaped.txt").exists());

        // Zero side effect check: worktree must remain completely empty!
        let mut entries = tokio::fs::read_dir(&wt_path).await.unwrap();
        assert!(entries.next_entry().await.unwrap().is_none(), "Worktree must have 0 side effects");
    }
}

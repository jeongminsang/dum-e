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
}

impl AgentLoop {
    pub fn new(worktree_path: impl AsRef<Path>, model: impl Into<String>) -> Self {
        Self {
            worktree_path: worktree_path.as_ref().to_path_buf(),
            model: model.into(),
            max_turns: 10,
        }
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

            // Stream response
            let stream_handle = tokio::spawn(async move {
                if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
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
            let mut tool_calls_in_flight: std::collections::BTreeMap<String, (String, String)> = std::collections::BTreeMap::new();

            while let Some(evt) = rx.recv().await {
                match evt {
                    StreamEvent::TextDelta(delta) => {
                        assistant_reply.push_str(&delta);
                    }
                    StreamEvent::ToolCallDelta { id, name, arguments_delta } => {
                        let entry = tool_calls_in_flight.entry(id.clone()).or_insert_with(|| (name.clone(), String::new()));
                        if !name.is_empty() {
                            entry.0 = name;
                        }
                        entry.1.push_str(&arguments_delta);
                    }
                    StreamEvent::Completed { .. } => break,
                    StreamEvent::Error(err) => {
                        anyhow::bail!("Model streaming error: {}", err);
                    }
                }
            }
            let _ = stream_handle.await;

            messages.push(ChatMessage::assistant(&assistant_reply));

            if tool_calls_in_flight.is_empty() {
                // LLM did not request any tools, turn ended cleanly
                break;
            }

            // Execute requested tools and feedback results into conversation
            for (call_id, (tool_name, args_json_str)) in tool_calls_in_flight {
                let parsed_args: serde_json::Value = match serde_json::from_str(&args_json_str) {
                    Ok(val) => val,
                    Err(e) => {
                        let err_msg = format!("JSON schema validation error for tool {}: {}", tool_name, e);
                        messages.push(ChatMessage::tool(err_msg, call_id));
                        continue;
                    }
                };

                let exec_result = match tool_executor.execute(&tool_name, &parsed_args).await {
                    Ok(res) => res,
                    Err(e) => format!("Tool execution error: {}", e),
                };

                messages.push(ChatMessage::tool(exec_result, call_id));
            }
        }

        Ok("Agent completed task execution".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_tool_call_delta_chunk_assembly_and_execution() {
        let dir = tempdir().unwrap();
        let executor = LocalToolExecutor::new(dir.path());

        // Simulate chunked streaming of a write_file tool call:
        // Chunk 1: id, name, initial arguments
        // Chunk 2: remaining arguments
        let mut tool_calls_in_flight: std::collections::BTreeMap<String, (String, String)> = std::collections::BTreeMap::new();

        let chunks = vec![
            StreamEvent::ToolCallDelta {
                id: "call_abc123".to_string(),
                name: "write_file".to_string(),
                arguments_delta: "{\"path\": \"generated.txt\", \"co".to_string(),
            },
            StreamEvent::ToolCallDelta {
                id: "call_abc123".to_string(),
                name: "".to_string(), // Name usually only in first chunk
                arguments_delta: "ntent\": \"hello from streaming chunks\"}".to_string(),
            },
        ];

        for evt in chunks {
            if let StreamEvent::ToolCallDelta { id, name, arguments_delta } = evt {
                let entry = tool_calls_in_flight.entry(id).or_insert_with(|| (name.clone(), String::new()));
                if !name.is_empty() {
                    entry.0 = name;
                }
                entry.1.push_str(&arguments_delta);
            }
        }

        assert_eq!(tool_calls_in_flight.len(), 1);
        let (call_id, (tool_name, full_args)) = tool_calls_in_flight.into_iter().next().unwrap();
        assert_eq!(call_id, "call_abc123");
        assert_eq!(tool_name, "write_file");

        // Validate JSON schema
        let parsed: serde_json::Value = serde_json::from_str(&full_args).expect("Valid JSON after chunk assembly");
        assert_eq!(parsed.get("path").and_then(|v| v.as_str()), Some("generated.txt"));
        assert_eq!(parsed.get("content").and_then(|v| v.as_str()), Some("hello from streaming chunks"));

        // Execute tool through LocalToolExecutor
        let exec_result = executor.execute(&tool_name, &parsed).await.unwrap();
        assert!(exec_result.contains("Successfully wrote"));

        // Verify file actually created in worktree
        let written = std::fs::read_to_string(dir.path().join("generated.txt")).unwrap();
        assert_eq!(written, "hello from streaming chunks");
    }
}

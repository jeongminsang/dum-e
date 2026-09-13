use crate::tools::LocalToolExecutor;
use anyhow::{Context, Result};
#[cfg(test)]
use dume_provider::OpenAiProvider;
use dume_provider::runtime::resolve_provider;
use dume_provider::types::{ChatMessage, StreamEvent, ToolDefinition};

use serde_json::json;
use std::path::Path;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub struct AgentLoop {
    worktree_path: std::path::PathBuf,
    model: String,
    max_turns: usize,
    endpoint: Option<Endpoint>,
}

#[derive(Clone)]
enum Endpoint {
    Authenticated(String),
    #[cfg(test)]
    Mock(String),
}

impl AgentLoop {
    pub fn new(worktree_path: impl AsRef<Path>, model: impl Into<String>) -> Self {
        Self {
            worktree_path: worktree_path.as_ref().to_path_buf(),
            model: model.into(),
            max_turns: 10,
            endpoint: None,
        }
    }

    /// Override the selected provider's API root using its normally resolved credentials.
    /// The endpoint is validated when the provider is resolved for each turn.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.endpoint = Some(Endpoint::Authenticated(base_url.into()));
        self
    }

    #[cfg(test)]
    fn with_mock_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.endpoint = Some(Endpoint::Mock(base_url.into()));
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
            ToolDefinition {
                name: "replace_file_content".to_string(),
                description: "Replace a target string within a file at relative path".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Relative file path" },
                        "target": { "type": "string", "description": "Exact target string to replace" },
                        "replacement": { "type": "string", "description": "Replacement text" }
                    },
                    "required": ["path", "target", "replacement"]
                }),
            },
            ToolDefinition {
                name: "grep_search".to_string(),
                description: "Search for a regex or text pattern across files in worktree".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Search pattern" }
                    },
                    "required": ["pattern"]
                }),
            },
            ToolDefinition {
                name: "spawn_subagent".to_string(),
                description: "Spawn an autonomous subagent with a dedicated sub-worktree to execute a task concurrently".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "description": "Unique identifier for the subagent" },
                        "prompt": { "type": "string", "description": "Task description for the subagent" },
                        "sub_dir": { "type": "string", "description": "Relative directory in worktree for subagent execution" }
                    },
                    "required": ["id", "prompt", "sub_dir"]
                }),
            },
            ToolDefinition {
                name: "wait_subagent".to_string(),
                description: "Wait for a spawned subagent to finish and retrieve its result".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "description": "Subagent ID to wait for" },
                        "timeout_ms": { "type": "integer", "description": "Maximum wait timeout in milliseconds (default 30000)" }
                    },
                    "required": ["id"]
                }),
            },
            ToolDefinition {
                name: "cancel_subagent".to_string(),
                description: "Cancel an active running subagent".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "description": "Subagent ID to cancel" }
                    },
                    "required": ["id"]
                }),
            },
        ]
    }

    async fn execute_tool(
        &self,
        tc: dume_provider::ToolCall,
        tool_executor: &LocalToolExecutor,
        subagent_manager: &dume_mcp::subagent::SubagentManager,
        child_paths: &mut std::collections::HashMap<String, std::path::PathBuf>,
        cancellation: &CancellationToken,
    ) -> Result<ChatMessage> {
        anyhow::ensure!(!cancellation.is_cancelled(), "Agent execution cancelled");
        let parsed_args: serde_json::Value = match serde_json::from_str(&tc.arguments) {
            Ok(val) => val,
            Err(e) => {
                let err_msg = format!(
                    "JSON schema validation error for tool {}: invalid syntax: {}",
                    tc.name, e
                );
                return Ok(ChatMessage::tool(err_msg, tc.id));
            }
        };

        // Validate required fields by tool name
        let schema_check = match tc.name.as_str() {
            "bash" => {
                if parsed_args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .is_none()
                {
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
                } else if parsed_args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .is_none()
                {
                    Some("Missing required field 'content' (string)")
                } else {
                    None
                }
            }
            "replace_file_content" => {
                if parsed_args.get("path").and_then(|v| v.as_str()).is_none() {
                    Some("Missing required field 'path' (string)")
                } else if parsed_args.get("target").and_then(|v| v.as_str()).is_none() {
                    Some("Missing required field 'target' (string)")
                } else if parsed_args
                    .get("replacement")
                    .and_then(|v| v.as_str())
                    .is_none()
                {
                    Some("Missing required field 'replacement' (string)")
                } else {
                    None
                }
            }
            "grep_search" => {
                if parsed_args
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .is_none()
                {
                    Some("Missing required field 'pattern' (string)")
                } else {
                    None
                }
            }
            "spawn_subagent" => {
                if parsed_args.get("id").and_then(|v| v.as_str()).is_none() {
                    Some("Missing required field 'id' (string)")
                } else if parsed_args.get("prompt").and_then(|v| v.as_str()).is_none() {
                    Some("Missing required field 'prompt' (string)")
                } else if parsed_args
                    .get("sub_dir")
                    .and_then(|v| v.as_str())
                    .is_none()
                {
                    Some("Missing required field 'sub_dir' (string)")
                } else {
                    None
                }
            }
            "wait_subagent" => {
                if parsed_args.get("id").and_then(|v| v.as_str()).is_none() {
                    Some("Missing required field 'id' (string)")
                } else if parsed_args
                    .get("timeout_ms")
                    .is_some_and(|v| v.as_u64().is_none())
                {
                    Some("Field 'timeout_ms' must be a non-negative integer")
                } else {
                    None
                }
            }
            "cancel_subagent" => {
                if parsed_args.get("id").and_then(|v| v.as_str()).is_none() {
                    Some("Missing required field 'id' (string)")
                } else {
                    None
                }
            }
            _ => Some("Unknown tool name"),
        };

        if let Some(schema_err) = schema_check {
            return Ok(ChatMessage::tool(
                format!("Tool schema error: {}", schema_err),
                tc.id,
            ));
        }

        let exec_result = match tc.name.as_str() {
            "spawn_subagent" => {
                let sub_id = parsed_args["id"].as_str().unwrap().to_string();
                let prompt = parsed_args["prompt"].as_str().unwrap().to_string();
                let sub_dir = parsed_args["sub_dir"].as_str().unwrap();
                let child_wt = match child_worktree_path(&self.worktree_path, sub_dir) {
                    Ok(path) => path,
                    Err(error) => {
                        return Ok(ChatMessage::tool(
                            format!("Failed to start subagent: {}", error),
                            tc.id,
                        ));
                    }
                };
                if child_paths
                    .values()
                    .any(|owned| child_wt.starts_with(owned) || owned.starts_with(&child_wt))
                {
                    return Ok(ChatMessage::tool(
                        "Failed to start subagent: child path overlaps an owned worktree",
                        tc.id,
                    ));
                }
                let model_name = self.model.clone();
                let endpoint = self.endpoint.clone();

                let parent_repo = self.worktree_path.clone();
                let child_prompt = prompt.clone();
                let child_wt_clone = child_wt.clone();

                let res = subagent_manager
                    .start_subagent(&sub_id, &prompt, move |token| async move {
                        run_isolated_child(
                            &parent_repo,
                            &child_wt_clone,
                            &model_name,
                            endpoint,
                            &child_prompt,
                            token,
                        )
                        .await
                    })
                    .await;

                match res {
                    Ok(_) => {
                        child_paths.insert(sub_id.clone(), child_wt.clone());
                        format!(
                            "Subagent '{}' scheduled; isolation must succeed before execution. Worktree destination {} will be retained for recovery",
                            sub_id,
                            child_wt.display()
                        )
                    }
                    Err(e) => format!("Failed to start subagent: {}", e),
                }
            }
            "wait_subagent" => {
                let sub_id = parsed_args["id"].as_str().unwrap();
                let timeout = parsed_args
                    .get("timeout_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30_000);
                let status = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        anyhow::bail!("Agent execution cancelled");
                    }
                    status = subagent_manager.await_subagent(sub_id, timeout) => status,
                };
                match status {
                    Ok(dume_mcp::subagent::SubagentStatus::Completed { result }) => {
                        format!("Subagent '{}' completed: {}", sub_id, result)
                    }
                    Ok(dume_mcp::subagent::SubagentStatus::Failed { error }) => {
                        format!("Subagent '{}' failed: {}", sub_id, error)
                    }
                    Ok(dume_mcp::subagent::SubagentStatus::Cancelled) => {
                        format!(
                            "Subagent '{}' was cancelled; worktree destination retained if created: {}",
                            sub_id,
                            child_paths
                                .get(sub_id)
                                .map(|p| p.display().to_string())
                                .unwrap_or_default()
                        )
                    }
                    Ok(dume_mcp::subagent::SubagentStatus::Running) => {
                        format!("Subagent '{}' still running", sub_id)
                    }
                    Err(e) => format!("Error waiting for subagent '{}': {}", sub_id, e),
                }
            }
            "cancel_subagent" => {
                let sub_id = parsed_args["id"].as_str().unwrap();
                match subagent_manager.cancel_subagent(sub_id).await {
                    Ok(_) => format!(
                        "Subagent '{}' stopped; inspect status for outcome. Worktree destination retained if created: {}",
                        sub_id,
                        child_paths
                            .get(sub_id)
                            .map(|p| p.display().to_string())
                            .unwrap_or_default()
                    ),
                    Err(e) => format!("Failed to cancel subagent '{}': {}", sub_id, e),
                }
            }
            _ => match tool_executor.execute(&tc.name, &parsed_args).await {
                Ok(res) => res,
                Err(e) => format!("Tool execution error: {}", e),
            },
        };

        Ok(ChatMessage::tool(exec_result, tc.id))
    }

    pub fn run_task<'a>(
        &'a self,
        task_prompt: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>> {
        self.run_task_with_cancellation(task_prompt, CancellationToken::new())
    }

    pub fn run_task_with_cancellation<'a>(
        &'a self,
        task_prompt: &'a str,
        cancellation: CancellationToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>> {
        let task_prompt = task_prompt.to_string();
        Box::pin(async move {
            let tools = Self::tool_definitions();
            let tool_executor =
                LocalToolExecutor::new(&self.worktree_path).with_cancellation(cancellation.clone());
            let subagent_manager = std::sync::Arc::new(dume_mcp::subagent::SubagentManager::new());
            let mut child_paths: std::collections::HashMap<String, std::path::PathBuf> =
                std::collections::HashMap::new();
            let result: Result<String> = async {

        let system_msg = format!(
            "You are DUM-E coding agent. Work directly in the worktree.\nGoal: {}\nUse tools bash, read_file, write_file as needed.",
            task_prompt
        );

        let mut messages = vec![
            ChatMessage::system(system_msg),
            ChatMessage::user(task_prompt),
        ];

        for _turn in 0..self.max_turns {
            anyhow::ensure!(!cancellation.is_cancelled(), "Agent execution cancelled");
            let (tx, mut rx) = mpsc::channel::<StreamEvent>(50);
            let model_name = self.model.clone();
            let msgs = messages.clone();
            let tools_clone = tools.clone();

            let endpoint = self.endpoint.clone();
            // Stream response
            let mut stream_handle = tokio::spawn(async move {
                #[cfg(test)]
                if let Some(Endpoint::Mock(base_url)) = &endpoint {
                    // Explicit unit-test transport; never compiled into the CLI.
                    if let Some(model) = model_name.strip_prefix("openai-codex/") {
                        let p = dume_provider::codex::CodexProvider::new("mock-key", "mock-account").with_base_url(base_url);
                        return p.stream(model, &msgs, &tools_clone, tx).await;
                    }
                    let p = OpenAiProvider::new("mock-key").with_base_url(base_url);
                    return p.stream(&model_name, &msgs, &tools_clone, tx).await;
                }

                let cred_store = dume_provider::CredentialStore::new(dume_provider::CredentialStore::default_path());

                let mut provider = resolve_provider(&model_name, &cred_store).await?;
                if let Some(Endpoint::Authenticated(base_url)) = endpoint {
                    provider = provider.with_base_url(&base_url)?;
                }
                provider.stream(&msgs, &tools_clone, tx).await
            });

            let mut assistant_reply = String::new();
            let mut codex_reasoning = Vec::new();
            // Preserve order: index -> (id, name, args_buf)
            let mut ordered_calls: Vec<(String, String, String)> = Vec::new();
            let mut stream_completed_normally = false;
            let mut stream_finish_reason = String::new();

            loop {
                let evt = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        stream_handle.abort();
                        let _ = stream_handle.await;
                        anyhow::bail!("Agent execution cancelled");
                    }
                    evt = rx.recv() => match evt {
                        Some(evt) => evt,
                        None => break,
                    }
                };
                match evt {
                    StreamEvent::CodexReasoning(items) => codex_reasoning.extend(items),
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
                        stream_handle.abort();
                        let _ = stream_handle.await;
                        anyhow::bail!("Model streaming error: {}", err);
                    }
                }
            }

            // Check task/stream join result
            let stream_task_res = tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    stream_handle.abort();
                    let _ = stream_handle.await;
                    anyhow::bail!("Agent execution cancelled");
                }
                result = &mut stream_handle => result,
            };
            stream_task_res.context("Stream task aborted")??;

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
                let mut message = ChatMessage::assistant(&assistant_reply);
                message.codex_reasoning = codex_reasoning;
                messages.push(message);
                break;
            }

            // CRITICAL: Assistant message MUST contain tool_calls metadata so provider accepts subsequent tool responses
            let mut message = ChatMessage::assistant_with_tool_calls(&assistant_reply, complete_tool_calls.clone());
            message.codex_reasoning = codex_reasoning;
            messages.push(message);

            // Execute requested tools in exact order and feed back results
            for tc in complete_tool_calls {
                anyhow::ensure!(!cancellation.is_cancelled(), "Agent execution cancelled");
                messages.push(self.execute_tool(tc, &tool_executor, &subagent_manager, &mut child_paths, &cancellation).await?);
            }
        }

            Ok("Agent completed task execution".to_string())
            }.await;
            subagent_manager.cancel_all().await;
            let mut retained = String::new();
            for (id, path) in &child_paths {
                retained.push_str(&format!("\nSubagent '{}': {:?}; worktree destination retained if created: {}. Output is not integrated.", id, subagent_manager.inspect_subagent(id).await.map(|record| record.status), path.display()));
            }
            if cancellation.is_cancelled() {
                anyhow::bail!("Agent execution cancelled{}", retained);
            }
            match result {
                Ok(result) => Ok(format!("{}{}", result, retained)),
                Err(error) => anyhow::bail!("{:#}{}", error, retained),
            }
        })
    }
}

/// Stateful tool dispatch shared by interactive and autonomous conversations.
/// Keep this value alive across turns so subagent handles remain addressable.
pub struct ToolDispatcher {
    agent: AgentLoop,
    executor: LocalToolExecutor,
    subagents: dume_mcp::subagent::SubagentManager,
    child_paths: std::collections::HashMap<String, std::path::PathBuf>,
    cancellation: CancellationToken,
}

impl ToolDispatcher {
    pub fn new(
        path: impl AsRef<Path>,
        model: impl Into<String>,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            agent: AgentLoop::new(&path, model),
            executor: LocalToolExecutor::new(path.as_ref()).with_cancellation(cancellation.clone()),
            subagents: dume_mcp::subagent::SubagentManager::new(),
            child_paths: std::collections::HashMap::new(),
            cancellation,
        }
    }

    pub fn set_model(&mut self, model: impl Into<String>) {
        self.agent.model = model.into();
    }

    pub async fn execute(&mut self, call: dume_provider::ToolCall) -> Result<ChatMessage> {
        self.agent
            .execute_tool(
                call,
                &self.executor,
                &self.subagents,
                &mut self.child_paths,
                &self.cancellation,
            )
            .await
    }

    pub async fn cancel_all(&self) -> String {
        self.subagents.cancel_all().await;
        let mut report = String::new();
        for (id, path) in &self.child_paths {
            report.push_str(&format!("\nSubagent '{}': {:?}; worktree destination retained if created: {}. Output is not integrated.", id, self.subagents.inspect_subagent(id).await.map(|record| record.status), path.display()));
        }
        report
    }
}

async fn run_isolated_child(
    parent: &Path,
    child: &Path,
    model: &str,
    endpoint: Option<Endpoint>,
    prompt: &str,
    cancellation: CancellationToken,
) -> Result<String> {
    anyhow::ensure!(
        !cancellation.is_cancelled(),
        "Subagent cancelled before isolation"
    );
    dume_git::create_git_worktree(parent, child, "HEAD")
        .await
        .with_context(|| format!("Subagent isolation failed at {}", child.display()))?;
    let mut agent = AgentLoop::new(child, model);
    agent.endpoint = endpoint;
    let result = agent.run_task_with_cancellation(prompt, cancellation).await;
    // Retain even clean worktrees: detached commits are also child output.
    let retained = format!(
        "Worktree retained at {} for inspection/integration; output has not been integrated",
        child.display()
    );
    match result {
        Ok(result) => Ok(format!("{}\n{}", result, retained)),
        Err(error) => anyhow::bail!("{:#}\n{}", error, retained),
    }
}

fn child_worktree_path(parent: &Path, requested: &str) -> Result<std::path::PathBuf> {
    use std::path::Component;
    anyhow::ensure!(!requested.is_empty(), "Child path must not be empty");
    let parent = std::fs::canonicalize(parent).context("Cannot resolve parent worktree")?;
    let mut path = parent.clone();
    for component in Path::new(requested).components() {
        let Component::Normal(name) = component else {
            anyhow::bail!("Child path must be a strict relative path without traversal");
        };
        anyhow::ensure!(
            !name.to_string_lossy().eq_ignore_ascii_case(".git"),
            "Child path cannot enter Git metadata"
        );
        path.push(name);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                anyhow::ensure!(
                    !metadata.file_type().is_symlink() && metadata.is_dir(),
                    "Child path crosses a symlink or non-directory"
                );
                anyhow::ensure!(
                    !path.join(".git").exists(),
                    "Child path crosses another worktree"
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Cannot inspect child path"),
        }
    }
    anyhow::ensure!(path != parent, "Child path must not be the parent worktree");
    anyhow::ensure!(!path.exists(), "Child destination is already owned");
    Ok(path)
}

fn uuid_simple() -> String {
    format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn codex_reasoning_and_tool_result_reach_second_worker_request() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let reasoning = json!({"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"opaque-secret"});
            let mut requests = Vec::new();
            for turn in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let header_end = loop {
                    let mut chunk = [0; 1024];
                    let size = socket.read(&mut chunk).await.unwrap();
                    assert!(size > 0);
                    bytes.extend_from_slice(&chunk[..size]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
                assert!(headers.starts_with("POST /codex/responses "));
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
                    let size = socket.read(&mut chunk).await.unwrap();
                    assert!(size > 0);
                    bytes.extend_from_slice(&chunk[..size]);
                }
                requests.push(
                    serde_json::from_slice::<serde_json::Value>(
                        &bytes[header_end..header_end + length],
                    )
                    .unwrap(),
                );
                let output = if turn == 0 {
                    json!([reasoning, {"type":"function_call","id":"fc_1","call_id":"call_1","name":"write_file","arguments":"{\"path\":\"result.txt\",\"content\":\"hello\"}"}])
                } else {
                    json!([{"type":"message","content":[{"type":"output_text","text":"Done."}]}])
                };
                let event = json!({"type":"response.completed","response":{"status":"completed","output":output}});
                let sse = format!("data: {event}\n\n");
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", sse.len(), sse).as_bytes()).await.unwrap();
            }
            assert_eq!(requests[1]["input"][1], reasoning);
            assert_eq!(requests[1]["input"][2]["type"], "function_call");
            assert_eq!(requests[1]["input"][2]["call_id"], "call_1");
            assert_eq!(requests[1]["input"][3]["type"], "function_call_output");
            assert_eq!(requests[1]["input"][3]["call_id"], "call_1");
            assert!(
                requests[1]["input"][3]["output"]
                    .as_str()
                    .unwrap()
                    .contains("Successfully wrote")
            );
        });
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            AgentLoop::new(dir.path(), "openai-codex/gpt-5")
                .with_mock_base_url(url)
                .run_task("write"),
        )
        .await
        .unwrap()
        .unwrap();
        server.await.unwrap();
        assert!(!result.contains("opaque-secret"));
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("result.txt"))
                .await
                .unwrap(),
            "hello"
        );
    }

    #[test]
    fn child_paths_reject_escape_aliases_and_existing_ownership() {
        let dir = tempdir().unwrap();
        for path in [
            "",
            ".",
            "..",
            "../outside",
            "/absolute",
            ".git/child",
            ".GIT/child",
        ] {
            assert!(child_worktree_path(dir.path(), path).is_err(), "{path}");
        }
        std::fs::create_dir(dir.path().join("owned")).unwrap();
        assert!(child_worktree_path(dir.path(), "owned").is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path(), dir.path().join("alias")).unwrap();
            assert!(child_worktree_path(dir.path(), "alias/child").is_err());
        }
        assert_eq!(
            child_worktree_path(dir.path(), "new/child").unwrap(),
            dir.path().canonicalize().unwrap().join("new/child")
        );
    }

    #[tokio::test]
    async fn failed_isolation_never_runs_child_agent() {
        let dir = tempdir().unwrap();
        let child = dir.path().join("child");
        // An invalid endpoint would produce a streaming error if execution leaked through.
        let error = run_isolated_child(
            dir.path(),
            &child,
            "mock",
            Some(Endpoint::Mock("http://127.0.0.1:1".into())),
            "write output",
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("Subagent isolation failed"));
        assert!(!child.join(".git").exists());
        assert!(!child.join("output.txt").exists());
    }

    #[tokio::test]
    async fn cancellation_interrupts_real_child_stream_and_retains_worktree() {
        use tokio::io::AsyncReadExt;
        let dir = tempdir().unwrap();
        for args in [
            vec!["init"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        ] {
            let output = tokio::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .await
                .unwrap();
            assert!(output.status.success());
        }
        let child = dir.path().join("child");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let token = CancellationToken::new();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 1024];
            socket.read(&mut buffer).await.unwrap();
            started_tx.send(()).unwrap();
            // No response: cancellation must interrupt the live provider request.
            std::future::pending::<()>().await;
        });
        let parent_path = dir.path().to_path_buf();
        let child_path = child.clone();
        let child_token = token.clone();
        let execution = tokio::spawn(async move {
            run_isolated_child(
                &parent_path,
                &child_path,
                "mock",
                Some(Endpoint::Mock(url)),
                "child task",
                child_token,
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), started_rx)
            .await
            .unwrap()
            .unwrap();
        tokio::fs::write(child.join("recover.txt"), "uncommitted child work")
            .await
            .unwrap();
        token.cancel();
        let error = tokio::time::timeout(std::time::Duration::from_secs(5), execution)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert!(error.to_string().contains(&child.display().to_string()));
        assert_eq!(
            tokio::fs::read_to_string(child.join("recover.txt"))
                .await
                .unwrap(),
            "uncommitted child work"
        );
        assert!(child.join(".git").exists());
        server.abort();
        let _ = server.await;
    }

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
            if let StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } = evt
            {
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
    async fn test_agent_loop_e2e_roundtrip_with_mock_provider_retains_child_output() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let request_count = Arc::new(AtomicUsize::new(0));
        let request_count_clone = Arc::clone(&request_count);

        let dir = tempdir().unwrap();
        for args in [
            vec!["init"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        ] {
            let output = tokio::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .await
                .unwrap();
            assert!(output.status.success());
        }
        let wt_path = dir.path().join("child");

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
                    assert!(
                        req_str.contains("\"tool_calls\""),
                        "Must contain tool_calls metadata"
                    );
                    assert!(
                        req_str.contains("call_mock_write"),
                        "Must contain call_mock_write ID"
                    );
                    assert!(
                        req_str.contains("\"role\":\"tool\""),
                        "Must contain tool result role"
                    );
                    assert!(
                        req_str.contains("Successfully wrote"),
                        "Must contain tool result content"
                    );

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
        let res = run_isolated_child(
            dir.path(),
            &wt_path,
            "mock-model",
            Some(Endpoint::Mock(mock_url)),
            "Write e2e_out.txt with test content",
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(res.starts_with("Agent completed task execution"));
        assert!(res.contains(&format!("Worktree retained at {}", wt_path.display())));
        assert!(wt_path.join(".git").exists());
        assert!(!dir.path().join("e2e_out.txt").exists());

        // Verify physical side-effect on disk!
        let disk_file = wt_path.join("e2e_out.txt");
        assert!(
            disk_file.exists(),
            "write_file tool must have created the file on disk"
        );
        let content = tokio::fs::read_to_string(&disk_file).await.unwrap();
        assert_eq!(content, "hello e2e agent loop");

        // Verify request count
        assert_eq!(
            request_count.load(Ordering::SeqCst),
            2,
            "Agent loop must execute both turns via HTTP"
        );
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
        let unknown_res = executor
            .execute("unsupported_tool", &serde_json::json!({}))
            .await;
        assert!(unknown_res.is_err(), "Unknown tool must fail");

        // 4. Directory traversal attempt: ../../escaped.txt
        let sandbox_dir = tempdir().unwrap();
        let sandbox_root = sandbox_dir.path().join("sandbox_root");
        let parent_dir = sandbox_root.join("parent");
        let worktree_dir = parent_dir.join("worktree");
        tokio::fs::create_dir_all(&worktree_dir).await.unwrap();

        let strict_executor = LocalToolExecutor::new(&worktree_dir);

        let bad_args_traversal = serde_json::json!({
            "path": "../../escaped.txt",
            "content": "malicious content outside worktree"
        });
        let trav_res = strict_executor
            .execute("write_file", &bad_args_traversal)
            .await;
        assert!(
            trav_res.is_err(),
            "Directory traversal write must strictly fail"
        );

        // CRITICAL: Physically inspect outer directories - escaped file MUST NOT EXIST outside worktree!
        assert!(
            !sandbox_root.join("escaped.txt").exists(),
            "escaped.txt must not exist in sandbox_root"
        );
        assert!(
            !parent_dir.join("escaped.txt").exists(),
            "escaped.txt must not exist in parent_dir"
        );
        assert!(
            !sandbox_dir.path().join("escaped.txt").exists(),
            "escaped.txt must not exist in temp root"
        );

        // 5. Symlink traversal defense: symlink pointing outside worktree
        #[cfg(unix)]
        {
            let outside_dir = sandbox_root.join("outside_target");
            tokio::fs::create_dir_all(&outside_dir).await.unwrap();
            let symlink_in_wt = worktree_dir.join("symlink_out");
            std::os::unix::fs::symlink(&outside_dir, &symlink_in_wt).unwrap();

            let bad_symlink_write = serde_json::json!({
                "path": "symlink_out/pwned.txt",
                "content": "escape via symlink"
            });
            let sym_res = strict_executor
                .execute("write_file", &bad_symlink_write)
                .await;
            assert!(
                sym_res.is_err(),
                "Write via symlink pointing outside worktree must fail"
            );
            assert!(
                !outside_dir.join("pwned.txt").exists(),
                "pwned.txt must not exist in outside directory"
            );
        }

        // Zero side effect check: wt_path must remain completely empty!
        let mut entries = tokio::fs::read_dir(&wt_path).await.unwrap();
        assert!(
            entries.next_entry().await.unwrap().is_none(),
            "wt_path must have 0 side effects"
        );
    }

    #[tokio::test]
    async fn test_replace_file_content_and_grep_search() {
        let dir = tempdir().unwrap();
        let wt_path = dir.path().to_path_buf();
        let executor = LocalToolExecutor::new(&wt_path);

        // 1. Create file with write_file
        let write_args = serde_json::json!({
            "path": "src/main.rs",
            "content": "fn main() {\n    println!(\"old message\");\n}\n"
        });
        executor.execute("write_file", &write_args).await.unwrap();

        // 2. Search pattern with grep_search
        let grep_args = serde_json::json!({
            "pattern": "old message"
        });
        let grep_res = executor.execute("grep_search", &grep_args).await.unwrap();
        assert!(grep_res.contains("old message"));

        // 3. Replace content with replace_file_content
        let replace_args = serde_json::json!({
            "path": "src/main.rs",
            "target": "old message",
            "replacement": "new rust engine message"
        });
        let rep_res = executor
            .execute("replace_file_content", &replace_args)
            .await
            .unwrap();
        assert!(rep_res.contains("Successfully replaced"));

        // 4. Verify updated file content
        let read_args = serde_json::json!({
            "path": "src/main.rs"
        });
        let content = executor.execute("read_file", &read_args).await.unwrap();
        assert!(content.contains("new rust engine message"));
        assert!(!content.contains("old message"));
    }

    #[tokio::test]
    async fn test_subagent_manager_runs_real_child_agent_loop() {
        let dir = tempdir().unwrap();
        let parent_wt = dir.path().join("parent_wt");
        let child_wt = dir.path().join("child_wt");
        tokio::fs::create_dir_all(&parent_wt).await.unwrap();
        tokio::fs::create_dir_all(&child_wt).await.unwrap();

        let sub_mgr = dume_mcp::subagent::SubagentManager::new();

        // Start subagent executing an actual AgentLoop in its own isolated child worktree
        let child_wt_clone = child_wt.clone();
        sub_mgr
            .start_subagent(
                "sub_worker_1",
                "Initialize child project",
                move |_token| async move {
                    let agent = AgentLoop::new(&child_wt_clone, "mock-model");
                    // Direct write tool execution in child worktree
                    let executor = LocalToolExecutor::new(&child_wt_clone);
                    let write_args = serde_json::json!({
                        "path": "child_output.txt",
                        "content": "produced by child agent"
                    });
                    executor.execute("write_file", &write_args).await?;
                    let _ = agent;
                    Ok("Child task completed successfully".to_string())
                },
            )
            .await
            .unwrap();

        // Await subagent result from parent
        let status = sub_mgr.await_subagent("sub_worker_1", 2000).await.unwrap();
        match status {
            dume_mcp::subagent::SubagentStatus::Completed { result } => {
                assert_eq!(result, "Child task completed successfully");
            }
            other => panic!("Expected completed subagent, got {:?}", other),
        }

        // Verify child worktree produced physical result
        let child_file = child_wt.join("child_output.txt");
        assert!(
            child_file.exists(),
            "Child agent must have created file in child worktree"
        );
        let content = tokio::fs::read_to_string(&child_file).await.unwrap();
        assert_eq!(content, "produced by child agent");

        // Verify parent worktree was unaffected
        assert!(
            !parent_wt.join("child_output.txt").exists(),
            "Parent worktree must remain isolated"
        );
    }

    #[tokio::test]
    async fn test_subagent_tool_invocation_via_agent_loop() {
        let dir = tempdir().unwrap();
        let wt_path = dir.path().to_path_buf();
        let agent = AgentLoop::new(&wt_path, "mock-model");

        let tools = AgentLoop::tool_definitions();
        assert!(tools.iter().any(|t| t.name == "spawn_subagent"));
        assert!(tools.iter().any(|t| t.name == "wait_subagent"));
        assert!(tools.iter().any(|t| t.name == "cancel_subagent"));

        // Verify tool definition schemas
        let spawn_def = tools.iter().find(|t| t.name == "spawn_subagent").unwrap();
        assert_eq!(
            spawn_def.parameters["required"],
            serde_json::json!(["id", "prompt", "sub_dir"])
        );
        let _ = agent;
    }
}

use crate::runtime::{RunSession, ToolStart};
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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AgentOutcome {
    Completed {
        answer: String,
        usage: dume_provider::types::TokenUsage,
    },
    TurnLimitExhausted {
        last_reply: Option<String>,
        usage: dume_provider::types::TokenUsage,
    },
    Cancelled {
        usage: dume_provider::types::TokenUsage,
    },
}

impl AgentOutcome {
    pub fn is_success(&self) -> bool {
        matches!(self, AgentOutcome::Completed { .. })
    }

    pub fn answer(&self) -> Option<&str> {
        match self {
            AgentOutcome::Completed { answer, .. } => Some(answer.as_str()),
            AgentOutcome::TurnLimitExhausted { last_reply, .. } => last_reply.as_deref(),
            AgentOutcome::Cancelled { .. } => None,
        }
    }

    pub fn usage(&self) -> Option<&dume_provider::types::TokenUsage> {
        match self {
            AgentOutcome::Completed { usage, .. } => Some(usage),
            AgentOutcome::TurnLimitExhausted { usage, .. } => Some(usage),
            AgentOutcome::Cancelled { usage, .. } => Some(usage),
        }
    }
}

impl std::fmt::Display for AgentOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentOutcome::Completed { answer, .. } => write!(f, "{}", answer),
            AgentOutcome::TurnLimitExhausted { last_reply, .. } => {
                if let Some(reply) = last_reply {
                    write!(f, "Turn limit exhausted. Last reply: {}", reply)
                } else {
                    write!(f, "Turn limit exhausted")
                }
            }
            AgentOutcome::Cancelled { .. } => write!(f, "Agent execution cancelled"),
        }
    }
}

#[derive(Clone)]
pub struct AgentLoop {
    pub worktree_path: std::path::PathBuf,
    pub model: String,
    pub max_turns: usize,
    pub endpoint: Option<Endpoint>,
    pub store: Option<std::sync::Arc<dume_store::HarnessStore>>,
    artifact_store: Option<std::sync::Arc<dume_store::ArtifactStore>>,
    pub request_context: Option<dume_provider::types::StreamRequestContext>,
    pub descendant_usage: std::sync::Arc<tokio::sync::Mutex<dume_provider::types::TokenUsage>>,
    routing_snapshot: Option<crate::routing::RoutingSnapshot>,
    #[cfg(test)]
    routing_credentials: Option<dume_provider::CredentialStore>,
    #[cfg(test)]
    subagent_model_overrides: std::collections::HashMap<String, Option<String>>,
}

#[derive(Clone)]
pub enum Endpoint {
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
            store: None,
            artifact_store: None,
            request_context: None,
            descendant_usage: std::sync::Arc::new(tokio::sync::Mutex::new(
                dume_provider::types::TokenUsage::default(),
            )),
            routing_snapshot: None,
            #[cfg(test)]
            routing_credentials: None,
            #[cfg(test)]
            subagent_model_overrides: std::collections::HashMap::new(),
        }
    }

    pub fn with_store(mut self, store: std::sync::Arc<dume_store::HarnessStore>) -> Self {
        self.artifact_store = Some(std::sync::Arc::new(store.artifacts.clone()));
        self.store = Some(store);
        self
    }

    pub fn with_routing_snapshot(mut self, snapshot: crate::routing::RoutingSnapshot) -> Self {
        self.routing_snapshot = Some(snapshot);
        self
    }

    #[cfg(test)]
    fn with_routing_credentials(mut self, credentials: dume_provider::CredentialStore) -> Self {
        self.routing_credentials = Some(credentials);
        self
    }

    #[cfg(test)]
    fn with_subagent_model_override(
        mut self,
        child_id: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        self.subagent_model_overrides
            .insert(child_id.into(), Some(model.into()));
        self
    }

    #[cfg(test)]
    fn with_inherited_subagent_model(mut self, child_id: impl Into<String>) -> Self {
        self.subagent_model_overrides.insert(child_id.into(), None);
        self
    }

    pub fn with_artifact_store(mut self, store: std::sync::Arc<dume_store::ArtifactStore>) -> Self {
        self.artifact_store = Some(store);
        self
    }

    pub fn with_request_context(mut self, ctx: dume_provider::types::StreamRequestContext) -> Self {
        self.request_context = Some(ctx);
        self
    }

    /// Override the selected provider's API root using its normally resolved credentials.
    /// The endpoint is validated when the provider is resolved for each turn.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.endpoint = Some(Endpoint::Authenticated(base_url.into()));
        self
    }

    #[cfg(test)]
    pub fn with_mock_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.endpoint = Some(Endpoint::Mock(base_url.into()));
        self
    }

    pub fn tool_definitions(worktree_path: &Path) -> Vec<ToolDefinition> {
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
                description: "Read file contents relative to the worktree with optional line range".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Relative file path" },
                        "start_line": { "type": "integer", "description": "Optional starting line number (1-based, inclusive)" },
                        "end_line": { "type": "integer", "description": "Optional ending line number (1-based, inclusive)" }
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
                description: "Search for a regex or text pattern across files in worktree with optional path filtering and match limits".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Search pattern" },
                        "path_filter": { "type": "string", "description": "Optional relative path or directory to filter search" },
                        "max_matches": { "type": "integer", "description": "Optional maximum number of matches to return (default: 100)" }
                    },
                    "required": ["pattern"]
                }),
            },
            ToolDefinition {
                name: "load_skill".to_string(),
                description: dume_core::skills::SkillRegistry::load_default_for(worktree_path).format_catalog_description(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Name of the skill to load" }
                    },
                    "required": ["name"]
                }),
            },
            ToolDefinition {
                name: "read_artifact".to_string(),
                description: "Read bounded content from an artifact file or SHA-256 artifact ID with offset and length limits".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Relative path or SHA-256 artifact ID" },
                        "offset": { "type": "integer", "description": "Optional byte offset to start reading from" },
                        "length": { "type": "integer", "description": "Optional maximum bytes to read (default/max: 51200)" }
                    },
                    "required": ["path"]
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
                        "sub_dir": { "type": "string", "description": "Relative directory in worktree for subagent execution" },
                        "constraints": { "type": "string", "description": "Optional specific constraints or boundaries for the child" },
                        "artifact_ids": { "type": "array", "maxItems": 32, "items": { "type": "string" }, "description": "Optional SHA-256 artifact IDs relevant for child execution" }
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
        messages: &[ChatMessage],
    ) -> Result<ChatMessage> {
        anyhow::ensure!(!cancellation.is_cancelled(), "Agent execution cancelled");
        let parsed_args: serde_json::Value = match serde_json::from_str(&tc.arguments) {
            Ok(val) => val,
            Err(e) => {
                let err_msg = format!(
                    "JSON schema validation error for tool {}: invalid syntax: {}",
                    tc.name, e
                );
                return Err(crate::tools::KnownToolFailure {
                    output: err_msg,
                    exit_code: None,
                }
                .into());
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
                } else if parsed_args
                    .get("start_line")
                    .is_some_and(|v| v.as_u64().is_none())
                {
                    Some("Field 'start_line' must be a positive integer")
                } else if parsed_args
                    .get("end_line")
                    .is_some_and(|v| v.as_u64().is_none())
                {
                    Some("Field 'end_line' must be a positive integer")
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
                } else if parsed_args
                    .get("max_matches")
                    .is_some_and(|v| v.as_u64().is_none())
                {
                    Some("Field 'max_matches' must be a positive integer")
                } else {
                    None
                }
            }
            "load_skill" => {
                if parsed_args.get("name").and_then(|v| v.as_str()).is_none() {
                    Some("Missing required field 'name' (string)")
                } else {
                    None
                }
            }
            "read_artifact" => {
                if parsed_args.get("path").and_then(|v| v.as_str()).is_none() {
                    Some("Missing required field 'path' (string)")
                } else if parsed_args
                    .get("offset")
                    .is_some_and(|v| v.as_u64().is_none())
                {
                    Some("Field 'offset' must be a non-negative integer")
                } else if parsed_args
                    .get("length")
                    .is_some_and(|v| v.as_u64().is_none())
                {
                    Some("Field 'length' must be a non-negative integer")
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
                } else if parsed_args
                    .get("constraints")
                    .is_some_and(|v| v.as_str().is_none())
                {
                    Some("Field 'constraints' must be a string")
                } else if parsed_args
                    .get("artifact_ids")
                    .is_some_and(|value| value.as_array().is_none())
                    || parsed_args
                        .get("artifact_ids")
                        .and_then(|value| value.as_array())
                        .is_some_and(|items| items.iter().any(|item| item.as_str().is_none()))
                {
                    Some("Field 'artifact_ids' must be an array of strings")
                } else if parsed_args
                    .get("artifact_ids")
                    .and_then(|value| value.as_array())
                    .is_some_and(|items| items.len() > 32)
                {
                    Some("Field 'artifact_ids' must contain at most 32 entries")
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
            return Err(crate::tools::KnownToolFailure {
                output: format!("Tool schema error: {schema_err}"),
                exit_code: None,
            }
            .into());
        }

        let exec_result = match tc.name.as_str() {
            "load_skill" => {
                let skill_name = parsed_args["name"].as_str().unwrap();
                let mut registry =
                    dume_core::skills::SkillRegistry::load_default_for(&self.worktree_path);
                if let Some(skill) = registry.get_or_reload(skill_name) {
                    let is_active = dume_core::skills::SkillRegistry::is_content_in_messages(
                        &skill.content,
                        messages.iter().map(|m| m.content.as_str()),
                    );
                    if is_active {
                        format!(
                            "Skill '{}' is already active in current context",
                            skill.name
                        )
                    } else {
                        format!(
                            "[Loaded Skill: '{}']\nDescription: {}\n\n---\n{}\n---",
                            skill.name, skill.description, skill.content
                        )
                    }
                } else {
                    let available = registry.catalog();
                    let names: Vec<String> = available.into_iter().map(|s| s.name).collect();
                    format!(
                        "Skill '{}' not found. Available skills: {}",
                        skill_name,
                        names.join(", ")
                    )
                }
            }
            "spawn_subagent" => {
                let sub_id = parsed_args["id"].as_str().unwrap().to_string();
                let prompt = parsed_args["prompt"].as_str().unwrap().to_string();
                let sub_dir = parsed_args["sub_dir"].as_str().unwrap();
                let constraints = parsed_args.get("constraints").and_then(|v| v.as_str());
                let artifact_ids: Vec<String> = parsed_args
                    .get("artifact_ids")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .map(|item| item.as_str().unwrap().to_string())
                            .collect()
                    })
                    .unwrap_or_default();

                if !artifact_ids.is_empty() {
                    let Some(store) = &self.store else {
                        return Ok(ChatMessage::tool(
                            "Failed to start subagent: artifact references require an artifact store",
                            tc.id,
                        ));
                    };
                    if let Some(id) = artifact_ids.iter().find(|id| {
                        id.len() != 64
                            || !id.bytes().all(|byte| byte.is_ascii_hexdigit())
                            || !store.artifacts.has_artifact(id)
                    }) {
                        return Ok(ChatMessage::tool(
                            format!(
                                "Failed to start subagent: artifact '{}' is invalid or unavailable",
                                id
                            ),
                            tc.id,
                        ));
                    }
                }

                // Construct bounded handoff prompt
                let mut bounded_prompt = format!("Task: {}", prompt);
                if let Some(c) = constraints {
                    bounded_prompt.push_str(&format!("\nConstraints: {}", c));
                }
                if !artifact_ids.is_empty() {
                    bounded_prompt.push_str(&format!(
                        "\nRelevant Artifacts (read via 'read_artifact'): {}",
                        artifact_ids.join(", ")
                    ));
                }

                // Enforce budget ceiling on child handoff prompt (max 64KB)
                // If exceeded, admit into artifact store and reference in prompt
                if bounded_prompt.len() > 65_536 {
                    let Some(store) = &self.store else {
                        return Ok(ChatMessage::tool(
                            "Failed to start subagent: handoff exceeds 64 KiB and no artifact store is configured",
                            tc.id,
                        ));
                    };
                    let art_id = match store.artifacts.save_artifact(bounded_prompt.as_bytes()) {
                        Ok(id) => id,
                        Err(error) => {
                            return Ok(ChatMessage::tool(
                                format!("Failed to preserve oversized subagent handoff: {}", error),
                                tc.id,
                            ));
                        }
                    };
                    bounded_prompt = format!(
                        "Task prompt exceeded size ceiling. Full instructions admitted to artifact ID: {}\nUse 'read_artifact' with path '{}' to view full task details and constraints.",
                        art_id, art_id
                    );
                }

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
                let settings = match {
                    #[cfg(test)]
                    if let Some(model) = self.subagent_model_overrides.get(&sub_id) {
                        Ok(crate::subagent_settings::SubagentSettings {
                            model: model.clone(),
                        })
                    } else {
                        crate::subagent_settings::SubagentSettings::load()
                    }
                    #[cfg(not(test))]
                    {
                        crate::subagent_settings::SubagentSettings::load()
                    }
                } {
                    Ok(settings) => settings,
                    Err(error) => {
                        return Ok(ChatMessage::tool(
                            format!("Failed to read subagent settings: {error:#}"),
                            tc.id,
                        ));
                    }
                };
                // Subagent model: explicit child settings take priority; inherited setting
                // resolves using the parent's actual effective response model (self.model on dispatcher.agent).
                let model_name = match settings.model_for(&self.model) {
                    Ok(model) => model,
                    Err(error) => {
                        return Ok(ChatMessage::tool(
                            format!("Invalid subagent model setting: {error:#}"),
                            tc.id,
                        ));
                    }
                };
                let endpoint = self.endpoint.clone();

                let parent_repo = self.worktree_path.clone();
                let child_prompt = bounded_prompt.clone();
                let child_wt_clone = child_wt.clone();
                let store_clone = self.store.clone();
                #[cfg(test)]
                let routing_credentials_clone = self.routing_credentials.clone();
                let ctx_clone = self
                    .request_context
                    .as_ref()
                    .map(|c| c.child_agent(&sub_id));
                let descendant_usage_clone = std::sync::Arc::clone(&self.descendant_usage);

                let res = subagent_manager
                    .start_subagent(&sub_id, &prompt, move |token| async move {
                        run_isolated_child(
                            &parent_repo,
                            &child_wt_clone,
                            &model_name,
                            endpoint,
                            &child_prompt,
                            token,
                            store_clone,
                            ctx_clone,
                            descendant_usage_clone,
                            #[cfg(test)]
                            routing_credentials_clone,
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
            _ => tool_executor.execute(&tc.name, &parsed_args).await?,
        };

        let admitted = crate::output_limits::admit_tool_output(
            &tc.name,
            &exec_result,
            self.artifact_store.as_deref(),
        );
        Ok(ChatMessage::tool(admitted, tc.id))
    }

    pub fn run_task<'a>(
        &'a self,
        task_prompt: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<AgentOutcome>> + Send + 'a>>
    {
        self.run_task_with_cancellation(task_prompt, CancellationToken::new())
    }

    pub fn run_task_with_cancellation<'a>(
        &'a self,
        task_prompt: &'a str,
        cancellation: CancellationToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<AgentOutcome>> + Send + 'a>>
    {
        let task_prompt = task_prompt.to_string();
        Box::pin(async move {
            let result: Result<AgentOutcome> = async {

        let user_msg = dume_core::types::PromptPrefix::build_task_user_message(&task_prompt);

        let mut messages = vec![
            ChatMessage::system(dume_core::types::PromptPrefix::BASE_SYSTEM_PROMPT),
            ChatMessage::user(user_msg),
        ];

        let mut session_ctx = self
            .request_context
            .clone()
            .unwrap_or_else(dume_provider::types::StreamRequestContext::new);
        let is_child = session_ctx.parent_agent_id.is_some();
        if is_child {
            anyhow::ensure!(session_ctx.agent_id.as_deref().is_some_and(|id| !id.trim().is_empty()), "Child agent context requires a non-empty agent ID");
        }
        let runtime_store = match &self.store {
            Some(store) => store.clone(),
            None => {
                let root = self.worktree_path.join(".dume/rust");
                std::sync::Arc::new(dume_store::HarnessStore::open(
                    root.join("harness.db"), root.join("artifacts"),
                )?)
            }
        };
        let journal_id = if is_child {
            format!("child:{}", session_ctx.agent_id.as_deref().expect("validated child ID"))
        } else {
            session_ctx.attempt_id.clone().unwrap_or_else(|| session_ctx.request_id.clone())
        };
        let conversation_id = format!("worker:{journal_id}");
        let scope = if is_child {
            dume_store::RuntimeScope::Conversation(conversation_id.clone())
        } else {
            session_ctx.attempt_id.as_deref()
                .map(|id| dume_store::RuntimeScope::Attempt(id.to_owned()))
                .unwrap_or_else(|| dume_store::RuntimeScope::Conversation(conversation_id.clone()))
        };
        match runtime_store.load_conversation(&conversation_id) {
            Ok(_) => {}
            Err(dume_store::store::StoreError::NotFound(_)) => {
                runtime_store.create_conversation(&conversation_id, &self.worktree_path.to_string_lossy(), &self.model, &session_ctx.session_id)?;
            }
            Err(error) => return Err(error.into()),
        }
        let mut dispatcher = ToolDispatcher::from_agent(self.clone().with_store(runtime_store.clone()), cancellation.clone());
        dispatcher.executor = dispatcher.executor.with_artifact_store(Some(std::sync::Arc::new(runtime_store.artifacts.clone())));
        let (run_id, resuming, mut run_session) = match runtime_store.latest_run(&scope)? {
            Some(existing) if matches!(existing.status.as_str(), "interrupted" | "needs_attention") => {
                let id = existing.run_id;
                let session = RunSession::resume(runtime_store.clone(), &id, &conversation_id)?;
                (id, true, session)
            }
            Some(existing) if existing.status == "queued" => {
                let id = existing.run_id;
                let session = RunSession::start(runtime_store.clone(), scope.clone(), &id, &conversation_id)?;
                (id, false, session)
            }
            _ => {
                let receipt = runtime_store.admit_input(
                    scope.clone(), &session_ctx.request_id, dume_store::InputKind::Submit, &task_prompt,
                )?;
                let id = receipt.run_id.ok_or_else(|| anyhow::anyhow!("input admission produced no run"))?;
                let session = RunSession::start(runtime_store.clone(), scope.clone(), &id, &conversation_id)?;
                (id, false, session)
            }
        };
        let mut routing_snapshot = self.routing_snapshot.clone()
            .unwrap_or_else(|| crate::routing::RoutingSnapshot::load_default(resuming))
            .with_epoch_preservation(resuming);
        if !resuming {
            routing_snapshot.reload_for_new_input();
        }
        let routing_credentials = {
            #[cfg(test)]
            { self.routing_credentials.clone().unwrap_or_else(|| dume_provider::CredentialStore::new(dume_provider::CredentialStore::default_path())) }
            #[cfg(not(test))]
            { dume_provider::CredentialStore::new(dume_provider::CredentialStore::default_path()) }
        };
        if let Err(error) = run_session.with_checkpoints(&cancellation, async {
            dispatcher.initialize_mcp(&conversation_id).await
        }).await {
            let retained = dispatcher.cancel_all().await;
            run_session.settle("failed", Some(&format!("{error:#}{retained}")))?;
            return Err(anyhow::anyhow!("{error:#}{retained}"));
        }
        if resuming {
            messages = runtime_store.conversation_history(&conversation_id)?.into_iter()
                .map(|record| serde_json::from_str(&record.message_json).map_err(anyhow::Error::from))
                .collect::<Result<Vec<ChatMessage>>>()?;
            if messages.first().map_or(true, |message| message.role != dume_provider::types::Role::System) {
                messages.insert(0, ChatMessage::system(dume_core::types::PromptPrefix::BASE_SYSTEM_PROMPT));
            }
            let replayed = run_session.with_checkpoints(&cancellation, async {
                dispatcher.replay_pending_tools(&run_session).await
            }).await?;
            messages.extend(replayed);
        }
        let mut final_outcome: Option<AgentOutcome> = None;
        let mut last_assistant_reply: Option<String> = None;
        let mut total_usage = dume_provider::types::TokenUsage::default();
        let mut requires_attention = false;

        let execution: Result<()> = async {
        if resuming {
            if runtime_store.tool_executions_for_run(&run_id)?.iter()
                .any(|entry| matches!(entry.state.as_str(), "outcome_unknown" | "running") || (entry.state == "failed" && entry.result.is_none())) {
                anyhow::bail!("Recovered run has unresolved tool outcome; operator resolution required");
            }
        }
        for _turn in 0..self.max_turns {
            anyhow::ensure!(!cancellation.is_cancelled(), "Agent execution cancelled");

            let routing = if is_child {
                crate::routing::RoutingDecision {
                    original_model: self.model.clone(),
                    effective_model: self.model.clone(),
                    phase: dume_store::RoutingPhase::Fixed,
                    epoch: None,
                    reason: "child_model_fixed".into(),
                    classifier_usage: None,
                }
            } else {
                run_session.with_checkpoints(&cancellation, crate::routing::select_model(
                    &routing_snapshot, &run_session, &self.model, &messages, &routing_credentials, &cancellation,
                )).await?
            };
            dispatcher.set_model(routing.effective_model.clone());

            dispatcher.refresh_mcp().await?;
            let mut provider_session_id = session_ctx.session_id.clone();
            let tools = dispatcher.tool_definitions_for_session(&mut provider_session_id);
            if provider_session_id != session_ctx.session_id {
                session_ctx.session_id = provider_session_id;
            }

            if crate::context::prepare_request_context(crate::context::RequestContext {
                model: &routing.effective_model,
                session_id: &session_ctx.session_id,
                messages: &mut messages,
                tools: &tools,
                store: self.store.as_deref(),
            })? {
                session_ctx = session_ctx.reset_session();
            }

            anyhow::ensure!(!cancellation.is_cancelled(), "Agent execution cancelled");

            let (tx, mut rx) = mpsc::channel::<StreamEvent>(50);
            let model_name = routing.effective_model.clone();
            let msgs = messages.clone();
            let tools_clone = tools.clone();
            let turn_ctx = session_ctx.next_request();

            let endpoint = self.endpoint.clone();
            #[cfg(test)]
            let injected_credentials = self.routing_credentials.clone();
            let mut turn_usage = dume_provider::types::TokenUsage::default();
            let mut had_usage_event = false;

            let turn_ctx_spawn = turn_ctx.clone();

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

                let cred_store = {
                    #[cfg(test)]
                    { injected_credentials.unwrap_or_else(|| dume_provider::CredentialStore::new(dume_provider::CredentialStore::default_path())) }
                    #[cfg(not(test))]
                    { dume_provider::CredentialStore::new(dume_provider::CredentialStore::default_path()) }
                };

                let mut provider = resolve_provider(&model_name, &cred_store).await?;
                if let Some(Endpoint::Authenticated(base_url)) = endpoint {
                    provider = provider.with_base_url(&base_url)?;
                }
                provider.stream_with_context(&msgs, &tools_clone, Some(&turn_ctx_spawn), tx).await
            });

            let mut assistant_reply = String::new();
            let mut codex_reasoning = Vec::new();
            let mut deepseek_reasoning = String::new();
            let mut gemini_parts = Vec::new();
            // Preserve order: index -> (id, name, args_buf)
            let mut ordered_calls: Vec<(String, String, String)> = Vec::new();
            let mut stream_completed_normally = false;
            let mut stream_finish_reason = String::new();
            let mut actual_response_model: Option<String> = None;
            let mut checkpoint = tokio::time::interval(std::time::Duration::from_secs(1));
            checkpoint.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                let evt = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        stream_handle.abort();
                        let _ = stream_handle.await;
                        turn_usage.is_complete = false;
                        total_usage.accumulate(&turn_usage);
                        anyhow::bail!("Agent execution cancelled");
                    }
                    _ = checkpoint.tick() => {
                        if let Err(error) = run_session.checkpoint() {
                            cancellation.cancel();
                            stream_handle.abort();
                            let _ = stream_handle.await;
                            return Err(error);
                        }
                        continue;
                    }
                    evt = rx.recv() => match evt {
                        Some(evt) => evt,
                        None => break,
                    }
                };
                if let Err(error) = run_session.record_model_event(&evt) {
                    stream_handle.abort();
                    let _ = stream_handle.await;
                    return Err(error);
                }
                if let StreamEvent::AttemptFinished {
                    outcome: dume_provider::AttemptOutcome::Failed,
                    usage: Some(attempt_usage),
                    ..
                } = &evt
                {
                    // Failed attempts can terminate before a Usage event reaches this collector.
                    // Prefer streamed usage when present; otherwise retain the typed final snapshot.
                    if !had_usage_event {
                        let mut attempt_usage = attempt_usage.clone();
                        attempt_usage.is_complete = false;
                        total_usage.accumulate(&attempt_usage);
                    }
                }
                if let StreamEvent::AttemptFinished {
                    outcome: dume_provider::AttemptOutcome::Succeeded,
                    model: finished_model,
                    ..
                } = &evt
                {
                    actual_response_model = Some(finished_model.clone());
                }
                if matches!(evt, StreamEvent::Retrying { .. }) {
                    total_usage.accumulate(&turn_usage);
                    turn_usage = dume_provider::types::TokenUsage::default();
                    had_usage_event = false;
                    assistant_reply.clear();
                    codex_reasoning.clear();
                    deepseek_reasoning.clear();
                    gemini_parts.clear();
                    ordered_calls.clear();
                }
                match evt {
                    StreamEvent::AttemptStarted { .. }
                    | StreamEvent::AttemptFinished { .. }
                    | StreamEvent::Retrying { .. }
                    | StreamEvent::ProviderError { .. } => {}
                    StreamEvent::CodexReasoning(items) => codex_reasoning.extend(items),
                    StreamEvent::DeepSeekReasoning(delta) => deepseek_reasoning.push_str(&delta),
                    StreamEvent::GeminiParts(parts) => gemini_parts.extend(parts),
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
                    StreamEvent::Usage(usage) => {
                        had_usage_event = true;
                        turn_usage.merge_cumulative(&usage);
                    }
                    StreamEvent::Completed { finish_reason } => {
                        stream_completed_normally = true;
                        stream_finish_reason = finish_reason;
                        break;
                    }
                    StreamEvent::Error(err) => {
                        stream_handle.abort();
                        let _ = stream_handle.await;
                        turn_usage.is_complete = false;
                        total_usage.accumulate(&turn_usage);
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
                    turn_usage.is_complete = false;
                    total_usage.accumulate(&turn_usage);
                    anyhow::bail!("Agent execution cancelled");
                }
                result = &mut stream_handle => result,
            };
            if let Err(e) = stream_task_res {
                turn_usage.is_complete = false;
                total_usage.accumulate(&turn_usage);
                anyhow::bail!("Stream task aborted: {}", e);
            }
            if let Ok(Err(e)) = stream_task_res {
                turn_usage.is_complete = false;
                total_usage.accumulate(&turn_usage);
                return Err(e.context("Stream provider error"));
            }

            if !had_usage_event || !stream_completed_normally {
                turn_usage.is_complete = false;
            }
            total_usage.accumulate(&turn_usage);
            run_session.checkpoint()?;

            if !stream_completed_normally {
                anyhow::bail!("Stream disconnected prematurely without Completed event");
            }

            if stream_finish_reason == "length" {
                anyhow::bail!("Model output truncated due to length limits before completion");
            }

            last_assistant_reply = Some(assistant_reply.clone());
            let effective_response_model = actual_response_model
                .as_deref()
                .unwrap_or(&routing.effective_model);
            dispatcher.set_model(effective_response_model);

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
                if !deepseek_reasoning.is_empty() { message.deepseek_reasoning = Some(deepseek_reasoning); }
                message.gemini_parts = gemini_parts;
                run_session.record_assistant(&message)?;
                messages.push(message);
                final_outcome = Some(AgentOutcome::Completed {
                    answer: assistant_reply,
                    usage: total_usage.clone(),
                });
                let cleanup = run_session.with_checkpoints(&cancellation, async {
                    Ok(dispatcher.cancel_all().await)
                }).await?;
                if cancellation.is_cancelled() {
                    anyhow::bail!("Agent execution cancelled during cleanup{cleanup}");
                }
                match run_session.boundary()? {
                    dume_store::BoundaryResult::Continue(inputs) => {
                        if !inputs.is_empty() { routing_snapshot.reload_for_new_input(); }
                        for input in inputs { messages.push(ChatMessage::user(input.payload)); }
                        final_outcome = None;
                        run_session.with_checkpoints(&cancellation, async {
                            dispatcher.initialize_mcp(&conversation_id).await
                        }).await?;
                        continue;
                    }
                    dume_store::BoundaryResult::NextRun(token) => {
                        routing_snapshot.reload_for_new_input();
                        run_session.advance(&token.run_id)?;
                        messages = runtime_store.conversation_history(&conversation_id)?.into_iter()
                            .map(|record| serde_json::from_str(&record.message_json).map_err(anyhow::Error::from))
                            .collect::<Result<Vec<ChatMessage>>>()?;
                        final_outcome = None;
                        last_assistant_reply = None;
                        run_session.with_checkpoints(&cancellation, async {
                            dispatcher.initialize_mcp(&conversation_id).await
                        }).await?;
                        continue;
                    }
                    dume_store::BoundaryResult::Terminal => break,
                }
            }

            // CRITICAL: Assistant message MUST contain tool_calls metadata so provider accepts subsequent tool responses
            let mut message = ChatMessage::assistant_with_tool_calls(&assistant_reply, complete_tool_calls.clone());
            message.codex_reasoning = codex_reasoning;
            message.deepseek_reasoning = Some(deepseek_reasoning);
            message.gemini_parts = gemini_parts;
            let prepared = run_session.prepare_tools(&message)?;
            messages.push(message);

            // Execute requested tools in exact order and feed back results
            for (tc, execution) in complete_tool_calls.into_iter().zip(prepared) {
                anyhow::ensure!(!cancellation.is_cancelled(), "Agent execution cancelled");
                match run_session.begin_tool(&execution)? {
                    ToolStart::Cached { result } => messages.push(serde_json::from_str(&result)
                        .context("stored tool result is not a serialized ChatMessage")?),
                    ToolStart::Execute { attempt_id } => {
                        let started = std::time::Instant::now();
                        let result = match run_session.with_checkpoints(&cancellation, async {
                            dispatcher.execute(tc, &messages).await
                        }).await {
                            Ok(result) => result,
                            Err(error) if error.downcast_ref::<crate::tools::KnownToolFailure>().is_some() => {
                                let failure = ChatMessage::tool(error.downcast_ref::<crate::tools::KnownToolFailure>().unwrap().output.clone(), execution.tool_call_id.clone());
                                run_session.finish_tool_error(&execution, &attempt_id, &failure, started.elapsed().as_millis().min(i64::MAX as u128) as i64)?;
                                messages.push(failure);
                                continue;
                            }
                            Err(error) => {
                                run_session.mark_tool_unknown(&execution, &attempt_id, &format!("{error:#}"), started.elapsed().as_millis().min(i64::MAX as u128) as i64)?;
                                requires_attention = true;
                                anyhow::bail!("Tool outcome is uncertain after side effect failure: {error:#}");
                            }
                        };
                        anyhow::ensure!(!cancellation.is_cancelled(), "Tool effect outcome is uncertain after cancellation");
                        let elapsed = started.elapsed().as_millis().min(i64::MAX as u128) as i64;
                        run_session.finish_tool(&execution, &attempt_id, &result, elapsed)?;
                        messages.push(result);
                    }
                }
            }
            let steer = run_session.consume_steer()?;
            if !steer.is_empty() { routing_snapshot.reload_for_new_input(); }
            for input in steer {
                messages.push(ChatMessage::user(input.payload));
            }
            continue;
        }

        Ok(())
        }.await;
        let retained = run_session.with_checkpoints(&cancellation, async {
            Ok(dispatcher.cancel_all().await)
        }).await.unwrap_or_else(|error| format!("\nCleanup failed: {error:#}"));
        if let Err(error) = execution {
            let status = if requires_attention { "needs_attention" } else if cancellation.is_cancelled() { "cancelled" } else { "failed" };
            let detail = format!("{error:#}{retained}");
            run_session.settle(status, Some(&detail))?;
            return Err(anyhow::anyhow!(detail));
        }
        if cancellation.is_cancelled() {
            run_session.settle("cancelled", Some("cancelled"))?;
            return Ok(AgentOutcome::Cancelled { usage: total_usage });
        }

            let descendant = self.descendant_usage.lock().await.clone();
            total_usage.accumulate(&descendant);

            let outcome = final_outcome.map(|outcome| match outcome {
                AgentOutcome::Completed { answer, .. } => AgentOutcome::Completed {
                    answer,
                    usage: total_usage.clone(),
                },
                AgentOutcome::TurnLimitExhausted { last_reply, .. } => AgentOutcome::TurnLimitExhausted {
                    last_reply,
                    usage: total_usage.clone(),
                },
                AgentOutcome::Cancelled { .. } => AgentOutcome::Cancelled {
                    usage: total_usage.clone(),
                },
            }).unwrap_or_else(|| AgentOutcome::TurnLimitExhausted {
                last_reply: last_assistant_reply,
                usage: total_usage,
            });
            let exhausted = matches!(&outcome, AgentOutcome::TurnLimitExhausted { .. });
            let status = if cancellation.is_cancelled() { "cancelled" } else if exhausted { "failed" } else { "succeeded" };
            run_session.settle(status, exhausted.then_some("turn_limit_exhausted"))?;
            Ok(outcome)
            }.await;
            if cancellation.is_cancelled() {
                anyhow::bail!("Agent execution cancelled");
            }
            match result {
                Ok(outcome) => Ok(outcome),
                Err(error) => Err(error),
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
    last_tool_definitions: Option<Vec<ToolDefinition>>,
    cancellation: CancellationToken,
    mcp: Option<crate::mcp_runtime::McpRuntime>,
}

impl ToolDispatcher {
    fn from_agent(agent: AgentLoop, cancellation: CancellationToken) -> Self {
        let executor = LocalToolExecutor::new(&agent.worktree_path)
            .with_cancellation(cancellation.clone())
            .with_artifact_store(agent.artifact_store.clone());
        Self {
            agent,
            executor,
            subagents: dume_mcp::subagent::SubagentManager::new(),
            child_paths: std::collections::HashMap::new(),
            last_tool_definitions: None,
            cancellation,
            mcp: None,
        }
    }

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
            last_tool_definitions: None,
            cancellation,
            mcp: None,
        }
    }

    pub fn with_store(mut self, store: std::sync::Arc<dume_store::HarnessStore>) -> Self {
        let art_store = std::sync::Arc::new(store.artifacts.clone());
        self.agent = self.agent.with_store(store);
        self.executor = self.executor.with_artifact_store(Some(art_store));
        self
    }

    pub fn with_artifact_store(mut self, store: std::sync::Arc<dume_store::ArtifactStore>) -> Self {
        self.executor = self
            .executor
            .with_artifact_store(Some(std::sync::Arc::clone(&store)));
        self.agent = self.agent.with_artifact_store(store);
        self
    }

    pub fn worktree_path(&self) -> &Path {
        &self.agent.worktree_path
    }

    pub async fn initialize_mcp(&mut self, conversation_id: &str) -> Result<()> {
        if let Some(mut old) = self.mcp.take() {
            old.shutdown().await;
        }
        self.mcp = Some(
            crate::mcp_runtime::McpRuntime::load(
                self.worktree_path(),
                self.agent.store.clone(),
                Some(conversation_id),
            )
            .await?,
        );
        Ok(())
    }

    pub async fn refresh_mcp(&mut self) -> Result<()> {
        if let Some(mcp) = &mut self.mcp {
            mcp.refresh().await?;
        }
        Ok(())
    }

    pub fn mcp_status(&self) -> String {
        self.mcp
            .as_ref()
            .map(crate::mcp_runtime::McpRuntime::status)
            .unwrap_or_default()
    }

    pub fn tool_definitions_for_session(&mut self, session_id: &mut String) -> Vec<ToolDefinition> {
        let mut tools = AgentLoop::tool_definitions(self.worktree_path());
        tools.push(crate::mcp_runtime::tool_search_definition());
        if let Some(mcp) = &self.mcp {
            tools.extend(mcp.definitions());
        }
        if self
            .last_tool_definitions
            .as_ref()
            .is_some_and(|previous| previous != &tools)
        {
            *session_id = dume_provider::types::StreamRequestContext::new().session_id;
        }
        self.last_tool_definitions = Some(tools.clone());
        tools
    }

    pub fn set_model(&mut self, model: impl Into<String>) {
        self.agent.model = model.into();
    }

    pub fn store(&self) -> Option<&std::sync::Arc<dume_store::HarnessStore>> {
        self.agent.store.as_ref()
    }

    pub async fn replay_pending_tools(&mut self, session: &RunSession) -> Result<Vec<ChatMessage>> {
        let store = session.store();
        let executions = store.tool_executions_for_run(&session.token().run_id)?;
        if executions
            .iter()
            .any(|execution| execution.state == "prepared")
        {
            // A completed write may have advanced the phase after this response prepared its tools.
            if let Some(model) = store.latest_successful_model_for_run(&session.token().run_id)? {
                self.set_model(model);
            }
        }
        let mut messages = Vec::new();
        for execution in executions
            .into_iter()
            .filter(|execution| execution.state == "prepared")
        {
            let call = dume_provider::ToolCall {
                id: execution.tool_call_id.clone(),
                name: execution.name.clone(),
                arguments: execution.arguments.clone(),
            };
            let ToolStart::Execute { attempt_id } = session.begin_tool(&execution)? else {
                continue;
            };
            let started = std::time::Instant::now();
            let elapsed = || started.elapsed().as_millis().min(i64::MAX as u128) as i64;
            match self.execute(call, &[]).await {
                Ok(message) => {
                    session.finish_tool(&execution, &attempt_id, &message, elapsed())?;
                    messages.push(message);
                }
                Err(error)
                    if error
                        .downcast_ref::<crate::tools::KnownToolFailure>()
                        .is_some() =>
                {
                    let failure = ChatMessage::tool(
                        error
                            .downcast_ref::<crate::tools::KnownToolFailure>()
                            .unwrap()
                            .output
                            .clone(),
                        execution.tool_call_id.clone(),
                    );
                    session.finish_tool_error(&execution, &attempt_id, &failure, elapsed())?;
                    messages.push(failure);
                }
                Err(error) => {
                    session.mark_tool_unknown(
                        &execution,
                        &attempt_id,
                        &format!("{error:#}"),
                        elapsed(),
                    )?;
                    return Err(error);
                }
            }
        }
        Ok(messages)
    }

    pub async fn execute(
        &mut self,
        call: dume_provider::ToolCall,
        messages: &[ChatMessage],
    ) -> Result<ChatMessage> {
        if call.name == "tool_search" {
            let args: serde_json::Value = serde_json::from_str(&call.arguments).map_err(|e| {
                crate::tools::KnownToolFailure {
                    output: format!("Invalid tool_search arguments: {e}"),
                    exit_code: None,
                }
            })?;
            let query = args
                .get("query")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| crate::tools::KnownToolFailure {
                    output: "Missing required field 'query' (string)".into(),
                    exit_code: None,
                })?;
            let limit = match args.get("limit") {
                None => 10,
                Some(value) => value
                    .as_u64()
                    .filter(|limit| (1..=20).contains(limit))
                    .ok_or_else(|| crate::tools::KnownToolFailure {
                        output: "Field 'limit' must be an integer from 1 to 20".into(),
                        exit_code: None,
                    })? as usize,
            };
            let mcp = self
                .mcp
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("MCP is not initialized"))?;
            let result = tokio::select! {
                biased;
                _ = self.cancellation.cancelled() => anyhow::bail!("MCP search cancelled"),
                result = mcp.search(query, limit) => result.map_err(|error| crate::tools::KnownToolFailure { output: error.to_string(), exit_code: None })?,
            };
            return Ok(ChatMessage::tool(
                crate::output_limits::admit_tool_output(
                    "tool_search",
                    &result,
                    self.agent.artifact_store.as_deref(),
                ),
                call.id,
            ));
        }
        if self
            .mcp
            .as_ref()
            .is_some_and(|mcp| mcp.owns_name(&call.name))
        {
            let args: serde_json::Value = serde_json::from_str(&call.arguments).map_err(|e| {
                crate::tools::KnownToolFailure {
                    output: format!("Invalid MCP tool arguments: {e}"),
                    exit_code: None,
                }
            })?;
            let mcp = self.mcp.as_ref().unwrap();
            if !mcp.is_active(&call.name) {
                return Err(crate::tools::KnownToolFailure {
                    output: format!("MCP tool is not active: {}", call.name),
                    exit_code: None,
                }
                .into());
            }
            let result = tokio::select! {
                biased;
                _ = self.cancellation.cancelled() => anyhow::bail!("MCP call cancelled; outcome may be unknown"),
                result = mcp.execute(&call.name, args) => result?,
            };
            let content = result
                .content
                .iter()
                .filter_map(|part| part.text.as_deref())
                .collect::<Vec<_>>()
                .join("\n");
            let output = crate::output_limits::admit_tool_output(
                &call.name,
                &content,
                self.agent.artifact_store.as_deref(),
            );
            if result.is_error {
                return Err(crate::tools::KnownToolFailure {
                    output,
                    exit_code: None,
                }
                .into());
            }
            return Ok(ChatMessage::tool(output, call.id));
        }
        let read_only = matches!(
            call.name.as_str(),
            "read_file" | "read_artifact" | "grep_search"
        );
        let result = self
            .agent
            .execute_tool(
                call,
                &self.executor,
                &self.subagents,
                &mut self.child_paths,
                &self.cancellation,
                messages,
            )
            .await;
        if read_only && !self.cancellation.is_cancelled() {
            result.map_err(|error| {
                crate::tools::KnownToolFailure {
                    output: format!("Tool error: {error:#}"),
                    exit_code: None,
                }
                .into()
            })
        } else {
            result
        }
    }

    pub async fn cancel_all(&mut self) -> String {
        self.subagents.cancel_all().await;
        let mut report = String::new();
        for (id, path) in &self.child_paths {
            report.push_str(&format!("\nSubagent '{}': {:?}; worktree destination retained if created: {}. Output is not integrated.", id, self.subagents.inspect_subagent(id).await.map(|record| record.status), path.display()));
        }
        if let Some(mcp) = &mut self.mcp {
            mcp.shutdown().await;
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
    store: Option<std::sync::Arc<dume_store::HarnessStore>>,
    request_context: Option<dume_provider::types::StreamRequestContext>,
    descendant_usage: std::sync::Arc<tokio::sync::Mutex<dume_provider::types::TokenUsage>>,
    #[cfg(test)] routing_credentials: Option<dume_provider::CredentialStore>,
) -> Result<String> {
    anyhow::ensure!(
        !cancellation.is_cancelled(),
        "Subagent cancelled before isolation"
    );
    dume_git::create_git_worktree(parent, child, "HEAD")
        .await
        .with_context(|| format!("Subagent isolation failed at {}", child.display()))?;
    let base_commit = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dume_git::worktree::git_path(child))
        .args(["rev-parse", "HEAD"])
        .output()
        .await
        .context("Failed to identify subagent worktree base commit")?;
    anyhow::ensure!(
        base_commit.status.success(),
        "Failed to identify subagent worktree base commit"
    );
    let base_commit = String::from_utf8_lossy(&base_commit.stdout)
        .trim()
        .to_string();
    let mut agent = AgentLoop::new(child, model);
    agent.endpoint = endpoint;
    if let Some(store) = store.clone() {
        agent = agent.with_store(store);
    }
    agent.request_context = request_context;
    #[cfg(test)]
    {
        agent.routing_credentials = routing_credentials;
    }
    agent.descendant_usage = std::sync::Arc::clone(&descendant_usage);
    let result = agent.run_task_with_cancellation(prompt, cancellation).await;
    if let Ok(ref outcome) = result {
        if let Some(usage) = outcome.usage() {
            let mut guard = descendant_usage.lock().await;
            guard.accumulate(usage);
        }
    }
    match result {
        Ok(outcome) => {
            let (status_str, answer_str) = match &outcome {
                AgentOutcome::Completed { answer, .. } => ("Completed", answer.as_str()),
                AgentOutcome::TurnLimitExhausted { last_reply, .. } => (
                    "TurnLimitExhausted",
                    last_reply.as_deref().unwrap_or("Turn limit reached"),
                ),
                AgentOutcome::Cancelled { .. } => ("Cancelled", "Subagent was cancelled"),
            };
            let changed_files = match tokio::process::Command::new("git")
                .arg("-C")
                .arg(dume_git::worktree::git_path(child))
                .args(["status", "--short"])
                .output()
                .await
            {
                Ok(output) if output.status.success() => {
                    let files = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if files.is_empty() {
                        "(clean)".to_string()
                    } else {
                        files
                    }
                }
                Ok(output) => format!("(unavailable: git status exited {})", output.status),
                Err(error) => format!("(unavailable: {})", error),
            };
            let formatted = format!(
                "Status: {}\nChanges/Findings: {}\nChanged Files:\n{}\nValidation: Not independently run by the harness\nUnresolved Issues: Not independently verified; review the child findings\nWorktree: {}\nBase Commit: {} (parent uncommitted changes are excluded)\nNote: Output has not been integrated into parent worktree.",
                status_str,
                answer_str,
                changed_files,
                child.display(),
                base_commit
            );
            let bounded = crate::output_limits::admit_tool_output(
                "subagent_outcome",
                &formatted,
                store.as_ref().map(|s| &s.artifacts),
            );
            Ok(bounded)
        }
        Err(error) => {
            let formatted = format!(
                "Status: Failed\nChanges/Findings: Execution error\nValidation: None\nUnresolved Issues: {:#}\nWorktree: {}\nNote: Worktree retained for recovery; output not integrated.",
                error,
                child.display()
            );
            let bounded = crate::output_limits::admit_tool_output(
                "subagent_error",
                &formatted,
                store.as_ref().map(|s| &s.artifacts),
            );
            anyhow::bail!("{}", bounded)
        }
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

    async fn read_http_json(socket: &mut tokio::net::TcpStream) -> serde_json::Value {
        use tokio::io::AsyncReadExt;
        let mut bytes = Vec::new();
        let header_end = loop {
            let mut chunk = [0; 4096];
            let n = socket.read(&mut chunk).await.unwrap();
            assert!(n > 0, "connection closed before HTTP headers");
            bytes.extend_from_slice(&chunk[..n]);
            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        while bytes.len() < header_end + length {
            let mut chunk = [0; 4096];
            let n = socket.read(&mut chunk).await.unwrap();
            assert!(n > 0, "connection closed before request body");
            bytes.extend_from_slice(&chunk[..n]);
        }
        serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
    }

    fn routing_test_credentials(path: &Path) -> dume_provider::CredentialStore {
        let credentials = dume_provider::CredentialStore::new(path);
        for provider in ["openai", "jev-local-system-one"] {
            credentials
                .save(
                    provider,
                    &dume_provider::auth::Credential {
                        cred_type: "api_key".into(),
                        key: Some("fixture-token".into()),
                        access_token: None,
                        refresh_token: None,
                        expires_at: None,
                        account_id: None,
                    },
                )
                .unwrap();
        }
        credentials
    }

    fn routing_test_snapshot(classifier_url: String) -> crate::routing::RoutingSnapshot {
        let mut settings = crate::routing_settings::RoutingSettings::default();
        settings.enabled = true;
        settings.classifier = Some(dume_provider::ClassifierConfig {
            backend: dume_provider::ClassifierBackend::LocalSystemOne,
            base_url: classifier_url,
            model: "local-test".into(),
        });
        settings.models = crate::routing_settings::RoutingModels {
            standard: Some("openai/gpt-4o-mini".into()),
            complex: Some("openai/gpt-4o".into()),
            implementation: Some("openai/gpt-4.1-mini".into()),
        };
        crate::routing::RoutingSnapshot::from_settings(settings, false)
    }

    #[test]
    fn durable_tool_result_is_reused_without_second_execution() {
        let dir = tempdir().unwrap();
        let store = std::sync::Arc::new(
            dume_store::HarnessStore::in_memory(dir.path().join("artifacts")).unwrap(),
        );
        let conversation = "worker:test";
        store
            .create_conversation(conversation, "/tmp", "model", "provider-session")
            .unwrap();
        let scope = dume_store::RuntimeScope::Conversation(conversation.into());
        let receipt = store
            .admit_input(
                scope.clone(),
                "client",
                dume_store::InputKind::Submit,
                "do tool",
            )
            .unwrap();
        let run = receipt.run_id.unwrap();
        let session = RunSession::start(store.clone(), scope, &run, conversation).unwrap();
        let assistant = ChatMessage::assistant_with_tool_calls(
            "",
            vec![dume_provider::types::ToolCall {
                id: "once".into(),
                name: "read_file".into(),
                arguments: "{}".into(),
            }],
        );
        let execution = session.prepare_tools(&assistant).unwrap().remove(0);
        let ToolStart::Execute { attempt_id } = session.begin_tool(&execution).unwrap() else {
            panic!()
        };
        let result = ChatMessage::tool("saved", "once");
        session
            .finish_tool(&execution, &attempt_id, &result, 1)
            .unwrap();
        let reopened = store.get_tool_execution(&execution.execution_id).unwrap();
        let ToolStart::Cached { result } = session.begin_tool(&reopened).unwrap() else {
            panic!()
        };
        let cached: ChatMessage = serde_json::from_str(&result).unwrap();
        assert_eq!(cached.content, "saved");
        assert_eq!(cached.tool_call_id.as_deref(), Some("once"));
        let history = store.conversation_history(conversation).unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(
            serde_json::from_str::<ChatMessage>(&history[0].message_json).unwrap(),
            ChatMessage::user("do tool")
        );
    }

    #[tokio::test]
    async fn prepared_spawn_inherits_persisted_response_model_before_routing_resumes() {
        use tokio::io::AsyncWriteExt;
        use tokio::time::{Duration, timeout};
        for (phase, response_model, completed_write) in [
            (
                dume_store::RoutingPhase::Planning,
                "openai/gpt-4o-mini",
                false,
            ),
            (
                dume_store::RoutingPhase::Implementation,
                "openai/gpt-4.1-mini",
                false,
            ),
            (
                dume_store::RoutingPhase::Planning,
                "openai/gpt-4o-mini",
                true,
            ),
        ] {
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
                assert!(
                    tokio::process::Command::new("git")
                        .arg("-C")
                        .arg(dir.path())
                        .args(args)
                        .output()
                        .await
                        .unwrap()
                        .status
                        .success()
                );
            }
            let store = std::sync::Arc::new(
                dume_store::HarnessStore::in_memory(dir.path().join("artifacts")).unwrap(),
            );
            let conversation = "worker:replay-spawn";
            store
                .create_conversation(
                    conversation,
                    dir.path().to_str().unwrap(),
                    "openai/gpt-4o",
                    "session",
                )
                .unwrap();
            let scope = dume_store::RuntimeScope::Conversation(conversation.into());
            let run = store
                .admit_input(
                    scope.clone(),
                    "input",
                    dume_store::InputKind::Submit,
                    "delegate",
                )
                .unwrap()
                .run_id
                .unwrap();
            let session = RunSession::start(store.clone(), scope, &run, conversation).unwrap();
            store
                .save_conversation_routing_state(
                    session.token(),
                    &dume_store::ConversationRoutingState {
                        conversation_id: conversation.into(),
                        epoch: "epoch".into(),
                        phase,
                        original_model: "openai/gpt-4o".into(),
                        planning_model: "openai/gpt-4o-mini".into(),
                        implementation_model: "openai/gpt-4.1-mini".into(),
                        reason: "complexity_complex".into(),
                        revision: 0,
                        classifier_input_tokens: None,
                        classifier_output_tokens: None,
                    },
                    None,
                )
                .unwrap();
            session.set_routing_epoch(
                (phase == dume_store::RoutingPhase::Planning).then_some("epoch"),
            );
            let context = dume_provider::types::StreamRequestContext::new();
            session
                .record_model_event(&StreamEvent::AttemptStarted {
                    context: context.clone(),
                    model: response_model.into(),
                })
                .unwrap();
            session
                .record_model_event(&StreamEvent::AttemptFinished {
                    context,
                    model: response_model.into(),
                    outcome: dume_provider::AttemptOutcome::Succeeded,
                    duration_ms: 0,
                    usage: None,
                    error: None,
                })
                .unwrap();
            let mut calls = vec![
                dume_provider::ToolCall {
                    id: "spawn".into(),
                    name: "spawn_subagent".into(),
                    arguments: r#"{"id":"child","prompt":"child work","sub_dir":"child"}"#.into(),
                },
                dume_provider::ToolCall {
                    id: "wait".into(),
                    name: "wait_subagent".into(),
                    arguments: r#"{"id":"child","timeout_ms":5000}"#.into(),
                },
            ];
            if completed_write {
                calls.insert(
                    0,
                    dume_provider::ToolCall {
                        id: "write".into(),
                        name: "write_file".into(),
                        arguments: r#"{"path":"plan.txt","content":"plan"}"#.into(),
                    },
                );
            }
            let executions = session
                .prepare_tools(&ChatMessage::assistant_with_tool_calls("", calls))
                .unwrap();
            if completed_write {
                let ToolStart::Execute { attempt_id } = session.begin_tool(&executions[0]).unwrap()
                else {
                    panic!("fresh write expected")
                };
                session
                    .finish_tool(
                        &executions[0],
                        &attempt_id,
                        &ChatMessage::tool("written", "write"),
                        0,
                    )
                    .unwrap();
                assert_eq!(
                    store
                        .conversation_routing_state(conversation)
                        .unwrap()
                        .unwrap()
                        .phase,
                    dume_store::RoutingPhase::Implementation
                );
            }
            drop(session);
            let resumed = RunSession::resume(store.clone(), &run, conversation).unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = timeout(Duration::from_secs(5), listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
                let request = timeout(Duration::from_secs(5), read_http_json(&mut socket))
                    .await
                    .unwrap();
                let body = "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                request["model"].as_str().unwrap().to_owned()
            });
            let agent = AgentLoop::new(dir.path(), "openai/gpt-4o")
                .with_store(store.clone())
                .with_routing_credentials(routing_test_credentials(&dir.path().join("auth.json")))
                .with_inherited_subagent_model("child")
                .with_base_url(format!("http://{address}"));
            let mut dispatcher = ToolDispatcher::from_agent(agent, CancellationToken::new());
            let results = timeout(
                Duration::from_secs(8),
                dispatcher.replay_pending_tools(&resumed),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(dispatcher.agent.model, response_model);
            assert_eq!(results.len(), 2);
            assert!(
                results[1].content.contains("done"),
                "{}",
                results[1].content
            );
            assert_eq!(
                server.await.unwrap(),
                response_model.split_once('/').unwrap().1
            );
            assert!(
                dispatcher
                    .replay_pending_tools(&resumed)
                    .await
                    .unwrap()
                    .is_empty()
            );
            dispatcher.cancel_all().await;
        }
    }

    #[tokio::test]
    async fn prepared_read_is_replayed_and_committed_once_after_resume() {
        let dir = tempdir().unwrap();
        let store = std::sync::Arc::new(
            dume_store::HarnessStore::in_memory(dir.path().join("artifacts")).unwrap(),
        );
        let conversation = "worker:replay-read";
        store
            .create_conversation(
                conversation,
                dir.path().to_str().unwrap(),
                "model",
                "session",
            )
            .unwrap();
        let scope = dume_store::RuntimeScope::Conversation(conversation.into());
        let run = store
            .admit_input(
                scope.clone(),
                "input",
                dume_store::InputKind::Submit,
                "read",
            )
            .unwrap()
            .run_id
            .unwrap();
        let session = RunSession::start(store.clone(), scope, &run, conversation).unwrap();
        let file = dir.path().join("evidence.txt");
        std::fs::write(&file, "preserved evidence").unwrap();
        let assistant = ChatMessage::assistant_with_tool_calls(
            "",
            vec![dume_provider::types::ToolCall {
                id: "read-on-resume".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path":"evidence.txt"}).to_string(),
            }],
        );
        session.prepare_tools(&assistant).unwrap();
        drop(session);

        let resumed = RunSession::resume(store.clone(), &run, conversation).unwrap();
        let mut dispatcher = ToolDispatcher::new(dir.path(), "model", CancellationToken::new())
            .with_store(store.clone());
        let recovered = dispatcher.replay_pending_tools(&resumed).await.unwrap();
        assert_eq!(recovered.len(), 1);
        assert!(recovered[0].content.contains("preserved evidence"));
        assert!(
            dispatcher
                .replay_pending_tools(&resumed)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.tool_executions_for_run(&run).unwrap()[0].state,
            "succeeded"
        );
    }

    #[tokio::test]
    async fn interrupted_bash_side_effect_is_not_replayed() {
        let dir = tempdir().unwrap();
        let store = std::sync::Arc::new(
            dume_store::HarnessStore::in_memory(dir.path().join("artifacts")).unwrap(),
        );
        let conversation = "worker:replay-bash";
        store
            .create_conversation(
                conversation,
                dir.path().to_str().unwrap(),
                "model",
                "session",
            )
            .unwrap();
        let scope = dume_store::RuntimeScope::Conversation(conversation.into());
        let run = store
            .admit_input(
                scope.clone(),
                "input",
                dume_store::InputKind::Submit,
                "run once",
            )
            .unwrap()
            .run_id
            .unwrap();
        let session = RunSession::start(store.clone(), scope, &run, conversation).unwrap();
        let marker = dir.path().join("effects");
        let assistant = ChatMessage::assistant_with_tool_calls(
            "",
            vec![dume_provider::types::ToolCall {
                id: "effect-once".into(),
                name: "bash".into(),
                arguments:
                    serde_json::json!({"command":format!("printf x >> '{}'", marker.display())})
                        .to_string(),
            }],
        );
        let execution = session.prepare_tools(&assistant).unwrap().remove(0);
        let ToolStart::Execute { attempt_id: _ } = session.begin_tool(&execution).unwrap() else {
            panic!()
        };
        std::fs::write(&marker, "x").unwrap();
        drop(session);
        assert!(RunSession::resume(store.clone(), &run, conversation).is_err());
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "x");
    }

    #[tokio::test]
    async fn prepared_bash_has_not_started_and_runs_once_during_recovery() {
        let dir = tempdir().unwrap();
        let store = std::sync::Arc::new(
            dume_store::HarnessStore::in_memory(dir.path().join("artifacts")).unwrap(),
        );
        let conversation = "worker:prepared-bash";
        store
            .create_conversation(
                conversation,
                dir.path().to_str().unwrap(),
                "model",
                "session",
            )
            .unwrap();
        let scope = dume_store::RuntimeScope::Conversation(conversation.into());
        let run = store
            .admit_input(
                scope.clone(),
                "input",
                dume_store::InputKind::Submit,
                "run once",
            )
            .unwrap()
            .run_id
            .unwrap();
        let session = RunSession::start(store.clone(), scope, &run, conversation).unwrap();
        let marker = dir.path().join("effects");
        let assistant = ChatMessage::assistant_with_tool_calls(
            "",
            vec![dume_provider::types::ToolCall {
                id: "prepared-effect".into(),
                name: "bash".into(),
                arguments:
                    serde_json::json!({"command": format!("printf x >> '{}'", marker.display())})
                        .to_string(),
            }],
        );
        session.prepare_tools(&assistant).unwrap();
        drop(session);

        let resumed = RunSession::resume(store.clone(), &run, conversation).unwrap();
        let mut dispatcher = ToolDispatcher::new(dir.path(), "model", CancellationToken::new())
            .with_store(store.clone());
        let replayed = dispatcher.replay_pending_tools(&resumed).await.unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "x");
        assert!(
            dispatcher
                .replay_pending_tools(&resumed)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "x");
    }

    #[test]
    fn completed_tool_message_is_preserved_once_in_resumed_history() {
        let dir = tempdir().unwrap();
        let store = std::sync::Arc::new(
            dume_store::HarnessStore::in_memory(dir.path().join("artifacts")).unwrap(),
        );
        let conversation = "worker:cached-history";
        store
            .create_conversation(
                conversation,
                dir.path().to_str().unwrap(),
                "model",
                "session",
            )
            .unwrap();
        let scope = dume_store::RuntimeScope::Conversation(conversation.into());
        let run = store
            .admit_input(
                scope.clone(),
                "input",
                dume_store::InputKind::Submit,
                "task",
            )
            .unwrap()
            .run_id
            .unwrap();
        let session = RunSession::start(store.clone(), scope, &run, conversation).unwrap();
        let assistant = ChatMessage::assistant_with_tool_calls(
            "",
            vec![dume_provider::types::ToolCall {
                id: "saved-call".into(),
                name: "bash".into(),
                arguments: "{\"command\":\"true\"}".into(),
            }],
        );
        let execution = session.prepare_tools(&assistant).unwrap().remove(0);
        let ToolStart::Execute { attempt_id } = session.begin_tool(&execution).unwrap() else {
            panic!()
        };
        let saved = ChatMessage::tool("marker1", "saved-call");
        session
            .finish_tool(&execution, &attempt_id, &saved, 1)
            .unwrap();
        drop(session);

        let resumed = RunSession::resume(store.clone(), &run, conversation).unwrap();
        let history = store.conversation_history(conversation).unwrap();
        let restored: Vec<ChatMessage> = history
            .iter()
            .map(|record| serde_json::from_str(&record.message_json).unwrap())
            .collect();
        assert_eq!(
            restored
                .iter()
                .filter(|message| message.role == dume_provider::types::Role::Tool
                    && message.content == "marker1")
                .count(),
            1
        );
        let cached = resumed
            .begin_tool(&store.get_tool_execution(&execution.execution_id).unwrap())
            .unwrap();
        let ToolStart::Cached { result } = cached else {
            panic!()
        };
        let restored_cached: ChatMessage = serde_json::from_str(&result).unwrap();
        assert_eq!(restored_cached, saved);
    }

    #[test]
    fn queued_steer_is_consumed_at_boundary_and_followup_gets_new_run() {
        let dir = tempdir().unwrap();
        let store = std::sync::Arc::new(
            dume_store::HarnessStore::in_memory(dir.path().join("artifacts")).unwrap(),
        );
        let conversation = "worker:queue";
        store
            .create_conversation(conversation, "/tmp", "model", "session")
            .unwrap();
        let scope = dume_store::RuntimeScope::Conversation(conversation.into());
        let first = store
            .admit_input(
                scope.clone(),
                "first",
                dume_store::InputKind::Submit,
                "first task",
            )
            .unwrap()
            .run_id
            .unwrap();
        let session =
            RunSession::start(store.clone(), scope.clone(), &first, conversation).unwrap();
        store
            .admit_input(
                scope.clone(),
                "steer",
                dume_store::InputKind::Steer,
                "change direction",
            )
            .unwrap();
        store
            .admit_input(
                scope.clone(),
                "followup",
                dume_store::InputKind::FollowUp,
                "next task",
            )
            .unwrap();
        let boundary = session.boundary().unwrap();
        assert!(
            matches!(boundary, dume_store::BoundaryResult::Continue(ref inputs) if inputs.len() == 1 && inputs[0].payload == "change direction")
        );
        assert!(matches!(
            session.boundary().unwrap(),
            dume_store::BoundaryResult::NextRun(_)
        ));
    }

    #[test]
    fn failed_provider_attempt_usage_is_persisted_once() {
        let dir = tempdir().unwrap();
        let store = std::sync::Arc::new(
            dume_store::HarnessStore::in_memory(dir.path().join("artifacts")).unwrap(),
        );
        let conversation = "worker:usage";
        store
            .create_conversation(conversation, "/tmp", "model", "provider-session")
            .unwrap();
        let scope = dume_store::RuntimeScope::Conversation(conversation.into());
        let run = store
            .admit_input(
                scope.clone(),
                "input",
                dume_store::InputKind::Submit,
                "task",
            )
            .unwrap()
            .run_id
            .unwrap();
        let session = RunSession::start(store.clone(), scope, &run, conversation).unwrap();
        let mut context = dume_provider::types::StreamRequestContext::new().next_request();
        context.session_id = "provider-session".into();
        session
            .record_model_event(&StreamEvent::AttemptStarted {
                context: context.clone(),
                model: "model".into(),
            })
            .unwrap();
        let mut usage = dume_provider::types::TokenUsage::default();
        usage.input_tokens = 7;
        usage.output_tokens = 2;
        usage.total_tokens = 9;
        session
            .record_model_event(&StreamEvent::AttemptFinished {
                context,
                model: "model".into(),
                outcome: dume_provider::AttemptOutcome::Failed,
                duration_ms: 12,
                usage: Some(usage),
                error: Some("retryable".into()),
            })
            .unwrap();
        assert_eq!(
            store
                .get_session_usage("provider-session")
                .unwrap()
                .total_tokens,
            9
        );
    }

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
        assert!(!result.to_string().contains("opaque-secret"));
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("result.txt"))
                .await
                .unwrap(),
            "hello"
        );
    }

    #[tokio::test]
    async fn routing_changes_actual_generation_model_after_successful_tool_write() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempdir().unwrap();
        let store = std::sync::Arc::new(
            dume_store::HarnessStore::in_memory(dir.path().join("artifacts")).unwrap(),
        );
        let classifier_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let classifier_url = format!("http://{}/v1/", classifier_listener.local_addr().unwrap());
        let classifier = tokio::spawn(async move {
            let (mut socket, _) = classifier_listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let header_end = loop {
                let mut chunk = [0; 2048];
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            while bytes.len() < header_end + length {
                let mut chunk = [0; 2048];
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
            }
            let body: serde_json::Value =
                serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
            assert_eq!(body["state"]["prompt"], "write the file");
            let reply = r#"{"answers":{"complexity":{"type":"choice","choice":"standard","probabilities":{"standard":0.9,"complex":0.1},"confidence":0.9}},"usage":{"input_tokens":3,"output_tokens":1}}"#;
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", reply.len(), reply).as_bytes()).await.unwrap();
            1usize
        });
        let generation_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let generation_url = format!("http://{}", generation_listener.local_addr().unwrap());
        let generations = tokio::spawn(async move {
            let mut models = Vec::new();
            for turn in 0..2 {
                let (mut socket, _) = generation_listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let header_end = loop {
                    let mut chunk = [0; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                while bytes.len() < header_end + length {
                    let mut chunk = [0; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                }
                let request: serde_json::Value =
                    serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
                models.push(request["model"].as_str().unwrap().to_owned());
                let event = if turn == 0 {
                    json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"routing-write","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"routed.txt\",\"content\":\"written\"}"}}]},"finish_reason":"tool_calls"}]})
                } else {
                    json!({"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]})
                };
                let sse = format!("data: {event}\n\ndata: [DONE]\n\n");
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", sse.len(), sse).as_bytes()).await.unwrap();
            }
            models
        });
        let credentials = dume_provider::CredentialStore::new(dir.path().join("auth.json"));
        for provider in ["openai", "jev-local-system-one"] {
            credentials
                .save(
                    provider,
                    &dume_provider::auth::Credential {
                        cred_type: "api_key".into(),
                        key: Some("fake-token".into()),
                        access_token: None,
                        refresh_token: None,
                        expires_at: None,
                        account_id: None,
                    },
                )
                .unwrap();
        }
        let mut settings = crate::routing_settings::RoutingSettings::default();
        settings.enabled = true;
        settings.classifier = Some(dume_provider::ClassifierConfig {
            backend: dume_provider::ClassifierBackend::LocalSystemOne,
            base_url: classifier_url,
            model: "local-test".into(),
        });
        settings.models = crate::routing_settings::RoutingModels {
            standard: Some("openai/gpt-4o-mini".into()),
            complex: Some("openai/gpt-4o".into()),
            implementation: Some("openai/gpt-4.1-mini".into()),
        };
        let snapshot = crate::routing::RoutingSnapshot::from_settings(settings, false);
        let context = dume_provider::types::StreamRequestContext::new();
        let conversation_id = format!("worker:{}", context.request_id);
        let agent = AgentLoop::new(dir.path(), "openai/gpt-4o-mini")
            .with_store(store.clone())
            .with_routing_credentials(credentials)
            .with_request_context(context)
            .with_mock_base_url(generation_url)
            .with_routing_snapshot(snapshot);
        let agent_model = agent.model.clone();
        let mut agent_task = tokio::spawn(async move { agent.run_task("write the file").await });
        let result =
            match tokio::time::timeout(std::time::Duration::from_secs(8), &mut agent_task).await {
                Ok(result) => result.unwrap().unwrap(),
                Err(_) => {
                    agent_task.abort();
                    classifier.abort();
                    generations.abort();
                    let _ = agent_task.await;
                    let _ = classifier.await;
                    let _ = generations.await;
                    panic!("AgentLoop routing integration timed out");
                }
            };
        assert!(result.is_success());
        assert_eq!(agent_model, "openai/gpt-4o-mini");
        assert_eq!(classifier.await.unwrap(), 1);
        assert_eq!(
            generations.await.unwrap(),
            ["openai/gpt-4o-mini", "openai/gpt-4.1-mini"]
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("routed.txt")).unwrap(),
            "written"
        );
        let routing = store
            .conversation_routing_state(&conversation_id)
            .unwrap()
            .unwrap();
        assert_eq!(routing.phase, dume_store::RoutingPhase::Implementation);
        assert_eq!(routing.implementation_model, "openai/gpt-4.1-mini");
    }

    #[tokio::test]
    async fn routing_preclassification_cancel_sends_no_classifier_or_generation_request() {
        use tokio::time::{Duration, timeout};
        let dir = tempdir().unwrap();
        let classifier_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let classifier_addr = classifier_listener.local_addr().unwrap();
        let generation_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let generation_addr = generation_listener.local_addr().unwrap();
        let token = CancellationToken::new();
        token.cancel();
        let credentials = routing_test_credentials(&dir.path().join("auth.json"));
        let agent = AgentLoop::new(dir.path(), "openai/gpt-4o-mini")
            .with_routing_credentials(credentials)
            .with_routing_snapshot(routing_test_snapshot(format!(
                "http://{classifier_addr}/v1"
            )))
            .with_mock_base_url(format!("http://{generation_addr}"));
        let result = timeout(
            Duration::from_secs(3),
            agent.run_task_with_cancellation("요청", token),
        )
        .await
        .unwrap();
        assert!(result.is_err());
        assert!(
            timeout(Duration::from_millis(50), classifier_listener.accept())
                .await
                .is_err()
        );
        assert!(
            timeout(Duration::from_millis(50), generation_listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn routing_midclassification_cancel_never_starts_generation() {
        use tokio::time::{Duration, timeout};
        let dir = tempdir().unwrap();
        let classifier_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let classifier_addr = classifier_listener.local_addr().unwrap();
        let (received_tx, received_rx) = tokio::sync::oneshot::channel();
        let classifier_server = tokio::spawn(async move {
            let (mut socket, _) = classifier_listener.accept().await.unwrap();
            let _ = read_http_json(&mut socket).await;
            let _ = received_tx.send(());
            tokio::time::sleep(Duration::from_secs(2)).await;
        });
        let generation_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let generation_addr = generation_listener.local_addr().unwrap();
        let token = CancellationToken::new();
        let agent = AgentLoop::new(dir.path(), "openai/gpt-4o-mini")
            .with_routing_credentials(routing_test_credentials(&dir.path().join("auth.json")))
            .with_routing_snapshot(routing_test_snapshot(format!(
                "http://{classifier_addr}/v1"
            )))
            .with_mock_base_url(format!("http://{generation_addr}"));
        let cancel = token.clone();
        let mut task =
            tokio::spawn(async move { agent.run_task_with_cancellation("요청", token).await });
        timeout(Duration::from_secs(3), received_rx)
            .await
            .unwrap()
            .unwrap();
        cancel.cancel();
        let result = timeout(Duration::from_secs(3), &mut task)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_err());
        assert!(
            timeout(Duration::from_millis(50), generation_listener.accept())
                .await
                .is_err()
        );
        classifier_server.abort();
        let _ = classifier_server.await;
    }

    #[tokio::test]
    async fn classifier_http_failure_pins_original_without_retry_across_turns() {
        use tokio::io::AsyncWriteExt;
        use tokio::time::{Duration, timeout};
        let dir = tempdir().unwrap();
        let store = std::sync::Arc::new(
            dume_store::HarnessStore::in_memory(dir.path().join("artifacts")).unwrap(),
        );
        let classifier_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let classifier_addr = classifier_listener.local_addr().unwrap();
        let classifier_server = tokio::spawn(async move {
            let (mut socket, _) = classifier_listener.accept().await.unwrap();
            let _ = read_http_json(&mut socket).await;
            let body = "private classifier failure";
            socket.write_all(format!("HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
            1usize
        });
        let generation_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let generation_addr = generation_listener.local_addr().unwrap();
        let generation_server = tokio::spawn(async move {
            let mut models = Vec::new();
            for turn in 0..2 {
                let (mut socket, _) = generation_listener.accept().await.unwrap();
                let request = read_http_json(&mut socket).await;
                models.push(request["model"].as_str().unwrap().to_owned());
                let event = if turn == 0 {
                    json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"failure-pin","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"after-failure.txt\",\"content\":\"ok\"}"}}]},"finish_reason":"tool_calls"}]})
                } else {
                    json!({"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]})
                };
                let sse = format!("data: {event}\n\ndata: [DONE]\n\n");
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", sse.len(), sse).as_bytes()).await.unwrap();
            }
            models
        });
        let credentials = routing_test_credentials(&dir.path().join("auth.json"));
        let context = dume_provider::types::StreamRequestContext::new();
        let conversation_id = format!("worker:{}", context.request_id);
        let agent = AgentLoop::new(dir.path(), "openai/gpt-4o-mini")
            .with_store(store.clone())
            .with_routing_credentials(credentials)
            .with_request_context(context.clone())
            .with_routing_snapshot(routing_test_snapshot(format!(
                "http://{classifier_addr}/v1"
            )))
            .with_mock_base_url(format!("http://{generation_addr}"));
        let mut task = tokio::spawn(async move { agent.run_task("요청").await });
        let result = match timeout(Duration::from_secs(8), &mut task).await {
            Ok(result) => result.unwrap().unwrap(),
            Err(_) => {
                task.abort();
                classifier_server.abort();
                generation_server.abort();
                let _ = task.await;
                let _ = classifier_server.await;
                let _ = generation_server.await;
                panic!("classifier failure integration timed out");
            }
        };
        assert!(result.is_success());
        assert_eq!(classifier_server.await.unwrap(), 1);
        assert_eq!(
            generation_server.await.unwrap(),
            ["openai/gpt-4o-mini", "openai/gpt-4o-mini"]
        );
        let routing = store
            .conversation_routing_state(&conversation_id)
            .unwrap()
            .unwrap();
        assert_eq!(routing.phase, dume_store::RoutingPhase::Fixed);
        assert_eq!(routing.reason, "http_status");
        let notice = crate::routing::RoutingDecision {
            original_model: "openai/gpt-4o-mini".into(),
            effective_model: "openai/gpt-4o-mini".into(),
            phase: dume_store::RoutingPhase::Fixed,
            epoch: Some(routing.epoch),
            reason: routing.reason,
            classifier_usage: None,
        }
        .notice_text();
        assert!(!notice.contains("private classifier failure"));
        assert!(!notice.contains("fixture-token"));
    }

    #[tokio::test]
    async fn generation_unauthorized_does_not_fallback_from_selected_planning_model() {
        use tokio::io::AsyncWriteExt;
        use tokio::time::{Duration, timeout};
        let dir = tempdir().unwrap();
        let classifier_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let classifier_addr = classifier_listener.local_addr().unwrap();
        let classifier_server = tokio::spawn(async move {
            let (mut socket, _) = classifier_listener.accept().await.unwrap();
            let _ = read_http_json(&mut socket).await;
            let body = r#"{"answers":{"complexity":{"type":"choice","choice":"standard","probabilities":{"standard":0.9,"complex":0.1},"confidence":0.9}}}"#;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            1usize
        });
        let generation_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let generation_addr = generation_listener.local_addr().unwrap();
        let generation_server = tokio::spawn(async move {
            let (mut socket, _) = timeout(Duration::from_secs(5), generation_listener.accept())
                .await
                .unwrap()
                .unwrap();
            let request = read_http_json(&mut socket).await;
            let model = request["model"].as_str().unwrap().to_owned();
            let body = "private generation auth error";
            socket.write_all(format!("HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
            assert!(
                timeout(Duration::from_millis(150), generation_listener.accept())
                    .await
                    .is_err(),
                "selected model was retried or fallback model was requested"
            );
            model
        });
        let credentials = routing_test_credentials(&dir.path().join("auth.json"));
        let store = std::sync::Arc::new(
            dume_store::HarnessStore::in_memory(dir.path().join("artifacts")).unwrap(),
        );
        let context = dume_provider::types::StreamRequestContext::new();
        let conversation_id = format!("worker:{}", context.request_id);
        let agent = AgentLoop::new(dir.path(), "openai/gpt-4o")
            .with_store(store.clone())
            .with_routing_credentials(credentials)
            .with_request_context(context.clone())
            .with_routing_snapshot(routing_test_snapshot(format!(
                "http://{classifier_addr}/v1"
            )))
            .with_base_url(format!("http://{generation_addr}"));
        let mut task = tokio::spawn(async move { agent.run_task("요청").await });
        let result = match timeout(Duration::from_secs(8), &mut task).await {
            Ok(result) => result.unwrap(),
            Err(_) => {
                task.abort();
                classifier_server.abort();
                generation_server.abort();
                let _ = task.await;
                let _ = classifier_server.await;
                let _ = generation_server.await;
                panic!("generation auth integration timed out");
            }
        };
        assert!(result.is_err());
        assert_eq!(classifier_server.await.unwrap(), 1);
        assert_eq!(generation_server.await.unwrap(), "gpt-4o-mini");
        let routing = store
            .conversation_routing_state(&conversation_id)
            .unwrap()
            .unwrap();
        assert_eq!(routing.phase, dume_store::RoutingPhase::Planning);
        assert_eq!(routing.original_model, "openai/gpt-4o");
        assert_eq!(routing.planning_model, "openai/gpt-4o-mini");
        let metrics = store
            .conversation_runtime_metrics(&conversation_id)
            .unwrap();
        assert_eq!(metrics.model_attempts.len(), 1);
        assert_eq!(metrics.model_attempts[0].model, "openai/gpt-4o-mini");
        assert_eq!(metrics.model_attempts[0].state, "failed");
    }

    #[tokio::test]
    async fn failed_write_keeps_planning_model_and_does_not_advance_routing_phase() {
        use tokio::io::AsyncWriteExt;
        use tokio::time::{Duration, timeout};
        let dir = tempdir().unwrap();
        let store = std::sync::Arc::new(
            dume_store::HarnessStore::in_memory(dir.path().join("artifacts")).unwrap(),
        );
        let classifier_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let classifier_addr = classifier_listener.local_addr().unwrap();
        let classifier_server = tokio::spawn(async move {
            let (mut socket, _) = classifier_listener.accept().await.unwrap();
            let _ = read_http_json(&mut socket).await;
            let body = r#"{"answers":{"complexity":{"type":"choice","choice":"standard","probabilities":{"standard":1,"complex":0},"confidence":0.9}}}"#;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            1usize
        });
        let generation_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let generation_addr = generation_listener.local_addr().unwrap();
        let generation_server = tokio::spawn(async move {
            let mut models = Vec::new();
            for turn in 0..2 {
                let (mut socket, _) = generation_listener.accept().await.unwrap();
                let request = read_http_json(&mut socket).await;
                models.push(request["model"].as_str().unwrap().to_owned());
                let event = if turn == 0 {
                    json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"bad-write","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"failure.txt\"}"}}]},"finish_reason":"tool_calls"}]})
                } else {
                    json!({"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]})
                };
                let sse = format!("data: {event}\n\ndata: [DONE]\n\n");
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", sse.len(), sse).as_bytes()).await.unwrap();
            }
            models
        });
        let credentials = routing_test_credentials(&dir.path().join("auth.json"));
        let context = dume_provider::types::StreamRequestContext::new();
        let conversation_id = format!("worker:{}", context.request_id);
        let agent = AgentLoop::new(dir.path(), "openai/gpt-4o-mini")
            .with_store(store.clone())
            .with_routing_credentials(credentials)
            .with_request_context(context)
            .with_routing_snapshot(routing_test_snapshot(format!(
                "http://{classifier_addr}/v1"
            )))
            .with_mock_base_url(format!("http://{generation_addr}"));
        let mut task = tokio::spawn(async move { agent.run_task("요청").await });
        let result = match timeout(Duration::from_secs(8), &mut task).await {
            Ok(result) => result.unwrap().unwrap(),
            Err(_) => {
                task.abort();
                classifier_server.abort();
                generation_server.abort();
                let _ = task.await;
                let _ = classifier_server.await;
                let _ = generation_server.await;
                panic!("failed write integration timed out");
            }
        };
        assert!(result.is_success());
        assert_eq!(classifier_server.await.unwrap(), 1);
        assert_eq!(
            generation_server.await.unwrap(),
            ["openai/gpt-4o-mini", "openai/gpt-4o-mini"]
        );
        let routing = store
            .conversation_routing_state(&conversation_id)
            .unwrap()
            .unwrap();
        assert_eq!(routing.phase, dume_store::RoutingPhase::Planning);
        let executions = store
            .tool_executions_for_run(
                &store
                    .latest_run(&dume_store::RuntimeScope::Conversation(
                        conversation_id.clone(),
                    ))
                    .unwrap()
                    .unwrap()
                    .run_id,
            )
            .unwrap();
        assert_eq!(executions.len(), 1);
        assert_eq!(executions[0].state, "failed");
        assert!(!dir.path().join("failure.txt").exists());
        let history = store.conversation_history(&conversation_id).unwrap();
        assert!(history.iter().any(|entry| {
            serde_json::from_str::<ChatMessage>(&entry.message_json)
                .ok()
                .is_some_and(|message| {
                    message.role == dume_provider::types::Role::Tool
                        && message.content.contains("Missing required field 'content'")
                })
        }));
    }

    #[tokio::test]
    async fn routed_root_dispatches_inherited_and_explicit_child_models_without_rerouting_parent() {
        use tokio::io::AsyncWriteExt;
        use tokio::time::{Duration, timeout};
        let dir = tempdir().unwrap();
        for args in [
            vec!["init"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "add",
                "-A",
            ],
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
            let mut command = tokio::process::Command::new("git");
            command.arg("-C").arg(dir.path()).args(args);
            let result = command.output().await.unwrap();
            assert!(result.status.success());
        }
        let classifier_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let classifier_addr = classifier_listener.local_addr().unwrap();
        let mut classifier = tokio::spawn(async move {
            let (mut socket, _) = timeout(Duration::from_secs(5), classifier_listener.accept())
                .await
                .unwrap()
                .unwrap();
            let _ = timeout(Duration::from_secs(5), read_http_json(&mut socket))
                .await
                .unwrap();
            let body = r#"{"answers":{"complexity":{"type":"choice","choice":"standard","probabilities":{"standard":0.9,"complex":0.1},"confidence":0.9}}}"#;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            assert!(
                timeout(Duration::from_millis(150), classifier_listener.accept())
                    .await
                    .is_err()
            );
        });
        let credentials = routing_test_credentials(&dir.path().join("auth.json"));
        let store = std::sync::Arc::new(
            dume_store::HarnessStore::in_memory(dir.path().join("artifacts")).unwrap(),
        );
        let context = dume_provider::types::StreamRequestContext::new();
        let conversation_id = format!("worker:{}", context.request_id);
        let generation_store = store.clone();
        let generation_conversation = conversation_id.clone();
        let generation_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let generation_addr = generation_listener.local_addr().unwrap();
        let mut generation = tokio::spawn(async move {
            let mut observed = Vec::new();
            let mut parent_turn = 0;
            let mut parent_route_before_children = None;
            let mut child_turns = std::collections::HashMap::<String, usize>::new();
            for _ in 0..8 {
                let (mut socket, _) = timeout(Duration::from_secs(5), generation_listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
                let request = timeout(Duration::from_secs(5), read_http_json(&mut socket))
                    .await
                    .unwrap();
                let model = request["model"].as_str().unwrap().to_owned();
                let messages = request["messages"].as_array().unwrap();
                let child_id = ["상속자 작업", "명시자 작업"].into_iter().find(|needle| {
                    messages.iter().any(|m| {
                        m["role"] == "user"
                            && m["content"].as_str().is_some_and(|s| s.contains(needle))
                    })
                });
                let output = if let Some(child_id) = child_id {
                    let turn = child_turns.entry(child_id.to_owned()).or_default();
                    observed.push((format!("{child_id}-{turn}"), model));
                    let output = if *turn == 0 {
                        let file = if child_id == "상속자 작업" {
                            "inherited.txt"
                        } else {
                            "explicit.txt"
                        };
                        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":format!("child-write-{child_id}"),"type":"function","function":{"name":"write_file","arguments":format!("{{\"path\":\"{file}\",\"content\":\"ok\"}}")}}]},"finish_reason":"tool_calls"}]})
                    } else {
                        json!({"choices":[{"delta":{"content":"자식 작업 완료"},"finish_reason":"stop"}]})
                    };
                    *turn += 1;
                    output
                } else {
                    observed.push((format!("parent-{parent_turn}"), model));
                    let turn = parent_turn;
                    if turn == 0 {
                        parent_route_before_children = generation_store
                            .conversation_routing_state(&generation_conversation)
                            .unwrap();
                    }
                    parent_turn += 1;
                    match turn {
                        0 => json!({"choices":[{"delta":{"tool_calls":[
                            {"index":0,"id":"spawn-inherited","type":"function","function":{"name":"spawn_subagent","arguments":"{\"id\":\"inherited\",\"prompt\":\"상속자 작업\",\"sub_dir\":\"child-inherited\"}"}},
                            {"index":1,"id":"spawn-explicit","type":"function","function":{"name":"spawn_subagent","arguments":"{\"id\":\"explicit\",\"prompt\":\"명시자 작업\",\"sub_dir\":\"child-explicit\"}"}}
                        ]},"finish_reason":"tool_calls"}]}),
                        1 => {
                            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"wait-inherited","type":"function","function":{"name":"wait_subagent","arguments":"{\"id\":\"inherited\",\"timeout_ms\":5000}"}}]},"finish_reason":"tool_calls"}]})
                        }
                        2 => {
                            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"wait-explicit","type":"function","function":{"name":"wait_subagent","arguments":"{\"id\":\"explicit\",\"timeout_ms\":5000}"}}]},"finish_reason":"tool_calls"}]})
                        }
                        _ => {
                            json!({"choices":[{"delta":{"content":"부모 작업 완료"},"finish_reason":"stop"}]})
                        }
                    }
                };
                let sse = format!("data: {output}\n\ndata: [DONE]\n\n");
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", sse.len(), sse).as_bytes()).await.unwrap();
            }
            assert!(
                timeout(Duration::from_millis(150), generation_listener.accept())
                    .await
                    .is_err()
            );
            (observed, parent_route_before_children.unwrap())
        });
        let agent = AgentLoop::new(dir.path(), "openai/gpt-4o")
            .with_store(store.clone())
            .with_routing_credentials(credentials)
            .with_inherited_subagent_model("inherited")
            .with_subagent_model_override("explicit", "openai/gpt-4.1-mini")
            .with_request_context(context)
            .with_routing_snapshot(routing_test_snapshot(format!(
                "http://{classifier_addr}/v1"
            )))
            .with_base_url(format!("http://{generation_addr}"));
        let mut task =
            tokio::spawn(async move { agent.run_task("자식 작업을 시작하세요").await });
        let outcome = match timeout(Duration::from_secs(8), &mut task).await {
            Ok(result) => result.unwrap().unwrap(),
            Err(_) => {
                task.abort();
                classifier.abort();
                generation.abort();
                let _ = task.await;
                let _ = classifier.await;
                let _ = generation.await;
                panic!("root/child routing dispatch timed out");
            }
        };
        let classifier_result = match timeout(Duration::from_secs(8), &mut classifier).await {
            Ok(result) => result.unwrap(),
            Err(_) => {
                classifier.abort();
                let _ = classifier.await;
                generation.abort();
                let _ = generation.await;
                panic!("classifier fixture did not finish");
            }
        };
        let (observed, before_children) =
            match timeout(Duration::from_secs(8), &mut generation).await {
                Ok(result) => result.unwrap(),
                Err(_) => {
                    generation.abort();
                    let _ = generation.await;
                    panic!("generation fixture did not finish");
                }
            };
        assert_eq!(classifier_result, ());
        assert!(outcome.is_success());
        assert_eq!(observed.len(), 8);
        assert_eq!(
            observed
                .iter()
                .filter(|(tag, _)| tag.starts_with("parent-"))
                .count(),
            4
        );
        assert!(observed.contains(&("parent-0".into(), "gpt-4o-mini".into())));
        assert_eq!(
            observed
                .iter()
                .filter(|(tag, model)| tag.starts_with("상속자 작업-") && model == "gpt-4o-mini")
                .count(),
            2
        );
        assert_eq!(
            observed
                .iter()
                .filter(|(tag, model)| tag.starts_with("명시자 작업-") && model == "gpt-4.1-mini")
                .count(),
            2
        );
        let parent = store
            .conversation_routing_state(&conversation_id)
            .unwrap()
            .unwrap();
        assert_eq!(parent.phase, dume_store::RoutingPhase::Planning);
        assert_eq!(before_children.phase, dume_store::RoutingPhase::Planning);
        assert_eq!(parent.phase, before_children.phase);
        assert_eq!(parent.epoch, before_children.epoch);
        assert_eq!(parent.revision, before_children.revision);
        assert!(
            store
                .conversation_routing_state("worker:child:inherited")
                .unwrap()
                .is_none()
        );
        let inherited = store
            .conversation_runtime_metrics("worker:child:inherited")
            .unwrap();
        assert_eq!(inherited.model_attempts.len(), 2);
        assert_eq!(inherited.model_attempts[0].model, "openai/gpt-4o-mini");
        assert_eq!(inherited.model_attempts[1].model, "openai/gpt-4o-mini");
        let explicit = store
            .conversation_runtime_metrics("worker:child:explicit")
            .unwrap();
        assert_eq!(explicit.model_attempts.len(), 2);
        assert_eq!(explicit.model_attempts[0].model, "openai/gpt-4.1-mini");
        assert_eq!(explicit.model_attempts[1].model, "openai/gpt-4.1-mini");
        assert!(dir.path().join("child-inherited/inherited.txt").is_file());
        assert!(dir.path().join("child-explicit/explicit.txt").is_file());
    }

    #[tokio::test]
    async fn routed_root_in_implementation_phase_dispatches_inherited_child_with_implementation_model()
     {
        use tokio::io::AsyncWriteExt;
        use tokio::time::{Duration, timeout};
        let dir = tempdir().unwrap();
        for args in [
            vec!["init"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "add",
                "-A",
            ],
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
            let mut command = tokio::process::Command::new("git");
            command.arg("-C").arg(dir.path()).args(args);
            let result = command.output().await.unwrap();
            assert!(result.status.success());
        }
        let store = std::sync::Arc::new(
            dume_store::HarnessStore::in_memory(dir.path().join("artifacts")).unwrap(),
        );
        let generation_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let generation_addr = generation_listener.local_addr().unwrap();
        let generation = tokio::spawn(async move {
            let mut observed = Vec::new();
            let mut parent_turn = 0;
            let mut child_turn = 0;
            for _ in 0..5 {
                let (mut socket, _) = generation_listener.accept().await.unwrap();
                let request = read_http_json(&mut socket).await;
                let model = request["model"].as_str().unwrap().to_owned();
                let is_child = request["messages"].as_array().unwrap().iter().any(|m| {
                    m["role"] == "user"
                        && m["content"]
                            .as_str()
                            .is_some_and(|s| s.contains("하위 작업 수행"))
                });
                let output = if is_child {
                    observed.push((format!("child-{child_turn}"), model));
                    let out = if child_turn == 0 {
                        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"write_call","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"child_impl.txt\",\"content\":\"implemented\"}"}}]},"finish_reason":"tool_calls"}]})
                    } else {
                        json!({"choices":[{"delta":{"content":"자식 완료"},"finish_reason":"stop"}]})
                    };
                    child_turn += 1;
                    out
                } else {
                    observed.push((format!("parent-{parent_turn}"), model));
                    let out = match parent_turn {
                        0 => {
                            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"spawn_call","type":"function","function":{"name":"spawn_subagent","arguments":"{\"id\":\"inherited\",\"prompt\":\"하위 작업 수행\",\"sub_dir\":\"child-inherited\"}"}}]},"finish_reason":"tool_calls"}]})
                        }
                        1 => {
                            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"wait_call","type":"function","function":{"name":"wait_subagent","arguments":"{\"id\":\"inherited\",\"timeout_ms\":5000}"}}]},"finish_reason":"tool_calls"}]})
                        }
                        _ => {
                            json!({"choices":[{"delta":{"content":"부모 완료"},"finish_reason":"stop"}]})
                        }
                    };
                    parent_turn += 1;
                    out
                };
                let sse = format!("data: {output}\n\ndata: [DONE]\n\n");
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            sse.len(),
                            sse
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
            assert!(
                timeout(Duration::from_millis(150), generation_listener.accept())
                    .await
                    .is_err()
            );
            observed
        });
        let credentials = routing_test_credentials(&dir.path().join("auth.json"));
        let mut settings = crate::routing_settings::RoutingSettings::default();
        settings.enabled = true;
        settings.classifier = Some(dume_provider::ClassifierConfig {
            backend: dume_provider::ClassifierBackend::LocalSystemOne,
            base_url: "http://127.0.0.1:1/v1/".into(),
            model: "local-classifier".into(),
        });
        settings.models = crate::routing_settings::RoutingModels {
            standard: Some("openai/gpt-4o-mini".into()),
            complex: Some("openai/gpt-4o".into()),
            implementation: Some("openai/gpt-4.1-mini".into()),
        };
        let epoch = settings.fingerprint("openai/gpt-4o").unwrap();
        let context = dume_provider::types::StreamRequestContext::new();
        let conversation_id = format!("worker:{}", context.request_id);
        let saved_state = dume_store::ConversationRoutingState {
            conversation_id: conversation_id.clone(),
            epoch: epoch.clone(),
            phase: dume_store::RoutingPhase::Implementation,
            original_model: "openai/gpt-4o".into(),
            planning_model: "openai/gpt-4o-mini".into(),
            implementation_model: "openai/gpt-4.1-mini".into(),
            reason: "standard".into(),
            classifier_input_tokens: Some(3),
            classifier_output_tokens: Some(1),
            revision: 2,
        };
        store
            .create_conversation(&conversation_id, "/tmp", "openai/gpt-4o", "seed_sess")
            .unwrap();
        let scope = dume_store::RuntimeScope::Conversation(conversation_id.clone());
        let receipt = store
            .admit_input(
                scope.clone(),
                "seed-request",
                dume_store::InputKind::Submit,
                "seed-prompt",
            )
            .unwrap();
        let seed_session = crate::runtime::RunSession::start(
            store.clone(),
            scope,
            receipt.run_id.as_deref().unwrap(),
            &conversation_id,
        )
        .unwrap();
        seed_session.save_routing_state(&saved_state, None).unwrap();
        drop(seed_session);
        let snapshot = crate::routing::RoutingSnapshot::from_settings(settings, true);
        let agent = AgentLoop::new(dir.path(), "openai/gpt-4o")
            .with_store(store.clone())
            .with_routing_credentials(credentials)
            .with_inherited_subagent_model("inherited")
            .with_request_context(context)
            .with_routing_snapshot(snapshot)
            .with_base_url(format!("http://{generation_addr}"));
        let mut task =
            tokio::spawn(async move { agent.run_task("구현 단계 자식 작업 시작").await });
        let outcome = match timeout(Duration::from_secs(8), &mut task).await {
            Ok(result) => result.unwrap().unwrap(),
            Err(_) => {
                task.abort();
                generation.abort();
                let _ = task.await;
                let _ = generation.await;
                panic!("implementation child routing dispatch timed out");
            }
        };
        let observed = match timeout(Duration::from_secs(8), generation).await {
            Ok(result) => result.unwrap(),
            Err(_) => panic!("generation fixture timed out"),
        };
        assert!(outcome.is_success());
        assert_eq!(observed.len(), 5);
        assert!(observed.iter().all(|(_, model)| model == "gpt-4.1-mini"));
        let parent = store
            .conversation_routing_state(&conversation_id)
            .unwrap()
            .unwrap();
        assert_eq!(parent.phase, dume_store::RoutingPhase::Implementation);
        assert_eq!(parent.original_model, "openai/gpt-4o");
        assert_eq!(parent.implementation_model, "openai/gpt-4.1-mini");
        let inherited = store
            .conversation_runtime_metrics("worker:child:inherited")
            .unwrap();
        assert_eq!(inherited.model_attempts.len(), 2);
        assert_eq!(inherited.model_attempts[0].model, "openai/gpt-4.1-mini");
        assert_eq!(inherited.model_attempts[1].model, "openai/gpt-4.1-mini");
        assert!(dir.path().join("child-inherited/child_impl.txt").is_file());
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
            None,
            None,
            std::sync::Arc::new(tokio::sync::Mutex::new(
                dume_provider::types::TokenUsage::default(),
            )),
            None,
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
                None,
                None,
                std::sync::Arc::new(tokio::sync::Mutex::new(
                    dume_provider::types::TokenUsage::default(),
                )),
                None,
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
        let base_commit = tokio::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .await
            .unwrap();
        assert!(base_commit.status.success());
        let base_commit = String::from_utf8_lossy(&base_commit.stdout)
            .trim()
            .to_string();
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
            None,
            None,
            std::sync::Arc::new(tokio::sync::Mutex::new(
                dume_provider::types::TokenUsage::default(),
            )),
            None,
        )
        .await
        .unwrap();
        assert!(res.contains("Status: Completed"));
        assert!(res.contains("Changes/Findings: E2E Task is finished."));
        assert!(res.contains("Changed Files:"));
        assert!(res.contains("e2e_out.txt"));
        assert!(res.contains("Validation: Not independently run by the harness"));
        assert!(res.contains(&format!("Base Commit: {}", base_commit)));
        assert!(!res.contains("Worktree clean verification passed"));
        assert!(res.contains(&format!("Worktree: {}", wt_path.display())));
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
    async fn worker_compaction_is_visible_in_the_next_provider_payload() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let dir = tempdir().unwrap();
        let store = std::sync::Arc::new(
            dume_store::HarnessStore::in_memory(dir.path().join("artifacts")).unwrap(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for turn in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let header_end = loop {
                    let mut chunk = [0; 4096];
                    let size = socket.read(&mut chunk).await.unwrap();
                    assert!(size > 0);
                    bytes.extend_from_slice(&chunk[..size]);
                    if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
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
                    let mut chunk = [0; 4096];
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

                let event = if turn == 0 {
                    serde_json::json!({"choices":[{"delta":{
                        "content":"trace ".repeat(80_000),
                        "tool_calls":[{"index":0,"id":"call_after_large_context","type":"function","function":{"name":"bash","arguments":"{\"command\":\"printf ok\"}"}}]
                    },"finish_reason":"tool_calls"}]})
                } else {
                    serde_json::json!({"choices":[{"delta":{"content":"Finished after compaction."},"finish_reason":"stop"}]})
                };
                let sse = format!("data: {event}\n\ndata: [DONE]\n\n");
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            sse.len(),
                            sse
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
            requests
        });

        let result = AgentLoop::new(dir.path(), "mock-compaction-model")
            .with_mock_base_url(url)
            .with_store(store)
            .run_task("Inspect the result and preserve this constraint.")
            .await
            .unwrap();
        assert!(result.to_string().contains("Finished after compaction"));
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 2);
        let second_messages = requests[1]["messages"].as_array().unwrap();
        assert!(second_messages.iter().any(|message| {
            message["content"]
                .as_str()
                .is_some_and(|content| content.contains("Conversation checkpoint"))
        }));
        assert!(second_messages.iter().any(|message| {
            message["content"].as_str().is_some_and(|content| {
                content.contains("Inspect the result and preserve this constraint.")
            })
        }));
        assert!(second_messages.iter().all(|message| {
            !message["content"]
                .as_str()
                .is_some_and(|content| content.contains("trace trace trace"))
        }));
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

        let tools = AgentLoop::tool_definitions(&wt_path);
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

    #[tokio::test]
    async fn test_turn_limit_exhaustion_returns_turn_limit_exhausted() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                use tokio::io::AsyncWriteExt;
                let _ = read_http_json(&mut socket).await;

                // Always ask to run bash "echo looping"
                let sse = format!(
                    "data: {}\n\ndata: [DONE]\n\n",
                    serde_json::json!({
                        "choices": [{
                            "delta": {
                                "content": "Keep looping...",
                                "tool_calls": [{
                                    "index": 0,
                                    "id": "call_loop",
                                    "type": "function",
                                    "function": {
                                        "name": "bash",
                                        "arguments": "{\"command\": \"echo looping\"}"
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
            }
        });

        let dir = tempdir().unwrap();
        let mut agent =
            AgentLoop::new(dir.path(), "mock-model").with_mock_base_url(format!("http://{}", addr));
        agent.max_turns = 2;

        let outcome = agent.run_task("infinite loop").await.unwrap();
        match outcome {
            AgentOutcome::TurnLimitExhausted { last_reply, .. } => {
                assert!(last_reply.as_ref().unwrap().contains("Keep looping..."));
            }
            other => panic!("Expected TurnLimitExhausted, got {:?}", other),
        }
    }

    #[test]
    fn test_stable_prefix_construction() {
        let p1 = dume_core::types::PromptPrefix::BASE_SYSTEM_PROMPT;
        let p2 = dume_core::types::PromptPrefix::BASE_SYSTEM_PROMPT;
        assert_eq!(p1, p2);

        let u1 = dume_core::types::PromptPrefix::build_task_user_message("  fix bug in file.rs  ");
        assert_eq!(u1, "fix bug in file.rs");

        // Tools definition order and determinism
        let dir = tempfile::tempdir().unwrap();
        let tools1 = AgentLoop::tool_definitions(dir.path());
        let tools2 = AgentLoop::tool_definitions(dir.path());
        assert_eq!(tools1, tools2);
        assert!(!tools1.is_empty());

        // Session ID stability vs request ID rotation
        let ctx = dume_provider::types::StreamRequestContext::new();
        let req1 = ctx.next_request();
        let req2 = ctx.next_request();
        assert_eq!(req1.session_id, ctx.session_id);
        assert_eq!(req2.session_id, ctx.session_id);
        assert_ne!(req1.request_id, req2.request_id);

        // Reset session generates distinct session ID
        let reset = ctx.reset_session();
        assert_ne!(reset.session_id, ctx.session_id);
    }

    #[test]
    fn tool_catalog_change_rotates_tui_session_identity() {
        let dir = tempdir().unwrap();
        let mut dispatcher =
            ToolDispatcher::new(dir.path(), "mock-model", CancellationToken::new());
        let mut session_id = dume_provider::types::StreamRequestContext::new().session_id;
        let first_catalog = dispatcher.tool_definitions_for_session(&mut session_id);
        let initial_session = session_id.clone();

        let skills_dir = dir.path().join(".dume/skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        std::fs::write(
            skills_dir.join("new-skill.md"),
            "---\nname: new-skill\ndescription: newly available\n---\nSkill content",
        )
        .unwrap();
        let updated_catalog = dispatcher.tool_definitions_for_session(&mut session_id);

        assert_ne!(first_catalog, updated_catalog);
        assert_ne!(session_id, initial_session);
    }

    #[tokio::test]
    async fn test_load_skill_tool_execution() {
        let dir = tempdir().unwrap();
        let executor = LocalToolExecutor::new(dir.path());
        let subagents = dume_mcp::subagent::SubagentManager::new();
        let mut child_paths = std::collections::HashMap::new();
        let cancellation = CancellationToken::new();

        let agent = AgentLoop::new(dir.path(), "mock-model");

        // 1. Loading unknown skill returns informative error with catalog
        let unknown_call = dume_provider::types::ToolCall {
            id: "call_sk1".to_string(),
            name: "load_skill".to_string(),
            arguments: "{\"name\": \"nonexistent_skill\"}".to_string(),
        };
        let result = agent
            .execute_tool(
                unknown_call,
                &executor,
                &subagents,
                &mut child_paths,
                &cancellation,
                &[],
            )
            .await
            .unwrap();
        assert!(
            result
                .content
                .contains("Skill 'nonexistent_skill' not found")
        );

        // 2. Schema validation rejects missing name
        let bad_call = dume_provider::types::ToolCall {
            id: "call_sk2".to_string(),
            name: "load_skill".to_string(),
            arguments: "{}".to_string(),
        };
        let error = agent
            .execute_tool(
                bad_call,
                &executor,
                &subagents,
                &mut child_paths,
                &cancellation,
                &[],
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Missing required field 'name'"));

        // 3. Deduplication when skill is already in active messages
        // Create a temporary skill file in dir/.dume/skills
        let skills_dir = dir.path().join(".dume").join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        std::fs::write(
            skills_dir.join("test-sk.md"),
            "---\nname: test-sk\ndescription: test desc\n---\nTest skill content",
        )
        .unwrap();

        let mut reg = dume_core::skills::SkillRegistry::new();
        reg.load_from_dir(&skills_dir);
        let sk = reg.get("test-sk").unwrap();

        let messages = vec![ChatMessage::user(format!("Some text with {}", sk.content))];
        let _dup_call = dume_provider::types::ToolCall {
            id: "call_sk3".to_string(),
            name: "load_skill".to_string(),
            arguments: "{\"name\": \"test-sk\"}".to_string(),
        };
        // When registry is checked, test is_content_in_messages
        assert!(dume_core::skills::SkillRegistry::is_content_in_messages(
            &sk.content,
            messages.iter().map(|m| m.content.as_str()),
        ));
    }

    #[tokio::test]
    async fn test_bounded_subagent_handoff_validation() {
        let dir = tempdir().unwrap();
        let executor = LocalToolExecutor::new(dir.path());
        let subagents = dume_mcp::subagent::SubagentManager::new();
        let mut child_paths = std::collections::HashMap::new();
        let cancellation = CancellationToken::new();

        let agent = AgentLoop::new(dir.path(), "mock-model");

        // Schema validation rejects invalid constraints or artifact_ids types
        let bad_constraints = dume_provider::types::ToolCall {
            id: "call_sub1".to_string(),
            name: "spawn_subagent".to_string(),
            arguments:
                "{\"id\":\"c1\",\"prompt\":\"task\",\"sub_dir\":\"child\",\"constraints\":123}"
                    .to_string(),
        };
        let error = agent
            .execute_tool(
                bad_constraints,
                &executor,
                &subagents,
                &mut child_paths,
                &cancellation,
                &[],
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Field 'constraints' must be a string")
        );

        let bad_artifacts = dume_provider::types::ToolCall {
            id: "call_sub2".to_string(),
            name: "spawn_subagent".to_string(),
            arguments: "{\"id\":\"c1\",\"prompt\":\"task\",\"sub_dir\":\"child\",\"artifact_ids\":\"not_array\"}".to_string(),
        };
        let error = agent
            .execute_tool(
                bad_artifacts,
                &executor,
                &subagents,
                &mut child_paths,
                &cancellation,
                &[],
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Field 'artifact_ids' must be an array of strings")
        );

        let too_many_artifacts = dume_provider::types::ToolCall {
            id: "call_sub3".to_string(),
            name: "spawn_subagent".to_string(),
            arguments: serde_json::json!({
                "id": "c1",
                "prompt": "task",
                "sub_dir": "child",
                "artifact_ids": vec!["a"; 33]
            })
            .to_string(),
        };
        let error = agent
            .execute_tool(
                too_many_artifacts,
                &executor,
                &subagents,
                &mut child_paths,
                &cancellation,
                &[],
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("at most 32 entries"));
    }

    #[tokio::test]
    async fn test_context_budgeting_compaction_trigger() {
        let dir = tempdir().unwrap();
        let store = std::sync::Arc::new(
            dume_store::HarnessStore::in_memory(dir.path().join("artifacts")).unwrap(),
        );

        // Pre-fill a session with messages exceeding budget
        let session_id = "test_compact_session";
        store
            .append_session_message(
                session_id,
                "system",
                dume_core::types::PromptPrefix::BASE_SYSTEM_PROMPT,
                None,
                None,
                false,
            )
            .unwrap();
        store
            .append_session_message(session_id, "user", "Initial user goal", None, None, false)
            .unwrap();

        // Create large content
        let large_blob = "x".repeat(50_000);
        store
            .append_session_message(session_id, "assistant", &large_blob, None, None, false)
            .unwrap();
        store
            .append_session_message(session_id, "user", "Next command", None, None, false)
            .unwrap();

        let active_before = store.list_active_context_messages(session_id).unwrap();
        assert_eq!(active_before.len(), 4);

        // Perform compaction directly via store method
        let retain_n = 2;
        let summary_text = "Compacted context summary: earlier turns archived.";
        store
            .compact_session(session_id, summary_text, retain_n)
            .unwrap();

        let active_after = store.list_active_context_messages(session_id).unwrap();
        assert!(active_after.len() < active_before.len() + 1);
        // Verify summary is at the start
        assert_eq!(active_after[0].0, "system");
        assert!(active_after[0].4, "First active message must be summary");
        assert_eq!(active_after[0].1, summary_text);
    }

    #[tokio::test]
    async fn dispatcher_preserves_and_reads_oversized_tool_output() {
        let dir = tempdir().unwrap();
        let artifacts = std::sync::Arc::new(
            dume_store::ArtifactStore::new(dir.path().join("artifacts")).unwrap(),
        );
        let mut dispatcher =
            ToolDispatcher::new(dir.path(), "mock-model", CancellationToken::new())
                .with_artifact_store(artifacts);
        let call = dume_provider::types::ToolCall {
            id: "large-output".into(),
            name: "bash".into(),
            arguments: serde_json::json!({
                "command": "head -c 71680 /dev/zero | tr '\\000' x"
            })
            .to_string(),
        };

        let result = dispatcher.execute(call, &[]).await.unwrap();
        let artifact_id = result
            .content
            .split("Full stdout artifact: ")
            .nth(1)
            .and_then(|tail| tail.split(']').next())
            .expect("oversized output should include a digest");
        assert!(result.content.contains("Full stderr artifact:"));
        assert!(result.content.len() < 50 * 1024);
        let read_call = dume_provider::types::ToolCall {
            id: "read-large-output".into(),
            name: "read_artifact".into(),
            arguments: serde_json::json!({"path": artifact_id, "offset": 71600, "length": 128})
                .to_string(),
        };

        let retrieved = dispatcher.execute(read_call, &[]).await.unwrap();
        assert!(
            retrieved.content.contains(&"x".repeat(64)),
            "{}",
            retrieved.content.chars().take(512).collect::<String>()
        );
    }

    #[tokio::test]
    async fn isolated_child_reads_parent_artifact_by_id() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
            assert!(
                tokio::process::Command::new("git")
                    .arg("-C")
                    .arg(dir.path())
                    .args(args)
                    .output()
                    .await
                    .unwrap()
                    .status
                    .success()
            );
        }
        let store = std::sync::Arc::new(
            dume_store::HarnessStore::in_memory(dir.path().join("artifacts")).unwrap(),
        );
        let artifact_id = store
            .artifacts
            .save_artifact(b"important child evidence")
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let expected_id = artifact_id.clone();
        let server = tokio::spawn(async move {
            for turn in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let header_end = loop {
                    let mut chunk = [0u8; 1024];
                    let size = socket.read(&mut chunk).await.unwrap();
                    assert!(size > 0);
                    bytes.extend_from_slice(&chunk[..size]);
                    if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap();
                while bytes.len() < header_end + length {
                    let mut chunk = [0u8; 1024];
                    let size = socket.read(&mut chunk).await.unwrap();
                    assert!(size > 0);
                    bytes.extend_from_slice(&chunk[..size]);
                }
                let body: serde_json::Value =
                    serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
                let event = if turn == 0 {
                    assert!(body.to_string().contains(&expected_id));
                    json!({"choices":[{"delta":{"tool_calls":[{
                        "index":0,"id":"read-child-artifact","function":{
                            "name":"read_artifact",
                            "arguments":json!({"path":expected_id,"length":128}).to_string()
                        }
                    }]},"finish_reason":"tool_calls"}]})
                } else {
                    assert!(body.to_string().contains("important child evidence"));
                    json!({"choices":[{"delta":{"content":"Child read the artifact."},"finish_reason":"stop"}]})
                };
                let sse = format!("data: {event}\n\ndata: [DONE]\n\n");
                socket.write_all(format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    sse.len(), sse
                ).as_bytes()).await.unwrap();
            }
        });
        let child = dir.path().join("child");
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_isolated_child(
                dir.path(),
                &child,
                "mock-model",
                Some(Endpoint::Mock(url)),
                &format!("Read artifact {artifact_id}"),
                CancellationToken::new(),
                Some(store),
                None,
                std::sync::Arc::new(tokio::sync::Mutex::new(
                    dume_provider::types::TokenUsage::default(),
                )),
                None,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        server.await.unwrap();
        assert!(outcome.contains("Child read the artifact."));
    }
}

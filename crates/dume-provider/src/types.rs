use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Opaque Responses reasoning items, for stateless Codex replay only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub codex_reasoning: Vec<Value>,
}

impl ChatMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_call_id: None,
            tool_calls: None,
            codex_reasoning: Vec::new(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_call_id: None,
            tool_calls: None,
            codex_reasoning: Vec::new(),
        }
    }

    pub fn assistant_with_tool_calls(
        content: impl Into<String>,
        tool_calls: Vec<ToolCall>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_call_id: None,
            tool_calls: Some(tool_calls),
            codex_reasoning: Vec::new(),
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            tool_call_id: None,
            tool_calls: None,
            codex_reasoning: Vec::new(),
        }
    }

    pub fn tool(content: impl Into<String>, tool_call_id: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_call_id: Some(tool_call_id.into()),
            tool_calls: None,
            codex_reasoning: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_usage: Option<Value>,
    #[serde(default = "default_is_complete")]
    pub is_complete: bool,
}

fn default_is_complete() -> bool {
    true
}

impl Default for TokenUsage {
    fn default() -> Self {
        Self {
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            cache_read_tokens: None,
            cache_write_tokens: None,
            raw_usage: None,
            is_complete: true,
        }
    }
}

impl TokenUsage {
    pub fn unknown() -> Self {
        Self {
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            cache_read_tokens: None,
            cache_write_tokens: None,
            raw_usage: None,
            is_complete: false,
        }
    }

    /// Sum usage across DISTINCT requests (e.g. across turns or between parent and children).
    pub fn accumulate(&mut self, other: &TokenUsage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.total_tokens += other.total_tokens;
        if let Some(r) = other.cache_read_tokens {
            *self.cache_read_tokens.get_or_insert(0) += r;
        }
        if let Some(w) = other.cache_write_tokens {
            *self.cache_write_tokens.get_or_insert(0) += w;
        }
        if !other.is_complete {
            self.is_complete = false;
        }
    }

    /// Merge updates for the SAME request where events may carry cumulative or partial fields.
    /// This prevents double counting when message_start and message_delta or repeated chunks arrive.
    pub fn merge_cumulative(&mut self, other: &TokenUsage) {
        if other.input_tokens > 0 {
            self.input_tokens = self.input_tokens.max(other.input_tokens);
        }
        if other.output_tokens > 0 {
            self.output_tokens = self.output_tokens.max(other.output_tokens);
        }
        self.total_tokens = self.input_tokens + self.output_tokens;
        if other.cache_read_tokens.is_some() {
            self.cache_read_tokens = other.cache_read_tokens;
        }
        if other.cache_write_tokens.is_some() {
            self.cache_write_tokens = other.cache_write_tokens;
        }
        if other.raw_usage.is_some() {
            self.raw_usage = other.raw_usage.clone();
        }
        if !other.is_complete {
            self.is_complete = false;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    TextDelta(String),
    /// Opaque replay metadata. Never render as transcript or streaming text.
    CodexReasoning(Vec<Value>),
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments_delta: String,
    },
    Usage(TokenUsage),
    Completed {
        finish_reason: String,
    },
    Error(String),
}

/// Request and session context for model invocations.
/// Transports that require session tracking (e.g. OpenCode) use these fields,
/// while other transports ignore them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamRequestContext {
    pub session_id: String,
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub benchmark_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub case_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<String>,
}

impl StreamRequestContext {
    /// Create a new session context with fresh random opaque session and request IDs.
    pub fn new() -> Self {
        Self {
            session_id: Self::generate_opaque_id(),
            request_id: Self::generate_opaque_id(),
            benchmark_run_id: None,
            case_id: None,
            variant: None,
            attempt_id: None,
            agent_id: Some("main".to_string()),
            parent_agent_id: None,
        }
    }

    /// Create a new request context for an existing session with a new opaque request ID.
    pub fn for_session(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            request_id: Self::generate_opaque_id(),
            benchmark_run_id: None,
            case_id: None,
            variant: None,
            attempt_id: None,
            agent_id: Some("main".to_string()),
            parent_agent_id: None,
        }
    }

    pub fn from_ids(session_id: impl Into<String>, request_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            request_id: request_id.into(),
            benchmark_run_id: None,
            case_id: None,
            variant: None,
            attempt_id: None,
            agent_id: Some("main".to_string()),
            parent_agent_id: None,
        }
    }

    /// Produce a new request context retaining the current session ID and benchmark metadata
    /// but with a fresh request ID.
    pub fn next_request(&self) -> Self {
        Self {
            session_id: self.session_id.clone(),
            request_id: Self::generate_opaque_id(),
            benchmark_run_id: self.benchmark_run_id.clone(),
            case_id: self.case_id.clone(),
            variant: self.variant.clone(),
            attempt_id: self.attempt_id.clone(),
            agent_id: self.agent_id.clone(),
            parent_agent_id: self.parent_agent_id.clone(),
        }
    }

    /// Produce a child context for a subagent.
    pub fn child_agent(&self, child_id: &str) -> Self {
        Self {
            session_id: Self::generate_opaque_id(),
            request_id: Self::generate_opaque_id(),
            benchmark_run_id: self.benchmark_run_id.clone(),
            case_id: self.case_id.clone(),
            variant: self.variant.clone(),
            attempt_id: self.attempt_id.clone(),
            agent_id: Some(child_id.to_string()),
            parent_agent_id: self.agent_id.clone().or_else(|| Some("main".to_string())),
        }
    }

    fn generate_opaque_id() -> String {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).expect("Failed to generate random bytes for request context");
        hex::encode(bytes)
    }
}

impl Default for StreamRequestContext {
    fn default() -> Self {
        Self::new()
    }
}

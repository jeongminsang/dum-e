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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamRequestContext {
    pub session_id: String,
    pub request_id: String,
}

impl StreamRequestContext {
    /// Create a new session context with fresh random opaque session and request IDs.
    pub fn new() -> Self {
        Self {
            session_id: Self::generate_opaque_id(),
            request_id: Self::generate_opaque_id(),
        }
    }

    /// Create a new request context for an existing session with a new opaque request ID.
    pub fn for_session(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            request_id: Self::generate_opaque_id(),
        }
    }

    /// Produce a new request context retaining the current session ID but with a fresh request ID.
    pub fn next_request(&self) -> Self {
        Self {
            session_id: self.session_id.clone(),
            request_id: Self::generate_opaque_id(),
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

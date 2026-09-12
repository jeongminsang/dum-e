pub mod anthropic;
pub mod auth;
pub mod catalog;
pub mod client;
pub mod gemini;
pub mod openai;
pub mod oauth;
pub mod types;

pub use anthropic::AnthropicProvider;
pub use auth::{Credential, CredentialStore};
pub use catalog::{ModelCatalog, ModelInfo};
pub use client::LlmClient;
pub use gemini::GeminiProvider;
pub use oauth::{generate_pkce, start_oauth_callback_server, exchange_code_for_token, refresh_oauth_token, OAuthTokenResponse};
pub use openai::OpenAiProvider;
pub use types::{ChatMessage, Role, StreamEvent, ToolCall, ToolDefinition};


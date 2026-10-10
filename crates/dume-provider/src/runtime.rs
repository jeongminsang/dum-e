use crate::auth::{Credential, CredentialStore};
use crate::catalog::{ModelCatalog, ModelInfo};
use crate::codex::CodexProvider;
use crate::types::{
    AttemptOutcome, ChatMessage, ProviderFailureKind, StreamEvent, TokenUsage, ToolDefinition,
};
use crate::{AnthropicProvider, GeminiProvider, OpenAiProvider};
use anyhow::{Context, Result};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// A catalog model bound to exactly one authenticated inference transport.
/// Resolve again before every model turn rather than retaining expired tokens.
pub struct ResolvedProvider {
    pub model: ModelInfo,
    transport: Transport,
}

enum Transport {
    Anthropic(AnthropicProvider),
    AnthropicOAuth(AnthropicProvider),
    OpenAi(OpenAiProvider),
    Codex(CodexProvider),
    Google(GeminiProvider),
}

impl ResolvedProvider {
    /// Override the selected transport's API root, retaining its resolved credentials.
    /// Callers must trust this endpoint with those credentials.
    pub fn with_base_url(mut self, base_url: &str) -> Result<Self> {
        let url = reqwest::Url::parse(base_url).context("Invalid provider base URL")?;
        anyhow::ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "Provider base URL must be an HTTP(S) API root without credentials, query, or fragment"
        );
        self.transport = match self.transport {
            Transport::Anthropic(p) => Transport::Anthropic(p.with_base_url(base_url)),
            Transport::AnthropicOAuth(p) => Transport::AnthropicOAuth(p.with_base_url(base_url)),
            Transport::OpenAi(p) => Transport::OpenAi(p.with_base_url(base_url)),
            Transport::Codex(p) => Transport::Codex(p.with_base_url(base_url)),
            Transport::Google(p) => Transport::Google(p.with_base_url(base_url)),
        };
        Ok(self)
    }

    pub async fn stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        self.stream_with_context(messages, tools, None, tx).await
    }

    pub async fn stream_with_context(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        context: Option<&crate::types::StreamRequestContext>,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        let seed = context.cloned().unwrap_or_default();
        let mut attempt_context = seed.clone();
        let mut delay = Duration::from_millis(500);
        let retry_started = Instant::now();
        for attempt in 1..=4 {
            let started = Instant::now();
            let (attempt_tx, mut attempt_rx) = mpsc::channel(32);
            let model = &self.model.id;
            let transport = &self.transport;
            let attempt_ctx = attempt_context.clone();
            let run = async {
                match transport {
                    Transport::Anthropic(p) | Transport::AnthropicOAuth(p) => {
                        p.stream_with_context(
                            model,
                            messages,
                            tools,
                            Some(&attempt_ctx),
                            attempt_tx,
                        )
                        .await
                    }
                    Transport::OpenAi(p) => {
                        p.stream_with_context(
                            model,
                            messages,
                            tools,
                            Some(&attempt_ctx),
                            attempt_tx,
                        )
                        .await
                    }
                    Transport::Codex(p) => {
                        p.stream_with_context(
                            model,
                            messages,
                            tools,
                            Some(&attempt_ctx),
                            attempt_tx,
                        )
                        .await
                    }
                    Transport::Google(p) => p.stream(model, messages, tools, attempt_tx).await,
                }
            };
            tokio::pin!(run);
            let _ = tx
                .send(StreamEvent::AttemptStarted {
                    context: attempt_context.clone(),
                    model: format!("{}/{}", self.model.provider, self.model.id),
                })
                .await;
            let mut usage: Option<TokenUsage> = None;
            let mut completion = None;
            let mut provider_error = None;
            let mut failure_kind = None;
            let mut event_retry_after = None;
            let result = loop {
                tokio::select! {
                  event = attempt_rx.recv() => {
                    let Some(event) = event else { break Err(anyhow::anyhow!("Provider stream ended without completion")); };
                  match event {
                      StreamEvent::Usage(u) => { usage.get_or_insert_with(TokenUsage::default).merge_cumulative(&u); let _ = tx.send(StreamEvent::Usage(u)).await; }
                      StreamEvent::Completed { finish_reason } => completion = Some(finish_reason),
                      StreamEvent::Error(error) => provider_error = Some(error),
                      StreamEvent::ProviderError { kind, message, retry_after_ms } => { failure_kind = Some(kind); event_retry_after = retry_after_ms.map(Duration::from_millis); provider_error = Some(message); }
                      other => { let _ = tx.send(other).await; }
                  }
                  },
                  result = &mut run => break result,
                }
            };
            while let Some(event) = attempt_rx.recv().await {
                match event {
                    StreamEvent::Error(error) => provider_error = Some(error),
                    StreamEvent::ProviderError {
                        kind,
                        message,
                        retry_after_ms,
                    } => {
                        failure_kind = Some(kind);
                        event_retry_after = retry_after_ms.map(Duration::from_millis);
                        provider_error = Some(message);
                    }
                    StreamEvent::Completed { finish_reason } => completion = Some(finish_reason),
                    StreamEvent::Usage(u) => {
                        usage
                            .get_or_insert_with(TokenUsage::default)
                            .merge_cumulative(&u);
                        let _ = tx.send(StreamEvent::Usage(u)).await;
                    }
                    other => {
                        let _ = tx.send(other).await;
                    }
                }
            }
            let mut server_delay = None;
            if let Err(error) = &result {
                if let Some(http) = error.downcast_ref::<crate::client::HttpFailure>() {
                    server_delay = http.retry_after.map(|d| d.min(Duration::from_secs(30)));
                    if http.status == reqwest::StatusCode::TOO_MANY_REQUESTS
                        || http.status.is_server_error()
                    {
                        failure_kind = Some(ProviderFailureKind::Transient);
                    } else if http.status == reqwest::StatusCode::UNAUTHORIZED
                        || http.status == reqwest::StatusCode::FORBIDDEN
                    {
                        failure_kind = Some(ProviderFailureKind::Auth);
                    } else {
                        failure_kind = Some(ProviderFailureKind::InvalidRequest);
                    }
                }
                provider_error.get_or_insert_with(|| format!("{error:#}"));
            }
            let error_text = provider_error.unwrap_or_default();
            if result.is_err() && failure_kind.is_none() {
                failure_kind = Some(ProviderFailureKind::Protocol);
            }
            let retryable = failure_kind == Some(ProviderFailureKind::Transient);
            let succeeded = result.is_ok() && error_text.is_empty() && completion.is_some();
            if !succeeded {
                if let Some(usage) = usage.as_mut() {
                    usage.is_complete = false;
                }
            }
            let elapsed = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
            let _ = tx
                .send(StreamEvent::AttemptFinished {
                    context: attempt_context.clone(),
                    model: format!("{}/{}", self.model.provider, self.model.id),
                    outcome: if succeeded {
                        AttemptOutcome::Succeeded
                    } else {
                        AttemptOutcome::Failed
                    },
                    duration_ms: elapsed,
                    usage,
                    error: (!succeeded).then(|| {
                        if error_text.is_empty() {
                            "Provider stream ended without completion".to_string()
                        } else {
                            error_text.clone()
                        }
                    }),
                })
                .await;
            if succeeded {
                let _ = tx
                    .send(StreamEvent::Completed {
                        finish_reason: completion.unwrap(),
                    })
                    .await;
                return Ok(());
            }
            if retryable && attempt < 4 && retry_started.elapsed() < Duration::from_secs(120) {
                let retry_delay = server_delay
                    .or(event_retry_after)
                    .unwrap_or(delay)
                    .min(Duration::from_secs(30))
                    .min(Duration::from_secs(120).saturating_sub(retry_started.elapsed()));
                let delay_ms = retry_delay.as_millis() as u64;
                let _ = tx
                    .send(StreamEvent::Retrying {
                        next_attempt: attempt + 1,
                        delay_ms,
                        reason: error_text.clone(),
                    })
                    .await;
                tokio::time::sleep(retry_delay).await;
                delay = (delay * 2).min(Duration::from_secs(2));
                attempt_context = seed.next_request();
                continue;
            }
            let _ = tx
                .send(StreamEvent::ProviderError {
                    kind: failure_kind.unwrap_or(ProviderFailureKind::Unknown),
                    message: if error_text.is_empty() {
                        "Provider stream ended without completion".into()
                    } else {
                        error_text.clone()
                    },
                    retry_after_ms: event_retry_after
                        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64),
                })
                .await;
            if let Err(error) = result {
                return Err(error);
            }
            anyhow::bail!("{}", error_text);
        }
        unreachable!()
    }
}

/// Check if a provider and its wire protocol (api) are supported by local execution transports.
pub fn is_transport_supported(provider: &str, api: &str) -> bool {
    match provider {
        "anthropic" => api == "anthropic-messages" || api.is_empty(),
        "openai" => api == "openai-responses" || api == "openai-completions" || api.is_empty(),
        "openai-codex" => {
            api == "openai-codex-responses" || api == "openai-responses" || api.is_empty()
        }
        "google" => api == "google-generative-ai" || api.is_empty(),
        "deepseek" => api == "openai-completions" || api.is_empty(),
        "opencode" | "opencode-go" => api == "openai-completions" || api.is_empty(),
        _ => false,
    }
}

/// Check if a catalog model info is supported by local execution transports.
pub fn is_model_supported(model: &ModelInfo) -> bool {
    is_transport_supported(&model.provider, &model.api)
}

fn supported(provider: &str) -> bool {
    matches!(
        provider,
        "anthropic"
            | "openai"
            | "openai-codex"
            | "google"
            | "deepseek"
            | "opencode"
            | "opencode-go"
    )
}

/// Resolve only supported catalog entries, independently of available credentials.
pub fn resolve_model(selection: &str) -> Result<ModelInfo> {
    resolve_model_with_root(selection, &ModelCatalog::models_dir())
}

/// Resolve only supported catalog entries with a custom models directory root.
pub fn resolve_model_with_root(selection: &str, root: &std::path::Path) -> Result<ModelInfo> {
    let selection = selection.trim();
    let (provider, id) = match selection.split_once('/') {
        Some((provider, id)) => {
            let provider = if provider == "gemini" {
                "google"
            } else {
                provider
            };
            anyhow::ensure!(
                supported(provider),
                "Unsupported inference provider '{provider}'"
            );
            (Some(provider), id)
        }
        None => (None, selection),
    };
    let mut matches: Vec<_> = ModelCatalog::list_all_models_with_root(root)?
        .into_iter()
        .filter(|model| {
            is_model_supported(model)
                && model.id == id
                && provider.is_none_or(|provider| model.provider == provider)
        })
        .collect();
    anyhow::ensure!(
        !matches.is_empty(),
        "Unknown supported model '{selection}'; select a catalog provider/model"
    );
    if matches.len() > 1 && provider.is_none() {
        // Tie-breaker: if query is unqualified (e.g. 'claude-sonnet-4-5'), prefer the primary native provider
        // (anthropic, openai, openai-codex, google) over proxy/reseller catalogs (opencode, opencode-go).
        let native_matches: Vec<_> = matches
            .iter()
            .filter(|m| {
                matches!(
                    m.provider.as_str(),
                    "anthropic" | "openai" | "openai-codex" | "google" | "deepseek"
                )
            })
            .cloned()
            .collect();
        if native_matches.len() == 1 {
            return Ok(native_matches.into_iter().next().unwrap());
        } else if !native_matches.is_empty() {
            matches = native_matches;
        }
    }
    if matches.len() > 1 {
        let choices = matches
            .iter()
            .map(|model| format!("{}/{}", model.provider, model.id))
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!("Ambiguous model '{selection}'; qualify as provider/model: {choices}");
    }
    Ok(matches.remove(0))
}

pub async fn resolve_provider(
    selection: &str,
    store: &CredentialStore,
) -> Result<ResolvedProvider> {
    resolve_with(selection, |provider| async move {
        store.resolve_credential(&provider).await
    })
    .await
}

async fn resolve_with<F, Fut>(selection: &str, credential: F) -> Result<ResolvedProvider>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<Option<Credential>>>,
{
    let model = resolve_model(selection)?;
    let cred = credential(model.provider.clone())
        .await
        .with_context(|| format!("Authentication failed for {}", model.provider))?
        .with_context(|| {
            format!(
                "No credentials for {}; authenticate via `dume login`",
                model.provider
            )
        })?;
    bind_credential(model, cred)
}

fn bind_credential(model: ModelInfo, credential: Credential) -> Result<ResolvedProvider> {
    let required = |value: Option<&str>, field: &str| -> Result<String> {
        value
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
            .with_context(|| format!("Invalid {} credential: missing {field}", model.provider))
    };
    let transport = match (model.provider.as_str(), credential.cred_type.as_str()) {
        ("anthropic", "api_key") => Transport::Anthropic(AnthropicProvider::new(&required(
            credential.key.as_deref(),
            "API key",
        )?)),
        ("anthropic", "oauth") => Transport::AnthropicOAuth(AnthropicProvider::new_oauth(
            &required(credential.access_token.as_deref(), "access token")?,
        )),
        ("openai", "api_key") => Transport::OpenAi(OpenAiProvider::new(&required(
            credential.key.as_deref(),
            "API key",
        )?)),
        ("deepseek", "api_key") => Transport::OpenAi(OpenAiProvider::new_deepseek(&required(
            credential.key.as_deref(),
            "API key",
        )?)),
        ("openai-codex", "oauth") => Transport::Codex(CodexProvider::new(
            &required(credential.access_token.as_deref(), "access token")?,
            &required(credential.account_id.as_deref(), "account ID")?,
        )),
        ("google", "api_key") => Transport::Google(GeminiProvider::new(&required(
            credential.key.as_deref(),
            "API key",
        )?)),
        ("opencode" | "opencode-go", "api_key") => {
            let key = required(credential.key.as_deref(), "API key")?;
            let default_base = if model.provider == "opencode-go" {
                "https://opencode.ai/zen/go/v1"
            } else {
                "https://opencode.ai/zen/v1"
            };
            let base_url = model.base_url.as_deref().unwrap_or(default_base);
            match model.api.as_str() {
                "openai-completions" | "" => {
                    let is_free = model.is_free_model();
                    Transport::OpenAi(
                        OpenAiProvider::new(&key)
                            .with_base_url(base_url)
                            .with_opencode_profile(is_free),
                    )
                }
                api => anyhow::bail!(
                    "Unsupported OpenCode API protocol '{api}' for {}; only openai-completions is currently supported",
                    model.provider
                ),
            }
        }
        _ => anyhow::bail!(
            "Unsupported credential type '{}' for {}; OpenAI OAuth requires openai-codex/model",
            credential.cred_type,
            model.provider
        ),
    };
    Ok(ResolvedProvider { model, transport })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum AgentRole {
    Default,
    Planner,
    Architect,
    Executor,
    Critic,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RoleModelProfile {
    pub role: AgentRole,
    pub candidate_models: Vec<String>,
}

impl RoleModelProfile {
    pub fn default_for(role: AgentRole) -> Self {
        let candidates = match role {
            AgentRole::Default => vec![
                "openai-codex/gpt-5.6-luna".into(),
                "anthropic/claude-sonnet-4-5".into(),
                "openai/gpt-4o".into(),
                "opencode/claude-sonnet-4-5".into(),
            ],
            AgentRole::Planner => vec![
                "openai-codex/gpt-5.6-luna".into(),
                "anthropic/claude-opus-4".into(),
                "anthropic/claude-sonnet-4-5".into(),
                "openai/o3-mini".into(),
            ],
            AgentRole::Architect => vec![
                "anthropic/claude-opus-4".into(),
                "openai-codex/gpt-5.6-luna".into(),
                "anthropic/claude-sonnet-4-5".into(),
                "openai/gpt-4o".into(),
            ],
            AgentRole::Executor => vec![
                "anthropic/claude-sonnet-4-5".into(),
                "openai-codex/gpt-5.6-luna".into(),
                "opencode/claude-sonnet-4-5".into(),
                "google/gemini-2.5-pro".into(),
            ],
            AgentRole::Critic => vec![
                "openai-codex/gpt-5.6-luna".into(),
                "anthropic/claude-opus-4".into(),
                "openai/o1".into(),
                "anthropic/claude-sonnet-4-5".into(),
            ],
        };
        Self {
            role,
            candidate_models: candidates,
        }
    }
}

pub async fn resolve_role_provider(
    role: AgentRole,
    override_model: Option<&str>,
    store: &CredentialStore,
) -> Result<ResolvedProvider> {
    if let Some(m) = override_model {
        if !m.trim().is_empty() {
            if let Ok(p) = resolve_provider(m, store).await {
                return Ok(p);
            }
        }
    }

    let profile = RoleModelProfile::default_for(role);
    let mut last_err = None;

    for candidate in &profile.candidate_models {
        match resolve_provider(candidate, store).await {
            Ok(provider) => return Ok(provider),
            Err(e) => {
                last_err = Some(e);
            }
        }
    }

    // If role candidates failed, attempt fallback to any authenticated supported model
    let all = ModelCatalog::list_all_models().unwrap_or_default();
    for m in all {
        if is_model_supported(&m) && store.has_credential(&m.provider) {
            let qualified = format!("{}/{}", m.provider, m.id);
            if let Ok(provider) = resolve_provider(&qualified, store).await {
                return Ok(provider);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| {
        anyhow::anyhow!(
            "No authenticated provider model available for role {:?}",
            role
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::StreamRequestContext;
    use std::collections::HashMap;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    fn credential(kind: &str) -> Credential {
        Credential {
            cred_type: kind.into(),
            key: (kind == "api_key").then(|| "test-key".into()),
            access_token: (kind == "oauth").then(|| "test-access".into()),
            refresh_token: None,
            expires_at: None,
            account_id: Some("test-account".into()),
        }
    }

    fn status_fixture(statuses: Vec<&'static str>) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            for status in statuses {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut data = Vec::new();
                loop {
                    let mut buf = [0; 2048];
                    let n = socket.read(&mut buf).unwrap();
                    data.extend_from_slice(&buf[..n]);
                    let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&data[..end + 4]);
                    let length = headers
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if data.len() >= end + 4 + length {
                        break;
                    }
                }
                let body = "{}";
                write!(socket, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{body}").unwrap();
            }
        });
        (url, handle)
    }

    #[tokio::test]
    async fn attempt_events_keep_provider_identity_while_wire_model_stays_bare() {
        for selection in ["openai/gpt-5.4", "opencode/glm-5.1", "opencode-go/glm-5.1"] {
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            let (url, server) = crate::codex::tests::fixture(body.into());
            let provider =
                bind_credential(resolve_model(selection).unwrap(), credential("api_key"))
                    .unwrap()
                    .with_base_url(&url)
                    .unwrap();
            let (tx, mut rx) = mpsc::channel(32);
            provider
                .stream(&[ChatMessage::user("hi")], &[], tx)
                .await
                .unwrap();
            let (_, request) = server.join().unwrap();
            assert_eq!(request["model"], selection.split_once('/').unwrap().1);
            let mut identities = Vec::new();
            while let Some(event) = rx.recv().await {
                match event {
                    StreamEvent::AttemptStarted { model, .. } => identities.push(model),
                    StreamEvent::AttemptFinished { model, outcome, .. } => {
                        assert_eq!(outcome, AttemptOutcome::Succeeded);
                        identities.push(model);
                    }
                    _ => {}
                }
            }
            assert_eq!(identities, [selection, selection]);
        }
    }

    #[tokio::test]
    async fn retries_are_capped_and_auth_is_never_retried() {
        for (statuses, expected) in [
            (vec!["503 Service Unavailable"; 4], 4),
            (vec!["401 Unauthorized"], 1),
        ] {
            let (url, server) = status_fixture(statuses);
            let provider = bind_credential(
                resolve_model("claude-sonnet-4-5").unwrap(),
                credential("api_key"),
            )
            .unwrap()
            .with_base_url(&url)
            .unwrap();
            let (tx, mut rx) = mpsc::channel(32);
            assert!(
                provider
                    .stream(&[ChatMessage::user("hi")], &[], tx)
                    .await
                    .is_err()
            );
            server.join().unwrap();
            let mut starts = 0;
            while let Some(event) = rx.recv().await {
                if matches!(event, StreamEvent::AttemptStarted { .. }) {
                    starts += 1;
                }
            }
            assert_eq!(starts, expected);
        }
    }

    #[tokio::test]
    async fn unknown_anthropic_sse_error_is_not_retried() {
        let body = "data: {\"type\":\"error\",\"error\":{\"type\":\"future_error_code\",\"message\":\"unknown\"}}\n\n";
        let (url, server) = crate::codex::tests::fixture(body.to_string());
        let provider = bind_credential(
            resolve_model("claude-sonnet-4-5").unwrap(),
            credential("api_key"),
        )
        .unwrap()
        .with_base_url(&url)
        .unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        assert!(
            provider
                .stream(&[ChatMessage::user("hi")], &[], tx)
                .await
                .is_err()
        );
        server.join().unwrap();
        let mut starts = 0;
        let mut terminal_kind = None;
        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::AttemptStarted { .. } => starts += 1,
                StreamEvent::ProviderError { kind, .. } => terminal_kind = Some(kind),
                _ => {}
            }
        }
        assert_eq!(starts, 1);
        assert_eq!(terminal_kind, Some(ProviderFailureKind::Unknown));
    }

    #[tokio::test]
    async fn retries_busy_http_with_attempt_identity_and_reset_boundary() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            for (status, payload, content_type) in [
                (
                    "503 Service Unavailable",
                    r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#,
                    "application/json",
                ),
                (
                    "200 OK",
                    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"discard\"}}\n\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"partial-call\",\"name\":\"run\",\"input\":{}}}\n\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7}}}\n\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}\n\n",
                    "text/event-stream",
                ),
                (
                    "200 OK",
                    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
                    "text/event-stream",
                ),
            ] {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut data = Vec::new();
                loop {
                    let mut buf = [0; 2048];
                    let n = socket.read(&mut buf).unwrap();
                    data.extend_from_slice(&buf[..n]);
                    let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&data[..end + 4]);
                    let length = headers
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if data.len() >= end + 4 + length {
                        break;
                    }
                }
                thread::sleep(Duration::from_millis(10));
                write!(socket, "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len()).unwrap();
            }
        });
        let provider = bind_credential(
            resolve_model("claude-sonnet-4-5").unwrap(),
            credential("api_key"),
        )
        .unwrap()
        .with_base_url(&url)
        .unwrap();
        let ctx = StreamRequestContext::from_ids("session", "first-request");
        let (tx, mut rx) = mpsc::channel(32);
        provider
            .stream_with_context(&[ChatMessage::user("hi")], &[], Some(&ctx), tx)
            .await
            .unwrap();
        server.join().unwrap();
        let mut starts = Vec::new();
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            match &event {
                StreamEvent::AttemptStarted { context, .. } => {
                    starts.push(context.request_id.clone())
                }
                _ => {}
            }
            events.push(event);
        }
        assert_eq!(starts.len(), 3);
        assert_eq!(starts[0], "first-request");
        assert_ne!(starts[0], starts[1]);
        assert_ne!(starts[1], starts[2]);
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::Retrying {
                next_attempt: 2,
                ..
            }
        )));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::TextDelta(t) if t == "done"))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::TextDelta(t) if t == "discard"))
        );
        assert!(events.iter().any(
            |e| matches!(e, StreamEvent::ToolCallDelta { id: Some(id), .. } if id == "partial-call")
        ));
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::Retrying {
                next_attempt: 3,
                ..
            }
        )));
        let finished: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::AttemptFinished {
                    duration_ms,
                    outcome,
                    ..
                } => Some((*duration_ms, *outcome)),
                _ => None,
            })
            .collect();
        assert_eq!(finished.len(), 3);
        assert!(finished.iter().all(|(duration, _)| *duration > 0));
        assert_eq!(finished[0].1, AttemptOutcome::Failed);
        assert_eq!(finished[1].1, AttemptOutcome::Failed);
        assert_eq!(finished[2].1, AttemptOutcome::Succeeded);
        assert!(
            matches!(events.iter().find(|e| matches!(e, StreamEvent::AttemptFinished { context, outcome: AttemptOutcome::Failed, .. } if context.request_id == starts[1])), Some(StreamEvent::AttemptFinished { usage: Some(usage), .. }) if usage.input_tokens == 7 && !usage.is_complete)
        );
    }

    #[tokio::test]
    async fn two_credentials_route_by_model_not_credential_order() {
        let credentials = HashMap::from([
            ("anthropic".to_string(), credential("api_key")),
            ("openai".to_string(), credential("api_key")),
        ]);
        for (selection, expected) in [
            ("claude-sonnet-4-5", "anthropic"),
            ("openai/gpt-5.4", "openai"),
        ] {
            let resolved = resolve_with(selection, |provider| {
                let credential = credentials.get(&provider).cloned();
                async move { Ok(credential) }
            })
            .await
            .unwrap();
            assert_eq!(resolved.model.provider, expected);
            assert!(matches!(
                (expected, resolved.transport),
                ("anthropic", Transport::Anthropic(_)) | ("openai", Transport::OpenAi(_))
            ));
        }
    }

    #[test]
    fn ambiguous_qualified_and_unsupported_selections() {
        let error = resolve_model("gpt-5.4").unwrap_err().to_string();
        assert!(error.contains("Ambiguous"));
        assert!(error.contains("openai/gpt-5.4"));
        assert!(error.contains("openai-codex/gpt-5.4"));
        assert_eq!(
            resolve_model("openai-codex/gpt-5.4").unwrap().provider,
            "openai-codex"
        );
        assert!(resolve_model("openrouter/gpt-5.4").is_err());
        assert!(resolve_model("not-a-model").is_err());
        assert!(resolve_model("openai/").is_err());
    }

    #[test]
    fn endpoint_override_preserves_transport_and_rejects_invalid_roots() {
        for (selection, kind) in [
            ("anthropic/claude-sonnet-4-5", "api_key"),
            ("anthropic/claude-sonnet-4-5", "oauth"),
            ("openai/gpt-5.4", "api_key"),
            ("openai-codex/gpt-5.4", "oauth"),
        ] {
            let provider =
                bind_credential(resolve_model(selection).unwrap(), credential(kind)).unwrap();
            let transport = std::mem::discriminant(&provider.transport);
            let provider = provider.with_base_url("http://127.0.0.1:1234/v1").unwrap();
            assert_eq!(std::mem::discriminant(&provider.transport), transport);
            let expected = resolve_model(selection).unwrap();
            assert_eq!(provider.model.id, expected.id);
            assert_eq!(provider.model.provider, expected.provider);
        }
        for url in [
            "not a URL",
            "file:///tmp/api",
            "https://user:secret@example.com",
            "https://example.com/v1?key=secret",
            "https://example.com/v1#fragment",
        ] {
            let provider = bind_credential(
                resolve_model("openai/gpt-5.4").unwrap(),
                credential("api_key"),
            )
            .unwrap();
            assert!(provider.with_base_url(url).is_err(), "{url}");
        }
        let google = bind_credential(
            resolve_model("google/gemini-2.5-pro").unwrap(),
            credential("api_key"),
        )
        .unwrap();
        assert!(google.with_base_url("http://127.0.0.1:1234").is_ok());
    }

    #[tokio::test]
    async fn google_alias_uses_canonical_credentials_and_model() {
        for selection in [
            "google/gemini-2.5-pro",
            "gemini/gemini-2.5-pro",
            "gemini-2.5-pro",
        ] {
            let resolved = resolve_with(selection, |provider| async move {
                assert_eq!(provider, "google");
                Ok(Some(credential("api_key")))
            })
            .await
            .unwrap();
            assert_eq!(resolved.model.id, "gemini-2.5-pro");
            assert!(matches!(resolved.transport, Transport::Google(_)));
        }
    }

    #[tokio::test]
    async fn missing_credentials_and_refresh_errors_are_not_dropped() {
        let result = resolve_with("openai/gpt-5.4", |_| async { Ok(None) }).await;
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("No credentials for openai")
        );
        let result = resolve_with("openai-codex/gpt-5.4", |_| async {
            anyhow::bail!("refresh rejected")
        })
        .await;
        assert!(format!("{:#}", result.err().unwrap()).contains("refresh rejected"));
        let result = resolve_with("gpt-5.4", |_| async {
            panic!("Ambiguity must precede credentials")
        })
        .await;
        assert!(result.is_err());
    }

    #[test]
    fn oauth_uses_dedicated_transports_and_never_api_key_endpoints() {
        let anthropic = bind_credential(
            resolve_model("claude-sonnet-4-5").unwrap(),
            credential("oauth"),
        )
        .unwrap();
        assert!(matches!(anthropic.transport, Transport::AnthropicOAuth(_)));
        let codex = bind_credential(
            resolve_model("openai-codex/gpt-5.4").unwrap(),
            credential("oauth"),
        )
        .unwrap();
        assert!(matches!(codex.transport, Transport::Codex(_)));
        for (selection, kind) in [
            ("openai/gpt-5.4", "oauth"),
            ("google/gemini-2.5-pro", "oauth"),
            ("openai-codex/gpt-5.4", "api_key"),
        ] {
            assert!(bind_credential(resolve_model(selection).unwrap(), credential(kind)).is_err());
        }
        let mut missing_account = credential("oauth");
        missing_account.account_id = None;
        assert!(
            bind_credential(
                resolve_model("openai-codex/gpt-5.4").unwrap(),
                missing_account
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn each_resolution_reads_current_store_and_propagates_auth_failures() {
        // Codex has no environment API-key override; no global environment mutation.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.json");
        let store = CredentialStore::new(&path);
        let selection = "openai-codex/gpt-5.4";
        let mut valid = credential("oauth");
        valid.expires_at = Some(i64::MAX);
        store.save("openai-codex", &valid).unwrap();
        assert!(matches!(
            resolve_provider(selection, &store).await.unwrap().transport,
            Transport::Codex(_)
        ));

        store.delete("openai-codex").unwrap();
        let error = resolve_provider(selection, &store).await.err().unwrap();
        assert!(
            error
                .to_string()
                .contains("No credentials for openai-codex")
        );

        // Expired without a refresh token fails locally, before any HTTP request.
        valid.expires_at = Some(0);
        store.save("openai-codex", &valid).unwrap();
        let error = resolve_provider(selection, &store).await.err().unwrap();
        assert!(format!("{error:#}").contains("Missing refresh token"));

        std::fs::write(&path, "{").unwrap();
        let error = resolve_provider(selection, &store).await.err().unwrap();
        assert!(format!("{error:#}").contains("Malformed credential store"));
    }

    #[test]
    fn test_transport_capability_matching() {
        assert!(is_transport_supported("anthropic", "anthropic-messages"));
        assert!(!is_transport_supported("anthropic", "unknown-protocol"));
        assert!(is_transport_supported("openai", "openai-responses"));
        assert!(is_transport_supported("openai", "openai-completions"));
        assert!(is_transport_supported(
            "openai-codex",
            "openai-codex-responses"
        ));
        assert!(is_transport_supported("google", "google-generative-ai"));
        assert!(is_transport_supported("opencode", "openai-completions"));
        for provider in ["opencode", "opencode-go"] {
            for api in [
                "",
                "openai-completions",
                "openai-responses",
                "anthropic-messages",
                "google-generative-ai",
            ] {
                let mut model = resolve_model("opencode/big-pickle").unwrap();
                model.provider = provider.into();
                model.api = api.into();
                assert_eq!(
                    is_model_supported(&model),
                    bind_credential(model, credential("api_key")).is_ok(),
                    "{provider}/{api}"
                );
            }
        }
        assert!(!is_transport_supported("bedrock", "bedrock-runtime"));

        let claude = resolve_model("claude-sonnet-4-5").unwrap();
        assert_eq!(claude.api, "anthropic-messages");
        assert!(is_model_supported(&claude));

        let opencode_model = resolve_model("opencode/big-pickle").unwrap();
        assert_eq!(opencode_model.provider, "opencode");
        assert!(is_model_supported(&opencode_model));
    }

    #[test]
    fn opencode_bind_credential_configures_transport() {
        let mut model = resolve_model("opencode/nemotron-3.5-lightning-free").unwrap();
        let cred = credential("api_key");
        let resolved = bind_credential(model.clone(), cred.clone()).unwrap();
        assert!(matches!(resolved.transport, Transport::OpenAi(_)));

        // Non-supported protocol on opencode rejects cleanly
        model.api = "anthropic-messages".to_string();
        let err = bind_credential(model, cred).err().unwrap();
        assert!(
            err.to_string()
                .contains("only openai-completions is currently supported")
        );
    }

    #[test]
    fn deepseek_catalog_and_api_key_route_to_direct_transport() {
        let model = resolve_model("deepseek/deepseek-v4-flash").unwrap();
        assert_eq!(model.api, "openai-completions");
        let resolved = bind_credential(model, credential("api_key")).unwrap();
        assert!(matches!(resolved.transport, Transport::OpenAi(_)));
        assert!(!is_transport_supported("deepseek", "openai-responses"));
        assert!(
            bind_credential(
                resolve_model("deepseek/deepseek-v4-flash").unwrap(),
                credential("oauth")
            )
            .is_err()
        );
    }

    #[test]
    fn resolve_dynamic_cached_model() {
        let temp_dir = tempfile::tempdir().unwrap();
        let agent_dir = temp_dir.path().join(".dume/agent");
        std::fs::create_dir_all(&agent_dir).unwrap();

        // Write a dynamic model into models-cache.json
        let cache_content = r#"[
            {
                "id": "gpt-6-luna",
                "name": "GPT-6 Luna",
                "provider": "openai",
                "api": "openai-responses",
                "contextWindow": 400000,
                "maxTokens": 128000
            }
        ]"#;
        std::fs::write(agent_dir.join("models-cache.json"), cache_content).unwrap();

        let resolved = resolve_model_with_root("openai/gpt-6-luna", &agent_dir);

        let m = resolved.expect("Failed to resolve dynamic cached model");
        assert_eq!(m.id, "gpt-6-luna");
        assert_eq!(m.provider, "openai");
        assert_eq!(m.base_url.as_deref(), Some("https://api.openai.com/v1"));
        assert!(is_model_supported(&m));
    }
}

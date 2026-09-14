use crate::auth::{Credential, CredentialStore};
use crate::catalog::{ModelCatalog, ModelInfo};
use crate::codex::CodexProvider;
use crate::types::{ChatMessage, StreamEvent, ToolDefinition};
use crate::{AnthropicProvider, GeminiProvider, OpenAiProvider};
use anyhow::{Context, Result};
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
            Transport::Google(_) => anyhow::bail!("Base URL override is unsupported for google"),
        };
        Ok(self)
    }

    pub async fn stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        let model = &self.model.id;
        match &self.transport {
            Transport::Anthropic(p) | Transport::AnthropicOAuth(p) => {
                p.stream(model, messages, tools, tx).await
            }
            Transport::OpenAi(p) => p.stream(model, messages, tools, tx).await,
            Transport::Codex(p) => p.stream(model, messages, tools, tx).await,
            Transport::Google(p) => p.stream(model, messages, tools, tx).await,
        }
    }
}

/// Check if a provider and its wire protocol (api) are supported by local execution transports.
pub fn is_transport_supported(provider: &str, api: &str) -> bool {
    match provider {
        "anthropic" => api == "anthropic-messages" || api.is_empty(),
        "openai" => api == "openai-responses" || api == "openai-completions" || api.is_empty(),
        "openai-codex" => api == "openai-codex-responses" || api == "openai-responses" || api.is_empty(),
        "google" => api == "google-generative-ai" || api.is_empty(),
        "opencode" => {
            api == "openai-completions"
                || api == "openai-responses"
                || api == "anthropic-messages"
                || api == "google-generative-ai"
                || api.is_empty()
        }
        "opencode-go" => {
            api == "openai-completions"
                || api == "openai-responses"
                || api == "anthropic-messages"
                || api.is_empty()
        }
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
        "anthropic" | "openai" | "openai-codex" | "google" | "opencode" | "opencode-go"
    )
}

/// Resolve only supported catalog entries, independently of available credentials.
pub fn resolve_model(selection: &str) -> Result<ModelInfo> {
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
    let mut matches: Vec<_> = ModelCatalog::list_all_builtin_models()?
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
            .filter(|m| matches!(m.provider.as_str(), "anthropic" | "openai" | "openai-codex" | "google"))
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
                "anthropic-messages" => {
                    Transport::Anthropic(AnthropicProvider::new(&key).with_base_url(base_url))
                }
                "google-generative-ai" => {
                    Transport::Google(GeminiProvider::new(&key))
                }
                _ => {
                    Transport::OpenAi(OpenAiProvider::new(&key).with_base_url(base_url))
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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
        assert!(google.with_base_url("http://127.0.0.1:1234").is_err());
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
        assert!(is_transport_supported("openai-codex", "openai-codex-responses"));
        assert!(is_transport_supported("google", "google-generative-ai"));
        assert!(is_transport_supported("opencode", "openai-completions"));
        assert!(is_transport_supported("opencode-go", "openai-responses"));
        assert!(!is_transport_supported("bedrock", "bedrock-runtime"));

        let claude = resolve_model("claude-sonnet-4-5").unwrap();
        assert_eq!(claude.api, "anthropic-messages");
        assert!(is_model_supported(&claude));

        let opencode_model = resolve_model("opencode/big-pickle").unwrap();
        assert_eq!(opencode_model.provider, "opencode");
        assert!(is_model_supported(&opencode_model));
    }
}

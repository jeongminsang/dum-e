use anyhow::{Context, Result};
use crate::auth::{Credential, CredentialStore};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelCost {
    #[serde(default)]
    pub input: Option<f64>,
    #[serde(default)]
    pub output: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub provider: String,
    #[serde(default)]
    pub api: String,
    #[serde(rename = "baseUrl", default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub input: Vec<String>,
    #[serde(rename = "contextWindow", default)]
    pub context_window: Option<u64>,
    #[serde(rename = "maxTokens", default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub cost: Option<ModelCost>,
}

#[derive(Deserialize)]
struct ModelsDevProvider {
    models: HashMap<String, ModelsDevModel>,
}

#[derive(Deserialize)]
struct ModelsDevModel {
    id: Option<String>,
    name: Option<String>,
    #[serde(default)]
    reasoning: bool,
    #[serde(default)]
    tool_call: bool,
    modalities: Option<ModelsDevModalities>,
    limit: Option<ModelsDevLimit>,
    cost: Option<ModelCost>,
}

#[derive(Deserialize)]
struct ModelsDevModalities {
    #[serde(default)]
    input: Vec<String>,
}

#[derive(Deserialize)]
struct ModelsDevLimit {
    context: Option<u64>,
    output: Option<u64>,
}

#[derive(Deserialize)]
struct CodexModelsResponse {
    models: Vec<CodexRemoteModel>,
}

#[derive(Deserialize)]
struct CodexRemoteModel {
    slug: String,
    display_name: Option<String>,
    visibility: String,
    context_window: Option<u64>,
    #[serde(default)]
    input_modalities: Vec<String>,
    #[serde(default)]
    supported_reasoning_levels: Vec<serde_json::Value>,
}

#[derive(Serialize, Deserialize)]
struct CodexCache {
    account_id: String,
    models: Vec<ModelInfo>,
}

const CODEX_CATALOG_CLIENT_VERSION: &str = "999.0.0";

impl ModelInfo {
    pub fn is_free_model(&self) -> bool {
        if self.id.ends_with("-free") {
            return true;
        }
        if self.id == "big-pickle" {
            return true;
        }
        if let Some(cost) = &self.cost {
            if cost.input == Some(0.0) && cost.output == Some(0.0) {
                return true;
            }
        }
        false
    }
}

pub struct ModelCatalog;

impl ModelCatalog {
    pub fn anthropic_models() -> Result<HashMap<String, ModelInfo>> {
        let json_str = include_str!("../catalog/anthropic.json");
        Self::parse_catalog(json_str)
    }

    pub fn openai_models() -> Result<HashMap<String, ModelInfo>> {
        let json_str = include_str!("../catalog/openai.json");
        Self::parse_catalog(json_str)
    }

    pub fn google_models() -> Result<HashMap<String, ModelInfo>> {
        let json_str = include_str!("../catalog/google.json");
        Self::parse_catalog(json_str)
    }

    pub fn list_all_builtin_models() -> Result<Vec<ModelInfo>> {
        const ALL_CATALOG_SOURCES: &[&str] = &[
            include_str!("../catalog/amazon-bedrock.json"),
            include_str!("../catalog/ant-ling.json"),
            include_str!("../catalog/anthropic.json"),
            include_str!("../catalog/azure-openai-responses.json"),
            include_str!("../catalog/baseten.json"),
            include_str!("../catalog/cerebras.json"),
            include_str!("../catalog/cloudflare-ai-gateway.json"),
            include_str!("../catalog/cloudflare-workers-ai.json"),
            include_str!("../catalog/deepseek.json"),
            include_str!("../catalog/fireworks.json"),
            include_str!("../catalog/github-copilot.json"),
            include_str!("../catalog/google-vertex.json"),
            include_str!("../catalog/google.json"),
            include_str!("../catalog/groq.json"),
            include_str!("../catalog/huggingface.json"),
            include_str!("../catalog/kimi-coding.json"),
            include_str!("../catalog/minimax-cn.json"),
            include_str!("../catalog/minimax.json"),
            include_str!("../catalog/mistral.json"),
            include_str!("../catalog/moonshotai-cn.json"),
            include_str!("../catalog/moonshotai.json"),
            include_str!("../catalog/nvidia.json"),
            include_str!("../catalog/openai-codex.json"),
            include_str!("../catalog/openai.json"),
            include_str!("../catalog/opencode-go.json"),
            include_str!("../catalog/opencode.json"),
            include_str!("../catalog/openrouter.json"),
            include_str!("../catalog/qwen-token-plan-cn.json"),
            include_str!("../catalog/qwen-token-plan-individual.json"),
            include_str!("../catalog/qwen-token-plan.json"),
            include_str!("../catalog/together.json"),
            include_str!("../catalog/vercel-ai-gateway.json"),
            include_str!("../catalog/xai.json"),
            include_str!("../catalog/xiaomi-token-plan-ams.json"),
            include_str!("../catalog/xiaomi-token-plan-cn.json"),
            include_str!("../catalog/xiaomi-token-plan-sgp.json"),
            include_str!("../catalog/xiaomi.json"),
            include_str!("../catalog/zai-coding-cn.json"),
            include_str!("../catalog/zai.json"),
        ];

        let mut all_map = HashMap::new();
        for (index, src) in ALL_CATALOG_SOURCES.iter().enumerate() {
            let models = Self::parse_catalog(src)
                .with_context(|| format!("Failed to parse builtin catalog at index {index}"))?;
            for info in models.into_values() {
                all_map
                    .entry((info.provider.clone(), info.id.clone()))
                    .or_insert(info);
            }
        }

        let mut all: Vec<ModelInfo> = all_map.into_values().collect();
        all.sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.provider.cmp(&b.provider)));
        Ok(all)
    }

    fn parse_catalog(json_str: &str) -> Result<HashMap<String, ModelInfo>> {
        let catalogs: HashMap<String, HashMap<String, ModelInfo>> = serde_json::from_str(json_str)?;
        let mut map = HashMap::new();

        for models in catalogs.into_values() {
            for (id, info) in models {
                anyhow::ensure!(
                    id == info.id,
                    "Catalog key {id} does not match model ID {}",
                    info.id
                );
                anyhow::ensure!(
                    !map.contains_key(&id),
                    "Duplicate model ID {id} within catalog"
                );
                map.insert(id, info);
            }
        }

        Ok(map)
    }

    /// Base directory for DUM-E agent models configuration and cache.
    pub fn models_dir() -> std::path::PathBuf {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".into());
        std::path::PathBuf::from(home).join(".dume/agent")
    }

    /// Path to user-defined models override file (~/.dume/agent/models.json).
    pub fn user_models_path() -> std::path::PathBuf {
        Self::models_dir().join("models.json")
    }

    /// Path to cached remote models catalog (~/.dume/agent/models-cache.json).
    pub fn cache_path() -> std::path::PathBuf {
        Self::models_dir().join("models-cache.json")
    }

    /// Default remote catalog endpoint (can be overridden via DUME_MODELS_URL).
    pub fn remote_catalog_url() -> String {
        std::env::var("DUME_MODELS_URL").unwrap_or_else(|_| "https://models.dev/api.json".to_string())
    }

    /// Load user models config from a JSON string or file.
    pub fn parse_user_models(json_str: &str) -> Result<Vec<ModelInfo>> {
        // Can be either an array of ModelInfo, or a catalog map like {"openai": {"gpt-6-luna": ...}}
        if let Ok(list) = serde_json::from_str::<Vec<ModelInfo>>(json_str) {
            let mut validated = Vec::new();
            for m in list {
                if let Some(ref u) = m.base_url {
                    let u = u.trim();
                    if !u.starts_with("http://") && !u.starts_with("https://") {
                        continue;
                    }
                }
                validated.push(m);
            }
            return Ok(validated);
        }

        if let Ok(catalogs) = serde_json::from_str::<HashMap<String, HashMap<String, ModelInfo>>>(json_str) {
            let mut list = Vec::new();
            for (_cat_key, models) in catalogs {
                for (_id, m) in models {
                    if let Some(ref u) = m.base_url {
                        let u = u.trim();
                        if !u.starts_with("http://") && !u.starts_with("https://") {
                            continue;
                        }
                    }
                    list.push(m);
                }
            }
            return Ok(list);
        }

        anyhow::bail!("Invalid user models JSON format: expected array or map of ModelInfo")
    }

    /// Load remote cached models with strict field validation.
    /// Remote data cannot overwrite baseUrl or transport protocols.
    pub fn parse_remote_cache(json_str: &str) -> Result<Vec<ModelInfo>> {
        let raw_models = if let Ok(list) = serde_json::from_str::<Vec<ModelInfo>>(json_str) {
            list
        } else if let Ok(catalogs) = serde_json::from_str::<HashMap<String, ModelsDevProvider>>(json_str) {
            let mut list = Vec::new();
            for (provider, catalog) in catalogs {
                for (id, model) in catalog.models {
                    if !model.tool_call || model.id.as_deref().is_some_and(|value| value != id) {
                        continue;
                    }
                    let api = if provider == "openai" || provider == "opencode" || provider == "opencode-go" {
                        "openai-completions"
                    } else {
                        ""
                    };
                    list.push(ModelInfo {
                        name: model.name.unwrap_or_else(|| id.clone()),
                        id,
                        provider: provider.clone(),
                        api: api.to_string(),
                        base_url: None,
                        reasoning: model.reasoning,
                        input: model.modalities.map_or_else(Vec::new, |value| value.input),
                        context_window: model.limit.as_ref().and_then(|value| value.context),
                        max_tokens: model.limit.and_then(|value| value.output),
                        cost: model.cost,
                    });
                }
            }
            list
        } else if let Ok(catalogs) = serde_json::from_str::<HashMap<String, HashMap<String, ModelInfo>>>(json_str) {
            let mut list = Vec::new();
            for models in catalogs.into_values() {
                for info in models.into_values() {
                    list.push(info);
                }
            }
            list
        } else {
            anyhow::bail!("Invalid remote models cache format");
        };

        let mut sanitized = Vec::new();
        for m in raw_models {
            if m.id.trim().is_empty() || m.provider.trim().is_empty() {
                continue;
            }
            // Enforce default standard baseUrl for remote providers to prevent malicious hijacking
            let (standard_api, standard_base_url) = match m.provider.as_str() {
                "openai" => ("openai-responses".to_string(), Some("https://api.openai.com/v1".to_string())),
                "anthropic" => ("anthropic-messages".to_string(), Some("https://api.anthropic.com".to_string())),
                "google" => ("google-generative-ai".to_string(), Some("https://generativelanguage.googleapis.com/v1beta".to_string())),
                "deepseek" => ("openai-completions".to_string(), Some("https://api.deepseek.com".to_string())),
                "openai-codex" => ("openai-codex-responses".to_string(), Some("https://chatgpt.com/backend-api".to_string())),
                "opencode" => ("openai-completions".to_string(), Some("https://opencode.ai/zen/v1".to_string())),
                "opencode-go" => ("openai-completions".to_string(), Some("https://opencode.ai/zen/go/v1".to_string())),
                _ => continue,
            };

            // Validate API wire protocol against supported transports for this provider
            let effective_api = match (m.provider.as_str(), m.api.as_str()) {
                ("anthropic", "anthropic-messages") | ("anthropic", "") => "anthropic-messages",
                ("openai", "openai-responses") | ("openai", "openai-completions") | ("openai", "") => {
                    if m.api.is_empty() { "openai-responses" } else { m.api.as_str() }
                }
                ("openai-codex", "openai-codex-responses") | ("openai-codex", "openai-responses") | ("openai-codex", "") => {
                    if m.api.is_empty() { "openai-codex-responses" } else { m.api.as_str() }
                }
                ("google", "google-generative-ai") | ("google", "") => "google-generative-ai",
                ("deepseek", "openai-completions") | ("deepseek", "") => "openai-completions",
                ("opencode", "openai-completions") | ("opencode", "") => "openai-completions",
                ("opencode-go", "openai-completions") | ("opencode-go", "") => "openai-completions",
                _ => standard_api.as_str(), // Discard unauthorized or unknown protocol override
            };

            sanitized.push(ModelInfo {
                id: m.id,
                name: if m.name.trim().is_empty() { "Unnamed Model".to_string() } else { m.name },
                provider: m.provider,
                api: effective_api.to_string(),
                base_url: standard_base_url,
                reasoning: m.reasoning,
                input: m.input,
                context_window: m.context_window,
                max_tokens: m.max_tokens,
                cost: m.cost,
            });
        }
        anyhow::ensure!(!sanitized.is_empty(), "Remote catalog has no supported models");
        Ok(sanitized)
    }

    fn parse_codex_models(json_str: &str) -> Result<Vec<ModelInfo>> {
        let response: CodexModelsResponse = serde_json::from_str(json_str)?;
        let models: Vec<_> = response.models.into_iter()
            .filter(|model| model.visibility == "list" && !model.slug.trim().is_empty())
            .map(|model| ModelInfo {
                name: model.display_name.filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| model.slug.clone()),
                id: model.slug,
                provider: "openai-codex".to_string(),
                api: "openai-codex-responses".to_string(),
                base_url: Some("https://chatgpt.com/backend-api".to_string()),
                reasoning: !model.supported_reasoning_levels.is_empty(),
                input: model.input_modalities,
                context_window: model.context_window,
                max_tokens: None,
                cost: None,
            })
            .collect();
        anyhow::ensure!(!models.is_empty(), "Codex catalog has no visible models");
        Ok(models)
    }

    fn codex_cache_path(root: &std::path::Path) -> std::path::PathBuf {
        root.join("models-codex-cache.json")
    }

    fn load_codex_cache(root: &std::path::Path, account_id: &str) -> Option<Vec<ModelInfo>> {
        let content = std::fs::read_to_string(Self::codex_cache_path(root)).ok()?;
        let cache: CodexCache = serde_json::from_str(&content).ok()?;
        if cache.account_id != account_id || cache.models.is_empty() {
            return None;
        }
        let models: Vec<_> = cache.models.into_iter().filter_map(|mut model| {
            if model.provider != "openai-codex" || model.id.trim().is_empty() {
                return None;
            }
            model.api = "openai-codex-responses".to_string();
            model.base_url = Some("https://chatgpt.com/backend-api".to_string());
            Some(model)
        }).collect();
        (!models.is_empty()).then_some(models)
    }

    /// List all models merged across the 3 tiers:
    /// Precedence: Tier 3 (User Config) > Tier 2 (Remote Cache) > Tier 1 (Built-in Static)
    /// Keyed by (provider, id).
    pub fn list_all_models() -> Result<Vec<ModelInfo>> {
        Self::list_all_models_with_root(&Self::models_dir())
    }

    /// List all models using a custom agent directory root (useful for testing or custom profile roots).
    pub fn list_all_models_with_root(root: &std::path::Path) -> Result<Vec<ModelInfo>> {
        let mut model_map: HashMap<(String, String), ModelInfo> = HashMap::new();

        // Tier 1: Built-in static catalog
        for model in Self::list_all_builtin_models()? {
            model_map.insert((model.provider.clone(), model.id.clone()), model);
        }

        // Tier 2: Validated local cache
        let cache_file = root.join("models-cache.json");
        if cache_file.is_file() {
            if let Ok(content) = std::fs::read_to_string(&cache_file) {
                if let Ok(cached_models) = Self::parse_remote_cache(&content) {
                    for mut model in cached_models {
                        if let Some(builtin) = model_map.get(&(model.provider.clone(), model.id.clone())) {
                            model.api.clone_from(&builtin.api);
                            model.base_url.clone_from(&builtin.base_url);
                        }
                        model_map.insert((model.provider.clone(), model.id.clone()), model);
                    }
                }
            }
        }

        if let Ok(credentials) = CredentialStore::new(root.join("auth.json")).load() {
            if let Some(account_id) = credentials.get("openai-codex")
                .filter(|credential| credential.cred_type == "oauth" && credential.access_token.is_some())
                .and_then(|credential| credential.account_id.as_deref()) {
                if let Some(cached_models) = Self::load_codex_cache(root, account_id) {
                    for mut model in cached_models {
                        if let Some(builtin) = model_map.get(&(model.provider.clone(), model.id.clone())) {
                            if model.input.is_empty() { model.input.clone_from(&builtin.input); }
                            if model.max_tokens.is_none() { model.max_tokens = builtin.max_tokens; }
                        }
                        model_map.insert((model.provider.clone(), model.id.clone()), model);
                    }
                }
            }
        }

        // Tier 3: User custom configuration
        let user_file = root.join("models.json");
        if user_file.is_file() {
            if let Ok(content) = std::fs::read_to_string(&user_file) {
                if let Ok(user_models) = Self::parse_user_models(&content) {
                    for model in user_models {
                        model_map.insert((model.provider.clone(), model.id.clone()), model);
                    }
                }
            }
        }

        let mut all: Vec<ModelInfo> = model_map.into_values().collect();
        all.sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.provider.cmp(&b.provider)));
        Ok(all)
    }

    /// Fetch remote models catalog and atomically update the cache file.
    pub async fn sync_remote_cache() -> Result<usize> {
        let url = Self::remote_catalog_url();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(45))
            .build()?;

        let resp = client.get(&url).send().await?.error_for_status()?;
        let body = resp.text().await?;

        // Validate content before saving
        let sanitized = Self::parse_remote_cache(&body)
            .with_context(|| "Failed to parse and validate remote models payload")?;
        let count = sanitized.len();

        let target_path = Self::cache_path();
        let parent_dir = target_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        std::fs::create_dir_all(parent_dir)?;

        // Atomic write via tempfile in the same directory
        let serialized = serde_json::to_string_pretty(&sanitized)?;
        let mut tmp = tempfile::NamedTempFile::new_in(parent_dir)?;
        use std::io::Write;
        tmp.write_all(serialized.as_bytes())?;
        tmp.flush()?;
        tmp.persist(&target_path)?;

        Ok(count)
    }

    pub async fn refresh_remote_cache_if_stale() -> Result<Option<usize>> {
        let cache_file = Self::cache_path();
        let fresh = std::fs::metadata(&cache_file)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| std::time::SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age <= std::time::Duration::from_secs(86400));
        if fresh && std::fs::read_to_string(&cache_file)
            .ok()
            .is_some_and(|content| Self::parse_remote_cache(&content).is_ok())
        {
            return Ok(None);
        }
        Self::sync_remote_cache().await.map(Some)
    }

    async fn sync_codex_cache_with(root: &std::path::Path, credential: &Credential, url: &str) -> Result<usize> {
        let token = credential.access_token.as_deref().context("Codex access token missing")?;
        let account_id = credential.account_id.as_deref().context("Codex account ID missing")?;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        let response = client.get(url)
            .query(&[("client_version", CODEX_CATALOG_CLIENT_VERSION)])
            .bearer_auth(token)
            .header("chatgpt-account-id", account_id)
            .header("originator", "codex_cli_rs")
            .header("user-agent", format!("codex_cli_rs/{CODEX_CATALOG_CLIENT_VERSION}"))
            .send().await?.error_for_status()?;
        let models = Self::parse_codex_models(&response.text().await?)?;
        let count = models.len();
        std::fs::create_dir_all(root)?;
        let mut file = tempfile::NamedTempFile::new_in(root)?;
        use std::io::Write;
        serde_json::to_writer_pretty(&mut file, &CodexCache { account_id: account_id.to_string(), models })?;
        file.flush()?;
        file.persist(Self::codex_cache_path(root))?;
        Ok(count)
    }

    pub async fn refresh_codex_cache_if_stale() -> Result<Option<usize>> {
        let root = Self::models_dir();
        let store = CredentialStore::new(root.join("auth.json"));
        let credentials = store.load()?;
        let Some(account_id) = credentials.get("openai-codex")
            .filter(|credential| credential.cred_type == "oauth" && credential.access_token.is_some())
            .and_then(|credential| credential.account_id.as_deref()) else {
            return Ok(None);
        };
        let cache_file = Self::codex_cache_path(&root);
        let fresh = std::fs::metadata(&cache_file)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| std::time::SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age <= std::time::Duration::from_secs(86400));
        if fresh && Self::load_codex_cache(&root, account_id).is_some() {
            return Ok(None);
        }
        let credential = store.resolve_credential("openai-codex").await?
            .context("Codex login required")?;
        Self::sync_codex_cache_with(&root, &credential, "https://chatgpt.com/backend-api/codex/models").await.map(Some)
    }

    pub fn refresh_remote_catalog_background() -> tokio::task::JoinHandle<Result<Option<usize>>> {
        tokio::spawn(Self::refresh_remote_cache_if_stale())
    }

    /// Find model info by exact ID or provider/model format (e.g. "anthropic/claude-sonnet-4-5" or "claude-sonnet-4-5").
    pub fn find_model(model_name: &str) -> Option<ModelInfo> {
        let (provider_hint, pure_id) = if let Some((p, m)) = model_name.split_once('/') {
            (Some(p), m)
        } else {
            (None, model_name)
        };

        let all = Self::list_all_models().ok()?;
        // Try exact match first
        for info in &all {
            if let Some(p) = provider_hint {
                if (info.provider.eq_ignore_ascii_case(p) || info.api.eq_ignore_ascii_case(p))
                    && info.id.eq_ignore_ascii_case(pure_id)
                {
                    return Some(info.clone());
                }
            } else if info.id.eq_ignore_ascii_case(pure_id) {
                return Some(info.clone());
            }
        }

        // Secondary fallback: match id ignoring case
        all.into_iter().find(|info| info.id.eq_ignore_ascii_case(pure_id))
    }

    /// Resolve usable context budget for a model.
    /// Deducts max_tokens or safety margin. Defaults to 100_000 if not found or unspecified.
    pub fn context_budget(model_name: &str) -> usize {
        const DEFAULT_BUDGET: usize = 100_000;
        let Some(info) = Self::find_model(model_name) else {
            return DEFAULT_BUDGET;
        };

        let context_window = info.context_window.unwrap_or(128_000) as usize;
        let max_output = info.max_tokens.unwrap_or(8192) as usize;
        let safety_margin = (context_window / 10).max(4096);

        context_window.saturating_sub(max_output + safety_margin).max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_catalog_loads_anthropic_models() {
        let models = ModelCatalog::anthropic_models().expect("Failed to parse anthropic models");
        assert!(!models.is_empty());
        assert!(models.contains_key("claude-sonnet-4-5") || models.contains_key("claude-opus-4-5"));
    }

    #[test]
    fn test_catalog_loads_openai_models() {
        let models = ModelCatalog::openai_models().expect("Failed to parse openai models");
        assert!(!models.is_empty());
    }

    #[test]
    fn test_catalog_loads_google_models() {
        let models = ModelCatalog::google_models().expect("Failed to parse google models");
        assert!(!models.is_empty());
    }

    #[test]
    fn test_list_all_builtin_models() {
        let all = ModelCatalog::list_all_builtin_models().expect("Failed to list all models");
        assert!(all.len() > 10);
    }

    #[test]
    fn test_overlapping_model_ids_survive_provider_filtering() {
        let all = ModelCatalog::list_all_builtin_models().unwrap();
        for provider in ["azure-openai-responses", "openai"] {
            let models: Vec<_> = all
                .iter()
                .filter(|model| model.provider == provider)
                .collect();
            assert_eq!(models.iter().filter(|model| model.id == "gpt-4").count(), 1);
        }
        let openai = ModelCatalog::openai_models().unwrap();
        let gpt4 = all
            .iter()
            .find(|model| model.provider == "openai" && model.id == "gpt-4")
            .unwrap();
        assert_eq!(
            serde_json::to_value(gpt4).unwrap(),
            serde_json::to_value(&openai["gpt-4"]).unwrap()
        );
    }

    #[test]
    fn test_all_builtin_catalog_entries_are_accessible_and_ordered() {
        let all = ModelCatalog::list_all_builtin_models().unwrap();
        let mut expected = HashMap::new();
        let mut catalog_count = 0;
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("catalog");
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            catalog_count += 1;
            let source = std::fs::read_to_string(path).unwrap();
            let catalogs: HashMap<String, HashMap<String, ModelInfo>> =
                serde_json::from_str(&source).unwrap();
            for models in catalogs.into_values() {
                for model in models.into_values() {
                    let key = (model.provider.clone(), model.id.clone());
                    assert!(
                        expected
                            .insert(key, serde_json::to_value(model).unwrap())
                            .is_none()
                    );
                }
            }
        }
        assert_eq!(catalog_count, 39);
        assert_eq!(expected.len(), 1376);
        assert_eq!(all.len(), expected.len());
        for model in &all {
            let key = (model.provider.clone(), model.id.clone());
            assert_eq!(
                expected.remove(&key),
                Some(serde_json::to_value(model).unwrap())
            );
        }
        assert!(expected.is_empty());
        assert!(
            all.windows(2).all(|pair| {
                (&pair[0].id, &pair[0].provider) < (&pair[1].id, &pair[1].provider)
            })
        );
    }

    #[test]
    fn test_catalog_parse_failures_are_reported() {
        for source in [
            "{",
            "[]",
            r#"{"api":[]}"#,
            r#"{"api":{"broken":{"id":"broken"}}}"#,
            r#"{"api":{"wrong":{"id":"model","name":"Model","provider":"provider"}}}"#,
            r#"{"api":{"model":{"id":"model","name":"Model","provider":"provider"}},"other":{"model":{"id":"model","name":"Model","provider":"provider"}}}"#,
        ] {
            assert!(ModelCatalog::parse_catalog(source).is_err(), "{source}");
        }
    }

    #[test]
    fn test_remote_cache_sanitization() {
        let payload = r#"[
            {
                "id": "gpt-6-luna",
                "name": "GPT-6 Luna",
                "provider": "openai",
                "baseUrl": "https://malicious-domain.com/evil",
                "api": "unsupported-protocol",
                "contextWindow": 1000000,
                "maxTokens": 128000
            }
        ]"#;
        let sanitized = ModelCatalog::parse_remote_cache(payload).unwrap();
        assert_eq!(sanitized.len(), 1);
        let m = &sanitized[0];
        assert_eq!(m.id, "gpt-6-luna");
        assert_eq!(m.provider, "openai");
        // Strict isolation: baseUrl and api cannot be hijacked by remote response
        assert_eq!(m.base_url.as_deref(), Some("https://api.openai.com/v1"));
        assert_eq!(m.api, "openai-responses");
        assert_eq!(m.context_window, Some(1000000));
    }

    #[test]
    fn test_models_dev_catalog_is_flattened() {
        let payload = r#"{
            "anthropic": {
                "id": "anthropic",
                "models": {
                    "claude-future": {
                        "id": "claude-future",
                        "name": "Claude Future",
                        "reasoning": true,
                        "tool_call": true,
                        "modalities": {"input": ["text", "image"]},
                        "limit": {"context": 200000, "output": 64000},
                        "cost": {"input": 3, "output": 15}
                    }
                }
            },
            "openrouter": {"models": {"unsupported": {"id": "unsupported", "tool_call": true}}}
        }"#;
        let models = ModelCatalog::parse_remote_cache(payload).unwrap();
        assert_eq!(models.len(), 1);
        let model = &models[0];
        assert_eq!(model.id, "claude-future");
        assert_eq!(model.provider, "anthropic");
        assert_eq!(model.api, "anthropic-messages");
        assert_eq!(model.base_url.as_deref(), Some("https://api.anthropic.com"));
        assert_eq!(model.input, ["text", "image"]);
        assert_eq!(model.context_window, Some(200000));
        assert_eq!(model.max_tokens, Some(64000));
        assert_eq!(model.cost.as_ref().and_then(|cost| cost.output), Some(15.0));
    }

    #[test]
    fn test_codex_models_response_uses_visible_slugs() {
        let payload = r#"{
            "models": [
                {
                    "slug": "gpt-future-codex",
                    "display_name": "GPT Future Codex",
                    "visibility": "list",
                    "context_window": 272000,
                    "input_modalities": ["text", "image"],
                    "supported_reasoning_levels": [{"effort": "low"}, {"effort": "high"}]
                },
                {"slug": "hidden-internal", "display_name": "Hidden", "visibility": "hide"}
            ]
        }"#;
        let models = ModelCatalog::parse_codex_models(payload).unwrap();
        assert_eq!(models.len(), 1);
        let model = &models[0];
        assert_eq!(model.id, "gpt-future-codex");
        assert_eq!(model.provider, "openai-codex");
        assert_eq!(model.api, "openai-codex-responses");
        assert_eq!(model.base_url.as_deref(), Some("https://chatgpt.com/backend-api"));
        assert_eq!(model.context_window, Some(272000));
        assert_eq!(model.input, ["text", "image"]);
        assert!(model.reasoning);
    }

    #[tokio::test]
    async fn test_codex_catalog_sync_is_authenticated_and_account_scoped() {
        use std::io::{Read, Write};

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("agent");
        std::fs::create_dir_all(&root).unwrap();
        let store = CredentialStore::new(root.join("auth.json"));
        let mut credential = Credential {
            cred_type: "oauth".to_string(),
            key: None,
            access_token: Some("test-access".to_string()),
            refresh_token: Some("test-refresh".to_string()),
            expires_at: Some(i64::MAX),
            account_id: Some("account-a".to_string()),
        };
        store.save("openai-codex", &credential).unwrap();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/models", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = [0u8; 4096];
            let count = stream.read(&mut bytes).unwrap();
            let request = String::from_utf8_lossy(&bytes[..count]).to_ascii_lowercase();
            assert!(request.starts_with("get /models?client_version=999.0.0 http/1.1"));
            assert!(request.contains("authorization: bearer test-access"));
            assert!(request.contains("chatgpt-account-id: account-a"));
            let body = r#"{"models":[{"slug":"gpt-new-codex","display_name":"GPT New Codex","visibility":"list"}]}"#;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });

        assert_eq!(ModelCatalog::sync_codex_cache_with(&root, &credential, &url).await.unwrap(), 1);
        server.join().unwrap();
        assert!(ModelCatalog::list_all_models_with_root(&root).unwrap()
            .iter().any(|model| model.provider == "openai-codex" && model.id == "gpt-new-codex"));

        credential.account_id = Some("account-b".to_string());
        store.save("openai-codex", &credential).unwrap();
        assert!(!ModelCatalog::list_all_models_with_root(&root).unwrap()
            .iter().any(|model| model.provider == "openai-codex" && model.id == "gpt-new-codex"));
    }

    #[test]
    fn test_user_models_url_validation() {
        let payload = r#"[
            {
                "id": "local-llama",
                "name": "Local Llama",
                "provider": "openai",
                "baseUrl": "javascript:alert(1)"
            },
            {
                "id": "valid-local",
                "name": "Valid Local",
                "provider": "openai",
                "baseUrl": "http://localhost:8080/v1"
            }
        ]"#;
        let models = ModelCatalog::parse_user_models(payload).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "valid-local");
    }

    #[test]
    fn test_three_tier_precedence() {
        let temp_dir = tempfile::tempdir().unwrap();
        let agent_dir = temp_dir.path().join(".dume/agent");
        std::fs::create_dir_all(&agent_dir).unwrap();

        // 1. Write Tier 2 cache with gpt-4 (override) and gpt-6-luna (new)
        let cache_content = r#"[
            {
                "id": "gpt-4",
                "name": "GPT-4 Remote Override",
                "provider": "openai"
            },
            {
                "id": "gpt-6-luna",
                "name": "GPT-6 Luna Remote",
                "provider": "openai"
            }
        ]"#;
        std::fs::write(agent_dir.join("models-cache.json"), cache_content).unwrap();

        // 2. Write Tier 3 user model overriding gpt-4
        let user_content = r#"[
            {
                "id": "gpt-4",
                "name": "GPT-4 User Override",
                "provider": "openai",
                "baseUrl": "http://localhost:11434/v1"
            }
        ]"#;
        std::fs::write(agent_dir.join("models.json"), user_content).unwrap();

        let all = ModelCatalog::list_all_models_with_root(&agent_dir).unwrap();

        // gpt-4 should come from Tier 3 User Config
        let gpt4 = all.iter().find(|m| m.provider == "openai" && m.id == "gpt-4").unwrap();
        assert_eq!(gpt4.name, "GPT-4 User Override");
        assert_eq!(gpt4.base_url.as_deref(), Some("http://localhost:11434/v1"));

        // gpt-6-luna should come from Tier 2 Remote Cache
        let gpt6 = all.iter().find(|m| m.provider == "openai" && m.id == "gpt-6-luna").unwrap();
        assert_eq!(gpt6.name, "GPT-6 Luna Remote");
    }
}

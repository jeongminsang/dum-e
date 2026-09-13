use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub provider: String,
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
}

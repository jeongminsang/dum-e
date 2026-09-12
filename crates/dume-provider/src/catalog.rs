use anyhow::Result;
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
        let mut all = Vec::new();
        if let Ok(m) = Self::anthropic_models() {
            all.extend(m.into_values());
        }
        if let Ok(m) = Self::openai_models() {
            all.extend(m.into_values());
        }
        if let Ok(m) = Self::google_models() {
            all.extend(m.into_values());
        }
        all.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(all)
    }

    fn parse_catalog(json_str: &str) -> Result<HashMap<String, ModelInfo>> {
        let val: serde_json::Value = serde_json::from_str(json_str)?;
        let mut map = HashMap::new();

        if let Some(obj) = val.as_object() {
            for (_api_type, models_val) in obj {
                if let Some(models_obj) = models_val.as_object() {
                    for (id, model_val) in models_obj {
                        if let Ok(info) = serde_json::from_value::<ModelInfo>(model_val.clone()) {
                            map.insert(id.clone(), info);
                        }
                    }
                }
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
}

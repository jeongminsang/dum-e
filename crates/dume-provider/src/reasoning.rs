use crate::catalog::{ModelCatalog, ModelInfo};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    #[default]
    Default,
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ReasoningEffort {
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "DEFAULT",
            Self::None => "OFF",
            Self::Minimal => "MIN",
            Self::Low => "LOW",
            Self::Medium => "MED",
            Self::High => "HIGH",
            Self::Xhigh => "XHIGH",
            Self::Max => "MAX",
        }
    }

    pub fn is_default(&self) -> bool {
        *self == Self::Default
    }
}

impl ModelInfo {
    pub fn qualified_id(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }

    pub fn supported_efforts(&self) -> Vec<ReasoningEffort> {
        let mut supported = vec![ReasoningEffort::Default];
        for level in self.reasoning_levels.iter().flatten() {
            if !supported.contains(level) {
                supported.push(*level);
            }
        }
        supported
    }

    pub fn validate_effort(&self, effort: ReasoningEffort) -> Result<()> {
        anyhow::ensure!(
            self.supported_efforts().contains(&effort),
            "{} does not support thinking {}",
            self.qualified_id(),
            effort.label()
        );
        Ok(())
    }
}

pub type ReasoningPreferences = BTreeMap<String, ReasoningEffort>;

pub fn load_preferences(path: &Path) -> Result<ReasoningPreferences> {
    let content = match std::fs::read(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error.into()),
    };
    let value: Value = serde_json::from_slice(&content).context("Invalid model settings JSON")?;
    value.get("thinking_by_model").map_or_else(
        || Ok(BTreeMap::new()),
        |value| serde_json::from_value(value.clone()).context("Invalid thinking settings"),
    )
}

pub fn preferences_path() -> std::path::PathBuf {
    ModelCatalog::models_dir().join("models-store.json")
}

pub fn saved_effort(model: &ModelInfo) -> Result<ReasoningEffort> {
    let effort = load_preferences(&preferences_path())?
        .get(&model.qualified_id())
        .copied()
        .unwrap_or_default();
    Ok(if model.supported_efforts().contains(&effort) {
        effort
    } else {
        ReasoningEffort::Default
    })
}

pub fn save_effort(path: &Path, model: &ModelInfo, effort: ReasoningEffort) -> Result<()> {
    model.validate_effort(effort)?;
    update_settings(path, |object| {
        let levels = object
            .entry("thinking_by_model")
            .or_insert_with(|| json!({}));
        levels
            .as_object_mut()
            .context("Thinking settings must be an object")?
            .insert(model.qualified_id(), json!(effort));
        Ok(())
    })
}

pub fn update_settings(
    path: &Path,
    update: impl FnOnce(&mut serde_json::Map<String, Value>) -> Result<()>,
) -> Result<()> {
    let directory = path
        .parent()
        .context("Model settings require a directory")?;
    std::fs::create_dir_all(directory)?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))?;
    lock.lock()?;
    let mut value: Value = match std::fs::read(path) {
        Ok(content) => serde_json::from_slice(&content).context("Invalid model settings JSON")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error.into()),
    };
    let object = value
        .as_object_mut()
        .context("Model settings must be an object")?;
    update(object)?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer_pretty(&mut file, &value)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferences_are_provider_scoped_preserve_other_settings_and_reject_invalid_levels() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("models-store.json");
        std::fs::write(&path, r#"{"last_used_model":"keep","other":42}"#).unwrap();
        let mut model = crate::resolve_model("openai-codex/gpt-5.4").unwrap();
        model.id = "gpt-6-astra".into();
        model.reasoning_levels = Some(vec![ReasoningEffort::Low, ReasoningEffort::High]);
        save_effort(&path, &model, ReasoningEffort::High).unwrap();
        model.provider = "opencodex".into();
        save_effort(&path, &model, ReasoningEffort::Low).unwrap();
        let prefs = load_preferences(&path).unwrap();
        assert_eq!(prefs["openai-codex/gpt-6-astra"], ReasoningEffort::High);
        assert_eq!(prefs["opencodex/gpt-6-astra"], ReasoningEffort::Low);
        assert!(save_effort(&path, &model, ReasoningEffort::Medium).is_err());
        assert!(save_effort(&path, &model, ReasoningEffort::None).is_err());
        let value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["last_used_model"], "keep");
        assert_eq!(value["other"], 42);
        model.reasoning_levels = None;
        assert_eq!(model.supported_efforts(), [ReasoningEffort::Default]);
        model.reasoning_levels = Some(vec![
            ReasoningEffort::None,
            ReasoningEffort::Xhigh,
            ReasoningEffort::None,
        ]);
        assert_eq!(
            model.supported_efforts(),
            [
                ReasoningEffort::Default,
                ReasoningEffort::None,
                ReasoningEffort::Xhigh
            ]
        );
    }
}

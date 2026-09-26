use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Default, Deserialize, Serialize)]
pub struct SubagentSettings {
    pub model: Option<String>,
}

impl SubagentSettings {
    pub fn path() -> Result<PathBuf> {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home).join(".dume/agent/subagents.json"))
    }

    pub fn load() -> Result<Self> {
        match Self::path() {
            Ok(path) => Self::load_from(&path),
            Err(_) => Ok(Self::default()),
        }
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("Invalid subagent settings at {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("Cannot read {}", path.display())),
        }
    }

    pub fn model_for(&self, parent_model: &str) -> Result<String> {
        match &self.model {
            Some(selection) => {
                let model = dume_provider::resolve_model(selection)?;
                Ok(format!("{}/{}", model.provider, model.id))
            }
            None => Ok(parent_model.to_string()),
        }
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .context("Subagent settings path has no parent")?;
        std::fs::create_dir_all(parent)?;
        let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
        std::fs::write(&temporary, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&temporary, path)
            .with_context(|| format!("Cannot save {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_inherit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        assert!(SubagentSettings::load_from(&path).unwrap().model.is_none());

        SubagentSettings {
            model: Some("openai/gpt-5.4".into()),
        }
        .save_to(&path)
        .unwrap();
        assert_eq!(
            SubagentSettings::load_from(&path).unwrap().model.as_deref(),
            Some("openai/gpt-5.4")
        );
        assert_eq!(
            SubagentSettings::load_from(&path)
                .unwrap()
                .model_for("parent/model")
                .unwrap(),
            "openai/gpt-5.4"
        );

        SubagentSettings::default().save_to(&path).unwrap();
        assert_eq!(
            SubagentSettings::load_from(&path)
                .unwrap()
                .model_for("parent/model")
                .unwrap(),
            "parent/model"
        );
    }
}

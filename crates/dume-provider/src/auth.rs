use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credential {
    #[serde(rename = "type")]
    pub cred_type: String,
    pub key: Option<String>,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct CredentialStore {
    file_path: PathBuf,
}

impl CredentialStore {
    pub fn new<P: AsRef<Path>>(file_path: P) -> Self {
        Self {
            file_path: file_path.as_ref().to_path_buf(),
        }
    }

    pub fn default_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".dume/agent/auth.json")
    }

    pub fn get_api_key(&self, provider: &str) -> Option<String> {
        // 1. Check environment variables first
        let env_var = match provider {
            "anthropic" => "ANTHROPIC_API_KEY",
            "openai" => "OPENAI_API_KEY",
            "google" | "gemini" => "GEMINI_API_KEY",
            _ => "",
        };

        if !env_var.is_empty() {
            if let Ok(val) = std::env::var(env_var) {
                if !val.trim().is_empty() {
                    return Some(val);
                }
            }
        }

        // 2. Fall back to credentials file
        if let Ok(creds) = self.load() {
            if let Some(cred) = creds.get(provider) {
                if let Some(ref k) = cred.key {
                    return Some(k.clone());
                }
                if let Some(ref tok) = cred.access_token {
                    return Some(tok.clone());
                }
            }
        }

        None
    }

    pub fn save(&self, provider: &str, cred: &Credential) -> Result<()> {
        let mut creds = self.load().unwrap_or_default();
        creds.insert(provider.to_string(), cred.clone());

        if let Some(parent) = self.file_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let serialized = serde_json::to_string_pretty(&creds)?;
        fs::write(&self.file_path, serialized)?;
        Ok(())
    }

    pub fn delete(&self, provider: &str) -> Result<()> {
        let mut creds = self.load().unwrap_or_default();
        creds.remove(provider);

        if let Some(parent) = self.file_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let serialized = serde_json::to_string_pretty(&creds)?;
        fs::write(&self.file_path, serialized)?;
        Ok(())
    }

    pub fn save_credential(&self, provider: &str, key: &str) -> Result<()> {
        self.save(
            provider,
            &Credential {
                cred_type: "api_key".to_string(),
                key: Some(key.to_string()),
                access_token: None,
                refresh_token: None,
                expires_at: None,
            },
        )
    }

    fn load(&self) -> Result<HashMap<String, Credential>> {
        if !self.file_path.exists() {
            return Ok(HashMap::new());
        }
        let content = fs::read_to_string(&self.file_path)?;
        let map = serde_json::from_str(&content)?;
        Ok(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_credential_save_and_retrieve() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let store = CredentialStore::new(&path);

        store.save_credential("anthropic", "sk-ant-test").unwrap();
        assert_eq!(store.get_api_key("anthropic"), Some("sk-ant-test".to_string()));
    }
}

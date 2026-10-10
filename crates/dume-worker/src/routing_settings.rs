use anyhow::{Result, anyhow};
use dume_provider::ClassifierConfig;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingSettings {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub classifier: Option<ClassifierConfig>,
    #[serde(default)]
    pub models: RoutingModels,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoutingModels {
    pub standard: Option<String>,
    pub complex: Option<String>,
    pub implementation: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoutingRole {
    StandardPlanning,
    ComplexPlanning,
    Implementation,
}

impl Default for RoutingSettings {
    fn default() -> Self {
        Self {
            version: default_version(),
            enabled: false,
            classifier: None,
            models: RoutingModels::default(),
        }
    }
}

fn default_version() -> u32 {
    1
}

#[cfg(unix)]
fn create_private_dirs(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_dirs(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

fn private_file_options() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

impl RoutingSettings {
    pub fn global_path() -> Result<PathBuf> {
        let home = std::env::var_os("HOME")
            .ok_or_else(|| anyhow!("HOME 환경 변수를 확인할 수 없습니다"))?;
        routing_path_from_home(&PathBuf::from(home))
    }

    pub fn load(path: &Path) -> Result<Self> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(_) => return Err(anyhow!("라우팅 설정을 읽을 수 없습니다")),
        };
        let settings: Self = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow!("라우팅 설정 형식이 올바르지 않습니다"))?;
        settings
            .validate()
            .map_err(|_| anyhow!("라우팅 설정 값이 올바르지 않습니다"))?;
        Ok(settings)
    }

    pub fn load_default() -> Result<Self> {
        Self::load(&Self::global_path()?)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()
            .map_err(|_| anyhow!("라우팅 설정 값이 올바르지 않습니다"))?;
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("라우팅 설정 경로를 확인할 수 없습니다"))?;
        create_private_dirs(parent)
            .map_err(|_| anyhow!("라우팅 설정 디렉터리를 만들 수 없습니다"))?;
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|_| anyhow!("라우팅 설정을 직렬화할 수 없습니다"))?;
        static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
        let mut temporary = None;
        for _ in 0..32 {
            let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let candidate = path.with_extension(format!("tmp-{}-{id}", std::process::id()));
            match private_file_options().open(&candidate) {
                Ok(mut file) => {
                    use std::io::Write;
                    if file
                        .write_all(&bytes)
                        .and_then(|_| file.sync_all())
                        .is_err()
                    {
                        drop(file);
                        let _ = std::fs::remove_file(&candidate);
                        return Err(anyhow!("라우팅 설정 파일을 쓸 수 없습니다"));
                    }
                    temporary = Some(candidate);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => return Err(anyhow!("임시 라우팅 설정 파일을 만들 수 없습니다")),
            }
        }
        let temporary =
            temporary.ok_or_else(|| anyhow!("임시 라우팅 설정 파일을 확보할 수 없습니다"))?;
        if std::fs::rename(&temporary, path).is_err() {
            let _ = std::fs::remove_file(&temporary);
            return Err(anyhow!("라우팅 설정을 저장할 수 없습니다"));
        }
        sync_directory(parent)
            .map_err(|_| anyhow!("라우팅 설정 디렉터리를 동기화할 수 없습니다"))?;
        Ok(())
    }

    pub fn save_default(&self) -> Result<()> {
        self.save(&Self::global_path()?)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            return Err(anyhow!("지원하지 않는 라우팅 설정 버전입니다"));
        }
        match (self.enabled, self.classifier.as_ref()) {
            (true, None) => return Err(anyhow!("라우팅을 사용하려면 분류기 설정이 필요합니다")),
            (true, Some(classifier)) => classifier
                .validate()
                .map_err(|_| anyhow!("분류기 설정 값이 올바르지 않습니다"))?,
            (false, Some(classifier))
                if !classifier.base_url.trim().is_empty()
                    || !classifier.model.trim().is_empty() =>
            {
                classifier
                    .validate()
                    .map_err(|_| anyhow!("분류기 설정 값이 올바르지 않습니다"))?;
            }
            (false, _) => {}
        }
        Ok(())
    }

    pub fn fingerprint(&self, original_model: &str) -> Result<String> {
        self.validate()
            .map_err(|_| anyhow!("라우팅 설정 값이 올바르지 않습니다"))?;
        let config =
            serde_json::to_vec(self).map_err(|_| anyhow!("라우팅 설정을 직렬화할 수 없습니다"))?;
        let mut hasher = Sha256::new();
        hasher.update(b"dume-routing-settings-v1\0");
        hasher.update((config.len() as u64).to_be_bytes());
        hasher.update(config);
        hasher.update((original_model.len() as u64).to_be_bytes());
        hasher.update(original_model.as_bytes());
        Ok(hex::encode(hasher.finalize()))
    }

    pub fn model_for<'a>(&'a self, role: RoutingRole, original_model: &'a str) -> &'a str {
        let selected = match role {
            RoutingRole::StandardPlanning => self.models.standard.as_deref(),
            RoutingRole::ComplexPlanning => self.models.complex.as_deref(),
            RoutingRole::Implementation => self.models.implementation.as_deref(),
        };
        selected
            .filter(|model| !model.trim().is_empty())
            .unwrap_or(original_model)
    }

    pub fn all_roles_match_original(&self, original_model: &str) -> bool {
        [
            RoutingRole::StandardPlanning,
            RoutingRole::ComplexPlanning,
            RoutingRole::Implementation,
        ]
        .iter()
        .all(|role| self.model_for(*role, original_model) == original_model)
    }
}

fn routing_path_from_home(home: &Path) -> Result<PathBuf> {
    if !home.is_absolute() {
        return Err(anyhow!("HOME 환경 변수는 절대 경로여야 합니다"));
    }
    Ok(home.join(".dume/agent/routing.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_home_cannot_enable_project_routing_settings() {
        for home in ["", ".", "relative-home"] {
            assert!(routing_path_from_home(Path::new(home)).is_err());
        }
        let home = tempfile::tempdir().unwrap();
        assert_eq!(
            routing_path_from_home(home.path()).unwrap(),
            home.path().join(".dume/agent/routing.json")
        );
    }

    #[test]
    fn actual_file_round_trip_and_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("routing.json");
        assert!(!RoutingSettings::load(&path).unwrap().enabled);
        let settings = RoutingSettings {
            models: RoutingModels {
                standard: Some("alpha".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        settings.save(&path).unwrap();
        assert_eq!(RoutingSettings::load(&path).unwrap(), settings);
    }

    #[test]
    fn malformed_and_unknown_secret_fields_are_rejected_without_echo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("routing.json");
        for content in ["{", r#"{"api_key":"secret"}"#] {
            std::fs::write(&path, content).unwrap();
            let error = RoutingSettings::load(&path).unwrap_err().to_string();
            assert!(!error.contains("secret"));
            assert!(!error.contains("api_key"));
        }
    }

    #[test]
    fn validates_enabled_classifier_and_version() {
        let mut settings = RoutingSettings {
            enabled: true,
            ..Default::default()
        };
        assert!(settings.validate().is_err());
        settings.enabled = false;
        settings.version = 9;
        assert!(settings.validate().is_err());
    }

    #[test]
    fn inactive_classifier_must_be_complete_and_secret_free() {
        let safe = ClassifierConfig {
            backend: dume_provider::ClassifierBackend::OpenCodeZen,
            base_url: "https://classifier.example/v1".into(),
            model: "classify-v1".into(),
        };
        let complete = RoutingSettings {
            classifier: Some(safe),
            ..Default::default()
        };
        assert!(complete.validate().is_ok());
        assert!(
            RoutingSettings {
                classifier: Some(ClassifierConfig {
                    backend: dume_provider::ClassifierBackend::OpenCodeZen,
                    base_url: " ".into(),
                    model: "\t".into()
                }),
                ..Default::default()
            }
            .validate()
            .is_ok()
        );

        for url in [
            "https://user:fake-secret@classifier.example/v1",
            "https://classifier.example/v1?api_key=fake-secret",
            "https://classifier.example/v1#fake-secret",
        ] {
            let settings = RoutingSettings {
                classifier: Some(ClassifierConfig {
                    backend: dume_provider::ClassifierBackend::OpenCodeZen,
                    base_url: url.into(),
                    model: "classify-v1".into(),
                }),
                ..Default::default()
            };
            let error = settings.validate().unwrap_err().to_string();
            assert_eq!(error, "분류기 설정 값이 올바르지 않습니다");
            assert!(!error.contains(url));
        }
        let invalid = RoutingSettings {
            classifier: Some(ClassifierConfig {
                backend: dume_provider::ClassifierBackend::OpenCodeZen,
                base_url: "not a URL fake-secret".into(),
                model: "classify-v1".into(),
            }),
            ..Default::default()
        };
        let error = invalid.validate().unwrap_err().to_string();
        assert!(!error.contains("not a URL"));
        assert!(!error.contains("fake-secret"));
    }

    #[test]
    fn failed_save_preserves_existing_file_and_cleans_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("settings-dir");
        std::fs::create_dir(&target).unwrap();
        let marker = target.join("keep");
        std::fs::write(&marker, "original").unwrap();
        assert!(RoutingSettings::default().save(&target).is_err());
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "original");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn fingerprint_is_stable_and_changes_with_public_configuration_or_baseline() {
        let settings = RoutingSettings::default();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("routing.json");
        settings.save(&path).unwrap();
        let reloaded = RoutingSettings::load(&path).unwrap();
        assert_eq!(
            settings.fingerprint("base").unwrap(),
            reloaded.fingerprint("base").unwrap()
        );
        let reordered = r#"{"models":{"implementation":null,"complex":null,"standard":null},"classifier":null,"enabled":false,"version":1}"#;
        let reordered: RoutingSettings = serde_json::from_str(reordered).unwrap();
        assert_eq!(
            settings.fingerprint("base").unwrap(),
            reordered.fingerprint("base").unwrap()
        );
        assert_ne!(
            settings.fingerprint("base").unwrap(),
            settings.fingerprint("other").unwrap()
        );
        let changed = RoutingSettings {
            models: RoutingModels {
                standard: Some("different".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_ne!(
            settings.fingerprint("base").unwrap(),
            changed.fingerprint("base").unwrap()
        );
    }

    #[test]
    fn roles_inherit_blank_values_and_distinguish_other_models() {
        let settings = RoutingSettings {
            models: RoutingModels {
                standard: Some("".into()),
                complex: Some("different".into()),
                implementation: Some("third".into()),
            },
            ..Default::default()
        };
        assert_eq!(
            settings.model_for(RoutingRole::StandardPlanning, "base"),
            "base"
        );
        assert!(!settings.all_roles_match_original("base"));
        let same_other = RoutingSettings {
            models: RoutingModels {
                standard: Some("other".into()),
                complex: Some("other".into()),
                implementation: Some("other".into()),
            },
            ..Default::default()
        };
        assert!(!same_other.all_roles_match_original("base"));
        assert!(RoutingSettings::default().all_roles_match_original("base"));
    }

    #[cfg(unix)]
    #[test]
    fn new_file_and_directories_have_restricted_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new-parent/child/routing.json");
        RoutingSettings::default().save(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap().parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn invalid_secret_field_is_not_saved_and_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("routing.json");
        std::fs::write(&path, r#"{"api_key":"original-secret"}"#).unwrap();
        let before = std::fs::read(&path).unwrap();
        let loaded = RoutingSettings::load(&path);
        assert!(loaded.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn invalid_inactive_classifier_save_preserves_existing_file_and_cleans_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("routing.json");
        let original = RoutingSettings::default();
        original.save(&path).unwrap();
        let before = std::fs::read(&path).unwrap();
        let unsafe_draft = RoutingSettings {
            classifier: Some(ClassifierConfig {
                backend: dume_provider::ClassifierBackend::OpenCodeZen,
                base_url: "https://user:fake-secret@classifier.example/v1?api_key=fake-secret"
                    .into(),
                model: "classify-v1".into(),
            }),
            ..Default::default()
        };
        let error = unsafe_draft.save(&path).unwrap_err().to_string();
        assert_eq!(error, "라우팅 설정 값이 올바르지 않습니다");
        assert!(!error.contains("fake-secret"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}

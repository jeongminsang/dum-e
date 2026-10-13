use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

static REFRESH_OWNERS: AtomicUsize = AtomicUsize::new(0);
static REFRESH_NOTIFY: OnceLock<Notify> = OnceLock::new();

/// Wait for all automatically-owned credential resolutions to finish, bounded by `timeout`.
/// This cannot protect refreshes from forced process termination or machine failure.
pub async fn drain_refresh_owners(timeout: Duration) -> Result<()> {
    let wait = async {
        let notify = REFRESH_NOTIFY.get_or_init(Notify::new);
        loop {
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if REFRESH_OWNERS.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    };
    tokio::time::timeout(timeout, wait)
        .await
        .map_err(|_| anyhow::anyhow!("Timed out waiting for credential refresh owners"))
}

struct RefreshOwner;
impl Drop for RefreshOwner {
    fn drop(&mut self) {
        REFRESH_OWNERS.fetch_sub(1, Ordering::AcqRel);
        if let Some(notify) = REFRESH_NOTIFY.get() {
            notify.notify_waiters();
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Credential {
    #[serde(rename = "type")]
    pub cred_type: String,
    pub key: Option<String>,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub account_id: Option<String>,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("cred_type", &self.cred_type)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

pub fn normalize_provider(provider: &str) -> &str {
    if provider == "gemini" {
        "google"
    } else {
        provider
    }
}

fn credential_env(provider: &str) -> &'static str {
    match normalize_provider(provider) {
        "anthropic" => "ANTHROPIC_API_KEY",
        "openai" => "OPENAI_API_KEY",
        "google" => "GEMINI_API_KEY",
        "deepseek" => "DEEPSEEK_API_KEY",
        "opencode" | "opencode-go" => "OPENCODE_API_KEY",
        "opencodex" => "OPENCODEX_API_KEY",
        _ => "",
    }
}

fn now_ms() -> Result<i64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}

impl Credential {
    pub fn from_oauth(
        provider: &str,
        token: crate::oauth::OAuthTokenResponse,
        previous: Option<&Credential>,
    ) -> Result<Self> {
        let account_id = if provider == "openai-codex" {
            Some(
                crate::oauth::extract_account_id(&token.access_token)
                    .or_else(|| previous.and_then(|c| c.account_id.clone()))
                    .context("Codex token missing account ID")?,
            )
        } else {
            None
        };
        let expires_in = token
            .expires_in
            .filter(|v| *v > 0)
            .context("OAuth token missing expiration")?;
        let expires_at = now_ms()?
            .checked_add(
                expires_in
                    .checked_mul(1000)
                    .context("Invalid token expiration")?,
            )
            .context("Invalid token expiration")?;
        anyhow::ensure!(
            !token.access_token.trim().is_empty(),
            "Empty OAuth access token"
        );
        let refresh_token = token
            .refresh_token
            .or_else(|| previous.and_then(|c| c.refresh_token.clone()));
        anyhow::ensure!(
            refresh_token.as_ref().is_some_and(|s| !s.trim().is_empty()),
            "Missing OAuth refresh token"
        );
        Ok(Self {
            cred_type: "oauth".into(),
            key: None,
            access_token: Some(token.access_token),
            refresh_token,
            expires_at: Some(expires_at),
            account_id,
        })
    }
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
        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
            .join(".dume/agent/auth.json")
    }

    fn parent(&self) -> &Path {
        self.file_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
    }

    fn ensure_parent_dir(&self) -> Result<()> {
        let parent = self.parent();
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(metadata) = fs::metadata(parent) {
                let mut permissions = metadata.permissions();
                if permissions.mode() & 0o077 != 0 {
                    permissions.set_mode(0o700);
                    let _ = fs::set_permissions(parent, permissions);
                }
            }
        }
        Ok(())
    }

    // Stable sidecar inode: never unlink it; replacing the credential file cannot invalidate this lock.
    fn open_lock(&self) -> Result<File> {
        self.ensure_parent_dir()?;
        let mut path = self.file_path.as_os_str().to_os_string();
        path.push(".lock");
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        Ok(options.open(PathBuf::from(path))?)
    }

    fn lock(&self) -> Result<File> {
        let file = self.open_lock()?;
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25))
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    anyhow::bail!("Credential store lock timed out")
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
            }
        }
    }

    pub async fn resolve_credential(&self, provider: &str) -> Result<Option<Credential>> {
        let provider = normalize_provider(provider);
        let env = credential_env(provider);
        if let Ok(key) = std::env::var(env) {
            if !key.trim().is_empty() {
                return Ok(Some(Self::api_key(&key)));
            }
        }
        let provider = provider.to_owned();
        self.spawn_resolve_owner(provider, None).await
    }

    async fn spawn_resolve_owner(
        &self,
        provider: String,
        token_url: Option<String>,
    ) -> Result<Option<Credential>> {
        let store = self.clone();
        let (send, receive) = tokio::sync::oneshot::channel();
        REFRESH_OWNERS.fetch_add(1, Ordering::AcqRel);
        tokio::spawn(async move {
            let _owner = RefreshOwner;
            let result = store.resolve_owned(&provider, token_url.as_deref()).await;
            if result.is_err() {
                tracing::warn!(provider = %provider, "Credential resolution/refresh failed");
            }
            let _ = send.send(result);
        });
        receive.await.context("Credential refresh owner stopped")?
    }

    async fn resolve_owned(
        &self,
        provider: &str,
        token_url: Option<&str>,
    ) -> Result<Option<Credential>> {
        let store = self.clone();
        let _lock = tokio::task::spawn_blocking(move || store.lock()).await??;
        let mut credentials = self.load()?;
        let Some(mut credential) = credentials.get(provider).cloned() else {
            return Ok(None);
        };
        match credential.cred_type.as_str() {
            "api_key" => {
                anyhow::ensure!(provider != "openai-codex", "Codex requires OAuth login");
                anyhow::ensure!(
                    credential
                        .key
                        .as_ref()
                        .is_some_and(|s| !s.trim().is_empty()),
                    "Stored API key is empty"
                );
            }
            "oauth" => {
                let config = crate::oauth::get_oauth_config(provider)
                    .context("Provider requires API-key login")?;
                let expires = credential
                    .expires_at
                    .context("Stored OAuth credential missing expiration; login again")?;
                if expires.saturating_sub(now_ms()?) <= 300_000 {
                    let refresh = credential
                        .refresh_token
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .context("Missing refresh token; login again")?;
                    let token = match token_url {
                        Some(url) => {
                            crate::oauth::refresh_oauth_token_at(&config, url, refresh).await?
                        }
                        None => crate::oauth::refresh_oauth_token(&config, refresh).await?,
                    };
                    credential = Credential::from_oauth(provider, token, Some(&credential))?;
                    credentials.insert(provider.to_owned(), credential.clone());
                    self.write_all(&credentials)?;
                }
                anyhow::ensure!(
                    credential
                        .access_token
                        .as_ref()
                        .is_some_and(|s| !s.trim().is_empty()),
                    "Stored OAuth access token is empty"
                );
                if provider == "openai-codex" && credential.account_id.is_none() {
                    credential.account_id = crate::oauth::extract_account_id(
                        credential.access_token.as_deref().unwrap(),
                    );
                    anyhow::ensure!(
                        credential.account_id.is_some(),
                        "Codex credential missing account ID; login again"
                    );
                    credentials.insert(provider.to_owned(), credential.clone());
                    self.write_all(&credentials)?;
                }
            }
            _ => anyhow::bail!("Unknown stored credential type"),
        }
        Ok(Some(credential))
    }

    pub fn save(&self, provider: &str, credential: &Credential) -> Result<()> {
        let _lock = self.lock()?;
        let mut credentials = self.load()?;
        credentials.insert(normalize_provider(provider).to_owned(), credential.clone());
        self.write_all(&credentials)
    }

    pub fn delete(&self, provider: &str) -> Result<()> {
        let _lock = self.lock()?;
        let mut credentials = self.load()?;
        credentials.remove(normalize_provider(provider));
        self.write_all(&credentials)
    }

    fn api_key(key: &str) -> Credential {
        Credential {
            cred_type: "api_key".into(),
            key: Some(key.to_owned()),
            access_token: None,
            refresh_token: None,
            expires_at: None,
            account_id: None,
        }
    }

    pub fn save_credential(&self, provider: &str, key: &str) -> Result<()> {
        anyhow::ensure!(!key.trim().is_empty(), "API key cannot be empty");
        anyhow::ensure!(provider != "openai-codex", "Codex requires OAuth login");
        self.save(provider, &Self::api_key(key))
    }

    fn write_all(&self, credentials: &HashMap<String, Credential>) -> Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(self.parent())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(&serde_json::to_vec_pretty(credentials)?)?;
        file.as_file().sync_all()?;
        file.persist(&self.file_path)
            .map_err(|_| anyhow::anyhow!("Unable to persist credential store"))?;
        Ok(())
    }

    pub fn load(&self) -> Result<HashMap<String, Credential>> {
        let content = match fs::read(&self.file_path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
            Err(e) => return Err(e.into()),
        };
        serde_json::from_slice(&content).map_err(|_| anyhow::anyhow!("Malformed credential store"))
    }

    pub fn has_credential(&self, provider: &str) -> bool {
        let provider = normalize_provider(provider);
        let env = credential_env(provider);
        if let Ok(key) = std::env::var(env) {
            if !key.trim().is_empty() {
                return true;
            }
        }
        if let Ok(map) = self.load() {
            if let Some(cred) = map.get(provider) {
                match cred.cred_type.as_str() {
                    "api_key" => {
                        return cred.key.as_ref().is_some_and(|k| !k.trim().is_empty());
                    }
                    "oauth" => {
                        return cred
                            .access_token
                            .as_ref()
                            .is_some_and(|t| !t.trim().is_empty());
                    }
                    _ => {}
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifier_env_mapping_does_not_alias_local_backend() {
        assert_eq!(credential_env("opencode"), "OPENCODE_API_KEY");
        assert_eq!(credential_env("jev-local-system-one"), "");
    }

    #[tokio::test]
    async fn refresh_owner_drain_is_bounded_and_completes() {
        REFRESH_OWNERS.fetch_add(1, Ordering::AcqRel);
        let owner = RefreshOwner;
        assert!(
            drain_refresh_owners(Duration::from_millis(1))
                .await
                .is_err()
        );
        drop(owner);
        drain_refresh_owners(Duration::from_secs(1)).await.unwrap();
    }

    async fn mock_refresh_server(
        status: &str,
        body: &'static str,
        delay: bool,
    ) -> (
        String,
        tokio::sync::oneshot::Receiver<String>,
        tokio::task::JoinHandle<()>,
        std::sync::Arc<tokio::sync::Notify>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let release_server = release.clone();
        let status = status.to_owned();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                bytes.push(stream.read_u8().await.unwrap());
            }
            let headers = String::from_utf8(bytes.clone()).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            let mut body_bytes = vec![0; length];
            stream.read_exact(&mut body_bytes).await.unwrap();
            let request = String::from_utf8(body_bytes).unwrap();
            let _ = seen_tx.send(request);
            if delay {
                release_server.notified().await;
            }
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        (url, seen_rx, task, release)
    }

    fn expired_oauth() -> Credential {
        Credential {
            cred_type: "oauth".into(),
            key: None,
            access_token: Some("old-access".into()),
            refresh_token: Some("old-refresh".into()),
            expires_at: Some(1),
            account_id: None,
        }
    }

    #[tokio::test]
    async fn caller_cancellation_does_not_cancel_rotating_refresh_owner() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(dir.path().join("auth.json"));
        store.save("anthropic", &expired_oauth()).unwrap();
        let (url, seen, server, release) = mock_refresh_server(
            "200 OK",
            r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#,
            true,
        )
        .await;
        let task_store = store.clone();
        let caller = tokio::spawn(async move {
            task_store
                .spawn_resolve_owner("anthropic".into(), Some(url))
                .await
        });
        let request = tokio::time::timeout(Duration::from_secs(2), seen)
            .await
            .unwrap()
            .unwrap();
        assert!(request.contains("old-refresh"));
        caller.abort();
        release.notify_one();
        server.await.unwrap();
        drain_refresh_owners(Duration::from_secs(2)).await.unwrap();
        let saved = store.load().unwrap();
        assert_eq!(
            saved["anthropic"].access_token.as_deref(),
            Some("new-access")
        );
        assert_eq!(
            saved["anthropic"].refresh_token.as_deref(),
            Some("new-refresh")
        );
    }

    #[tokio::test]
    async fn concurrent_resolves_dedupe_and_rejected_refresh_preserves_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(dir.path().join("auth.json"));
        store.save("anthropic", &expired_oauth()).unwrap();
        let (url, seen, server, release) = mock_refresh_server(
            "200 OK",
            r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#,
            true,
        )
        .await;
        let first_store = store.clone();
        let first_url = url.clone();
        let first = tokio::spawn(async move {
            first_store
                .spawn_resolve_owner("anthropic".into(), Some(first_url))
                .await
        });
        assert!(seen.await.unwrap().contains("old-refresh"));
        let second_store = store.clone();
        let second = tokio::spawn(async move {
            second_store
                .spawn_resolve_owner("anthropic".into(), Some(url))
                .await
        });
        release.notify_one();
        server.await.unwrap();
        assert_eq!(
            first
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .access_token
                .as_deref(),
            Some("new-access")
        );
        assert_eq!(
            second
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .access_token
                .as_deref(),
            Some("new-access")
        );
        drain_refresh_owners(Duration::from_secs(2)).await.unwrap();

        store.save("anthropic", &expired_oauth()).unwrap();
        let (url, _, server, _) =
            mock_refresh_server("400 Bad Request", r#"{"error":"invalid_grant"}"#, false).await;
        assert!(
            store
                .spawn_resolve_owner("anthropic".into(), Some(url))
                .await
                .is_err()
        );
        server.await.unwrap();
        let preserved = store.load().unwrap();
        assert_eq!(
            preserved["anthropic"].access_token.as_deref(),
            Some("old-access")
        );
        assert_eq!(
            preserved["anthropic"].refresh_token.as_deref(),
            Some("old-refresh")
        );
    }

    #[test]
    fn updates_preserve_other_credentials_and_normalize_alias() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(dir.path().join("auth.json"));
        store.save_credential("gemini", "google-key").unwrap();
        store.save_credential("openai", "openai-key").unwrap();
        store.delete("gemini").unwrap();
        let map = store.load().unwrap();
        assert!(!map.contains_key("google"));
        assert_eq!(map["openai"].key.as_deref(), Some("openai-key"));
    }

    #[test]
    fn malformed_store_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        fs::write(&path, "bad secret content").unwrap();
        let store = CredentialStore::new(&path);
        assert!(store.save_credential("openai", "key").is_err());
        assert!(store.delete("openai").is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "bad secret content");
    }

    #[test]
    fn concurrent_updates_are_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(dir.path().join("auth.json"));
        let workers: Vec<_> = (0..12)
            .map(|i| {
                let store = store.clone();
                std::thread::spawn(move || {
                    store
                        .save_credential(&format!("provider-{i}"), "key")
                        .unwrap()
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(store.load().unwrap().len(), 12);
    }

    #[tokio::test]
    async fn expired_credentials_without_refresh_fail() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(dir.path().join("auth.json"));
        let mut credential = SelfTestCredential::expired();
        store.save("openai-codex", &credential).unwrap();
        assert!(store.resolve_credential("openai-codex").await.is_err());
        credential.expires_at = None;
        store.save("openai-codex", &credential).unwrap();
        assert!(store.resolve_credential("openai-codex").await.is_err());
    }

    struct SelfTestCredential;
    impl SelfTestCredential {
        fn expired() -> Credential {
            Credential {
                cred_type: "oauth".into(),
                key: None,
                access_token: Some("expired".into()),
                refresh_token: None,
                expires_at: Some(1),
                account_id: Some("account".into()),
            }
        }
    }

    #[test]
    fn rotation_preserves_refresh_and_account_metadata() {
        let mut old = SelfTestCredential::expired();
        old.refresh_token = Some("refresh".into());
        let token = crate::oauth::OAuthTokenResponse {
            access_token: "new".into(),
            refresh_token: None,
            expires_in: Some(3600),
            token_type: None,
            scope: None,
        };
        let new = Credential::from_oauth("openai-codex", token, Some(&old)).unwrap();
        assert_eq!(new.refresh_token, old.refresh_token);
        assert_eq!(new.account_id, old.account_id);
        assert!(!format!("{new:?}").contains("refresh"));
    }

    #[cfg(unix)]
    #[test]
    fn test_credential_store_file_and_dir_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("secure_sub");
        let path = sub.join("auth.json");
        let store = CredentialStore::new(&path);

        store.save_credential("anthropic", "sk-ant-test").unwrap();

        let dir_perm = fs::metadata(&sub).unwrap().permissions().mode();
        assert_eq!(
            dir_perm & 0o077,
            0,
            "Directory permissions should not be world or group accessible: {:o}",
            dir_perm
        );

        let file_perm = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            file_perm & 0o077,
            0,
            "File permissions should not be world or group accessible: {:o}",
            file_perm
        );
    }
}

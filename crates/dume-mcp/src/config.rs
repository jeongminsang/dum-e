use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ToolExposure {
    #[default]
    Direct,
    Deferred,
    Hidden,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub exposure: ToolExposure,
    #[serde(default)]
    pub tool_overrides: BTreeMap<String, ToolExposure>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "default_output_limit")]
    pub output_limit_bytes: usize,
}
fn default_true() -> bool {
    true
}
fn default_timeout() -> u64 {
    30_000
}
fn default_output_limit() -> usize {
    1_048_576
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: BTreeMap<String, McpServerConfig>,
}

impl McpConfig {
    pub fn read(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        serde_json::from_slice(
            &std::fs::read(path).with_context(|| format!("read {}", path.display()))?,
        )
        .with_context(|| format!("parse {}", path.display()))
    }

    pub fn global_path() -> Result<PathBuf> {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home).join(".dume/agent/mcp.json"))
    }

    pub fn load(global: Option<&Path>, project: Option<&Path>) -> Result<Self> {
        Ok(Self::load_with_origins(global, project)?.config)
    }

    pub fn load_with_origins(
        global: Option<&Path>,
        project: Option<&Path>,
    ) -> Result<McpConfigLoad> {
        let global = match global {
            Some(p) => Self::read(p)?,
            None => Self::read(&Self::global_path()?)?,
        };
        let mut merged = global;
        let mut project_servers = std::collections::BTreeSet::new();
        if let Some(path) = project {
            let local = Self::read(path)?;
            for (name, server) in local.servers {
                if merged.servers.get(&name).is_some_and(|g| !g.enabled) {
                    continue;
                }
                merged.servers.insert(name.clone(), server);
                project_servers.insert(name);
            }
        }
        Ok(McpConfigLoad {
            config: merged,
            project_servers,
        })
    }

    pub fn load_for_project(repo_root: &Path) -> Result<Self> {
        Self::load(None, Some(&repo_root.join(".dume/mcp.json")))
    }

    pub fn expand_environment(&self) -> Result<Self> {
        let mut result = self.clone();
        for server in result.servers.values_mut() {
            server.command = expand(&server.command)?;
            for arg in &mut server.args {
                *arg = expand(arg)?;
            }
            for value in server.env.values_mut() {
                *value = expand(value)?;
            }
            if let Some(cwd) = &server.cwd {
                server.cwd = Some(PathBuf::from(expand(&cwd.to_string_lossy())?));
            }
        }
        Ok(result)
    }

    pub fn canonical_digest(&self, server_name: &str) -> Result<String> {
        let server = self
            .servers
            .get(server_name)
            .context("unknown MCP server")?;
        let expanded = McpConfig {
            servers: [(server_name.to_owned(), server.clone())]
                .into_iter()
                .collect(),
        }
        .expand_environment()?;
        let canonical = serde_json::to_vec(expanded.servers.get(server_name).unwrap())?;
        Ok(hex::encode(Sha256::digest(canonical)))
    }

    pub fn catalog_fingerprint(&self) -> Result<String> {
        let canonical = serde_json::to_vec(self)?;
        Ok(hex::encode(Sha256::digest(canonical)))
    }
}

#[derive(Debug, Clone)]
pub struct McpConfigLoad {
    pub config: McpConfig,
    pub project_servers: std::collections::BTreeSet<String>,
}

fn expand(input: &str) -> Result<String> {
    let mut out = String::new();
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 2..];
        let end = tail
            .find('}')
            .context("unterminated ${VAR} environment expansion")?;
        let key = &tail[..end];
        anyhow::ensure!(
            !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "invalid environment variable name"
        );
        out.push_str(
            &std::env::var(key)
                .with_context(|| format!("environment variable {key} is not set"))?,
        );
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    anyhow::ensure!(!out.contains("${"), "unterminated environment expansion");
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectTrust {
    pub canonical_repo: PathBuf,
    pub config_digest: String,
}

#[derive(Debug, Clone, Default)]
pub struct TrustStore {
    trusted: Vec<ProjectTrust>,
}
impl TrustStore {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let trusted = serde_json::from_slice(
            &std::fs::read(path).with_context(|| format!("read {}", path.display()))?,
        )
        .with_context(|| format!("parse {}", path.display()))?;
        Ok(Self { trusted })
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(&self.trusted)?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(tmp, path).with_context(|| format!("replace {}", path.display()))
    }
    pub fn inspect(&self, repo: &Path, digest: &str) -> Result<bool> {
        let canonical_repo = repository_identity(repo)?;
        Ok(self
            .trusted
            .iter()
            .any(|x| x.canonical_repo == canonical_repo && x.config_digest == digest))
    }
    pub fn trust(&mut self, repo: &Path, digest: impl Into<String>) -> Result<ProjectTrust> {
        let record = ProjectTrust {
            canonical_repo: repository_identity(repo)?,
            config_digest: digest.into(),
        };
        if !self.trusted.contains(&record) {
            self.trusted.push(record.clone());
        }
        Ok(record)
    }
    pub fn records(&self) -> &[ProjectTrust] {
        &self.trusted
    }
}

fn repository_identity(repo: &Path) -> Result<PathBuf> {
    let canonical = repo
        .canonicalize()
        .with_context(|| format!("canonicalize repo {}", repo.display()))?;
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&canonical)
        .output();
    let Ok(output) = output else {
        return Ok(canonical);
    };
    if !output.status.success() {
        return Ok(canonical);
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let root = PathBuf::from(root).canonicalize();
    let Ok(root) = root else { return Ok(canonical) };
    if root != canonical {
        return Ok(canonical);
    }

    let worktrees = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(&canonical)
        .output();
    let Ok(worktrees) = worktrees else {
        return Ok(canonical);
    };
    if !worktrees.status.success() {
        return Ok(canonical);
    }
    let registered = String::from_utf8_lossy(&worktrees.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .filter_map(|path| PathBuf::from(path).canonicalize().ok())
        .any(|path| path == canonical);
    if !registered {
        return Ok(canonical);
    }

    let common = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(&canonical)
        .output()?;
    anyhow::ensure!(
        common.status.success(),
        "resolve git common directory for {}",
        canonical.display()
    );
    let common = String::from_utf8_lossy(&common.stdout).trim().to_owned();
    let common = PathBuf::from(common);
    Ok(if common.is_absolute() {
        common
    } else {
        canonical.join(common)
    }
    .canonicalize()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_and_exact_overrides_deserialize() {
        let config: McpConfig = serde_json::from_value(serde_json::json!({"servers":{"s":{"command":"tool","exposure":"deferred","tool_overrides":{"hidden":"hidden"}}}})).unwrap();
        let server = &config.servers["s"];
        assert!(server.enabled);
        assert_eq!(server.exposure, ToolExposure::Deferred);
        assert_eq!(server.tool_overrides["hidden"], ToolExposure::Hidden);
    }

    #[test]
    fn expansion_does_not_interpret_shell_syntax() {
        let path = std::env::var("PATH").unwrap();
        assert_eq!(
            expand("${PATH}; $(touch /tmp/nope)").unwrap(),
            format!("{path}; $(touch /tmp/nope)")
        );
        assert!(expand("${DUME_MCP_MISSING}").is_err());
    }

    #[test]
    fn trust_keeps_multiple_digests_for_one_repository() {
        let dir = tempfile::tempdir().unwrap();
        let mut trust = TrustStore::default();
        trust.trust(dir.path(), "a").unwrap();
        trust.trust(dir.path(), "b").unwrap();
        assert!(trust.inspect(dir.path(), "a").unwrap());
        assert!(trust.inspect(dir.path(), "b").unwrap());
        trust.trust(dir.path(), "a").unwrap();
        assert_eq!(trust.records().len(), 2);
    }

    #[test]
    #[cfg(unix)]
    fn registered_worktrees_share_trust_without_trusting_gitfile_aliases() {
        use std::process::Command;
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        let worker = temp.path().join("worker");
        std::fs::create_dir(&main).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&main, &["init"]);
        git(&main, &["config", "user.email", "test@example.invalid"]);
        git(&main, &["config", "user.name", "Test"]);
        std::fs::write(main.join("file"), "test").unwrap();
        git(&main, &["add", "file"]);
        git(&main, &["commit", "-m", "fixture"]);
        git(
            &main,
            &["worktree", "add", "-b", "worker", worker.to_str().unwrap()],
        );

        let mut trust = TrustStore::default();
        trust.trust(&main, "server-a-digest").unwrap();
        assert!(trust.inspect(&worker, "server-a-digest").unwrap());
        assert!(!trust.inspect(&worker, "server-b-digest").unwrap());
        let link = temp.path().join("worker-link");
        std::os::unix::fs::symlink(&worker, &link).unwrap();
        assert!(trust.inspect(&link, "server-a-digest").unwrap());

        let unrelated = temp.path().join("unrelated");
        std::fs::create_dir(&unrelated).unwrap();
        git(&unrelated, &["init"]);
        assert!(!trust.inspect(&unrelated, "server-a-digest").unwrap());
        let forged = temp.path().join("forged");
        std::fs::create_dir(&forged).unwrap();
        let git_dir = main.join(".git").canonicalize().unwrap();
        std::fs::write(
            forged.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .unwrap();
        assert!(!trust.inspect(&forged, "server-a-digest").unwrap());
    }

    #[test]
    fn config_digest_is_sha256() {
        let config: McpConfig =
            serde_json::from_value(serde_json::json!({"servers":{"s":{"command":"tool"}}}))
                .unwrap();
        assert_eq!(config.canonical_digest("s").unwrap().len(), 64);
        assert!(
            serde_json::from_value::<McpConfig>(
                serde_json::json!({"servers":{"s":{"command":"tool","mode":"hidden"}}})
            )
            .is_err()
        );
    }
}

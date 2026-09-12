use anyhow::{Context, Result};
use serde_json::Value;
use std::path::PathBuf;
use tokio::process::Command;


pub struct LocalToolExecutor {
    worktree_path: PathBuf,
}

impl LocalToolExecutor {
    pub fn new(worktree_path: impl Into<PathBuf>) -> Self {
        Self {
            worktree_path: worktree_path.into(),
        }
    }

    pub async fn execute(&self, name: &str, args: &Value) -> Result<String> {
        match name {
            "bash" => {
                let cmd_str = args.get("command")
                    .and_then(|v| v.as_str())
                    .context("Missing 'command' argument for bash")?;
                self.run_bash(cmd_str).await
            }
            "read_file" => {
                let path_str = args.get("path")
                    .and_then(|v| v.as_str())
                    .context("Missing 'path' argument for read_file")?;
                self.read_file(path_str).await
            }
            "write_file" => {
                let path_str = args.get("path")
                    .and_then(|v| v.as_str())
                    .context("Missing 'path' argument for write_file")?;
                let content = args.get("content")
                    .and_then(|v| v.as_str())
                    .context("Missing 'content' argument for write_file")?;
                self.write_file(path_str, content).await
            }
            _ => anyhow::bail!("Unknown tool: {}", name),
        }
    }

    async fn run_bash(&self, command_str: &str) -> Result<String> {
        let output = Command::new("sh")
            .arg("-c")
            .arg(command_str)
            .current_dir(&self.worktree_path)
            .output()
            .await
            .context("Failed to execute bash command")?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let status = output.status.code().unwrap_or(-1);

        Ok(format!(
            "Exit code: {}\nStdout:\n{}\nStderr:\n{}",
            status, stdout, stderr
        ))
    }

    async fn read_file(&self, rel_path: &str) -> Result<String> {
        let full_path = self.resolve_path(rel_path)?;
        let content = tokio::fs::read_to_string(&full_path)
            .await
            .with_context(|| format!("Failed to read file: {}", full_path.display()))?;
        Ok(content)
    }

    async fn write_file(&self, rel_path: &str, content: &str) -> Result<String> {
        let full_path = self.resolve_path(rel_path)?;
        if let Some(parent) = full_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&full_path, content)
            .await
            .with_context(|| format!("Failed to write file: {}", full_path.display()))?;
        Ok(format!("Successfully wrote {} bytes to {}", content.len(), rel_path))
    }

    fn resolve_path(&self, rel_path: &str) -> Result<PathBuf> {
        let clean = rel_path.trim_start_matches('/');
        let resolved = self.worktree_path.join(clean);
        // Normalize and guard against directory traversal
        let canonical_base = std::fs::canonicalize(&self.worktree_path).unwrap_or_else(|_| self.worktree_path.clone());
        if let Ok(canon_resolved) = std::fs::canonicalize(&resolved) {
            if !canon_resolved.starts_with(&canonical_base) {
                anyhow::bail!("Security violation: Path outside worktree: {}", rel_path);
            }
        }
        Ok(resolved)
    }
}

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
            "replace_file_content" => {
                let path_str = args.get("path")
                    .and_then(|v| v.as_str())
                    .context("Missing 'path' argument for replace_file_content")?;
                let target = args.get("target")
                    .and_then(|v| v.as_str())
                    .context("Missing 'target' argument for replace_file_content")?;
                let replacement = args.get("replacement")
                    .and_then(|v| v.as_str())
                    .context("Missing 'replacement' argument for replace_file_content")?;
                self.replace_file_content(path_str, target, replacement).await
            }
            "grep_search" => {
                let pattern = args.get("pattern")
                    .and_then(|v| v.as_str())
                    .context("Missing 'pattern' argument for grep_search")?;
                self.grep_search(pattern).await
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

    async fn replace_file_content(&self, rel_path: &str, target: &str, replacement: &str) -> Result<String> {
        let full_path = self.resolve_path(rel_path)?;
        let content = tokio::fs::read_to_string(&full_path)
            .await
            .with_context(|| format!("Failed to read file for replace: {}", full_path.display()))?;

        if !content.contains(target) {
            anyhow::bail!("Target content not found in file: {}", rel_path);
        }

        let new_content = content.replacen(target, replacement, 1);
        tokio::fs::write(&full_path, new_content)
            .await
            .with_context(|| format!("Failed to write modified file: {}", full_path.display()))?;

        Ok(format!("Successfully replaced content in {}", rel_path))
    }

    async fn grep_search(&self, pattern: &str) -> Result<String> {
        let output = Command::new("git")
            .args(["grep", "-n", "--", pattern])
            .current_dir(&self.worktree_path)
            .output()
            .await;

        match output {
            Ok(out) if out.status.success() => {
                Ok(String::from_utf8_lossy(&out.stdout).to_string())
            }
            Ok(out) if out.status.code() == Some(1) => {
                Ok("No matches found".to_string())
            }
            _ => {
                // Fallback to find + grep for non-git worktree directories
                let sh_out = Command::new("grep")
                    .args(["-rn", pattern, "."])
                    .current_dir(&self.worktree_path)
                    .output()
                    .await
                    .context("Failed to run grep search")?;
                let text = String::from_utf8_lossy(&sh_out.stdout);
                if text.is_empty() {
                    Ok("No matches found".to_string())
                } else {
                    Ok(text.to_string())
                }
            }
        }
    }

    fn resolve_path(&self, rel_path: &str) -> Result<PathBuf> {
        let clean = rel_path.trim_start_matches('/');

        // Canonical base directory
        let canonical_base = std::fs::canonicalize(&self.worktree_path)
            .unwrap_or_else(|_| self.worktree_path.clone());

        // Build normalized path starting from canonical_base
        let mut normalized = canonical_base.clone();
        for comp in std::path::Path::new(clean).components() {
            match comp {
                std::path::Component::ParentDir => {
                    if normalized == canonical_base || !normalized.pop() {
                        anyhow::bail!("Security violation: Path outside worktree: {}", rel_path);
                    }
                }
                std::path::Component::CurDir => {}
                std::path::Component::Normal(c) => normalized.push(c),
                _ => anyhow::bail!("Security violation: Invalid path component in {}", rel_path),
            }
        }

        // If target or any existing component is a symlink, verify canonical target remains within canonical_base
        if normalized.exists() || normalized.is_symlink() {
            if let Ok(canon) = std::fs::canonicalize(&normalized) {
                if !canon.starts_with(&canonical_base) {
                    anyhow::bail!("Security violation: Symlink or path outside worktree: {}", rel_path);
                }
            }
        } else {
            let mut parent_check = normalized.parent();
            while let Some(p) = parent_check {
                if p.exists() {
                    if let Ok(canon_p) = std::fs::canonicalize(p) {
                        if !canon_p.starts_with(&canonical_base) {
                            anyhow::bail!("Security violation: Parent outside worktree: {}", rel_path);
                        }
                    }
                    break;
                }
                parent_check = p.parent();
            }
        }

        if !normalized.starts_with(&canonical_base) {
            anyhow::bail!("Security violation: Target outside worktree: {}", rel_path);
        }

        Ok(normalized)
    }
}

use anyhow::{Context, Result};
use serde_json::Value;
use std::path::PathBuf;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

pub struct LocalToolExecutor {
    worktree_path: PathBuf,
    cancellation: CancellationToken,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_stops_shell_descendants_before_returning() {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let dir = tempfile::tempdir().unwrap();
        let gate_path = dir.path().join("gate");
        let gate_c_path = std::ffi::CString::new(gate_path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(gate_c_path.as_ptr(), 0o600) }, 0);
        let mut gate = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(gate_path)
            .unwrap();
        let token = CancellationToken::new();
        let executor = LocalToolExecutor::new(dir.path()).with_cancellation(token.clone());
        let running = tokio::spawn(async move {
            executor.execute("bash", &serde_json::json!({
                "command": "echo $$ > shell-pid; sh -c 'echo $$ > descendant-pid; touch ready; read release < gate; echo escaped > late-output' & wait"
            })).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !dir.path().join("ready").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        token.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), running)
            .await
            .unwrap()
            .unwrap();
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        // Release the payload only after cancellation returns, without depending on
        // how quickly the test runtime schedules the cancellation request.
        gate.write_all(b"release\n").unwrap();
        for name in ["shell-pid", "descendant-pid"] {
            let pid: i32 = std::fs::read_to_string(dir.path().join(name))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "{name} is still alive");
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
            if name == "shell-pid" {
                assert_eq!(
                    unsafe { libc::kill(-pid, 0) },
                    -1,
                    "process group is still alive"
                );
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::ESRCH)
                );
            }
        }
        assert!(!dir.path().join("late-output").exists());
    }
}

impl LocalToolExecutor {
    pub fn new(worktree_path: impl Into<PathBuf>) -> Self {
        Self {
            worktree_path: worktree_path.into(),
            cancellation: CancellationToken::new(),
        }
    }

    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    pub async fn execute(&self, name: &str, args: &Value) -> Result<String> {
        anyhow::ensure!(
            !self.cancellation.is_cancelled(),
            "Tool execution cancelled"
        );
        match name {
            "bash" => {
                let cmd_str = args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .context("Missing 'command' argument for bash")?;
                self.run_bash(cmd_str).await
            }
            "read_file" => {
                let path_str = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .context("Missing 'path' argument for read_file")?;
                self.read_file(path_str).await
            }
            "write_file" => {
                let path_str = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .context("Missing 'path' argument for write_file")?;
                let content = args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .context("Missing 'content' argument for write_file")?;
                self.write_file(path_str, content).await
            }
            "replace_file_content" => {
                let path_str = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .context("Missing 'path' argument for replace_file_content")?;
                let target = args
                    .get("target")
                    .and_then(|v| v.as_str())
                    .context("Missing 'target' argument for replace_file_content")?;
                let replacement = args
                    .get("replacement")
                    .and_then(|v| v.as_str())
                    .context("Missing 'replacement' argument for replace_file_content")?;
                self.replace_file_content(path_str, target, replacement)
                    .await
            }
            "grep_search" => {
                let pattern = args
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .context("Missing 'pattern' argument for grep_search")?;
                self.grep_search(pattern).await
            }
            _ => anyhow::bail!("Unknown tool: {}", name),
        }
    }

    async fn run_bash(&self, command_str: &str) -> Result<String> {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(command_str)
            .current_dir(&self.worktree_path);
        let output = self.run_command(&mut command).await?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let status = output.status.code().unwrap_or(-1);

        Ok(format!(
            "Exit code: {}\nStdout:\n{}\nStderr:\n{}",
            status, stdout, stderr
        ))
    }

    async fn run_command(&self, command: &mut Command) -> Result<std::process::Output> {
        anyhow::ensure!(
            !self.cancellation.is_cancelled(),
            "Tool execution cancelled"
        );
        command
            .kill_on_drop(true)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);
        let child = command.spawn().context("Failed to spawn tool process")?;
        let pid = child.id().context("Tool process has no ID")?;
        let output = child.wait_with_output();
        tokio::pin!(output);
        tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => {
                #[cfg(unix)]
                {
                    let terminate_group = async {
                        let mut signalled = false;
                        loop {
                            // A descendant racing a fork can miss one group signal.
                            // Keep killing until the group no longer exists, not just
                            // until its leader exits or its output pipes close.
                            if unsafe { libc::kill(-(pid as i32), libc::SIGKILL) } != 0 {
                                let error = std::io::Error::last_os_error();
                                if error.raw_os_error() == Some(libc::ESRCH) {
                                    return Ok::<(), anyhow::Error>(());
                                }
                                // Darwin can reject a repeated SIGKILL while the
                                // successfully signalled group is exiting. Retry
                                // within the deadline, never treating EPERM as exit.
                                if !signalled || error.raw_os_error() != Some(libc::EPERM) {
                                    return Err(error).context("Failed to terminate cancelled tool process group");
                                }
                            } else {
                                signalled = true;
                            }
                            tokio::task::yield_now().await;
                        }
                    };
                    // Reap the leader concurrently so its zombie cannot keep the
                    // process group alive while termination waits for ESRCH.
                    tokio::time::timeout(std::time::Duration::from_secs(5), async {
                        tokio::try_join!(terminate_group, async {
                            output.await.context("Failed to reap cancelled tool process")
                        })
                    }).await.context("Timed out terminating cancelled tool process group")??;
                }
                #[cfg(not(unix))]
                {
                    let status = Command::new("taskkill")
                        .args(["/F", "/T", "/PID", &pid.to_string()])
                        .status().await.context("Failed to terminate cancelled tool process tree")?;
                    anyhow::ensure!(status.success(), "Failed to terminate cancelled tool process tree");
                    output.await.context("Failed to reap cancelled tool process")?;
                }
                anyhow::bail!("Tool execution cancelled");
            }
            result = &mut output => Ok(result?),
        }
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
        Ok(format!(
            "Successfully wrote {} bytes to {}",
            content.len(),
            rel_path
        ))
    }

    async fn replace_file_content(
        &self,
        rel_path: &str,
        target: &str,
        replacement: &str,
    ) -> Result<String> {
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
        let mut command = Command::new("git");
        command
            .args(["grep", "-n", "--", pattern])
            .current_dir(&self.worktree_path);
        let output = self.run_command(&mut command).await;

        match output {
            Ok(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).to_string()),
            Ok(out) if out.status.code() == Some(1) => Ok("No matches found".to_string()),
            _ => {
                // Fallback to find + grep for non-git worktree directories
                let mut command = Command::new("grep");
                command
                    .args(["-rn", pattern, "."])
                    .current_dir(&self.worktree_path);
                let sh_out = self
                    .run_command(&mut command)
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
                    anyhow::bail!(
                        "Security violation: Symlink or path outside worktree: {}",
                        rel_path
                    );
                }
            }
        } else {
            let mut parent_check = normalized.parent();
            while let Some(p) = parent_check {
                if p.exists() {
                    if let Ok(canon_p) = std::fs::canonicalize(p) {
                        if !canon_p.starts_with(&canonical_base) {
                            anyhow::bail!(
                                "Security violation: Parent outside worktree: {}",
                                rel_path
                            );
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

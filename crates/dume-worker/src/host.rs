use crate::ipc::{WorkerToHostMessage, deserialize_message};
use anyhow::{Context, Result};
use dume_core::manifest::ResultManifest;
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct WorkerHost {
    binary_path: String,
    db_path: Option<std::path::PathBuf>,
    artifacts_dir: Option<std::path::PathBuf>,
}

impl WorkerHost {
    pub fn new(binary_path: &str) -> Self {
        Self {
            binary_path: binary_path.to_string(),
            db_path: None,
            artifacts_dir: None,
        }
    }

    pub fn with_db_path(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.db_path = Some(path.into());
        self
    }

    pub fn with_artifacts_dir(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.artifacts_dir = Some(path.into());
        self
    }

    pub async fn run_attempt(
        &self,
        attempt_id: &str,
        _task_id: &str,
        _coordinator_epoch: i64,
        worktree_path: &Path,
        test_command: Option<String>,
        task_prompt: Option<String>,
        model: Option<String>,
        cancel_token: CancellationToken,
    ) -> Result<ResultManifest> {
        let mut cmd = Command::new(&self.binary_path);
        cmd.args([
            "worker",
            "--attempt-id",
            attempt_id,
            "--worktree-path",
            worktree_path.to_str().unwrap(),
        ]);

        if let Some(ref tc) = test_command {
            cmd.args(["--test-command", tc]);
        }
        if let Some(ref tp) = task_prompt {
            cmd.args(["--task-prompt", tp]);
        }
        if let Some(ref m) = model {
            cmd.args(["--model", m]);
        }
        if let Some(path) = &self.db_path {
            cmd.arg("--db-path").arg(path);
        }
        if let Some(path) = &self.artifacts_dir {
            cmd.arg("--artifacts-dir").arg(path);
        }

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                // Setsid to create independent process group
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child: Child = cmd.spawn().context("Failed to spawn worker process")?;
        let pid = child.id();

        let stdout = child
            .stdout
            .take()
            .context("Failed to capture worker stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("Failed to capture worker stderr")?;
        let mut stderr_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut buffer = [0u8; 4096];
            loop {
                match tokio::io::AsyncReadExt::read(&mut reader, &mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        });
        let mut reader = BufReader::new(stdout).lines();

        let mut final_manifest = None;

        let result: Result<()> = tokio::select! {
            _ = cancel_token.cancelled() => {
                anyhow::bail!("Attempt {} cancelled by host", attempt_id);
            }
            res = async {
                while let Ok(Some(line)) = reader.next_line().await {
                    if let Ok(msg) = deserialize_message::<WorkerToHostMessage>(&line) {
                        match msg {
                            WorkerToHostMessage::Completed { manifest, outcome } => {
                                if matches!(&outcome, Some(crate::agent_loop::AgentOutcome::TurnLimitExhausted { .. } | crate::agent_loop::AgentOutcome::Cancelled { .. })) {
                                    anyhow::bail!("Worker agent did not complete successfully: {}", outcome.unwrap());
                                }
                                final_manifest = Some(manifest);
                                break;
                            }
                            WorkerToHostMessage::Outcome { outcome } => match outcome {
                                crate::agent_loop::AgentOutcome::Completed { .. } => {}
                                other => anyhow::bail!("Worker agent did not complete successfully: {}", other),
                            },
                            WorkerToHostMessage::Failed { error } => {
                                anyhow::bail!("Worker failed: {}", error);
                            }
                            WorkerToHostMessage::Heartbeat { .. } => {}
                            WorkerToHostMessage::Progress { message, .. } => {
                                tracing::debug!("Worker progress: {}", message);
                            }
                        }
                    }
                }
                let status = child.wait().await?;
                if !status.success() {
                    anyhow::bail!("Worker process exited with code: {:?}", status.code());
                }
                Ok(())
            } => {
                res
            }
        };

        if result.is_err() {
            if let Some(p) = pid {
                #[cfg(unix)]
                unsafe {
                    libc::kill(-(p as libc::pid_t), libc::SIGTERM);
                }
                #[cfg(not(unix))]
                {
                    let _ = child.start_kill();
                }
                if tokio::time::timeout(std::time::Duration::from_secs(2), child.wait())
                    .await
                    .is_err()
                {
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(-(p as libc::pid_t), libc::SIGKILL);
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = child.start_kill();
                    }
                    let _ = child.wait().await;
                } else {
                    // Reap descendants that inherited the worker's pipes as well.
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(-(p as libc::pid_t), libc::SIGKILL);
                    }
                }
            } else {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        }
        if tokio::time::timeout(std::time::Duration::from_secs(1), &mut stderr_task)
            .await
            .is_err()
        {
            stderr_task.abort();
            let _ = stderr_task.await;
        }
        result?;

        final_manifest.context("Worker terminated without providing ResultManifest")
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fake_worker(dir: &Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("fake-worker.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[tokio::test]
    async fn failed_worker_is_reaped_and_stderr_is_drained() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived");
        let worker = fake_worker(
            dir.path(),
            &format!(
                "(sleep 1; printf survived > '{}') &\ni=0; while [ $i -lt 4000 ]; do printf 'stderr line %s\\n' \"$i\" >&2; i=$((i+1)); done\nprintf '{{\"type\":\"failed\",\"error\":\"expected failure\"}}\\n'\nwait",
                marker.display()
            ),
        );
        let result = WorkerHost::new(worker.to_str().unwrap())
            .run_attempt(
                "attempt",
                "task",
                0,
                dir.path(),
                None,
                None,
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(result.unwrap_err().to_string().contains("Worker failed"));
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        assert!(
            !marker.exists(),
            "worker descendant survived failure cleanup"
        );
    }

    #[tokio::test]
    async fn cancellation_kills_worker_group_and_drains_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived-cancel");
        let worker = fake_worker(
            dir.path(),
            &format!(
                "(sleep 1; printf survived > '{}') &\ni=0; while [ $i -lt 4000 ]; do printf 'stderr line %s\\n' \"$i\" >&2; i=$((i+1)); done\nwait",
                marker.display()
            ),
        );
        let token = CancellationToken::new();
        let cancel = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            cancel.cancel();
        });
        let result = WorkerHost::new(worker.to_str().unwrap())
            .run_attempt("attempt", "task", 0, dir.path(), None, None, None, token)
            .await;
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        assert!(
            !marker.exists(),
            "worker descendant survived cancellation cleanup"
        );
    }
}

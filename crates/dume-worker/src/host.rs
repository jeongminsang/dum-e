use crate::ipc::{deserialize_message, WorkerToHostMessage};
use anyhow::{Context, Result};
use dume_core::manifest::ResultManifest;
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

pub struct WorkerHost {
    binary_path: String,
}

impl WorkerHost {
    pub fn new(binary_path: &str) -> Self {
        Self {
            binary_path: binary_path.to_string(),
        }
    }

    pub async fn run_attempt(
        &self,
        attempt_id: &str,
        _task_id: &str,
        _coordinator_epoch: i64,
        worktree_path: &Path,
        test_command: Option<String>,
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

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                // Setsid to create independent process group
                libc::setsid();
                Ok(())
            });
        }

        let mut child: Child = cmd.spawn().context("Failed to spawn worker process")?;
        let pid = child.id();

        let stdout = child.stdout.take().context("Failed to capture worker stdout")?;
        let mut reader = BufReader::new(stdout).lines();

        let mut final_manifest = None;

        tokio::select! {
            _ = cancel_token.cancelled() => {
                if let Some(p) = pid {
                    let _ = crate::cancel::terminate_process_group(p, 500).await;
                }
                anyhow::bail!("Attempt {} cancelled by host", attempt_id);
            }
            res = async {
                while let Ok(Some(line)) = reader.next_line().await {
                    if let Ok(msg) = deserialize_message::<WorkerToHostMessage>(&line) {
                        match msg {
                            WorkerToHostMessage::Completed { manifest } => {
                                final_manifest = Some(manifest);
                                break;
                            }
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
                if !status.success() && final_manifest.is_none() {
                    anyhow::bail!("Worker process exited with code: {:?}", status.code());
                }
                Ok(())
            } => {
                res?;
            }
        }

        final_manifest.context("Worker terminated without providing ResultManifest")
    }
}

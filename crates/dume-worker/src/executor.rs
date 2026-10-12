use anyhow::Result;
use dume_core::manifest::{ResultManifest, TestResult};
use std::path::Path;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

pub struct WorkerExecutor;

impl WorkerExecutor {
    pub async fn run_test_command(worktree_path: &Path, command_str: &str) -> Result<TestResult> {
        Self::run_test_command_with_cancellation(
            worktree_path,
            command_str,
            CancellationToken::new(),
        )
        .await
    }

    pub async fn run_test_command_with_cancellation(
        worktree_path: &Path,
        command_str: &str,
        cancellation: CancellationToken,
    ) -> Result<TestResult> {
        anyhow::ensure!(
            !cancellation.is_cancelled(),
            "Validation cancelled before launch"
        );
        let mut cmd = if cfg!(target_os = "windows") {
            let comspec = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
            let mut cmd = Command::new(comspec);
            cmd.args(["/C", command_str]).current_dir(worktree_path);
            if let Ok(sys_root) = std::env::var("SystemRoot") {
                cmd.env("SystemRoot", sys_root);
            }
            if let Ok(path) = std::env::var("PATH") {
                cmd.env("PATH", path);
            }
            cmd
        } else {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", command_str]).current_dir(worktree_path);
            cmd
        };

        cmd.kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        let pid = child.id();
        let mut output = Box::pin(child.wait_with_output());
        let output = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                if let Some(pid) = pid { crate::terminate_process_group(pid, 100).await?; }
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), &mut output).await;
                anyhow::bail!("Validation cancelled");
            }
            result = &mut output => result?,
        };

        let exit_code = output.status.code().unwrap_or(-1);
        let passed = output.status.success();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        Ok(TestResult {
            name: command_str.to_string(),
            passed,
            exit_code,
            stdout,
            stderr,
        })
    }

    pub async fn finalize_manifest(
        attempt_id: &str,
        worktree_path: &Path,
        test_results: Vec<TestResult>,
        summary: &str,
    ) -> Result<ResultManifest> {
        // 1. Get modified files
        let modified_files = dume_git::diff::get_worktree_modified_files(worktree_path).await?;

        // 2. Commit worktree changes to create candidate commit
        let commit_msg = format!("feat(dume): attempt {} - {}", attempt_id, summary);
        let candidate_commit =
            dume_git::commit::commit_worktree_changes(worktree_path, &commit_msg).await?;

        Ok(ResultManifest {
            attempt_id: attempt_id.to_string(),
            candidate_commit,
            modified_files,
            changed_artifacts: vec![],
            test_results,
            summary: summary.to_string(),
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_validation_does_not_launch() {
        let dir = tempfile::tempdir().unwrap();
        let token = CancellationToken::new();
        token.cancel();
        assert!(
            WorkerExecutor::run_test_command_with_cancellation(dir.path(), "touch started", token)
                .await
                .is_err()
        );
        assert!(!dir.path().join("started").exists());
    }

    #[tokio::test]
    async fn cancellation_stops_validation_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let token = CancellationToken::new();
        let cancel = token.clone();
        let path = dir.path().to_path_buf();
        let running = tokio::spawn(async move {
            WorkerExecutor::run_test_command_with_cancellation(&path,
                "sh -c 'touch ready; while [ ! -e release ]; do sleep 0.02; done; echo survived > late' & wait", token).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !dir.path().join("ready").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        cancel.cancel();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), running)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        std::fs::write(dir.path().join("release"), "go").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!dir.path().join("late").exists());
    }
}

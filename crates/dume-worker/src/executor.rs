use anyhow::Result;
use dume_core::manifest::{ResultManifest, TestResult};
use std::path::Path;
use tokio::process::Command;

pub struct WorkerExecutor;

impl WorkerExecutor {
    pub async fn run_test_command(
        worktree_path: &Path,
        command_str: &str,
    ) -> Result<TestResult> {
        let output = if cfg!(target_os = "windows") {
            let comspec = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
            let mut cmd = Command::new(comspec);
            cmd.args(["/C", command_str])
                .current_dir(worktree_path);
            if let Ok(sys_root) = std::env::var("SystemRoot") {
                cmd.env("SystemRoot", sys_root);
            }
            if let Ok(path) = std::env::var("PATH") {
                cmd.env("PATH", path);
            }
            cmd.output().await?
        } else {
            Command::new("sh")
                .args(["-c", command_str])
                .current_dir(worktree_path)
                .output()
                .await?
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
        let candidate_commit = dume_git::commit::commit_worktree_changes(worktree_path, &commit_msg).await?;

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

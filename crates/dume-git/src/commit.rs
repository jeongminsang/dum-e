use anyhow::{Context, Result};
use std::path::Path;
use tokio::process::Command;

pub async fn commit_worktree_changes(worktree_dir: &Path, message: &str) -> Result<String> {
    // 1. Stage all changes
    let add_output = Command::new("git")
        .args(["-C", worktree_dir.to_str().unwrap(), "add", "-A"])
        .output()
        .await
        .context("Failed to stage changes in worktree")?;

    if !add_output.status.success() {
        let stderr = String::from_utf8_lossy(&add_output.stderr);
        anyhow::bail!("git add failed: {}", stderr);
    }

    // 2. Commit
    let commit_output = Command::new("git")
        .args([
            "-C",
            worktree_dir.to_str().unwrap(),
            "commit",
            "--allow-empty",
            "-m",
            message,
        ])
        .output()
        .await
        .context("Failed to commit changes in worktree")?;

    if !commit_output.status.success() {
        let stderr = String::from_utf8_lossy(&commit_output.stderr);
        anyhow::bail!("git commit failed: {}", stderr);
    }

    // 3. Get rev-parse HEAD
    let rev_output = Command::new("git")
        .args(["-C", worktree_dir.to_str().unwrap(), "rev-parse", "HEAD"])
        .output()
        .await
        .context("Failed to resolve candidate commit SHA")?;

    if !rev_output.status.success() {
        let stderr = String::from_utf8_lossy(&rev_output.stderr);
        anyhow::bail!("git rev-parse HEAD failed: {}", stderr);
    }

    let sha = String::from_utf8_lossy(&rev_output.stdout).trim().to_string();
    Ok(sha)
}

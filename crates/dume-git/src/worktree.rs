use anyhow::{Context, Result};
use std::path::Path;
use tokio::process::Command;

pub async fn create_git_worktree(
    repo_path: &Path,
    worktree_dir: &Path,
    commit_or_branch: &str,
) -> Result<()> {
    if let Some(parent) = worktree_dir.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let output = Command::new("git")
        .args([
            "-C",
            repo_path.to_str().unwrap(),
            "worktree",
            "add",
            "--detach",
            worktree_dir.to_str().unwrap(),
            commit_or_branch,
        ])
        .output()
        .await
        .context("Failed to run git worktree add")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git worktree add failed: {}", stderr);
    }

    Ok(())
}

pub async fn remove_git_worktree(repo_path: &Path, worktree_dir: &Path) -> Result<()> {
    let output = Command::new("git")
        .args([
            "-C",
            repo_path.to_str().unwrap(),
            "worktree",
            "remove",
            "--force",
            worktree_dir.to_str().unwrap(),
        ])
        .output()
        .await
        .context("Failed to run git worktree remove")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!("git worktree remove warning: {}", stderr);
    }

    // Prune stale worktrees
    let _ = Command::new("git")
        .args(["-C", repo_path.to_str().unwrap(), "worktree", "prune"])
        .output()
        .await;

    Ok(())
}

use anyhow::{Context, Result};
use std::path::Path;
use tokio::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntegrationResult {
    Success {
        base_commit: String,
        integration_commit: String,
    },
    Conflict {
        base_commit: String,
        details: String,
    },
    Failed {
        error: String,
    },
}

pub async fn integrate_candidate_commit(
    repo_path: &Path,
    target_branch: &str,
    candidate_commit: &str,
    integration_worktree_dir: &Path,
) -> Result<IntegrationResult> {
    let repo_str = repo_path.to_str().unwrap();

    // 1. Verify candidate commit exists
    let cat_check = Command::new("git")
        .args(["-C", repo_str, "cat-file", "-t", candidate_commit])
        .output()
        .await?;

    if !cat_check.status.success() || String::from_utf8_lossy(&cat_check.stdout).trim() != "commit" {
        return Ok(IntegrationResult::Failed {
            error: format!("Invalid commit object: {}", candidate_commit),
        });
    }

    // 2. Prepare clean integration worktree on target_branch
    let _ = crate::worktree::remove_git_worktree(repo_path, integration_worktree_dir).await;
    crate::worktree::create_git_worktree(repo_path, integration_worktree_dir, target_branch).await?;

    let wt_str = integration_worktree_dir.to_str().unwrap();

    // Get base_commit
    let base_rev = Command::new("git")
        .args(["-C", wt_str, "rev-parse", "HEAD"])
        .output()
        .await?;
    let base_commit = String::from_utf8_lossy(&base_rev.stdout).trim().to_string();

    // 3. Cherry-pick candidate commit
    let cp_output = Command::new("git")
        .args(["-C", wt_str, "cherry-pick", candidate_commit])
        .output()
        .await
        .context("Failed to execute git cherry-pick")?;

    if !cp_output.status.success() {
        let stderr = String::from_utf8_lossy(&cp_output.stderr);
        let stdout = String::from_utf8_lossy(&cp_output.stdout);
        let combined = format!("{}\n{}", stdout, stderr);

        // Abort in-flight cherry-pick to restore clean worktree
        let _ = Command::new("git")
            .args(["-C", wt_str, "cherry-pick", "--abort"])
            .output()
            .await;

        let _ = crate::worktree::remove_git_worktree(repo_path, integration_worktree_dir).await;

        return Ok(IntegrationResult::Conflict {
            base_commit,
            details: combined,
        });
    }

    // 4. Resolve new integration commit SHA
    let head_rev = Command::new("git")
        .args(["-C", wt_str, "rev-parse", "HEAD"])
        .output()
        .await?;
    let integration_commit = String::from_utf8_lossy(&head_rev.stdout).trim().to_string();

    // 5. Update target branch reference to integration_commit
    let update_ref = Command::new("git")
        .args([
            "-C",
            repo_str,
            "update-ref",
            &format!("refs/heads/{}", target_branch),
            &integration_commit,
            &base_commit,
        ])
        .output()
        .await?;

    if !update_ref.status.success() {
        let stderr = String::from_utf8_lossy(&update_ref.stderr);
        let _ = crate::worktree::remove_git_worktree(repo_path, integration_worktree_dir).await;
        return Ok(IntegrationResult::Failed {
            error: format!("Failed to update branch ref: {}", stderr),
        });
    }

    // Clean up integration worktree
    let _ = crate::worktree::remove_git_worktree(repo_path, integration_worktree_dir).await;

    Ok(IntegrationResult::Success {
        base_commit,
        integration_commit,
    })
}

pub async fn prepare_candidate_cherry_pick(
    repo_path: &Path,
    target_branch: &str,
    candidate_commit: &str,
    integration_worktree_dir: &Path,
) -> Result<IntegrationResult> {
    let repo_str = repo_path.to_str().unwrap();

    let cat_check = Command::new("git")
        .args(["-C", repo_str, "cat-file", "-t", candidate_commit])
        .output()
        .await?;

    if !cat_check.status.success() || String::from_utf8_lossy(&cat_check.stdout).trim() != "commit" {
        return Ok(IntegrationResult::Failed {
            error: format!("Invalid commit object: {}", candidate_commit),
        });
    }

    let _ = crate::worktree::remove_git_worktree(repo_path, integration_worktree_dir).await;
    crate::worktree::create_git_worktree(repo_path, integration_worktree_dir, target_branch).await?;

    let wt_str = integration_worktree_dir.to_str().unwrap();

    let base_rev = Command::new("git")
        .args(["-C", wt_str, "rev-parse", "HEAD"])
        .output()
        .await?;
    let base_commit = String::from_utf8_lossy(&base_rev.stdout).trim().to_string();

    let cp_output = Command::new("git")
        .args(["-C", wt_str, "cherry-pick", candidate_commit])
        .output()
        .await
        .context("Failed to execute git cherry-pick")?;

    if !cp_output.status.success() {
        let stderr = String::from_utf8_lossy(&cp_output.stderr);
        let stdout = String::from_utf8_lossy(&cp_output.stdout);
        let combined = format!("{}\n{}", stdout, stderr);

        let _ = Command::new("git")
            .args(["-C", wt_str, "cherry-pick", "--abort"])
            .output()
            .await;

        let _ = crate::worktree::remove_git_worktree(repo_path, integration_worktree_dir).await;

        return Ok(IntegrationResult::Conflict {
            base_commit,
            details: combined,
        });
    }

    let head_rev = Command::new("git")
        .args(["-C", wt_str, "rev-parse", "HEAD"])
        .output()
        .await?;
    let integration_commit = String::from_utf8_lossy(&head_rev.stdout).trim().to_string();

    let _ = crate::worktree::remove_git_worktree(repo_path, integration_worktree_dir).await;

    Ok(IntegrationResult::Success {
        base_commit,
        integration_commit,
    })
}

pub async fn resolve_ref(repo_path: &Path, branch: &str) -> Result<String> {
    let repo_str = repo_path.to_str().unwrap();
    let out = Command::new("git")
        .args(["-C", repo_str, "rev-parse", &format!("refs/heads/{}", branch)])
        .output()
        .await?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("Failed to resolve branch ref {}: {}", branch, err);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub async fn apply_branch_update(
    repo_path: &Path,
    target_branch: &str,
    new_commit: &str,
    expected_old_commit: &str,
) -> Result<()> {
    let repo_str = repo_path.to_str().unwrap();
    let update_ref = Command::new("git")
        .args([
            "-C",
            repo_str,
            "update-ref",
            &format!("refs/heads/{}", target_branch),
            new_commit,
            expected_old_commit,
        ])
        .output()
        .await?;

    if !update_ref.status.success() {
        let stderr = String::from_utf8_lossy(&update_ref.stderr);
        anyhow::bail!("atomic git update-ref failed: {}", stderr);
    }
    Ok(())
}

pub async fn is_ancestor(repo_path: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let repo_str = repo_path.to_str().unwrap();
    let out = Command::new("git")
        .args(["-C", repo_str, "merge-base", "--is-ancestor", ancestor, descendant])
        .output()
        .await?;
    Ok(out.status.success())
}

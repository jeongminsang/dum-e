use anyhow::{Context, Result};
use std::borrow::Cow;
use std::path::Path;
use tokio::process::Command;

pub fn git_path(path: &Path) -> Cow<'_, std::ffi::OsStr> {
    #[cfg(windows)]
    if let Some(std::path::Component::Prefix(prefix)) = path.components().next() {
        let mut normalized = match prefix.kind() {
            std::path::Prefix::VerbatimDisk(drive) => {
                std::path::PathBuf::from(format!("{}:", drive as char))
            }
            std::path::Prefix::VerbatimUNC(server, share) => {
                let mut unc = std::ffi::OsString::from(r"\\");
                unc.push(server);
                unc.push(r"\");
                unc.push(share);
                std::path::PathBuf::from(unc)
            }
            _ => return Cow::Borrowed(path.as_os_str()),
        };
        // Git for Windows does not accept canonical Windows verbatim path prefixes.
        normalized.extend(path.components().skip(1));
        return Cow::Owned(normalized.into_os_string());
    }
    Cow::Borrowed(path.as_os_str())
}

pub async fn create_git_worktree(
    repo_path: &Path,
    worktree_dir: &Path,
    commit_or_branch: &str,
) -> Result<()> {
    anyhow::ensure!(
        tokio::fs::symlink_metadata(worktree_dir)
            .await
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "Worktree destination already exists or cannot be inspected: {}",
        worktree_dir.display()
    );
    if let Some(parent) = worktree_dir.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    // Atomically claim this destination; Git accepts an existing empty directory.
    tokio::fs::create_dir(worktree_dir).await.with_context(|| {
        format!(
            "Cannot claim worktree destination {}",
            worktree_dir.display()
        )
    })?;

    let output = Command::new("git")
        .arg("-C")
        .arg(git_path(repo_path))
        .args(["worktree", "add", "--detach", "--"])
        .arg(git_path(worktree_dir))
        .arg(commit_or_branch)
        .output()
        .await
        .with_context(|| {
            format!(
                "Failed to run git worktree add; destination retained at {}",
                worktree_dir.display()
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "git worktree add failed at {} (destination retained for inspection): {}",
            worktree_dir.display(),
            stderr
        );
    }

    Ok(())
}

pub async fn remove_git_worktree(repo_path: &Path, worktree_dir: &Path) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(git_path(repo_path))
        .args(["worktree", "remove", "--"])
        .arg(git_path(worktree_dir))
        .output()
        .await
        .with_context(|| {
            format!(
                "Failed to run git worktree remove; worktree retained at {}",
                worktree_dir.display()
            )
        })
        .inspect_err(|error| tracing::warn!("{:#}", error))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(
            "Worktree retained at {}: {}",
            worktree_dir.display(),
            stderr
        );
        anyhow::bail!(
            "Worktree retained at {}: git worktree remove failed: {}",
            worktree_dir.display(),
            stderr
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(git_path(repo))
            .args(args)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    async fn worktree_ownership_and_dirty_output_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let repo = canonical.as_path();
        git(repo, &["init"]).await;
        git(
            repo,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        )
        .await;
        let child = repo.join("child");
        create_git_worktree(repo, &child, "HEAD").await.unwrap();
        assert!(create_git_worktree(repo, &child, "HEAD").await.is_err());
        tokio::fs::write(child.join("output.txt"), "recover me")
            .await
            .unwrap();
        let error = remove_git_worktree(repo, &child).await.unwrap_err();
        assert!(error.to_string().contains(&child.display().to_string()));
        assert_eq!(
            tokio::fs::read_to_string(child.join("output.txt"))
                .await
                .unwrap(),
            "recover me"
        );
        git(&child, &["add", "output.txt"]).await;
        assert!(remove_git_worktree(repo, &child).await.is_err());
        assert!(child.join(".git").exists());
    }

    #[tokio::test]
    async fn worktree_creation_fails_closed_and_does_not_reuse_directories() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("child");
        assert!(
            create_git_worktree(dir.path(), &child, "HEAD")
                .await
                .is_err()
        );
        assert!(!child.join(".git").exists());
        tokio::fs::write(child.join("sentinel"), "owned")
            .await
            .unwrap();
        assert!(
            create_git_worktree(dir.path(), &child, "HEAD")
                .await
                .is_err()
        );
        assert_eq!(
            tokio::fs::read_to_string(child.join("sentinel"))
                .await
                .unwrap(),
            "owned"
        );
        assert!(remove_git_worktree(dir.path(), &child).await.is_err());
    }
}

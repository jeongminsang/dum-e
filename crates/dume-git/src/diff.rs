use anyhow::{Context, Result};
use std::path::Path;
use tokio::process::Command;

/// Paths in the patch that cherry-pick will apply, including both sides of renames.
pub async fn get_commit_modified_files(repo: &Path, commit: &str) -> Result<Vec<String>> {
    anyhow::ensure!(
        matches!(commit.len(), 40 | 64) && commit.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Candidate must be an immutable commit hash"
    );
    let parents = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-list", "--parents", "-n", "1", commit, "--"])
        .output()
        .await?;
    anyhow::ensure!(
        parents.status.success(),
        "Failed to resolve candidate parents"
    );
    anyhow::ensure!(
        String::from_utf8_lossy(&parents.stdout)
            .split_whitespace()
            .count()
            <= 2
            && !parents.stdout.is_empty(),
        "Candidate must not be a merge commit"
    );
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "--no-renames",
            "--root",
            "-r",
            "-z",
            commit,
            "--",
        ])
        .output()
        .await
        .context("Failed to inspect candidate patch")?;
    anyhow::ensure!(
        output.status.success(),
        "git diff-tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| Ok(std::str::from_utf8(path)?.to_owned()))
        .collect()
}

pub async fn get_worktree_modified_files(worktree_dir: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args([
            "-C",
            worktree_dir.to_str().unwrap(),
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
        ])
        .output()
        .await
        .context("Failed to run git status")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git status failed: {}", stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut files = Vec::new();

    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Format is XY <path> or XY <old> -> <new>
        if line.len() > 3 {
            let path_part = &line[3..];
            if let Some((_, new_path)) = path_part.split_once(" -> ") {
                files.push(new_path.trim().to_string());
            } else {
                files.push(path_part.trim().to_string());
            }
        }
    }

    Ok(files)
}

#[cfg(test)]
mod tests {

    #[test]
    fn test_parse_porcelain_lines() {
        let sample = " M src/lib.rs\n?? new_file.txt\nR  old.txt -> new.txt\n";
        let mut files = Vec::new();
        for line in sample.lines() {
            if line.len() > 3 {
                let path_part = &line[3..];
                if let Some((_, new_path)) = path_part.split_once(" -> ") {
                    files.push(new_path.trim().to_string());
                } else {
                    files.push(path_part.trim().to_string());
                }
            }
        }
        assert_eq!(files, vec!["src/lib.rs", "new_file.txt", "new.txt"]);
    }
}

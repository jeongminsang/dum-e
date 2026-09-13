use anyhow::{Context, Result};
use std::path::Path;
use tokio::process::Command;

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

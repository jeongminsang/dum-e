use anyhow::{Context, Result};
use serde_json::Value;
use std::path::PathBuf;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::output_limits::{
    truncate_output_head_tail, DEFAULT_HEAD_LINES, DEFAULT_MAX_LINES, DEFAULT_MAX_MATCHES,
    DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_TAIL_LINES,
};

pub struct LocalToolExecutor {
    worktree_path: PathBuf,
    cancellation: CancellationToken,
    artifact_store: Option<std::sync::Arc<dume_store::ArtifactStore>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_stops_shell_descendants_before_returning() {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let dir = tempfile::tempdir().unwrap();
        let gate_path = dir.path().join("gate");
        let gate_c_path = std::ffi::CString::new(gate_path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(gate_c_path.as_ptr(), 0o600) }, 0);
        let mut gate = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(gate_path)
            .unwrap();
        let token = CancellationToken::new();
        let executor = LocalToolExecutor::new(dir.path()).with_cancellation(token.clone());
        let running = tokio::spawn(async move {
            executor.execute("bash", &serde_json::json!({
                "command": "echo $$ > shell-pid; sh -c 'echo $$ > descendant-pid; touch ready; read release < gate; echo escaped > late-output' & wait"
            })).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !dir.path().join("ready").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        token.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), running)
            .await
            .unwrap()
            .unwrap();
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        // Release the payload only after cancellation returns, without depending on
        // how quickly the test runtime schedules the cancellation request.
        gate.write_all(b"release\n").unwrap();
        for name in ["shell-pid", "descendant-pid"] {
            let pid: i32 = std::fs::read_to_string(dir.path().join(name))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "{name} is still alive");
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
            if name == "shell-pid" {
                assert_eq!(
                    unsafe { libc::kill(-pid, 0) },
                    -1,
                    "process group is still alive"
                );
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::ESRCH)
                );
            }
        }
        assert!(!dir.path().join("late-output").exists());
    }

    #[tokio::test]
    async fn test_read_file_range_selection() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("sample.txt");
        let content = (1..=20).map(|i| format!("Line {}", i)).collect::<Vec<_>>().join("\n");
        tokio::fs::write(&file_path, content).await.unwrap();

        let executor = LocalToolExecutor::new(dir.path());

        // Full read
        let full = executor.execute("read_file", &serde_json::json!({
            "path": "sample.txt"
        })).await.unwrap();
        assert!(full.contains("Line 1"));
        assert!(full.contains("Line 20"));

        // Range slice lines 5 to 8
        let slice = executor.execute("read_file", &serde_json::json!({
            "path": "sample.txt",
            "start_line": 5,
            "end_line": 8
        })).await.unwrap();
        assert!(slice.contains("[Showing lines 5 to 8 of 20 total lines]"));
        assert!(slice.contains("5: Line 5"));
        assert!(slice.contains("8: Line 8"));
        assert!(!slice.contains("4: Line 4"));
        assert!(!slice.contains("9: Line 9"));
    }

    #[tokio::test]
    async fn test_read_artifact_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let art_path = dir.path().join("artifact.bin");
        let data = "0123456789abcdefghijklmnopqrstuvwxyz";
        tokio::fs::write(&art_path, data).await.unwrap();

        let executor = LocalToolExecutor::new(dir.path());
        let res = executor.execute("read_artifact", &serde_json::json!({
            "path": "artifact.bin",
            "offset": 10,
            "length": 6
        })).await.unwrap();

        assert!(res.contains("[Artifact: artifact.bin | Offset: 10 | Read: 6 bytes"));
        assert!(res.contains("abcdef"));
    }

    #[tokio::test]
    async fn test_grep_search_capped_and_filtered() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("subdir");
        tokio::fs::create_dir_all(&sub).await.unwrap();

        for i in 1..=15 {
            tokio::fs::write(sub.join(format!("file_{}.txt", i)), "target_match_word\n").await.unwrap();
        }
        tokio::fs::write(dir.path().join("outside.txt"), "target_match_word\n").await.unwrap();

        let executor = LocalToolExecutor::new(dir.path());

        // Path filter to subdir with max_matches 5
        let res = executor.execute("grep_search", &serde_json::json!({
            "pattern": "target_match_word",
            "path_filter": "subdir",
            "max_matches": 5
        })).await.unwrap();

        assert!(res.contains("subdir"));
        assert!(!res.contains("outside.txt"));
        assert!(res.contains("Capped at 5 matches"));
    }

    #[tokio::test]
    async fn test_read_artifact_by_hash_and_tamper_detection() {
        let dir = tempfile::tempdir().unwrap();
        let art_dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(dume_store::ArtifactStore::new(art_dir.path()).unwrap());

        let raw_data = b"Arbitrary large binary or text payload to be tested via artifact store";
        let hash = store.save_artifact(raw_data).unwrap();

        let executor = LocalToolExecutor::new(dir.path())
            .with_artifact_store(Some(store.clone()));

        // 1. Read by valid artifact ID
        let res = executor.execute("read_artifact", &serde_json::json!({
            "path": hash,
            "offset": 0,
            "length": 20
        })).await.unwrap();
        assert!(res.contains(&format!("[Artifact: {} | Offset: 0 | Read: 20 bytes", hash)));
        assert!(res.contains("Arbitrary large bina"));

        // 2. Tampered content detection
        let shard = &hash[..2];
        let file_path = art_dir.path().join(shard).join(&hash);
        std::fs::write(&file_path, b"Tampered data completely different").unwrap();

        let err = executor.execute("read_artifact", &serde_json::json!({
            "path": hash
        })).await;
        assert!(err.is_err());
        let err_msg = format!("{:#}", err.unwrap_err());
        assert!(err_msg.contains("Artifact corruption detected"));

        // 3. Fallback to worktree file MUST NOT happen for 64-hex hash
        let fake_hash = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        // Create a file in worktree with the same name as fake_hash
        std::fs::write(dir.path().join(fake_hash), b"malicious worktree content").unwrap();
        let fake_err = executor.execute("read_artifact", &serde_json::json!({
            "path": fake_hash
        })).await;
        // Must fail with NotFound from ArtifactStore, NOT return the worktree file!
        assert!(fake_err.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn artifact_id_reader_rejects_external_symlink_targets() {
        use std::os::unix::fs::symlink;

        let worktree = tempfile::tempdir().unwrap();
        let artifacts = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(dume_store::ArtifactStore::new(artifacts.path()).unwrap());
        let hash = store.save_artifact(b"verified data").unwrap();
        let artifact_path = artifacts.path().join(&hash[..2]).join(&hash);
        std::fs::remove_file(&artifact_path).unwrap();
        let outside_file = outside.path().join("target");
        std::fs::write(&outside_file, b"verified data").unwrap();
        symlink(&outside_file, &artifact_path).unwrap();
        assert!(!store.has_artifact(&hash));

        let executor = LocalToolExecutor::new(worktree.path())
            .with_artifact_store(Some(store));
        let error = executor
            .execute("read_artifact", &serde_json::json!({"path": hash}))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("Artifact must be a regular file"));
    }

    #[tokio::test]
    async fn test_symlink_containment_in_tools() {
        let dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let secret_file = outside_dir.path().join("secret.txt");
        std::fs::write(&secret_file, b"secret content").unwrap();

        #[cfg(unix)]
        {
            let symlink_path = dir.path().join("link_to_secret");
            std::os::unix::fs::symlink(&secret_file, &symlink_path).unwrap();

            let executor = LocalToolExecutor::new(dir.path());
            let err = executor.execute("read_artifact", &serde_json::json!({
                "path": "link_to_secret"
            })).await;
            assert!(err.is_err());
            assert!(err.unwrap_err().to_string().contains("Security violation"));
        }
    }

    #[tokio::test]
    async fn test_bash_preserves_large_output_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let art_dir = tempfile::tempdir().unwrap();
        let store = dume_store::ArtifactStore::new(art_dir.path()).unwrap();

        let executor = LocalToolExecutor::new(dir.path())
            .with_artifact_store(Some(std::sync::Arc::new(store.clone())));

        // Produce >70KB output from bash
        let res = executor.execute("bash", &serde_json::json!({
            "command": "python3 -c 'print(\"A\" * 71680)' 2>/dev/null || printf '%*s' 71680 | tr ' ' 'A'"
        })).await.unwrap();

        // Raw output from executor must NOT be truncated before admission!
        assert!(res.len() >= 71680);

        // Admitting tool output with store
        let admitted = crate::output_limits::admit_tool_output("bash", &res, Some(&store));
        assert!(admitted.contains("Output Ceiling Exceeded for tool 'bash'"));
        assert!(admitted.contains("Full raw output persisted to Artifact ID:"));

        // Extract artifact ID and verify retrieval
        let marker = "Artifact ID: ";
        let id_start = admitted.find(marker).unwrap() + marker.len();
        let artifact_id = &admitted[id_start..id_start + 64];

        let retrieved = executor.execute("read_artifact", &serde_json::json!({
            "path": artifact_id,
            "offset": 0,
            "length": 100
        })).await.unwrap();

        assert!(retrieved.contains(artifact_id));
        assert!(retrieved.contains("AAAAA"));
    }
}

impl LocalToolExecutor {
    pub fn new(worktree_path: impl Into<PathBuf>) -> Self {
        Self {
            worktree_path: worktree_path.into(),
            cancellation: CancellationToken::new(),
            artifact_store: None,
        }
    }

    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    pub fn with_artifact_store(
        mut self,
        artifact_store: Option<std::sync::Arc<dume_store::ArtifactStore>>,
    ) -> Self {
        self.artifact_store = artifact_store;
        self
    }

    pub async fn execute(&self, name: &str, args: &Value) -> Result<String> {
        anyhow::ensure!(
            !self.cancellation.is_cancelled(),
            "Tool execution cancelled"
        );
        match name {
            "bash" => {
                let cmd_str = args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .context("Missing 'command' argument for bash")?;
                self.run_bash(cmd_str).await
            }
            "read_file" => {
                let path_str = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .context("Missing 'path' argument for read_file")?;
                let start_line = args.get("start_line").and_then(|v| v.as_u64()).map(|n| n as usize);
                let end_line = args.get("end_line").and_then(|v| v.as_u64()).map(|n| n as usize);
                self.read_file(path_str, start_line, end_line).await
            }
            "write_file" => {
                let path_str = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .context("Missing 'path' argument for write_file")?;
                let content = args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .context("Missing 'content' argument for write_file")?;
                self.write_file(path_str, content).await
            }
            "replace_file_content" => {
                let path_str = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .context("Missing 'path' argument for replace_file_content")?;
                let target = args
                    .get("target")
                    .and_then(|v| v.as_str())
                    .context("Missing 'target' argument for replace_file_content")?;
                let replacement = args
                    .get("replacement")
                    .and_then(|v| v.as_str())
                    .context("Missing 'replacement' argument for replace_file_content")?;
                self.replace_file_content(path_str, target, replacement)
                    .await
            }
            "grep_search" => {
                let pattern = args
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .context("Missing 'pattern' argument for grep_search")?;
                let path_filter = args.get("path_filter").and_then(|v| v.as_str());
                let max_matches = args.get("max_matches").and_then(|v| v.as_u64()).map(|n| n as usize);
                self.grep_search(pattern, path_filter, max_matches).await
            }
            "read_artifact" => {
                let path_str = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .context("Missing 'path' argument for read_artifact")?;
                let offset = args.get("offset").and_then(|v| v.as_u64()).map(|n| n as usize);
                let length = args.get("length").and_then(|v| v.as_u64()).map(|n| n as usize);
                self.read_artifact(path_str, offset, length).await
            }
            _ => anyhow::bail!("Unknown tool: {}", name),
        }
    }

    async fn run_bash(&self, command_str: &str) -> Result<String> {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(command_str)
            .current_dir(&self.worktree_path);
        let output = self.run_command(&mut command).await?;

        let stdout_raw = String::from_utf8_lossy(&output.stdout);
        let stderr_raw = String::from_utf8_lossy(&output.stderr);
        let status = output.status.code().unwrap_or(-1);

        Ok(format!(
            "Exit code: {}\nStdout:\n{}\nStderr:\n{}",
            status, stdout_raw, stderr_raw
        ))
    }

    async fn run_command(&self, command: &mut Command) -> Result<std::process::Output> {
        anyhow::ensure!(
            !self.cancellation.is_cancelled(),
            "Tool execution cancelled"
        );
        command
            .kill_on_drop(true)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);
        let child = command.spawn().context("Failed to spawn tool process")?;
        let pid = child.id().context("Tool process has no ID")?;
        let output = child.wait_with_output();
        tokio::pin!(output);
        tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => {
                #[cfg(unix)]
                {
                    let terminate_group = async {
                        let mut signalled = false;
                        loop {
                            // A descendant racing a fork can miss one group signal.
                            // Keep killing until the group no longer exists, not just
                            // until its leader exits or its output pipes close.
                            if unsafe { libc::kill(-(pid as i32), libc::SIGKILL) } != 0 {
                                let error = std::io::Error::last_os_error();
                                if error.raw_os_error() == Some(libc::ESRCH) {
                                    return Ok::<(), anyhow::Error>(());
                                }
                                // Darwin can reject a repeated SIGKILL while the
                                // successfully signalled group is exiting. Retry
                                // within the deadline, never treating EPERM as exit.
                                if !signalled || error.raw_os_error() != Some(libc::EPERM) {
                                    return Err(error).context("Failed to terminate cancelled tool process group");
                                }
                            } else {
                                signalled = true;
                            }
                            tokio::task::yield_now().await;
                        }
                    };
                    // Reap the leader concurrently so its zombie cannot keep the
                    // process group alive while termination waits for ESRCH.
                    tokio::time::timeout(std::time::Duration::from_secs(5), async {
                        tokio::try_join!(terminate_group, async {
                            output.await.context("Failed to reap cancelled tool process")
                        })
                    }).await.context("Timed out terminating cancelled tool process group")??;
                }
                #[cfg(not(unix))]
                {
                    let status = Command::new("taskkill")
                        .args(["/F", "/T", "/PID", &pid.to_string()])
                        .status().await.context("Failed to terminate cancelled tool process tree")?;
                    anyhow::ensure!(status.success(), "Failed to terminate cancelled tool process tree");
                    output.await.context("Failed to reap cancelled tool process")?;
                }
                anyhow::bail!("Tool execution cancelled");
            }
            result = &mut output => Ok(result?),
        }
    }

    async fn read_file(
        &self,
        rel_path: &str,
        start_line: Option<usize>,
        end_line: Option<usize>,
    ) -> Result<String> {
        let full_path = self.resolve_path(rel_path)?;
        let content = tokio::fs::read_to_string(&full_path)
            .await
            .with_context(|| format!("Failed to read file: {}", full_path.display()))?;

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        if total_lines == 0 {
            return Ok(String::new());
        }

        // If no range specified and line count is within limits, return exact content
        if start_line.is_none() && end_line.is_none() && total_lines <= DEFAULT_MAX_LINES {
            let (bounded, _) = truncate_output_head_tail(
                &content,
                DEFAULT_MAX_OUTPUT_BYTES,
                DEFAULT_HEAD_LINES,
                DEFAULT_TAIL_LINES,
            );
            return Ok(bounded);
        }

        let start = start_line.unwrap_or(1).max(1);
        if start > total_lines {
            return Ok(format!(
                "[Specified start_line {} exceeds total line count {}]",
                start, total_lines
            ));
        }

        let end = end_line
            .unwrap_or_else(|| (start + DEFAULT_MAX_LINES - 1).min(total_lines))
            .min(total_lines);

        if start > end {
            return Ok(format!(
                "[Invalid range: start_line {} is greater than end_line {}]",
                start, end
            ));
        }

        let mut output = String::new();
        output.push_str(&format!(
            "[Showing lines {} to {} of {} total lines]\n",
            start, end, total_lines
        ));

        for (idx, line) in lines[(start - 1)..end].iter().enumerate() {
            output.push_str(&format!("{}: {}\n", start + idx, line));
        }

        Ok(output)
    }

    async fn write_file(&self, rel_path: &str, content: &str) -> Result<String> {
        let full_path = self.resolve_path(rel_path)?;
        if let Some(parent) = full_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&full_path, content)
            .await
            .with_context(|| format!("Failed to write file: {}", full_path.display()))?;
        Ok(format!(
            "Successfully wrote {} bytes to {}",
            content.len(),
            rel_path
        ))
    }

    async fn replace_file_content(
        &self,
        rel_path: &str,
        target: &str,
        replacement: &str,
    ) -> Result<String> {
        let full_path = self.resolve_path(rel_path)?;
        let content = tokio::fs::read_to_string(&full_path)
            .await
            .with_context(|| format!("Failed to read file for replace: {}", full_path.display()))?;

        if !content.contains(target) {
            anyhow::bail!("Target content not found in file: {}", rel_path);
        }

        let new_content = content.replacen(target, replacement, 1);
        tokio::fs::write(&full_path, new_content)
            .await
            .with_context(|| format!("Failed to write modified file: {}", full_path.display()))?;

        Ok(format!("Successfully replaced content in {}", rel_path))
    }

    async fn grep_search(
        &self,
        pattern: &str,
        path_filter: Option<&str>,
        max_matches: Option<usize>,
    ) -> Result<String> {
        let limit = max_matches.unwrap_or(DEFAULT_MAX_MATCHES);
        let mut git_args = vec!["grep", "-n", "--", pattern];
        if let Some(pf) = path_filter {
            if !pf.trim().is_empty() {
                git_args.push(pf.trim());
            }
        }

        let mut command = Command::new("git");
        command.args(&git_args).current_dir(&self.worktree_path);
        let output = self.run_command(&mut command).await;

        match output {
            Ok(out) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let lines: Vec<&str> = stdout.lines().collect();
                if lines.len() <= limit {
                    Ok(stdout.to_string())
                } else {
                    let mut capped = lines[..limit].join("\n");
                    capped.push_str(&format!(
                        "\n... [Capped at {} matches out of {} total matches] ...",
                        limit,
                        lines.len()
                    ));
                    Ok(capped)
                }
            }
            Ok(out) if out.status.code() == Some(1) => Ok("No matches found".to_string()),
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                if stderr.contains("not a git repository") {
                    self.grep_fallback(pattern, path_filter, limit).await
                } else {
                    anyhow::bail!("git grep error (exit code {:?}): {}", out.status.code(), stderr);
                }
            }
            Err(e) => {
                self.grep_fallback(pattern, path_filter, limit)
                    .await
                    .with_context(|| format!("git grep failed: {:#}", e))
            }
        }
    }

    async fn grep_fallback(
        &self,
        pattern: &str,
        path_filter: Option<&str>,
        limit: usize,
    ) -> Result<String> {
        let search_path = if let Some(pf) = path_filter {
            self.resolve_path(pf)?.to_string_lossy().to_string()
        } else {
            ".".to_string()
        };

        let mut command = Command::new("grep");
        command
            .args(["-rn", pattern, &search_path])
            .current_dir(&self.worktree_path);
        let sh_out = self
            .run_command(&mut command)
            .await
            .context("Failed to run fallback grep search")?;

        if sh_out.status.success() {
            let text = String::from_utf8_lossy(&sh_out.stdout);
            let lines: Vec<&str> = text.lines().collect();
            if lines.len() <= limit {
                Ok(text.to_string())
            } else {
                let mut capped = lines[..limit].join("\n");
                capped.push_str(&format!(
                    "\n... [Capped at {} matches out of {} total matches] ...",
                    limit,
                    lines.len()
                ));
                Ok(capped)
            }
        } else if sh_out.status.code() == Some(1) {
            Ok("No matches found".to_string())
        } else {
            let err = String::from_utf8_lossy(&sh_out.stderr);
            anyhow::bail!("grep failed (code {:?}): {}", sh_out.status.code(), err);
        }
    }

    async fn read_artifact(
        &self,
        rel_path: &str,
        offset: Option<usize>,
        length: Option<usize>,
    ) -> Result<String> {
        let trimmed = rel_path.trim();
        let off = offset.unwrap_or(0);
        let max_len = length.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES).min(DEFAULT_MAX_OUTPUT_BYTES);

        // Check if rel_path is a 64-character SHA-256 artifact ID
        if trimmed.len() == 64 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
            let store = self.artifact_store.as_ref().context(
                "No ArtifactStore configured to read artifact by ID"
            )?;
            let store_clone = store.clone();
            let hash = trimmed.to_string();
            let (slice, total_bytes) = tokio::task::spawn_blocking(move || {
                store_clone.get_artifact_range_with_total(&hash, off, max_len)
            })
            .await
            .context("Task join error reading artifact")?
            .with_context(|| format!("Failed to read artifact ID '{}'", trimmed))?;

            let read_bytes = slice.len();
            let content_str = String::from_utf8_lossy(&slice).to_string();
            let header = format!(
                "[Artifact: {} | Offset: {} | Read: {} bytes | Total: {} bytes]\n",
                trimmed, off, read_bytes, total_bytes
            );
            return Ok(format!("{}{}", header, content_str));
        }

        // Otherwise, read as a normal worktree-relative file with path containment
        let full_path = self.resolve_path(rel_path)?;
        use tokio::io::AsyncReadExt;
        let mut file = tokio::fs::File::open(&full_path)
            .await
            .with_context(|| format!("Failed to open file: {}", full_path.display()))?;
        let meta = file
            .metadata()
            .await
            .with_context(|| format!("Failed to get metadata for file: {}", full_path.display()))?;
        let total_bytes = meta.len() as usize;

        if off >= total_bytes {
            let header = format!(
                "[Artifact: {} | Offset: {} | Read: 0 bytes | Total: {} bytes]\n",
                rel_path, off, total_bytes
            );
            return Ok(header);
        }

        use tokio::io::AsyncSeekExt;
        file.seek(std::io::SeekFrom::Start(off as u64)).await?;
        let to_read = max_len.min(total_bytes - off);
        let mut slice = vec![0u8; to_read];
        file.read_exact(&mut slice).await?;

        let content_str = String::from_utf8_lossy(&slice).to_string();
        let header = format!(
            "[Artifact: {} | Offset: {} | Read: {} bytes | Total: {} bytes]\n",
            rel_path, off, to_read, total_bytes
        );
        Ok(format!("{}{}", header, content_str))
    }

    fn resolve_path(&self, rel_path: &str) -> Result<PathBuf> {
        let clean = rel_path.trim_start_matches('/');

        // Canonical base directory
        let canonical_base = std::fs::canonicalize(&self.worktree_path)
            .unwrap_or_else(|_| self.worktree_path.clone());

        // Build normalized path starting from canonical_base
        let mut normalized = canonical_base.clone();
        for comp in std::path::Path::new(clean).components() {
            match comp {
                std::path::Component::ParentDir => {
                    if normalized == canonical_base || !normalized.pop() {
                        anyhow::bail!("Security violation: Path outside worktree: {}", rel_path);
                    }
                }
                std::path::Component::CurDir => {}
                std::path::Component::Normal(c) => normalized.push(c),
                _ => anyhow::bail!("Security violation: Invalid path component in {}", rel_path),
            }
        }

        // If target or any existing component is a symlink, verify canonical target remains within canonical_base
        if normalized.exists() || normalized.is_symlink() {
            if let Ok(canon) = std::fs::canonicalize(&normalized) {
                if !canon.starts_with(&canonical_base) {
                    anyhow::bail!(
                        "Security violation: Symlink or path outside worktree: {}",
                        rel_path
                    );
                }
            }
        } else {
            let mut parent_check = normalized.parent();
            while let Some(p) = parent_check {
                if p.exists() {
                    if let Ok(canon_p) = std::fs::canonicalize(p) {
                        if !canon_p.starts_with(&canonical_base) {
                            anyhow::bail!(
                                "Security violation: Parent outside worktree: {}",
                                rel_path
                            );
                        }
                    }
                    break;
                }
                parent_check = p.parent();
            }
        }

        if !normalized.starts_with(&canonical_base) {
            anyhow::bail!("Security violation: Target outside worktree: {}", rel_path);
        }

        Ok(normalized)
    }
}

use crate::types::*;
use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::AtomicI32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};

const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 16 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

pub struct McpClient {
    child: Arc<Mutex<Child>>,
    stdin_tx: mpsc::Sender<String>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
    next_id: AtomicU64,
    timeout: Duration,
    stderr: Arc<Mutex<Vec<u8>>>,
    closed: Arc<tokio::sync::Notify>,
    notifications: broadcast::Sender<JsonRpcNotification>,
    #[cfg(unix)]
    process_group: AtomicI32,
}

impl McpClient {
    pub async fn spawn(command: &str, args: &[&str]) -> Result<Self> {
        Self::spawn_with_options(command, args, None, None, DEFAULT_TIMEOUT).await
    }

    pub async fn spawn_with_options(
        command: &str,
        args: &[&str],
        cwd: Option<&std::path::Path>,
        env: Option<&[(String, String)]>,
        timeout: Duration,
    ) -> Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.as_std_mut().process_group(0);
        }
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        if let Some(env) = env {
            cmd.envs(env.iter().cloned());
        }
        let mut child = cmd.spawn().context("failed to spawn MCP server")?;
        let stdin = child.stdin.take().context("MCP stdin unavailable")?;
        let stdout = child.stdout.take().context("MCP stdout unavailable")?;
        let stderr_pipe = child.stderr.take().context("MCP stderr unavailable")?;
        let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(32);
        let pending = Arc::new(Mutex::new(
            HashMap::<u64, oneshot::Sender<JsonRpcResponse>>::new(),
        ));
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let closed = Arc::new(tokio::sync::Notify::new());
        let (notification_tx, _) = broadcast::channel(32);
        let reader_notifications = notification_tx.clone();
        let writer_pending = pending.clone();
        let writer_closed = closed.clone();
        tokio::spawn(async move {
            let mut writer = stdin;
            while let Some(line) = stdin_rx.recv().await {
                if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err()
                {
                    break;
                }
            }
            let mut p = writer_pending.lock().await;
            p.clear();
            writer_closed.notify_waiters();
        });
        let reader_pending = pending.clone();
        let reader_closed = closed.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut frame = Vec::new();
                let read = tokio::time::timeout(
                    Duration::from_secs(3600),
                    read_bounded_frame(&mut reader, &mut frame),
                )
                .await;
                match read {
                    Ok(Ok(None)) | Err(_) | Ok(Err(_)) => break,
                    Ok(Ok(Some(true))) => continue,
                    Ok(Ok(Some(false))) => {
                        if let Ok(resp) = serde_json::from_slice::<JsonRpcResponse>(&frame) {
                            if let Some(tx) = reader_pending.lock().await.remove(&resp.id) {
                                let _ = tx.send(resp);
                            }
                        } else if let Ok(notification) =
                            serde_json::from_slice::<JsonRpcNotification>(&frame)
                        {
                            tracing::debug!(method=%notification.method, "MCP notification received");
                            let _ = reader_notifications.send(notification);
                        }
                    }
                }
            }
            let mut p = reader_pending.lock().await;
            p.clear();
            reader_closed.notify_waiters();
        });
        let stderr_capture = stderr.clone();
        tokio::spawn(async move {
            let mut r = BufReader::new(stderr_pipe);
            let mut chunk = [0u8; 1024];
            loop {
                use tokio::io::AsyncReadExt;
                match r.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut out = stderr_capture.lock().await;
                        out.extend_from_slice(&chunk[..n]);
                        if out.len() > MAX_STDERR_BYTES {
                            let excess = out.len() - MAX_STDERR_BYTES;
                            out.drain(..excess);
                        }
                    }
                }
            }
        });
        #[cfg(unix)]
        let process_group = AtomicI32::new(child.id().unwrap_or_default() as i32);
        let client = Self {
            child: Arc::new(Mutex::new(child)),
            stdin_tx,
            pending,
            next_id: AtomicU64::new(1),
            timeout,
            stderr,
            closed,
            notifications: notification_tx,
            #[cfg(unix)]
            process_group,
        };
        if let Err(e) = client.initialize().await {
            client.shutdown().await;
            return Err(e);
        }
        Ok(client)
    }

    pub async fn stderr_tail(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().await).into_owned()
    }

    pub fn subscribe_notifications(&self) -> broadcast::Receiver<JsonRpcNotification> {
        self.notifications.subscribe()
    }

    pub async fn send_request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<JsonRpcResponse> {
        self.send_request_timeout(method, params, self.timeout)
            .await
    }

    pub async fn send_request_timeout(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<JsonRpcResponse> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id,
            method: method.into(),
            params,
        };
        let line = serde_json::to_string(&request)? + "\n";
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        if self.stdin_tx.send(line).await.is_err() {
            self.pending.lock().await.remove(&id);
            return Err(anyhow!("MCP stdin writer closed"));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&id);
                Err(anyhow!("MCP response channel closed"))
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                let _ = self.cancel_request(id, "request timed out").await;
                Err(anyhow!("MCP request {method} timed out"))
            }
        }
    }

    pub async fn initialize(&self) -> Result<()> {
        let response = self.send_request("initialize", Some(json!({"protocolVersion":"2024-11-05","clientInfo":{"name":"dume-mcp","version":"0.1.0"},"capabilities":{"tools":{}}}))).await?;
        ensure_response(response, "initialize")?;
        self.send_notification("notifications/initialized", None)
            .await
    }

    pub async fn list_tools(&self) -> Result<Vec<McpTool>> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = std::collections::HashSet::new();
        loop {
            let params = cursor.as_ref().map(|c| json!({"cursor":c}));
            let response =
                ensure_response(self.send_request("tools/list", params).await?, "tools/list")?;
            let result = response.result.context("empty tools/list result")?;
            let tools: Vec<McpTool> =
                serde_json::from_value(result.get("tools").cloned().context("missing tools")?)?;
            all.extend(tools);
            anyhow::ensure!(
                all.len() <= 4096,
                "MCP tools/list exceeded the 4096 tool limit"
            );
            cursor = result
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if cursor.as_ref().is_none_or(|c| !seen.insert(c.clone())) {
                break;
            }
        }
        Ok(all)
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<CallToolResult> {
        let response = ensure_response(
            self.send_request(
                "tools/call",
                Some(json!({"name":name,"arguments":arguments})),
            )
            .await?,
            "tools/call",
        )?;
        Ok(serde_json::from_value(
            response.result.context("empty tools/call result")?,
        )?)
    }

    async fn send_notification(&self, method: &str, params: Option<Value>) -> Result<()> {
        let note = JsonRpcNotification {
            jsonrpc: "2.0".into(),
            method: method.into(),
            params,
        };
        let line = serde_json::to_string(&note)? + "\n";
        self.stdin_tx
            .send(line)
            .await
            .context("MCP stdin writer closed")?;
        Ok(())
    }

    pub async fn cancel_request(&self, request_id: u64, reason: &str) -> Result<()> {
        self.send_notification(
            "notifications/cancelled",
            Some(json!({"requestId":request_id,"reason":reason})),
        )
        .await
    }

    pub async fn shutdown(&self) {
        self.pending.lock().await.clear();
        let _ = self.send_notification("exit", None).await;
        let mut child = self.child.lock().await;
        #[cfg(unix)]
        let pid = self.process_group.swap(0, Ordering::AcqRel);
        if child.id().is_none() {
            #[cfg(unix)]
            if pid > 0 {
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
            return;
        }
        let grace = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;
        if !matches!(grace, Ok(Ok(_))) {
            #[cfg(unix)]
            if pid > 0 {
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        #[cfg(unix)]
        if pid > 0 {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
        self.closed.notify_waiters();
    }
}

// Reads one newline-delimited frame without ever retaining more than the configured cap.
// Oversize lines are fully discarded so their suffix cannot be mistaken for a response.
async fn read_bounded_frame<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    frame: &mut Vec<u8>,
) -> std::io::Result<Option<bool>> {
    use tokio::io::AsyncBufReadExt;
    let mut oversized = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(None);
        }
        let newline = available.iter().position(|b| *b == b'\n');
        let count = newline.map_or(available.len(), |n| n + 1);
        if !oversized {
            if frame.len().saturating_add(count) > MAX_FRAME_BYTES {
                frame.clear();
                oversized = true;
            } else {
                frame.extend_from_slice(&available[..count]);
            }
        }
        reader.consume(count);
        if newline.is_some() {
            return Ok(Some(oversized));
        }
    }
}

fn ensure_response(response: JsonRpcResponse, method: &str) -> Result<JsonRpcResponse> {
    if let Some(error) = response.error {
        anyhow::bail!(
            "MCP {method} error: {} (code {})",
            error.message,
            error.code
        );
    }
    Ok(response)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn fixture(_mode: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.sh");
        let script = r#"#!/bin/sh
mode="$1"
while IFS= read -r line; do
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  case "$line" in *'"id":'*) id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p');; *) continue;; esac
  case "$method" in
    initialize) if [ "$mode" = group ]; then sleep 30 & echo $! > "$MCP_CHILD_PID_FILE"; fi; printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}\n' "$id";;
    tools/list)
      case "$mode" in
        paginate) case "$line" in *'"cursor":"next"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"second","inputSchema":{"type":"object"}}]}}\n' "$id";; *) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"first","inputSchema":{"type":"object"}}],"nextCursor":"next"}}\n' "$id";; esac;;
        oversize) printf '{"jsonrpc":"2.0","id":%s,"result":{"padding":"' "$id"; i=0; while [ "$i" -lt 9000000 ]; do printf x; i=$((i+1)); done; printf '"}}\n';;
        eof) exit 0;;
      esac;;
    tools/call) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"called"}],"isError":false}}\n' "$id";;
    slow) sleep 2; printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id";;
    *) :;;
  esac
done
"#;
        std::fs::write(&path, script).unwrap();
        (dir, path.to_string_lossy().into_owned())
    }

    async fn spawn(script: &str, mode: &str) -> McpClient {
        McpClient::spawn_with_options(
            "/bin/sh",
            &[script, mode],
            None,
            None,
            Duration::from_secs(3),
        )
        .await
        .unwrap()
    }

    fn process_has_exited(pid: i32) -> Result<bool> {
        if unsafe { libc::kill(pid, 0) } == -1 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(true);
            }
            return Err(error).context("Failed to probe descendant process");
        }
        #[cfg(target_os = "linux")]
        {
            // An orphan can remain a zombie until init reaps it; PID existence
            // alone does not mean it can still execute.
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Ok(stat) => {
                    let state = stat
                        .rsplit_once(')')
                        .and_then(|(_, fields)| fields.split_whitespace().next())
                        .context("Missing descendant process state")?;
                    Ok(matches!(state, "Z" | "X"))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
                Err(error) => Err(error).context("Failed to read descendant process state"),
            }
        }
        #[cfg(not(target_os = "linux"))]
        Ok(false)
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn exited_process_is_detected_before_its_zombie_is_reaped() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let exited = tokio::time::timeout(Duration::from_secs(2), async {
            while !process_has_exited(pid).unwrap() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        let probe = unsafe { libc::kill(pid, 0) };
        let status = child.wait().unwrap();
        assert!(status.success());
        assert!(exited.is_ok(), "Exited child was not detected as a zombie");
        assert_eq!(probe, 0, "Child must still have a PID before wait reaps it");
    }

    #[tokio::test]
    async fn stdio_initialize_paginated_list_and_call() {
        let (_dir, script) = fixture("paginate");
        let client = spawn(&script, "paginate").await;
        let tools = client.list_tools().await.unwrap();
        assert_eq!(
            tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["first", "second"]
        );
        let result = client.call_tool("first", json!({"x":1})).await.unwrap();
        assert_eq!(result.content[0].text.as_deref(), Some("called"));
        client.shutdown().await;
    }

    #[tokio::test]
    async fn stdio_timeout_eof_and_oversize_are_handled() {
        let (_dir, script) = fixture("paginate");
        let client = spawn(&script, "paginate").await;
        assert!(
            client
                .send_request_timeout("slow", None, Duration::from_millis(30))
                .await
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );
        client.shutdown().await;

        let (_dir, script) = fixture("eof");
        let client = spawn(&script, "eof").await;
        assert!(client.list_tools().await.is_err());
        client.shutdown().await;

        let (_dir, script) = fixture("oversize");
        let client = spawn(&script, "oversize").await;
        assert!(
            tokio::time::timeout(Duration::from_secs(5), client.list_tools())
                .await
                .unwrap()
                .is_err()
        );
        client.shutdown().await;
    }

    #[tokio::test]
    async fn explicit_shutdown_kills_descendants_and_reaps_child() {
        let (dir, script) = fixture("group");
        let pid_file = dir.path().join("child.pid");
        let env = vec![(
            "MCP_CHILD_PID_FILE".into(),
            pid_file.to_string_lossy().into_owned(),
        )];
        let client = McpClient::spawn_with_options(
            "/bin/sh",
            &[&script, "group"],
            None,
            Some(&env),
            Duration::from_secs(3),
        )
        .await
        .unwrap();
        let descendant: i32 = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(value) = std::fs::read_to_string(&pid_file) {
                    if let Ok(pid) = value.trim().parse() {
                        break pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(!process_has_exited(descendant).unwrap());
        client.shutdown().await;
        assert!(client.child.lock().await.id().is_none());
        tokio::time::timeout(Duration::from_secs(2), async {
            while !process_has_exited(descendant).unwrap() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("MCP descendant is still running after shutdown");
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let pid = self.process_group.swap(0, Ordering::AcqRel);
            if pid > 0 {
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
        }
    }
}

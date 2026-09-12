use crate::types::*;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

pub struct McpClient {
    _child: Arc<Mutex<Child>>,
    stdin_tx: mpsc::Sender<String>,
    pending_requests: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
    next_id: AtomicU64,
}

impl McpClient {
    pub async fn spawn(command: &str, args: &[&str]) -> Result<Self> {
        let mut child = Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("Failed to spawn MCP server process")?;

        let stdin = child.stdin.take().context("Failed to open stdin")?;
        let stdout = child.stdout.take().context("Failed to open stdout")?;

        let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(32);

        // Stdin writer task
        tokio::spawn(async move {
            let mut writer = stdin;
            while let Some(line) = stdin_rx.recv().await {
                if writer.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                let _ = writer.flush().await;
            }
        });

        let pending = Arc::new(Mutex::new(HashMap::<u64, oneshot::Sender<JsonRpcResponse>>::new()));
        let pending_clone = Arc::clone(&pending);

        // Stdout reader task
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(trimmed) {
                    let mut lock = pending_clone.lock().await;
                    if let Some(tx) = lock.remove(&resp.id) {
                        let _ = tx.send(resp);
                    }
                }
            }
        });

        let client = Self {
            _child: Arc::new(Mutex::new(child)),
            stdin_tx,
            pending_requests: pending,
            next_id: AtomicU64::new(1),
        };

        // Initialize handshake
        client.initialize().await?;

        Ok(client)
    }

    pub async fn send_request(&self, method: &str, params: Option<Value>) -> Result<JsonRpcResponse> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_string(),
            params,
        };

        let serialized = serde_json::to_string(&req)? + "\n";
        let (tx, rx) = oneshot::channel();

        {
            let mut lock = self.pending_requests.lock().await;
            lock.insert(id, tx);
        }

        self.stdin_tx
            .send(serialized)
            .await
            .context("Failed to write request to MCP server")?;

        let resp = rx.await.context("MCP response channel closed")?;
        Ok(resp)
    }

    pub async fn initialize(&self) -> Result<()> {
        let params = json!({
            "protocolVersion": "2024-11-05",
            "clientInfo": {
                "name": "dume-mcp",
                "version": "0.1.0"
            },
            "capabilities": {
                "tools": {}
            }
        });

        let resp = self.send_request("initialize", Some(params)).await?;
        if let Some(err) = resp.error {
            anyhow::bail!("MCP initialize error: {} (code: {})", err.message, err.code);
        }

        // Send initialized notification
        let notif = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/initialized".to_string(),
            params: None,
        };
        let notif_line = serde_json::to_string(&notif)? + "\n";
        self.stdin_tx.send(notif_line).await?;

        Ok(())
    }

    pub async fn list_tools(&self) -> Result<Vec<McpTool>> {
        let resp = self.send_request("tools/list", None).await?;
        if let Some(err) = resp.error {
            anyhow::bail!("MCP tools/list error: {}", err.message);
        }

        let result = resp.result.context("Empty tools/list result")?;
        let tools_val = result.get("tools").context("Missing 'tools' field")?;
        let tools: Vec<McpTool> = serde_json::from_value(tools_val.clone())?;
        Ok(tools)
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<CallToolResult> {
        let params = json!({
            "name": name,
            "arguments": arguments
        });

        let resp = self.send_request("tools/call", Some(params)).await?;
        if let Some(err) = resp.error {
            anyhow::bail!("MCP tools/call error: {}", err.message);
        }

        let result = resp.result.context("Empty tools/call result")?;
        let call_res: CallToolResult = serde_json::from_value(result)?;
        Ok(call_res)
    }

    pub async fn cancel_request(&self, request_id: u64, reason: &str) -> Result<()> {
        let notif = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/cancelled".to_string(),
            params: Some(json!({
                "requestId": request_id,
                "reason": reason
            })),
        };
        let line = serde_json::to_string(&notif)? + "\n";
        self.stdin_tx.send(line).await?;
        Ok(())
    }
}

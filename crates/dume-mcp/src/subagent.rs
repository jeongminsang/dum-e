use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentStatus {
    Running,
    Completed { result: String },
    Failed { error: String },
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct SubagentRecord {
    pub id: String,
    pub prompt: String,
    pub status: SubagentStatus,
    pub created_at: u64,
}

pub struct SubagentManager {
    subagents: Arc<Mutex<HashMap<String, (SubagentRecord, CancellationToken)>>>,
}

impl Default for SubagentManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SubagentManager {
    pub fn new() -> Self {
        Self {
            subagents: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn start_subagent<F, Fut>(&self, id: &str, prompt: &str, task_fn: F) -> Result<String>
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<String>> + Send + 'static,
    {
        let token = CancellationToken::new();
        let record = SubagentRecord {
            id: id.to_string(),
            prompt: prompt.to_string(),
            status: SubagentStatus::Running,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };

        {
            let mut lock = self.subagents.lock().await;
            lock.insert(id.to_string(), (record, token.clone()));
        }

        let map_clone = Arc::clone(&self.subagents);
        let subagent_id = id.to_string();

        tokio::spawn(async move {
            let res = task_fn(token).await;
            let mut lock = map_clone.lock().await;
            if let Some((rec, _)) = lock.get_mut(&subagent_id) {
                match res {
                    Ok(val) => rec.status = SubagentStatus::Completed { result: val },
                    Err(e) => rec.status = SubagentStatus::Failed { error: e.to_string() },
                }
            }
        });

        Ok(id.to_string())
    }

    pub async fn list_subagents(&self) -> Vec<SubagentRecord> {
        let lock = self.subagents.lock().await;
        lock.values().map(|(rec, _)| rec.clone()).collect()
    }

    pub async fn inspect_subagent(&self, id: &str) -> Option<SubagentRecord> {
        let lock = self.subagents.lock().await;
        lock.get(id).map(|(rec, _)| rec.clone())
    }

    pub async fn cancel_subagent(&self, id: &str) -> Result<()> {
        let mut lock = self.subagents.lock().await;
        let (rec, token) = lock.get_mut(id).context("Subagent not found")?;
        token.cancel();
        rec.status = SubagentStatus::Cancelled;
        Ok(())
    }

    pub async fn await_subagent(&self, id: &str, timeout_ms: u64) -> Result<SubagentStatus> {
        let start = tokio::time::Instant::now();
        let timeout = tokio::time::Duration::from_millis(timeout_ms);

        loop {
            if let Some(rec) = self.inspect_subagent(id).await {
                match rec.status {
                    SubagentStatus::Completed { .. }
                    | SubagentStatus::Failed { .. }
                    | SubagentStatus::Cancelled => return Ok(rec.status),
                    SubagentStatus::Running => {}
                }
            } else {
                anyhow::bail!("Subagent {} not found", id);
            }

            if start.elapsed() > timeout {
                anyhow::bail!("Timeout awaiting subagent {}", id);
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_subagent_lifecycle() {
        let mgr = SubagentManager::new();
        mgr.start_subagent("sub_1", "calculate 2+2", |_token| async move {
            Ok("4".to_string())
        })
        .await
        .unwrap();

        let status = mgr.await_subagent("sub_1", 1000).await.unwrap();
        assert_eq!(status, SubagentStatus::Completed { result: "4".to_string() });
    }

    #[tokio::test]
    async fn test_subagent_cancellation() {
        let mgr = SubagentManager::new();
        mgr.start_subagent("sub_cancel", "long task", |token| async move {
            tokio::select! {
                _ = token.cancelled() => Ok("cancelled early".to_string()),
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(10)) => Ok("done".to_string()),
            }
        })
        .await
        .unwrap();

        mgr.cancel_subagent("sub_cancel").await.unwrap();
        let rec = mgr.inspect_subagent("sub_cancel").await.unwrap();
        assert_eq!(rec.status, SubagentStatus::Cancelled);
    }
}

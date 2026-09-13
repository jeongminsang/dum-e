use dume_core::manifest::ResultManifest;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostToWorkerMessage {
    Start {
        attempt_id: String,
        task_id: String,
        coordinator_epoch: i64,
        worktree_path: String,
        test_command: Option<String>,
    },
    Cancel {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerToHostMessage {
    Heartbeat {
        attempt_id: String,
        epoch: i64,
        timestamp: i64,
    },
    Progress {
        attempt_id: String,
        message: String,
    },
    Completed {
        manifest: ResultManifest,
    },
    Failed {
        error: String,
    },
}

pub fn serialize_message<T: Serialize>(msg: &T) -> anyhow::Result<String> {
    let json = serde_json::to_string(msg)?;
    Ok(format!("{}\n", json))
}

pub fn deserialize_message<T: for<'de> Deserialize<'de>>(line: &str) -> anyhow::Result<T> {
    let msg = serde_json::from_str(line.trim())?;
    Ok(msg)
}

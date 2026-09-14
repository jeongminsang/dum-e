use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Pending,
    Active,
    Verifying,
    Completed,
    Failed,
    Cancelled,
    NeedsAttention,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Blocked,
    Ready,
    InProgress,
    Verifying,
    Completed,
    Failed,
    Cancelled,
    NeedsAttention,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    Running,
    ResultSubmitted,
    Accepted,
    Rejected,
    Lost,
    Cancelled,
    NeedsAttention,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalOperationStatus {
    IntentRecorded,
    Dispatched,
    Confirmed,
    OutcomeUnknown,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationStatus {
    Pending,
    Applied,
    Conflict,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinatorLock {
    pub coordinator_id: String,
    pub owner_id: Option<String>,
    pub epoch: i64,
    pub lease_expires_at: i64,
    pub heartbeat_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    pub id: String,
    pub description: String,
    pub status: GoalStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub goal_id: String,
    pub title: String,
    pub description: String,
    pub status: TaskStatus,
    pub dependencies: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    pub allowed_paths: Option<Vec<String>>,
    pub target_branch: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    pub id: String,
    pub task_id: String,
    pub coordinator_epoch: i64,
    pub worker_id: String,
    pub worktree_path: String,
    pub status: AttemptStatus,
    pub lease_expires_at: i64,
    pub heartbeat_at: i64,
    pub candidate_commit: Option<String>,
    pub manifest_hash: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verification {
    pub attempt_id: String,
    pub epoch: i64,
    pub passed: bool,
    pub allowed_paths_passed: bool,
    pub acceptance_command_passed: bool,
    pub manifest_hash: String,
    pub details: String,
    pub verified_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Integration {
    pub target_branch: String,
    pub base_commit: String,
    pub candidate_commit: String,
    pub integration_commit: Option<String>,
    pub status: IntegrationStatus,
    pub error_message: Option<String>,
    pub integrated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalOperation {
    pub id: String,
    pub attempt_id: String,
    pub idempotency_key: String,
    pub description: String,
    pub status: ExternalOperationStatus,
    pub receipt_data: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessEvent {
    pub sequence: i64,
    pub entity_id: String,
    pub event_type: String,
    pub epoch: i64,
    pub dedup_key: Option<String>,
    pub payload: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUsage {
    pub session_id: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub updated_at: i64,
}

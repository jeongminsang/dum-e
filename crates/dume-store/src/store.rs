use crate::artifact::ArtifactStore;
use crate::schema::initialize_schema;
use dume_core::types::*;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("Database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Lock acquisition failed for coordinator {coordinator_id}: currently held by {owner_id:?}")]
    LockHeld {
        coordinator_id: String,
        owner_id: Option<String>,
    },
    #[error("Epoch fencing violation: entity epoch {actual} != required {expected}")]
    FencingViolation { expected: i64, actual: i64 },
    #[error("Stale epoch submission: attempt epoch {attempt_epoch} is superceded by active coordinator epoch {current_coordinator_epoch}")]
    StaleEpoch { attempt_epoch: i64, current_coordinator_epoch: i64 },
    #[error("Entity not found: {0}")]
    NotFound(String),
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub struct HarnessStore {
    conn: Mutex<Connection>,
    pub artifacts: ArtifactStore,
    pub db_path: PathBuf,
}

impl HarnessStore {
    pub fn open<P: AsRef<Path>, A: AsRef<Path>>(db_path: P, artifacts_dir: A) -> Result<Self, StoreError> {
        let db_p = db_path.as_ref().to_path_buf();
        if let Some(parent) = db_p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&db_p)?;
        initialize_schema(&conn)?;

        let artifacts = ArtifactStore::new(artifacts_dir)?;
        Ok(Self {
            conn: Mutex::new(conn),
            artifacts,
            db_path: db_p,
        })
    }

    pub fn in_memory<A: AsRef<Path>>(artifacts_dir: A) -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory()?;
        initialize_schema(&conn)?;
        let artifacts = ArtifactStore::new(artifacts_dir)?;
        Ok(Self {
            conn: Mutex::new(conn),
            artifacts,
            db_path: PathBuf::from(":memory:"),
        })
    }

    // --- Coordinator Lock (Epoch Fencing) ---

    pub fn acquire_coordinator_lock(
        &self,
        coordinator_id: &str,
        owner_id: &str,
        ttl_ms: i64,
    ) -> Result<CoordinatorLock, StoreError> {
        let conn = self.conn.lock().unwrap();
        let now = now_millis();
        let expires_at = now + ttl_ms;

        let existing: Option<(Option<String>, i64, i64)> = conn
            .query_row(
                "SELECT owner_id, epoch, lease_expires_at FROM coordinator_locks WHERE coordinator_id = ?1",
                params![coordinator_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;

        match existing {
            None => {
                conn.execute(
                    "INSERT INTO coordinator_locks (coordinator_id, owner_id, epoch, lease_expires_at, heartbeat_at) VALUES (?1, ?2, 1, ?3, ?4)",
                    params![coordinator_id, owner_id, expires_at, now],
                )?;
                Ok(CoordinatorLock {
                    coordinator_id: coordinator_id.to_string(),
                    owner_id: Some(owner_id.to_string()),
                    epoch: 1,
                    lease_expires_at: expires_at,
                    heartbeat_at: now,
                })
            }
            Some((current_owner, epoch, lease_expires_at)) => {
                let can_acquire = match &current_owner {
                    None => true,
                    Some(owner) if owner == owner_id => true,
                    Some(_) => lease_expires_at < now,
                };

                if !can_acquire {
                    return Err(StoreError::LockHeld {
                        coordinator_id: coordinator_id.to_string(),
                        owner_id: current_owner,
                    });
                }

                let new_epoch = if current_owner.as_deref() == Some(owner_id) {
                    epoch // Keep epoch if same owner renewing
                } else {
                    epoch + 1 // Monotonically increment when taking over
                };

                conn.execute(
                    "UPDATE coordinator_locks SET owner_id = ?1, epoch = ?2, lease_expires_at = ?3, heartbeat_at = ?4 WHERE coordinator_id = ?5",
                    params![owner_id, new_epoch, expires_at, now, coordinator_id],
                )?;

                Ok(CoordinatorLock {
                    coordinator_id: coordinator_id.to_string(),
                    owner_id: Some(owner_id.to_string()),
                    epoch: new_epoch,
                    lease_expires_at: expires_at,
                    heartbeat_at: now,
                })
            }
        }
    }

    pub fn heartbeat_coordinator(&self, coordinator_id: &str, owner_id: &str, ttl_ms: i64) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        let now = now_millis();
        let expires_at = now + ttl_ms;

        let rows = conn.execute(
            "UPDATE coordinator_locks SET lease_expires_at = ?1, heartbeat_at = ?2 WHERE coordinator_id = ?3 AND owner_id = ?4",
            params![expires_at, now, coordinator_id, owner_id],
        )?;

        if rows == 0 {
            Err(StoreError::NotFound(format!("Active lock for coordinator {} and owner {}", coordinator_id, owner_id)))
        } else {
            Ok(())
        }
    }

    pub fn release_coordinator_lock(&self, coordinator_id: &str, owner_id: &str) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE coordinator_locks SET owner_id = NULL WHERE coordinator_id = ?1 AND owner_id = ?2",
            params![coordinator_id, owner_id],
        )?;
        Ok(())
    }

    pub fn force_expire_coordinator_lock(&self, coordinator_id: &str) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE coordinator_locks SET lease_expires_at = 0, owner_id = NULL WHERE coordinator_id = ?1",
            params![coordinator_id],
        )?;
        Ok(())
    }

    // --- Goal & Task Management ---

    pub fn create_goal(&self, id: &str, description: &str) -> Result<Goal, StoreError> {
        let conn = self.conn.lock().unwrap();
        let now = now_millis();
        conn.execute(
            "INSERT INTO goals (id, description, status, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, description, "pending", now, now],
        )?;
        Ok(Goal {
            id: id.to_string(),
            description: description.to_string(),
            status: GoalStatus::Pending,
            created_at: now,
            updated_at: now,
        })
    }

    pub fn list_active_goals(&self) -> Result<Vec<Goal>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, description, status, created_at, updated_at FROM goals WHERE status NOT IN ('completed', 'failed')",
        )?;
        let rows = stmt.query_map([], |row| {
            let status_str: String = row.get(2)?;
            let status: GoalStatus = serde_json::from_str(&format!("\"{}\"", status_str)).unwrap_or(GoalStatus::Pending);
            Ok(Goal {
                id: row.get(0)?,
                description: row.get(1)?,
                status,
                created_at: row.get(3)?,
                updated_at: row.get(4)?,
            })
        })?;
        let mut goals = Vec::new();
        for r in rows {
            goals.push(r?);
        }
        Ok(goals)
    }

    pub fn update_goal_status(&self, id: &str, status: GoalStatus) -> Result<(), StoreError> {

        let conn = self.conn.lock().unwrap();
        let now = now_millis();
        let status_str = serde_json::to_string(&status)?.trim_matches('"').to_string();
        conn.execute(
            "UPDATE goals SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![status_str, now, id],
        )?;
        Ok(())
    }

    pub fn create_task(&self, task: &Task) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        let now = now_millis();
        let deps = serde_json::to_string(&task.dependencies)?;
        let crit = serde_json::to_string(&task.acceptance_criteria)?;
        let paths = task.allowed_paths.as_ref().map(|p| serde_json::to_string(p)).transpose()?;
        let status_str = serde_json::to_string(&task.status)?.trim_matches('"').to_string();

        conn.execute(
            "INSERT INTO tasks (id, goal_id, title, description, status, dependencies_json, acceptance_criteria_json, allowed_paths_json, target_branch, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                task.id,
                task.goal_id,
                task.title,
                task.description,
                status_str,
                deps,
                crit,
                paths,
                task.target_branch,
                now,
                now
            ],
        )?;
        Ok(())
    }

    pub fn get_task(&self, id: &str) -> Result<Task, StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, goal_id, title, description, status, dependencies_json, acceptance_criteria_json, allowed_paths_json, target_branch, created_at, updated_at
             FROM tasks WHERE id = ?1",
            params![id],
            |row| {
                let status_str: String = row.get(4)?;
                let deps_str: String = row.get(5)?;
                let crit_str: String = row.get(6)?;
                let paths_str: Option<String> = row.get(7)?;

                let status: TaskStatus = serde_json::from_str(&format!("\"{}\"", status_str)).map_err(|e| rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e)))?;
                let dependencies: Vec<String> = serde_json::from_str(&deps_str).unwrap_or_default();
                let acceptance_criteria: Vec<String> = serde_json::from_str(&crit_str).unwrap_or_default();
                let allowed_paths: Option<Vec<String>> = paths_str.and_then(|s| serde_json::from_str(&s).ok());

                Ok(Task {
                    id: row.get(0)?,
                    goal_id: row.get(1)?,
                    title: row.get(2)?,
                    description: row.get(3)?,
                    status,
                    dependencies,
                    acceptance_criteria,
                    allowed_paths,
                    target_branch: row.get(8)?,
                    created_at: row.get(9)?,
                    updated_at: row.get(10)?,
                })
            },
        ).map_err(StoreError::Db)
    }

    pub fn update_task_status(&self, id: &str, status: TaskStatus) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        let now = now_millis();
        let status_str = serde_json::to_string(&status)?.trim_matches('"').to_string();
        conn.execute(
            "UPDATE tasks SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![status_str, now, id],
        )?;
        Ok(())
    }

    pub fn list_tasks_for_goal(&self, goal_id: &str) -> Result<Vec<Task>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, goal_id, title, description, status, dependencies_json, acceptance_criteria_json, allowed_paths_json, target_branch, created_at, updated_at
             FROM tasks WHERE goal_id = ?1 ORDER BY created_at ASC",
        )?;

        let rows = stmt.query_map(params![goal_id], |row| {
            let status_str: String = row.get(4)?;
            let deps_str: String = row.get(5)?;
            let crit_str: String = row.get(6)?;
            let paths_str: Option<String> = row.get(7)?;

            let status: TaskStatus = serde_json::from_str(&format!("\"{}\"", status_str)).unwrap_or(TaskStatus::Blocked);
            let dependencies: Vec<String> = serde_json::from_str(&deps_str).unwrap_or_default();
            let acceptance_criteria: Vec<String> = serde_json::from_str(&crit_str).unwrap_or_default();
            let allowed_paths: Option<Vec<String>> = paths_str.and_then(|s| serde_json::from_str(&s).ok());

            Ok(Task {
                id: row.get(0)?,
                goal_id: row.get(1)?,
                title: row.get(2)?,
                description: row.get(3)?,
                status,
                dependencies,
                acceptance_criteria,
                allowed_paths,
                target_branch: row.get(8)?,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
            })
        })?;

        let mut tasks = Vec::new();
        for r in rows {
            tasks.push(r?);
        }
        Ok(tasks)
    }

    // --- Attempts ---

    pub fn create_attempt(
        &self,
        attempt_id: &str,
        task_id: &str,
        coordinator_epoch: i64,
        worker_id: &str,
        worktree_path: &str,
        ttl_ms: i64,
    ) -> Result<Attempt, StoreError> {
        let conn = self.conn.lock().unwrap();
        let now = now_millis();
        let expires_at = now + ttl_ms;

        conn.execute(
            "INSERT INTO attempts (id, task_id, coordinator_epoch, worker_id, worktree_path, status, lease_expires_at, heartbeat_at, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6, ?7, ?8, ?9)",
            params![attempt_id, task_id, coordinator_epoch, worker_id, worktree_path, expires_at, now, now, now],
        )?;

        Ok(Attempt {
            id: attempt_id.to_string(),
            task_id: task_id.to_string(),
            coordinator_epoch,
            worker_id: worker_id.to_string(),
            worktree_path: worktree_path.to_string(),
            status: AttemptStatus::Running,
            lease_expires_at: expires_at,
            heartbeat_at: now,
            candidate_commit: None,
            manifest_hash: None,
            created_at: now,
            updated_at: now,
        })
    }

    pub fn submit_attempt_result(
        &self,
        attempt_id: &str,
        epoch: i64,
        candidate_commit: &str,
        manifest_hash: &str,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        let now = now_millis();

        // Strict epoch fencing guard:
        // 1. Worker's reported epoch must match attempt's recorded epoch
        let attempt_epoch: i64 = conn.query_row(
            "SELECT coordinator_epoch FROM attempts WHERE id = ?1",
            params![attempt_id],
            |r| r.get(0),
        )?;

        if attempt_epoch != epoch {
            return Err(StoreError::FencingViolation {
                expected: attempt_epoch,
                actual: epoch,
            });
        }

        // 2. If coordinator lock has already advanced to a higher epoch (e.g., after crash/failover),
        // any delayed submissions from old epochs MUST be rejected (R2 guard)
        let current_epoch: i64 = conn
            .query_row("SELECT COALESCE(MAX(epoch), 0) FROM coordinator_locks", [], |r| r.get(0))?;

        if current_epoch > 0 && attempt_epoch < current_epoch {
            return Err(StoreError::StaleEpoch {
                attempt_epoch,
                current_coordinator_epoch: current_epoch,
            });
        }


        conn.execute(
            "UPDATE attempts SET status = 'result_submitted', candidate_commit = ?1, manifest_hash = ?2, updated_at = ?3 WHERE id = ?4",
            params![candidate_commit, manifest_hash, now, attempt_id],
        )?;
        Ok(())

    }

    pub fn update_attempt_status(&self, attempt_id: &str, status: AttemptStatus) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        let now = now_millis();
        let status_str = serde_json::to_string(&status)?.trim_matches('"').to_string();
        conn.execute(
            "UPDATE attempts SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![status_str, now, attempt_id],
        )?;
        Ok(())
    }

    pub fn get_attempt(&self, attempt_id: &str) -> Result<Attempt, StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, task_id, coordinator_epoch, worker_id, worktree_path, status, lease_expires_at, heartbeat_at, candidate_commit, manifest_hash, created_at, updated_at
             FROM attempts WHERE id = ?1",
            params![attempt_id],
            |row| {
                let status_str: String = row.get(5)?;
                let status: AttemptStatus = serde_json::from_str(&format!("\"{}\"", status_str)).unwrap_or(AttemptStatus::Lost);
                Ok(Attempt {
                    id: row.get(0)?,
                    task_id: row.get(1)?,
                    coordinator_epoch: row.get(2)?,
                    worker_id: row.get(3)?,
                    worktree_path: row.get(4)?,
                    status,
                    lease_expires_at: row.get(6)?,
                    heartbeat_at: row.get(7)?,
                    candidate_commit: row.get(8)?,
                    manifest_hash: row.get(9)?,
                    created_at: row.get(10)?,
                    updated_at: row.get(11)?,
                })
            },
        ).map_err(StoreError::Db)
    }

    // --- Verification & Integration Records ---

    pub fn record_verification(&self, v: &Verification) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO verifications (attempt_id, epoch, passed, allowed_paths_passed, acceptance_command_passed, manifest_hash, details, verified_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                v.attempt_id,
                v.epoch,
                v.passed as i32,
                v.allowed_paths_passed as i32,
                v.acceptance_command_passed as i32,
                v.manifest_hash,
                v.details,
                v.verified_at
            ],
        )?;
        Ok(())
    }

    pub fn record_integration(&self, i: &Integration) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        let status_str = serde_json::to_string(&i.status)?.trim_matches('"').to_string();
        conn.execute(
            "INSERT OR REPLACE INTO integrations (target_branch, base_commit, candidate_commit, integration_commit, status, error_message, integrated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                i.target_branch,
                i.base_commit,
                i.candidate_commit,
                i.integration_commit,
                status_str,
                i.error_message,
                i.integrated_at
            ],
        )?;
        Ok(())
    }

    pub fn get_integration(&self, target_branch: &str, candidate_commit: &str) -> Result<Option<Integration>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT target_branch, base_commit, candidate_commit, integration_commit, status, error_message, integrated_at
             FROM integrations WHERE target_branch = ?1 AND candidate_commit = ?2",
        )?;
        let mut rows = stmt.query(params![target_branch, candidate_commit])?;
        if let Some(row) = rows.next()? {
            let status_str: String = row.get(4)?;
            let status: IntegrationStatus = serde_json::from_str(&format!("\"{}\"", status_str))
                .unwrap_or(IntegrationStatus::Failed);
            Ok(Some(Integration {
                target_branch: row.get(0)?,
                base_commit: row.get(1)?,
                candidate_commit: row.get(2)?,
                integration_commit: row.get(3)?,
                status,
                error_message: row.get(5)?,
                integrated_at: row.get(6)?,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn list_pending_integrations(&self) -> Result<Vec<Integration>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT target_branch, base_commit, candidate_commit, integration_commit, status, error_message, integrated_at
             FROM integrations WHERE status = 'pending'",
        )?;
        let rows = stmt.query_map([], |row| {
            let status_str: String = row.get(4)?;
            let status: IntegrationStatus = serde_json::from_str(&format!("\"{}\"", status_str))
                .unwrap_or(IntegrationStatus::Pending);
            Ok(Integration {
                target_branch: row.get(0)?,
                base_commit: row.get(1)?,
                candidate_commit: row.get(2)?,
                integration_commit: row.get(3)?,
                status,
                error_message: row.get(5)?,
                integrated_at: row.get(6)?,
            })
        })?;
        let mut list = Vec::new();
        for r in rows {
            list.push(r?);
        }
        Ok(list)
    }

    // --- External Operations (Slice 2) ---

    pub fn record_external_operation_intent(
        &self,
        id: &str,
        attempt_id: &str,
        idempotency_key: &str,
        description: &str,
    ) -> Result<ExternalOperation, StoreError> {
        let conn = self.conn.lock().unwrap();
        let now = now_millis();
        conn.execute(
            "INSERT INTO external_operations (id, attempt_id, idempotency_key, description, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'intent_recorded', ?5, ?6)",
            params![id, attempt_id, idempotency_key, description, now, now],
        )?;

        Ok(ExternalOperation {
            id: id.to_string(),
            attempt_id: attempt_id.to_string(),
            idempotency_key: idempotency_key.to_string(),
            description: description.to_string(),
            status: ExternalOperationStatus::IntentRecorded,
            receipt_data: None,
            created_at: now,
            updated_at: now,
        })
    }

    pub fn update_external_operation_status(
        &self,
        id: &str,
        status: ExternalOperationStatus,
        receipt_data: Option<&str>,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        let now = now_millis();
        let status_str = serde_json::to_string(&status)?.trim_matches('"').to_string();

        conn.execute(
            "UPDATE external_operations SET status = ?1, receipt_data = COALESCE(?2, receipt_data), updated_at = ?3 WHERE id = ?4",
            params![status_str, receipt_data, now, id],
        )?;
        Ok(())
    }

    pub fn get_external_operation(&self, id: &str) -> Result<ExternalOperation, StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, attempt_id, idempotency_key, description, status, receipt_data, created_at, updated_at
             FROM external_operations WHERE id = ?1",
            params![id],
            |row| {
                let status_str: String = row.get(4)?;
                let status: ExternalOperationStatus = serde_json::from_str(&format!("\"{}\"", status_str))
                    .unwrap_or(ExternalOperationStatus::OutcomeUnknown);
                Ok(ExternalOperation {
                    id: row.get(0)?,
                    attempt_id: row.get(1)?,
                    idempotency_key: row.get(2)?,
                    description: row.get(3)?,
                    status,
                    receipt_data: row.get(5)?,
                    created_at: row.get(6)?,
                    updated_at: row.get(7)?,
                })
            },
        ).map_err(StoreError::Db)
    }

    pub fn record_durable_event(
        &self,
        entity_id: &str,
        event_type: &str,
        epoch: i64,
        dedup_key: Option<&str>,
        payload: &str,
    ) -> Result<i64, StoreError> {
        let conn = self.conn.lock().unwrap();
        let now = now_millis();

        conn.execute(
            "INSERT INTO events (entity_id, event_type, epoch, dedup_key, payload, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![entity_id, event_type, epoch, dedup_key, payload, now],
        )?;

        Ok(conn.last_insert_rowid())
    }

    // --- Recovery State Inspection ---

    pub fn recover_state(&self, current_epoch: i64) -> Result<(Vec<Attempt>, Vec<Attempt>, Vec<ExternalOperation>), StoreError> {
        let conn = self.conn.lock().unwrap();
        
        // 1. Mark stale running attempts from older epochs as needs_attention
        conn.execute(
            "UPDATE attempts SET status = 'needs_attention' WHERE status = 'running' AND coordinator_epoch < ?1",
            params![current_epoch],
        )?;

        // 2. R3: Dispatched external operations without confirmed receipt transition to outcome_unknown
        conn.execute(
            "UPDATE external_operations SET status = 'outcome_unknown' WHERE status = 'dispatched'",
            [],
        )?;

        // 3. Find result_submitted attempts ready for verification
        let mut stmt = conn.prepare(
            "SELECT id, task_id, coordinator_epoch, worker_id, worktree_path, status, lease_expires_at, heartbeat_at, candidate_commit, manifest_hash, created_at, updated_at
             FROM attempts WHERE status = 'result_submitted'",
        )?;
        let ready_for_verify = stmt.query_map([], |row| {
            Ok(Attempt {
                id: row.get(0)?,
                task_id: row.get(1)?,
                coordinator_epoch: row.get(2)?,
                worker_id: row.get(3)?,
                worktree_path: row.get(4)?,
                status: AttemptStatus::ResultSubmitted,
                lease_expires_at: row.get(6)?,
                heartbeat_at: row.get(7)?,
                candidate_commit: row.get(8)?,
                manifest_hash: row.get(9)?,
                created_at: row.get(10)?,
                updated_at: row.get(11)?,
            })
        })?.filter_map(|r| r.ok()).collect();

        // 4. Find attempts needing attention
        let mut stmt2 = conn.prepare(
            "SELECT id, task_id, coordinator_epoch, worker_id, worktree_path, status, lease_expires_at, heartbeat_at, candidate_commit, manifest_hash, created_at, updated_at
             FROM attempts WHERE status = 'needs_attention'",
        )?;
        let needs_attention = stmt2.query_map([], |row| {
            Ok(Attempt {
                id: row.get(0)?,
                task_id: row.get(1)?,
                coordinator_epoch: row.get(2)?,
                worker_id: row.get(3)?,
                worktree_path: row.get(4)?,
                status: AttemptStatus::NeedsAttention,
                lease_expires_at: row.get(6)?,
                heartbeat_at: row.get(7)?,
                candidate_commit: row.get(8)?,
                manifest_hash: row.get(9)?,
                created_at: row.get(10)?,
                updated_at: row.get(11)?,
            })
        })?.filter_map(|r| r.ok()).collect();

        // 5. Find unknown operations needing manual reconciliation
        let mut stmt3 = conn.prepare(
            "SELECT id, attempt_id, idempotency_key, description, status, receipt_data, created_at, updated_at
             FROM external_operations WHERE status = 'outcome_unknown'",
        )?;
        let unknown_ops = stmt3.query_map([], |row| {
            Ok(ExternalOperation {
                id: row.get(0)?,
                attempt_id: row.get(1)?,
                idempotency_key: row.get(2)?,
                description: row.get(3)?,
                status: ExternalOperationStatus::OutcomeUnknown,
                receipt_data: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
            })
        })?.filter_map(|r| r.ok()).collect();

        Ok((ready_for_verify, needs_attention, unknown_ops))
    }

    pub fn append_session_message(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
        tool_calls_json: Option<&str>,
        tool_call_id: Option<&str>,
        is_summary: bool,
    ) -> Result<i64, StoreError> {
        let conn = self.conn.lock().unwrap();
        let next_idx: i64 = conn.query_row(
            "SELECT COALESCE(MAX(message_index), -1) + 1 FROM session_messages WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?;

        conn.execute(
            r#"
            INSERT INTO session_messages (session_id, message_index, role, content, tool_calls_json, tool_call_id, is_compacted_summary, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
            params![
                session_id,
                next_idx,
                role,
                content,
                tool_calls_json,
                tool_call_id,
                if is_summary { 1 } else { 0 },
                now_millis()
            ],
        )?;

        Ok(next_idx)
    }

    pub fn list_session_messages(&self, session_id: &str) -> Result<Vec<(String, String, Option<String>, Option<String>, bool)>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"
            SELECT role, content, tool_calls_json, tool_call_id, is_compacted_summary
            FROM session_messages
            WHERE session_id = ?1
            ORDER BY message_index ASC
            "#,
        )?;

        let rows = stmt.query_map(params![session_id], |r| {
            let role: String = r.get(0)?;
            let content: String = r.get(1)?;
            let tool_calls: Option<String> = r.get(2)?;
            let tool_call_id: Option<String> = r.get(3)?;
            let is_summary: i32 = r.get(4)?;
            Ok((role, content, tool_calls, tool_call_id, is_summary == 1))
        })?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn list_active_context_messages(&self, session_id: &str) -> Result<Vec<(String, String, Option<String>, Option<String>, bool)>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"
            SELECT role, content, tool_calls_json, tool_call_id, is_compacted_summary
            FROM session_messages
            WHERE session_id = ?1 AND is_archived = 0
            ORDER BY is_compacted_summary DESC, message_index ASC
            "#,
        )?;

        let rows = stmt.query_map(params![session_id], |r| {
            let role: String = r.get(0)?;
            let content: String = r.get(1)?;
            let tool_calls: Option<String> = r.get(2)?;
            let tool_call_id: Option<String> = r.get(3)?;
            let is_summary: i32 = r.get(4)?;
            Ok((role, content, tool_calls, tool_call_id, is_summary == 1))
        })?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn compact_session(&self, session_id: &str, summary_content: &str, retain_last_n: usize) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        let active_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM session_messages WHERE session_id = ?1 AND is_archived = 0 AND is_compacted_summary = 0",
            params![session_id],
            |r| r.get(0),
        )?;

        if (active_count as usize) <= retain_last_n {
            return Ok(());
        }

        let mut cutoff_idx: i64 = conn.query_row(
            "SELECT message_index FROM session_messages WHERE session_id = ?1 AND is_archived = 0 AND is_compacted_summary = 0 ORDER BY message_index DESC LIMIT 1 OFFSET ?2",
            params![session_id, retain_last_n as i64],
            |r| r.get(0),
        )?;

        // ATOMIC PAIR PRESERVATION:
        // If cutoff_idx lands on an assistant message with tool_calls, do not split it from following tool results!
        // Move cutoff forward to include the tool results in the archive window so the active context never starts with an orphaned tool result.
        let is_tool_call_boundary: bool = conn.query_row(
            "SELECT COUNT(*) FROM session_messages WHERE session_id = ?1 AND message_index = ?2 AND tool_calls_json IS NOT NULL",
            params![session_id, cutoff_idx],
            |r| r.get::<_, i64>(0).map(|c| c > 0),
        ).unwrap_or(false);

        if is_tool_call_boundary {
            // Find max index of corresponding tool response
            if let Ok(next_res_idx) = conn.query_row(
                "SELECT message_index FROM session_messages WHERE session_id = ?1 AND message_index > ?2 AND role = 'tool' ORDER BY message_index ASC LIMIT 1",
                params![session_id, cutoff_idx],
                |r| r.get::<_, i64>(0),
            ) {
                cutoff_idx = next_res_idx;
            }
        }

        // ARCHIVE without deleting (Original raw audit history is completely preserved!)
        conn.execute(
            "UPDATE session_messages SET is_archived = 1 WHERE session_id = ?1 AND message_index <= ?2",
            params![session_id, cutoff_idx],
        )?;

        let max_idx: i64 = conn.query_row(
            "SELECT COALESCE(MAX(message_index), -1) + 1 FROM session_messages WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?;

        // Insert new compacted summary into active context with new unique index
        conn.execute(
            r#"
            INSERT INTO session_messages (session_id, message_index, role, content, tool_calls_json, tool_call_id, is_compacted_summary, is_archived, created_at)
            VALUES (?1, ?2, 'system', ?3, NULL, NULL, 1, 0, ?4)
            "#,
            params![session_id, max_idx, summary_content, now_millis()],
        )?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_coordinator_lock_monotonicity() {
        let dir = tempdir().unwrap();
        let store = HarnessStore::in_memory(dir.path()).unwrap();

        // Initial acquisition: epoch = 1
        let lock1 = store.acquire_coordinator_lock("coord_1", "worker_a", 1000).unwrap();
        assert_eq!(lock1.epoch, 1);
        assert_eq!(lock1.owner_id.as_deref(), Some("worker_a"));

        // Same owner renews: keeps epoch = 1
        let lock1_renew = store.acquire_coordinator_lock("coord_1", "worker_a", 1000).unwrap();
        assert_eq!(lock1_renew.epoch, 1);

        // Another owner fails while lease active
        assert!(store.acquire_coordinator_lock("coord_1", "worker_b", 1000).is_err());

        // Worker A releases lock
        store.release_coordinator_lock("coord_1", "worker_a").unwrap();

        // Worker B acquires released lock: epoch increments monotonically to 2
        let lock2 = store.acquire_coordinator_lock("coord_1", "worker_b", 1000).unwrap();
        assert_eq!(lock2.epoch, 2);
        assert_eq!(lock2.owner_id.as_deref(), Some("worker_b"));
    }

    #[test]
    fn test_epoch_fencing_submission() {
        let dir = tempdir().unwrap();
        let store = HarnessStore::in_memory(dir.path()).unwrap();

        let _goal = store.create_goal("g1", "Test Goal").unwrap();
        let task = Task {
            id: "t1".to_string(),
            goal_id: "g1".to_string(),
            title: "Task 1".to_string(),
            description: "".to_string(),
            status: TaskStatus::Ready,
            dependencies: vec![],
            acceptance_criteria: vec![],
            allowed_paths: None,
            target_branch: "main".to_string(),
            created_at: 0,
            updated_at: 0,
        };
        store.create_task(&task).unwrap();

        let _attempt = store.create_attempt("att_1", "t1", 5, "w1", "/tmp/wt1", 5000).unwrap();

        // Submitting with wrong epoch is rejected
        let res = store.submit_attempt_result("att_1", 4, "commit123", "hash123");
        assert!(matches!(res, Err(StoreError::FencingViolation { .. })));

        // Submitting with matching epoch succeeds
        store.submit_attempt_result("att_1", 5, "commit123", "hash123").unwrap();
        let updated = store.get_attempt("att_1").unwrap();
        assert_eq!(updated.status, AttemptStatus::ResultSubmitted);
        assert_eq!(updated.candidate_commit.as_deref(), Some("commit123"));
    }

    #[test]
    fn test_session_message_compaction_and_resume() {
        let dir = tempdir().unwrap();
        let store = HarnessStore::in_memory(dir.path()).unwrap();

        let session_id = "session_test_42";

        // Append 5 messages
        store.append_session_message(session_id, "user", "Task 1", None, None, false).unwrap();
        store.append_session_message(session_id, "assistant", "Working on task 1", None, None, false).unwrap();
        store.append_session_message(session_id, "user", "Task 2", None, None, false).unwrap();
        store.append_session_message(session_id, "assistant", "Done task 2", None, None, false).unwrap();
        store.append_session_message(session_id, "user", "Now do task 3", None, None, false).unwrap();

        let msgs = store.list_session_messages(session_id).unwrap();
        assert_eq!(msgs.len(), 5);

        // Compact session retaining only the last 2 messages
        let summary = "Summary of conversation: Task 1 and Task 2 were completed.";
        store.compact_session(session_id, summary, 2).unwrap();

        let raw_audit_msgs = store.list_session_messages(session_id).unwrap();
        // Raw audit log is fully preserved: 5 original messages + 1 summary = 6 total entries
        assert_eq!(raw_audit_msgs.len(), 6, "Audit trail must preserve all raw historical messages");

        // Active context returned to model contains only compacted summary + recent unarchived messages
        let active_msgs = store.list_active_context_messages(session_id).unwrap();
        assert_eq!(active_msgs.len(), 3, "Active context must contain summary + 2 retained messages");
        assert_eq!(active_msgs[0].0, "system");
        assert!(active_msgs[0].1.contains("Summary of conversation"));
        assert!(active_msgs[0].4, "Must be flagged as summary");
        assert_eq!(active_msgs[1].1, "Done task 2");
        assert_eq!(active_msgs[2].1, "Now do task 3");
    }
}

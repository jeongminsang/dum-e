use crate::store::{HarnessStore, StoreError};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::time::{SystemTime, UNIX_EPOCH};

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
fn digest(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RuntimeScope {
    Conversation(String),
    Attempt(String),
}
impl RuntimeScope {
    fn key(&self) -> String {
        match self {
            Self::Conversation(s) => format!("conversation:{s}"),
            Self::Attempt(s) => format!("attempt:{s}"),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunToken {
    pub run_id: String,
    pub owner_generation: i64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputKind {
    Submit,
    Steer,
    FollowUp,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputReceipt {
    pub input_id: i64,
    pub run_id: Option<String>,
    pub disposition: String,
    pub duplicate: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedInput {
    pub input_id: i64,
    pub client_request_id: String,
    pub kind: InputKind,
    pub payload: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BoundaryResult {
    Continue(Vec<QueuedInput>),
    Terminal,
    NextRun(RunToken),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub tool_call_id: String,
    pub name: String,
    pub arguments: String,
    pub replay_safe: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolExecution {
    pub execution_id: String,
    pub tool_call_id: String,
    pub name: String,
    pub arguments: String,
    pub state: String,
    pub revision: i64,
    pub result: Option<String>,
    pub active_attempt_id: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResolution {
    pub operator: String,
    pub kind: String,
    pub evidence: String,
    pub result: Option<String>,
    pub duplicate_effect_warning: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunMetrics {
    pub run_id: String,
    pub status: String,
    pub elapsed_ms: i64,
    pub timing_complete: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelAttemptOutcome {
    pub state: String,
    pub reason: Option<String>,
    pub usage: Option<serde_json::Value>,
    pub timing_complete: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    pub scope: RuntimeScope,
    pub status: String,
    pub owner_generation: i64,
    pub reason: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub elapsed_ms: i64,
    pub timing_complete: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxRecord {
    pub input_id: i64,
    pub scope: RuntimeScope,
    pub client_request_id: String,
    pub kind: InputKind,
    pub payload: String,
    pub disposition: String,
    pub target_run_id: Option<String>,
    pub state: String,
    pub consumed_run_id: Option<String>,
    pub transcript_idx: Option<i64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelAttemptRecord {
    pub request_id: String,
    pub run_id: String,
    pub session_id: String,
    pub model: String,
    pub state: String,
    pub reason: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub duration_ms: Option<i64>,
    pub timing_complete: bool,
    pub usage: Option<serde_json::Value>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolAttemptRecord {
    pub attempt_id: String,
    pub attempt_no: i64,
    pub state: String,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub duration_ms: Option<i64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolJournalRecord {
    pub execution: ToolExecution,
    pub error: Option<String>,
    pub result_source: Option<String>,
    pub attempts: Vec<ToolAttemptRecord>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationRuntimeMetrics {
    pub runs: Vec<RunRecord>,
    pub model_attempts: Vec<ModelAttemptRecord>,
    pub tool_executions: Vec<ToolExecution>,
    pub tool_attempts: Vec<ToolAttemptRecord>,
}

fn input_kind(s: &str) -> InputKind {
    match s {
        "steer" => InputKind::Steer,
        "follow_up" => InputKind::FollowUp,
        _ => InputKind::Submit,
    }
}
fn parse_scope(s: &str) -> RuntimeScope {
    if let Some(v) = s.strip_prefix("attempt:") {
        RuntimeScope::Attempt(v.into())
    } else {
        RuntimeScope::Conversation(s.strip_prefix("conversation:").unwrap_or(s).into())
    }
}

fn linked_conversation(
    tx: &rusqlite::Transaction<'_>,
    run_id: &str,
) -> Result<Option<String>, StoreError> {
    Ok(tx
        .query_row(
            "SELECT conversation_id FROM agent_runs WHERE run_id=?1",
            params![run_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten())
}
fn append_runtime_message(
    tx: &rusqlite::Transaction<'_>,
    conversation_id: &str,
    message_json: &str,
) -> Result<i64, StoreError> {
    let generation: i64 = tx
        .query_row(
            "SELECT generation FROM conversations WHERE id=?1",
            params![conversation_id],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound(format!("conversation {conversation_id}")))?;
    let idx:i64=tx.query_row("SELECT COALESCE(MAX(idx),-1)+1 FROM conversation_messages WHERE conversation_id=?1 AND generation=?2",params![conversation_id,generation],|r|r.get(0))?;
    tx.execute(
        "INSERT INTO conversation_messages VALUES(?1,?2,?3,?4,?5)",
        params![conversation_id, generation, idx, message_json, now()],
    )?;
    Ok(idx)
}

fn finalize_abandoned_model_attempts(
    tx: &rusqlite::Transaction<'_>,
    run_id: &str,
    status: &str,
    ended: i64,
) -> Result<(), StoreError> {
    let mut stmt=tx.prepare("SELECT request_id,session_id,model,context_json,usage_json,duration_ms FROM runtime_model_attempts WHERE run_id=?1 AND state='running'")?;
    let rows = stmt.query_map(params![run_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<i64>>(5)?,
        ))
    })?;
    let mut pending = Vec::new();
    for row in rows {
        pending.push(row?);
    }
    drop(stmt);
    for (request, session, model, context, usage_json, duration) in pending {
        let context: Option<serde_json::Value> =
            context.map(|v| serde_json::from_str(&v)).transpose()?;
        let usage: Option<serde_json::Value> = usage_json
            .as_ref()
            .map(|v| serde_json::from_str(v))
            .transpose()?;
        let meta = |snake: &str, camel: &str| {
            context
                .as_ref()
                .and_then(|v| v.get(snake).or_else(|| v.get(camel)))
                .and_then(|v| {
                    if v.is_null() {
                        None
                    } else if let Some(s) = v.as_str() {
                        Some(s.to_owned())
                    } else {
                        Some(v.to_string())
                    }
                })
        };
        let number = |key: &str| {
            usage
                .as_ref()
                .and_then(|v| v.get(key))
                .and_then(|n| n.as_i64())
                .unwrap_or(0)
        };
        let input = number("input_tokens");
        let output = number("output_tokens");
        let total = usage
            .as_ref()
            .and_then(|v| v.get("total_tokens").and_then(|n| n.as_i64()))
            .unwrap_or(input + output);
        let read = number("cache_read_tokens");
        let write = number("cache_write_tokens");
        let raw_usage = usage
            .as_ref()
            .and_then(|v| v.get("raw_usage"))
            .filter(|v| !v.is_null())
            .map(ToString::to_string);
        let inserted=tx.execute("INSERT OR IGNORE INTO request_usage(request_id,session_id,model,input_tokens,output_tokens,total_tokens,cache_read_tokens,cache_write_tokens,benchmark_run_id,case_id,variant,attempt_id,agent_id,parent_agent_id,raw_usage_json,is_complete,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,0,?16)",params![request,session,model,input,output,total,read,write,meta("benchmark_run_id","benchmarkRunId"),meta("case_id","caseId"),meta("variant","variant"),meta("attempt_id","attemptId"),meta("agent_id","agentId"),meta("parent_agent_id","parentAgentId"),raw_usage,ended])?;
        if inserted == 1 {
            tx.execute("INSERT INTO session_usage(session_id,input_tokens,output_tokens,total_tokens,cache_read_tokens,cache_write_tokens,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(session_id) DO UPDATE SET input_tokens=input_tokens+excluded.input_tokens,output_tokens=output_tokens+excluded.output_tokens,total_tokens=total_tokens+excluded.total_tokens,cache_read_tokens=cache_read_tokens+excluded.cache_read_tokens,cache_write_tokens=cache_write_tokens+excluded.cache_write_tokens,updated_at=excluded.updated_at",params![session,input,output,total,read,write,ended])?;
        }
        tx.execute("UPDATE runtime_model_attempts SET state=?1,reason='run_settled',ended_at=?2,duration_ms=?3,timing_complete=0,usage_json=CASE WHEN usage_json IS NULL THEN NULL ELSE json_set(usage_json,'$.is_complete',json('false')) END WHERE request_id=?4 AND state='running'",params![if status=="cancelled"{"cancelled"}else{"interrupted"},ended,duration.unwrap_or(0).max(0),request])?;
    }
    Ok(())
}

impl HarnessStore {
    pub fn attach_run_conversation(
        &self,
        token: &RunToken,
        conversation_id: &str,
    ) -> Result<(), StoreError> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let (scope, status, generation, _) = Self::check_owner(&tx, token)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM conversations WHERE id=?1)",
            params![conversation_id],
            |r| r.get(0),
        )?;
        if !exists {
            return Err(StoreError::NotFound(format!(
                "conversation {conversation_id}"
            )));
        }
        let prior: Option<String> = tx.query_row(
            "SELECT conversation_id FROM agent_runs WHERE run_id=?1",
            params![token.run_id],
            |r| r.get(0),
        )?;
        if let Some(prior) = prior {
            if prior != conversation_id {
                return Err(StoreError::InvalidSessionState(
                    "run already linked to another conversation".into(),
                ));
            }
        } else {
            if status != "running" {
                return Err(StoreError::InvalidSessionState(
                    "conversation link requires active run".into(),
                ));
            }
            let attempt = scope.strip_prefix("attempt:");
            let n=tx.execute("UPDATE agent_runs SET conversation_id=?1,fencing_attempt_id=COALESCE(fencing_attempt_id,?2) WHERE run_id=?3 AND owner_generation=?4 AND conversation_id IS NULL",params![conversation_id,attempt,token.run_id,generation])?;
            if n != 1 {
                return Err(StoreError::InvalidSessionState(
                    "conversation link changed concurrently".into(),
                ));
            }
        }
        let initial:Option<(i64,String)>=tx.query_row("SELECT input_id,payload FROM agent_inbox WHERE consumed_run_id=?1 AND transcript_idx IS NULL ORDER BY input_id LIMIT 1",params![token.run_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if let Some((input_id, payload)) = initial {
            let idx = append_runtime_message(
                &tx,
                conversation_id,
                &serde_json::json!({"role":"user","content":payload}).to_string(),
            )?;
            tx.execute("UPDATE agent_inbox SET transcript_idx=?1 WHERE input_id=?2 AND transcript_idx IS NULL",params![idx,input_id])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn admit_input(
        &self,
        scope: RuntimeScope,
        client_request_id: &str,
        kind: InputKind,
        payload: &str,
    ) -> Result<InputReceipt, StoreError> {
        let key = scope.key();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let existing: Option<(i64,String,String,Option<String>)> = tx.query_row("SELECT input_id,payload,disposition,COALESCE(target_run_id,consumed_run_id) FROM agent_inbox WHERE scope=?1 AND client_request_id=?2",params![key,client_request_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        if let Some((id, old, disp, rid)) = existing {
            let old_kind: String = tx.query_row(
                "SELECT kind FROM agent_inbox WHERE input_id=?1",
                params![id],
                |r| r.get(0),
            )?;
            let requested = match kind {
                InputKind::Submit => "submit",
                InputKind::Steer => "steer",
                InputKind::FollowUp => "follow_up",
            };
            if old != payload || old_kind != requested {
                return Err(StoreError::InvalidSessionState(
                    "client request ID reused with different kind or payload".into(),
                ));
            }
            tx.commit()?;
            return Ok(InputReceipt {
                input_id: id,
                run_id: rid,
                disposition: disp,
                duplicate: true,
            });
        }
        let pending_bytes:i64=tx.query_row("SELECT COALESCE(SUM(length(CAST(payload AS BLOB))),0) FROM agent_inbox WHERE scope=?1 AND state='pending'",params![key],|r|r.get(0))?;
        if pending_bytes + payload.len() as i64 > 512 * 1024 {
            return Err(StoreError::InvalidSessionState(
                "pending input payload quota exceeded".into(),
            ));
        }
        let active: Option<(String,String,i64)> = tx.query_row("SELECT run_id,status,owner_generation FROM agent_runs WHERE scope=?1 AND status IN ('queued','running','cancelling','needs_attention')",params![key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        if kind == InputKind::Submit && active.is_some() {
            return Err(StoreError::InvalidSessionState(
                "a run is already active in this scope".into(),
            ));
        }
        let kind_s = match kind {
            InputKind::Submit => "submit",
            InputKind::Steer => "steer",
            InputKind::FollowUp => "follow_up",
        };
        let (disp, target, state) = match active {
            Some((_id, status, _)) if status == "cancelling" || status == "needs_attention" => {
                ("pending", None, "paused")
            }
            Some((id, _, _)) if kind == InputKind::Steer => ("current_run", Some(id), "pending"),
            Some(_) => ("pending", None, "pending"),
            None => ("new_run", None, "pending"),
        };
        tx.execute("INSERT INTO agent_inbox(scope,client_request_id,kind,payload,disposition,target_run_id,state) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![key,client_request_id,kind_s,payload,disp,target,state])?;
        let id = tx.last_insert_rowid();
        let mut run_id = None;
        if disp == "new_run" {
            let rid = format!("run-{id}");
            tx.execute("INSERT INTO agent_runs(run_id,scope,status,owner_generation,started_at,conversation_id,fencing_attempt_id) VALUES(?1,?2,'queued',1,?3,?4,?5)",params![rid,key,now(),match &scope{RuntimeScope::Conversation(v)=>Some(v.as_str()),_=>None},match &scope{RuntimeScope::Attempt(v)=>Some(v.as_str()),_=>None}])?;
            if let RuntimeScope::Conversation(ref cid) = scope {
                let idx:i64=tx.query_row("SELECT COALESCE(MAX(idx),-1)+1 FROM conversation_messages WHERE conversation_id=?1 AND generation=(SELECT generation FROM conversations WHERE id=?1)",params![cid],|r|r.get(0))?;
                let msg = serde_json::json!({"role":"user","content":payload}).to_string();
                tx.execute("INSERT INTO conversation_messages(conversation_id,generation,idx,message_json,created_at) SELECT id,generation,?2,?3,?4 FROM conversations WHERE id=?1",params![cid,idx,msg,now()])?;
                tx.execute("UPDATE agent_inbox SET state='consumed',consumed_run_id=?1,transcript_idx=?2 WHERE input_id=?3",params![rid,idx,id])?;
            }
            run_id = Some(rid);
        }
        if run_id.is_none() {
            run_id = target.clone();
        }
        if disp == "new_run" {
            tx.execute(
                "UPDATE agent_inbox SET state='consumed',consumed_run_id=?1 WHERE input_id=?2",
                params![run_id, id],
            )?;
        }
        tx.commit()?;
        Ok(InputReceipt {
            input_id: id,
            run_id,
            disposition: disp.into(),
            duplicate: false,
        })
    }

    pub fn start_run(&self, scope: RuntimeScope, run_id: &str) -> Result<RunToken, StoreError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        if let RuntimeScope::Attempt(id) = &scope {
            let epoch: i64 = tx
                .query_row(
                    "SELECT coordinator_epoch FROM attempts WHERE id=?1",
                    params![id],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or_else(|| StoreError::NotFound(format!("attempt {id}")))?;
            let current: i64 = tx.query_row(
                "SELECT COALESCE(MAX(epoch),0) FROM coordinator_locks",
                [],
                |r| r.get(0),
            )?;
            if current > 0 && epoch < current {
                return Err(StoreError::StaleEpoch {
                    attempt_epoch: epoch,
                    current_coordinator_epoch: current,
                });
            }
        }
        let rows=tx.execute("UPDATE agent_runs SET status='running',started_at=?1 WHERE run_id=?2 AND scope=?3 AND status='queued'",params![now(),run_id,scope.key()])?;
        if rows == 0 {
            return Err(StoreError::InvalidSessionState("run is not queued".into()));
        }
        tx.commit()?;
        Ok(RunToken {
            run_id: run_id.into(),
            owner_generation: 1,
        })
    }
    fn check_owner(
        tx: &rusqlite::Transaction<'_>,
        token: &RunToken,
    ) -> Result<(String, String, i64, bool), StoreError> {
        let row: (String,String,i64,bool)=tx.query_row("SELECT scope,status,owner_generation,timing_complete FROM agent_runs WHERE run_id=?1",params![token.run_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,i64>(3)?!=0))).optional()?.ok_or_else(||StoreError::NotFound(token.run_id.clone()))?;
        if row.2 != token.owner_generation {
            return Err(StoreError::FencingViolation {
                expected: token.owner_generation,
                actual: row.2,
            });
        }
        if !["queued", "running", "cancelling", "needs_attention"].contains(&row.1.as_str()) {
            return Err(StoreError::InvalidSessionState("run is terminal".into()));
        }
        if let Some(attempt_id) = row.0.strip_prefix("attempt:") {
            let epoch: i64 = tx
                .query_row(
                    "SELECT coordinator_epoch FROM attempts WHERE id=?1",
                    params![attempt_id],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or_else(|| StoreError::NotFound(format!("attempt {attempt_id}")))?;
            let current: i64 = tx.query_row(
                "SELECT COALESCE(MAX(epoch),0) FROM coordinator_locks",
                [],
                |r| r.get(0),
            )?;
            if current > 0 && epoch < current {
                return Err(StoreError::StaleEpoch {
                    attempt_epoch: epoch,
                    current_coordinator_epoch: current,
                });
            }
        }
        Ok(row)
    }

    pub fn acquire_runtime_scope_lock(
        &self,
        scope: &RuntimeScope,
    ) -> Result<Option<File>, StoreError> {
        if self.db_path == std::path::Path::new(":memory:") {
            return Ok(None);
        }
        let path = self
            .db_path
            .with_extension(format!("runtime-{}.lock", digest(&scope.key())));
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(StoreError::Io(e)),
        }
    }
    pub fn checkpoint_run_elapsed(
        &self,
        token: &RunToken,
        elapsed_ms: i64,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        let n=conn.execute("UPDATE agent_runs SET elapsed_ms=MAX(elapsed_ms,?1) WHERE run_id=?2 AND owner_generation=?3 AND status='running'",params![elapsed_ms.max(0),token.run_id,token.owner_generation])?;
        if n == 0 {
            return Err(StoreError::InvalidSessionState(
                "run is not owned and running".into(),
            ));
        }
        Ok(())
    }
    pub fn settle_run(
        &self,
        token: &RunToken,
        status: &str,
        reason: Option<&str>,
        elapsed_ms: i64,
    ) -> Result<bool, StoreError> {
        if ![
            "succeeded",
            "failed",
            "cancelled",
            "interrupted",
            "needs_attention",
        ]
        .contains(&status)
        {
            return Err(StoreError::InvalidSessionState(
                "invalid terminal status".into(),
            ));
        }
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let current: Option<(String, i64)> = tx
            .query_row(
                "SELECT status,owner_generation FROM agent_runs WHERE run_id=?1",
                params![token.run_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((prior, owner_generation)) = current else {
            return Err(StoreError::NotFound(token.run_id.clone()));
        };
        if owner_generation != token.owner_generation {
            return Err(StoreError::FencingViolation {
                expected: token.owner_generation,
                actual: owner_generation,
            });
        }
        if prior == status {
            tx.commit()?;
            return Ok(false);
        }
        let (scope, old, _, _timing) = Self::check_owner(&tx, token)?;
        if old != "running" && old != "cancelling" && old != "needs_attention" {
            return Err(StoreError::InvalidSessionState("run is not active".into()));
        }
        let settled_at = now();
        finalize_abandoned_model_attempts(&tx, &token.run_id, status, settled_at)?;
        tx.execute("UPDATE agent_runs SET status=?1,reason=?2,ended_at=?3,elapsed_ms=MAX(elapsed_ms,?4),timing_complete=CASE WHEN timing_complete=0 THEN 0 ELSE 1 END WHERE run_id=?5",params![status,reason,settled_at,elapsed_ms.max(0),token.run_id])?;
        if status != "succeeded" {
            tx.execute("UPDATE agent_inbox SET state='paused',disposition='pending' WHERE scope=?1 AND state='pending'",params![scope])?;
        }
        tx.commit()?;
        Ok(true)
    }
    pub fn cancel_run(&self, token: &RunToken) -> Result<(), StoreError> {
        let c = self.conn.lock().unwrap();
        let n=c.execute("UPDATE agent_runs SET status='cancelling' WHERE run_id=?1 AND owner_generation=?2 AND status='running'",params![token.run_id,token.owner_generation])?;
        if n == 0 {
            return Err(StoreError::InvalidSessionState(
                "run cannot be cancelled".into(),
            ));
        }
        Ok(())
    }
    pub fn resume_run(&self, run_id: &str) -> Result<RunToken, StoreError> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let g: i64 = tx
            .query_row(
                "SELECT owner_generation FROM agent_runs WHERE run_id=?1",
                params![run_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(run_id.into()))?;
        let n=tx.execute("UPDATE agent_runs SET status='running',owner_generation=owner_generation+1,started_at=?1,ended_at=NULL WHERE run_id=?2 AND status IN ('interrupted','needs_attention')",params![now(),run_id])?;
        if n == 0 {
            return Err(StoreError::InvalidSessionState(
                "run is not resumable".into(),
            ));
        }
        tx.commit()?;
        Ok(RunToken {
            run_id: run_id.into(),
            owner_generation: g + 1,
        })
    }
    pub fn settle_or_consume_steer(
        &self,
        token: &RunToken,
        conversation_id: &str,
        elapsed_ms: i64,
    ) -> Result<BoundaryResult, StoreError> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        Self::check_owner(&tx, token)?;
        let cid =
            linked_conversation(&tx, &token.run_id)?.unwrap_or_else(|| conversation_id.to_owned());
        let mut stmt=tx.prepare("SELECT input_id,client_request_id,kind,payload FROM agent_inbox WHERE target_run_id=?1 AND state='pending' AND kind='steer' ORDER BY input_id")?;
        let rows = stmt.query_map(params![token.run_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut entries = Vec::new();
        for x in rows {
            entries.push(x?);
        }
        drop(stmt);
        if !entries.is_empty() {
            let mut result = Vec::new();
            for (id, client, _kind, payload) in entries {
                let json = serde_json::json!({"role":"user","content":payload}).to_string();
                let idx = append_runtime_message(&tx, &cid, &json)?;
                tx.execute("UPDATE agent_inbox SET state='consumed',consumed_run_id=?1,transcript_idx=?2 WHERE input_id=?3",params![token.run_id,idx,id])?;
                result.push(QueuedInput {
                    input_id: id,
                    client_request_id: client,
                    kind: InputKind::Steer,
                    payload,
                });
            }
            tx.commit()?;
            return Ok(BoundaryResult::Continue(result));
        }
        tx.execute("UPDATE agent_runs SET status='succeeded',ended_at=?1,elapsed_ms=MAX(elapsed_ms,?2) WHERE run_id=?3",params![now(),elapsed_ms.max(0),token.run_id])?;
        let follow:Option<(i64,String,String)>=tx.query_row("SELECT input_id,client_request_id,payload FROM agent_inbox WHERE scope=(SELECT scope FROM agent_runs WHERE run_id=?1) AND kind='follow_up' AND state='pending' ORDER BY input_id LIMIT 1",params![token.run_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        if let Some((id, _client, payload)) = follow {
            let rid = format!("run-follow-{id}");
            tx.execute("INSERT INTO agent_runs(run_id,scope,status,owner_generation,started_at,conversation_id,fencing_attempt_id) SELECT ?1,scope,'queued',1,?2,conversation_id,fencing_attempt_id FROM agent_runs WHERE run_id=?3",params![rid,now(),token.run_id])?;
            let idx = append_runtime_message(
                &tx,
                &cid,
                &serde_json::json!({"role":"user","content":payload}).to_string(),
            )?;
            tx.execute("UPDATE agent_inbox SET state='consumed',consumed_run_id=?1,transcript_idx=?3 WHERE input_id=?2",params![rid,id,idx])?;
            tx.commit()?;
            Ok(BoundaryResult::NextRun(RunToken {
                run_id: rid,
                owner_generation: 1,
            }))
        } else {
            tx.commit()?;
            Ok(BoundaryResult::Terminal)
        }
    }

    pub fn consume_inputs(
        &self,
        token: &RunToken,
        conversation_id: &str,
    ) -> Result<Vec<QueuedInput>, StoreError> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        Self::check_owner(&tx, token)?;
        let mut st=tx.prepare("SELECT input_id,client_request_id,kind,payload FROM agent_inbox WHERE target_run_id=?1 AND state='pending' AND kind='steer' ORDER BY input_id")?;
        let rows = st.query_map(params![token.run_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut items = Vec::new();
        for row in rows {
            items.push(row?);
        }
        drop(st);
        let mut result = Vec::new();
        for (id, client, kind, payload) in items {
            let idx:i64=tx.query_row("SELECT COALESCE(MAX(idx),-1)+1 FROM conversation_messages WHERE conversation_id=?1 AND generation=(SELECT generation FROM conversations WHERE id=?1)",params![conversation_id],|r|r.get(0))?;
            let json = serde_json::json!({"role":"user","content":payload}).to_string();
            tx.execute("INSERT INTO conversation_messages(conversation_id,generation,idx,message_json,created_at) SELECT id,generation,?2,?3,?4 FROM conversations WHERE id=?1",params![conversation_id,idx,json,now()])?;
            tx.execute("UPDATE agent_inbox SET state='consumed',consumed_run_id=?1,transcript_idx=?2 WHERE input_id=?3",params![token.run_id,idx,id])?;
            result.push(QueuedInput {
                input_id: id,
                client_request_id: client,
                kind: if kind == "steer" {
                    InputKind::Steer
                } else {
                    InputKind::Submit
                },
                payload,
            });
        }
        tx.commit()?;
        Ok(result)
    }

    pub fn prepare_assistant_turn_and_tools(
        &self,
        token: &RunToken,
        conversation_id: &str,
        assistant_json: &str,
        tools: &[ToolSpec],
    ) -> Result<Vec<ToolExecution>, StoreError> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        Self::check_owner(&tx, token)?;
        let linked = linked_conversation(&tx, &token.run_id)?;
        let cid = linked.as_deref().unwrap_or(conversation_id);
        if linked.as_ref().is_some_and(|v| v != conversation_id) {
            return Err(StoreError::InvalidSessionState(
                "assistant transcript conversation differs from run link".into(),
            ));
        }
        let mut out = Vec::new();
        let mut fresh = 0;
        for t in tools {
            let hash = digest(&t.arguments);
            let existing:Option<ToolExecution>=tx.query_row("SELECT execution_id,tool_call_id,name,arguments,state,revision,result,active_attempt_id FROM tool_executions WHERE run_id=?1 AND tool_call_id=?2",params![token.run_id,t.tool_call_id],|r|Ok(ToolExecution{execution_id:r.get(0)?,tool_call_id:r.get(1)?,name:r.get(2)?,arguments:r.get(3)?,state:r.get(4)?,revision:r.get(5)?,result:r.get(6)?,active_attempt_id:r.get(7)?})).optional()?;
            if let Some(e) = existing {
                if e.name != t.name || digest(&e.arguments) != hash {
                    return Err(StoreError::InvalidSessionState(
                        "tool call ID reused with different request".into(),
                    ));
                }
                out.push(e);
                continue;
            }
            let eid = format!("{}:{}", token.run_id, t.tool_call_id);
            tx.execute("INSERT INTO tool_executions(execution_id,run_id,tool_call_id,name,arguments,arguments_hash,replay_policy,state) VALUES(?1,?2,?3,?4,?5,?6,?7,'prepared')",params![eid,token.run_id,t.tool_call_id,t.name,t.arguments,hash,if t.replay_safe{"replay_safe"}else{"never"}])?;
            fresh += 1;
            out.push(ToolExecution {
                execution_id: eid,
                tool_call_id: t.tool_call_id.clone(),
                name: t.name.clone(),
                arguments: t.arguments.clone(),
                state: "prepared".into(),
                revision: 0,
                result: None,
                active_attempt_id: None,
            });
        }
        if fresh > 0 && fresh < tools.len() {
            return Err(StoreError::InvalidSessionState(
                "a new tool turn cannot reuse earlier tool call IDs".into(),
            ));
        }
        if fresh > 0 || tools.is_empty() {
            append_runtime_message(&tx, cid, assistant_json)?;
        }
        tx.commit()?;
        Ok(out)
    }
    pub fn replay_safe_tool(
        &self,
        token: &RunToken,
        execution_id: &str,
    ) -> Result<ToolExecution, StoreError> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        Self::check_owner(&tx, token)?;
        let (policy,name,state):(String,String,String)=tx.query_row("SELECT replay_policy,name,state FROM tool_executions WHERE execution_id=?1 AND run_id=?2",params![execution_id,token.run_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?.ok_or_else(||StoreError::NotFound(execution_id.into()))?;
        if policy != "replay_safe"
            || !matches!(name.as_str(), "read_file" | "read_artifact" | "grep_search")
            || state != "outcome_unknown"
        {
            return Err(StoreError::InvalidSessionState(
                "tool is not eligible for safe replay".into(),
            ));
        }
        tx.execute("UPDATE tool_executions SET state='prepared',revision=revision+1,active_attempt_id=NULL,error=NULL,result=NULL,result_source='replay_authorized' WHERE execution_id=?1 AND run_id=?2 AND state='outcome_unknown'",params![execution_id,token.run_id])?;
        let execution=tx.query_row("SELECT execution_id,tool_call_id,name,arguments,state,revision,result,active_attempt_id FROM tool_executions WHERE execution_id=?1",params![execution_id],|r|Ok(ToolExecution{execution_id:r.get(0)?,tool_call_id:r.get(1)?,name:r.get(2)?,arguments:r.get(3)?,state:r.get(4)?,revision:r.get(5)?,result:r.get(6)?,active_attempt_id:r.get(7)?}))?;
        tx.commit()?;
        Ok(execution)
    }
    pub fn mark_tool_unknown(
        &self,
        token: &RunToken,
        execution_id: &str,
        attempt_id: &str,
        reason: &str,
        duration_ms: i64,
    ) -> Result<(), StoreError> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let (scope, _, _, _) = Self::check_owner(&tx, token)?;
        let n=tx.execute("UPDATE tool_executions SET state='outcome_unknown',error=?1,revision=revision+1 WHERE execution_id=?2 AND run_id=?3 AND active_attempt_id=?4 AND state='running'",params![reason,execution_id,token.run_id,attempt_id])?;
        if n == 0 {
            return Err(StoreError::InvalidSessionState(
                "tool attempt is stale or not running".into(),
            ));
        }
        let n=tx.execute("UPDATE tool_execution_attempts SET state='outcome_unknown',ended_at=?1,duration_ms=?2 WHERE attempt_id=?3 AND execution_id=?4 AND state='running'",params![now(),duration_ms.max(0),attempt_id,execution_id])?;
        if n == 0 {
            return Err(StoreError::InvalidSessionState(
                "tool attempt record is stale or not running".into(),
            ));
        }
        tx.execute("UPDATE agent_runs SET status='needs_attention',reason=?1,timing_complete=0 WHERE run_id=?2 AND owner_generation=?3 AND status IN ('running','cancelling')",params![reason,token.run_id,token.owner_generation])?;
        tx.execute("UPDATE agent_inbox SET state='paused',disposition='pending' WHERE scope=?1 AND state='pending'",params![scope])?;
        tx.commit()?;
        Ok(())
    }
    pub fn begin_tool(
        &self,
        token: &RunToken,
        execution_id: &str,
        attempt_id: &str,
    ) -> Result<bool, StoreError> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        Self::check_owner(&tx, token)?;
        let (state,no,approved):(String,i64,Option<String>)=tx.query_row("SELECT state,COALESCE((SELECT MAX(attempt_no)+1 FROM tool_execution_attempts WHERE execution_id=?1),1),active_attempt_id FROM tool_executions WHERE execution_id=?1 AND run_id=?2",params![execution_id,token.run_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?.ok_or_else(||StoreError::NotFound(execution_id.into()))?;
        if state != "prepared" {
            tx.commit()?;
            return Ok(false);
        }
        if let Some(approved) = approved {
            if approved != attempt_id {
                return Err(StoreError::InvalidSessionState(
                    "retry must use the authorized attempt ID".into(),
                ));
            }
            let updated = tx.execute("UPDATE tool_execution_attempts SET state='running',started_at=?1 WHERE attempt_id=?2 AND execution_id=?3 AND state='prepared'", params![now(), attempt_id, execution_id])?;
            if updated != 1 {
                return Err(StoreError::InvalidSessionState(
                    "authorized attempt is not prepared".into(),
                ));
            }
        } else {
            tx.execute("INSERT INTO tool_execution_attempts(attempt_id,execution_id,attempt_no,state,started_at) VALUES(?1,?2,?3,'running',?4)",params![attempt_id,execution_id,no,now()])?;
        }
        tx.execute("UPDATE tool_executions SET state='running',active_attempt_id=?1,revision=revision+1 WHERE execution_id=?2",params![attempt_id,execution_id])?;
        tx.commit()?;
        Ok(true)
    }
    pub fn commit_tool_result_and_message(
        &self,
        token: &RunToken,
        execution_id: &str,
        attempt_id: &str,
        result: &str,
        tool_message_json: &str,
        duration_ms: i64,
    ) -> Result<(), StoreError> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        Self::check_owner(&tx, token)?;
        let n=tx.execute("UPDATE tool_executions SET state='succeeded',result=?1,revision=revision+1 WHERE execution_id=?2 AND run_id=?3 AND active_attempt_id=?4 AND state='running'",params![result,execution_id,token.run_id,attempt_id])?;
        if n == 0 {
            return Err(StoreError::InvalidSessionState(
                "tool attempt is stale or not running".into(),
            ));
        }
        tx.execute("UPDATE tool_execution_attempts SET state='succeeded',ended_at=?1,duration_ms=?2 WHERE attempt_id=?3",params![now(),duration_ms.max(0),attempt_id])?;
        if let Some(cid) = linked_conversation(&tx, &token.run_id)? {
            append_runtime_message(&tx, &cid, tool_message_json)?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn recover_run(&self, run_id: &str) -> Result<Vec<ToolExecution>, StoreError> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let scope: Option<String> = tx
            .query_row(
                "SELECT scope FROM agent_runs WHERE run_id=?1",
                params![run_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(scope) = scope else {
            return Err(StoreError::NotFound(run_id.into()));
        };
        finalize_abandoned_model_attempts(&tx, run_id, "interrupted", now())?;
        tx.execute("UPDATE tool_execution_attempts SET state='outcome_unknown',ended_at=?1 WHERE execution_id IN (SELECT execution_id FROM tool_executions WHERE run_id=?2) AND state='running'",params![now(),run_id])?;
        tx.execute("UPDATE tool_executions SET state='outcome_unknown',revision=revision+1 WHERE run_id=?1 AND state='running'",params![run_id])?;
        tx.execute("UPDATE agent_runs SET status='needs_attention',timing_complete=0 WHERE run_id=?1 AND status IN ('running','cancelling')",params![run_id])?;
        tx.execute("UPDATE agent_inbox SET state='paused',disposition='pending' WHERE scope=?1 AND state='pending'",params![scope])?;
        let mut st=tx.prepare("SELECT execution_id,tool_call_id,name,arguments,state,revision,result,active_attempt_id FROM tool_executions WHERE run_id=?1 ORDER BY rowid")?;
        let rows = st.query_map(params![run_id], |r| {
            Ok(ToolExecution {
                execution_id: r.get(0)?,
                tool_call_id: r.get(1)?,
                name: r.get(2)?,
                arguments: r.get(3)?,
                state: r.get(4)?,
                revision: r.get(5)?,
                result: r.get(6)?,
                active_attempt_id: r.get(7)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        drop(st);
        tx.commit()?;
        Ok(out)
    }
    pub fn resolve_unknown(
        &self,
        token: &RunToken,
        execution_id: &str,
        expected_revision: i64,
        resolution: ToolResolution,
    ) -> Result<(), StoreError> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        Self::check_owner(&tx, token)?;
        let (state,rev,attempt):(String,i64,String)=tx.query_row("SELECT state,revision,active_attempt_id FROM tool_executions WHERE execution_id=?1 AND run_id=?2",params![execution_id,token.run_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?.ok_or_else(||StoreError::NotFound(execution_id.into()))?;
        if state != "outcome_unknown" || rev != expected_revision {
            return Err(StoreError::InvalidSessionState(
                "unknown tool revision/state mismatch".into(),
            ));
        }
        if resolution.operator.is_empty() || resolution.evidence.is_empty() {
            return Err(StoreError::InvalidSessionState(
                "operator and evidence are required".into(),
            ));
        }
        if resolution.kind == "retry_authorized" && !resolution.duplicate_effect_warning {
            return Err(StoreError::InvalidSessionState(
                "retry requires duplicate-effect warning".into(),
            ));
        }
        if !["observed_succeeded", "observed_not_run", "retry_authorized"]
            .contains(&resolution.kind.as_str())
        {
            return Err(StoreError::InvalidSessionState(
                "invalid resolution kind".into(),
            ));
        }
        if resolution.kind == "observed_succeeded" && resolution.result.is_none() {
            return Err(StoreError::InvalidSessionState(
                "observed success requires bounded result".into(),
            ));
        }
        if resolution.kind == "observed_not_run"
            && !resolution.evidence.starts_with("confirmed_not_run:")
        {
            return Err(StoreError::InvalidSessionState(
                "observed_not_run requires authoritative confirmation evidence".into(),
            ));
        }
        let rid = format!("resolution-{}-{}", execution_id, rev);
        tx.execute(
            "INSERT INTO tool_resolutions VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                rid,
                execution_id,
                attempt,
                rev,
                resolution.operator,
                resolution.kind,
                resolution.evidence,
                resolution.duplicate_effect_warning as i64,
                now()
            ],
        )?;
        if resolution.kind == "observed_succeeded" {
            let result = resolution.result.as_deref().unwrap();
            let call: String = tx.query_row(
                "SELECT tool_call_id FROM tool_executions WHERE execution_id=?1",
                params![execution_id],
                |r| r.get(0),
            )?;
            let message=serde_json::json!({"role":"tool","tool_call_id":call,"content":result,"source":"operator_observed"}).to_string();
            tx.execute("UPDATE tool_executions SET state='succeeded',result=?1,result_source='operator_observed',revision=revision+1 WHERE execution_id=?2",params![message,execution_id])?;
            let conv = linked_conversation(&tx, &token.run_id)?;
            if let Some(cid) = conv.as_ref() {
                let g: i64 = tx.query_row(
                    "SELECT generation FROM conversations WHERE id=?1",
                    params![cid],
                    |r| r.get(0),
                )?;
                let idx:i64=tx.query_row("SELECT COALESCE(MAX(idx),-1)+1 FROM conversation_messages WHERE conversation_id=?1 AND generation=?2",params![cid,g],|r|r.get(0))?;
                tx.execute(
                    "INSERT INTO conversation_messages VALUES(?1,?2,?3,?4,?5)",
                    params![cid, g, idx, message, now()],
                )?;
            }
        } else {
            let new_id = format!("{execution_id}:retry:{}", rev + 1);
            let no:i64=tx.query_row("SELECT COALESCE(MAX(attempt_no),0)+1 FROM tool_execution_attempts WHERE execution_id=?1",params![execution_id],|r|r.get(0))?;
            tx.execute("INSERT INTO tool_execution_attempts(attempt_id,execution_id,attempt_no,state) VALUES(?1,?2,?3,'prepared')",params![new_id,execution_id,no])?;
            tx.execute("UPDATE tool_executions SET state='prepared',active_attempt_id=?1,revision=revision+1 WHERE execution_id=?2",params![new_id,execution_id])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn start_model_attempt(
        &self,
        token: &RunToken,
        request_id: &str,
        session_id: &str,
        model: &str,
    ) -> Result<(), StoreError> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let (_, status, owner, _) = Self::check_owner(&tx, token)?;
        if status != "running" {
            return Err(StoreError::InvalidSessionState(
                "model attempts require a running run".into(),
            ));
        }
        tx.execute("INSERT INTO runtime_model_attempts(request_id,run_id,session_id,model,state,started_at,owner_generation) VALUES(?1,?2,?3,?4,'running',?5,?6)",params![request_id,token.run_id,session_id,model,now(),owner])?;
        tx.commit()?;
        Ok(())
    }
    pub fn set_model_attempt_context(
        &self,
        request_id: &str,
        context_json: &str,
    ) -> Result<(), StoreError> {
        let context: serde_json::Value = serde_json::from_str(context_json)?;
        if !context.is_object() {
            return Err(StoreError::InvalidSessionState(
                "model attempt context must be a JSON object".into(),
            ));
        }
        let c = self.conn.lock().unwrap();
        let n=c.execute("UPDATE runtime_model_attempts SET context_json=?1 WHERE request_id=?2 AND state='running'",params![context_json,request_id])?;
        if n == 0 {
            return Err(StoreError::InvalidSessionState(
                "model attempt is missing or finalized".into(),
            ));
        }
        Ok(())
    }
    pub fn finish_model_attempt(
        &self,
        request_id: &str,
        outcome: ModelAttemptOutcome,
        duration_ms: i64,
    ) -> Result<bool, StoreError> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let row:Option<(String,String,String,Option<String>,String,i64)>=tx.query_row("SELECT session_id,model,state,context_json,run_id,owner_generation FROM runtime_model_attempts WHERE request_id=?1",params![request_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
        let Some((session, model, state, context_json, run_id, started_gen)) = row else {
            return Err(StoreError::NotFound(request_id.into()));
        };
        let current: Option<(i64, String)> = tx
            .query_row(
                "SELECT owner_generation,status FROM agent_runs WHERE run_id=?1",
                params![run_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((current_gen, current_status)) = current else {
            return Err(StoreError::NotFound(run_id));
        };
        if current_gen != started_gen
            || !(current_status == "running" || current_status == "cancelling")
        {
            return Err(StoreError::FencingViolation {
                expected: started_gen,
                actual: current_gen,
            });
        }
        if state != "running" {
            tx.commit()?;
            return Ok(false);
        }
        let context: Option<serde_json::Value> =
            context_json.map(|v| serde_json::from_str(&v)).transpose()?;
        let meta = |snake: &str, camel: &str| {
            context
                .as_ref()
                .and_then(|v| v.get(snake).or_else(|| v.get(camel)))
                .and_then(|v| {
                    if v.is_null() {
                        None
                    } else if let Some(s) = v.as_str() {
                        Some(s.to_string())
                    } else {
                        Some(v.to_string())
                    }
                })
        };
        let vals = outcome.usage.as_ref();
        let number = |key: &str| {
            vals.and_then(|v| v.get(key))
                .and_then(|n| n.as_i64())
                .unwrap_or(0)
        };
        let known = vals
            .and_then(|v| v.get("is_complete"))
            .and_then(serde_json::Value::as_bool)
            == Some(true);
        let input = number("input_tokens");
        let output = number("output_tokens");
        let total = vals
            .and_then(|v| v.get("total_tokens").and_then(|n| n.as_i64()))
            .unwrap_or(input + output);
        let read = number("cache_read_tokens");
        let write = number("cache_write_tokens");
        let usage_json = outcome.usage.as_ref().map(|v| v.to_string());
        let raw_usage = vals
            .and_then(|v| v.get("raw_usage"))
            .filter(|v| !v.is_null())
            .map(ToString::to_string);
        tx.execute("UPDATE runtime_model_attempts SET state=?1,reason=?2,ended_at=?3,duration_ms=?4,timing_complete=?5,usage_json=?6 WHERE request_id=?7",params![outcome.state,outcome.reason,now(),duration_ms.max(0),outcome.timing_complete as i64,usage_json,request_id])?;
        let inserted=tx.execute("INSERT OR IGNORE INTO request_usage(request_id,session_id,model,input_tokens,output_tokens,total_tokens,cache_read_tokens,cache_write_tokens,benchmark_run_id,case_id,variant,attempt_id,agent_id,parent_agent_id,raw_usage_json,is_complete,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",params![request_id,session,model,input,output,total,read,write,meta("benchmark_run_id","benchmarkRunId"),meta("case_id","caseId"),meta("variant","variant"),meta("attempt_id","attemptId"),meta("agent_id","agentId"),meta("parent_agent_id","parentAgentId"),raw_usage,known as i64,now()])?;
        if inserted == 1 {
            tx.execute("INSERT INTO session_usage(session_id,input_tokens,output_tokens,total_tokens,cache_read_tokens,cache_write_tokens,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(session_id) DO UPDATE SET input_tokens=input_tokens+excluded.input_tokens,output_tokens=output_tokens+excluded.output_tokens,total_tokens=total_tokens+excluded.total_tokens,cache_read_tokens=cache_read_tokens+excluded.cache_read_tokens,cache_write_tokens=cache_write_tokens+excluded.cache_write_tokens,updated_at=excluded.updated_at",params![session,input,output,total,read,write,now()])?;
        }
        tx.commit()?;
        Ok(true)
    }
    pub fn activate_tools(
        &self,
        conversation_id: &str,
        ids: &[String],
        fingerprints: &std::collections::HashMap<String, String>,
    ) -> Result<(), StoreError> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        tx.execute(
            "DELETE FROM conversation_active_tools WHERE conversation_id=?1",
            params![conversation_id],
        )?;
        for id in ids {
            let fp = fingerprints.get(id).ok_or_else(|| {
                StoreError::InvalidSessionState(format!("missing fingerprint for {id}"))
            })?;
            tx.execute(
                "INSERT INTO conversation_active_tools VALUES(?1,?2,?3)",
                params![conversation_id, id, fp],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn active_tools(&self, conversation_id: &str) -> Result<Vec<(String, String)>, StoreError> {
        let c = self.conn.lock().unwrap();
        let mut st=c.prepare("SELECT tool_id,fingerprint FROM conversation_active_tools WHERE conversation_id=?1 ORDER BY tool_id")?;
        let rows = st.query_map(params![conversation_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
    pub fn run_metrics(&self, run_id: &str) -> Result<RunMetrics, StoreError> {
        let c = self.conn.lock().unwrap();
        c.query_row(
            "SELECT run_id,status,elapsed_ms,timing_complete FROM agent_runs WHERE run_id=?1",
            params![run_id],
            |r| {
                Ok(RunMetrics {
                    run_id: r.get(0)?,
                    status: r.get(1)?,
                    elapsed_ms: r.get(2)?,
                    timing_complete: r.get::<_, i64>(3)? != 0,
                })
            },
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound(run_id.into()))
    }
    pub fn conversation_metrics(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<RunMetrics>, StoreError> {
        let c = self.conn.lock().unwrap();
        let mut st=c.prepare("SELECT run_id,status,elapsed_ms,timing_complete FROM agent_runs WHERE scope=?1 ORDER BY started_at")?;
        let rows = st.query_map(params![format!("conversation:{conversation_id}")], |r| {
            Ok(RunMetrics {
                run_id: r.get(0)?,
                status: r.get(1)?,
                elapsed_ms: r.get(2)?,
                timing_complete: r.get::<_, i64>(3)? != 0,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn inbox(
        &self,
        scope: &RuntimeScope,
        states: &[&str],
    ) -> Result<Vec<InboxRecord>, StoreError> {
        let c = self.conn.lock().unwrap();
        let mut st=c.prepare("SELECT input_id,scope,client_request_id,kind,payload,disposition,target_run_id,state,consumed_run_id,transcript_idx FROM agent_inbox WHERE scope=?1 ORDER BY input_id")?;
        let rows = st.query_map(params![scope.key()], |r| {
            Ok(InboxRecord {
                input_id: r.get(0)?,
                scope: parse_scope(&r.get::<_, String>(1)?),
                client_request_id: r.get(2)?,
                kind: input_kind(&r.get::<_, String>(3)?),
                payload: r.get(4)?,
                disposition: r.get(5)?,
                target_run_id: r.get(6)?,
                state: r.get(7)?,
                consumed_run_id: r.get(8)?,
                transcript_idx: r.get(9)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            let item = r?;
            if states.is_empty() || states.contains(&item.state.as_str()) {
                out.push(item);
            }
        }
        Ok(out)
    }
    pub fn withdraw_input(&self, scope: &RuntimeScope, input_id: i64) -> Result<bool, StoreError> {
        let c = self.conn.lock().unwrap();
        Ok(c.execute("UPDATE agent_inbox SET state='withdrawn' WHERE scope=?1 AND input_id=?2 AND state IN ('pending','paused')",params![scope.key(),input_id])?==1)
    }
    pub fn reactivate_paused_input(
        &self,
        scope: &RuntimeScope,
        input_id: i64,
    ) -> Result<InputReceipt, StoreError> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let (kind,payload):(String,String)=tx.query_row("SELECT kind,payload FROM agent_inbox WHERE scope=?1 AND input_id=?2 AND state='paused'",params![scope.key(),input_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?.ok_or_else(||StoreError::InvalidSessionState("input is not paused".into()))?;
        let active:Option<(String,String)>=tx.query_row("SELECT run_id,status FROM agent_runs WHERE scope=?1 AND status IN ('queued','running','cancelling','needs_attention')",params![scope.key()],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let (disp, target, state, run) = match active {
            Some((_id, status)) if status == "cancelling" || status == "needs_attention" => {
                ("pending", None, "paused", None)
            }
            Some((id, _)) if kind == "steer" => ("current_run", Some(id), "pending", None),
            Some(_) => ("pending", None, "pending", None),
            None => {
                let rid = format!("run-reactivated-{input_id}");
                tx.execute("INSERT INTO agent_runs(run_id,scope,status,owner_generation,started_at) VALUES(?1,?2,'queued',1,?3)",params![rid,scope.key(),now()])?;
                ("new_run", None, "pending", Some(rid))
            }
        };
        tx.execute(
            "UPDATE agent_inbox SET disposition=?1,target_run_id=?2,state=?3 WHERE input_id=?4",
            params![disp, target, state, input_id],
        )?;
        if let Some(ref rid) = run {
            if let RuntimeScope::Conversation(cid) = scope {
                let idx:i64=tx.query_row("SELECT COALESCE(MAX(idx),-1)+1 FROM conversation_messages WHERE conversation_id=?1 AND generation=(SELECT generation FROM conversations WHERE id=?1)",params![cid],|r|r.get(0))?;
                tx.execute("INSERT INTO conversation_messages(conversation_id,generation,idx,message_json,created_at) SELECT id,generation,?2,?3,?4 FROM conversations WHERE id=?1",params![cid,idx,serde_json::json!({"role":"user","content":payload}).to_string(),now()])?;
                tx.execute("UPDATE agent_inbox SET state='consumed',consumed_run_id=?1,transcript_idx=?2 WHERE input_id=?3",params![rid,idx,input_id])?;
            }
        }
        tx.commit()?;
        Ok(InputReceipt {
            input_id,
            run_id: run.or(target),
            disposition: disp.into(),
            duplicate: false,
        })
    }

    pub fn get_run(&self, run_id: &str) -> Result<RunRecord, StoreError> {
        let c = self.conn.lock().unwrap();
        c.query_row("SELECT run_id,scope,status,owner_generation,reason,started_at,ended_at,elapsed_ms,timing_complete FROM agent_runs WHERE run_id=?1",params![run_id],|r|Ok(RunRecord{run_id:r.get(0)?,scope:parse_scope(&r.get::<_,String>(1)?),status:r.get(2)?,owner_generation:r.get(3)?,reason:r.get(4)?,started_at:r.get(5)?,ended_at:r.get(6)?,elapsed_ms:r.get(7)?,timing_complete:r.get::<_,i64>(8)?!=0})).optional()?.ok_or_else(||StoreError::NotFound(run_id.into()))
    }
    pub fn active_run(&self, scope: &RuntimeScope) -> Result<Option<RunRecord>, StoreError> {
        let id: Option<String> = {
            let c = self.conn.lock().unwrap();
            c.query_row("SELECT run_id FROM agent_runs WHERE scope=?1 AND status IN ('queued','running','cancelling','needs_attention') ORDER BY started_at DESC LIMIT 1",params![scope.key()],|r|r.get::<_,String>(0)).optional()?
        };
        id.map(|v| self.get_run(&v)).transpose()
    }
    pub fn latest_run(&self, scope: &RuntimeScope) -> Result<Option<RunRecord>, StoreError> {
        let id: Option<String> = {
            let c = self.conn.lock().unwrap();
            c.query_row("SELECT run_id FROM agent_runs WHERE scope=?1 ORDER BY started_at DESC,rowid DESC LIMIT 1",params![scope.key()],|r|r.get::<_,String>(0)).optional()?
        };
        id.map(|v| self.get_run(&v)).transpose()
    }
    pub fn recover_scope(&self, scope: &RuntimeScope) -> Result<Vec<ToolExecution>, StoreError> {
        let active = self.active_run(scope)?;
        match active {
            Some(r) if r.status == "running" || r.status == "cancelling" => {
                self.recover_run(&r.run_id)
            }
            _ => Ok(Vec::new()),
        }
    }
    pub fn conversation_history(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<crate::store::ConversationMessageRecord>, StoreError> {
        let (_, messages) = self.load_conversation(conversation_id)?;
        Ok(messages)
    }
    pub fn tool_attempts(&self, execution_id: &str) -> Result<Vec<ToolAttemptRecord>, StoreError> {
        let c = self.conn.lock().unwrap();
        let mut st=c.prepare("SELECT attempt_id,attempt_no,state,started_at,ended_at,duration_ms FROM tool_execution_attempts WHERE execution_id=?1 ORDER BY attempt_no")?;
        let rows = st.query_map(params![execution_id], |r| {
            Ok(ToolAttemptRecord {
                attempt_id: r.get(0)?,
                attempt_no: r.get(1)?,
                state: r.get(2)?,
                started_at: r.get(3)?,
                ended_at: r.get(4)?,
                duration_ms: r.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
    pub fn get_tool_execution(&self, execution_id: &str) -> Result<ToolExecution, StoreError> {
        let c = self.conn.lock().unwrap();
        c.query_row("SELECT execution_id,tool_call_id,name,arguments,state,revision,result,active_attempt_id FROM tool_executions WHERE execution_id=?1",params![execution_id],|r|Ok(ToolExecution{execution_id:r.get(0)?,tool_call_id:r.get(1)?,name:r.get(2)?,arguments:r.get(3)?,state:r.get(4)?,revision:r.get(5)?,result:r.get(6)?,active_attempt_id:r.get(7)?})).optional()?.ok_or_else(||StoreError::NotFound(execution_id.into()))
    }
    pub fn tool_journal(&self, execution_id: &str) -> Result<ToolJournalRecord, StoreError> {
        let execution = self.get_tool_execution(execution_id)?;
        let c = self.conn.lock().unwrap();
        let (error, source): (Option<String>, Option<String>) = c.query_row(
            "SELECT error,result_source FROM tool_executions WHERE execution_id=?1",
            params![execution_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let mut st=c.prepare("SELECT attempt_id,attempt_no,state,started_at,ended_at,duration_ms FROM tool_execution_attempts WHERE execution_id=?1 ORDER BY attempt_no")?;
        let rows = st.query_map(params![execution_id], |r| {
            Ok(ToolAttemptRecord {
                attempt_id: r.get(0)?,
                attempt_no: r.get(1)?,
                state: r.get(2)?,
                started_at: r.get(3)?,
                ended_at: r.get(4)?,
                duration_ms: r.get(5)?,
            })
        })?;
        let mut attempts = Vec::new();
        for r in rows {
            attempts.push(r?);
        }
        Ok(ToolJournalRecord {
            execution,
            error,
            result_source: source,
            attempts,
        })
    }
    pub fn fail_tool_attempt(
        &self,
        token: &RunToken,
        execution_id: &str,
        attempt_id: &str,
        error: &str,
        duration_ms: i64,
    ) -> Result<(), StoreError> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        Self::check_owner(&tx, token)?;
        let n=tx.execute("UPDATE tool_executions SET state='failed',error=?1,revision=revision+1 WHERE execution_id=?2 AND run_id=?3 AND active_attempt_id=?4 AND state='running'",params![error,execution_id,token.run_id,attempt_id])?;
        if n == 0 {
            return Err(StoreError::InvalidSessionState(
                "tool attempt is stale or not running".into(),
            ));
        }
        tx.execute("UPDATE tool_execution_attempts SET state='failed',ended_at=?1,duration_ms=?2 WHERE attempt_id=?3",params![now(),duration_ms.max(0),attempt_id])?;
        tx.commit()?;
        Ok(())
    }
    pub fn commit_tool_failure_and_message(
        &self,
        token: &RunToken,
        execution_id: &str,
        attempt_id: &str,
        error: &str,
        tool_message_json: &str,
        duration_ms: i64,
    ) -> Result<(), StoreError> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        Self::check_owner(&tx, token)?;
        let n=tx.execute("UPDATE tool_executions SET state='failed',error=?1,result=?5,revision=revision+1 WHERE execution_id=?2 AND run_id=?3 AND active_attempt_id=?4 AND state='running'",params![error,execution_id,token.run_id,attempt_id,tool_message_json])?;
        if n == 0 {
            return Err(StoreError::InvalidSessionState(
                "tool attempt is stale or not running".into(),
            ));
        }
        tx.execute("UPDATE tool_execution_attempts SET state='failed',ended_at=?1,duration_ms=?2 WHERE attempt_id=?3 AND state='running'",params![now(),duration_ms.max(0),attempt_id])?;
        if let Some(cid) = linked_conversation(&tx, &token.run_id)? {
            append_runtime_message(&tx, &cid, tool_message_json)?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn tool_executions_for_run(&self, run_id: &str) -> Result<Vec<ToolExecution>, StoreError> {
        let c = self.conn.lock().unwrap();
        let mut st=c.prepare("SELECT execution_id,tool_call_id,name,arguments,state,revision,result,active_attempt_id FROM tool_executions WHERE run_id=?1 ORDER BY rowid")?;
        let rows = st.query_map(params![run_id], |r| {
            Ok(ToolExecution {
                execution_id: r.get(0)?,
                tool_call_id: r.get(1)?,
                name: r.get(2)?,
                arguments: r.get(3)?,
                state: r.get(4)?,
                revision: r.get(5)?,
                result: r.get(6)?,
                active_attempt_id: r.get(7)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn conversation_runtime_metrics(
        &self,
        conversation_id: &str,
    ) -> Result<ConversationRuntimeMetrics, StoreError> {
        let c = self.conn.lock().unwrap();
        let runs = self.conversation_runs_locked(&c, conversation_id)?;
        let mut model_attempts = Vec::new();
        {
            let mut st=c.prepare("SELECT request_id,run_id,session_id,model,state,reason,started_at,ended_at,duration_ms,timing_complete,usage_json FROM runtime_model_attempts WHERE run_id IN (SELECT run_id FROM agent_runs WHERE conversation_id=?1) ORDER BY started_at")?;
            let rows = st.query_map(params![conversation_id], |r| {
                let usage: Option<String> = r.get(10)?;
                Ok(ModelAttemptRecord {
                    request_id: r.get(0)?,
                    run_id: r.get(1)?,
                    session_id: r.get(2)?,
                    model: r.get(3)?,
                    state: r.get(4)?,
                    reason: r.get(5)?,
                    started_at: r.get(6)?,
                    ended_at: r.get(7)?,
                    duration_ms: r.get(8)?,
                    timing_complete: r.get::<_, i64>(9)? != 0,
                    usage: usage
                        .map(|x| serde_json::from_str(&x))
                        .transpose()
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                })
            })?;
            for r in rows {
                model_attempts.push(r?);
            }
        }
        let mut tool_executions = Vec::new();
        {
            let mut st=c.prepare("SELECT execution_id,tool_call_id,name,arguments,state,revision,result,active_attempt_id FROM tool_executions WHERE run_id IN (SELECT run_id FROM agent_runs WHERE conversation_id=?1) ORDER BY rowid")?;
            let rows = st.query_map(params![conversation_id], |r| {
                Ok(ToolExecution {
                    execution_id: r.get(0)?,
                    tool_call_id: r.get(1)?,
                    name: r.get(2)?,
                    arguments: r.get(3)?,
                    state: r.get(4)?,
                    revision: r.get(5)?,
                    result: r.get(6)?,
                    active_attempt_id: r.get(7)?,
                })
            })?;
            for r in rows {
                tool_executions.push(r?);
            }
        }
        let mut tool_attempts = Vec::new();
        for e in &tool_executions {
            let mut st=c.prepare("SELECT attempt_id,attempt_no,state,started_at,ended_at,duration_ms FROM tool_execution_attempts WHERE execution_id=?1 ORDER BY attempt_no")?;
            let rows = st.query_map(params![e.execution_id], |r| {
                Ok(ToolAttemptRecord {
                    attempt_id: r.get(0)?,
                    attempt_no: r.get(1)?,
                    state: r.get(2)?,
                    started_at: r.get(3)?,
                    ended_at: r.get(4)?,
                    duration_ms: r.get(5)?,
                })
            })?;
            for r in rows {
                tool_attempts.push(r?);
            }
        }
        Ok(ConversationRuntimeMetrics {
            runs,
            model_attempts,
            tool_executions,
            tool_attempts,
        })
    }
    fn conversation_runs_locked(
        &self,
        c: &rusqlite::Connection,
        id: &str,
    ) -> Result<Vec<RunRecord>, StoreError> {
        let mut st=c.prepare("SELECT run_id,scope,status,owner_generation,reason,started_at,ended_at,elapsed_ms,timing_complete FROM agent_runs WHERE conversation_id=?1 ORDER BY started_at")?;
        let rows = st.query_map(params![id], |r| {
            Ok(RunRecord {
                run_id: r.get(0)?,
                scope: parse_scope(&r.get::<_, String>(1)?),
                status: r.get(2)?,
                owner_generation: r.get(3)?,
                reason: r.get(4)?,
                started_at: r.get(5)?,
                ended_at: r.get(6)?,
                elapsed_ms: r.get(7)?,
                timing_complete: r.get::<_, i64>(8)? != 0,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

impl HarnessStore {
    pub fn checkpoint_model_attempt(
        &self,
        token: &RunToken,
        request_id: &str,
        usage_json: &str,
        duration_ms: i64,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let (_, status, generation, _) = Self::check_owner(&tx, token)?;
        if !["running", "cancelling"].contains(&status.as_str()) {
            return Err(StoreError::InvalidSessionState(
                "model attempt is not active".into(),
            ));
        }
        let owner: Option<(String, i64)> = tx.query_row(
            "SELECT run_id,owner_generation FROM runtime_model_attempts WHERE request_id=?1 AND state='running'",
            params![request_id], |r| Ok((r.get(0)?, r.get(1)?)),
        ).optional()?;
        let Some((run_id, attempt_generation)) = owner else {
            return Err(StoreError::InvalidSessionState(
                "model attempt is not active".into(),
            ));
        };
        if run_id != token.run_id || attempt_generation != generation {
            return Err(StoreError::FencingViolation {
                expected: token.owner_generation,
                actual: attempt_generation,
            });
        }
        tx.execute(
            "UPDATE runtime_model_attempts SET usage_json=?1,duration_ms=MAX(COALESCE(duration_ms,0),?2),timing_complete=0 WHERE request_id=?3 AND state='running'",
            params![usage_json, duration_ms.max(0), request_id],
        )?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn store() -> HarnessStore {
        let d = tempdir().unwrap();
        let path = d.keep();
        HarnessStore::in_memory(path.join("artifacts")).unwrap()
    }
    fn conversation(s: &HarnessStore, id: &str) {
        s.create_conversation(id, "/tmp", "m", "session").unwrap();
    }

    #[test]
    fn admission_and_settlement_are_serialized_and_followup_starts_once() {
        let s = store();
        conversation(&s, "c");
        let a = s
            .admit_input(
                RuntimeScope::Conversation("c".into()),
                "first",
                InputKind::Submit,
                "hello",
            )
            .unwrap();
        let t = s
            .start_run(
                RuntimeScope::Conversation("c".into()),
                a.run_id.as_deref().unwrap(),
            )
            .unwrap();
        s.admit_input(
            RuntimeScope::Conversation("c".into()),
            "later",
            InputKind::FollowUp,
            "next",
        )
        .unwrap();
        assert!(matches!(
            s.settle_or_consume_steer(&t, "c", 12).unwrap(),
            BoundaryResult::NextRun(_)
        ));
        assert_eq!(s.run_metrics(&t.run_id).unwrap().status, "succeeded");
    }
    #[test]
    fn duplicate_client_id_requires_identical_payload() {
        let s = store();
        conversation(&s, "c");
        let scope = RuntimeScope::Conversation("c".into());
        let first = s
            .admit_input(scope.clone(), "same", InputKind::Submit, "one")
            .unwrap();
        let dup = s
            .admit_input(scope.clone(), "same", InputKind::Submit, "one")
            .unwrap();
        assert!(dup.duplicate);
        assert_eq!(first.run_id, dup.run_id);
        assert!(
            s.admit_input(scope.clone(), "same", InputKind::Submit, "different")
                .is_err()
        );
        assert!(
            s.admit_input(scope, "same", InputKind::Steer, "one")
                .is_err()
        );
    }
    #[test]
    fn running_tool_recovers_unknown_and_never_blindly_replays() {
        let s = store();
        conversation(&s, "c");
        let r = s
            .admit_input(
                RuntimeScope::Conversation("c".into()),
                "r",
                InputKind::Submit,
                "hi",
            )
            .unwrap();
        let t = s
            .start_run(
                RuntimeScope::Conversation("c".into()),
                r.run_id.as_deref().unwrap(),
            )
            .unwrap();
        let e = s
            .prepare_assistant_turn_and_tools(
                &t,
                "c",
                "{\"role\":\"assistant\"}",
                &[ToolSpec {
                    tool_call_id: "call".into(),
                    name: "bash".into(),
                    arguments: "{}".into(),
                    replay_safe: false,
                }],
            )
            .unwrap()
            .remove(0);
        assert!(s.begin_tool(&t, &e.execution_id, "attempt").unwrap());
        assert_eq!(
            s.recover_run(&t.run_id).unwrap()[0].state,
            "outcome_unknown"
        );
        assert!(!s.begin_tool(&t, &e.execution_id, "again").unwrap());
    }
    #[test]
    fn owner_generation_fences_checkpoint_and_finalize_is_idempotent() {
        let s = store();
        conversation(&s, "c");
        let r = s
            .admit_input(
                RuntimeScope::Conversation("c".into()),
                "r",
                InputKind::Submit,
                "hi",
            )
            .unwrap();
        let t = s
            .start_run(
                RuntimeScope::Conversation("c".into()),
                r.run_id.as_deref().unwrap(),
            )
            .unwrap();
        s.settle_run(&t, "interrupted", Some("owner_lost"), 42)
            .unwrap();
        let resumed = s.resume_run(&t.run_id).unwrap();
        assert!(s.checkpoint_run_elapsed(&t, 100).is_err());
        s.settle_run(&resumed, "succeeded", None, 42).unwrap();
        assert!(!s.settle_run(&resumed, "succeeded", None, 999).unwrap());
        assert_eq!(s.run_metrics(&t.run_id).unwrap().elapsed_ms, 42);
    }
    #[test]
    fn tool_checkpoint_commits_result_with_transcript_and_rejects_stale_revision() {
        let s = store();
        conversation(&s, "c");
        let r = s
            .admit_input(
                RuntimeScope::Conversation("c".into()),
                "r",
                InputKind::Submit,
                "hi",
            )
            .unwrap();
        let t = s
            .start_run(
                RuntimeScope::Conversation("c".into()),
                r.run_id.as_deref().unwrap(),
            )
            .unwrap();
        let e = s
            .prepare_assistant_turn_and_tools(
                &t,
                "c",
                "{}",
                &[ToolSpec {
                    tool_call_id: "x".into(),
                    name: "read".into(),
                    arguments: "{}".into(),
                    replay_safe: true,
                }],
            )
            .unwrap()
            .remove(0);
        s.begin_tool(&t, &e.execution_id, "a").unwrap();
        s.commit_tool_result_and_message(&t, &e.execution_id, "a", "ok", "{\"role\":\"tool\"}", 5)
            .unwrap();
        assert!(
            s.commit_tool_result_and_message(&t, &e.execution_id, "a", "bad", "{}", 1)
                .is_err()
        );
        assert_eq!(s.load_conversation("c").unwrap().1.len(), 3);
    }
    #[test]
    fn active_tool_set_survives_reopenable_database_state() {
        let s = store();
        conversation(&s, "c");
        let mut fp = std::collections::HashMap::new();
        fp.insert("read".into(), "schema1".into());
        s.activate_tools("c", &["read".into()], &fp).unwrap();
        assert_eq!(
            s.active_tools("c").unwrap(),
            vec![("read".into(), "schema1".into())]
        );
    }
    #[test]
    fn model_finalize_records_known_usage_and_aggregates_once() {
        let s = store();
        conversation(&s, "c");
        let r = s
            .admit_input(
                RuntimeScope::Conversation("c".into()),
                "r",
                InputKind::Submit,
                "hi",
            )
            .unwrap();
        let t = s
            .start_run(
                RuntimeScope::Conversation("c".into()),
                r.run_id.as_deref().unwrap(),
            )
            .unwrap();
        s.start_model_attempt(&t, "req", "provider-session", "model")
            .unwrap();
        let out = ModelAttemptOutcome {
            state: "succeeded".into(),
            reason: None,
            usage: Some(
                serde_json::json!({"input_tokens":3,"output_tokens":2,"cache_read_tokens":1}),
            ),
            timing_complete: true,
        };
        assert!(s.finish_model_attempt("req", out.clone(), 8).unwrap());
        assert!(!s.finish_model_attempt("req", out, 8).unwrap());
        assert_eq!(
            s.get_session_usage("provider-session")
                .unwrap()
                .total_tokens,
            5
        );
        assert_eq!(
            s.get_session_request_usages("provider-session")
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn model_context_metadata_survives_failed_and_successful_attempts() {
        let s = store();
        conversation(&s, "c");
        let r = s
            .admit_input(
                RuntimeScope::Conversation("c".into()),
                "r",
                InputKind::Submit,
                "hi",
            )
            .unwrap();
        let t = s
            .start_run(
                RuntimeScope::Conversation("c".into()),
                r.run_id.as_deref().unwrap(),
            )
            .unwrap();
        let context = r#"{"session_id":"s","request_id":"x","benchmark_run_id":"bench","case_id":"case","variant":"v","attempt_id":"attempt","agent_id":"agent","parent_agent_id":"parent"}"#;
        for (id, state) in [("req-fail", "failed"), ("req-ok", "succeeded")] {
            s.start_model_attempt(&t, id, "s", "model").unwrap();
            s.set_model_attempt_context(id, context).unwrap();
            s.finish_model_attempt(
                id,
                ModelAttemptOutcome {
                    state: state.into(),
                    reason: None,
                    usage: None,
                    timing_complete: true,
                },
                4,
            )
            .unwrap();
        }
        let rows = s.get_benchmark_request_usages("bench").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].case_id.as_deref(), Some("case"));
        assert_eq!(rows[0].variant.as_deref(), Some("v"));
        assert_eq!(rows[0].attempt_id.as_deref(), Some("attempt"));
        assert_eq!(rows[0].agent_id.as_deref(), Some("agent"));
        assert_eq!(rows[0].parent_agent_id.as_deref(), Some("parent"));
        assert_eq!(rows[0].is_complete, false);
        assert_eq!(rows[1].is_complete, false);
    }
    #[test]
    fn tool_failure_and_message_are_committed_together() {
        let s = store();
        conversation(&s, "c");
        let r = s
            .admit_input(
                RuntimeScope::Conversation("c".into()),
                "r",
                InputKind::Submit,
                "hi",
            )
            .unwrap();
        let t = s
            .start_run(
                RuntimeScope::Conversation("c".into()),
                r.run_id.as_deref().unwrap(),
            )
            .unwrap();
        let e = s
            .prepare_assistant_turn_and_tools(
                &t,
                "c",
                "{}",
                &[ToolSpec {
                    tool_call_id: "x".into(),
                    name: "bash".into(),
                    arguments: "{}".into(),
                    replay_safe: false,
                }],
            )
            .unwrap()
            .remove(0);
        s.begin_tool(&t, &e.execution_id, "a").unwrap();
        s.commit_tool_failure_and_message(
            &t,
            &e.execution_id,
            "a",
            "failed",
            "{\"role\":\"tool\",\"content\":\"failed\"}",
            7,
        )
        .unwrap();
        let journal = s.tool_journal(&e.execution_id).unwrap();
        assert_eq!(journal.execution.state, "failed");
        assert_eq!(journal.error.as_deref(), Some("failed"));
        assert_eq!(journal.attempts[0].duration_ms, Some(7));
        assert_eq!(s.conversation_history("c").unwrap().len(), 3);
    }
    #[test]
    fn scope_lock_excludes_and_releases() {
        let d = tempdir().unwrap();
        let db = d.path().join("store.db");
        let a = HarnessStore::open(&db, d.path().join("a")).unwrap();
        let b = HarnessStore::open(&db, d.path().join("b")).unwrap();
        let scope = RuntimeScope::Conversation("lock-test".into());
        let lock = a.acquire_runtime_scope_lock(&scope).unwrap().unwrap();
        assert!(b.acquire_runtime_scope_lock(&scope).unwrap().is_none());
        drop(lock);
        assert!(b.acquire_runtime_scope_lock(&scope).unwrap().is_some());
    }
    #[test]
    fn failed_run_pauses_followup_and_terminal_owner_cannot_mutate() {
        let s = store();
        conversation(&s, "c");
        let scope = RuntimeScope::Conversation("c".into());
        let r = s
            .admit_input(scope.clone(), "r", InputKind::Submit, "hi")
            .unwrap();
        let t = s
            .start_run(scope.clone(), r.run_id.as_deref().unwrap())
            .unwrap();
        s.admit_input(scope.clone(), "f", InputKind::FollowUp, "later")
            .unwrap();
        s.settle_run(&t, "failed", Some("provider_error"), 10)
            .unwrap();
        assert!(
            s.inbox(&scope, &["paused"])
                .unwrap()
                .iter()
                .any(|x| x.client_request_id == "f")
        );
        assert!(s.checkpoint_run_elapsed(&t, 20).is_err());
        let next = s
            .admit_input(scope, "new", InputKind::Submit, "new run")
            .unwrap();
        let nt = s
            .start_run(
                RuntimeScope::Conversation("c".into()),
                next.run_id.as_deref().unwrap(),
            )
            .unwrap();
        assert_eq!(
            s.inbox(&RuntimeScope::Conversation("c".into()), &["paused"])
                .unwrap()
                .iter()
                .filter(|x| x.client_request_id == "f")
                .count(),
            1
        );
        assert!(s.settle_run(&t, "succeeded", None, 100).is_err());
        assert_eq!(s.run_metrics(&nt.run_id).unwrap().status, "running");
    }
    #[test]
    fn resumed_owner_rejects_late_model_attempt_finalize() {
        let s = store();
        conversation(&s, "c");
        let r = s
            .admit_input(
                RuntimeScope::Conversation("c".into()),
                "r",
                InputKind::Submit,
                "hi",
            )
            .unwrap();
        let t = s
            .start_run(
                RuntimeScope::Conversation("c".into()),
                r.run_id.as_deref().unwrap(),
            )
            .unwrap();
        s.start_model_attempt(&t, "late", "s", "m").unwrap();
        s.settle_run(&t, "interrupted", None, 1).unwrap();
        let _new = s.resume_run(&t.run_id).unwrap();
        assert!(
            s.finish_model_attempt(
                "late",
                ModelAttemptOutcome {
                    state: "succeeded".into(),
                    reason: None,
                    usage: None,
                    timing_complete: true
                },
                5
            )
            .is_err()
        );
    }
    #[test]
    fn attempt_scoped_run_routes_transcript_to_explicit_conversation() {
        let s = store();
        conversation(&s, "worker:attempt");
        s.create_goal("worker-goal", "worker").unwrap();
        s.create_task(&dume_core::Task {
            id: "worker-task".into(),
            goal_id: "worker-goal".into(),
            title: "worker".into(),
            description: String::new(),
            status: dume_core::TaskStatus::Ready,
            dependencies: vec![],
            acceptance_criteria: vec![],
            allowed_paths: None,
            target_branch: "main".into(),
            created_at: 0,
            updated_at: 0,
        })
        .unwrap();
        s.create_attempt("attempt", "worker-task", 1, "worker", "/tmp/worker", 60_000)
            .unwrap();
        s.acquire_coordinator_lock("coordinator", "worker", 60_000)
            .unwrap();
        let scope = RuntimeScope::Attempt("attempt".into());
        let admitted = s
            .admit_input(scope.clone(), "worker-input", InputKind::Submit, "start")
            .unwrap();
        let token = s
            .start_run(scope, admitted.run_id.as_deref().unwrap())
            .unwrap();
        s.attach_run_conversation(&token, "worker:attempt").unwrap();
        s.prepare_assistant_turn_and_tools(
            &token,
            "worker:attempt",
            "{\"role\":\"assistant\"}",
            &[ToolSpec {
                tool_call_id: "call".into(),
                name: "read".into(),
                arguments: "{}".into(),
                replay_safe: true,
            }],
        )
        .unwrap();
        let execution = s.tool_executions_for_run(&token.run_id).unwrap().remove(0);
        s.begin_tool(&token, &execution.execution_id, "tool-attempt")
            .unwrap();
        s.commit_tool_result_and_message(
            &token,
            &execution.execution_id,
            "tool-attempt",
            "ok",
            "{\"role\":\"tool\"}",
            2,
        )
        .unwrap();
        assert_eq!(s.conversation_history("worker:attempt").unwrap().len(), 3);
        assert!(
            s.conversation_runtime_metrics("worker:attempt")
                .unwrap()
                .runs
                .iter()
                .any(|r| r.run_id == token.run_id)
        );
    }

    #[test]
    fn attempt_attach_backfills_initial_submit_once_including_retry() {
        let s = store();
        conversation(&s, "backfill");
        s.create_goal("backfill-goal", "worker").unwrap();
        s.create_task(&dume_core::Task {
            id: "backfill-task".into(),
            goal_id: "backfill-goal".into(),
            title: "worker".into(),
            description: String::new(),
            status: dume_core::TaskStatus::Ready,
            dependencies: vec![],
            acceptance_criteria: vec![],
            allowed_paths: None,
            target_branch: "main".into(),
            created_at: 0,
            updated_at: 0,
        })
        .unwrap();
        s.create_attempt(
            "backfill-attempt",
            "backfill-task",
            1,
            "worker",
            "/tmp/worker",
            60_000,
        )
        .unwrap();
        s.acquire_coordinator_lock("coordinator", "worker", 60_000)
            .unwrap();
        let scope = RuntimeScope::Attempt("backfill-attempt".into());
        let receipt = s
            .admit_input(scope.clone(), "initial", InputKind::Submit, "payload")
            .unwrap();
        let token = s
            .start_run(scope, receipt.run_id.as_deref().unwrap())
            .unwrap();
        assert!(s.conversation_history("backfill").unwrap().is_empty());
        s.attach_run_conversation(&token, "backfill").unwrap();
        let history = s.conversation_history("backfill").unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&history[0].message_json).unwrap(),
            serde_json::json!({"role":"user","content":"payload"})
        );
        s.attach_run_conversation(&token, "backfill").unwrap();
        assert_eq!(s.conversation_history("backfill").unwrap().len(), 1);
    }
    #[test]
    fn replay_safe_tool_requires_native_read_allowlist_and_unknown_state() {
        let s = store();
        conversation(&s, "safe");
        let receipt = s
            .admit_input(
                RuntimeScope::Conversation("safe".into()),
                "r",
                InputKind::Submit,
                "hi",
            )
            .unwrap();
        let token = s
            .start_run(
                RuntimeScope::Conversation("safe".into()),
                receipt.run_id.as_deref().unwrap(),
            )
            .unwrap();
        let safe = s
            .prepare_assistant_turn_and_tools(
                &token,
                "safe",
                "{}",
                &[ToolSpec {
                    tool_call_id: "read".into(),
                    name: "read_file".into(),
                    arguments: "{}".into(),
                    replay_safe: true,
                }],
            )
            .unwrap()
            .remove(0);
        s.begin_tool(&token, &safe.execution_id, "a").unwrap();
        s.mark_tool_unknown(&token, &safe.execution_id, "a", "transport lost", 9)
            .unwrap();
        let replay = s.replay_safe_tool(&token, &safe.execution_id).unwrap();
        assert_eq!(replay.state, "prepared");
        assert_eq!(replay.revision, safe.revision + 3);
        assert_eq!(replay.active_attempt_id, None);
        let unsafe_tool = s
            .prepare_assistant_turn_and_tools(
                &token,
                "safe",
                "{}",
                &[ToolSpec {
                    tool_call_id: "bash".into(),
                    name: "bash".into(),
                    arguments: "{}".into(),
                    replay_safe: true,
                }],
            )
            .unwrap()
            .remove(0);
        s.begin_tool(&token, &unsafe_tool.execution_id, "b")
            .unwrap();
        s.mark_tool_unknown(&token, &unsafe_tool.execution_id, "b", "transport lost", 9)
            .unwrap();
        // A forged persisted policy is insufficient for tools outside the native allowlist.
        assert!(
            s.replay_safe_tool(&token, &unsafe_tool.execution_id)
                .is_err()
        );
    }
    #[test]
    fn uncertain_tool_error_does_not_append_fake_tool_message_and_rejects_stale_attempt() {
        let s = store();
        conversation(&s, "unknown");
        let receipt = s
            .admit_input(
                RuntimeScope::Conversation("unknown".into()),
                "r",
                InputKind::Submit,
                "hi",
            )
            .unwrap();
        let token = s
            .start_run(
                RuntimeScope::Conversation("unknown".into()),
                receipt.run_id.as_deref().unwrap(),
            )
            .unwrap();
        let e = s
            .prepare_assistant_turn_and_tools(
                &token,
                "unknown",
                "{}",
                &[ToolSpec {
                    tool_call_id: "read".into(),
                    name: "read_file".into(),
                    arguments: "{}".into(),
                    replay_safe: true,
                }],
            )
            .unwrap()
            .remove(0);
        s.begin_tool(&token, &e.execution_id, "attempt").unwrap();
        assert!(
            s.mark_tool_unknown(&token, &e.execution_id, "stale", "uncertain", 4)
                .is_err()
        );
        assert_eq!(s.conversation_history("unknown").unwrap().len(), 2);
        s.mark_tool_unknown(&token, &e.execution_id, "attempt", "uncertain", 4)
            .unwrap();
        assert_eq!(s.conversation_history("unknown").unwrap().len(), 2);
        assert_eq!(
            s.tool_journal(&e.execution_id).unwrap().execution.state,
            "outcome_unknown"
        );
    }

    #[test]
    fn submit_is_rejected_for_active_scope_without_inbox_orphan() {
        let s = store();
        conversation(&s, "busy-submit");
        let scope = RuntimeScope::Conversation("busy-submit".into());
        let receipt = s
            .admit_input(scope.clone(), "first", InputKind::Submit, "start")
            .unwrap();
        let _token = s
            .start_run(scope.clone(), receipt.run_id.as_deref().unwrap())
            .unwrap();
        assert!(
            s.admit_input(scope.clone(), "second", InputKind::Submit, "another start")
                .is_err()
        );
        assert!(
            !s.inbox(&scope, &["pending"])
                .unwrap()
                .iter()
                .any(|x| x.client_request_id == "second")
        );
    }

    #[test]
    fn abandoned_checkpoint_preserves_known_usage_metadata_and_is_idempotent() {
        for status in ["cancelled", "interrupted"] {
            let s = store();
            conversation(&s, status);
            let scope = RuntimeScope::Conversation(status.into());
            let r = s
                .admit_input(scope.clone(), "r", InputKind::Submit, "hi")
                .unwrap();
            let t = s.start_run(scope, r.run_id.as_deref().unwrap()).unwrap();
            s.start_model_attempt(&t, "req", "provider-session", "model")
                .unwrap();
            s.set_model_attempt_context("req",r#"{"benchmark_run_id":"bench","case_id":"case","variant":"v","attempt_id":"attempt","agent_id":"agent","parent_agent_id":"parent"}"#).unwrap();
            let raw = serde_json::json!({"vendor":"snapshot"});
            let checkpoint = serde_json::json!({"input_tokens":7,"output_tokens":2,"total_tokens":9,"cache_read_tokens":3,"raw_usage":raw,"is_complete":true});
            s.checkpoint_model_attempt(&t, "req", &checkpoint.to_string(), 33)
                .unwrap();
            s.settle_run(&t, status, None, 40).unwrap();
            s.recover_run(&t.run_id).unwrap();
            let metrics = s.conversation_runtime_metrics(status).unwrap();
            let attempt = metrics
                .model_attempts
                .iter()
                .find(|x| x.request_id == "req")
                .unwrap();
            assert_eq!(attempt.duration_ms, Some(33));
            assert!(!attempt.timing_complete);
            assert_eq!(attempt.usage.as_ref().unwrap()["is_complete"], false);
            let usage = s.get_session_request_usages("provider-session").unwrap();
            assert_eq!(usage.len(), 1);
            let u = &usage[0];
            assert_eq!((u.input_tokens, u.output_tokens, u.total_tokens), (7, 2, 9));
            assert!(!u.is_complete);
            assert_eq!(
                u.raw_usage_json.as_deref(),
                Some(r#"{"vendor":"snapshot"}"#)
            );
            assert_eq!(u.benchmark_run_id.as_deref(), Some("bench"));
            assert_eq!(u.case_id.as_deref(), Some("case"));
            assert_eq!(u.variant.as_deref(), Some("v"));
            assert_eq!(u.attempt_id.as_deref(), Some("attempt"));
            assert_eq!(u.agent_id.as_deref(), Some("agent"));
            assert_eq!(u.parent_agent_id.as_deref(), Some("parent"));
            s.recover_run(&t.run_id).unwrap();
            assert_eq!(
                s.get_session_usage("provider-session")
                    .unwrap()
                    .total_tokens,
                9
            );
            assert_eq!(
                s.get_session_request_usages("provider-session")
                    .unwrap()
                    .len(),
                1
            );
        }
    }

    #[test]
    fn same_owner_can_finish_during_cancellation_with_exact_usage() {
        let s = store();
        conversation(&s, "cancelling");
        let r = s
            .admit_input(
                RuntimeScope::Conversation("cancelling".into()),
                "r",
                InputKind::Submit,
                "hi",
            )
            .unwrap();
        let t = s
            .start_run(
                RuntimeScope::Conversation("cancelling".into()),
                r.run_id.as_deref().unwrap(),
            )
            .unwrap();
        s.start_model_attempt(&t, "req", "s", "m").unwrap();
        s.cancel_run(&t).unwrap();
        let usage = serde_json::json!({"input_tokens":7,"output_tokens":2,"total_tokens":9,"is_complete":true,"raw_usage":{"vendor":"exact"}});
        assert!(
            s.finish_model_attempt(
                "req",
                ModelAttemptOutcome {
                    state: "succeeded".into(),
                    reason: None,
                    usage: Some(usage),
                    timing_complete: true
                },
                33
            )
            .unwrap()
        );
        let request = s.get_session_request_usages("s").unwrap().remove(0);
        assert_eq!(
            (
                request.input_tokens,
                request.output_tokens,
                request.total_tokens
            ),
            (7, 2, 9)
        );
        assert_eq!(
            request.raw_usage_json.as_deref(),
            Some(r#"{"vendor":"exact"}"#)
        );
        assert!(request.is_complete);
    }

    #[test]
    fn tool_plan_is_idempotent_and_mismatched_duplicate_rolls_back_assistant_message() {
        let s = store();
        conversation(&s, "tool-idempotence");
        let r = s
            .admit_input(
                RuntimeScope::Conversation("tool-idempotence".into()),
                "r",
                InputKind::Submit,
                "hi",
            )
            .unwrap();
        let t = s
            .start_run(
                RuntimeScope::Conversation("tool-idempotence".into()),
                r.run_id.as_deref().unwrap(),
            )
            .unwrap();
        let specs = [ToolSpec {
            tool_call_id: "call".into(),
            name: "read".into(),
            arguments: r#"{"path":"a"}"#.into(),
            replay_safe: true,
        }];
        let first = s
            .prepare_assistant_turn_and_tools(
                &t,
                "tool-idempotence",
                r#"{"role":"assistant","tool_calls":[]}"#,
                &specs,
            )
            .unwrap();
        assert!(s.begin_tool(&t, &first[0].execution_id, "once").unwrap());
        s.commit_tool_result_and_message(
            &t,
            &first[0].execution_id,
            "once",
            "result",
            r#"{"role":"tool","content":"result"}"#,
            3,
        )
        .unwrap();
        let completed = s.tool_executions_for_run(&t.run_id).unwrap();
        let again = s
            .prepare_assistant_turn_and_tools(
                &t,
                "tool-idempotence",
                r#"{"role":"assistant","tool_calls":[]}"#,
                &specs,
            )
            .unwrap();
        assert_eq!(completed, again);
        assert!(!s.begin_tool(&t, &again[0].execution_id, "twice").unwrap());
        assert_eq!(s.conversation_history("tool-idempotence").unwrap().len(), 3);
        let mismatch = [ToolSpec {
            arguments: r#"{"path":"b"}"#.into(),
            ..specs[0].clone()
        }];
        assert!(
            s.prepare_assistant_turn_and_tools(
                &t,
                "tool-idempotence",
                r#"{"role":"assistant","tool_calls":[]}"#,
                &mismatch
            )
            .is_err()
        );
        assert_eq!(s.conversation_history("tool-idempotence").unwrap().len(), 3);
    }

    #[test]
    fn mixed_new_and_saved_tool_ids_roll_back_entire_plan() {
        let s = store();
        conversation(&s, "mixed-plan");
        let scope = RuntimeScope::Conversation("mixed-plan".into());
        let receipt = s
            .admit_input(scope.clone(), "request", InputKind::Submit, "read")
            .unwrap();
        let token = s
            .start_run(scope, receipt.run_id.as_deref().unwrap())
            .unwrap();
        let original = ToolSpec {
            tool_call_id: "original".into(),
            name: "read_file".into(),
            arguments: "{}".into(),
            replay_safe: true,
        };
        let assistant = r#"{"role":"assistant","tool_calls":[]}"#;
        let saved = s
            .prepare_assistant_turn_and_tools(&token, "mixed-plan", assistant, &[original.clone()])
            .unwrap();
        let mixed = [
            ToolSpec {
                tool_call_id: "new".into(),
                ..original.clone()
            },
            original.clone(),
        ];
        assert!(
            s.prepare_assistant_turn_and_tools(&token, "mixed-plan", assistant, &mixed)
                .is_err()
        );
        assert_eq!(s.tool_executions_for_run(&token.run_id).unwrap(), saved);
        assert_eq!(s.conversation_history("mixed-plan").unwrap().len(), 2);
        assert_eq!(
            s.prepare_assistant_turn_and_tools(&token, "mixed-plan", assistant, &[original])
                .unwrap(),
            saved
        );
        assert_eq!(s.conversation_history("mixed-plan").unwrap().len(), 2);
    }

    #[test]
    fn pending_payload_quota_counts_utf8_bytes_and_rejection_preserves_input() {
        let s = store();
        conversation(&s, "byte-quota");
        let scope = RuntimeScope::Conversation("byte-quota".into());
        let run = s
            .admit_input(scope.clone(), "start", InputKind::Submit, "start")
            .unwrap();
        let _token = s
            .start_run(scope.clone(), run.run_id.as_deref().unwrap())
            .unwrap();
        let first = "가".repeat(128 * 1024);
        s.admit_input(scope.clone(), "first", InputKind::Steer, &first)
            .unwrap();
        let payload = "가".repeat(128 * 1024);
        assert!(
            s.admit_input(scope.clone(), "over-limit", InputKind::FollowUp, &payload)
                .is_err()
        );
        let entries = s.inbox(&scope, &["pending"]).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].payload, first);
    }
}

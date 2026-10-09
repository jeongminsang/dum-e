use anyhow::{Result, anyhow};
use dume_provider::types::TokenUsage;
use dume_provider::{AttemptOutcome, ChatMessage, StreamEvent};
use dume_store::{
    BoundaryResult, HarnessStore, ModelAttemptOutcome, RunToken, RuntimeScope, ToolExecution,
    ToolSpec,
};
use std::fs::File;
use std::future::Future;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStart {
    Execute { attempt_id: String },
    Cached { result: String },
}

/// Shared durable lifecycle and timing state for one agent run.
pub struct RunSession {
    store: Arc<HarnessStore>,
    token: RunToken,
    scope: RuntimeScope,
    conversation_id: String,
    elapsed_base_ms: i64,
    active_since: Option<Instant>,
    active_attempt: std::sync::Mutex<Option<ActiveModelAttempt>>,
    _scope_lock: Option<File>,
}

struct ActiveModelAttempt {
    request_id: String,
    started: Instant,
    usage: TokenUsage,
}

impl RunSession {
    pub fn start(
        store: Arc<HarnessStore>,
        scope: RuntimeScope,
        run_id: &str,
        conversation_id: &str,
    ) -> Result<Self> {
        let lock = store.acquire_runtime_scope_lock(&scope)?;
        if lock.is_none() && store.db_path != std::path::Path::new(":memory:") {
            return Err(anyhow!("runtime scope is already owned by another process"));
        }
        let token = store.start_run(scope.clone(), run_id)?;
        let session = Self {
            store,
            token,
            scope: scope.clone(),
            conversation_id: conversation_id.to_owned(),
            elapsed_base_ms: 0,
            active_since: Some(Instant::now()),
            active_attempt: std::sync::Mutex::new(None),
            _scope_lock: lock,
        };
        session
            .store
            .attach_run_conversation(&session.token, &session.conversation_id)?;
        Ok(session)
    }

    pub fn resume(store: Arc<HarnessStore>, run_id: &str, conversation_id: &str) -> Result<Self> {
        let record = store.get_run(run_id)?;
        let scope = record.scope;
        let lock = store.acquire_runtime_scope_lock(&scope)?;
        if lock.is_none() && store.db_path != std::path::Path::new(":memory:") {
            return Err(anyhow!("runtime scope is already owned by another process"));
        }
        store.recover_run(run_id)?;
        let elapsed_base_ms = store.run_metrics(run_id)?.elapsed_ms;
        let token = store.resume_run(run_id)?;
        let session = Self {
            store,
            token,
            scope,
            conversation_id: conversation_id.to_owned(),
            elapsed_base_ms,
            active_since: Some(Instant::now()),
            active_attempt: std::sync::Mutex::new(None),
            _scope_lock: lock,
        };
        session
            .store
            .attach_run_conversation(&session.token, &session.conversation_id)?;
        for execution in session.store.tool_executions_for_run(run_id)? {
            if execution.state == "outcome_unknown" {
                match session
                    .store
                    .replay_safe_tool(&session.token, &execution.execution_id)
                {
                    Ok(_) => {}
                    Err(error) => {
                        let detail = format!(
                            "tool '{}' ({}) has unknown outcome and cannot be safely replayed: {error}",
                            execution.name, execution.tool_call_id
                        );
                        session.settle("needs_attention", Some(&detail))?;
                        return Err(anyhow!(detail));
                    }
                }
            }
        }
        Ok(session)
    }

    pub fn token(&self) -> &RunToken {
        &self.token
    }

    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    pub fn elapsed_ms(&self) -> i64 {
        self.elapsed_base_ms.saturating_add(
            self.active_since
                .map(|started| started.elapsed().as_millis().min(i64::MAX as u128) as i64)
                .unwrap_or(0),
        )
    }

    /// Stop counting active time while the UI is blocked awaiting external input.
    pub fn pause_timing(&mut self) {
        if let Some(started) = self.active_since.take() {
            self.elapsed_base_ms = self
                .elapsed_base_ms
                .saturating_add(started.elapsed().as_millis().min(i64::MAX as u128) as i64);
        }
    }

    /// Resume the active segment after a blocked UI interval.
    pub fn resume_timing(&mut self) {
        if self.active_since.is_none() {
            self.active_since = Some(Instant::now());
        }
    }

    pub fn checkpoint(&self) -> Result<()> {
        self.store
            .checkpoint_run_elapsed(&self.token, self.elapsed_ms())?;
        Ok(())
    }

    /// Persist active elapsed time while an asynchronous operation is pending.
    /// On a storage/fencing failure, cancel and await the operation before returning.
    pub async fn with_checkpoints<T, F>(
        &self,
        cancellation: &CancellationToken,
        future: F,
    ) -> Result<T>
    where
        F: Future<Output = Result<T>>,
    {
        tokio::pin!(future);
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;
        loop {
            tokio::select! {
                biased;
                result = &mut future => return result,
                _ = interval.tick() => {
                    if let Err(error) = self.checkpoint() {
                        cancellation.cancel();
                        let _ = future.await;
                        return Err(error);
                    }
                }
            }
        }
    }

    pub fn record_model_event(&self, event: &StreamEvent) -> Result<()> {
        match event {
            StreamEvent::AttemptStarted { context, model } => {
                self.store.start_model_attempt(
                    &self.token,
                    &context.request_id,
                    &context.session_id,
                    model,
                )?;
                self.store.set_model_attempt_context(
                    &context.request_id,
                    &serde_json::to_string(context)?,
                )?;
                *self.active_attempt.lock().unwrap() = Some(ActiveModelAttempt {
                    request_id: context.request_id.clone(),
                    started: Instant::now(),
                    usage: TokenUsage::default(),
                });
            }
            StreamEvent::Usage(usage) => {
                let mut active = self.active_attempt.lock().unwrap();
                if let Some(attempt) = active.as_mut() {
                    attempt.usage.merge_cumulative(usage);
                    let checkpoint = serde_json::json!({
                        "input_tokens": attempt.usage.input_tokens,
                        "output_tokens": attempt.usage.output_tokens,
                        "total_tokens": attempt.usage.total_tokens,
                        "cache_read_tokens": attempt.usage.cache_read_tokens.unwrap_or(0),
                        "cache_write_tokens": attempt.usage.cache_write_tokens.unwrap_or(0),
                        "raw_usage": attempt.usage.raw_usage,
                        "is_complete": false,
                    });
                    self.store.checkpoint_model_attempt(
                        &self.token,
                        &attempt.request_id,
                        &checkpoint.to_string(),
                        attempt.started.elapsed().as_millis().min(i64::MAX as u128) as i64,
                    )?;
                }
            }
            StreamEvent::AttemptFinished {
                context,
                outcome,
                duration_ms,
                usage,
                error,
                ..
            } => {
                let usage = usage.as_ref().map(|usage| {
                    serde_json::json!({
                        "input_tokens": usage.input_tokens,
                        "output_tokens": usage.output_tokens,
                        "total_tokens": usage.total_tokens,
                        "cache_read_tokens": usage.cache_read_tokens.unwrap_or(0),
                        "cache_write_tokens": usage.cache_write_tokens.unwrap_or(0),
                        "raw_usage": usage.raw_usage,
                        "is_complete": usage.is_complete,
                    })
                });
                self.store.finish_model_attempt(
                    &context.request_id,
                    ModelAttemptOutcome {
                        state: match outcome {
                            AttemptOutcome::Succeeded => "succeeded",
                            AttemptOutcome::Failed => "failed",
                        }
                        .to_owned(),
                        reason: error.clone(),
                        usage,
                        timing_complete: true,
                    },
                    (*duration_ms).min(i64::MAX as u64) as i64,
                )?;
                self.active_attempt.lock().unwrap().take();
            }
            _ => {}
        }
        Ok(())
    }

    pub fn prepare_tools(&self, message: &ChatMessage) -> Result<Vec<ToolExecution>> {
        let calls = message.tool_calls.as_deref().unwrap_or_default();
        let specs: Vec<_> = calls
            .iter()
            .map(|call| ToolSpec {
                tool_call_id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
                replay_safe: replay_safe_tool(&call.name),
            })
            .collect();
        let serialized = serde_json::to_string(message)?;
        Ok(self.store.prepare_assistant_turn_and_tools(
            &self.token,
            &self.conversation_id,
            &serialized,
            &specs,
        )?)
    }

    /// Persist a normal assistant reply that does not contain tool calls.
    pub fn record_assistant(&self, message: &ChatMessage) -> Result<()> {
        if message
            .tool_calls
            .as_ref()
            .is_some_and(|calls| !calls.is_empty())
        {
            return Err(anyhow!(
                "record_assistant cannot persist a tool-call message"
            ));
        }
        self.prepare_tools(message)?;
        Ok(())
    }

    /// Mark an executing tool as outcome-unknown when the engine cannot
    /// establish whether its side effect completed.
    pub fn mark_tool_unknown(
        &self,
        execution: &ToolExecution,
        attempt_id: &str,
        reason: &str,
        duration_ms: i64,
    ) -> Result<()> {
        self.store.mark_tool_unknown(
            &self.token,
            &execution.execution_id,
            attempt_id,
            reason,
            duration_ms.max(0),
        )?;
        Ok(())
    }

    pub fn begin_tool(&self, execution: &ToolExecution) -> Result<ToolStart> {
        let current = self.store.get_tool_execution(&execution.execution_id)?;
        let attempt_id = current
            .active_attempt_id
            .clone()
            .unwrap_or_else(|| format!("{}:{}", current.execution_id, current.revision + 1));
        if !self
            .store
            .begin_tool(&self.token, &current.execution_id, &attempt_id)?
        {
            let latest = self.store.get_tool_execution(&current.execution_id)?;
            if matches!(latest.state.as_str(), "succeeded" | "failed") {
                return Ok(ToolStart::Cached {
                    result: latest
                        .result
                        .ok_or_else(|| anyhow!("completed tool execution has no stored result"))?,
                });
            }
            if matches!(latest.state.as_str(), "outcome_unknown" | "running") {
                return Err(anyhow!(
                    "tool outcome is unknown; operator resolution required"
                ));
            }
            return Err(anyhow!("tool execution is not eligible to run"));
        }
        Ok(ToolStart::Execute { attempt_id })
    }

    pub fn finish_tool(
        &self,
        execution: &ToolExecution,
        attempt_id: &str,
        message: &ChatMessage,
        duration_ms: i64,
    ) -> Result<()> {
        let serialized = serde_json::to_string(message)?;
        self.store.commit_tool_result_and_message(
            &self.token,
            &execution.execution_id,
            attempt_id,
            &serialized,
            &serialized,
            duration_ms.max(0),
        )?;
        Ok(())
    }

    pub fn finish_tool_error(
        &self,
        execution: &ToolExecution,
        attempt_id: &str,
        message: &ChatMessage,
        duration_ms: i64,
    ) -> Result<()> {
        let error = message.content.clone();
        let serialized = serde_json::to_string(message)?;
        self.store.commit_tool_failure_and_message(
            &self.token,
            &execution.execution_id,
            attempt_id,
            &error,
            &serialized,
            duration_ms.max(0),
        )?;
        Ok(())
    }

    pub fn boundary(&self) -> Result<BoundaryResult> {
        Ok(self.store.settle_or_consume_steer(
            &self.token,
            &self.conversation_id,
            self.elapsed_ms(),
        )?)
    }

    pub fn consume_steer(&self) -> Result<Vec<dume_store::QueuedInput>> {
        Ok(self
            .store
            .consume_inputs(&self.token, &self.conversation_id)?)
    }

    /// Start a queued follow-up run while retaining the scope lock owned by this session.
    pub fn advance(&mut self, next_run_id: &str) -> Result<()> {
        let next = self.store.get_run(next_run_id)?;
        if next.scope != self.scope || next.status != "queued" {
            return Err(anyhow!("next run is not queued in this runtime scope"));
        }
        if self.store.get_run(&self.token.run_id)?.status != "succeeded" {
            return Err(anyhow!("current run must be terminal before advancing"));
        }
        self.token = self.store.start_run(self.scope.clone(), next_run_id)?;
        self.elapsed_base_ms = 0;
        self.active_since = Some(Instant::now());
        *self.active_attempt.lock().unwrap() = None;
        self.store
            .attach_run_conversation(&self.token, &self.conversation_id)?;
        Ok(())
    }

    pub fn settle(&self, status: &str, reason: Option<&str>) -> Result<()> {
        self.store
            .settle_run(&self.token, status, reason, self.elapsed_ms())?;
        Ok(())
    }

    pub fn store(&self) -> &Arc<HarnessStore> {
        &self.store
    }
}

impl Drop for RunSession {
    fn drop(&mut self) {
        let status = match self.store.run_metrics(&self.token.run_id) {
            Ok(metrics) => metrics.status,
            Err(error) => {
                tracing::error!(run_id = %self.token.run_id, %error, "failed to inspect run while dropping session");
                return;
            }
        };
        if !matches!(status.as_str(), "running" | "cancelling") {
            return;
        }
        if let Err(error) = self
            .store
            .checkpoint_run_elapsed(&self.token, self.elapsed_ms())
        {
            tracing::error!(run_id = %self.token.run_id, %error, "failed to checkpoint dropped run");
        }
        if let Err(error) = self.store.settle_run(
            &self.token,
            "interrupted",
            Some("session_dropped"),
            self.elapsed_ms(),
        ) {
            tracing::error!(run_id = %self.token.run_id, %error, "failed to mark dropped run interrupted");
        }
    }
}

fn replay_safe_tool(name: &str) -> bool {
    matches!(name, "read_file" | "read_artifact" | "grep_search")
}

#[cfg(test)]
mod tests {
    use super::*;
    use dume_store::{InputKind, RuntimeScope};

    fn running_session() -> (tempfile::TempDir, Arc<HarnessStore>, RunSession) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(HarnessStore::in_memory(dir.path().join("artifacts")).unwrap());
        store
            .create_conversation("conversation", "/tmp", "model", "session")
            .unwrap();
        let scope = RuntimeScope::Conversation("conversation".into());
        let receipt = store
            .admit_input(scope.clone(), "input", InputKind::Submit, "hello")
            .unwrap();
        let run_id = receipt.run_id.unwrap();
        let session = RunSession::start(store.clone(), scope, &run_id, "conversation").unwrap();
        (dir, store, session)
    }

    #[test]
    fn followup_advance_reuses_session_and_consumes_steer_without_settling() {
        let (_dir, store, mut session) = running_session();
        let scope = RuntimeScope::Conversation("conversation".into());
        store
            .admit_input(scope, "follow-up", InputKind::FollowUp, "next task")
            .unwrap();
        let steer = store
            .admit_input(
                RuntimeScope::Conversation("conversation".into()),
                "steer",
                InputKind::Steer,
                "change direction",
            )
            .unwrap();
        let consumed = session.consume_steer().unwrap();
        assert_eq!(consumed.len(), 1);
        assert_eq!(consumed[0].payload, "change direction");
        assert_eq!(
            store.run_metrics(&session.token().run_id).unwrap().status,
            "running"
        );
        let next = session.boundary().unwrap();
        let dume_store::BoundaryResult::NextRun(token) = next else {
            panic!("follow-up expected")
        };
        assert_eq!(store.conversation_history("conversation").unwrap().len(), 3);
        let previous_run = session.token().run_id.clone();
        session.advance(&token.run_id).unwrap();
        assert_ne!(session.token().run_id, previous_run);
        assert_eq!(session.token().run_id, token.run_id);
        assert_eq!(
            store.run_metrics(&session.token().run_id).unwrap().status,
            "running"
        );
        assert!(
            store
                .inbox(
                    &RuntimeScope::Conversation("conversation".into()),
                    &["consumed"]
                )
                .unwrap()
                .iter()
                .any(|input| input.input_id == steer.input_id)
        );
        store
            .attach_run_conversation(session.token(), session.conversation_id())
            .unwrap();
        let history = store
            .conversation_history("conversation")
            .unwrap()
            .into_iter()
            .map(|row| serde_json::from_str::<ChatMessage>(&row.message_json).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            history,
            vec![
                ChatMessage::user("hello"),
                ChatMessage::user("change direction"),
                ChatMessage::user("next task")
            ]
        );
    }

    #[test]
    fn operator_resolution_reuses_observation_or_exact_authorized_attempt() {
        for decision in ["observed_succeeded", "observed_not_run", "retry_authorized"] {
            let (_directory, store, session) = running_session();
            let assistant = ChatMessage::assistant_with_tool_calls(
                "",
                vec![dume_provider::ToolCall {
                    id: "operator".into(),
                    name: "bash".into(),
                    arguments: r#"{"command":"true"}"#.into(),
                }],
            );
            let execution = session.prepare_tools(&assistant).unwrap().remove(0);
            let ToolStart::Execute { attempt_id } = session.begin_tool(&execution).unwrap() else {
                panic!("execution expected")
            };
            session
                .mark_tool_unknown(&execution, &attempt_id, "result lost", 0)
                .unwrap();
            let unknown = store.get_tool_execution(&execution.execution_id).unwrap();
            store
                .resolve_unknown(
                    session.token(),
                    &execution.execution_id,
                    unknown.revision,
                    dume_store::ToolResolution {
                        operator: "test operator".into(),
                        kind: decision.into(),
                        evidence: "confirmed_not_run: test executor was never launched".into(),
                        result: (decision == "observed_succeeded")
                            .then(|| "observed result".into()),
                        duplicate_effect_warning: true,
                    },
                )
                .unwrap();
            if decision == "observed_succeeded" {
                let ToolStart::Cached { result } = session.begin_tool(&execution).unwrap() else {
                    panic!("observation must be reused")
                };
                let message = serde_json::from_str::<ChatMessage>(&result).unwrap();
                assert_eq!(message, ChatMessage::tool("observed result", "operator"));
                assert_eq!(
                    store
                        .tool_journal(&execution.execution_id)
                        .unwrap()
                        .attempts
                        .len(),
                    1
                );
                assert_eq!(
                    store
                        .conversation_history("conversation")
                        .unwrap()
                        .last()
                        .unwrap()
                        .message_json,
                    result
                );
            } else {
                let approved = store
                    .get_tool_execution(&execution.execution_id)
                    .unwrap()
                    .active_attempt_id
                    .unwrap();
                assert!(
                    store
                        .begin_tool(session.token(), &execution.execution_id, "unapproved")
                        .is_err()
                );
                let ToolStart::Execute { attempt_id: retry } =
                    session.begin_tool(&execution).unwrap()
                else {
                    panic!("approved retry expected")
                };
                assert_eq!(retry, approved);
                let journal = store.tool_journal(&execution.execution_id).unwrap();
                assert_eq!(journal.attempts.len(), 2);
                assert_eq!(journal.attempts[1].attempt_id, approved);
                assert_eq!(journal.attempts[1].state, "running");
            }
        }
    }

    #[test]
    fn cached_success_and_failure_are_reusable_only_by_current_owner() {
        for failed in [false, true] {
            let (_directory, store, session) = running_session();
            let assistant = ChatMessage::assistant_with_tool_calls(
                "",
                vec![dume_provider::ToolCall {
                    id: "cached".into(),
                    name: "read_file".into(),
                    arguments: "{}".into(),
                }],
            );
            let execution = session.prepare_tools(&assistant).unwrap().remove(0);
            let ToolStart::Execute { attempt_id } = session.begin_tool(&execution).unwrap() else {
                panic!("execution expected")
            };
            let reply = ChatMessage::tool(
                if failed {
                    "known failure"
                } else {
                    "saved success"
                },
                "cached",
            );
            if failed {
                session
                    .finish_tool_error(&execution, &attempt_id, &reply, 0)
                    .unwrap();
            } else {
                session
                    .finish_tool(&execution, &attempt_id, &reply, 0)
                    .unwrap();
            }
            let ToolStart::Cached { result } = session.begin_tool(&execution).unwrap() else {
                panic!("cached result expected")
            };
            assert_eq!(serde_json::from_str::<ChatMessage>(&result).unwrap(), reply);
            session
                .settle("interrupted", Some("owner handoff"))
                .unwrap();
            store.resume_run(&session.token().run_id).unwrap();
            assert!(session.begin_tool(&execution).is_err());
            assert_eq!(store.conversation_history("conversation").unwrap().len(), 3);
        }
    }

    #[test]
    fn timing_pause_is_excluded_and_checkpoint_is_monotonic() {
        let (_dir, store, mut session) = running_session();
        session.active_since = Some(Instant::now() - std::time::Duration::from_millis(37));
        session.pause_timing();
        let paused = session.elapsed_ms();
        assert!(paused >= 30, "active segment must be accumulated");
        assert_eq!(session.elapsed_ms(), paused, "paused time must not accrue");
        session.resume_timing();
        let active_start = session.active_since;
        session.resume_timing();
        assert_eq!(
            session.active_since, active_start,
            "resume must not double-start a segment"
        );
        session.checkpoint().unwrap();
        let persisted = store
            .run_metrics(&session.token().run_id)
            .unwrap()
            .elapsed_ms;
        assert!(persisted <= session.elapsed_ms());
    }

    #[tokio::test]
    async fn blocked_await_checkpoints_elapsed_before_session_interruption() {
        let (_dir, store, session) = running_session();
        let cancellation = CancellationToken::new();
        let result = session
            .with_checkpoints(&cancellation, async {
                tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
                Ok(())
            })
            .await;
        result.unwrap();
        let run_id = session.token().run_id.clone();
        let checkpointed = store.run_metrics(&run_id).unwrap().elapsed_ms;
        assert!(
            checkpointed >= 900,
            "blocked operation did not persist elapsed time: {checkpointed}ms"
        );
        drop(session);
        let recovered = store.get_run(&run_id).unwrap();
        assert_eq!(recovered.status, "interrupted");
        assert!(store.run_metrics(&run_id).unwrap().elapsed_ms >= checkpointed);
    }

    #[test]
    fn resumed_timing_uses_persisted_lower_bound_without_double_counting() {
        let (_dir, store, session) = running_session();
        session.checkpoint().unwrap();
        let checkpoint = store
            .run_metrics(&session.token().run_id)
            .unwrap()
            .elapsed_ms;
        store.recover_run(&session.token().run_id).unwrap();
        let resumed =
            RunSession::resume(store.clone(), &session.token().run_id, "conversation").unwrap();
        assert_eq!(resumed.elapsed_base_ms, checkpoint);
        resumed.checkpoint().unwrap();
        let after = store
            .run_metrics(&resumed.token().run_id)
            .unwrap()
            .elapsed_ms;
        assert!(after >= checkpoint);
        assert!(after <= resumed.elapsed_ms());
    }

    #[test]
    fn dropping_active_session_records_interrupted_not_success() {
        let (_dir, store, session) = running_session();
        let run_id = session.token().run_id.clone();
        drop(session);
        let run = store.get_run(&run_id).unwrap();
        assert_eq!(run.status, "interrupted");
        assert_eq!(run.reason.as_deref(), Some("session_dropped"));
    }

    #[test]
    fn dropping_settled_session_does_not_overwrite_terminal_status() {
        let (_dir, store, session) = running_session();
        let run_id = session.token().run_id.clone();
        session.settle("succeeded", None).unwrap();
        drop(session);
        assert_eq!(store.get_run(&run_id).unwrap().status, "succeeded");
    }

    #[test]
    fn no_tool_assistant_is_persisted_once_by_both_public_apis() {
        let (_dir, store, session) = running_session();
        let reply = ChatMessage::assistant("normal answer");
        assert!(session.prepare_tools(&reply).unwrap().is_empty());
        let first = store.conversation_history("conversation").unwrap();
        assert_eq!(
            first
                .iter()
                .filter(|record| {
                    serde_json::from_str::<ChatMessage>(&record.message_json)
                        .is_ok_and(|message| message.content == "normal answer")
                })
                .count(),
            1
        );

        let second_reply = ChatMessage::assistant("recorded answer");
        session.record_assistant(&second_reply).unwrap();
        let history = store.conversation_history("conversation").unwrap();
        assert_eq!(
            history
                .iter()
                .filter(|record| {
                    serde_json::from_str::<ChatMessage>(&record.message_json)
                        .is_ok_and(|message| message.content == "recorded answer")
                })
                .count(),
            1
        );
    }

    #[test]
    fn nonzero_bash_failure_is_journaled_as_failed_not_unknown() {
        let (_dir, store, session) = running_session();
        let assistant = ChatMessage::assistant_with_tool_calls(
            "",
            vec![dume_provider::ToolCall {
                id: "failed-bash".into(),
                name: "bash".into(),
                arguments: r#"{"command":"exit 7"}"#.into(),
            }],
        );
        let execution = session.prepare_tools(&assistant).unwrap().remove(0);
        let ToolStart::Execute { attempt_id } = session.begin_tool(&execution).unwrap() else {
            panic!()
        };
        let failure = ChatMessage::tool("command exited with status 7", "failed-bash");
        session
            .finish_tool_error(&execution, &attempt_id, &failure, 1)
            .unwrap();
        assert_eq!(
            store
                .get_tool_execution(&execution.execution_id)
                .unwrap()
                .state,
            "failed"
        );
        assert_eq!(
            store.tool_attempts(&execution.execution_id).unwrap()[0].state,
            "failed"
        );
    }

    #[test]
    fn attempt_scoped_session_attaches_conversation_for_normal_assistant() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(HarnessStore::in_memory(dir.path().join("artifacts")).unwrap());
        store
            .create_conversation("internal", "/tmp", "model", "session")
            .unwrap();
        store.create_goal("goal", "test").unwrap();
        store
            .create_task(&dume_core::types::Task {
                id: "task".into(),
                goal_id: "goal".into(),
                title: "test".into(),
                description: "test".into(),
                status: dume_core::types::TaskStatus::Ready,
                dependencies: vec![],
                acceptance_criteria: vec![],
                allowed_paths: None,
                target_branch: "main".into(),
                created_at: 0,
                updated_at: 0,
            })
            .unwrap();
        let epoch = store
            .acquire_coordinator_lock("coordinator", "worker", 60_000)
            .unwrap()
            .epoch;
        store
            .create_attempt("attempt-id", "task", epoch, "worker", "/tmp", 60_000)
            .unwrap();
        let scope = RuntimeScope::Attempt("attempt-id".into());
        let receipt = store
            .admit_input(scope.clone(), "input", InputKind::Submit, "hello")
            .unwrap();
        let session = RunSession::start(
            store.clone(),
            scope,
            receipt.run_id.as_deref().unwrap(),
            "internal",
        )
        .unwrap();
        session
            .record_assistant(&ChatMessage::assistant("reply"))
            .unwrap();
        assert!(
            store
                .conversation_history("internal")
                .unwrap()
                .iter()
                .any(|row| row.message_json.contains("reply"))
        );
    }

    #[test]
    fn resume_replays_persisted_safe_reads_and_refuses_unsafe_unknown_tools() {
        let (_dir, store, session) = running_session();
        let run_id = session.token().run_id.clone();
        let message = ChatMessage::assistant_with_tool_calls(
            "reading",
            vec![dume_provider::ToolCall {
                id: "read-call".into(),
                name: "read_file".into(),
                arguments: r#"{"path":"a.txt"}"#.into(),
            }],
        );
        let read = session.prepare_tools(&message).unwrap().remove(0);
        let ToolStart::Execute { attempt_id } = session.begin_tool(&read).unwrap() else {
            panic!("new tool must start");
        };
        session
            .mark_tool_unknown(&read, &attempt_id, "engine interrupted", 1)
            .unwrap();
        drop(session);
        let resumed = RunSession::resume(store.clone(), &run_id, "conversation").unwrap();
        assert_eq!(
            store.get_tool_execution(&read.execution_id).unwrap().state,
            "prepared"
        );
        drop(resumed);

        let (_dir, store, session) = running_session();
        let run_id = session.token().run_id.clone();
        let message = ChatMessage::assistant_with_tool_calls(
            "editing",
            vec![dume_provider::ToolCall {
                id: "edit-call".into(),
                name: "bash".into(),
                arguments: r#"{"command":"printf x >> marker"}"#.into(),
            }],
        );
        let edit = session.prepare_tools(&message).unwrap().remove(0);
        let ToolStart::Execute { attempt_id } = session.begin_tool(&edit).unwrap() else {
            panic!("new tool must start");
        };
        session
            .mark_tool_unknown(&edit, &attempt_id, "engine interrupted", 1)
            .unwrap();
        drop(session);
        let error = match RunSession::resume(store.clone(), &run_id, "conversation") {
            Ok(_) => panic!("unsafe unknown tool must refuse resume"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("bash"));
        assert_eq!(
            store.get_tool_execution(&edit.execution_id).unwrap().state,
            "outcome_unknown"
        );
        assert_eq!(store.get_run(&run_id).unwrap().status, "needs_attention");
    }

    #[test]
    fn assistant_recording_rejects_stale_owner_generation() {
        let (_dir, store, session) = running_session();
        let run_id = session.token().run_id.clone();
        store
            .settle_run(session.token(), "interrupted", Some("test takeover"), 0)
            .unwrap();
        store.resume_run(&run_id).unwrap();
        assert!(
            session
                .record_assistant(&ChatMessage::assistant("stale"))
                .is_err()
        );
    }
}

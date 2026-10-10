use rusqlite::{Connection, Result};

pub fn initialize_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        PRAGMA foreign_keys = ON;
        PRAGMA busy_timeout = 5000;

        CREATE TABLE IF NOT EXISTS coordinator_locks (
            coordinator_id TEXT PRIMARY KEY,
            owner_id TEXT,
            epoch INTEGER NOT NULL DEFAULT 1,
            lease_expires_at INTEGER NOT NULL,
            heartbeat_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS goals (
            id TEXT PRIMARY KEY,
            description TEXT NOT NULL,
            status TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS tasks (
            id TEXT PRIMARY KEY,
            goal_id TEXT NOT NULL,
            title TEXT NOT NULL,
            description TEXT NOT NULL,
            status TEXT NOT NULL,
            dependencies_json TEXT NOT NULL DEFAULT '[]',
            acceptance_criteria_json TEXT NOT NULL DEFAULT '[]',
            allowed_paths_json TEXT,
            target_branch TEXT NOT NULL DEFAULT 'main',
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            FOREIGN KEY (goal_id) REFERENCES goals(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS attempts (
            id TEXT PRIMARY KEY,
            task_id TEXT NOT NULL,
            coordinator_epoch INTEGER NOT NULL,
            worker_id TEXT NOT NULL,
            worktree_path TEXT NOT NULL,
            status TEXT NOT NULL,
            lease_expires_at INTEGER NOT NULL,
            heartbeat_at INTEGER NOT NULL,
            candidate_commit TEXT,
            manifest_hash TEXT,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS verifications (
            attempt_id TEXT PRIMARY KEY,
            epoch INTEGER NOT NULL,
            passed INTEGER NOT NULL,
            allowed_paths_passed INTEGER NOT NULL,
            acceptance_command_passed INTEGER NOT NULL,
            manifest_hash TEXT NOT NULL,
            details TEXT NOT NULL,
            verified_at INTEGER NOT NULL,
            FOREIGN KEY (attempt_id) REFERENCES attempts(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS integrations (
            target_branch TEXT NOT NULL,
            base_commit TEXT NOT NULL,
            candidate_commit TEXT NOT NULL,
            integration_commit TEXT,
            status TEXT NOT NULL,
            error_message TEXT,
            integrated_at INTEGER NOT NULL,
            PRIMARY KEY (target_branch, candidate_commit)
        );

        CREATE TABLE IF NOT EXISTS external_operations (
            id TEXT PRIMARY KEY,
            attempt_id TEXT NOT NULL,
            idempotency_key TEXT NOT NULL UNIQUE,
            description TEXT NOT NULL,
            status TEXT NOT NULL,
            receipt_data TEXT,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS events (
            sequence INTEGER PRIMARY KEY AUTOINCREMENT,
            entity_id TEXT NOT NULL,
            event_type TEXT NOT NULL,
            epoch INTEGER NOT NULL,
            dedup_key TEXT UNIQUE,
            payload TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS session_messages (
            session_id TEXT NOT NULL,
            message_index INTEGER NOT NULL,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            tool_calls_json TEXT,
            tool_call_id TEXT,
            is_compacted_summary INTEGER NOT NULL DEFAULT 0,
            is_archived INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL,
            PRIMARY KEY (session_id, message_index)
        );

        CREATE TABLE IF NOT EXISTS session_usage (
            session_id TEXT PRIMARY KEY,
            input_tokens INTEGER NOT NULL DEFAULT 0,
            output_tokens INTEGER NOT NULL DEFAULT 0,
            total_tokens INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens INTEGER NOT NULL DEFAULT 0,
            cache_write_tokens INTEGER NOT NULL DEFAULT 0,
            updated_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS request_usage (
            request_id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            model TEXT NOT NULL,
            input_tokens INTEGER NOT NULL DEFAULT 0,
            output_tokens INTEGER NOT NULL DEFAULT 0,
            total_tokens INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens INTEGER NOT NULL DEFAULT 0,
            cache_write_tokens INTEGER NOT NULL DEFAULT 0,
            benchmark_run_id TEXT,
            case_id TEXT,
            variant TEXT,
            attempt_id TEXT,
            agent_id TEXT,
            parent_agent_id TEXT,
            raw_usage_json TEXT,
            is_complete INTEGER NOT NULL DEFAULT 1,
            created_at INTEGER NOT NULL
        );
        "#,
    )?;

    // Safe backwards-compatible column migrations BEFORE creating indices
    apply_column_migration(
        conn,
        "ALTER TABLE session_usage ADD COLUMN cache_read_tokens INTEGER NOT NULL DEFAULT 0;",
    )?;
    apply_column_migration(
        conn,
        "ALTER TABLE session_usage ADD COLUMN cache_write_tokens INTEGER NOT NULL DEFAULT 0;",
    )?;
    apply_column_migration(
        conn,
        "ALTER TABLE request_usage ADD COLUMN cache_read_tokens INTEGER NOT NULL DEFAULT 0;",
    )?;
    apply_column_migration(
        conn,
        "ALTER TABLE request_usage ADD COLUMN cache_write_tokens INTEGER NOT NULL DEFAULT 0;",
    )?;
    apply_column_migration(
        conn,
        "ALTER TABLE request_usage ADD COLUMN benchmark_run_id TEXT;",
    )?;
    apply_column_migration(conn, "ALTER TABLE request_usage ADD COLUMN case_id TEXT;")?;
    apply_column_migration(conn, "ALTER TABLE request_usage ADD COLUMN variant TEXT;")?;
    apply_column_migration(
        conn,
        "ALTER TABLE request_usage ADD COLUMN attempt_id TEXT;",
    )?;
    apply_column_migration(conn, "ALTER TABLE request_usage ADD COLUMN agent_id TEXT;")?;
    apply_column_migration(
        conn,
        "ALTER TABLE request_usage ADD COLUMN parent_agent_id TEXT;",
    )?;
    apply_column_migration(
        conn,
        "ALTER TABLE request_usage ADD COLUMN raw_usage_json TEXT;",
    )?;
    apply_column_migration(
        conn,
        "ALTER TABLE request_usage ADD COLUMN is_complete INTEGER NOT NULL DEFAULT 1;",
    )?;

    conn.execute_batch(
        r#"
        CREATE INDEX IF NOT EXISTS idx_request_usage_session ON request_usage(session_id);
        CREATE INDEX IF NOT EXISTS idx_request_usage_bench ON request_usage(benchmark_run_id, case_id, variant);
        CREATE INDEX IF NOT EXISTS idx_request_usage_attempt ON request_usage(attempt_id);

        CREATE TABLE IF NOT EXISTS workflow_runs (
            run_id TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            status TEXT NOT NULL,
            goal_id TEXT,
            params_json TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS workflow_artifacts (
            id TEXT PRIMARY KEY,
            run_id TEXT NOT NULL,
            kind TEXT NOT NULL,
            digest TEXT NOT NULL,
            content TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            FOREIGN KEY (run_id) REFERENCES workflow_runs(run_id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS conversations (
            id TEXT PRIMARY KEY,
            cwd TEXT NOT NULL,
            model TEXT NOT NULL,
            provider_session_id TEXT NOT NULL,
            status TEXT NOT NULL,
            generation INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_conversations_cwd ON conversations(cwd, updated_at DESC);

        CREATE TABLE IF NOT EXISTS conversation_messages (
            conversation_id TEXT NOT NULL,
            generation INTEGER NOT NULL,
            idx INTEGER NOT NULL,
            message_json TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            PRIMARY KEY (conversation_id, generation, idx),
            FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS agent_runs (
            run_id TEXT PRIMARY KEY, scope TEXT NOT NULL, status TEXT NOT NULL,
            owner_generation INTEGER NOT NULL, reason TEXT, started_at INTEGER NOT NULL,
            ended_at INTEGER, elapsed_ms INTEGER NOT NULL DEFAULT 0,
            timing_complete INTEGER NOT NULL DEFAULT 1,
            conversation_id TEXT, parent_run_id TEXT, fencing_attempt_id TEXT
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_runs_active_scope
            ON agent_runs(scope) WHERE status IN ('queued','running','cancelling','needs_attention');
        CREATE TABLE IF NOT EXISTS agent_inbox (
            input_id INTEGER PRIMARY KEY AUTOINCREMENT, scope TEXT NOT NULL,
            client_request_id TEXT NOT NULL, kind TEXT NOT NULL, payload TEXT NOT NULL,
            disposition TEXT NOT NULL, target_run_id TEXT, state TEXT NOT NULL,
            consumed_run_id TEXT, transcript_idx INTEGER,
            UNIQUE(scope, client_request_id)
        );
        CREATE INDEX IF NOT EXISTS idx_agent_inbox_fifo ON agent_inbox(scope,state,input_id);
        CREATE TRIGGER IF NOT EXISTS trg_followup_transcript
        AFTER UPDATE OF state ON agent_inbox
        WHEN NEW.state='consumed' AND NEW.kind='follow_up' AND NEW.transcript_idx IS NULL
             AND NEW.consumed_run_id IS NOT NULL
        BEGIN
            INSERT INTO conversation_messages(conversation_id,generation,idx,message_json,created_at)
            SELECT c.id,generation,
                   COALESCE((SELECT MAX(idx)+1 FROM conversation_messages WHERE conversation_id=c.id AND generation=c.generation),0),
                   json_object('role','user','content',NEW.payload),strftime('%s','now')*1000
            FROM agent_runs r JOIN conversations c ON c.id=r.conversation_id WHERE r.run_id=NEW.consumed_run_id;
            UPDATE agent_inbox SET transcript_idx=(SELECT MAX(idx) FROM conversation_messages WHERE conversation_id=(SELECT conversation_id FROM agent_runs WHERE run_id=NEW.consumed_run_id) AND generation=(SELECT generation FROM conversations WHERE id=(SELECT conversation_id FROM agent_runs WHERE run_id=NEW.consumed_run_id))) WHERE input_id=NEW.input_id;
        END;
        CREATE TABLE IF NOT EXISTS tool_executions (
            execution_id TEXT PRIMARY KEY, run_id TEXT NOT NULL, tool_call_id TEXT NOT NULL,
            name TEXT NOT NULL, arguments TEXT NOT NULL, arguments_hash TEXT NOT NULL,
            replay_policy TEXT NOT NULL, state TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 0,
            active_attempt_id TEXT, result TEXT, error TEXT, result_source TEXT,
            routing_epoch TEXT,
            routing_cancelled INTEGER NOT NULL DEFAULT 0 CHECK(routing_cancelled IN (0,1)),
            UNIQUE(run_id,tool_call_id)
        );
        CREATE TABLE IF NOT EXISTS tool_execution_attempts (
            attempt_id TEXT PRIMARY KEY, execution_id TEXT NOT NULL, attempt_no INTEGER NOT NULL,
            state TEXT NOT NULL, started_at INTEGER, ended_at INTEGER, duration_ms INTEGER,
            UNIQUE(execution_id,attempt_no)
        );
        CREATE TABLE IF NOT EXISTS tool_resolutions (
            resolution_id TEXT PRIMARY KEY, execution_id TEXT NOT NULL, attempt_id TEXT NOT NULL,
            expected_revision INTEGER NOT NULL, operator TEXT NOT NULL, kind TEXT NOT NULL,
            evidence TEXT NOT NULL, duplicate_effect_warning INTEGER NOT NULL, created_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS conversation_active_tools (
            conversation_id TEXT NOT NULL, tool_id TEXT NOT NULL, fingerprint TEXT NOT NULL,
            PRIMARY KEY(conversation_id,tool_id)
        );
        CREATE TABLE IF NOT EXISTS runtime_model_attempts (
            request_id TEXT PRIMARY KEY, run_id TEXT NOT NULL, session_id TEXT NOT NULL,
            model TEXT NOT NULL, state TEXT NOT NULL, reason TEXT, started_at INTEGER NOT NULL,
            ended_at INTEGER, duration_ms INTEGER, timing_complete INTEGER NOT NULL DEFAULT 1,
            usage_json TEXT, owner_generation INTEGER NOT NULL DEFAULT 1
        );
        CREATE TABLE IF NOT EXISTS conversation_routing_state (
            conversation_id TEXT PRIMARY KEY,
            epoch TEXT NOT NULL CHECK(length(epoch)>0),
            phase TEXT NOT NULL CHECK(phase IN ('fixed','planning','implementation')),
            original_model TEXT NOT NULL CHECK(length(original_model)>0),
            planning_model TEXT NOT NULL CHECK(length(planning_model)>0),
            implementation_model TEXT NOT NULL CHECK(length(implementation_model)>0),
            reason TEXT NOT NULL,
            revision INTEGER NOT NULL,
            classifier_input_tokens INTEGER CHECK(classifier_input_tokens IS NULL OR classifier_input_tokens>=0),
            classifier_output_tokens INTEGER CHECK(classifier_output_tokens IS NULL OR classifier_output_tokens>=0),
            FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
        );
        "#,
    )?;

    apply_column_migration(
        conn,
        "ALTER TABLE runtime_model_attempts ADD COLUMN context_json TEXT;",
    )?;
    apply_column_migration(
        conn,
        "ALTER TABLE runtime_model_attempts ADD COLUMN owner_generation INTEGER NOT NULL DEFAULT 1;",
    )?;
    apply_column_migration(
        conn,
        "ALTER TABLE agent_runs ADD COLUMN conversation_id TEXT;",
    )?;
    apply_column_migration(
        conn,
        "ALTER TABLE agent_runs ADD COLUMN parent_run_id TEXT;",
    )?;
    apply_column_migration(
        conn,
        "ALTER TABLE agent_runs ADD COLUMN fencing_attempt_id TEXT;",
    )?;
    apply_column_migration(
        conn,
        "ALTER TABLE tool_executions ADD COLUMN routing_epoch TEXT;",
    )?;
    apply_column_migration(
        conn,
        "ALTER TABLE tool_executions ADD COLUMN routing_cancelled INTEGER NOT NULL DEFAULT 0 CHECK(routing_cancelled IN (0,1));",
    )?;

    Ok(())
}

fn apply_column_migration(conn: &Connection, sql: &str) -> Result<()> {
    match conn.execute(sql, []) {
        Ok(_) => Ok(()),
        Err(e) => {
            let msg = e.to_string();
            // SQLite returns "duplicate column name: ..." if column already exists
            if msg.contains("duplicate column") {
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

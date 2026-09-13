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
        "#,
    )?;
    Ok(())
}

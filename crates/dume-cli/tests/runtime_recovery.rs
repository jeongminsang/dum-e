use dume_store::{HarnessStore, InputKind, RuntimeScope, ToolSpec};
use std::process::{Command, Output};

fn run(bin: &str, home: &std::path::Path, args: &[&str]) -> Output {
    Command::new(bin)
        .env("HOME", home)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn queue_is_persistent_deduplicated_and_withdrawable() {
    let home = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_dume");
    let db = home.path().join("runtime.db");
    let artifacts = home.path().join("artifacts");
    let db_arg = db.to_str().unwrap();
    let art_arg = artifacts.to_str().unwrap();
    let first = run(
        bin,
        home.path(),
        &[
            "runtime",
            "queue",
            "add",
            "conversation:abc",
            "hello",
            "--request-id",
            "client-1",
            "--db-path",
            db_arg,
            "--artifacts-dir",
            art_arg,
        ],
    );
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let duplicate = run(
        bin,
        home.path(),
        &[
            "runtime",
            "queue",
            "add",
            "conversation:abc",
            "hello",
            "--request-id",
            "client-1",
            "--db-path",
            db_arg,
            "--artifacts-dir",
            art_arg,
        ],
    );
    assert!(duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stdout).contains("\"duplicate\":true"));
    let pending_args = [
        "runtime",
        "queue",
        "add",
        "conversation:abc",
        "later",
        "--kind",
        "follow_up",
        "--request-id",
        "client-2",
        "--db-path",
        db_arg,
        "--artifacts-dir",
        art_arg,
    ];
    let pending = run(bin, home.path(), &pending_args);
    assert!(
        pending.status.success(),
        "{}",
        String::from_utf8_lossy(&pending.stderr)
    );
    let pending_duplicate = run(bin, home.path(), &pending_args);
    assert!(pending_duplicate.status.success());
    assert!(String::from_utf8_lossy(&pending_duplicate.stdout).contains("\"duplicate\":true"));
    let list = run(
        bin,
        home.path(),
        &[
            "runtime",
            "queue",
            "list",
            "conversation:abc",
            "--db-path",
            db_arg,
            "--artifacts-dir",
            art_arg,
        ],
    );
    assert!(list.status.success());
    let rows: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 2);
    assert_eq!(rows[0]["state"], "consumed");
    assert_eq!(rows[1]["state"], "pending");
    let consumed_id = rows[0]["input_id"].as_i64().unwrap().to_string();
    let consumed_withdrawal = run(
        bin,
        home.path(),
        &[
            "runtime",
            "queue",
            "withdraw",
            "conversation:abc",
            &consumed_id,
            "--db-path",
            db_arg,
            "--artifacts-dir",
            art_arg,
        ],
    );
    assert!(!consumed_withdrawal.status.success());
    let id = rows[1]["input_id"].as_i64().unwrap().to_string();
    let withdrawn = run(
        bin,
        home.path(),
        &[
            "runtime",
            "queue",
            "withdraw",
            "conversation:abc",
            &id,
            "--db-path",
            db_arg,
            "--artifacts-dir",
            art_arg,
        ],
    );
    assert!(
        withdrawn.status.success(),
        "{}",
        String::from_utf8_lossy(&withdrawn.stderr)
    );
    let list = run(
        bin,
        home.path(),
        &[
            "runtime",
            "queue",
            "list",
            "conversation:abc",
            "--db-path",
            db_arg,
            "--artifacts-dir",
            art_arg,
        ],
    );
    let rows: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(rows[0]["state"], "consumed");
    assert_eq!(rows[1]["state"], "withdrawn");
}

#[test]
fn project_mcp_config_requires_explicit_current_fingerprint_trust() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let config_dir = project.path().join(".dume");
    std::fs::create_dir_all(&config_dir).unwrap();
    let config = config_dir.join("mcp.json");
    let global_config = home.path().join(".dume/agent/mcp.json");
    std::fs::create_dir_all(global_config.parent().unwrap()).unwrap();
    let changed_marker = home.path().join("changed-launched");
    let global_marker = home.path().join("global-launched");
    let changed_command = marker_command(home.path(), "changed-child", &changed_marker);
    let global_command = marker_command(home.path(), "global-child", &global_marker);
    std::fs::write(
        &config,
        format!(
            r#"{{"servers":{{"local":{{"command":"{}"}}}}}}"#,
            changed_command.display()
        ),
    )
    .unwrap();
    std::fs::write(
        &global_config,
        format!(
            r#"{{"servers":{{"global":{{"command":"{}"}}}}}}"#,
            global_command.display()
        ),
    )
    .unwrap();
    let bin = env!("CARGO_BIN_EXE_dume");
    let repo = project.path().to_str().unwrap();
    let listed = run(bin, home.path(), &["mcp", "list", "--project", repo]);
    assert!(!listed.status.success());
    assert!(String::from_utf8_lossy(&listed.stderr).contains("not trusted"));
    assert!(
        global_marker.exists(),
        "unchanged global server was not launched"
    );
    assert!(
        !changed_marker.exists(),
        "untrusted project server was executed"
    );
    let trusted = run(bin, home.path(), &["mcp", "trust", "--repo", repo]);
    assert!(
        trusted.status.success(),
        "{}",
        String::from_utf8_lossy(&trusted.stderr)
    );
    let first_trust: serde_json::Value = serde_json::from_slice(
        &std::fs::read(home.path().join(".dume/agent/mcp-trust.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(first_trust.as_array().unwrap().len(), 1);
    std::fs::write(
        &config,
        r#"{"servers":{"local":{"command":"different-untrusted-command"}}}"#,
    )
    .unwrap();
    std::fs::remove_file(&global_marker).unwrap();
    let changed = run(bin, home.path(), &["mcp", "list", "--project", repo]);
    assert!(!changed.status.success());
    assert!(String::from_utf8_lossy(&changed.stderr).contains("not trusted"));
    assert!(
        global_marker.exists(),
        "unchanged global server must remain usable after project config changes"
    );
    assert!(
        !changed_marker.exists(),
        "changed untrusted project server was executed"
    );
}

#[cfg(unix)]
#[test]
fn project_mcp_trust_is_shared_with_registered_worker_worktree() {
    let home = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let main = temp.path().join("main");
    let worker = temp.path().join("worker");
    std::fs::create_dir(&main).unwrap();
    let git = |dir: &std::path::Path, args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&main, &["init"]);
    git(&main, &["config", "user.email", "test@example.invalid"]);
    git(&main, &["config", "user.name", "Test"]);
    let marker = home.path().join("worker-launched");
    let command = marker_command(home.path(), "worker-child", &marker);
    let config_dir = main.join(".dume");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("mcp.json"),
        format!(
            r#"{{"servers":{{"local":{{"command":"{}"}}}}}}"#,
            command.display()
        ),
    )
    .unwrap();
    git(&main, &["add", ".dume/mcp.json"]);
    git(&main, &["commit", "-m", "fixture"]);
    let bin = env!("CARGO_BIN_EXE_dume");
    let trusted = run(
        bin,
        home.path(),
        &["mcp", "trust", "--repo", main.to_str().unwrap()],
    );
    assert!(
        trusted.status.success(),
        "{}",
        String::from_utf8_lossy(&trusted.stderr)
    );
    git(
        &main,
        &["worktree", "add", "-b", "worker", worker.to_str().unwrap()],
    );
    let listed = run(
        bin,
        home.path(),
        &["mcp", "list", "--project", worker.to_str().unwrap()],
    );
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    assert!(
        marker.exists(),
        "approved project MCP server was not started from worker worktree"
    );
}

#[cfg(unix)]
fn marker_command(
    directory: &std::path::Path,
    name: &str,
    marker: &std::path::Path,
) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let command = directory.join(name);
    std::fs::write(
        &command,
        format!(
            "#!/bin/sh\nprintf launched > '{}'\nexit 0\n",
            marker.display()
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&command).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&command, permissions).unwrap();
    command
}

#[test]
fn disabled_global_server_overrides_project_and_is_not_project_trusted() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let global_dir = home.path().join(".dume/agent");
    let project_dir = project.path().join(".dume");
    std::fs::create_dir_all(&global_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    let marker = home.path().join("disabled-launched");
    let command = marker_command(home.path(), "disabled-child", &marker);
    std::fs::write(
        global_dir.join("mcp.json"),
        format!(
            r#"{{"servers":{{"same":{{"command":"{}","enabled":false}}}}}}"#,
            command.display()
        ),
    )
    .unwrap();
    std::fs::write(
        project_dir.join("mcp.json"),
        format!(
            r#"{{"servers":{{"same":{{"command":"{}","enabled":true}}}}}}"#,
            command.display()
        ),
    )
    .unwrap();
    let bin = env!("CARGO_BIN_EXE_dume");
    let repo = project.path().to_str().unwrap();
    let trusted = run(bin, home.path(), &["mcp", "trust", "--repo", repo]);
    assert!(
        trusted.status.success(),
        "{}",
        String::from_utf8_lossy(&trusted.stderr)
    );
    let records: serde_json::Value = serde_json::from_slice(
        &std::fs::read(home.path().join(".dume/agent/mcp-trust.json")).unwrap(),
    )
    .unwrap();
    assert!(records.as_array().unwrap().is_empty());
    let listed = run(bin, home.path(), &["mcp", "list", "--project", repo]);
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    assert!(
        !marker.exists(),
        "globally disabled server was launched from project config"
    );
}

#[test]
fn unknown_resolution_requires_auditable_evidence_and_is_revision_fenced() {
    let home = tempfile::tempdir().unwrap();
    let db = home.path().join("runtime.db");
    let artifacts = home.path().join("artifacts");
    let store = HarnessStore::open(&db, &artifacts).unwrap();
    store
        .create_conversation("conv", home.path().to_str().unwrap(), "model", "session")
        .unwrap();
    let scope = RuntimeScope::Conversation("conv".into());
    let admitted = store
        .admit_input(scope.clone(), "submit-1", InputKind::Submit, "inspect")
        .unwrap();
    let token = store
        .start_run(scope, admitted.run_id.as_deref().unwrap())
        .unwrap();
    let execution = store
        .prepare_assistant_turn_and_tools(
            &token,
            "conv",
            "{}",
            &[ToolSpec {
                tool_call_id: "call-1".into(),
                name: "external".into(),
                arguments: "{}".into(),
                replay_safe: false,
            }],
            None,
        )
        .unwrap()
        .remove(0);
    store
        .begin_tool(&token, &execution.execution_id, "attempt-1")
        .unwrap();
    let unknown = store.recover_run(&token.run_id).unwrap().remove(0);
    drop(store);

    let bin = env!("CARGO_BIN_EXE_dume");
    let revision = unknown.revision.to_string();
    let args = [
        "runtime",
        "resolve",
        unknown.execution_id.as_str(),
        "--run-id",
        token.run_id.as_str(),
        "--revision",
        revision.as_str(),
        "--operator",
        "operator-1",
        "--decision",
        "observed_not_run",
        "--evidence",
        "confirmed_not_run:checked audit log",
        "--db-path",
        db.to_str().unwrap(),
        "--artifacts-dir",
        artifacts.to_str().unwrap(),
    ];
    let resolved = run(bin, home.path(), &args);
    assert!(
        resolved.status.success(),
        "{}",
        String::from_utf8_lossy(&resolved.stderr)
    );
    let connection = rusqlite::Connection::open(&db).unwrap();
    let audited: i64 = connection
        .query_row("SELECT COUNT(*) FROM tool_resolutions", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(audited, 1);
    let duplicate = run(bin, home.path(), &args);
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("revision/state mismatch"));
    let audited: i64 = connection
        .query_row("SELECT COUNT(*) FROM tool_resolutions", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(audited, 1);
}

#[test]
fn paused_queue_input_can_be_resumed_after_recovery() {
    let home = tempfile::tempdir().unwrap();
    let db = home.path().join("runtime.db");
    let artifacts = home.path().join("artifacts");
    let store = HarnessStore::open(&db, &artifacts).unwrap();
    store
        .create_conversation("conv", home.path().to_str().unwrap(), "model", "session")
        .unwrap();
    let scope = RuntimeScope::Conversation("conv".into());
    let first = store
        .admit_input(scope.clone(), "first", InputKind::Submit, "start")
        .unwrap();
    let token = store
        .start_run(scope.clone(), first.run_id.as_deref().unwrap())
        .unwrap();
    store
        .settle_run(&token, "needs_attention", Some("recovery"), 1)
        .unwrap();
    let paused = store
        .admit_input(scope, "paused", InputKind::FollowUp, "continue")
        .unwrap();
    store.resume_run(&token.run_id).unwrap();
    drop(store);

    let bin = env!("CARGO_BIN_EXE_dume");
    let id = paused.input_id.to_string();
    let output = run(
        bin,
        home.path(),
        &[
            "runtime",
            "queue",
            "resume",
            "conversation:conv",
            &id,
            "--db-path",
            db.to_str().unwrap(),
            "--artifacts-dir",
            artifacts.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let store = HarnessStore::open(&db, &artifacts).unwrap();
    assert_eq!(
        store
            .inbox(&RuntimeScope::Conversation("conv".into()), &[])
            .unwrap()[1]
            .state,
        "pending"
    );
}

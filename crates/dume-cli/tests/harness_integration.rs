use dume_core::dag::TaskDag;
use dume_core::manifest::{ResultManifest, TestResult};
use dume_core::types::*;
use dume_core::verify::verify_allowed_paths;
use dume_store::{HarnessStore, StoreError};
use tempfile::tempdir;

#[test]
fn test_harness_dag_and_epoch_fencing() {
    let dir = tempdir().unwrap();
    let artifacts_dir = dir.path().join("artifacts");
    let store = HarnessStore::in_memory(&artifacts_dir).unwrap();

    // 1. Initial coordinator acquires lock at epoch 1
    let lock1 = store.acquire_coordinator_lock("coord_main", "host_process_1", 5000).unwrap();
    assert_eq!(lock1.epoch, 1);

    // 2. Create goal and DAG tasks: t1 -> t2
    let _goal = store.create_goal("goal_rust", "Rust Migration Goal").unwrap();
    let t1 = Task {
        id: "task_1".to_string(),
        goal_id: "goal_rust".to_string(),
        title: "Setup Core".to_string(),
        description: "Implement types".to_string(),
        status: TaskStatus::Ready,
        dependencies: vec![],
        acceptance_criteria: vec!["true".to_string()],
        allowed_paths: Some(vec!["src/".to_string()]),
        target_branch: "main".to_string(),
        created_at: 0,
        updated_at: 0,
    };
    let t2 = Task {
        id: "task_2".to_string(),
        goal_id: "goal_rust".to_string(),
        title: "Setup Store".to_string(),
        description: "Implement SQLite".to_string(),
        status: TaskStatus::Blocked,
        dependencies: vec!["task_1".to_string()],
        acceptance_criteria: vec!["true".to_string()],
        allowed_paths: Some(vec!["src/".to_string()]),
        target_branch: "main".to_string(),
        created_at: 0,
        updated_at: 0,
    };
    store.create_task(&t1).unwrap();
    store.create_task(&t2).unwrap();

    let all_tasks = store.list_tasks_for_goal("goal_rust").unwrap();
    let dag = TaskDag::new(&all_tasks).unwrap();
    let ready = dag.get_ready_tasks();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].id, "task_1");

    // 3. Worker starts attempt for task_1 under epoch 1
    let _attempt = store.create_attempt("att_1", "task_1", 1, "worker_1", "/tmp/wt1", 5000).unwrap();

    // 4. Simulate coordinator termination/crash (R1): release lock and new coordinator takes over
    store.release_coordinator_lock("coord_main", "host_process_1").unwrap();
    let lock2 = store.acquire_coordinator_lock("coord_main", "host_process_2", 5000).unwrap();
    assert_eq!(lock2.epoch, 2); // Monotonic increase to epoch 2

    // 5. Stale worker from epoch 1 tries to submit result -> MUST BE REJECTED by epoch fencing guard (R2)
    let stale_submit = store.submit_attempt_result("att_1", 1, "commit_old", "hash_old");
    assert!(
        matches!(stale_submit, Err(StoreError::StaleEpoch { attempt_epoch: 1, current_coordinator_epoch: 2 })),
        "Stale epoch submission from old coordinator epoch must be rejected"
    );

    // If another invalid epoch submits to an attempt recorded under epoch 1, it must fail with FencingViolation:
    let foreign_submit = store.submit_attempt_result("att_1", 99, "commit_fake", "hash_fake");
    assert!(matches!(foreign_submit, Err(StoreError::FencingViolation { .. })));


    // 6. External operation dispatched without confirmation before crash (R3)
    store.record_external_operation_intent("op_1", "att_1", "deploy_prod_1", "Deploy service").unwrap();
    store.update_external_operation_status("op_1", ExternalOperationStatus::Dispatched, None).unwrap();

    // 7. Crash recovery inspection (R1 & R3)
    let (ready_verify, needs_attention, unknown_ops) = store.recover_state(lock2.epoch).unwrap();
    // Stale attempt was not successfully submitted before crash, so it transitions to needs_attention:
    assert_eq!(ready_verify.len(), 0);
    assert_eq!(needs_attention.len(), 1);
    assert_eq!(needs_attention[0].id, "att_1");


    // Operation was automatically moved to OutcomeUnknown upon unconfirmed crash recovery
    assert_eq!(unknown_ops.len(), 1);
    assert_eq!(unknown_ops[0].id, "op_1");
    assert_eq!(unknown_ops[0].status, ExternalOperationStatus::OutcomeUnknown);

    // OutcomeUnknown guard prevents automatic re-dispatch
    assert_eq!(
        dume_core::operation::validate_operation_transition(
            unknown_ops[0].status,
            ExternalOperationStatus::Dispatched
        ),
        Err(dume_core::operation::OperationError::OutcomeUnknownGuard)
    );
}

#[test]
fn test_verification_path_whitelist_rejection() {
    let task = Task {
        id: "t_verify".to_string(),
        goal_id: "g1".to_string(),
        title: "Test Verify".to_string(),
        description: "".to_string(),
        status: TaskStatus::Ready,
        dependencies: vec![],
        acceptance_criteria: vec![],
        allowed_paths: Some(vec!["crates/dume-core/".to_string()]),
        target_branch: "main".to_string(),
        created_at: 0,
        updated_at: 0,
    };

    // Compliant manifest
    let ok_manifest = ResultManifest {
        attempt_id: "a1".to_string(),
        candidate_commit: "c1".to_string(),
        modified_files: vec!["crates/dume-core/src/lib.rs".to_string()],
        changed_artifacts: vec![],
        test_results: vec![TestResult {
            name: "test".to_string(),
            passed: true,
            exit_code: 0,
            stdout: "".to_string(),
            stderr: "".to_string(),
        }],
        summary: "valid".to_string(),
    };
    assert!(verify_allowed_paths(&task, &ok_manifest));

    // Non-compliant manifest (attempting to edit unauthorized file)
    let bad_manifest = ResultManifest {
        attempt_id: "a2".to_string(),
        candidate_commit: "c2".to_string(),
        modified_files: vec![
            "crates/dume-core/src/lib.rs".to_string(),
            "package.json".to_string(),
        ],
        changed_artifacts: vec![],
        test_results: vec![],
        summary: "invalid".to_string(),
    };
    assert!(!verify_allowed_paths(&task, &bad_manifest));
}

#[tokio::test]
async fn test_real_process_crash_and_recovery() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("harness.db");
    let artifacts_dir = dir.path().join("artifacts");

    // 1. Coordinator 1 starts and acquires lock
    {
        let store = HarnessStore::open(&db_path, &artifacts_dir).unwrap();
        let lock = store.acquire_coordinator_lock("coord_main", "proc_parent_1", 10_000).unwrap();
        assert_eq!(lock.epoch, 1);

        // Create goal and task
        store.create_goal("g_crash", "Crash Test Goal").unwrap();
        let task = Task {
            id: "t_crash".to_string(),
            goal_id: "g_crash".to_string(),
            title: "Task".to_string(),
            description: "Will crash".to_string(),
            status: TaskStatus::Ready,
            dependencies: vec![],
            acceptance_criteria: vec!["true".to_string()],
            allowed_paths: None,
            target_branch: "main".to_string(),
            created_at: 0,
            updated_at: 0,
        };
        store.create_task(&task).unwrap();

        // Worker begins running attempt
        let _att = store.create_attempt("att_c1", "t_crash", 1, "w1", "/tmp/wt_c1", 10_000).unwrap();

        // Simulate abrupt SIGKILL of coordinator 1 without release_coordinator_lock:
        // (Coordinator drops without clean exit, lease remains in DB)
    }

    // Fast-forward or expire lease:
    // In real scenario, after lease_expires_at passes (or forced failover):
    {
        // 2. New Coordinator 2 starts up, recovers state
        let store2 = HarnessStore::open(&db_path, &artifacts_dir).unwrap();
        
        // Coordinator 2 acquires lock (taking over with incremented monotonic epoch)
        // Set lease expired in SQLite to simulate time passing
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute("UPDATE coordinator_locks SET lease_expires_at = 0", []).unwrap();
        }

        let lock2 = store2.acquire_coordinator_lock("coord_main", "proc_parent_2", 10_000).unwrap();
        assert_eq!(lock2.epoch, 2); // Monotonic increase to 2

        // Crash recovery: uncommitted attempt from epoch 1 transitions to needs_attention
        let (ready, needs_attention, _) = store2.recover_state(lock2.epoch).unwrap();
        assert_eq!(ready.len(), 0);
        assert_eq!(needs_attention.len(), 1);
        assert_eq!(needs_attention[0].id, "att_c1");
        assert_eq!(needs_attention[0].status, AttemptStatus::NeedsAttention);

        // Stale worker att_c1 late submission is rejected
        let stale = store2.submit_attempt_result("att_c1", 1, "commit_stale", "hash_stale");
        assert!(stale.is_err(), "Late worker submission after coordinator crash must be rejected");
    }
}


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

        // Worker 1 starts attempt (running, not submitted)
        let _att1 = store.create_attempt("att_c1", "t_crash", 1, "w1", "/tmp/wt_c1", 10_000).unwrap();

        // Worker 2 finished and submitted result just before crash
        let _att2 = store.create_attempt("att_c2", "t_crash", 1, "w2", "/tmp/wt_c2", 10_000).unwrap();
        store.submit_attempt_result("att_c2", 1, "commit_submitted", "hash_submitted").unwrap();

        // Task 3 was already verified and completed earlier
        let _att3 = store.create_attempt("att_c3", "t_crash", 1, "w3", "/tmp/wt_c3", 10_000).unwrap();
        store.submit_attempt_result("att_c3", 1, "commit_completed", "hash_completed").unwrap();
        store.update_attempt_status("att_c3", AttemptStatus::Accepted).unwrap();

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

        // Crash recovery:
        // 1. Uncommitted running attempt from epoch 1 transitions to needs_attention
        // 2. Already submitted attempt is recovered into ready_for_verify queue!
        // 3. Already accepted attempt remains undisturbed
        let (ready, needs_attention, _) = store2.recover_state(lock2.epoch).unwrap();
        
        // att_c2 was submitted before crash -> MUST be recovered for verification
        assert_eq!(ready.len(), 1, "Result-submitted attempt must be recovered for verification");
        assert_eq!(ready[0].id, "att_c2");
        assert_eq!(ready[0].candidate_commit.as_deref(), Some("commit_submitted"));

        // att_c1 was running without submission -> MUST be quarantined to needs_attention
        assert_eq!(needs_attention.len(), 1);
        assert_eq!(needs_attention[0].id, "att_c1");
        assert_eq!(needs_attention[0].status, AttemptStatus::NeedsAttention);

        // att_c3 was already accepted -> remains Accepted
        let att3 = store2.get_attempt("att_c3").unwrap();
        assert_eq!(att3.status, AttemptStatus::Accepted);

        // Stale worker att_c1 late submission is rejected
        let stale = store2.submit_attempt_result("att_c1", 1, "commit_stale", "hash_stale");
        assert!(stale.is_err(), "Late worker submission after coordinator crash must be rejected");
    }
}


#[tokio::test]
async fn test_e2e_full_agent_workflow() {
    // 1. Initialize temporary Git repository
    let temp_repo = tempdir().unwrap();
    let repo_path = temp_repo.path();

    // Initialize git repository with initial commit
    let run_git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(["-C", repo_path.to_str().unwrap()])
            .args(args)
            .output()
            .expect("Failed to run git command");
        assert!(output.status.success(), "Git command failed: {:?}", args);
    };

    run_git(&["init", "-b", "main"]);
    run_git(&["config", "user.email", "dume@example.com"]);
    run_git(&["config", "user.name", "DUM-E Agent"]);

    // Create initial commit
    let readme_file = repo_path.join("README.md");
    std::fs::write(&readme_file, "# Initial Repo\n").unwrap();
    run_git(&["add", "README.md"]);
    run_git(&["commit", "-m", "Initial commit"]);

    let db_path = repo_path.join(".dume/rust/harness.db");
    let artifacts_dir = repo_path.join(".dume/rust/artifacts");
    let store = HarnessStore::open(&db_path, &artifacts_dir).unwrap();

    // 2. Create Goal and Task with executable acceptance criteria
    let goal = store.create_goal("g_e2e", "Add math function").unwrap();
    let task = Task {
        id: "t_add_feature".to_string(),
        goal_id: goal.id.clone(),
        title: "Create math module".to_string(),
        description: "Create math.txt with 42".to_string(),
        status: TaskStatus::Ready,
        dependencies: vec![],
        acceptance_criteria: vec!["test -f math.txt".to_string(), "grep -q '42' math.txt".to_string()],
        allowed_paths: Some(vec!["math.txt".to_string()]),
        target_branch: "main".to_string(),
        created_at: 0,
        updated_at: 0,
    };
    store.create_task(&task).unwrap();

    // 3. Simulate Worker execution in isolated worktree
    let wt_dir = repo_path.join(".dume/rust/worktrees/wt_e2e");
    dume_git::worktree::create_git_worktree(repo_path, &wt_dir, "main").await.unwrap();

    let attempt = store.create_attempt(
        "att_e2e_1",
        &task.id,
        1,
        "worker_e2e",
        wt_dir.to_str().unwrap(),
        30_000,
    ).unwrap();

    // Worker modifies file according to task
    let target_file = wt_dir.join("math.txt");
    std::fs::write(&target_file, "42\n").unwrap();

    // Worker commits changes
    let candidate_commit = dume_git::commit::commit_worktree_changes(&wt_dir, "feat: add math.txt with 42").await.unwrap();

    // Worker creates result manifest
    let manifest = ResultManifest {
        attempt_id: attempt.id.clone(),
        candidate_commit: candidate_commit.clone(),
        modified_files: vec!["math.txt".to_string()],
        changed_artifacts: vec![],
        test_results: vec![TestResult {
            name: "test -f math.txt".to_string(),
            passed: true,
            exit_code: 0,
            stdout: "".to_string(),
            stderr: "".to_string(),
        }],
        summary: "Created math.txt".to_string(),
    };

    let manifest_bytes = serde_json::to_string(&manifest).unwrap();
    let manifest_hash = store.artifacts.save_artifact(manifest_bytes.as_bytes()).unwrap();

    // Worker submits attempt result
    store.submit_attempt_result(&attempt.id, 1, &candidate_commit, &manifest_hash).unwrap();
    let submitted_attempt = store.get_attempt(&attempt.id).unwrap();
    assert_eq!(submitted_attempt.status, AttemptStatus::ResultSubmitted);

    // Clean up worker worktree
    dume_git::worktree::remove_git_worktree(repo_path, &wt_dir).await.unwrap();

    // 4. Coordinator runs independent verification and cherry-pick integration
    let repo_str = repo_path.to_str().unwrap();
    let int_wt = repo_path.join(".dume/rust/integration-worktree");

    // Run independent acceptance test on candidate commit
    let verify_wt = repo_path.join(".dume/rust/verify-wt");
    dume_git::worktree::create_git_worktree(repo_path, &verify_wt, &candidate_commit).await.unwrap();

    let check_cmd = std::process::Command::new("sh")
        .arg("-c")
        .arg("test -f math.txt && grep -q '42' math.txt")
        .current_dir(&verify_wt)
        .output()
        .unwrap();
    assert!(check_cmd.status.success(), "Independent acceptance check passed");
    dume_git::worktree::remove_git_worktree(repo_path, &verify_wt).await.unwrap();

    // Perform cherry-pick integration
    let int_res = dume_git::integrate::integrate_candidate_commit(
        repo_path,
        "main",
        &candidate_commit,
        &int_wt,
    ).await.unwrap();

    match int_res {
        dume_git::integrate::IntegrationResult::Success { integration_commit, .. } => {
            store.update_attempt_status(&attempt.id, AttemptStatus::Accepted).unwrap();
            store.update_task_status(&task.id, TaskStatus::Completed).unwrap();
            store.update_goal_status(&goal.id, GoalStatus::Completed).unwrap();

            // Verify target branch actually contains the change and integration commit
            let main_head = std::process::Command::new("git")
                .args(["-C", repo_str, "rev-parse", "main"])
                .output()
                .unwrap();
            let main_sha = String::from_utf8_lossy(&main_head.stdout).trim().to_string();
            assert_eq!(main_sha, integration_commit, "Target branch ref was updated to integration commit");

            // Verify math.txt exists in main
            let show_file = std::process::Command::new("git")
                .args(["-C", repo_str, "show", "main:math.txt"])
                .output()
                .unwrap();
            assert!(show_file.status.success());
            assert_eq!(String::from_utf8_lossy(&show_file.stdout).trim(), "42");
        }
        _ => panic!("Integration expected to succeed"),
    }
}

#[tokio::test]
async fn test_r4_conflict_vs_acceptance_failure_separation() {
    let temp_repo = tempdir().unwrap();
    let repo_path = temp_repo.path();
    let repo_str = repo_path.to_str().unwrap();

    let run_git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(["-C", repo_str])
            .args(args)
            .output()
            .expect("Failed to run git command");
        assert!(output.status.success(), "Git command failed: {:?}", args);
    };

    run_git(&["init", "-b", "main"]);
    run_git(&["config", "user.email", "dume@example.com"]);
    run_git(&["config", "user.name", "DUM-E Agent"]);

    // Initial commit
    std::fs::write(repo_path.join("file.txt"), "line1\n").unwrap();
    run_git(&["add", "file.txt"]);
    run_git(&["commit", "-m", "Initial commit"]);

    let initial_head = {
        let out = std::process::Command::new("git").args(["-C", repo_str, "rev-parse", "main"]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    // 1. Create a conflicting commit on candidate branch
    let wt_conf = repo_path.join("wt_conf");
    dume_git::worktree::create_git_worktree(repo_path, &wt_conf, "main").await.unwrap();
    std::fs::write(wt_conf.join("file.txt"), "candidate conflicting edit\n").unwrap();
    let conf_commit = dume_git::commit::commit_worktree_changes(&wt_conf, "conflicting edit").await.unwrap();
    dume_git::worktree::remove_git_worktree(repo_path, &wt_conf).await.unwrap();

    // In main repo, create conflicting change
    std::fs::write(repo_path.join("file.txt"), "main different edit\n").unwrap();
    run_git(&["add", "file.txt"]);
    run_git(&["commit", "-m", "main edit"]);
    let main_updated_head = {
        let out = std::process::Command::new("git").args(["-C", repo_str, "rev-parse", "main"]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    // Attempt cherry-pick integration -> MUST result in Conflict, NOT acceptance failure
    let int_wt = repo_path.join("wt_int");
    let res = dume_git::integrate::integrate_candidate_commit(repo_path, "main", &conf_commit, &int_wt).await.unwrap();
    
    match res {
        dume_git::integrate::IntegrationResult::Conflict { .. } => {
            // Target branch ref must remain COMPLETELY UNTOUCHED
            let current_head = {
                let out = std::process::Command::new("git").args(["-C", repo_str, "rev-parse", "main"]).output().unwrap();
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            };
            assert_eq!(current_head, main_updated_head, "Target branch ref was not modified during conflict");
        }
        _ => panic!("Expected Conflict result, got {:?}", res),
    }
}

#[tokio::test]
async fn test_r5_branch_ref_ancestry_reconciliation() {
    let temp_repo = tempdir().unwrap();
    let repo_path = temp_repo.path();
    let repo_str = repo_path.to_str().unwrap();

    let run_git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(["-C", repo_str])
            .args(args)
            .output()
            .expect("Failed to run git command");
        assert!(output.status.success(), "Git command failed: {:?}", args);
    };

    run_git(&["init", "-b", "main"]);
    run_git(&["config", "user.email", "dume@example.com"]);
    run_git(&["config", "user.name", "DUM-E Agent"]);

    std::fs::write(repo_path.join("base.txt"), "base\n").unwrap();
    run_git(&["add", "base.txt"]);
    run_git(&["commit", "-m", "Base commit"]);

    let base_commit = {
        let out = std::process::Command::new("git").args(["-C", repo_str, "rev-parse", "main"]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    // Create candidate commit based on initial base
    let wt_cand = repo_path.join("wt_cand");
    dume_git::worktree::create_git_worktree(repo_path, &wt_cand, "main").await.unwrap();
    std::fs::write(wt_cand.join("feature.txt"), "new feature\n").unwrap();
    let candidate_commit = dume_git::commit::commit_worktree_changes(&wt_cand, "feat: new feature").await.unwrap();
    dume_git::worktree::remove_git_worktree(repo_path, &wt_cand).await.unwrap();

    // Advance main with an independent commit so cherry-pick has a new parent and produces a distinct hash
    std::fs::write(repo_path.join("independent.txt"), "independent work on main\n").unwrap();
    run_git(&["add", "independent.txt"]);
    run_git(&["commit", "-m", "independent main work"]);

    // Cherry-pick integrate
    let int_wt = repo_path.join("wt_int_r5");
    let int_res = dume_git::integrate::integrate_candidate_commit(repo_path, "main", &candidate_commit, &int_wt).await.unwrap();


    let integration_commit = match int_res {
        dume_git::integrate::IntegrationResult::Success { integration_commit, .. } => integration_commit,
        _ => panic!("Expected integration success"),
    };

    // Simulate normal subsequent commits created on main after integration
    std::fs::write(repo_path.join("later.txt"), "later commit\n").unwrap();
    run_git(&["add", "later.txt"]);
    run_git(&["commit", "-m", "subsequent commit on main"]);

    // Test Ancestry check (R5):
    // Check if integration_commit is an ancestor of main
    let is_ancestor = std::process::Command::new("git")
        .args(["-C", repo_str, "merge-base", "--is-ancestor", &integration_commit, "main"])
        .status()
        .unwrap()
        .success();

    assert!(is_ancestor, "R5 Ancestry check proves integration_commit is in main branch history even with subsequent commits");

    // Candidate commit hash itself is different from integration_commit hash
    assert_ne!(candidate_commit, integration_commit, "Candidate commit hash must differ from cherry-picked integration commit");
}




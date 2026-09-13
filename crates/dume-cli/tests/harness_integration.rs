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
    let lock1 = store
        .acquire_coordinator_lock("coord_main", "host_process_1", 5000)
        .unwrap();
    assert_eq!(lock1.epoch, 1);

    // 2. Create goal and DAG tasks: t1 -> t2
    let _goal = store
        .create_goal("goal_rust", "Rust Migration Goal")
        .unwrap();
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
    let _attempt = store
        .create_attempt("att_1", "task_1", 1, "worker_1", "/tmp/wt1", 5000)
        .unwrap();

    // 4. Simulate coordinator termination/crash (R1): release lock and new coordinator takes over
    store
        .release_coordinator_lock("coord_main", "host_process_1")
        .unwrap();
    let lock2 = store
        .acquire_coordinator_lock("coord_main", "host_process_2", 5000)
        .unwrap();
    assert_eq!(lock2.epoch, 2); // Monotonic increase to epoch 2

    // 5. Stale worker from epoch 1 tries to submit result -> MUST BE REJECTED by epoch fencing guard (R2)
    let stale_submit = store.submit_attempt_result("att_1", 1, "commit_old", "hash_old");
    assert!(
        matches!(
            stale_submit,
            Err(StoreError::StaleEpoch {
                attempt_epoch: 1,
                current_coordinator_epoch: 2
            })
        ),
        "Stale epoch submission from old coordinator epoch must be rejected"
    );

    // If another invalid epoch submits to an attempt recorded under epoch 1, it must fail with FencingViolation:
    let foreign_submit = store.submit_attempt_result("att_1", 99, "commit_fake", "hash_fake");
    assert!(matches!(
        foreign_submit,
        Err(StoreError::FencingViolation { .. })
    ));

    // 6. External operation dispatched without confirmation before crash (R3)
    store
        .record_external_operation_intent("op_1", "att_1", "deploy_prod_1", "Deploy service")
        .unwrap();
    store
        .update_external_operation_status("op_1", ExternalOperationStatus::Dispatched, None)
        .unwrap();

    // 7. Crash recovery inspection (R1 & R3)
    let (ready_verify, needs_attention, unknown_ops) = store.recover_state(lock2.epoch).unwrap();
    // Stale attempt was not successfully submitted before crash, so it transitions to needs_attention:
    assert_eq!(ready_verify.len(), 0);
    assert_eq!(needs_attention.len(), 1);
    assert_eq!(needs_attention[0].id, "att_1");

    // Operation was automatically moved to OutcomeUnknown upon unconfirmed crash recovery
    assert_eq!(unknown_ops.len(), 1);
    assert_eq!(unknown_ops[0].id, "op_1");
    assert_eq!(
        unknown_ops[0].status,
        ExternalOperationStatus::OutcomeUnknown
    );

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
        let lock = store
            .acquire_coordinator_lock("coord_main", "proc_parent_1", 10_000)
            .unwrap();
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
        let _att1 = store
            .create_attempt("att_c1", "t_crash", 1, "w1", "/tmp/wt_c1", 10_000)
            .unwrap();

        // Worker 2 finished and submitted result just before crash
        let _att2 = store
            .create_attempt("att_c2", "t_crash", 1, "w2", "/tmp/wt_c2", 10_000)
            .unwrap();
        store
            .submit_attempt_result("att_c2", 1, "commit_submitted", "hash_submitted")
            .unwrap();

        // Task 3 was already verified and completed earlier
        let _att3 = store
            .create_attempt("att_c3", "t_crash", 1, "w3", "/tmp/wt_c3", 10_000)
            .unwrap();
        store
            .submit_attempt_result("att_c3", 1, "commit_completed", "hash_completed")
            .unwrap();
        store
            .update_attempt_status("att_c3", AttemptStatus::Accepted)
            .unwrap();

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
            conn.execute("UPDATE coordinator_locks SET lease_expires_at = 0", [])
                .unwrap();
        }

        let lock2 = store2
            .acquire_coordinator_lock("coord_main", "proc_parent_2", 10_000)
            .unwrap();
        assert_eq!(lock2.epoch, 2); // Monotonic increase to 2

        // Crash recovery:
        // 1. Uncommitted running attempt from epoch 1 transitions to needs_attention
        // 2. Already submitted attempt is recovered into ready_for_verify queue!
        // 3. Already accepted attempt remains undisturbed
        let (ready, needs_attention, _) = store2.recover_state(lock2.epoch).unwrap();

        // att_c2 was submitted before crash -> MUST be recovered for verification
        assert_eq!(
            ready.len(),
            1,
            "Result-submitted attempt must be recovered for verification"
        );
        assert_eq!(ready[0].id, "att_c2");
        assert_eq!(
            ready[0].candidate_commit.as_deref(),
            Some("commit_submitted")
        );

        // att_c1 was running without submission -> MUST be quarantined to needs_attention
        assert_eq!(needs_attention.len(), 1);
        assert_eq!(needs_attention[0].id, "att_c1");
        assert_eq!(needs_attention[0].status, AttemptStatus::NeedsAttention);

        // att_c3 was already accepted -> remains Accepted
        let att3 = store2.get_attempt("att_c3").unwrap();
        assert_eq!(att3.status, AttemptStatus::Accepted);

        // Stale worker att_c1 late submission is rejected
        let stale = store2.submit_attempt_result("att_c1", 1, "commit_stale", "hash_stale");
        assert!(
            stale.is_err(),
            "Late worker submission after coordinator crash must be rejected"
        );
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
        acceptance_criteria: vec![
            "test -f math.txt".to_string(),
            "grep -q '42' math.txt".to_string(),
        ],
        allowed_paths: Some(vec!["math.txt".to_string()]),
        target_branch: "main".to_string(),
        created_at: 0,
        updated_at: 0,
    };
    store.create_task(&task).unwrap();

    // 3. Simulate Worker execution in isolated worktree
    let wt_dir = repo_path.join(".dume/rust/worktrees/wt_e2e");
    dume_git::worktree::create_git_worktree(repo_path, &wt_dir, "main")
        .await
        .unwrap();

    let attempt = store
        .create_attempt(
            "att_e2e_1",
            &task.id,
            1,
            "worker_e2e",
            wt_dir.to_str().unwrap(),
            30_000,
        )
        .unwrap();

    // Worker modifies file according to task
    let target_file = wt_dir.join("math.txt");
    std::fs::write(&target_file, "42\n").unwrap();

    // Worker commits changes
    let candidate_commit =
        dume_git::commit::commit_worktree_changes(&wt_dir, "feat: add math.txt with 42")
            .await
            .unwrap();

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
    let manifest_hash = store
        .artifacts
        .save_artifact(manifest_bytes.as_bytes())
        .unwrap();

    // Worker submits attempt result
    store
        .submit_attempt_result(&attempt.id, 1, &candidate_commit, &manifest_hash)
        .unwrap();
    let submitted_attempt = store.get_attempt(&attempt.id).unwrap();
    assert_eq!(submitted_attempt.status, AttemptStatus::ResultSubmitted);

    // Clean up worker worktree
    dume_git::worktree::remove_git_worktree(repo_path, &wt_dir)
        .await
        .unwrap();

    // 4. Coordinator runs independent verification and cherry-pick integration
    let repo_str = repo_path.to_str().unwrap();
    let int_wt = repo_path.join(".dume/rust/integration-worktree");

    // Run independent acceptance test on candidate commit
    let verify_wt = repo_path.join(".dume/rust/verify-wt");
    dume_git::worktree::create_git_worktree(repo_path, &verify_wt, &candidate_commit)
        .await
        .unwrap();

    let check_cmd = std::process::Command::new("sh")
        .arg("-c")
        .arg("test -f math.txt && grep -q '42' math.txt")
        .current_dir(&verify_wt)
        .output()
        .unwrap();
    assert!(
        check_cmd.status.success(),
        "Independent acceptance check passed"
    );
    dume_git::worktree::remove_git_worktree(repo_path, &verify_wt)
        .await
        .unwrap();

    // Perform cherry-pick integration
    let int_res = dume_git::integrate::integrate_candidate_commit(
        repo_path,
        "main",
        &candidate_commit,
        &int_wt,
    )
    .await
    .unwrap();

    match int_res {
        dume_git::integrate::IntegrationResult::Success {
            integration_commit, ..
        } => {
            store
                .update_attempt_status(&attempt.id, AttemptStatus::Accepted)
                .unwrap();
            store
                .update_task_status(&task.id, TaskStatus::Completed)
                .unwrap();
            store
                .update_goal_status(&goal.id, GoalStatus::Completed)
                .unwrap();

            // Verify target branch actually contains the change and integration commit
            let main_head = std::process::Command::new("git")
                .args(["-C", repo_str, "rev-parse", "main"])
                .output()
                .unwrap();
            let main_sha = String::from_utf8_lossy(&main_head.stdout)
                .trim()
                .to_string();
            assert_eq!(
                main_sha, integration_commit,
                "Target branch ref was updated to integration commit"
            );

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

    let _initial_head = {
        let out = std::process::Command::new("git")
            .args(["-C", repo_str, "rev-parse", "main"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    // 1. Create a conflicting commit on candidate branch
    let wt_conf = repo_path.join("wt_conf");
    dume_git::worktree::create_git_worktree(repo_path, &wt_conf, "main")
        .await
        .unwrap();
    std::fs::write(wt_conf.join("file.txt"), "candidate conflicting edit\n").unwrap();
    let conf_commit = dume_git::commit::commit_worktree_changes(&wt_conf, "conflicting edit")
        .await
        .unwrap();
    dume_git::worktree::remove_git_worktree(repo_path, &wt_conf)
        .await
        .unwrap();

    // In main repo, create conflicting change
    std::fs::write(repo_path.join("file.txt"), "main different edit\n").unwrap();
    run_git(&["add", "file.txt"]);
    run_git(&["commit", "-m", "main edit"]);
    let main_updated_head = {
        let out = std::process::Command::new("git")
            .args(["-C", repo_str, "rev-parse", "main"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    // Attempt cherry-pick integration -> MUST result in Conflict, NOT acceptance failure
    let int_wt = repo_path.join("wt_int");
    let res =
        dume_git::integrate::integrate_candidate_commit(repo_path, "main", &conf_commit, &int_wt)
            .await
            .unwrap();

    match res {
        dume_git::integrate::IntegrationResult::Conflict { .. } => {
            // Target branch ref must remain COMPLETELY UNTOUCHED
            let current_head = {
                let out = std::process::Command::new("git")
                    .args(["-C", repo_str, "rev-parse", "main"])
                    .output()
                    .unwrap();
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            };
            assert_eq!(
                current_head, main_updated_head,
                "Target branch ref was not modified during conflict"
            );
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

    let _base_commit = {
        let out = std::process::Command::new("git")
            .args(["-C", repo_str, "rev-parse", "main"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    // Create candidate commit based on initial base
    let wt_cand = repo_path.join("wt_cand");
    dume_git::worktree::create_git_worktree(repo_path, &wt_cand, "main")
        .await
        .unwrap();
    std::fs::write(wt_cand.join("feature.txt"), "new feature\n").unwrap();
    let candidate_commit = dume_git::commit::commit_worktree_changes(&wt_cand, "feat: new feature")
        .await
        .unwrap();
    dume_git::worktree::remove_git_worktree(repo_path, &wt_cand)
        .await
        .unwrap();

    // Advance main with an independent commit so cherry-pick has a new parent and produces a distinct hash
    std::fs::write(
        repo_path.join("independent.txt"),
        "independent work on main\n",
    )
    .unwrap();
    run_git(&["add", "independent.txt"]);
    run_git(&["commit", "-m", "independent main work"]);

    // Cherry-pick integrate
    let int_wt = repo_path.join("wt_int_r5");
    let int_res = dume_git::integrate::integrate_candidate_commit(
        repo_path,
        "main",
        &candidate_commit,
        &int_wt,
    )
    .await
    .unwrap();

    let integration_commit = match int_res {
        dume_git::integrate::IntegrationResult::Success {
            integration_commit, ..
        } => integration_commit,
        _ => panic!("Expected integration success"),
    };

    // Simulate normal subsequent commits created on main after integration
    std::fs::write(repo_path.join("later.txt"), "later commit\n").unwrap();
    run_git(&["add", "later.txt"]);
    run_git(&["commit", "-m", "subsequent commit on main"]);

    // Test Ancestry check (R5):
    // Check if integration_commit is an ancestor of main
    let is_ancestor = std::process::Command::new("git")
        .args([
            "-C",
            repo_str,
            "merge-base",
            "--is-ancestor",
            &integration_commit,
            "main",
        ])
        .status()
        .unwrap()
        .success();

    assert!(
        is_ancestor,
        "R5 Ancestry check proves integration_commit is in main branch history even with subsequent commits"
    );

    // Candidate commit hash itself is different from integration_commit hash
    assert_ne!(
        candidate_commit, integration_commit,
        "Candidate commit hash must differ from cherry-picked integration commit"
    );
}

#[tokio::test]
async fn test_r4_clean_merge_but_acceptance_criteria_failure_rejects() {
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

    std::fs::write(repo_path.join("init.txt"), "hello\n").unwrap();
    run_git(&["add", "init.txt"]);
    run_git(&["commit", "-m", "init"]);
    let base_head = {
        let out = std::process::Command::new("git")
            .args(["-C", repo_str, "rev-parse", "main"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    let db_path = repo_path.join(".dume/rust/harness.db");
    let artifacts_dir = repo_path.join(".dume/rust/artifacts");
    let store = HarnessStore::open(&db_path, &artifacts_dir).unwrap();
    let _goal = store
        .create_goal("g_r4", "Test R4 acceptance failure")
        .unwrap();

    // Task specifies acceptance criteria that WILL FAIL (e.g. grep for required_secret)
    let task = Task {
        id: "t_r4_fail".to_string(),
        goal_id: "g_r4".to_string(),
        title: "Feature with failing test".to_string(),
        description: "Must fail acceptance".to_string(),
        status: TaskStatus::Ready,
        dependencies: vec![],
        acceptance_criteria: vec!["grep -q 'required_secret' app.txt".to_string()],
        allowed_paths: Some(vec!["app.txt".to_string()]),
        target_branch: "main".to_string(),
        created_at: 0,
        updated_at: 0,
    };
    store.create_task(&task).unwrap();

    // Create a candidate commit that cleanly merges but FAILS acceptance criteria
    let wt_cand = repo_path.join("wt_r4_cand");
    dume_git::worktree::create_git_worktree(repo_path, &wt_cand, "main")
        .await
        .unwrap();
    std::fs::write(
        wt_cand.join("app.txt"),
        "wrong content without required string\n",
    )
    .unwrap();
    let candidate_commit =
        dume_git::commit::commit_worktree_changes(&wt_cand, "feat: wrong content")
            .await
            .unwrap();
    dume_git::worktree::remove_git_worktree(repo_path, &wt_cand)
        .await
        .unwrap();

    let manifest = ResultManifest {
        attempt_id: "att_r4_fail".to_string(),
        candidate_commit: candidate_commit.clone(),
        modified_files: vec!["app.txt".to_string()],
        changed_artifacts: vec![],
        test_results: vec![],
        summary: "wrong content".to_string(),
    };
    let manifest_bytes = serde_json::to_string(&manifest).unwrap();
    let manifest_hash = store
        .artifacts
        .save_artifact(manifest_bytes.as_bytes())
        .unwrap();

    let _attempt = store
        .create_attempt("att_r4_fail", &task.id, 1, "worker", "/tmp/wt", 30_000)
        .unwrap();
    store
        .submit_attempt_result("att_r4_fail", 1, &candidate_commit, &manifest_hash)
        .unwrap();

    // Coordinator runs real production verify_and_integrate_attempt directly
    let res = dume_cli::verify_and_integrate_attempt(
        &store,
        repo_str,
        &store.get_attempt("att_r4_fail").unwrap(),
        1,
    )
    .await;
    assert!(res.is_ok());

    // Task and Attempt MUST be Rejected / Failed
    let updated_att = store.get_attempt("att_r4_fail").unwrap();
    assert_eq!(updated_att.status, AttemptStatus::Rejected);
    let updated_task = store.get_task(&task.id).unwrap();
    assert_eq!(updated_task.status, TaskStatus::Failed);

    // Target branch ref MUST be completely unchanged
    let current_head = {
        let out = std::process::Command::new("git")
            .args(["-C", repo_str, "rev-parse", "main"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    assert_eq!(
        current_head, base_head,
        "Target branch head must remain untouched upon acceptance failure"
    );
}

#[tokio::test]
async fn test_r7_tampered_manifest_artifact_blocks_verification() {
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
    std::fs::write(repo_path.join("init.txt"), "hello\n").unwrap();
    run_git(&["add", "init.txt"]);
    run_git(&["commit", "-m", "init"]);
    let base_head = {
        let out = std::process::Command::new("git")
            .args(["-C", repo_str, "rev-parse", "main"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    let db_path = repo_path.join(".dume/rust/harness.db");
    let artifacts_dir = repo_path.join(".dume/rust/artifacts");
    let store = HarnessStore::open(&db_path, &artifacts_dir).unwrap();
    let _goal = store.create_goal("g_r7", "Test R7 tampering").unwrap();

    let task = Task {
        id: "t_r7".to_string(),
        goal_id: "g_r7".to_string(),
        title: "Tampered manifest test".to_string(),
        description: "Evidence tampering".to_string(),
        status: TaskStatus::Ready,
        dependencies: vec![],
        acceptance_criteria: vec!["true".to_string()],
        allowed_paths: Some(vec!["valid.txt".to_string()]),
        target_branch: "main".to_string(),
        created_at: 0,
        updated_at: 0,
    };
    store.create_task(&task).unwrap();

    let wt_cand = repo_path.join("wt_r7_cand");
    dume_git::worktree::create_git_worktree(repo_path, &wt_cand, "main")
        .await
        .unwrap();
    std::fs::write(wt_cand.join("valid.txt"), "content\n").unwrap();
    let candidate_commit = dume_git::commit::commit_worktree_changes(&wt_cand, "feat: valid")
        .await
        .unwrap();
    dume_git::worktree::remove_git_worktree(repo_path, &wt_cand)
        .await
        .unwrap();

    let manifest = ResultManifest {
        attempt_id: "att_r7".to_string(),
        candidate_commit: candidate_commit.clone(),
        modified_files: vec!["valid.txt".to_string()],
        changed_artifacts: vec![],
        test_results: vec![],
        summary: "valid".to_string(),
    };
    let manifest_bytes = serde_json::to_string(&manifest).unwrap();
    let manifest_hash = store
        .artifacts
        .save_artifact(manifest_bytes.as_bytes())
        .unwrap();

    let _attempt = store
        .create_attempt("att_r7", &task.id, 1, "worker", "/tmp/wt", 30_000)
        .unwrap();
    store
        .submit_attempt_result("att_r7", 1, &candidate_commit, &manifest_hash)
        .unwrap();

    // External corruption / tampering: modify artifact file on disk
    let shard = &manifest_hash[..2];
    let artifact_file = artifacts_dir.join(shard).join(&manifest_hash);
    std::fs::write(&artifact_file, b"corrupted payload").unwrap();

    // Verify should catch tampering via InvalidData and REJECT integration
    let res = dume_cli::verify_and_integrate_attempt(
        &store,
        repo_str,
        &store.get_attempt("att_r7").unwrap(),
        1,
    )
    .await;
    assert!(res.is_ok());

    let updated_att = store.get_attempt("att_r7").unwrap();
    assert_eq!(
        updated_att.status,
        AttemptStatus::Rejected,
        "Tampered artifact MUST be rejected"
    );

    let updated_task = store.get_task(&task.id).unwrap();
    assert_eq!(updated_task.status, TaskStatus::Failed);

    let current_head = {
        let out = std::process::Command::new("git")
            .args(["-C", repo_str, "rev-parse", "main"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    assert_eq!(
        current_head, base_head,
        "Target branch ref must not advance when artifact is corrupted"
    );
}

#[tokio::test]
async fn test_r5_crash_after_git_update_before_applied_db_record_recovery() {
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
    std::fs::write(repo_path.join("init.txt"), "base\n").unwrap();
    run_git(&["add", "init.txt"]);
    run_git(&["commit", "-m", "init"]);
    let base_head = {
        let out = std::process::Command::new("git")
            .args(["-C", repo_str, "rev-parse", "main"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    let db_path = repo_path.join(".dume/rust/harness.db");
    let artifacts_dir = repo_path.join(".dume/rust/artifacts");

    let candidate_commit = {
        let wt = repo_path.join("wt_r5_crash");
        dume_git::worktree::create_git_worktree(repo_path, &wt, "main")
            .await
            .unwrap();
        std::fs::write(wt.join("feature.txt"), "crash-resilient feature\n").unwrap();
        let c = dume_git::commit::commit_worktree_changes(&wt, "feat: crash-resilient")
            .await
            .unwrap();
        dume_git::worktree::remove_git_worktree(repo_path, &wt)
            .await
            .unwrap();
        c
    };

    let int_wt = repo_path.join("wt_r5_int");
    let prep_res = dume_git::integrate::prepare_candidate_cherry_pick(
        repo_path,
        "main",
        &candidate_commit,
        &int_wt,
    )
    .await
    .unwrap();

    let integration_commit = match prep_res {
        dume_git::integrate::IntegrationResult::Success {
            integration_commit, ..
        } => integration_commit,
        _ => panic!("Expected prep success"),
    };

    // Phase 1: DB writes 'pending' record
    {
        let store = HarnessStore::open(&db_path, &artifacts_dir).unwrap();
        let pending = Integration {
            target_branch: "main".to_string(),
            base_commit: base_head.clone(),
            candidate_commit: candidate_commit.clone(),
            integration_commit: Some(integration_commit.clone()),
            status: IntegrationStatus::Pending,
            error_message: None,
            integrated_at: 1000,
        };
        store.record_integration(&pending).unwrap();
    }

    // Phase 2: Git branch ref is updated to integration_commit
    dume_git::integrate::apply_branch_update(repo_path, "main", &integration_commit, &base_head)
        .await
        .unwrap();

    // CRASH INJECTION:
    // Process abruptly terminates here BEFORE writing status = 'applied' to SQLite!
    // At this moment:
    // - Git 'main' points to integration_commit
    // - SQLite record is still 'pending'

    // RECOVERY (New Coordinator starts up):
    {
        let store2 = HarnessStore::open(&db_path, &artifacts_dir).unwrap();
        // Coordinator inspects pending integrations on startup
        let pending_list = store2.list_pending_integrations().unwrap();
        assert_eq!(
            pending_list.len(),
            1,
            "Must find exactly one pending integration requiring reconciliation"
        );

        let pending = &pending_list[0];
        let current_ref = dume_git::integrate::resolve_ref(repo_path, &pending.target_branch)
            .await
            .unwrap();
        assert_eq!(
            current_ref, integration_commit,
            "Git ref was already advanced before crash"
        );

        // Reconcile: advance DB record to Applied WITHOUT performing a redundant cherry-pick
        let mut reconciled = pending.clone();
        reconciled.status = IntegrationStatus::Applied;
        store2.record_integration(&reconciled).unwrap();

        let updated_record = store2
            .get_integration("main", &candidate_commit)
            .unwrap()
            .unwrap();
        assert_eq!(updated_record.status, IntegrationStatus::Applied);
        assert_eq!(
            updated_record.integration_commit.as_deref(),
            Some(integration_commit.as_str())
        );

        // Check git commit history has only ONE integration commit (no duplicate cherry-picks)
        let log_out = std::process::Command::new("git")
            .args(["-C", repo_str, "log", "--oneline", "main"])
            .output()
            .unwrap();
        let log_str = String::from_utf8_lossy(&log_out.stdout);
        let commit_count = log_str
            .lines()
            .filter(|l| l.contains("feat: crash-resilient"))
            .count();
        assert_eq!(
            commit_count, 1,
            "Exactly one cherry-pick commit must exist in history, no duplicates"
        );
    }
}

#[tokio::test]
async fn test_r5_subsequent_commit_ancestry_reconciliation() {
    let dir = tempdir().unwrap();
    let repo_path = dir.path().join("repo");
    let repo_str = repo_path.to_str().unwrap();
    let db_path = dir.path().join("harness.db");
    let artifacts_dir = dir.path().join("artifacts");

    // Init Git repository with initial commit
    std::process::Command::new("git")
        .args(["init", "-b", "main", repo_str])
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["-C", repo_str, "config", "user.name", "Dume Test"])
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["-C", repo_str, "config", "user.email", "test@dume.local"])
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["-C", repo_str, "config", "commit.gpgSign", "false"])
        .output()
        .unwrap();

    std::fs::write(repo_path.join("README.md"), "# Repo\n").unwrap();
    std::process::Command::new("git")
        .args(["-C", repo_str, "add", "."])
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["-C", repo_str, "commit", "-m", "initial"])
        .output()
        .unwrap();

    let base_head = dume_git::integrate::resolve_ref(&repo_path, "main")
        .await
        .unwrap();

    // Create candidate commit
    let candidate_commit = {
        let wt = repo_path.join("wt_cand");
        dume_git::worktree::create_git_worktree(&repo_path, &wt, "main")
            .await
            .unwrap();
        std::fs::write(wt.join("feature.txt"), "feature 1\n").unwrap();
        let c = dume_git::commit::commit_worktree_changes(&wt, "feat: feature 1")
            .await
            .unwrap();
        dume_git::worktree::remove_git_worktree(&repo_path, &wt)
            .await
            .unwrap();
        c
    };

    let int_wt = repo_path.join("wt_int");
    let prep_res = dume_git::integrate::prepare_candidate_cherry_pick(
        &repo_path,
        "main",
        &candidate_commit,
        &int_wt,
    )
    .await
    .unwrap();

    let integration_commit = match prep_res {
        dume_git::integrate::IntegrationResult::Success {
            integration_commit, ..
        } => integration_commit,
        _ => panic!("Expected prep success"),
    };

    // Store writes Pending record
    {
        let store = HarnessStore::open(&db_path, &artifacts_dir).unwrap();
        let pending = Integration {
            target_branch: "main".to_string(),
            base_commit: base_head.clone(),
            candidate_commit: candidate_commit.clone(),
            integration_commit: Some(integration_commit.clone()),
            status: IntegrationStatus::Pending,
            error_message: None,
            integrated_at: 1000,
        };
        store.record_integration(&pending).unwrap();
    }

    // Git ref updated to integration_commit
    dume_git::integrate::apply_branch_update(&repo_path, "main", &integration_commit, &base_head)
        .await
        .unwrap();

    // Now a normal subsequent commit is added to main by another task/user!
    std::fs::write(
        repo_path.join("subsequent.txt"),
        "subsequent normal commit\n",
    )
    .unwrap();
    std::process::Command::new("git")
        .args(["-C", repo_str, "add", "."])
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["-C", repo_str, "commit", "-m", "feat: subsequent normal"])
        .output()
        .unwrap();

    let subsequent_head = dume_git::integrate::resolve_ref(&repo_path, "main")
        .await
        .unwrap();
    assert_ne!(
        subsequent_head, integration_commit,
        "HEAD has advanced past integration_commit"
    );

    // Coordinator reconciles pending integration on restart
    {
        let store = HarnessStore::open(&db_path, &artifacts_dir).unwrap();
        let pending_list = store.list_pending_integrations().unwrap();
        assert_eq!(pending_list.len(), 1);

        let pending = &pending_list[0];
        let current_ref = dume_git::integrate::resolve_ref(&repo_path, &pending.target_branch)
            .await
            .unwrap();

        // Ancestry check: integration_commit is an ancestor of subsequent_head
        let is_in_ancestry = dume_git::integrate::is_ancestor(
            &repo_path,
            pending.integration_commit.as_ref().unwrap(),
            &current_ref,
        )
        .await
        .unwrap();
        assert!(
            is_in_ancestry,
            "integration_commit must be recognized in ancestry"
        );

        let mut applied = pending.clone();
        applied.status = IntegrationStatus::Applied;
        store.record_integration(&applied).unwrap();

        let updated = store
            .get_integration("main", &candidate_commit)
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, IntegrationStatus::Applied);
    }
}

#[tokio::test]
async fn test_r5_process_boundary_crash_and_recovery() {
    let dir = tempdir().unwrap();
    let repo_path = dir.path().join("repo");
    let repo_str = repo_path.to_str().unwrap();
    let db_path = dir.path().join("harness.db");
    let artifacts_dir = dir.path().join("artifacts");

    // Initialize Git repository
    std::process::Command::new("git")
        .args(["init", "-b", "main", repo_str])
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["-C", repo_str, "config", "user.name", "Dume Test"])
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["-C", repo_str, "config", "user.email", "test@dume.local"])
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["-C", repo_str, "config", "commit.gpgSign", "false"])
        .output()
        .unwrap();

    std::fs::write(repo_path.join("README.md"), "# Initial\n").unwrap();
    std::process::Command::new("git")
        .args(["-C", repo_str, "add", "."])
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["-C", repo_str, "commit", "-m", "initial"])
        .output()
        .unwrap();

    let _base_head = dume_git::integrate::resolve_ref(&repo_path, "main")
        .await
        .unwrap();

    // Prepare candidate cherry-pick
    let candidate_commit = {
        let wt = repo_path.join("wt_cand");
        dume_git::worktree::create_git_worktree(&repo_path, &wt, "main")
            .await
            .unwrap();
        std::fs::write(wt.join("crash_boundary.txt"), "boundary feature\n").unwrap();
        let c = dume_git::commit::commit_worktree_changes(&wt, "feat: boundary feature")
            .await
            .unwrap();
        dume_git::worktree::remove_git_worktree(&repo_path, &wt)
            .await
            .unwrap();
        c
    };

    // 1. Initial process: Set up candidate attempt ready for verification
    let _attempt = {
        let store = HarnessStore::open(&db_path, &artifacts_dir).unwrap();
        store
            .create_goal("goal_r5", "R5 Crash Boundary Test")
            .unwrap();
        let task = Task {
            id: "task_r5".to_string(),
            goal_id: "goal_r5".to_string(),
            title: "Task R5".to_string(),
            description: "Test crash boundary".to_string(),
            status: TaskStatus::Ready,
            dependencies: vec![],
            acceptance_criteria: vec!["true".to_string()],
            allowed_paths: None,
            target_branch: "main".to_string(),
            created_at: 0,
            updated_at: 0,
        };
        store.create_task(&task).unwrap();

        let manifest = ResultManifest {
            attempt_id: "att_r5_1".to_string(),
            candidate_commit: candidate_commit.clone(),
            modified_files: vec!["crash_boundary.txt".to_string()],
            changed_artifacts: vec![],
            test_results: vec![],
            summary: "boundary feature".to_string(),
        };
        let manifest_bytes = serde_json::to_string(&manifest).unwrap();
        let manifest_hash = store
            .artifacts
            .save_artifact(manifest_bytes.as_bytes())
            .unwrap();

        let att = store
            .create_attempt("att_r5_1", "task_r5", 1, "worker_1", repo_str, 10_000)
            .unwrap();
        store
            .submit_attempt_result("att_r5_1", 1, &candidate_commit, &manifest_hash)
            .unwrap();
        att
    };

    // 2. Spawn real coordinator OS child process WITH exact crash hook enabled:
    // DUME_TEST_CRASH_AFTER_GIT_UPDATE=1
    // The coordinator will:
    // 1) Verify attempt
    // 2) Write Pending to SQLite
    // 3) Successfully update Git ref via CAS
    // 4) Emit boundary signal "DUME_BOUNDARY_GIT_UPDATED_BEFORE_DB_APPLIED"
    // 5) Exit immediately before writing Applied!
    let dume_bin = env!("CARGO_BIN_EXE_dume");
    let child = std::process::Command::new(dume_bin)
        .args([
            "coordinator",
            "--db-path",
            db_path.to_str().unwrap(),
            "--artifacts-dir",
            artifacts_dir.to_str().unwrap(),
            "--repo-path",
            repo_str,
        ])
        .env("DUME_TEST_CRASH_AFTER_GIT_UPDATE", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to spawn real dume coordinator process with failure injection");

    let pid = child.id();
    assert!(pid > 0, "Real child process must have valid OS PID");

    // Wait for child to exit at exact crash boundary
    let child_res = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&child_res.stdout);
    assert!(
        stdout.contains("DUME_BOUNDARY_GIT_UPDATED_BEFORE_DB_APPLIED"),
        "Child must reach exact crash boundary between Git ref update and DB write"
    );
    assert_eq!(
        child_res.status.code(),
        Some(137),
        "Child must exit immediately at crash boundary"
    );

    // 3. Verify exact crash state:
    // - Git ref WAS updated to integration_commit
    // - SQLite record is STILL 'pending' (crash-before-db-record confirmed!)
    {
        let store_crashed = HarnessStore::open(&db_path, &artifacts_dir).unwrap();
        let pending = store_crashed
            .get_integration("main", &candidate_commit)
            .unwrap()
            .expect("Pending record must exist in DB");
        assert_eq!(
            pending.status,
            IntegrationStatus::Pending,
            "Database must still be in Pending state at crash moment"
        );
        let expected_integration_commit = pending
            .integration_commit
            .clone()
            .expect("Pending record must record planned integration_commit");

        let current_git_ref = dume_git::integrate::resolve_ref(&repo_path, "main")
            .await
            .unwrap();
        assert_eq!(
            current_git_ref, expected_integration_commit,
            "Git ref must have already advanced to integration commit"
        );
    }

    // 4. Recovery: Start a NEW, fresh coordinator process (without failure injection hook)
    // Mark crashed process's lease expired so recovery coordinator takes over immediately
    {
        let store = HarnessStore::open(&db_path, &artifacts_dir).unwrap();
        store
            .force_expire_coordinator_lock("default_coordinator")
            .unwrap();
    }

    let recovery_child = std::process::Command::new(dume_bin)
        .args([
            "coordinator",
            "--db-path",
            db_path.to_str().unwrap(),
            "--artifacts-dir",
            artifacts_dir.to_str().unwrap(),
            "--repo-path",
            repo_str,
        ])
        .output()
        .expect("Failed to run recovery coordinator process");

    let rec_stdout = String::from_utf8_lossy(&recovery_child.stdout);
    let rec_stderr = String::from_utf8_lossy(&recovery_child.stderr);
    assert!(
        recovery_child.status.success(),
        "Recovery coordinator must exit successfully. stdout: {}, stderr: {}",
        rec_stdout,
        rec_stderr
    );

    // 5. Verify post-recovery state:
    // DB record is now Applied, no duplicate cherry-picks exist in git log
    {
        let store_recovered = HarnessStore::open(&db_path, &artifacts_dir).unwrap();
        let record = store_recovered
            .get_integration("main", &candidate_commit)
            .unwrap()
            .expect("Record must exist");
        assert_eq!(
            record.status,
            IntegrationStatus::Applied,
            "Recovered coordinator must reconcile Pending to Applied"
        );

        // Verify git log has exactly ONE cherry-pick commit
        let log_out = std::process::Command::new("git")
            .args(["-C", repo_str, "log", "--oneline", "main"])
            .output()
            .unwrap();
        let log_str = String::from_utf8_lossy(&log_out.stdout);
        let count = log_str
            .lines()
            .filter(|l| l.contains("feat: boundary feature"))
            .count();
        assert_eq!(
            count, 1,
            "Exactly one cherry-pick commit in history, no duplicate integration"
        );
    }
}

fn isolated_cli_command(program: &str, home: &std::path::Path) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    // Whitelist only process essentials: no provider credentials, proxy settings,
    // credential-store overrides, or user Git configuration can leak into this fixture.
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(home);

    // Native Windows needs these system environment variables to launch child processes and resolve networking
    for var in ["SystemRoot", "SYSTEMROOT", "windir", "WINDIR", "ComSpec", "COMSPEC", "PATHEXT"] {
        if let Some(val) = std::env::var_os(var) {
            command.env(var, val);
        }
    }

    command
}

async fn cli_http_fixture(
    listener: tokio::net::TcpListener,
    reject: bool,
) -> Vec<serde_json::Value> {
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut requests = Vec::new();
    for turn in 0..if reject { 1 } else { 2 } {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let header_end = loop {
            let mut chunk = [0; 4096];
            let size = socket.read(&mut chunk).await.unwrap();
            assert!(size > 0, "Worker closed connection before request headers");
            bytes.extend_from_slice(&chunk[..size]);
            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
        assert!(headers.starts_with("POST /v1/chat/completions "));
        assert!(headers.lines().any(|line| {
            line.split_once(':').is_some_and(|(key, value)| {
                key.eq_ignore_ascii_case("authorization")
                    && value.trim() == "Bearer cli-fixture-key"
            })
        }));
        assert!(!headers.to_ascii_lowercase().contains("chatgpt-account-id:"));
        let length: usize = headers
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().unwrap())
            })
            .unwrap();
        while bytes.len() < header_end + length {
            let mut chunk = [0; 4096];
            let size = socket.read(&mut chunk).await.unwrap();
            assert!(size > 0, "Worker closed connection before request body");
            bytes.extend_from_slice(&chunk[..size]);
        }
        let request: serde_json::Value =
            serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
        assert_eq!(request["model"], "gpt-5.4");
        assert_eq!(request["stream"], true);
        assert_eq!(request["messages"][1]["content"], "Initialize feature");
        requests.push(request);

        let (status, content_type, body) = if reject {
            (
                "401 Unauthorized",
                "application/json",
                json!({"error":{"message":"fixture rejected"}}).to_string(),
            )
        } else {
            let choice = if turn == 0 {
                json!({
                    "delta": {"tool_calls": [{
                        "index": 0, "id": "call_cli_write",
                        "function": {
                            "name": "write_file",
                            "arguments": "{\"path\":\"feature.txt\",\"content\":\"fixture feature\\n\"}"
                        }
                    }]},
                    "finish_reason": "tool_calls"
                })
            } else {
                json!({"delta":{"content":"Feature initialized."},"finish_reason":"stop"})
            };
            (
                "200 OK",
                "text/event-stream",
                format!("data: {}\n\n", json!({"choices":[choice]})),
            )
        };
        socket.write_all(format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ).as_bytes()).await.unwrap();
    }
    requests
}

async fn run_binary_cli_lifecycle(reject: bool) {
    let dir = tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let repo_path = dir.path().join("repo");
    let repo_str = repo_path.to_str().unwrap();
    let db_path = dir.path().join("harness.db");
    let artifacts_dir = dir.path().join("artifacts");
    let run_git = |args: &[&str]| {
        let output = isolated_cli_command("git", &home)
            .args(["-C", repo_str])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };

    // Init Git repo
    std::fs::create_dir(&repo_path).unwrap();
    run_git(&["init", "-b", "main"]);
    run_git(&["config", "user.name", "Dume Test"]);
    run_git(&["config", "user.email", "test@dume.local"]);
    run_git(&["config", "commit.gpgSign", "false"]);
    std::fs::write(repo_path.join("README.md"), "# E2E Binary Test\n").unwrap();
    run_git(&["add", "."]);
    run_git(&["commit", "-m", "init"]);
    let initial_commit = String::from_utf8(run_git(&["rev-parse", "HEAD"]).stdout).unwrap();

    let dume_bin = env!("CARGO_BIN_EXE_dume");

    // 1. Run binary status check on empty db
    let status_out = isolated_cli_command(dume_bin, &home)
        .args([
            "status",
            "--db-path",
            db_path.to_str().unwrap(),
            "--artifacts-dir",
            artifacts_dir.to_str().unwrap(),
        ])
        .output()
        .expect("Failed to execute dume status");
    assert!(
        status_out.status.success(),
        "dume status command must succeed"
    );
    let status_str = String::from_utf8_lossy(&status_out.stdout);
    assert!(
        status_str.contains("DUM-E Harness Status"),
        "Must print status banner"
    );

    // 2. Run worker attempt command directly via CLI
    let wt = repo_path.join("worker_e2e_wt");
    run_git(&["worktree", "add", "--detach", wt.to_str().unwrap(), "main"]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            cli_http_fixture(listener, reject),
        )
        .await
        .expect("CLI mock server timed out")
    });

    let test_cmd = if cfg!(windows) {
        "powershell -NoProfile -Command \"if ((Get-Content feature.txt) -eq 'fixture feature') { exit 0 } else { exit 1 }\""
    } else {
        "test \"$(cat feature.txt)\" = 'fixture feature'"
    };

    let mut worker = isolated_cli_command(dume_bin, &home);
    worker.env("OPENAI_API_KEY", "cli-fixture-key").args([
        "worker",
        "--attempt-id",
        "att_cli_1",
        "--worktree-path",
        wt.to_str().unwrap(),
        "--test-command",
        test_cmd,
        "--task-prompt",
        "Initialize feature",
        "--model",
        "openai/gpt-5.4",
        "--base-url",
        &base_url,
    ]);
    let worker_out = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        tokio::process::Command::from(worker)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("Worker timed out")
    .expect("Failed to execute dume worker");
    let requests = server.await.unwrap();
    let worker_stdout = String::from_utf8_lossy(&worker_out.stdout);
    let messages: Vec<serde_json::Value> = worker_stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let completed = messages
        .iter()
        .find(|message| message["type"] == "completed");
    if reject {
        assert!(
            !worker_out.status.success(),
            "HTTP rejection must fail the CLI"
        );
        assert!(String::from_utf8_lossy(&worker_out.stderr).contains("401"));
        assert!(
            completed.is_none(),
            "Failed inference must not emit a completed manifest"
        );
        assert_eq!(requests.len(), 1);
        assert!(!wt.join("feature.txt").exists());
        let head = run_git(&["-C", wt.to_str().unwrap(), "rev-parse", "HEAD"]);
        assert_eq!(String::from_utf8(head.stdout).unwrap(), initial_commit);
    } else {
        assert!(
            worker_out.status.success(),
            "Worker failed: {}",
            String::from_utf8_lossy(&worker_out.stderr)
        );
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1]["messages"][2]["tool_calls"][0]["id"],
            "call_cli_write"
        );
        assert_eq!(requests[1]["messages"][3]["role"], "tool");
        assert_eq!(requests[1]["messages"][3]["tool_call_id"], "call_cli_write");
        assert!(
            requests[1]["messages"][3]["content"]
                .as_str()
                .unwrap()
                .contains("Successfully wrote")
        );
        let manifest: ResultManifest = serde_json::from_value(
            completed.expect("Missing completed manifest")["manifest"].clone(),
        )
        .unwrap();
        assert_eq!(manifest.attempt_id, "att_cli_1");
        assert_eq!(manifest.modified_files, vec!["feature.txt"]);
        assert_eq!(manifest.test_results.len(), 1);
        assert!(
            manifest.test_results[0].passed,
            "Test command failed! exit_code: {}, stdout: {}, stderr: {}",
            manifest.test_results[0].exit_code,
            manifest.test_results[0].stdout,
            manifest.test_results[0].stderr
        );
        assert_eq!(manifest.test_results[0].exit_code, 0);
        assert_ne!(manifest.candidate_commit, initial_commit.trim());
        let committed = run_git(&[
            "show",
            &format!("{}:feature.txt", manifest.candidate_commit),
        ]);
        assert_eq!(
            String::from_utf8(committed.stdout).unwrap(),
            "fixture feature\n"
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("feature.txt")).unwrap(),
            "fixture feature\n"
        );
    }
}

#[tokio::test]
async fn test_binary_cli_lifecycle_end_to_end() {
    run_binary_cli_lifecycle(false).await;
}

#[tokio::test]
async fn test_binary_cli_http_failure_does_not_complete_attempt() {
    run_binary_cli_lifecycle(true).await;
}

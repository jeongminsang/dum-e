pub mod bench;
pub mod coordinator;

use anyhow::{Context, Result};
use dume_core::types::*;
use dume_core::verify::{verify_allowed_paths, verify_changed_paths};
use dume_store::HarnessStore;
use dume_worker::WorkerExecutor;
use std::path::Path;
use tokio_util::sync::CancellationToken;

pub async fn verify_and_integrate_attempt(
    store: &HarnessStore,
    repo_path: &str,
    attempt: &Attempt,
    lock: &CoordinatorLock,
    cancellation: CancellationToken,
) -> Result<()> {
    store.check_coordinator_fence(lock)?;
    anyhow::ensure!(!cancellation.is_cancelled(), "Coordinator cancelled");
    let epoch = lock.epoch;
    let task = store.get_task(&attempt.task_id)?;
    let candidate_commit = attempt
        .candidate_commit
        .as_ref()
        .context("Attempt missing candidate commit")?;

    tracing::info!(
        "Verifying candidate commit {} for task {}",
        candidate_commit,
        task.id
    );

    let repo_p = Path::new(repo_path);
    let verify_wt_dir = repo_p.join(format!(".dume/rust/verify-wt-{}-{}", attempt.id, epoch));

    // Create an isolated worktree checked out directly to candidate_commit to verify immutability
    dume_git::worktree::create_git_worktree(repo_p, &verify_wt_dir, candidate_commit).await?;

    // 1. Independent acceptance verification:
    // Run acceptance criteria command strictly in the isolated candidate worktree
    let (acceptance_passed, acceptance_details) = if task.acceptance_criteria.is_empty() {
        (false, "No acceptance criteria defined for task".to_string())
    } else {
        let mut passed = true;
        let mut detail = "Acceptance criteria passed".to_string();
        for cmd in &task.acceptance_criteria {
            store.check_coordinator_fence(lock)?;
            let test_res = WorkerExecutor::run_test_command_with_cancellation(
                &verify_wt_dir,
                cmd,
                cancellation.clone(),
            )
            .await?;
            if !test_res.passed {
                passed = false;
                detail = format!(
                    "Acceptance command '{}' failed with exit code {}",
                    cmd, test_res.exit_code
                );
                tracing::warn!("{}", detail);
                break;
            }
        }
        (passed, detail)
    };

    // Clean up verification worktree immediately
    let _ = dume_git::worktree::remove_git_worktree(repo_p, &verify_wt_dir).await;

    // 2. Verify allowed paths whitelist
    let (allowed_paths_passed, manifest_hash_str) = match &attempt.manifest_hash {
        Some(h) => match store.artifacts.get_artifact(h) {
            Ok(bytes) => {
                match serde_json::from_slice::<dume_core::manifest::ResultManifest>(&bytes) {
                    Ok(mut manifest) => {
                        let identity_matches = manifest.attempt_id == attempt.id
                            && manifest.candidate_commit == *candidate_commit;
                        manifest.modified_files =
                            dume_git::diff::get_commit_modified_files(repo_p, candidate_commit)
                                .await?;
                        let ok = identity_matches && verify_allowed_paths(&task, &manifest);
                        (ok, h.clone())
                    }
                    Err(_) => (false, h.clone()),
                }
            }
            Err(_) => (false, h.clone()),
        },
        None => (false, "".to_string()),
    };

    let overall_passed = acceptance_passed && allowed_paths_passed;

    let verification = Verification {
        attempt_id: attempt.id.clone(),
        epoch,
        passed: overall_passed,
        allowed_paths_passed,
        acceptance_command_passed: acceptance_passed,
        manifest_hash: manifest_hash_str,
        details: if overall_passed {
            "All acceptance tests and path constraints passed".to_string()
        } else {
            format!("Verification criteria failed: {}", acceptance_details)
        },
        verified_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64,
    };
    store.record_verification_fenced(lock, &verification)?;

    if !overall_passed {
        store.update_attempt_task_status_fenced(
            lock,
            &attempt.id,
            &task.id,
            AttemptStatus::Rejected,
            TaskStatus::Failed,
        )?;
        tracing::warn!("Attempt {} rejected by verification gate", attempt.id);
        return Ok(());
    }

    // 3. Serialized Two-Phase Cherry-pick integration (R5 crash-safe transaction)
    let integration_wt = repo_p.join(format!(
        ".dume/rust/integration-wt-{}-{}",
        attempt.id, epoch
    ));

    // Phase 0: Check if already integrated in DB or Git ancestry
    if let Ok(Some(existing_int)) = store.get_integration(&task.target_branch, candidate_commit) {
        if existing_int.status == IntegrationStatus::Applied {
            store.update_attempt_task_status_fenced(
                lock,
                &attempt.id,
                &task.id,
                AttemptStatus::Accepted,
                TaskStatus::Completed,
            )?;
            tracing::info!(
                "Candidate commit {} already applied on {}",
                candidate_commit,
                task.target_branch
            );
            return Ok(());
        }
        if let Some(int_commit) = &existing_int.integration_commit {
            if let Ok(current_ref) =
                dume_git::integrate::resolve_ref(repo_p, &task.target_branch).await
            {
                if current_ref == *int_commit
                    || dume_git::integrate::is_ancestor(repo_p, int_commit, &current_ref)
                        .await
                        .unwrap_or(false)
                {
                    let mut applied = existing_int.clone();
                    applied.status = IntegrationStatus::Applied;
                    store.record_integration_fenced(lock, &applied)?;
                    store.update_attempt_task_status_fenced(
                        lock,
                        &attempt.id,
                        &task.id,
                        AttemptStatus::Accepted,
                        TaskStatus::Completed,
                    )?;
                    tracing::info!(
                        "Candidate commit {} already integrated in ancestry of {}",
                        candidate_commit,
                        task.target_branch
                    );
                    return Ok(());
                }
            }
        }
    }

    // Phase 1: Prepare cherry-pick in isolated worktree to generate integration commit
    store.check_coordinator_fence(lock)?;
    anyhow::ensure!(!cancellation.is_cancelled(), "Coordinator cancelled");
    let prep_res = dume_git::integrate::prepare_candidate_cherry_pick(
        repo_p,
        &task.target_branch,
        candidate_commit,
        &integration_wt,
    )
    .await?;

    match prep_res {
        dume_git::integrate::IntegrationResult::Success {
            base_commit,
            integration_commit,
        } => {
            let integrated_paths =
                dume_git::diff::get_commit_modified_files(repo_p, &integration_commit).await?;
            let integrated_paths_passed = verify_changed_paths(&task, &integrated_paths);
            // Candidate validation does not prove that the combined target tree works.
            let merged_wt = repo_p.join(format!(
                ".dume/rust/merged-verify-wt-{}-{}",
                attempt.id, epoch
            ));
            dume_git::create_git_worktree(repo_p, &merged_wt, &integration_commit).await?;
            let merged_validation = if integrated_paths_passed {
                validate_commit(
                    store,
                    lock,
                    &merged_wt,
                    &integration_commit,
                    &task.acceptance_criteria,
                    cancellation.clone(),
                )
                .await
            } else {
                Ok((false, "Integrated patch violates allowed paths".to_owned()))
            };
            let _ = dume_git::remove_git_worktree(repo_p, &merged_wt).await;
            let (passed, details) = merged_validation?;
            let merged_verification = Verification {
                passed,
                allowed_paths_passed: integrated_paths_passed,
                acceptance_command_passed: passed,
                details,
                ..verification
            };
            store.record_verification_fenced(lock, &merged_verification)?;
            if !passed {
                store.update_attempt_task_status_fenced(
                    lock,
                    &attempt.id,
                    &task.id,
                    AttemptStatus::Rejected,
                    TaskStatus::Failed,
                )?;
                return Ok(());
            }
            // Record Pending intent in DB BEFORE updating Git branch ref
            let pending_record = Integration {
                target_branch: task.target_branch.clone(),
                base_commit: base_commit.clone(),
                candidate_commit: candidate_commit.clone(),
                integration_commit: Some(integration_commit.clone()),
                status: IntegrationStatus::Pending,
                error_message: None,
                integrated_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            };
            store.record_integration_fenced(lock, &pending_record)?;

            // Atomic Git ref update with CAS (fails if base_commit moved concurrently)
            anyhow::ensure!(!cancellation.is_cancelled(), "Coordinator cancelled");
            apply_integration_fenced(store, repo_p, &pending_record, lock)?;

            // Exact failure injection hook: test asks coordinator to terminate immediately after git ref updated but before Applied DB write
            if std::env::var("DUME_TEST_CRASH_AFTER_GIT_UPDATE")
                .map(|v| v == "1")
                .unwrap_or(false)
            {
                // Signal to parent process via stdout exactly at the boundary
                println!("DUME_BOUNDARY_GIT_UPDATED_BEFORE_DB_APPLIED");
                let _ = std::io::Write::flush(&mut std::io::stdout());
                // Immediate ungraceful exit (simulating SIGKILL crash)
                std::process::exit(137);
            }

            // Transition DB record to Applied
            let applied_record = Integration {
                status: IntegrationStatus::Applied,
                ..pending_record
            };
            store.record_integration_fenced(lock, &applied_record)?;
            store.update_attempt_task_status_fenced(
                lock,
                &attempt.id,
                &task.id,
                AttemptStatus::Accepted,
                TaskStatus::Completed,
            )?;
            tracing::info!(
                "Successfully integrated commit {} as {} into {}",
                candidate_commit,
                integration_commit,
                task.target_branch
            );
        }
        dume_git::integrate::IntegrationResult::Conflict {
            base_commit,
            details,
        } => {
            let record = Integration {
                target_branch: task.target_branch.clone(),
                base_commit,
                candidate_commit: candidate_commit.clone(),
                integration_commit: None,
                status: IntegrationStatus::Conflict,
                error_message: Some(details.clone()),
                integrated_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            };
            store.record_integration_fenced(lock, &record)?;
            store.update_attempt_task_status_fenced(
                lock,
                &attempt.id,
                &task.id,
                AttemptStatus::NeedsAttention,
                TaskStatus::NeedsAttention,
            )?;
            tracing::error!(
                "Cherry-pick integration conflict for attempt {}: {}",
                attempt.id,
                details
            );
        }
        dume_git::integrate::IntegrationResult::Failed { error } => {
            let record = Integration {
                target_branch: task.target_branch.clone(),
                base_commit: "".to_string(),
                candidate_commit: candidate_commit.clone(),
                integration_commit: None,
                status: IntegrationStatus::Failed,
                error_message: Some(error.clone()),
                integrated_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            };
            store.record_integration_fenced(lock, &record)?;
            store.update_attempt_task_status_fenced(
                lock,
                &attempt.id,
                &task.id,
                AttemptStatus::Rejected,
                TaskStatus::Failed,
            )?;
            tracing::error!("Integration failed for attempt {}: {}", attempt.id, error);
        }
    }

    Ok(())
}

pub fn apply_integration_fenced(
    store: &HarnessStore,
    repo: &Path,
    integration: &Integration,
    lock: &CoordinatorLock,
) -> Result<()> {
    let commit = integration
        .integration_commit
        .as_deref()
        .context("Integration missing commit")?;
    store.with_coordinator_fence(lock, |_| {
        dume_git::integrate::apply_branch_update_sync(
            repo,
            &integration.target_branch,
            commit,
            &integration.base_commit,
        )
    })
}

async fn validate_commit(
    store: &HarnessStore,
    lock: &CoordinatorLock,
    worktree: &Path,
    commit: &str,
    commands: &[String],
    cancellation: CancellationToken,
) -> Result<(bool, String)> {
    for command in commands {
        store.check_coordinator_fence(lock)?;
        let result = WorkerExecutor::run_test_command_with_cancellation(
            worktree,
            command,
            cancellation.clone(),
        )
        .await?;
        if !result.passed {
            return Ok((
                false,
                format!(
                    "Integration commit {commit}: '{command}' failed with exit code {}",
                    result.exit_code
                ),
            ));
        }
    }
    let head = tokio::process::Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["rev-parse", "HEAD"])
        .output()
        .await?;
    let clean = tokio::process::Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["diff", "--quiet", "HEAD", "--"])
        .status()
        .await?;
    let unchanged = head.status.success()
        && String::from_utf8_lossy(&head.stdout).trim() == commit
        && clean.success();
    Ok((
        unchanged,
        if unchanged {
            format!("Integration commit {commit} passed all acceptance checks")
        } else {
            "Acceptance checks modified the validated tree".to_owned()
        },
    ))
}

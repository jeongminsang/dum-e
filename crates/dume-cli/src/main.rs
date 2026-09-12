use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use dume_core::types::*;
use dume_core::verify::verify_allowed_paths;
use dume_git::integrate::IntegrationResult;
use dume_store::HarnessStore;
use dume_worker::executor::WorkerExecutor;
use dume_worker::ipc::{serialize_message, WorkerToHostMessage};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(name = "dume", about = "DUM-E Clean-Engine Autonomous Multi-Agent Harness (Rust)")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start autonomous coordinator with state recovery
    Coordinator {
        #[arg(long, default_value = ".dume/rust/harness.db")]
        db_path: String,
        #[arg(long, default_value = ".dume/rust/artifacts")]
        artifacts_dir: String,
        #[arg(long, default_value = ".")]
        repo_path: String,
    },
    /// Run worker attempt process in worktree
    Worker {
        #[arg(long)]
        attempt_id: String,
        #[arg(long)]
        worktree_path: String,
        #[arg(long)]
        test_command: Option<String>,
    },
    /// Show current harness status
    Status {
        #[arg(long, default_value = ".dume/rust/harness.db")]
        db_path: String,
        #[arg(long, default_value = ".dume/rust/artifacts")]
        artifacts_dir: String,
    },
    /// Launch interactive TUI terminal interface
    Interactive {
        #[arg(long, default_value = "claude-3-5-sonnet")]
        model: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Coordinator { db_path, artifacts_dir, repo_path }) => {
            tracing_subscriber::fmt::init();
            run_coordinator(&db_path, &artifacts_dir, &repo_path).await?;
        }
        Some(Commands::Worker { attempt_id, worktree_path, test_command }) => {
            run_worker_child(&attempt_id, &worktree_path, test_command.as_deref()).await?;
        }
        Some(Commands::Status { db_path, artifacts_dir }) => {
            print_status(&db_path, &artifacts_dir)?;
        }
        Some(Commands::Interactive { model }) => {
            dume_tui::run_tui(&model).await?;
        }
        None => {
            // Default to interactive TUI
            dume_tui::run_tui("claude-3-5-sonnet").await?;
        }
    }

    Ok(())
}


async fn run_worker_child(
    attempt_id: &str,
    worktree_path: &str,
    test_command: Option<&str>,
) -> Result<()> {
    let path = Path::new(worktree_path);

    // Send initial progress
    let progress_msg = WorkerToHostMessage::Progress {
        attempt_id: attempt_id.to_string(),
        message: format!("Worker starting in worktree {}", worktree_path),
    };
    print!("{}", serialize_message(&progress_msg)?);

    // 1. Run test command if provided
    let mut test_results = Vec::new();
    if let Some(cmd) = test_command {
        let result = WorkerExecutor::run_test_command(path, cmd).await?;
        test_results.push(result);
    }

    // 2. Finalize manifest (detect modified files, commit changes)
    let manifest = WorkerExecutor::finalize_manifest(
        attempt_id,
        path,
        test_results,
        "Worker completed attempt",
    )
    .await?;

    // Send completed message to host
    let completed_msg = WorkerToHostMessage::Completed { manifest };
    print!("{}", serialize_message(&completed_msg)?);

    Ok(())
}

async fn run_coordinator(
    db_path: &str,
    artifacts_dir: &str,
    repo_path: &str,
) -> Result<()> {
    let store = Arc::new(HarnessStore::open(db_path, artifacts_dir)?);
    let coordinator_id = "default_coordinator";
    let owner_id = format!("pid_{}", std::process::id());
    let ttl_ms = 15_000;

    // 1. Acquire coordinator lock with monotonic epoch fencing
    let lock = store.acquire_coordinator_lock(coordinator_id, &owner_id, ttl_ms)
        .context("Failed to acquire coordinator lock")?;
    let epoch = lock.epoch;
    tracing::info!("Acquired coordinator lock with epoch {}", epoch);

    // 2. Crash recovery: inspect previous state
    let (ready_for_verify, needs_attention, unknown_ops) = store.recover_state(epoch)?;
    tracing::info!(
        "Crash recovery complete: {} submitted attempts ready for verification, {} attempts needing attention, {} external operations with unknown outcome",
        ready_for_verify.len(),
        needs_attention.len(),
        unknown_ops.len()
    );

    // 3. Start background heartbeat task
    let running = Arc::new(AtomicBool::new(true));
    let store_hb = Arc::clone(&store);
    let owner_id_hb = owner_id.clone();
    let running_hb = Arc::clone(&running);
    tokio::spawn(async move {
        while running_hb.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(5_000)).await;
            if let Err(e) = store_hb.heartbeat_coordinator(coordinator_id, &owner_id_hb, ttl_ms) {
                tracing::warn!("Coordinator heartbeat error: {}", e);
                break;
            }
        }
    });

    // 4. Recover and verify any pending results
    for attempt in ready_for_verify {
        verify_and_integrate_attempt(&store, repo_path, &attempt, epoch).await?;
    }

    // Stop coordinator gracefully
    running.store(false, Ordering::Relaxed);
    store.release_coordinator_lock(coordinator_id, &owner_id)?;
    tracing::info!("Coordinator released lock successfully");

    Ok(())
}

pub async fn verify_and_integrate_attempt(
    store: &HarnessStore,
    repo_path: &str,
    attempt: &Attempt,
    epoch: i64,
) -> Result<()> {
    let task = store.get_task(&attempt.task_id)?;
    let candidate_commit = attempt.candidate_commit.as_ref().context("Attempt missing candidate commit")?;

    tracing::info!("Verifying candidate commit {} for task {}", candidate_commit, task.id);

    // 1. Independent acceptance verification:
    // Run acceptance criteria command directly in candidate worktree or candidate commit
    let mut acceptance_passed = true;
    for cmd in &task.acceptance_criteria {
        let wt = Path::new(&attempt.worktree_path);
        let test_res = WorkerExecutor::run_test_command(wt, cmd).await?;
        if !test_res.passed {
            acceptance_passed = false;
            tracing::warn!("Acceptance command '{}' failed with exit code {}", cmd, test_res.exit_code);
            break;
        }
    }

    // 2. Verify allowed paths whitelist
    let manifest_bytes = if let Some(ref h) = attempt.manifest_hash {
        store.artifacts.get_artifact(h).ok()
    } else {
        None
    };

    let allowed_paths_passed = if let Some(bytes) = manifest_bytes {
        if let Ok(manifest) = serde_json::from_slice::<dume_core::manifest::ResultManifest>(&bytes) {
            verify_allowed_paths(&task, &manifest)
        } else {
            true
        }
    } else {
        true
    };

    let overall_passed = acceptance_passed && allowed_paths_passed;

    let verification = Verification {
        attempt_id: attempt.id.clone(),
        epoch,
        passed: overall_passed,
        allowed_paths_passed,
        acceptance_command_passed: acceptance_passed,
        manifest_hash: attempt.manifest_hash.clone().unwrap_or_default(),
        details: if overall_passed {
            "All acceptance tests and path constraints passed".to_string()
        } else {
            "Verification criteria failed".to_string()
        },
        verified_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64,
    };
    store.record_verification(&verification)?;

    if !overall_passed {
        store.update_attempt_status(&attempt.id, AttemptStatus::Rejected)?;
        store.update_task_status(&task.id, TaskStatus::Failed)?;
        tracing::warn!("Attempt {} rejected by verification gate", attempt.id);
        return Ok(());
    }

    // 3. Serialized Cherry-pick integration
    let repo_p = Path::new(repo_path);
    let integration_wt = repo_p.join(".dume/rust/integration-worktree");

    let int_res = dume_git::integrate::integrate_candidate_commit(
        repo_p,
        &task.target_branch,
        candidate_commit,
        &integration_wt,
    )
    .await?;

    match int_res {
        IntegrationResult::Success { base_commit, integration_commit } => {
            let record = Integration {
                target_branch: task.target_branch.clone(),
                base_commit,
                candidate_commit: candidate_commit.clone(),
                integration_commit: Some(integration_commit.clone()),
                status: IntegrationStatus::Applied,
                error_message: None,
                integrated_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            };
            store.record_integration(&record)?;
            store.update_attempt_status(&attempt.id, AttemptStatus::Accepted)?;
            store.update_task_status(&task.id, TaskStatus::Completed)?;
            tracing::info!(
                "Successfully integrated commit {} as {} into {}",
                candidate_commit,
                integration_commit,
                task.target_branch
            );
        }
        IntegrationResult::Conflict { base_commit, details } => {
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
            store.record_integration(&record)?;
            store.update_attempt_status(&attempt.id, AttemptStatus::NeedsAttention)?;
            store.update_task_status(&task.id, TaskStatus::NeedsAttention)?;
            tracing::error!("Cherry-pick integration conflict for attempt {}: {}", attempt.id, details);
        }
        IntegrationResult::Failed { error } => {
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
            store.record_integration(&record)?;
            store.update_attempt_status(&attempt.id, AttemptStatus::Rejected)?;
            store.update_task_status(&task.id, TaskStatus::Failed)?;
            tracing::error!("Integration failed for attempt {}: {}", attempt.id, error);
        }
    }

    Ok(())
}

fn print_status(db_path: &str, artifacts_dir: &str) -> Result<()> {
    let store = HarnessStore::open(db_path, artifacts_dir)?;
    println!("=== DUM-E Harness Status (Rust) ===");
    println!("Database: {}", store.db_path.display());
    Ok(())
}

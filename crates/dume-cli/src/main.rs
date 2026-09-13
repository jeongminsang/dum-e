use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use dume_core::types::*;
use dume_store::HarnessStore;
use dume_worker::executor::WorkerExecutor;
use dume_worker::ipc::{WorkerToHostMessage, serialize_message};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(version)]
#[command(
    name = "dume",
    about = "DUM-E Clean-Engine Autonomous Multi-Agent Harness (Rust)"
)]
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
        #[arg(long)]
        task_prompt: Option<String>,
        #[arg(long, default_value = "anthropic/claude-sonnet-4-5")]
        model: String,
        /// Override the selected provider's API root (except Google); sends its resolved credentials to this URL
        #[arg(long, requires = "task_prompt")]
        base_url: Option<String>,
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
        #[arg(long, default_value = "anthropic/claude-sonnet-4-5")]
        model: String,
    },
    /// Create a new goal
    Goal {
        #[arg(long)]
        id: String,
        #[arg(long)]
        description: String,
        #[arg(long, default_value = ".dume/rust/harness.db")]
        db_path: String,
        #[arg(long, default_value = ".dume/rust/artifacts")]
        artifacts_dir: String,
    },
    /// Create a new task under a goal
    Task {
        #[arg(long)]
        id: String,
        #[arg(long)]
        goal_id: String,
        #[arg(long)]
        title: String,
        #[arg(long)]
        description: String,
        #[arg(long)]
        test_command: Option<String>,
        #[arg(long, default_value = "main")]
        target_branch: String,
        #[arg(long, default_value = ".dume/rust/harness.db")]
        db_path: String,
        #[arg(long, default_value = ".dume/rust/artifacts")]
        artifacts_dir: String,
    },
    /// List available models across Anthropic, OpenAI, and Google
    Models {
        #[arg(long)]
        provider: Option<String>,
    },
    /// Login via browser OAuth flow or API key
    Login {
        #[arg(default_value = "anthropic")]
        provider: String,
        #[arg(long, conflicts_with_all = ["device", "manual"])]
        api_key: bool,
        #[arg(long, conflicts_with = "manual")]
        device: bool,
        #[arg(long)]
        manual: bool,
    },
    /// Logout and clear stored credentials
    Logout {
        #[arg(default_value = "anthropic")]
        provider: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Coordinator {
            db_path,
            artifacts_dir,
            repo_path,
        }) => {
            tracing_subscriber::fmt::init();
            run_coordinator(&db_path, &artifacts_dir, &repo_path).await?;
        }
        Some(Commands::Worker {
            attempt_id,
            worktree_path,
            test_command,
            task_prompt,
            model,
            base_url,
        }) => {
            run_worker_child(
                &attempt_id,
                &worktree_path,
                test_command.as_deref(),
                task_prompt.as_deref(),
                &model,
                base_url.as_deref(),
            )
            .await?;
        }
        Some(Commands::Status {
            db_path,
            artifacts_dir,
        }) => {
            print_status(&db_path, &artifacts_dir)?;
        }
        Some(Commands::Interactive { model }) => {
            dume_tui::run_tui(&model).await?;
        }
        Some(Commands::Goal {
            id,
            description,
            db_path,
            artifacts_dir,
        }) => {
            let store = HarnessStore::open(&db_path, &artifacts_dir)?;
            let goal = store.create_goal(&id, &description)?;
            println!("Created goal '{}': {}", goal.id, goal.description);
        }
        Some(Commands::Task {
            id,
            goal_id,
            title,
            description,
            test_command,
            target_branch,
            db_path,
            artifacts_dir,
        }) => {
            let store = HarnessStore::open(&db_path, &artifacts_dir)?;
            let task = Task {
                id: id.clone(),
                goal_id: goal_id.clone(),
                title: title.clone(),
                description,
                status: TaskStatus::Ready,
                dependencies: vec![],
                acceptance_criteria: test_command.into_iter().collect(),
                allowed_paths: None,
                target_branch,
                created_at: 0,
                updated_at: 0,
            };
            store.create_task(&task)?;
            println!("Created task '{}' under goal '{}'", task.id, task.goal_id);
        }
        Some(Commands::Models { provider }) => {
            let all = dume_provider::ModelCatalog::list_all_builtin_models()?;
            println!(
                "{:<30} {:<12} {:<10} {:<12} {}",
                "MODEL ID", "PROVIDER", "REASONING", "MAX TOKENS", "NAME"
            );
            println!("{}", "-".repeat(80));
            for m in all {
                if let Some(ref p) = provider {
                    if !m
                        .provider
                        .eq_ignore_ascii_case(dume_provider::normalize_provider(p))
                    {
                        continue;
                    }
                }
                let max_tok = m
                    .max_tokens
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "-".to_string());
                println!(
                    "{:<30} {:<12} {:<10} {:<12} {}",
                    m.id, m.provider, m.reasoning, max_tok, m.name
                );
            }
        }
        Some(Commands::Login {
            provider,
            api_key,
            device,
            manual,
        }) => {
            tokio::select! {
                result = run_login(&provider, api_key, device, manual) => result?,
                _ = tokio::signal::ctrl_c() => anyhow::bail!("Login cancelled"),
            }
        }
        Some(Commands::Logout { provider }) => {
            run_logout(&provider)?;
        }
        None => {
            // Default to interactive TUI
            dume_tui::run_tui("anthropic/claude-sonnet-4-5").await?;
        }
    }

    Ok(())
}

async fn read_login_input() -> Result<String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    // A detached thread does not prevent runtime shutdown when stdin is cancelled.
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = std::io::stdin().read_line(&mut line).map(|_| line);
        let _ = tx.send(result);
    });
    Ok(tokio::time::timeout(Duration::from_secs(300), rx)
        .await
        .context("Login input timed out")???)
}

fn login_provider(provider: &str) -> Result<&str> {
    let provider = dume_provider::normalize_provider(provider);
    anyhow::ensure!(
        matches!(provider, "anthropic" | "openai" | "openai-codex" | "google"),
        "Unsupported login provider"
    );
    Ok(provider)
}

async fn run_login(provider: &str, api_key: bool, device: bool, manual: bool) -> Result<()> {
    use dume_provider::oauth;
    let provider = login_provider(provider)?;
    anyhow::ensure!(
        !device || provider == "openai-codex",
        "Device login is only available for openai-codex"
    );
    anyhow::ensure!(
        !api_key || provider != "openai-codex",
        "Codex requires OAuth login"
    );
    let store = dume_provider::CredentialStore::new(dume_provider::CredentialStore::default_path());
    if api_key || matches!(provider, "openai" | "google") {
        anyhow::ensure!(
            !manual,
            "Manual OAuth login is unavailable for API-key providers"
        );
        println!("Enter API key for {provider} (input is visible):");
        let key = read_login_input().await?;
        store.save_credential(provider, key.trim())?;
    } else {
        let token = if device {
            oauth::login_codex_device(|url, code| println!("Open {url} and enter code: {code}"))
                .await?
        } else {
            let config =
                oauth::get_oauth_config(provider).context("OAuth unavailable for this provider")?;
            let pkce = oauth::generate_pkce()?;
            let state = if provider == "anthropic" {
                pkce.verifier.clone()
            } else {
                oauth::generate_state()?
            };
            let listener = if manual {
                None
            } else {
                Some(oauth::bind_oauth_callback(config.port).await?)
            };
            println!(
                "Open this URL in your browser:\n{}",
                oauth::build_authorization_url(&config, &state, &pkce.challenge)?
            );
            let code = if let Some(listener) = listener {
                oauth::wait_for_oauth_callback(listener, config.callback_path, &state).await?
            } else {
                println!("Paste the authorization code or final redirect URL:");
                oauth::parse_authorization_input(&read_login_input().await?, &state)?
            };
            oauth::exchange_code_for_token(
                &config,
                &code,
                &config.redirect_uri(),
                &pkce.verifier,
                &state,
            )
            .await?
        };
        let credential = dume_provider::Credential::from_oauth(provider, token, None)?;
        store.save(provider, &credential)?;
    }
    println!("Saved credentials for '{provider}'.");
    Ok(())
}

fn run_logout(provider: &str) -> Result<()> {
    let provider = login_provider(provider)?;
    let cred_store =
        dume_provider::CredentialStore::new(dume_provider::CredentialStore::default_path());
    cred_store.delete(provider)?;
    println!(
        "Successfully logged out and removed credentials for '{}'.",
        provider
    );
    Ok(())
}

async fn run_worker_child(
    attempt_id: &str,
    worktree_path: &str,
    test_command: Option<&str>,
    task_prompt: Option<&str>,
    model: &str,
    base_url: Option<&str>,
) -> Result<()> {
    let path = Path::new(worktree_path);

    // Send initial progress
    let progress_msg = WorkerToHostMessage::Progress {
        attempt_id: attempt_id.to_string(),
        message: format!("Worker starting in worktree {}", worktree_path),
    };
    print!("{}", serialize_message(&progress_msg)?);

    // 1. Run actual Agent Loop if task prompt is present
    if let Some(prompt) = task_prompt {
        let mut agent = dume_worker::AgentLoop::new(path, model);
        if let Some(base_url) = base_url {
            agent = agent.with_base_url(base_url);
        }
        let progress_exec = WorkerToHostMessage::Progress {
            attempt_id: attempt_id.to_string(),
            message: format!("Agent loop executing prompt: {}", prompt),
        };
        print!("{}", serialize_message(&progress_exec)?);
        let _ = agent.run_task(prompt).await?;
    }

    // 2. Run test command if provided
    let mut test_results = Vec::new();
    if let Some(cmd) = test_command {
        let result = WorkerExecutor::run_test_command(path, cmd).await?;
        test_results.push(result);
    }

    // 3. Finalize manifest (detect modified files, commit changes)
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

async fn run_coordinator(db_path: &str, artifacts_dir: &str, repo_path: &str) -> Result<()> {
    let store = Arc::new(HarnessStore::open(db_path, artifacts_dir)?);
    let coordinator_id = "default_coordinator";
    let owner_id = format!("pid_{}", std::process::id());
    let ttl_ms = 15_000;

    // 1. Acquire coordinator lock with monotonic epoch fencing
    let lock = store
        .acquire_coordinator_lock(coordinator_id, &owner_id, ttl_ms)
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

    // 4. Reconcile any in-flight pending integrations from prior crashes (R5 crash-safe transaction)
    let repo_p = Path::new(repo_path);
    let pending_integrations = store.list_pending_integrations()?;
    for pending in pending_integrations {
        if let Some(target_commit) = &pending.integration_commit {
            // Check if Git target branch was already advanced to the integration commit before crash
            if let Ok(current_ref) =
                dume_git::integrate::resolve_ref(repo_p, &pending.target_branch).await
            {
                let is_already_integrated = current_ref == *target_commit
                    || dume_git::integrate::is_ancestor(repo_p, target_commit, &current_ref)
                        .await
                        .unwrap_or(false);

                if is_already_integrated {
                    // Ref was already updated or further advanced by subsequent normal commits: transition DB status to Applied cleanly
                    let mut applied = pending.clone();
                    applied.status = IntegrationStatus::Applied;
                    store.record_integration(&applied)?;
                    tracing::info!(
                        "Reconciled pre-crash pending integration for branch {} (ref: {}) -> Applied",
                        pending.target_branch,
                        current_ref
                    );
                } else if current_ref == pending.base_commit {
                    // Ref was not updated: attempt atomic update-ref with CAS
                    match dume_git::integrate::apply_branch_update(
                        repo_p,
                        &pending.target_branch,
                        target_commit,
                        &pending.base_commit,
                    )
                    .await
                    {
                        Ok(()) => {
                            let mut applied = pending.clone();
                            applied.status = IntegrationStatus::Applied;
                            store.record_integration(&applied)?;
                            tracing::info!(
                                "Completed pre-crash pending integration for branch {} -> Applied",
                                pending.target_branch
                            );
                        }
                        Err(cas_err) => {
                            tracing::warn!(
                                "Pre-crash pending integration CAS update-ref failed for branch {}: {}",
                                pending.target_branch,
                                cas_err
                            );
                        }
                    }
                } else {
                    tracing::warn!(
                        "Target branch {} moved unexpectedly (current: {}, expected base: {}, candidate: {:?}); CAS update-ref rejected",
                        pending.target_branch,
                        current_ref,
                        pending.base_commit,
                        pending.integration_commit
                    );
                }
            }
        }
    }

    // 5. Recover and verify any pending results from prior crashes
    for attempt in ready_for_verify {
        verify_and_integrate_attempt(&store, repo_path, &attempt, epoch).await?;
    }

    // 5. Continuous Coordinator Dispatch Loop:
    // Polls active goals, evaluates TaskDag for ready tasks, spawns workers, and verifies results
    let worker_host = dume_worker::WorkerHost::new(
        std::env::current_exe()?
            .to_str()
            .context("Failed to get current executable path")?,
    );

    let repo_p = Path::new(repo_path);
    let mut idle_iterations = 0;

    while running.load(Ordering::Relaxed) {
        let active_goals = store.list_active_goals()?;
        if active_goals.is_empty() {
            idle_iterations += 1;
            if idle_iterations >= 3 {
                tracing::info!("No active goals pending in harness. Coordinator loop exiting.");
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }

        idle_iterations = 0;
        let mut any_work_done = false;

        for goal in &active_goals {
            let tasks = store.list_tasks_for_goal(&goal.id)?;
            if tasks.is_empty() {
                continue;
            }

            let all_completed = tasks.iter().all(|t| t.status == TaskStatus::Completed);
            if all_completed {
                store.update_goal_status(&goal.id, GoalStatus::Completed)?;
                tracing::info!("Goal {} all tasks completed successfully", goal.id);
                continue;
            }

            let any_failed = tasks
                .iter()
                .any(|t| t.status == TaskStatus::Failed || t.status == TaskStatus::NeedsAttention);
            if any_failed {
                store.update_goal_status(&goal.id, GoalStatus::Failed)?;
                tracing::warn!("Goal {} has failed/blocked tasks", goal.id);
                continue;
            }

            let dag = match dume_core::dag::TaskDag::new(&tasks) {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!("DAG error for goal {}: {}", goal.id, e);
                    store.update_goal_status(&goal.id, GoalStatus::Failed)?;
                    continue;
                }
            };

            let ready_tasks = dag.get_ready_tasks();
            for task in ready_tasks {
                any_work_done = true;
                let attempt_id = format!(
                    "att_{}_{}",
                    task.id,
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_millis()
                );
                let worktree_dir = repo_p.join(format!(".dume/rust/worktrees/{}", attempt_id));

                // 1. Create isolated worktree for worker
                dume_git::worktree::create_git_worktree(repo_p, &worktree_dir, &task.target_branch)
                    .await?;

                // 2. Record attempt in store under current coordinator epoch
                let attempt = store.create_attempt(
                    &attempt_id,
                    &task.id,
                    epoch,
                    &owner_id,
                    worktree_dir.to_str().unwrap(),
                    30_000,
                )?;

                tracing::info!(
                    "Dispatched task {} (attempt {}) to worker",
                    task.id,
                    attempt.id
                );

                // 3. Spawn separate OS worker process
                let cancel_token = tokio_util::sync::CancellationToken::new();
                let manifest_res = worker_host
                    .run_attempt(
                        &attempt.id,
                        &task.id,
                        epoch,
                        &worktree_dir,
                        task.acceptance_criteria.first().cloned(),
                        Some(task.description.clone()),
                        Some("anthropic/claude-sonnet-4-5".to_string()),
                        cancel_token,
                    )
                    .await;

                match manifest_res {
                    Ok(manifest) => {
                        // Save manifest artifact
                        let manifest_json = serde_json::to_string_pretty(&manifest)?;
                        let manifest_hash =
                            store.artifacts.save_artifact(manifest_json.as_bytes())?;

                        // Submit attempt result under epoch guard
                        store.submit_attempt_result(
                            &attempt.id,
                            epoch,
                            &manifest.candidate_commit,
                            &manifest_hash,
                        )?;

                        let updated_attempt = store.get_attempt(&attempt.id)?;
                        verify_and_integrate_attempt(&store, repo_path, &updated_attempt, epoch)
                            .await?;
                    }
                    Err(e) => {
                        tracing::error!("Worker failed for attempt {}: {}", attempt.id, e);
                        store.update_attempt_status(&attempt.id, AttemptStatus::Rejected)?;
                        store.update_task_status(&task.id, TaskStatus::Failed)?;
                    }
                }

                // Clean up worker worktree
                let _ = dume_git::worktree::remove_git_worktree(repo_p, &worktree_dir).await;
            }
        }

        if !any_work_done {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    // Stop coordinator gracefully
    running.store(false, Ordering::Relaxed);
    store.release_coordinator_lock(coordinator_id, &owner_id)?;
    tracing::info!("Coordinator released lock successfully");

    Ok(())
}

pub use dume_cli::verify_and_integrate_attempt;

#[cfg(test)]
mod login_tests {
    use super::*;

    #[test]
    fn login_modes_and_version_parse() {
        assert!(matches!(
            Cli::try_parse_from(["dume", "login", "openai-codex", "--device"])
                .unwrap()
                .command,
            Some(Commands::Login { device: true, .. })
        ));
        assert!(matches!(
            Cli::try_parse_from(["dume", "login", "anthropic", "--manual"])
                .unwrap()
                .command,
            Some(Commands::Login { manual: true, .. })
        ));
        assert!(matches!(
            Cli::try_parse_from(["dume", "login", "google", "--api-key"])
                .unwrap()
                .command,
            Some(Commands::Login { api_key: true, .. })
        ));
        assert!(
            Cli::try_parse_from(["dume", "login", "anthropic", "--api-key", "--device"]).is_err()
        );
        assert_eq!(
            Cli::try_parse_from(["dume", "--version"])
                .unwrap_err()
                .kind(),
            clap::error::ErrorKind::DisplayVersion
        );
        assert_eq!(login_provider("gemini").unwrap(), "google");
        assert!(login_provider("unsupported").is_err());
        assert!(matches!(
            Cli::try_parse_from(["dume", "models"]).unwrap().command,
            Some(Commands::Models { .. })
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "dume",
                "worker",
                "--attempt-id",
                "a",
                "--worktree-path",
                ".",
                "--task-prompt",
                "work",
                "--model",
                "openai/gpt-5.4",
                "--base-url",
                "http://127.0.0.1:1234/v1",
            ])
            .unwrap()
            .command,
            Some(Commands::Worker {
                base_url: Some(_),
                ..
            })
        ));
        assert!(
            Cli::try_parse_from([
                "dume",
                "worker",
                "--attempt-id",
                "a",
                "--worktree-path",
                ".",
                "--base-url",
                "http://127.0.0.1:1234/v1",
            ])
            .is_err()
        );
    }
}

fn print_status(db_path: &str, artifacts_dir: &str) -> Result<()> {
    let store = HarnessStore::open(db_path, artifacts_dir)?;
    println!("=== DUM-E Harness Status (Rust) ===");
    println!("Database: {}", store.db_path.display());

    // Coordinator lock status
    if let Ok(Some(lock)) = store.get_coordinator_lock("default_coordinator") {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let active = lock.lease_expires_at > now;
        println!(
            "Coordinator: epoch={}, owner={}, active={}",
            lock.epoch,
            lock.owner_id.as_deref().unwrap_or("<none>"),
            active
        );
    } else {
        println!("Coordinator: idle / not initialized");
    }

    // Goals breakdown
    let goals = store.list_all_goals()?;
    println!("Goals: {} total", goals.len());
    for g in goals {
        let tasks = store.list_tasks_for_goal(&g.id)?;
        println!(
            "  - [{:?}] {} ({}): {} tasks",
            g.status,
            g.id,
            g.description,
            tasks.len()
        );
        for t in tasks {
            println!("      * [{:?}] {} - {}", t.status, t.id, t.title);
        }
    }

    Ok(())
}

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use dume_core::types::*;
use dume_store::HarnessStore;
use dume_worker::executor::WorkerExecutor;
use dume_worker::ipc::{WorkerToHostMessage, serialize_message};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(version)]
#[command(
    name = "dume",
    about = "DUM-E Clean-Engine Autonomous Multi-Agent Harness (Rust)"
)]
struct Cli {
    /// Resume the latest conversation in this directory
    #[arg(short = 'c', long = "continue")]
    continue_session: bool,

    /// Resume a conversation by ID or open interactive session picker
    #[arg(short = 'r', long = "resume", num_args = 0..=1, default_missing_value = "")]
    resume: Option<String>,

    /// Show/search conversations across all directories
    #[arg(long = "all")]
    all: bool,

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
        #[arg(long)]
        db_path: Option<String>,
        #[arg(long)]
        artifacts_dir: Option<String>,
        #[arg(long)]
        benchmark_run_id: Option<String>,
        #[arg(long)]
        case_id: Option<String>,
        #[arg(long)]
        variant: Option<String>,
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
        #[arg(long, default_value = "")]
        model: String,
        /// Resume the latest conversation in this directory
        #[arg(short = 'c', long = "continue")]
        continue_session: bool,
        /// Resume a conversation by ID or open interactive session picker
        #[arg(short = 'r', long = "resume", num_args = 0..=1, default_missing_value = "")]
        resume: Option<String>,
        /// Show/search conversations across all directories
        #[arg(long = "all")]
        all: bool,
    },
    /// Export a conversation to JSONL format
    Export {
        /// Conversation ID to export (defaults to latest in current directory)
        id: Option<String>,
        /// Output file path (defaults to stdout)
        #[arg(short = 'o', long = "output")]
        output: Option<std::path::PathBuf>,
        #[arg(long, default_value = ".dume/agent/harness.db")]
        db_path: String,
        #[arg(long, default_value = ".dume/artifacts")]
        artifacts_dir: String,
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
    /// List available models across supported providers
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
    /// Check for or install updates in-place
    Update {
        /// Only check for updates without downloading or installing
        #[arg(long)]
        check: bool,
    },
    /// Show session token usage statistics
    Usage {
        #[arg(long, default_value = ".dume/rust/harness.db")]
        db_path: String,
        #[arg(long, default_value = ".dume/rust/artifacts")]
        artifacts_dir: String,
        #[arg(long, default_value = "default")]
        session_id: String,
    },
    /// Run token-efficiency benchmarks
    Bench {
        #[arg(long, value_delimiter = ',')]
        cases: Vec<String>,
        #[arg(long)]
        baseline_bin: Option<std::path::PathBuf>,
        #[arg(long)]
        candidate_bin: Option<std::path::PathBuf>,
        #[arg(long, default_value = "1")]
        repetitions: usize,
        #[arg(long, default_value = "anthropic/claude-sonnet-4-5")]
        model: String,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long, default_value = ".dume/rust/benchmarks")]
        output_dir: std::path::PathBuf,
        #[arg(long, default_value = ".dume/rust/harness.db")]
        db_path: std::path::PathBuf,
    },
    /// Inspect and resolve durable agent runtime state
    Runtime {
        #[command(subcommand)]
        command: RuntimeCommands,
    },
    /// Manage configured MCP servers and explicit project trust
    Mcp {
        #[command(subcommand)]
        command: McpCommands,
    },
}

#[derive(Subcommand, Debug)]
enum RuntimeCommands {
    Queue {
        #[command(subcommand)]
        command: QueueCommands,
    },
    Inspect {
        run_id: String,
        #[arg(long, default_value = ".dume/rust/harness.db")]
        db_path: String,
        #[arg(long, default_value = ".dume/rust/artifacts")]
        artifacts_dir: String,
    },
    Resolve {
        execution_id: String,
        #[arg(long)]
        run_id: String,
        #[arg(long)]
        revision: i64,
        #[arg(long)]
        operator: String,
        #[arg(long, value_parser = ["observed_succeeded", "observed_not_run", "retry_authorized"])]
        decision: String,
        #[arg(long)]
        evidence: String,
        #[arg(long)]
        result: Option<String>,
        #[arg(long, requires = "decision")]
        acknowledge_duplicate_effect: bool,
        #[arg(long, default_value = ".dume/rust/harness.db")]
        db_path: String,
        #[arg(long, default_value = ".dume/rust/artifacts")]
        artifacts_dir: String,
    },
}

#[derive(Subcommand, Debug)]
enum QueueCommands {
    List {
        scope: String,
        #[arg(long, default_value = ".dume/rust/harness.db")]
        db_path: String,
        #[arg(long, default_value = ".dume/rust/artifacts")]
        artifacts_dir: String,
    },
    Add {
        scope: String,
        payload: String,
        #[arg(long, default_value = "submit", value_parser = ["submit", "steer", "follow_up"])]
        kind: String,
        #[arg(long)]
        request_id: String,
        #[arg(long, default_value = ".dume/rust/harness.db")]
        db_path: String,
        #[arg(long, default_value = ".dume/rust/artifacts")]
        artifacts_dir: String,
    },
    Withdraw {
        scope: String,
        input_id: i64,
        #[arg(long, default_value = ".dume/rust/harness.db")]
        db_path: String,
        #[arg(long, default_value = ".dume/rust/artifacts")]
        artifacts_dir: String,
    },
    Resume {
        scope: String,
        input_id: i64,
        #[arg(long, default_value = ".dume/rust/harness.db")]
        db_path: String,
        #[arg(long, default_value = ".dume/rust/artifacts")]
        artifacts_dir: String,
    },
}

#[derive(Subcommand, Debug)]
enum McpCommands {
    List {
        #[arg(long)]
        project: Option<std::path::PathBuf>,
    },
    Trust {
        #[arg(long, default_value = ".")]
        repo: std::path::PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let result = dispatch(cli).await;
    if let Err(error) = dume_provider::auth::drain_refresh_owners(Duration::from_secs(95)).await {
        eprintln!("Credential refresh did not finish during shutdown: {error}");
        if result.is_ok() {
            return Err(error);
        }
    }
    result
}

async fn dispatch(cli: Cli) -> Result<()> {
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
            db_path,
            artifacts_dir,
            benchmark_run_id,
            case_id,
            variant,
        }) => {
            run_worker_child(
                &attempt_id,
                &worktree_path,
                test_command.as_deref(),
                task_prompt.as_deref(),
                &model,
                base_url.as_deref(),
                db_path.as_deref(),
                artifacts_dir.as_deref(),
                benchmark_run_id.as_deref(),
                case_id.as_deref(),
                variant.as_deref(),
            )
            .await?;
        }
        Some(Commands::Status {
            db_path,
            artifacts_dir,
        }) => {
            print_status(&db_path, &artifacts_dir)?;
        }
        Some(Commands::Interactive {
            model,
            continue_session,
            resume,
            all,
        }) => {
            let target = resolve_resume_target(continue_session, resume, all);
            let preserve_saved_epoch = !matches!(&target, dume_tui::ResumeTarget::None);
            let routing = dume_worker::routing::RoutingSnapshot::load_default(preserve_saved_epoch);
            dume_tui::run_tui_with_target(&model, target, routing).await?;
        }
        Some(Commands::Export {
            id,
            output,
            db_path,
            artifacts_dir,
        }) => {
            run_export(id.as_deref(), output.as_deref(), &db_path, &artifacts_dir)?;
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
            let codex_only = provider
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case("openai-codex"));
            let opencodex_only = provider
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case("opencodex"));
            if !codex_only && !opencodex_only {
                if let Err(error) =
                    dume_provider::ModelCatalog::refresh_remote_cache_if_stale().await
                {
                    eprintln!("Model catalog refresh failed; showing local models: {error:#}");
                }
            }
            if codex_only || provider.is_none() {
                if codex_only
                    && !dume_provider::CredentialStore::new(
                        dume_provider::CredentialStore::default_path(),
                    )
                    .has_credential("openai-codex")
                {
                    eprintln!(
                        "Live Codex models require `dume login openai-codex`; showing local models."
                    );
                }
                if let Err(error) =
                    dume_provider::ModelCatalog::refresh_codex_cache_if_stale().await
                {
                    eprintln!("Codex catalog refresh failed; showing local models: {error:#}");
                }
            }
            if opencodex_only || provider.is_none() {
                if let Err(error) = dume_provider::opencodex::refresh_cache_if_stale().await {
                    if opencodex_only {
                        return Err(error).context("Could not load local OpenCodex models");
                    }
                }
            }
            let all = dume_provider::ModelCatalog::list_all_models()?;
            println!(
                "{:<30} {:<12} {:<10} {:<12} {}",
                "MODEL ID", "PROVIDER", "REASONING", "MAX TOKENS", "NAME"
            );
            println!("{}", "-".repeat(80));
            for m in all {
                if !dume_provider::is_model_supported(&m) {
                    continue;
                }
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
        Some(Commands::Update { check }) => {
            run_update(check).await?;
        }
        Some(Commands::Usage {
            db_path,
            artifacts_dir,
            session_id,
        }) => {
            let store = HarnessStore::open(&db_path, &artifacts_dir)?;
            let usage = store.get_session_usage(&session_id)?;
            println!("Session Token Usage for '{}':", session_id);
            println!("  Input Tokens:  {}", usage.input_tokens);
            println!("  Output Tokens: {}", usage.output_tokens);
            println!("  Total Tokens:  {}", usage.total_tokens);
        }
        Some(Commands::Bench {
            cases,
            baseline_bin,
            candidate_bin,
            repetitions,
            model,
            base_url,
            output_dir,
            db_path,
        }) => {
            let run_id = format!(
                "bench_{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
            );
            let config = dume_cli::bench::BenchmarkRunConfig {
                run_id,
                case_ids: cases,
                baseline_bin,
                candidate_bin,
                repetitions,
                model,
                base_url,
                output_dir,
                db_path,
            };
            let runner = dume_cli::bench::BenchmarkRunner::new(config)?;
            let report = runner.run().await?;
            println!("{}", runner.render_markdown_report(&report));
        }
        Some(Commands::Runtime { command }) => match command {
            RuntimeCommands::Queue { command } => run_queue(command)?,
            RuntimeCommands::Inspect {
                run_id,
                db_path,
                artifacts_dir,
            } => {
                let store = HarnessStore::open(db_path, artifacts_dir)?;
                let run = store.get_run(&run_id)?;
                let executions = store.tool_executions_for_run(&run_id)?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"run":run,"tool_executions":executions})
                    )?
                );
            }
            RuntimeCommands::Resolve {
                execution_id,
                run_id,
                revision,
                operator,
                decision,
                evidence,
                result,
                acknowledge_duplicate_effect,
                db_path,
                artifacts_dir,
            } => {
                let store = HarnessStore::open(db_path, artifacts_dir)?;
                let run = store.get_run(&run_id)?;
                anyhow::ensure!(
                    run.status == "running" || run.status == "needs_attention",
                    "run must be active or needs attention to resolve an unknown tool"
                );
                let _scope_lock = store
                    .acquire_runtime_scope_lock(&run.scope)?
                    .context("runtime scope is owned by a live executor")?;
                let resolution = dume_store::ToolResolution {
                    operator,
                    kind: decision.clone(),
                    evidence,
                    result,
                    duplicate_effect_warning: acknowledge_duplicate_effect,
                };
                store.resolve_unknown(
                    &dume_store::RunToken {
                        run_id,
                        owner_generation: run.owner_generation,
                    },
                    &execution_id,
                    revision,
                    resolution,
                )?;
                println!("Resolved tool execution {execution_id} as {decision}");
            }
        },
        Some(Commands::Mcp { command }) => run_mcp(command).await?,
        None => {
            let target = resolve_resume_target(cli.continue_session, cli.resume, cli.all);
            let preserve_saved_epoch = !matches!(&target, dume_tui::ResumeTarget::None);
            let routing = dume_worker::routing::RoutingSnapshot::load_default(preserve_saved_epoch);
            dume_tui::run_tui_with_target("", target, routing).await?;
        }
    }

    Ok(())
}

fn runtime_scope(value: &str) -> Result<dume_store::RuntimeScope> {
    if let Some(id) = value.strip_prefix("attempt:") {
        anyhow::ensure!(!id.is_empty(), "attempt scope ID cannot be empty");
        Ok(dume_store::RuntimeScope::Attempt(id.to_owned()))
    } else {
        let id = value.strip_prefix("conversation:").unwrap_or(value);
        anyhow::ensure!(!id.is_empty(), "conversation scope ID cannot be empty");
        Ok(dume_store::RuntimeScope::Conversation(id.to_owned()))
    }
}

fn run_queue(command: QueueCommands) -> Result<()> {
    use dume_store::InputKind;
    match command {
        QueueCommands::List {
            scope,
            db_path,
            artifacts_dir,
        } => {
            let store = HarnessStore::open(db_path, artifacts_dir)?;
            let records = store.inbox(&runtime_scope(&scope)?, &[])?;
            println!("{}", serde_json::to_string_pretty(&records)?);
        }
        QueueCommands::Add {
            scope,
            payload,
            kind,
            request_id,
            db_path,
            artifacts_dir,
        } => {
            anyhow::ensure!(
                !request_id.trim().is_empty(),
                "--request-id must not be empty"
            );
            let input_kind = match kind.as_str() {
                "submit" => InputKind::Submit,
                "steer" => InputKind::Steer,
                "follow_up" => InputKind::FollowUp,
                _ => unreachable!(),
            };
            let store = HarnessStore::open(db_path, artifacts_dir)?;
            let receipt =
                store.admit_input(runtime_scope(&scope)?, &request_id, input_kind, &payload)?;
            println!("{}", serde_json::to_string(&receipt)?);
        }
        QueueCommands::Withdraw {
            scope,
            input_id,
            db_path,
            artifacts_dir,
        } => {
            let store = HarnessStore::open(db_path, artifacts_dir)?;
            anyhow::ensure!(
                store.withdraw_input(&runtime_scope(&scope)?, input_id)?,
                "input {input_id} is not pending or paused"
            );
            println!("Withdrawn input {input_id}");
        }
        QueueCommands::Resume {
            scope,
            input_id,
            db_path,
            artifacts_dir,
        } => {
            let store = HarnessStore::open(db_path, artifacts_dir)?;
            let receipt = store.reactivate_paused_input(&runtime_scope(&scope)?, input_id)?;
            println!("{}", serde_json::to_string(&receipt)?);
        }
    }
    Ok(())
}

async fn run_mcp(command: McpCommands) -> Result<()> {
    use dume_mcp::{McpConfig, McpToolBridge, TrustStore};
    let trust_path = McpConfig::global_path()?.with_file_name("mcp-trust.json");
    match command {
        McpCommands::List { project } => {
            let project_root = project
                .as_deref()
                .map(|p| {
                    p.canonicalize()
                        .with_context(|| format!("canonicalize project {}", p.display()))
                })
                .transpose()?;
            let project_config = project_root.as_deref().map(|p| p.join(".dume/mcp.json"));
            let mut loaded = McpConfig::load_with_origins(None, project_config.as_deref())?;
            let trust = TrustStore::load(&trust_path)?;
            let mut untrusted = Vec::new();
            for name in loaded.project_servers.clone() {
                let Some(server) = loaded.config.servers.get(&name) else {
                    continue;
                };
                if !server.enabled {
                    continue;
                }
                let digest = loaded.config.canonical_digest(&name)?;
                let repo = project_root
                    .as_deref()
                    .context("project-origin MCP server requires --project")?;
                if !trust.inspect(repo, &digest)? {
                    untrusted.push((name.clone(), digest));
                    if let Some(server) = loaded.config.servers.get_mut(&name) {
                        server.enabled = false;
                    }
                }
            }
            let mut bridge = McpToolBridge::new();
            bridge.trust = trust;
            let connect_result = bridge
                .connect_loaded_config(&loaded, project_root.as_deref())
                .await;
            let states = bridge.server_states();
            for (name, state, _) in &states {
                if !matches!(
                    state,
                    dume_mcp::ServerState::Ready | dume_mcp::ServerState::Deferred
                ) {
                    continue;
                }
                let server = &loaded.config.servers[name];
                if server.exposure == dume_mcp::ToolExposure::Deferred {
                    let _ = bridge.activate_deferred(name).await;
                } else {
                    let _ = bridge.discover_tools(name).await;
                }
            }
            for (name, state, count) in bridge.server_states() {
                let origin = if loaded.project_servers.contains(&name) {
                    "project"
                } else {
                    "global"
                };
                let digest = loaded.config.canonical_digest(&name)?;
                println!("{name}\t{origin}\t{state:?}\tfingerprint={digest}\ttools={count}");
                let available = bridge
                    .direct_tools()
                    .into_iter()
                    .chain(bridge.deferred_tools());
                for tool in available.filter(|tool| tool.name.starts_with(&format!("{name}__"))) {
                    println!("  {}\t{}", tool.name, tool.description.unwrap_or_default());
                }
            }
            bridge.shutdown().await;
            connect_result?;
            if !untrusted.is_empty() {
                let names = untrusted
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                anyhow::bail!(
                    "project MCP server(s) are not trusted for their current per-server fingerprints: {names}; review and run `dume mcp trust --repo {}`",
                    project_root.as_deref().unwrap().display()
                );
            }
        }
        McpCommands::Trust { repo } => {
            let root = repo
                .canonicalize()
                .with_context(|| format!("canonicalize project {}", repo.display()))?;
            let path = root.join(".dume/mcp.json");
            anyhow::ensure!(
                path.is_file(),
                "project MCP config does not exist: {}",
                path.display()
            );
            let loaded = McpConfig::load_with_origins(None, Some(&path))?;
            let mut trust = TrustStore::load(&trust_path)?;
            let mut approved = Vec::new();
            for name in &loaded.project_servers {
                if !loaded
                    .config
                    .servers
                    .get(name)
                    .is_some_and(|server| server.enabled)
                {
                    continue;
                }
                let digest = loaded.config.canonical_digest(name)?;
                trust.trust(&root, digest.clone())?;
                approved.push((name.clone(), digest));
            }
            trust.save(&trust_path)?;
            for (name, digest) in approved {
                println!(
                    "Trusted project MCP server {name} for {} (fingerprint {digest})",
                    root.display()
                );
            }
        }
    }
    Ok(())
}

fn resolve_resume_target(
    continue_session: bool,
    resume: Option<String>,
    all: bool,
) -> dume_tui::ResumeTarget {
    if continue_session {
        dume_tui::ResumeTarget::Latest
    } else if let Some(id_str) = resume {
        let trimmed = id_str.trim();
        if trimmed.is_empty() {
            dume_tui::ResumeTarget::Picker { all_dirs: all }
        } else {
            dume_tui::ResumeTarget::Id(trimmed.to_string())
        }
    } else {
        dume_tui::ResumeTarget::None
    }
}

fn run_export(
    id: Option<&str>,
    output_path: Option<&std::path::Path>,
    db_path: &str,
    artifacts_dir: &str,
) -> Result<()> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let resolved_db = if db_path == ".dume/agent/harness.db" {
        std::path::PathBuf::from(home).join(".dume/agent/harness.db")
    } else {
        std::path::PathBuf::from(db_path)
    };
    let store = HarnessStore::open(&resolved_db, artifacts_dir)?;

    let target_id = match id {
        Some(target) => {
            let found = store.load_conversation(target).ok().or_else(|| {
                let list = store.list_conversations(None, 50).ok()?;
                let matched = list.into_iter().find(|c| c.id.starts_with(target))?;
                store.load_conversation(&matched.id).ok()
            });
            match found {
                Some((rec, _)) => rec.id,
                None => anyhow::bail!("Conversation '{}' not found", target),
            }
        }
        None => {
            let raw_cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let cwd = dume_store::resolve_project_root(&raw_cwd)
                .to_str()
                .unwrap_or(".")
                .to_string();
            match store.latest_conversation_for_cwd(&cwd)? {
                Some(rec) => rec.id,
                None => anyhow::bail!("No conversations found for current directory"),
            }
        }
    };

    let (record, msg_records) = store.load_conversation(&target_id)?;
    let mut out_lines = Vec::new();

    let meta = serde_json::json!({
        "_type": "header",
        "conversation_id": record.id,
        "cwd": record.cwd,
        "model": record.model,
        "status": record.status,
        "created_at": record.created_at,
        "updated_at": record.updated_at,
    });
    out_lines.push(serde_json::to_string(&meta)?);

    for msg in msg_records {
        out_lines.push(msg.message_json);
    }

    let result_content = out_lines.join("\n") + "\n";
    if let Some(path) = output_path {
        std::fs::write(path, result_content)?;
        println!(
            "Exported conversation '{}' to {}",
            target_id,
            path.display()
        );
    } else {
        print!("{}", result_content);
    }

    Ok(())
}

async fn run_update(check_only: bool) -> Result<()> {
    let current_version = env!("CARGO_PKG_VERSION");
    println!(
        "Checking for updates (current version: v{})...",
        current_version
    );

    let repo = "jeongminsang/dum-e";
    match dume_core::updater::check_for_update(repo, current_version)? {
        Some(info) => {
            println!("\n🚀 New version available: v{}", info.latest_version);
            println!(
                "Target platform: {}",
                dume_core::updater::detect_platform()?
            );
            println!("Asset package: {}", info.asset_name);
            if !info.release_notes.is_empty() {
                println!("\nRelease Notes:\n{}", info.release_notes.trim());
            }

            if check_only {
                println!("\nRun `dume update` to apply this update in-place.");
                return Ok(());
            }

            println!("\nApplying hot update in-place...");
            let replaced_path = dume_core::updater::apply_update(&info, |msg| {
                println!("  • {}", msg);
            })?;
            println!(
                "\n✅ Successfully updated DUM-E in-place to v{}!",
                info.latest_version
            );
            println!("Binary location: {}", replaced_path.display());
        }
        None => {
            println!("✅ DUM-E is already up to date (v{}).", current_version);
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
        matches!(
            provider,
            "anthropic" | "openai" | "openai-codex" | "google" | "deepseek" | "opencodex"
        ),
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
    if api_key || matches!(provider, "openai" | "google" | "deepseek" | "opencodex") {
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
    db_path: Option<&str>,
    artifacts_dir: Option<&str>,
    benchmark_run_id: Option<&str>,
    case_id: Option<&str>,
    variant: Option<&str>,
) -> Result<()> {
    let path = Path::new(worktree_path);
    let cancellation = tokio_util::sync::CancellationToken::new();
    let signal_token = cancellation.clone();
    #[cfg(unix)]
    let mut terminate =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let _signal_task = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = terminate.recv() => {},
            }
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
        signal_token.cancel();
    }));

    // Send initial progress
    let progress_msg = WorkerToHostMessage::Progress {
        attempt_id: attempt_id.to_string(),
        message: format!("Worker starting in worktree {}", worktree_path),
    };
    print!("{}", serialize_message(&progress_msg)?);

    // 1. Run actual Agent Loop if task prompt is present
    let agent_outcome = if let Some(prompt) = task_prompt {
        let routing = dume_worker::routing::RoutingSnapshot::load_default(false);
        let mut agent = dume_worker::AgentLoop::new(path, model).with_routing_snapshot(routing);
        if let Some(base_url) = base_url {
            agent = agent.with_base_url(base_url);
        }
        let db_path = db_path.unwrap_or(".dume/rust/harness.db");
        let artifacts_path = artifacts_dir
            .map(Path::new)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| {
                Path::new(db_path)
                    .parent()
                    .map(|p| p.join("artifacts"))
                    .unwrap_or_else(|| Path::new(".dume/rust/artifacts").to_path_buf())
            });
        let store = HarnessStore::open(db_path, &artifacts_path)
            .with_context(|| format!("open worker runtime store {}", db_path))?;
        let ctx = dume_provider::types::StreamRequestContext {
            reasoning_effort: None,
            endpoint: None,
            session_id: attempt_id.to_string(),
            request_id: format!("req_{}_0", attempt_id),
            benchmark_run_id: benchmark_run_id.map(ToString::to_string),
            case_id: case_id.map(ToString::to_string),
            variant: variant.map(ToString::to_string),
            attempt_id: Some(attempt_id.to_string()),
            agent_id: Some("main".to_string()),
            parent_agent_id: None,
        };
        agent = agent.with_store(Arc::new(store)).with_request_context(ctx);
        let progress_exec = WorkerToHostMessage::Progress {
            attempt_id: attempt_id.to_string(),
            message: format!("Agent loop executing prompt: {}", prompt),
        };
        print!("{}", serialize_message(&progress_exec)?);
        let outcome = agent.run_task_with_cancellation(prompt, cancellation.clone()).await;
        Some(outcome?)
    } else {
        None
    };

    // 2. Run test command if provided
    let mut test_results = Vec::new();
    if let Some(cmd) = test_command {
        let result = WorkerExecutor::run_test_command_with_cancellation(path, cmd, cancellation.clone()).await?;
        test_results.push(result);
    }

    // 3. Finalize manifest (detect modified files, commit changes)
    let summary = match &agent_outcome {
        Some(dume_worker::AgentOutcome::Completed { answer, .. }) => {
            format!("Worker completed attempt: {}", answer)
        }
        Some(dume_worker::AgentOutcome::TurnLimitExhausted { last_reply, .. }) => {
            format!(
                "Worker turn limit exhausted. Last reply: {}",
                last_reply.as_deref().unwrap_or("none")
            )
        }
        Some(dume_worker::AgentOutcome::Cancelled { .. }) => "Worker cancelled".to_string(),
        None => "Worker completed attempt".to_string(),
    };

    if let Some(outcome) = &agent_outcome {
        match outcome {
            dume_worker::AgentOutcome::Completed { .. } => {}
            dume_worker::AgentOutcome::TurnLimitExhausted { .. } => {
                anyhow::bail!("worker turn limit exhausted; no successful manifest emitted")
            }
            dume_worker::AgentOutcome::Cancelled { .. } => {
                anyhow::bail!("worker cancelled; no successful manifest emitted")
            }
        }
    }

    anyhow::ensure!(!cancellation.is_cancelled(), "Worker cancelled before finalization");
    let manifest =
        WorkerExecutor::finalize_manifest(attempt_id, path, test_results, &summary).await?;

    // Send completed message to host
    let completed_msg = WorkerToHostMessage::Completed {
        manifest,
        outcome: agent_outcome,
    };
    print!("{}", serialize_message(&completed_msg)?);

    Ok(())
}

async fn run_coordinator(db_path: &str, artifacts_dir: &str, repo_path: &str) -> Result<()> {
    let repo = Path::new(repo_path).canonicalize()?;
    let store = Arc::new(HarnessStore::open(db_path, artifacts_dir)?);
    let host = dume_worker::WorkerHost::new(
        std::env::current_exe()?.to_str().context("Invalid executable path")?,
    )
    .with_db_path(store.db_path.canonicalize()?)
    .with_artifacts_dir(Path::new(artifacts_dir).canonicalize()?);
    dume_cli::coordinator::run_coordinator(store, repo, host).await
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
        assert_eq!(login_provider("deepseek").unwrap(), "deepseek");
        assert_eq!(login_provider("opencodex").unwrap(), "opencodex");
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
                "--artifacts-dir",
                "/tmp/custom-artifacts",
            ])
            .unwrap()
            .command,
            Some(Commands::Worker {
                base_url: Some(_),
                artifacts_dir: Some(path),
                ..
            }) if path == "/tmp/custom-artifacts"
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
        assert!(matches!(
            Cli::try_parse_from(["dume", "update", "--check"])
                .unwrap()
                .command,
            Some(Commands::Update { check: true })
        ));
        assert!(matches!(
            Cli::try_parse_from(["dume", "update"]).unwrap().command,
            Some(Commands::Update { check: false })
        ));
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

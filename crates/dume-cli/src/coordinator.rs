use anyhow::{Context, Result};
use dume_core::{TaskDag, types::*};
use dume_store::HarnessStore;
use dume_worker::WorkerHost;
use std::{collections::HashSet, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    sync::{Mutex, Semaphore},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

pub async fn run_coordinator(
    store: Arc<HarnessStore>,
    repo: PathBuf,
    host: WorkerHost,
) -> Result<()> {
    let owner = format!(
        "pid_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    );
    let lock = store.acquire_coordinator_lock("default_coordinator", &owner, 15_000)?;
    let cancellation = CancellationToken::new();
    let _shutdown = cancellation.clone().drop_guard();
    let heartbeat = {
        let store = store.clone();
        let lock = lock.clone();
        let cancellation = cancellation.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancellation.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {
                        if let Err(error) = store.heartbeat_coordinator(&lock.coordinator_id, lock.owner_id.as_deref().unwrap(), 15_000) {
                            tracing::error!("Coordinator ownership lost: {error}");
                            cancellation.cancel();
                            break;
                        }
                    }
                }
            }
        })
    };
    let mut jobs = JoinSet::new();
    let result: Result<()> = async {
        let (ready, attention, unknown) = store.recover_state(lock.epoch)?;
        tracing::info!(
            "Recovery: {} submitted, {} needing attention, {} unknown operations",
            ready.len(),
            attention.len(),
            unknown.len()
        );
        for pending in store.list_pending_integrations()? {
            if let Some(commit) = &pending.integration_commit {
                let current = dume_git::resolve_ref(&repo, &pending.target_branch).await?;
                if current == *commit || dume_git::is_ancestor(&repo, commit, &current).await? {
                    store.record_integration_fenced(
                        &lock,
                        &Integration {
                            status: IntegrationStatus::Applied,
                            ..pending
                        },
                    )?;
                }
                // An unpublished Pending intent is revalidated through its submitted attempt.
            }
        }
        for attempt in ready {
            crate::verify_and_integrate_attempt(
                &store,
                repo.to_str().context("Invalid repository path")?,
                &attempt,
                &lock,
                cancellation.clone(),
            )
            .await?;
        }

        let execution_slots = Arc::new(Semaphore::new(2));
        let integration_lock = Arc::new(Mutex::new(()));
        let mut in_flight = HashSet::new();
        let mut idle = 0;
        loop {
            anyhow::ensure!(
                !cancellation.is_cancelled(),
                "Coordinator cancelled after ownership loss"
            );
            store.check_coordinator_fence(&lock)?;
            let goals = store.list_active_goals()?;
            if goals.is_empty() && jobs.is_empty() {
                idle += 1;
                if idle >= 3 {
                    break;
                }
            } else {
                idle = 0;
            }
            for goal in goals {
                if matches!(
                    goal.status,
                    GoalStatus::Cancelled | GoalStatus::NeedsAttention
                ) {
                    continue;
                }
                let tasks = store.list_tasks_for_goal(&goal.id)?;
                if tasks.is_empty() {
                    continue;
                }
                if tasks
                    .iter()
                    .all(|task| task.status == TaskStatus::Completed)
                {
                    store.update_goal_status_fenced(&lock, &goal.id, GoalStatus::Completed)?;
                    continue;
                }
                if tasks.iter().any(|task| {
                    matches!(task.status, TaskStatus::Failed | TaskStatus::NeedsAttention)
                }) {
                    store.update_goal_status_fenced(&lock, &goal.id, GoalStatus::Failed)?;
                    continue;
                }
                let dag = match TaskDag::new(&tasks) {
                    Ok(dag) => dag,
                    Err(error) => {
                        tracing::error!("DAG error: {error}");
                        store.update_goal_status_fenced(&lock, &goal.id, GoalStatus::Failed)?;
                        continue;
                    }
                };
                let mut ready = dag.get_ready_tasks();
                ready.sort_by(|a, b| a.id.cmp(&b.id));
                for task in ready {
                    if !in_flight.insert(task.id.clone()) {
                        continue;
                    }
                    let task = task.clone();
                    let store = store.clone();
                    let repo = repo.clone();
                    let host = host.clone();
                    let lock = lock.clone();
                    let slots = execution_slots.clone();
                    let gate = integration_lock.clone();
                    let cancellation = cancellation.clone();
                    jobs.spawn(async move {
                        let id = task.id.clone();
                        let result =
                            run_attempt(store, repo, host, task, lock, slots, gate, cancellation)
                                .await;
                        (id, result)
                    });
                }
            }
            tokio::select! {
                _ = cancellation.cancelled() => anyhow::bail!("Coordinator ownership lost"),
                result = jobs.join_next(), if !jobs.is_empty() => {
                    let (id, result) = result.context("Missing coordinator job")??;
                    in_flight.remove(&id);
                    result?;
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
        }
        Ok(())
    }
    .await;
    cancellation.cancel();
    // Await cooperative process-group cleanup; aborting WorkerHost would orphan workers.
    while jobs.join_next().await.is_some() {}
    let _ = heartbeat.await;
    store.release_coordinator_lock(&lock.coordinator_id, &owner)?;
    result
}

async fn run_attempt(
    store: Arc<HarnessStore>,
    repo: PathBuf,
    host: WorkerHost,
    task: Task,
    lock: CoordinatorLock,
    slots: Arc<Semaphore>,
    gate: Arc<Mutex<()>>,
    cancellation: CancellationToken,
) -> Result<()> {
    let permit = tokio::select! {
        biased;
        _ = cancellation.cancelled() => anyhow::bail!("Attempt cancelled before launch"),
        permit = slots.acquire() => permit?,
    };
    store.check_coordinator_fence(&lock)?;
    let id = format!(
        "att_{}_{}",
        task.id,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    );
    let worktree = repo.join(format!(".dume/rust/worktrees/{id}"));
    dume_git::create_git_worktree(&repo, &worktree, &task.target_branch).await?;
    let attempt = store.create_attempt_fenced(
        &lock,
        &id,
        &task.id,
        lock.owner_id.as_deref().unwrap(),
        worktree.to_str().context("Invalid worktree path")?,
        30_000,
    )?;
    let manifest = host
        .run_attempt(
            &attempt.id,
            &task.id,
            lock.epoch,
            &worktree,
            task.acceptance_criteria.first().cloned(),
            Some(task.description.clone()),
            Some("anthropic/claude-sonnet-4-5".to_owned()),
            cancellation.clone(),
        )
        .await;
    drop(permit);
    match manifest {
        Ok(manifest) => {
            let hash = store
                .artifacts
                .save_artifact(&serde_json::to_vec_pretty(&manifest)?)?;
            // Persist the finished result before waiting for the publication lane.
            store.submit_attempt_result_fenced(&lock, &id, &manifest.candidate_commit, &hash)?;
            let _guard = tokio::select! {
                biased;
                _ = cancellation.cancelled() => anyhow::bail!("Attempt cancelled before verification"),
                guard = gate.lock() => guard,
            };
            crate::verify_and_integrate_attempt(
                &store,
                repo.to_str().context("Invalid repository path")?,
                &store.get_attempt(&id)?,
                &lock,
                cancellation.clone(),
            )
            .await?;
        }
        Err(error) => {
            if cancellation.is_cancelled() {
                return Err(error);
            }
            tracing::error!("Worker {id} failed: {error}");
            store.update_attempt_task_status_fenced(
                &lock,
                &id,
                &task.id,
                AttemptStatus::Rejected,
                TaskStatus::Failed,
            )?;
        }
    }
    let attempt = store.get_attempt(&id)?;
    if let Some(commit) = attempt.candidate_commit {
        if store
            .get_integration(&task.target_branch, &commit)?
            .is_some_and(|record| record.status == IntegrationStatus::Applied)
        {
            if let Err(error) = dume_git::remove_git_worktree(&repo, &worktree).await {
                tracing::warn!("Applied worktree retained: {error}");
            }
        }
    }
    Ok(())
}

#![cfg(unix)]

use dume_core::{manifest::ResultManifest, types::*};
use dume_store::{HarnessStore, StoreError};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tempfile::{TempDir, tempdir};
use tokio_util::sync::CancellationToken;

fn git(repo: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

struct Fixture {
    dir: TempDir,
    repo: PathBuf,
    store: Arc<HarnessStore>,
    lock: CoordinatorLock,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["config", "user.name", "Test"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["config", "commit.gpgSign", "false"]);
        std::fs::write(repo.join("value.txt"), "10\n").unwrap();
        std::fs::write(repo.join("limit.txt"), "200\n").unwrap();
        git(&repo, &["add", "value.txt", "limit.txt"]);
        git(&repo, &["commit", "-m", "initial"]);
        let store = Arc::new(
            HarnessStore::open(dir.path().join("harness.db"), dir.path().join("artifacts"))
                .unwrap(),
        );
        let lock = store
            .acquire_coordinator_lock("default_coordinator", "initial", 60_000)
            .unwrap();
        store.create_goal("goal", "test").unwrap();
        Self {
            dir,
            repo,
            store,
            lock,
        }
    }

    fn task(
        &self,
        id: &str,
        description: &str,
        commands: &[&str],
        paths: Option<Vec<String>>,
        dependencies: Vec<String>,
    ) -> Task {
        let task = Task {
            id: id.into(),
            goal_id: "goal".into(),
            title: id.into(),
            description: description.into(),
            status: TaskStatus::Ready,
            dependencies,
            acceptance_criteria: commands.iter().map(|s| (*s).into()).collect(),
            allowed_paths: paths,
            target_branch: "main".into(),
            created_at: 0,
            updated_at: 0,
        };
        self.store.create_task(&task).unwrap();
        task
    }

    async fn candidate(
        &self,
        task: &Task,
        changes: &[(&str, &str)],
        reported: Vec<String>,
    ) -> Attempt {
        let worktree = self.dir.path().join("candidate");
        dume_git::create_git_worktree(&self.repo, &worktree, "main")
            .await
            .unwrap();
        for (path, value) in changes {
            std::fs::write(worktree.join(path), value).unwrap();
        }
        let commit = dume_git::commit_worktree_changes(&worktree, "candidate")
            .await
            .unwrap();
        let attempt = self
            .store
            .create_attempt_fenced(
                &self.lock,
                "attempt",
                &task.id,
                "worker",
                worktree.to_str().unwrap(),
                60_000,
            )
            .unwrap();
        let manifest = ResultManifest {
            attempt_id: attempt.id.clone(),
            candidate_commit: commit.clone(),
            modified_files: reported,
            changed_artifacts: vec![],
            test_results: vec![],
            summary: "test".into(),
        };
        let hash = self
            .store
            .artifacts
            .save_artifact(&serde_json::to_vec(&manifest).unwrap())
            .unwrap();
        self.store
            .submit_attempt_result_fenced(&self.lock, &attempt.id, &commit, &hash)
            .unwrap();
        self.store.get_attempt(&attempt.id).unwrap()
    }

    async fn verify(&self, attempt: &Attempt) -> anyhow::Result<()> {
        dume_cli::verify_and_integrate_attempt(
            &self.store,
            self.repo.to_str().unwrap(),
            attempt,
            &self.lock,
            CancellationToken::new(),
        )
        .await
    }
}

#[tokio::test]
async fn individually_valid_candidate_is_rejected_when_merged_tree_fails() {
    let f = Fixture::new();
    let task = f.task(
        "task",
        "",
        &["test \"$(cat value.txt)\" -le \"$(cat limit.txt)\""],
        Some(vec!["value.txt".into()]),
        vec![],
    );
    let attempt = f
        .candidate(&task, &[("value.txt", "100\n")], vec!["value.txt".into()])
        .await;
    std::fs::write(f.repo.join("limit.txt"), "50\n").unwrap();
    git(&f.repo, &["add", "limit.txt"]);
    git(&f.repo, &["commit", "-m", "independent limit change"]);
    let before = git(&f.repo, &["rev-parse", "main"]);
    f.verify(&attempt).await.unwrap();
    assert_eq!(git(&f.repo, &["rev-parse", "main"]), before);
    assert_eq!(
        f.store.get_attempt(&attempt.id).unwrap().status,
        AttemptStatus::Rejected
    );
    assert!(f.store.list_pending_integrations().unwrap().is_empty());
}

#[tokio::test]
async fn forged_file_list_does_not_bypass_allowed_paths() {
    let f = Fixture::new();
    let task = f.task(
        "task",
        "",
        &["true"],
        Some(vec!["value.txt".into()]),
        vec![],
    );
    let attempt = f
        .candidate(
            &task,
            &[("value.txt", "20\n"), ("secret.txt", "not allowed")],
            vec![],
        )
        .await;
    let before = git(&f.repo, &["rev-parse", "main"]);
    f.verify(&attempt).await.unwrap();
    assert_eq!(
        f.store.get_attempt(&attempt.id).unwrap().status,
        AttemptStatus::Rejected
    );
    assert_eq!(git(&f.repo, &["rev-parse", "main"]), before);
}

#[tokio::test]
async fn rename_checks_source_and_unquoted_destination_paths() {
    let f = Fixture::new();
    let worktree = f.dir.path().join("rename");
    dume_git::create_git_worktree(&f.repo, &worktree, "main")
        .await
        .unwrap();
    std::fs::rename(
        worktree.join("value.txt"),
        worktree.join("allowed name\nfile.txt"),
    )
    .unwrap();
    let commit = dume_git::commit_worktree_changes(&worktree, "rename")
        .await
        .unwrap();
    let paths = dume_git::diff::get_commit_modified_files(&f.repo, &commit)
        .await
        .unwrap();
    assert!(paths.contains(&"value.txt".into()));
    assert!(paths.contains(&"allowed name\nfile.txt".into()));
}

#[tokio::test]
async fn target_rename_cannot_redirect_an_allowed_patch_outside_its_scope() {
    let f = Fixture::new();
    let task = f.task(
        "task",
        "",
        &["true"],
        Some(vec!["value.txt".into()]),
        vec![],
    );
    let attempt = f.candidate(&task, &[("value.txt", "20\n")], vec![]).await;
    git(&f.repo, &["mv", "value.txt", "protected-value.txt"]);
    git(&f.repo, &["commit", "-m", "rename on target"]);
    let before = git(&f.repo, &["rev-parse", "main"]);
    f.verify(&attempt).await.unwrap();
    assert_eq!(
        f.store.get_attempt(&attempt.id).unwrap().status,
        AttemptStatus::Rejected
    );
    assert_eq!(git(&f.repo, &["rev-parse", "main"]), before);
}

#[tokio::test]
async fn valid_integration_is_published_and_idempotent() {
    let f = Fixture::new();
    let task = f.task(
        "task",
        "",
        &["test \"$(cat value.txt)\" = 20"],
        Some(vec!["value.txt".into()]),
        vec![],
    );
    let attempt = f.candidate(&task, &[("value.txt", "20\n")], vec![]).await;
    f.verify(&attempt).await.unwrap();
    let after = git(&f.repo, &["rev-parse", "main"]);
    f.verify(&attempt).await.unwrap();
    assert_eq!(git(&f.repo, &["rev-parse", "main"]), after);
    assert_eq!(
        f.store.get_attempt(&attempt.id).unwrap().status,
        AttemptStatus::Accepted
    );
    assert_eq!(
        f.store.get_task(&task.id).unwrap().status,
        TaskStatus::Completed
    );
}

#[tokio::test]
async fn validation_cannot_modify_the_tree_it_certifies() {
    let f = Fixture::new();
    let task = f.task("task", "", &["echo 99 > value.txt"], None, vec![]);
    let attempt = f.candidate(&task, &[("value.txt", "20\n")], vec![]).await;
    let before = git(&f.repo, &["rev-parse", "main"]);
    f.verify(&attempt).await.unwrap();
    assert_eq!(
        f.store.get_attempt(&attempt.id).unwrap().status,
        AttemptStatus::Rejected
    );
    assert_eq!(git(&f.repo, &["rev-parse", "main"]), before);
}

#[tokio::test]
async fn stale_coordinator_cannot_start_verification_or_publish_pending() {
    let f = Fixture::new();
    let task = f.task("task", "", &["true"], None, vec![]);
    let attempt = f.candidate(&task, &[("value.txt", "20\n")], vec![]).await;
    let before = git(&f.repo, &["rev-parse", "main"]);
    f.store
        .force_expire_coordinator_lock("default_coordinator")
        .unwrap();
    f.store
        .acquire_coordinator_lock("default_coordinator", "replacement", 60_000)
        .unwrap();
    assert!(f.verify(&attempt).await.is_err());
    let pending = Integration {
        target_branch: "main".into(),
        base_commit: before.clone(),
        candidate_commit: attempt.candidate_commit.clone().unwrap(),
        integration_commit: attempt.candidate_commit.clone(),
        status: IntegrationStatus::Pending,
        error_message: None,
        integrated_at: 0,
    };
    assert!(dume_cli::apply_integration_fenced(&f.store, &f.repo, &pending, &f.lock).is_err());
    assert_eq!(git(&f.repo, &["rev-parse", "main"]), before);
}

#[test]
fn expired_lease_cannot_be_revived_by_heartbeat() {
    let f = Fixture::new();
    let conn = rusqlite::Connection::open(&f.store.db_path).unwrap();
    conn.execute("UPDATE coordinator_locks SET lease_expires_at = 0", [])
        .unwrap();
    assert!(
        f.store
            .heartbeat_coordinator("default_coordinator", "initial", 60_000)
            .is_err()
    );
    assert!(f.store.check_coordinator_fence(&f.lock).is_err());
}

#[test]
fn takeover_cannot_interleave_with_fenced_publication() {
    let f = Fixture::new();
    let other = HarnessStore::open(&f.store.db_path, f.dir.path().join("artifacts")).unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let thread = f
        .store
        .with_coordinator_fence(&f.lock, |conn| -> Result<_, StoreError> {
            conn.execute("UPDATE coordinator_locks SET lease_expires_at = 0", [])?;
            let thread = std::thread::spawn(move || {
                started_tx.send(()).unwrap();
                let lock = other
                    .acquire_coordinator_lock("default_coordinator", "replacement", 60_000)
                    .unwrap();
                done_tx.send(lock.epoch).unwrap();
            });
            started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
            Ok(thread)
        })
        .unwrap();
    assert_eq!(
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        f.lock.epoch + 1
    );
    thread.join().unwrap();
}

#[cfg(unix)]
async fn wait_for(path: &Path) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[cfg(unix)]
fn fake_worker(f: &Fixture) -> dume_worker::WorkerHost {
    use std::os::unix::fs::PermissionsExt;
    let path = f.dir.path().join("worker.sh");
    let root = f.dir.path().display();
    std::fs::write(&path, format!(r#"#!/bin/sh
set -eu
if [ "$1" = worker ]; then shift; fi
while [ "$#" -gt 0 ]; do
    case "$1" in
        --attempt-id) id="$2";;
        --worktree-path) wt="$2";;
        --task-prompt) task="$2";;
    esac
    shift 2
done
echo $$ > '{root}/pid-'"$task"
touch '{root}/started-'"$task"
if [ "$task" = slow ]; then
    while [ ! -e '{root}/release-slow' ]; do sleep 0.02; done
fi
printf '%s\n' "$task" > "$wt/$task.txt"
git -C "$wt" add -- "$task.txt"
git -C "$wt" commit -m "$task" >/dev/null
commit=$(git -C "$wt" rev-parse HEAD)
printf '{{"type":"completed","manifest":{{"attempt_id":"%s","candidate_commit":"%s","modified_files":["%s.txt"],"changed_artifacts":[],"test_results":[],"summary":"test"}}}}
' "$id" "$commit" "$task"
"#)).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    dume_worker::WorkerHost::new(path.to_str().unwrap())
}

#[cfg(unix)]
#[tokio::test]
async fn takeover_during_merged_validation_prevents_publication() {
    let f = Fixture::new();
    let count = f.dir.path().join("count");
    let ready = f.dir.path().join("merged-ready");
    let release = f.dir.path().join("merged-release");
    let command = format!(
        "n=0; if [ -f '{count}' ]; then n=$(cat '{count}'); fi; n=$((n+1)); echo $n > '{count}'; if [ $n -eq 2 ]; then touch '{ready}'; while [ ! -e '{release}' ]; do sleep 0.02; done; fi",
        count = count.display(),
        ready = ready.display(),
        release = release.display()
    );
    let task = f.task("task", "", &[&command], None, vec![]);
    let attempt = f.candidate(&task, &[("value.txt", "20\n")], vec![]).await;
    let before = git(&f.repo, &["rev-parse", "main"]);
    let store = f.store.clone();
    let repo = f.repo.clone();
    let lock = f.lock.clone();
    let running = tokio::spawn(async move {
        dume_cli::verify_and_integrate_attempt(
            &store,
            repo.to_str().unwrap(),
            &attempt,
            &lock,
            CancellationToken::new(),
        )
        .await
    });
    wait_for(&ready).await;
    f.store
        .force_expire_coordinator_lock("default_coordinator")
        .unwrap();
    f.store
        .acquire_coordinator_lock("default_coordinator", "replacement", 60_000)
        .unwrap();
    std::fs::write(release, "go").unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), running)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_eq!(git(&f.repo, &["rev-parse", "main"]), before);
    assert!(f.store.list_pending_integrations().unwrap().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn fast_predecessor_unlocks_successor_before_slow_sibling_finishes() {
    let f = Fixture::new();
    f.store
        .release_coordinator_lock("default_coordinator", "initial")
        .unwrap();
    f.task("a", "slow", &["true"], None, vec![]);
    f.task("b", "fast", &["true"], None, vec![]);
    f.task("c", "successor", &["true"], None, vec!["b".into()]);
    let host = fake_worker(&f);
    let store = f.store.clone();
    let repo = f.repo.clone();
    let running = tokio::spawn(dume_cli::coordinator::run_coordinator(store, repo, host));
    wait_for(&f.dir.path().join("started-successor")).await;
    assert_eq!(
        f.store.get_task("a").unwrap().status,
        TaskStatus::InProgress
    );
    std::fs::write(f.dir.path().join("release-slow"), "go").unwrap();
    tokio::time::timeout(Duration::from_secs(10), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(f.store.get_task("c").unwrap().status, TaskStatus::Completed);
}

#[cfg(unix)]
#[tokio::test]
async fn execution_slot_and_result_are_released_before_validation_waits() {
    let f = Fixture::new();
    f.store
        .release_coordinator_lock("default_coordinator", "initial")
        .unwrap();
    let check = format!(
        "touch '{}'; while [ ! -e '{}' ]; do sleep 0.02; done",
        f.dir.path().join("validating").display(),
        f.dir.path().join("release-validation").display()
    );
    f.task("a", "first", &[&check], None, vec![]);
    f.task("b", "slow", &["true"], None, vec![]);
    f.task("c", "third", &["true"], None, vec![]);
    let host = fake_worker(&f);
    let running = tokio::spawn(dume_cli::coordinator::run_coordinator(
        f.store.clone(),
        f.repo.clone(),
        host,
    ));
    wait_for(&f.dir.path().join("validating")).await;
    wait_for(&f.dir.path().join("started-third")).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let conn = rusqlite::Connection::open(&f.store.db_path).unwrap();
            let submitted: i64 = conn.query_row("SELECT COUNT(*) FROM attempts WHERE task_id = 'c' AND status = 'result_submitted'", [], |row| row.get(0)).unwrap();
            if submitted == 1 { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    std::fs::write(f.dir.path().join("release-validation"), "go").unwrap();
    std::fs::write(f.dir.path().join("release-slow"), "go").unwrap();
    tokio::time::timeout(Duration::from_secs(10), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn ownership_loss_cancels_workers_and_preserves_recovery_evidence() {
    let f = Fixture::new();
    f.store
        .release_coordinator_lock("default_coordinator", "initial")
        .unwrap();
    f.task("a", "slow", &["true"], None, vec![]);
    let host = fake_worker(&f);
    let running = tokio::spawn(dume_cli::coordinator::run_coordinator(
        f.store.clone(),
        f.repo.clone(),
        host,
    ));
    wait_for(&f.dir.path().join("started-slow")).await;
    let pid: i32 = std::fs::read_to_string(f.dir.path().join("pid-slow"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    f.store
        .force_expire_coordinator_lock("default_coordinator")
        .unwrap();
    let replacement = f
        .store
        .acquire_coordinator_lock("default_coordinator", "replacement", 60_000)
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(10), running)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    // The test uses kill(0) through a shell, without adding a libc dependency to CLI.
    assert!(
        !std::process::Command::new("sh")
            .args(["-c", &format!("kill -0 {pid} 2>/dev/null")])
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        f.store
            .get_coordinator_lock("default_coordinator")
            .unwrap()
            .unwrap()
            .owner_id
            .as_deref(),
        Some("replacement")
    );
    assert_eq!(f.store.recover_state(replacement.epoch).unwrap().1.len(), 1);
    assert_eq!(
        f.store.get_task("a").unwrap().status,
        TaskStatus::NeedsAttention
    );
}

#[tokio::test]
async fn cli_worker_sigterm_cancels_its_validation_process_group() {
    let f = Fixture::new();
    let ready = f.dir.path().join("validation-ready");
    let release = f.dir.path().join("validation-release");
    let late_output = f.dir.path().join("validation-survived");
    let command = format!(
        "touch '{}'; while [ ! -e '{}' ]; do sleep 0.02; done; echo survived > '{}'",
        ready.display(),
        release.display(),
        late_output.display()
    );
    let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_dume"))
        .args(["worker", "--attempt-id", "signal-test", "--worktree-path"])
        .arg(&f.repo)
        .args(["--test-command", &command])
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id().unwrap();
    wait_for(&ready).await;
    assert!(
        std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Validation cancelled"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("\"type\":\"completed\""));
    std::fs::write(release, "go").unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!late_output.exists());
}

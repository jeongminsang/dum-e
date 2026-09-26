use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

#[derive(Clone)]
pub struct BenchmarkFixture {
    pub id: &'static str,
    pub name: &'static str,
    pub task_prompt: &'static str,
    pub setup_fn: fn(&Path) -> Result<()>,
    pub check_fn: fn(&Path) -> Result<bool, String>,
}

pub fn all_fixtures() -> Vec<BenchmarkFixture> {
    vec![
        BenchmarkFixture {
            id: "targeted_bug_fix",
            name: "Targeted Bug Fix",
            task_prompt: "Fix the bug in calc.py so that add(a, b) returns a + b.",
            setup_fn: setup_targeted_bug_fix,
            check_fn: check_targeted_bug_fix,
        },
        BenchmarkFixture {
            id: "repo_investigation",
            name: "Repository Investigation",
            task_prompt: "Investigate git log to find the commit message containing 'CRITICAL_LEAK'. Write the exact commit subject to .dume_finding.txt.",
            setup_fn: setup_repo_investigation,
            check_fn: check_repo_investigation,
        },
        BenchmarkFixture {
            id: "large_log_diagnosis",
            name: "Diagnosis of Large Test Log",
            task_prompt: "Analyze test_suite.log and identify which test failed. Write ONLY the exact name of the failing test to .failed_test.txt.",
            setup_fn: setup_large_log_diagnosis,
            check_fn: check_large_log_diagnosis,
        },
        BenchmarkFixture {
            id: "multi_file_change",
            name: "Multi-file Change",
            task_prompt: "Update the field 'active' to 'is_active' in both service.py and client.py so that client uses user['is_active'].",
            setup_fn: setup_multi_file_change,
            check_fn: check_multi_file_change,
        },
        BenchmarkFixture {
            id: "continuation_constraint",
            name: "Continuation with User Constraint",
            task_prompt: "Constraint: Do not modify config.json under any circumstances. Update server.py to use PORT = 9090 instead of reading from config.",
            setup_fn: setup_continuation_constraint,
            check_fn: check_continuation_constraint,
        },
    ]
}

fn init_git_repo(path: &Path) -> Result<()> {
    Command::new("git")
        .arg("init")
        .current_dir(path)
        .output()
        .context("git init failed")?;
    Command::new("git")
        .args(["config", "user.name", "DUME Benchmark"])
        .current_dir(path)
        .output()?;
    Command::new("git")
        .args(["config", "user.email", "bench@dum-e.local"])
        .current_dir(path)
        .output()?;
    Ok(())
}

fn git_commit_all(path: &Path, msg: &str) -> Result<()> {
    Command::new("git")
        .args(["add", "."])
        .current_dir(path)
        .output()?;
    Command::new("git")
        .args(["commit", "-m", msg])
        .current_dir(path)
        .output()?;
    Ok(())
}

// 1. Targeted Bug Fix
fn setup_targeted_bug_fix(path: &Path) -> Result<()> {
    init_git_repo(path)?;
    std::fs::write(
        path.join("calc.py"),
        "def add(a, b):\n    return a - b  # BUG: should be +\n",
    )?;
    git_commit_all(path, "Initial commit with bug")?;
    Ok(())
}

fn check_targeted_bug_fix(path: &Path) -> Result<bool, String> {
    let calc_file = path.join("calc.py");
    if !calc_file.exists() {
        return Err("calc.py does not exist".to_string());
    }
    let content = std::fs::read_to_string(&calc_file).map_err(|e| e.to_string())?;
    // Acceptance check executes comprehensive python3 test suite outside the agent editable files
    let test_script = "import calc\nassert calc.add(2, 3) == 5\nassert calc.add(10, -5) == 5\nassert calc.add(0, 0) == 0\nassert calc.add(-7, -8) == -15\nassert calc.add(100, 200) == 300\n";
    let out = Command::new("python3")
        .args(["-B", "-c", test_script])
        .current_dir(path)
        .output();

    match out {
        Ok(o) if o.status.success() => Ok(true),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            Err(format!("calc.add test failed: {}", stderr.trim()))
        }
        Err(_) => {
            // Fallback syntax inspection if python3 is unavailable
            if content.contains("return a + b") || content.contains("return b + a") {
                Ok(true)
            } else {
                Err("calc.py does not contain general 'return a + b'".to_string())
            }
        }
    }
}

// 2. Repository Investigation
fn setup_repo_investigation(path: &Path) -> Result<()> {
    init_git_repo(path)?;
    std::fs::write(path.join("file1.txt"), "first version\n")?;
    git_commit_all(path, "Initial setup")?;

    std::fs::write(path.join("file2.txt"), "secret leak here\n")?;
    git_commit_all(path, "CRITICAL_LEAK: accidentally committed secret")?;

    std::fs::write(path.join("file3.txt"), "cleanup\n")?;
    git_commit_all(path, "Update documentation")?;
    Ok(())
}

fn check_repo_investigation(path: &Path) -> Result<bool, String> {
    let finding_file = path.join(".dume_finding.txt");
    if !finding_file.exists() {
        return Err(".dume_finding.txt was not created".to_string());
    }
    let content = std::fs::read_to_string(&finding_file).map_err(|e| e.to_string())?;
    let trimmed = content.trim();
    if trimmed == "CRITICAL_LEAK: accidentally committed secret"
        || trimmed.contains("CRITICAL_LEAK: accidentally committed secret")
    {
        Ok(true)
    } else {
        Err(format!(
            ".dume_finding.txt content '{:?}' does not match expected full commit subject 'CRITICAL_LEAK: accidentally committed secret'",
            trimmed
        ))
    }
}

// 3. Large Log Diagnosis
fn setup_large_log_diagnosis(path: &Path) -> Result<()> {
    init_git_repo(path)?;
    let mut log = String::with_capacity(1024 * 60);
    for i in 1..=500 {
        log.push_str(&format!("[INFO] 2026-09-22 10:00:{:02} - test_worker_suite_{} PASSED\n", i % 60, i));
    }
    log.push_str("[ERROR] 2026-09-22 10:08:14 - test_database_connection FAILED: ConnectionRefusedError(port 5432 unreachable)\n");
    for i in 501..=1000 {
        log.push_str(&format!("[INFO] 2026-09-22 10:15:{:02} - test_worker_suite_{} PASSED\n", i % 60, i));
    }
    std::fs::write(path.join("test_suite.log"), log)?;
    git_commit_all(path, "Add test run log")?;
    Ok(())
}

fn check_large_log_diagnosis(path: &Path) -> Result<bool, String> {
    let failed_file = path.join(".failed_test.txt");
    if !failed_file.exists() {
        return Err(".failed_test.txt was not created".to_string());
    }
    let content = std::fs::read_to_string(&failed_file).map_err(|e| e.to_string())?;
    if content.trim().contains("test_database_connection") {
        Ok(true)
    } else {
        Err(format!(
            ".failed_test.txt content '{:?}' does not contain 'test_database_connection'",
            content.trim()
        ))
    }
}

// 4. Multi-file Change
fn setup_multi_file_change(path: &Path) -> Result<()> {
    init_git_repo(path)?;
    std::fs::write(
        path.join("service.py"),
        "def get_user(id):\n    return {'id': id, 'active': True}\n",
    )?;
    std::fs::write(
        path.join("client.py"),
        "from service import get_user\n\ndef check_status(id):\n    user = get_user(id)\n    return user['active']\n",
    )?;
    git_commit_all(path, "Initial service and client")?;
    Ok(())
}

fn check_multi_file_change(path: &Path) -> Result<bool, String> {
    let service_file = path.join("service.py");
    let client_file = path.join("client.py");
    if !service_file.exists() || !client_file.exists() {
        return Err("service.py or client.py missing".to_string());
    }
    let s_content = std::fs::read_to_string(&service_file).map_err(|e| e.to_string())?;
    let c_content = std::fs::read_to_string(&client_file).map_err(|e| e.to_string())?;

    if !s_content.contains("'is_active'") && !s_content.contains("\"is_active\"") {
        return Err("service.py does not define 'is_active'".to_string());
    }
    if !c_content.contains("'is_active'") && !c_content.contains("\"is_active\"") {
        return Err("client.py does not access 'is_active'".to_string());
    }
    if c_content.contains("user['active']") || c_content.contains("user[\"active\"]") {
        return Err("client.py still references old 'active' field".to_string());
    }

    let out = Command::new("python3")
        .args(["-c", "import client; assert client.check_status(1) is True"])
        .current_dir(path)
        .output();
    if let Ok(o) = out {
        if !o.status.success() {
            return Err("Execution of client.check_status(1) failed".to_string());
        }
    }

    Ok(true)
}

// 5. Continuation Constraint
fn setup_continuation_constraint(path: &Path) -> Result<()> {
    init_git_repo(path)?;
    std::fs::write(
        path.join("config.json"),
        "{\n  \"port\": 8080,\n  \"protected\": true\n}\n",
    )?;
    std::fs::write(
        path.join("server.py"),
        "import json\n\nwith open('config.json') as f:\n    PORT = json.load(f)['port']\n",
    )?;
    git_commit_all(path, "Initial server with config")?;
    // Create baseline tag to verify files are not modified across working directory or committed history
    Command::new("git")
        .args(["tag", "benchmark_initial"])
        .current_dir(path)
        .output()
        .context("git tag failed")?;
    Ok(())
}

fn check_continuation_constraint(path: &Path) -> Result<bool, String> {
    // 1. Verify config.json was NOT modified in worktree or committed on top of initial tag
    let diff_working = Command::new("git")
        .args(["diff", "--name-only", "--", "config.json"])
        .current_dir(path)
        .output()
        .map_err(|e| e.to_string())?;
    let diff_files = String::from_utf8_lossy(&diff_working.stdout);
    if !diff_files.trim().is_empty() {
        return Err("Constraint violated: config.json was modified in working directory!".to_string());
    }

    let diff_initial = Command::new("git")
        .args(["diff", "--name-only", "benchmark_initial", "HEAD", "--", "config.json"])
        .current_dir(path)
        .output()
        .map_err(|e| e.to_string())?;
    let diff_committed = String::from_utf8_lossy(&diff_initial.stdout);
    if !diff_committed.trim().is_empty() {
        return Err("Constraint violated: config.json modification was committed!".to_string());
    }

    // Also verify content of config.json is still intact
    let config_file = path.join("config.json");
    if !config_file.exists() {
        return Err("Constraint violated: config.json was removed!".to_string());
    }
    let config_content = std::fs::read_to_string(&config_file).map_err(|e| e.to_string())?;
    if !config_content.contains("\"port\": 8080") || !config_content.contains("\"protected\": true") {
        return Err("Constraint violated: config.json content altered!".to_string());
    }

    // 2. Verify server.py uses PORT = 9090
    let server_file = path.join("server.py");
    if !server_file.exists() {
        return Err("server.py missing".to_string());
    }
    let content = std::fs::read_to_string(&server_file).map_err(|e| e.to_string())?;
    if !content.contains("9090") {
        return Err("server.py does not contain port 9090".to_string());
    }

    let out = Command::new("python3")
        .args(["-c", "import server; assert server.PORT == 9090"])
        .current_dir(path)
        .output();
    if let Ok(o) = out {
        if !o.status.success() {
            return Err("server.PORT != 9090".to_string());
        }
    }

    Ok(true)
}

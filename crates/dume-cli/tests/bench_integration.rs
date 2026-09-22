use dume_cli::bench::fixtures::all_fixtures;
use dume_cli::bench::{AttemptRecord, BenchmarkReport, BenchmarkRunConfig, BenchmarkRunner, CaseSummary};
use dume_core::types::*;
use dume_provider::types::*;
use dume_provider::AnthropicProvider;
use dume_store::HarnessStore;
use dume_worker::agent_loop::AgentLoop;
use std::sync::Arc;
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

// 1. Verify cumulative streaming usage does not repeatedly add previous turn usage
#[tokio::test]
async fn test_bench_streaming_usage_merge_cumulative() {
    let mut usage = TokenUsage::default();
    // Chunk 1 reports 10 output tokens
    let c1 = TokenUsage {
        input_tokens: 100,
        output_tokens: 10,
        total_tokens: 110,
        cache_read_tokens: Some(0),
        cache_write_tokens: Some(0),
        is_complete: true,
        raw_usage: None,
    };
    usage.merge_cumulative(&c1);
    assert_eq!(usage.output_tokens, 10);
    assert_eq!(usage.total_tokens, 110);

    // Chunk 2 reports cumulative 25 output tokens
    let c2 = TokenUsage {
        input_tokens: 100,
        output_tokens: 25,
        total_tokens: 125,
        cache_read_tokens: Some(0),
        cache_write_tokens: Some(0),
        is_complete: true,
        raw_usage: None,
    };
    usage.merge_cumulative(&c2);
    assert_eq!(usage.output_tokens, 25);
    assert_eq!(usage.total_tokens, 125);

    // Turn 2 adds 50 more input and 15 output via accumulate
    let mut turn2_usage = TokenUsage::default();
    let c3 = TokenUsage {
        input_tokens: 150,
        output_tokens: 15,
        total_tokens: 165,
        cache_read_tokens: Some(0),
        cache_write_tokens: Some(0),
        is_complete: true,
        raw_usage: None,
    };
    turn2_usage.merge_cumulative(&c3);
    usage.accumulate(&turn2_usage);
    assert_eq!(usage.input_tokens, 250);
    assert_eq!(usage.output_tokens, 40);
    assert_eq!(usage.total_tokens, 290);
}

// 2. Provider cache semantics produce correct normalized totals
#[tokio::test]
async fn test_bench_provider_cache_normalization() {
    // Anthropic normalization test
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = [0u8; 2048];
            let _ = socket.read(&mut buf).await;
            let sse_body = "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":50,\"cache_creation_input_tokens\":20,\"cache_read_input_tokens\":30,\"output_tokens\":0}}}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":15}}\n\ndata: {\"type\":\"message_stop\"}\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                sse_body.len(),
                sse_body
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });

    let mock_url = format!("http://{}", addr);
    let provider = AnthropicProvider::new("test-key").with_base_url(&mock_url);
    let (tx, mut rx) = mpsc::channel(10);
    let msgs = vec![ChatMessage::user("hello")];
    provider.stream("claude-sonnet-4-5", &msgs, &[], tx).await.unwrap();

    let mut accumulated = TokenUsage::default();
    while let Some(event) = rx.recv().await {
        if let StreamEvent::Usage(u) = event {
            accumulated.merge_cumulative(&u);
        }
    }

    // Anthropic input tokens: 50 uncached + 20 write + 30 read = 100 normalized input tokens
    assert_eq!(accumulated.input_tokens, 100);
    assert_eq!(accumulated.output_tokens, 15);
    assert_eq!(accumulated.total_tokens, 115);
    assert_eq!(accumulated.cache_read_tokens, Some(30));
    assert_eq!(accumulated.cache_write_tokens, Some(20));
    assert!(accumulated.is_complete);
}

// 3. Duplicate persistence does not duplicate aggregate usage (idempotency)
#[test]
fn test_bench_persistence_idempotency() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let artifacts_dir = dir.path().join("artifacts");
    let store = HarnessStore::open(&db_path, &artifacts_dir).unwrap();

    let usage = RequestUsage {
        session_id: "sess_1".to_string(),
        request_id: "req_dup_1".to_string(),
        model: "claude".to_string(),
        input_tokens: 100,
        output_tokens: 50,
        total_tokens: 150,
        cache_read_tokens: 10,
        cache_write_tokens: 0,
        created_at: 1000,
        benchmark_run_id: Some("bench_1".to_string()),
        case_id: Some("case_1".to_string()),
        variant: Some("baseline".to_string()),
        attempt_id: Some("att_1".to_string()),
        agent_id: Some("main".to_string()),
        parent_agent_id: None,
        raw_usage_json: None,
        is_complete: true,
    };

    // Record twice
    store.record_request_usage(&usage).unwrap();
    store.record_request_usage(&usage).unwrap();

    let records = store.get_attempt_request_usages("att_1").unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].total_tokens, 150);
}

// 4. Parent and child usage aggregate correctly
#[test]
fn test_bench_parent_child_usage_aggregation() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let artifacts_dir = dir.path().join("artifacts");
    let store = HarnessStore::open(&db_path, &artifacts_dir).unwrap();

    let parent_usage = RequestUsage {
        session_id: "sess_att_1".to_string(),
        request_id: "req_parent".to_string(),
        model: "claude".to_string(),
        input_tokens: 100,
        output_tokens: 50,
        total_tokens: 150,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        created_at: 1000,
        benchmark_run_id: Some("bench_1".to_string()),
        case_id: Some("case_1".to_string()),
        variant: Some("baseline".to_string()),
        attempt_id: Some("att_parent_child".to_string()),
        agent_id: Some("main".to_string()),
        parent_agent_id: None,
        raw_usage_json: None,
        is_complete: true,
    };

    let child_usage = RequestUsage {
        session_id: "sess_att_1".to_string(),
        request_id: "req_child".to_string(),
        model: "claude".to_string(),
        input_tokens: 200,
        output_tokens: 75,
        total_tokens: 275,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        created_at: 1005,
        benchmark_run_id: Some("bench_1".to_string()),
        case_id: Some("case_1".to_string()),
        variant: Some("baseline".to_string()),
        attempt_id: Some("att_parent_child".to_string()),
        agent_id: Some("sub_1".to_string()),
        parent_agent_id: Some("main".to_string()),
        raw_usage_json: None,
        is_complete: true,
    };

    store.record_request_usage(&parent_usage).unwrap();
    store.record_request_usage(&child_usage).unwrap();

    let usages = store.get_attempt_request_usages("att_parent_child").unwrap();
    assert_eq!(usages.len(), 2);

    let mut parent_tokens = 0;
    let mut descendant_tokens = 0;
    let mut total_tokens = 0;

    for u in &usages {
        total_tokens += u.total_tokens;
        if u.parent_agent_id.is_none() {
            parent_tokens += u.total_tokens;
        } else {
            descendant_tokens += u.total_tokens;
        }
    }

    assert_eq!(parent_tokens, 150);
    assert_eq!(descendant_tokens, 275);
    assert_eq!(total_tokens, 425);
}

// 5. Failed and cancelled requests retain available usage
#[tokio::test]
async fn test_bench_failed_cancelled_retains_usage() {
    unsafe {
        std::env::set_var("ANTHROPIC_API_KEY", "dummy_test_key");
    }
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let artifacts_dir = dir.path().join("artifacts");
    let store = Arc::new(HarnessStore::open(&db_path, &artifacts_dir).unwrap());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = [0u8; 2048];
            let _ = socket.read(&mut buf).await;
            // Send usage event and then crash/abort connection
            let sse_chunk = "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":88,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0,\"output_tokens\":0}}}\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{}",
                sse_chunk
            );
            let _ = socket.write_all(response.as_bytes()).await;
            // Socket drops
        }
    });

    let mock_url = format!("http://{}", addr);
    let agent = AgentLoop::new(dir.path(), "claude-sonnet-4-5")
        .with_base_url(&mock_url)
        .with_store(Arc::clone(&store))
        .with_request_context(StreamRequestContext {
            session_id: "sess_cancel".to_string(),
            request_id: "req_cancel_1".to_string(),
            benchmark_run_id: Some("bench_1".to_string()),
            case_id: Some("case_1".to_string()),
            variant: Some("baseline".to_string()),
            attempt_id: Some("att_cancelled".to_string()),
            agent_id: Some("main".to_string()),
            parent_agent_id: None,
        });

    let outcome = agent.run_task("do task").await;
    // Agent execution should fail due to unexpected stream EOF
    assert!(outcome.is_err());

    // Verify stored usage still retained available input tokens
    let records = store.get_attempt_request_usages("att_cancelled").unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].input_tokens, 88);
}

// 6. Missing usage marks run incomplete
#[test]
fn test_bench_missing_usage_marks_incomplete() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let artifacts_dir = dir.path().join("artifacts");
    let store = HarnessStore::open(&db_path, &artifacts_dir).unwrap();

    let incomplete_usage = RequestUsage {
        session_id: "sess_inc".to_string(),
        request_id: "req_inc".to_string(),
        model: "mock-model".to_string(),
        input_tokens: 0,
        output_tokens: 0,
        total_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        created_at: 1000,
        benchmark_run_id: Some("bench_inc".to_string()),
        case_id: Some("case_1".to_string()),
        variant: Some("baseline".to_string()),
        attempt_id: Some("att_inc".to_string()),
        agent_id: Some("main".to_string()),
        parent_agent_id: None,
        raw_usage_json: None,
        is_complete: false, // Provider did not report usage
    };

    store.record_request_usage(&incomplete_usage).unwrap();
    let records = store.get_attempt_request_usages("att_inc").unwrap();
    assert_eq!(records.len(), 1);
    assert!(!records[0].is_complete);
}

// 7. Acceptance check rejects incorrect results even if agent claims completed
#[test]
fn test_bench_acceptance_check_rejects_false_completion() {
    let fixtures = all_fixtures();
    let targeted_fix = fixtures.iter().find(|f| f.id == "targeted_bug_fix").unwrap();

    let dir = tempdir().unwrap();
    // Setup initial broken fixture
    (targeted_fix.setup_fn)(dir.path()).unwrap();

    // Acceptance check should fail on unmodified directory
    let check_before = (targeted_fix.check_fn)(dir.path());
    assert!(check_before.is_err() || check_before.unwrap() == false);

    // Apply incorrect fix
    std::fs::write(dir.path().join("calc.py"), "def add(a, b):\n    return a - b\n").unwrap();
    let check_wrong = (targeted_fix.check_fn)(dir.path());
    assert!(check_wrong.is_err() || check_wrong.unwrap() == false);

    // Apply correct fix
    std::fs::write(dir.path().join("calc.py"), "def add(a, b):\n    return a + b\n").unwrap();
    let check_correct = (targeted_fix.check_fn)(dir.path()).unwrap();
    assert!(check_correct);
}

// 8. Both variants receive identical initial fixtures
#[test]
fn test_bench_identical_initial_fixtures() {
    let fixtures = all_fixtures();
    for f in &fixtures {
        let dir1 = tempdir().unwrap();
        let dir2 = tempdir().unwrap();

        (f.setup_fn)(dir1.path()).unwrap();
        (f.setup_fn)(dir2.path()).unwrap();

        // Read all non-.git files in dir1 and compare with dir2
        for entry in walkdir(dir1.path()) {
            let rel_path = entry.strip_prefix(dir1.path()).unwrap();
            // Skip .git internal files which have commit timestamps
            if rel_path.starts_with(".git") {
                continue;
            }
            let other_file = dir2.path().join(rel_path);
            assert!(other_file.exists(), "File missing in second setup: {:?}", rel_path);
            if entry.is_file() {
                let content1 = std::fs::read(&entry).unwrap();
                let content2 = std::fs::read(&other_file).unwrap();
                assert_eq!(content1, content2, "Fixture contents differed for case: {}", f.id);
            }
        }
    }
}

fn walkdir(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                files.extend(walkdir(&p));
            } else {
                files.push(p);
            }
        }
    }
    files
}

// 9. Known synthetic totals produce expected reduction percentages
// 10. Zero successful runs do not cause invalid arithmetic (NaN or panic)
#[test]
fn test_bench_metrics_formulas_and_zero_division() {
    let dir = tempdir().unwrap();
    let artifacts_dir = dir.path().join("artifacts");
    let _store = Arc::new(HarnessStore::open(&dir.path().join("db.sqlite"), &artifacts_dir).unwrap());
    let config = BenchmarkRunConfig {
        run_id: "test_calc".to_string(),
        case_ids: vec![],
        baseline_bin: None,
        candidate_bin: None,
        repetitions: 1,
        model: "test-model".to_string(),
        base_url: None,
        output_dir: dir.path().to_path_buf(),
        db_path: dir.path().join("db.sqlite"),
    };
    let runner = BenchmarkRunner::new(config).unwrap();

    // Scenario A: Standard 25% reduction with successful runs
    let base_att = AttemptRecord {
        run_id: "r1".to_string(),
        case_id: "c1".to_string(),
        variant: "baseline".to_string(),
        repetition: 1,
        attempt_id: "a1".to_string(),
        session_id: "s1".to_string(),
        success: true,
        failure_reason: None,
        input_tokens: 800,
        output_tokens: 200,
        total_tokens: 1000,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        is_usage_complete: true,
        duration_ms: 100,
        request_count: 1,
        parent_tokens: 1000,
        descendant_tokens: 0,
    };

    let cand_att = AttemptRecord {
        run_id: "r1".to_string(),
        case_id: "c1".to_string(),
        variant: "candidate".to_string(),
        repetition: 1,
        attempt_id: "a2".to_string(),
        session_id: "s2".to_string(),
        success: true,
        failure_reason: None,
        input_tokens: 600,
        output_tokens: 150,
        total_tokens: 750,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        is_usage_complete: true,
        duration_ms: 80,
        request_count: 1,
        parent_tokens: 750,
        descendant_tokens: 0,
    };

    let report_a = runner.build_report(
        vec![BenchmarkRunner::calculate_case_summary("c1", vec![base_att], vec![cand_att])],
        vec!["baseline".to_string(), "candidate".to_string()],
    );

    assert_eq!(report_a.overall_baseline_tokens, 1000);
    assert_eq!(report_a.overall_candidate_tokens, 750);
    assert!((report_a.overall_token_reduction - 0.25).abs() < 1e-6);
    assert_eq!(report_a.overall_baseline_tokens_per_success, 1000.0);
    assert_eq!(report_a.overall_candidate_tokens_per_success, 750.0);

    // Scenario B: Zero successful runs
    let base_failed = AttemptRecord {
        run_id: "r2".to_string(),
        case_id: "c2".to_string(),
        variant: "baseline".to_string(),
        repetition: 1,
        attempt_id: "a3".to_string(),
        session_id: "s3".to_string(),
        success: false,
        failure_reason: Some("test failed".to_string()),
        input_tokens: 500,
        output_tokens: 50,
        total_tokens: 550,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        is_usage_complete: true,
        duration_ms: 100,
        request_count: 1,
        parent_tokens: 550,
        descendant_tokens: 0,
    };

    let cand_failed = AttemptRecord {
        run_id: "r2".to_string(),
        case_id: "c2".to_string(),
        variant: "candidate".to_string(),
        repetition: 1,
        attempt_id: "a4".to_string(),
        session_id: "s4".to_string(),
        success: false,
        failure_reason: Some("test failed".to_string()),
        input_tokens: 400,
        output_tokens: 40,
        total_tokens: 440,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        is_usage_complete: true,
        duration_ms: 80,
        request_count: 1,
        parent_tokens: 440,
        descendant_tokens: 0,
    };

    let report_b = runner.build_report(
        vec![BenchmarkRunner::calculate_case_summary("c2", vec![base_failed], vec![cand_failed])],
        vec!["baseline".to_string(), "candidate".to_string()],
    );

    assert_eq!(report_b.overall_baseline_success_rate, 0.0);
    assert_eq!(report_b.overall_candidate_success_rate, 0.0);
    assert_eq!(report_b.overall_baseline_tokens_per_success, 0.0);
    assert_eq!(report_b.overall_candidate_tokens_per_success, 0.0);
    assert!(!report_b.overall_baseline_tokens_per_success.is_nan());
    assert!(!report_b.overall_candidate_tokens_per_success.is_nan());
}

// 11. Saved results can be loaded and reported after process restart
#[test]
fn test_bench_persistence_and_markdown_rendering() {
    let dir = tempdir().unwrap();
    let json_path = dir.path().join("report_test.json");

    let report = BenchmarkReport {
        run_id: "bench_persist".to_string(),
        model: "anthropic/claude-sonnet-4-5".to_string(),
        created_at: 1720000000,
        cases: vec![CaseSummary {
            case_id: "targeted_bug_fix".to_string(),
            baseline_success_count: 1,
            baseline_total_count: 1,
            baseline_success_rate: 1.0,
            candidate_success_count: 1,
            candidate_total_count: 1,
            candidate_success_rate: 1.0,
            baseline_total_tokens: 2000,
            candidate_total_tokens: 1500,
            baseline_tokens_per_success: 2000.0,
            candidate_tokens_per_success: 1500.0,
            token_reduction: 0.25,
            baseline_attempts: vec![],
            candidate_attempts: vec![],
        }],
        overall_baseline_tokens: 2000,
        overall_candidate_tokens: 1500,
        overall_baseline_success_rate: 1.0,
        overall_candidate_success_rate: 1.0,
        overall_token_reduction: 0.25,
        overall_baseline_tokens_per_success: 2000.0,
        overall_candidate_tokens_per_success: 1500.0,
        incomplete_attempts_count: 0,
        execution_order: vec!["c1:rep1:baseline".to_string(), "c1:rep1:candidate".to_string()],
    };

    // Save JSON
    std::fs::write(&json_path, serde_json::to_string_pretty(&report).unwrap()).unwrap();

    // Reload JSON from disk simulating process restart
    let loaded: BenchmarkReport = serde_json::from_str(&std::fs::read_to_string(&json_path).unwrap()).unwrap();
    assert_eq!(loaded.run_id, "bench_persist");
    assert_eq!(loaded.overall_token_reduction, 0.25);

    // Render markdown
    let config = BenchmarkRunConfig {
        run_id: "bench_persist".to_string(),
        case_ids: vec![],
        baseline_bin: None,
        candidate_bin: None,
        repetitions: 1,
        model: "anthropic/claude-sonnet-4-5".to_string(),
        base_url: None,
        output_dir: dir.path().to_path_buf(),
        db_path: dir.path().join("db.sqlite"),
    };
    let runner = BenchmarkRunner::new(config).unwrap();
    let md = runner.render_markdown_report(&loaded);

    assert!(md.contains("# DUM-E Benchmark Report: bench_persist"));
    assert!(md.contains("25.00%"));
    assert!(md.contains("targeted_bug_fix"));
}

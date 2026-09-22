pub mod fixtures;

use anyhow::{Context, Result};
use dume_store::HarnessStore;
use fixtures::{all_fixtures, BenchmarkFixture};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkRunConfig {
    pub run_id: String,
    pub case_ids: Vec<String>,
    pub baseline_bin: Option<PathBuf>,
    pub candidate_bin: Option<PathBuf>,
    pub repetitions: usize,
    pub model: String,
    pub base_url: Option<String>,
    pub output_dir: PathBuf,
    pub db_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptRecord {
    pub run_id: String,
    pub case_id: String,
    pub variant: String,
    pub repetition: usize,
    pub attempt_id: String,
    pub session_id: String,
    pub success: bool,
    pub failure_reason: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub is_usage_complete: bool,
    pub duration_ms: u128,
    pub request_count: usize,
    pub parent_tokens: i64,
    pub descendant_tokens: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaseSummary {
    pub case_id: String,
    pub baseline_success_count: usize,
    pub baseline_total_count: usize,
    pub baseline_success_rate: f64,
    pub candidate_success_count: usize,
    pub candidate_total_count: usize,
    pub candidate_success_rate: f64,
    pub baseline_total_tokens: i64,
    pub candidate_total_tokens: i64,
    pub baseline_tokens_per_success: f64,
    pub candidate_tokens_per_success: f64,
    pub token_reduction: f64,
    pub baseline_attempts: Vec<AttemptRecord>,
    pub candidate_attempts: Vec<AttemptRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkReport {
    pub run_id: String,
    pub model: String,
    pub created_at: i64,
    pub cases: Vec<CaseSummary>,
    pub overall_baseline_tokens: i64,
    pub overall_candidate_tokens: i64,
    pub overall_baseline_success_rate: f64,
    pub overall_candidate_success_rate: f64,
    pub overall_token_reduction: f64,
    pub overall_baseline_tokens_per_success: f64,
    pub overall_candidate_tokens_per_success: f64,
    pub incomplete_attempts_count: usize,
    pub execution_order: Vec<String>,
}

pub struct BenchmarkRunner {
    config: BenchmarkRunConfig,
    store: Arc<HarnessStore>,
}

impl BenchmarkRunner {
    pub fn new(config: BenchmarkRunConfig) -> Result<Self> {
        let artifacts_dir = config.output_dir.join("artifacts");
        let store = Arc::new(HarnessStore::open(&config.db_path, &artifacts_dir)?);
        Ok(Self { config, store })
    }

    pub async fn run(&self) -> Result<BenchmarkReport> {
        std::fs::create_dir_all(&self.config.output_dir)?;
        let fixtures = all_fixtures();
        let selected_fixtures: Vec<BenchmarkFixture> = if self.config.case_ids.is_empty() {
            fixtures
        } else {
            fixtures
                .into_iter()
                .filter(|f| self.config.case_ids.contains(&f.id.to_string()))
                .collect()
        };

        anyhow::ensure!(!selected_fixtures.is_empty(), "No matching benchmark fixtures found");

        let mut all_case_summaries = Vec::new();
        let mut execution_order = Vec::new();

        for fixture in &selected_fixtures {
            let mut baseline_records = Vec::new();
            let mut candidate_records = Vec::new();

            for rep in 0..self.config.repetitions {
                // Alternate execution order to avoid ordering bias / cache warming bias
                let variants = if rep % 2 == 0 {
                    vec!["baseline", "candidate"]
                } else {
                    vec!["candidate", "baseline"]
                };

                for variant in variants {
                    let order_tag = format!("{}:{}:rep_{}", fixture.id, variant, rep);
                    execution_order.push(order_tag);

                    let record = self.execute_variant_attempt(fixture, variant, rep).await?;
                    if variant == "baseline" {
                        baseline_records.push(record);
                    } else {
                        candidate_records.push(record);
                    }
                }
            }

            let summary = Self::calculate_case_summary(fixture.id, baseline_records, candidate_records);
            all_case_summaries.push(summary);
        }

        let report = self.build_report(all_case_summaries, execution_order);
        self.save_reports(&report)?;

        Ok(report)
    }

    async fn execute_variant_attempt(
        &self,
        fixture: &BenchmarkFixture,
        variant: &str,
        repetition: usize,
    ) -> Result<AttemptRecord> {
        let temp_dir = tempfile::tempdir().context("Failed to create temporary worktree for benchmark")?;
        let worktree_path = temp_dir.path();

        // 1. Fresh repository fixture setup
        (fixture.setup_fn)(worktree_path).context("Fixture setup failed")?;

        let attempt_id = format!(
            "att_{}_{}_{}_{}",
            self.config.run_id, fixture.id, variant, repetition
        );
        let session_id = format!(
            "sess_{}_{}_{}_{}",
            self.config.run_id, fixture.id, variant, repetition
        );

        let bin_path = if variant == "baseline" {
            self.config.baseline_bin.clone()
        } else {
            self.config.candidate_bin.clone()
        };

        let start_time = Instant::now();

        // 2. Execute through real agent path (either binary subprocess or internal AgentLoop)
        let agent_error = if let Some(bin) = bin_path {
            self.run_via_binary(
                &bin,
                &attempt_id,
                worktree_path,
                fixture.task_prompt,
                variant,
                repetition,
            )
            .await
        } else {
            self.run_via_agent_loop(
                &attempt_id,
                &session_id,
                worktree_path,
                fixture.task_prompt,
                variant,
                repetition,
            )
            .await
        };

        let duration_ms = start_time.elapsed().as_millis();

        // 3. Independent acceptance check executed OUTSIDE the agent editable files
        let check_res = (fixture.check_fn)(worktree_path);
        let (success, failure_reason) = match check_res {
            Ok(true) => (true, None),
            Ok(false) => (false, Some("Acceptance check returned false".to_string())),
            Err(err) => (false, Some(err)),
        };

        // If acceptance check passed but agent crashed with hard error, consider failure reason
        let failure_reason = if !success {
            failure_reason.or_else(|| agent_error.map(|e| e.to_string()))
        } else {
            None
        };

        // 4. Retrieve usage records from database
        let usages = self.store.get_attempt_request_usages(&attempt_id).unwrap_or_default();

        let mut input_tokens = 0;
        let mut output_tokens = 0;
        let mut total_tokens = 0;
        let mut cache_read_tokens = 0;
        let mut cache_write_tokens = 0;
        let mut is_usage_complete = !usages.is_empty();
        let mut parent_tokens = 0;
        let mut descendant_tokens = 0;

        for u in &usages {
            input_tokens += u.input_tokens;
            output_tokens += u.output_tokens;
            total_tokens += u.total_tokens;
            cache_read_tokens += u.cache_read_tokens;
            cache_write_tokens += u.cache_write_tokens;
            if !u.is_complete {
                is_usage_complete = false;
            }
            if u.parent_agent_id.is_none() {
                parent_tokens += u.total_tokens;
            } else {
                descendant_tokens += u.total_tokens;
            }
        }

        Ok(AttemptRecord {
            run_id: self.config.run_id.clone(),
            case_id: fixture.id.to_string(),
            variant: variant.to_string(),
            repetition,
            attempt_id,
            session_id,
            success,
            failure_reason,
            input_tokens,
            output_tokens,
            total_tokens,
            cache_read_tokens,
            cache_write_tokens,
            is_usage_complete,
            duration_ms,
            request_count: usages.len(),
            parent_tokens,
            descendant_tokens,
        })
    }

    async fn run_via_agent_loop(
        &self,
        attempt_id: &str,
        session_id: &str,
        worktree_path: &Path,
        prompt: &str,
        variant: &str,
        _repetition: usize,
    ) -> Option<anyhow::Error> {
        let mut agent = dume_worker::AgentLoop::new(worktree_path, &self.config.model);
        if let Some(base_url) = &self.config.base_url {
            agent = agent.with_base_url(base_url);
        }
        let ctx = dume_provider::types::StreamRequestContext {
            session_id: session_id.to_string(),
            request_id: format!("req_{}_0", attempt_id),
            benchmark_run_id: Some(self.config.run_id.clone()),
            case_id: None,
            variant: Some(variant.to_string()),
            attempt_id: Some(attempt_id.to_string()),
            agent_id: Some("main".to_string()),
            parent_agent_id: None,
        };
        agent = agent.with_store(Arc::clone(&self.store)).with_request_context(ctx);

        match agent.run_task(prompt).await {
            Ok(_) => None,
            Err(e) => Some(e),
        }
    }

    async fn run_via_binary(
        &self,
        bin: &Path,
        attempt_id: &str,
        worktree_path: &Path,
        prompt: &str,
        variant: &str,
        _repetition: usize,
    ) -> Option<anyhow::Error> {
        let mut cmd = tokio::process::Command::new(bin);
        cmd.args([
            "worker",
            "--attempt-id",
            attempt_id,
            "--worktree-path",
            worktree_path.to_str().unwrap(),
            "--task-prompt",
            prompt,
            "--model",
            &self.config.model,
            "--db-path",
            self.config.db_path.to_str().unwrap(),
            "--benchmark-run-id",
            &self.config.run_id,
            "--variant",
            variant,
        ]);
        if let Some(base_url) = &self.config.base_url {
            cmd.args(["--base-url", base_url]);
        }

        match cmd.output().await {
            Ok(output) if output.status.success() => None,
            Ok(output) => {
                let err = String::from_utf8_lossy(&output.stderr);
                Some(anyhow::anyhow!("Binary exit failure: {}", err))
            }
            Err(e) => Some(e.into()),
        }
    }

    pub fn calculate_case_summary(
        case_id: &str,
        baseline_attempts: Vec<AttemptRecord>,
        candidate_attempts: Vec<AttemptRecord>,
    ) -> CaseSummary {
        let baseline_total_count = baseline_attempts.len();
        let baseline_success_count = baseline_attempts.iter().filter(|a| a.success).count();
        let baseline_success_rate = if baseline_total_count > 0 {
            baseline_success_count as f64 / baseline_total_count as f64
        } else {
            0.0
        };

        let candidate_total_count = candidate_attempts.len();
        let candidate_success_count = candidate_attempts.iter().filter(|a| a.success).count();
        let candidate_success_rate = if candidate_total_count > 0 {
            candidate_success_count as f64 / candidate_total_count as f64
        } else {
            0.0
        };

        let baseline_total_tokens: i64 = baseline_attempts.iter().map(|a| a.total_tokens).sum();
        let candidate_total_tokens: i64 = candidate_attempts.iter().map(|a| a.total_tokens).sum();

        let baseline_tokens_per_success = if baseline_success_count > 0 {
            baseline_total_tokens as f64 / baseline_success_count as f64
        } else {
            0.0
        };

        let candidate_tokens_per_success = if candidate_success_count > 0 {
            candidate_total_tokens as f64 / candidate_success_count as f64
        } else {
            0.0
        };

        let token_reduction = if baseline_total_tokens > 0 {
            1.0 - (candidate_total_tokens as f64 / baseline_total_tokens as f64)
        } else {
            0.0
        };

        CaseSummary {
            case_id: case_id.to_string(),
            baseline_success_count,
            baseline_total_count,
            baseline_success_rate,
            candidate_success_count,
            candidate_total_count,
            candidate_success_rate,
            baseline_total_tokens,
            candidate_total_tokens,
            baseline_tokens_per_success,
            candidate_tokens_per_success,
            token_reduction,
            baseline_attempts,
            candidate_attempts,
        }
    }

    pub fn build_report(&self, cases: Vec<CaseSummary>, execution_order: Vec<String>) -> BenchmarkReport {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        let mut overall_baseline_tokens = 0;
        let mut overall_candidate_tokens = 0;
        let mut overall_baseline_successes = 0;
        let mut overall_baseline_attempts = 0;
        let mut overall_candidate_successes = 0;
        let mut overall_candidate_attempts = 0;
        let mut incomplete_attempts_count = 0;

        for c in &cases {
            overall_baseline_tokens += c.baseline_total_tokens;
            overall_candidate_tokens += c.candidate_total_tokens;
            overall_baseline_successes += c.baseline_success_count;
            overall_baseline_attempts += c.baseline_total_count;
            overall_candidate_successes += c.candidate_success_count;
            overall_candidate_attempts += c.candidate_total_count;

            for a in &c.baseline_attempts {
                if !a.is_usage_complete {
                    incomplete_attempts_count += 1;
                }
            }
            for a in &c.candidate_attempts {
                if !a.is_usage_complete {
                    incomplete_attempts_count += 1;
                }
            }
        }

        let overall_baseline_success_rate = if overall_baseline_attempts > 0 {
            overall_baseline_successes as f64 / overall_baseline_attempts as f64
        } else {
            0.0
        };

        let overall_candidate_success_rate = if overall_candidate_attempts > 0 {
            overall_candidate_successes as f64 / overall_candidate_attempts as f64
        } else {
            0.0
        };

        let overall_token_reduction = if overall_baseline_tokens > 0 {
            1.0 - (overall_candidate_tokens as f64 / overall_baseline_tokens as f64)
        } else {
            0.0
        };

        let overall_baseline_tokens_per_success = if overall_baseline_successes > 0 {
            overall_baseline_tokens as f64 / overall_baseline_successes as f64
        } else {
            0.0
        };

        let overall_candidate_tokens_per_success = if overall_candidate_successes > 0 {
            overall_candidate_tokens as f64 / overall_candidate_successes as f64
        } else {
            0.0
        };

        BenchmarkReport {
            run_id: self.config.run_id.clone(),
            model: self.config.model.clone(),
            created_at: now,
            cases,
            overall_baseline_tokens,
            overall_candidate_tokens,
            overall_baseline_success_rate,
            overall_candidate_success_rate,
            overall_token_reduction,
            overall_baseline_tokens_per_success,
            overall_candidate_tokens_per_success,
            incomplete_attempts_count,
            execution_order,
        }
    }

    fn save_reports(&self, report: &BenchmarkReport) -> Result<()> {
        let json_path = self.config.output_dir.join(format!("report_{}.json", report.run_id));
        let md_path = self.config.output_dir.join(format!("report_{}.md", report.run_id));

        let json_content = serde_json::to_string_pretty(report)?;
        std::fs::write(&json_path, json_content)?;

        let md_content = self.render_markdown_report(report);
        std::fs::write(&md_path, md_content)?;

        Ok(())
    }

    pub fn render_markdown_report(&self, report: &BenchmarkReport) -> String {
        let mut md = String::new();
        md.push_str(&format!("# DUM-E Benchmark Report: {}\n\n", report.run_id));
        md.push_str(&format!("- **Model**: `{}`\n", report.model));
        md.push_str(&format!("- **Incomplete Measurements**: {}\n", report.incomplete_attempts_count));
        md.push_str(&format!(
            "- **Overall Token Reduction**: {:.2}%\n",
            report.overall_token_reduction * 100.0
        ));
        md.push_str(&format!(
            "- **Baseline Success Rate**: {:.1}% (Tokens/Success: {:.0})\n",
            report.overall_baseline_success_rate * 100.0,
            report.overall_baseline_tokens_per_success
        ));
        md.push_str(&format!(
            "- **Candidate Success Rate**: {:.1}% (Tokens/Success: {:.0})\n\n",
            report.overall_candidate_success_rate * 100.0,
            report.overall_candidate_tokens_per_success
        ));

        md.push_str("## Per-Case Results\n\n");
        md.push_str("| Case ID | Baseline Success | Candidate Success | Baseline Tokens | Candidate Tokens | Reduction | Base Tokens/Succ | Cand Tokens/Succ |\n");
        md.push_str("|---------|------------------|-------------------|-----------------|------------------|-----------|------------------|------------------|\n");

        for c in &report.cases {
            md.push_str(&format!(
                "| `{}` | {:.1}% ({}/{}) | {:.1}% ({}/{}) | {} | {} | {:.2}% | {:.0} | {:.0} |\n",
                c.case_id,
                c.baseline_success_rate * 100.0,
                c.baseline_success_count,
                c.baseline_total_count,
                c.candidate_success_rate * 100.0,
                c.candidate_success_count,
                c.candidate_total_count,
                c.baseline_total_tokens,
                c.candidate_total_tokens,
                c.token_reduction * 100.0,
                c.baseline_tokens_per_success,
                c.candidate_tokens_per_success,
            ));
        }

        md.push_str("\n## Execution Order\n\n");
        for (i, tag) in report.execution_order.iter().enumerate() {
            md.push_str(&format!("{}. `{}`\n", i + 1, tag));
        }

        md
    }
}

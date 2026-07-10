//! In-process job scheduler for deferred and recurring command execution.
//!
//! Lets an agent schedule an allowlisted command to run once after a delay or
//! repeatedly on an interval, then poll the captured results later — useful for
//! "kick off a build and check back" or "poll status every N seconds" flows
//! without holding a tool call open.
//!
//! A single [`SchedulerManager`] is shared via [`crate::ToolServices`]. Each job
//! captures the scheduling call's permission policy and working directory, so
//! scheduled commands run under exactly the same sandbox as a direct call.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use nebula_mcp_core::{Metrics, ToolContext};
use parking_lot::Mutex;
use serde::Serialize;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::common::exec::{run_raw, CommandSpec};

const MAX_RESULTS_PER_JOB: usize = 20;

/// A single captured run of a scheduled job.
#[derive(Debug, Clone, Serialize)]
pub struct RunResult {
    /// Unix timestamp (seconds) when the run completed.
    pub at_unix: i64,
    /// Exit code, or null if terminated by signal / not run.
    pub exit_code: Option<i32>,
    /// Whether the run succeeded (exit 0).
    pub success: bool,
    /// Duration in milliseconds.
    pub duration_ms: u128,
    /// Captured stdout (truncated to policy limits).
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// Error text if the run could not be executed.
    pub error: Option<String>,
}

/// Public metadata for a scheduled job.
#[derive(Debug, Clone, Serialize)]
pub struct JobInfo {
    /// Job identifier.
    pub id: String,
    /// `once` or `interval`.
    pub kind: &'static str,
    /// Program being run.
    pub program: String,
    /// Arguments.
    pub args: Vec<String>,
    /// Interval in seconds for recurring jobs.
    pub interval_secs: Option<u64>,
    /// Seconds since the job was created.
    pub age_secs: u64,
    /// Number of completed runs.
    pub runs: u64,
    /// Whether the job is still active.
    pub active: bool,
}

struct Job {
    id: String,
    kind: &'static str,
    program: String,
    args: Vec<String>,
    interval_secs: Option<u64>,
    created: Instant,
    runs: Arc<AtomicU64>,
    active: Arc<AtomicBool>,
    results: Arc<Mutex<VecDeque<RunResult>>>,
    cancel: CancellationToken,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl Job {
    fn info(&self) -> JobInfo {
        JobInfo {
            id: self.id.clone(),
            kind: self.kind,
            program: self.program.clone(),
            args: self.args.clone(),
            interval_secs: self.interval_secs,
            age_secs: self.created.elapsed().as_secs(),
            runs: self.runs.load(Ordering::Relaxed),
            active: self.active.load(Ordering::Relaxed),
        }
    }
}

/// The captured execution environment a scheduled command runs under.
#[derive(Clone)]
pub struct JobEnv {
    ctx: ToolContext,
}

impl JobEnv {
    /// Capture the environment from a scheduling call's context.
    #[must_use]
    pub fn from_context(ctx: &ToolContext) -> Self {
        Self { ctx: ctx.clone() }
    }
}

/// Manages scheduled jobs.
#[derive(Default)]
pub struct SchedulerManager {
    jobs: DashMap<String, Arc<Job>>,
}

impl SchedulerManager {
    /// Create an empty scheduler.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Schedule a job. `interval_secs` = None runs once after `delay_secs`;
    /// otherwise the command repeats every `interval_secs` (after an initial
    /// `delay_secs`).
    pub fn schedule(
        &self,
        env: JobEnv,
        program: String,
        args: Vec<String>,
        delay_secs: u64,
        interval_secs: Option<u64>,
    ) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let runs = Arc::new(AtomicU64::new(0));
        let active = Arc::new(AtomicBool::new(true));
        let results = Arc::new(Mutex::new(VecDeque::new()));
        let cancel = CancellationToken::new();

        let job = Arc::new(Job {
            id: id.clone(),
            kind: if interval_secs.is_some() {
                "interval"
            } else {
                "once"
            },
            program: program.clone(),
            args: args.clone(),
            interval_secs,
            created: Instant::now(),
            runs: runs.clone(),
            active: active.clone(),
            results: results.clone(),
            cancel: cancel.clone(),
            handle: Mutex::new(None),
        });

        let base_ctx = env.ctx.clone();
        let metrics = base_ctx.metrics.clone();
        let handle = tokio::spawn(async move {
            // Initial delay (interruptible).
            if delay_secs > 0 {
                tokio::select! {
                    () = cancel.cancelled() => { active.store(false, Ordering::Relaxed); return; }
                    () = tokio::time::sleep(Duration::from_secs(delay_secs)) => {}
                }
            }
            loop {
                if cancel.is_cancelled() {
                    break;
                }
                let result = run_once(&base_ctx, &program, &args, &cancel, &metrics).await;
                runs.fetch_add(1, Ordering::Relaxed);
                {
                    let mut guard = results.lock();
                    if guard.len() >= MAX_RESULTS_PER_JOB {
                        guard.pop_front();
                    }
                    guard.push_back(result);
                }
                match interval_secs {
                    None => break,
                    Some(iv) => {
                        tokio::select! {
                            () = cancel.cancelled() => break,
                            () = tokio::time::sleep(Duration::from_secs(iv.max(1))) => {}
                        }
                    }
                }
            }
            active.store(false, Ordering::Relaxed);
        });
        *job.handle.lock() = Some(handle);
        self.jobs.insert(id.clone(), job);
        id
    }

    /// List all jobs.
    #[must_use]
    pub fn list(&self) -> Vec<JobInfo> {
        let mut v: Vec<JobInfo> = self.jobs.iter().map(|kv| kv.value().info()).collect();
        v.sort_by_key(|j| j.age_secs);
        v
    }

    /// Fetch a job's captured results (does not clear them).
    pub fn results(&self, id: &str) -> Option<(JobInfo, Vec<RunResult>)> {
        let job = self.jobs.get(id)?;
        let info = job.info();
        let results = job.results.lock().iter().cloned().collect();
        Some((info, results))
    }

    /// Cancel a job. Returns false if unknown.
    pub fn cancel(&self, id: &str) -> bool {
        if let Some(job) = self.jobs.get(id) {
            job.cancel.cancel();
            if let Some(h) = job.handle.lock().take() {
                h.abort();
            }
            job.active.store(false, Ordering::Relaxed);
            true
        } else {
            false
        }
    }
}

/// Execute one run of a scheduled command under the captured context.
async fn run_once(
    base_ctx: &ToolContext,
    program: &str,
    args: &[String],
    cancel: &CancellationToken,
    _metrics: &Metrics,
) -> RunResult {
    // Fresh context sharing policy/workdir but with the job's cancel token.
    let mut ctx = base_ctx.clone();
    ctx.cancel = cancel.clone();
    ctx.progress = None;

    let spec = CommandSpec::new(program, ctx.working_dir.clone(), &ctx).args(args.to_vec());
    let timeout = ctx.policy.timeout();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    match run_raw(&ctx, spec, timeout).await {
        Ok(r) => RunResult {
            at_unix: now,
            exit_code: r.code,
            success: r.success(),
            duration_ms: r.duration.as_millis(),
            stdout: r.stdout,
            stderr: r.stderr,
            error: None,
        },
        Err(e) => RunResult {
            at_unix: now,
            exit_code: None,
            success: false,
            duration_ms: 0,
            stdout: String::new(),
            stderr: String::new(),
            error: Some(e.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_mcp_core::config::SecurityConfig;
    use nebula_mcp_core::security::EffectivePolicy;

    fn ctx() -> ToolContext {
        let base = SecurityConfig {
            allowed_commands: vec!["echo".into()],
            allowed_paths: vec!["/**".into()],
            default_timeout_secs: 10,
            max_runtime_secs: 30,
            max_output_bytes: 1 << 16,
            ..Default::default()
        };
        let policy = EffectivePolicy::build("sched", &base, None).unwrap();
        ToolContext {
            policy: Arc::new(policy),
            working_dir: std::env::temp_dir(),
            cancel: CancellationToken::new(),
            metrics: Metrics::new(),
            config: Arc::new(Default::default()),
            request_id: "r".into(),
            progress: None,
        }
    }

    #[tokio::test]
    async fn once_job_runs_and_captures_output() {
        if which::which("echo").is_err() {
            return;
        }
        let mgr = SchedulerManager::new();
        let id = mgr.schedule(
            JobEnv::from_context(&ctx()),
            "echo".into(),
            vec!["scheduled".into()],
            0,
            None,
        );
        // Poll for completion.
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Some((info, results)) = mgr.results(&id) {
                if info.runs >= 1 && !results.is_empty() {
                    assert!(results[0].stdout.contains("scheduled"));
                    assert!(results[0].success);
                    return;
                }
            }
        }
        panic!("scheduled job did not run in time");
    }

    #[tokio::test]
    async fn cancel_stops_interval_job() {
        let mgr = SchedulerManager::new();
        let id = mgr.schedule(
            JobEnv::from_context(&ctx()),
            "echo".into(),
            vec!["tick".into()],
            0,
            Some(1),
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(mgr.cancel(&id));
        assert!(!mgr.cancel("nope"));
    }
}

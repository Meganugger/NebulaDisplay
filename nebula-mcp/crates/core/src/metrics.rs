//! Lightweight, lock-free per-tool metrics.
//!
//! Metrics are kept in a [`DashMap`] keyed by tool name and updated with atomic
//! counters, so recording is cheap even under heavy concurrency. The server
//! periodically logs a snapshot when `logging.emit_metrics` is enabled and can
//! expose the same data via a diagnostics tool.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use serde::Serialize;

/// Atomic counters for a single tool.
#[derive(Debug, Default)]
struct ToolCounters {
    calls: AtomicU64,
    successes: AtomicU64,
    failures: AtomicU64,
    cancellations: AtomicU64,
    total_duration_us: AtomicU64,
    max_duration_us: AtomicU64,
    output_bytes: AtomicU64,
}

/// A point-in-time, serialisable snapshot of one tool's metrics.
#[derive(Debug, Clone, Serialize)]
pub struct ToolMetricsSnapshot {
    /// Tool name.
    pub tool: String,
    /// Total invocations.
    pub calls: u64,
    /// Invocations that returned success.
    pub successes: u64,
    /// Invocations that returned an error.
    pub failures: u64,
    /// Invocations cancelled or timed out.
    pub cancellations: u64,
    /// Mean duration across all completed calls, in microseconds.
    pub mean_duration_us: u64,
    /// Worst observed duration, in microseconds.
    pub max_duration_us: u64,
    /// Total captured output bytes.
    pub output_bytes: u64,
}

/// Shared metrics registry.
#[derive(Clone, Default)]
pub struct Metrics {
    tools: Arc<DashMap<String, ToolCounters>>,
}

impl Metrics {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a completed tool call.
    pub fn record(
        &self,
        tool: &str,
        outcome: Outcome,
        duration: std::time::Duration,
        output_bytes: u64,
    ) {
        let entry = self.tools.entry(tool.to_string()).or_default();
        entry.calls.fetch_add(1, Ordering::Relaxed);
        match outcome {
            Outcome::Success => {
                entry.successes.fetch_add(1, Ordering::Relaxed);
            }
            Outcome::Failure => {
                entry.failures.fetch_add(1, Ordering::Relaxed);
            }
            Outcome::Cancelled => {
                entry.cancellations.fetch_add(1, Ordering::Relaxed);
            }
        }
        let us = duration.as_micros() as u64;
        entry.total_duration_us.fetch_add(us, Ordering::Relaxed);
        entry
            .output_bytes
            .fetch_add(output_bytes, Ordering::Relaxed);
        // Update max via compare-and-swap loop.
        let mut cur = entry.max_duration_us.load(Ordering::Relaxed);
        while us > cur {
            match entry.max_duration_us.compare_exchange_weak(
                cur,
                us,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => cur = observed,
            }
        }
    }

    /// Snapshot metrics for every tool, sorted by name.
    #[must_use]
    pub fn snapshot(&self) -> Vec<ToolMetricsSnapshot> {
        let mut out: Vec<ToolMetricsSnapshot> = self
            .tools
            .iter()
            .map(|kv| {
                let c = kv.value();
                let calls = c.calls.load(Ordering::Relaxed);
                let total = c.total_duration_us.load(Ordering::Relaxed);
                ToolMetricsSnapshot {
                    tool: kv.key().clone(),
                    calls,
                    successes: c.successes.load(Ordering::Relaxed),
                    failures: c.failures.load(Ordering::Relaxed),
                    cancellations: c.cancellations.load(Ordering::Relaxed),
                    mean_duration_us: total.checked_div(calls).unwrap_or(0),
                    max_duration_us: c.max_duration_us.load(Ordering::Relaxed),
                    output_bytes: c.output_bytes.load(Ordering::Relaxed),
                }
            })
            .collect();
        out.sort_by(|a, b| a.tool.cmp(&b.tool));
        out
    }

    /// Render the current metrics in Prometheus text exposition format.
    #[must_use]
    pub fn to_prometheus(&self) -> String {
        let snap = self.snapshot();
        let mut s = String::new();

        // Metric families: (name, type, help, extractor).
        type Extract = fn(&ToolMetricsSnapshot) -> u64;
        let families: &[(&str, &str, &str, Extract)] = &[
            (
                "nebula_mcp_tool_calls_total",
                "counter",
                "Total tool invocations.",
                |m| m.calls,
            ),
            (
                "nebula_mcp_tool_successes_total",
                "counter",
                "Tool invocations that succeeded.",
                |m| m.successes,
            ),
            (
                "nebula_mcp_tool_failures_total",
                "counter",
                "Tool invocations that failed.",
                |m| m.failures,
            ),
            (
                "nebula_mcp_tool_cancellations_total",
                "counter",
                "Tool invocations that were cancelled or timed out.",
                |m| m.cancellations,
            ),
            (
                "nebula_mcp_tool_output_bytes_total",
                "counter",
                "Total captured output bytes.",
                |m| m.output_bytes,
            ),
            (
                "nebula_mcp_tool_duration_microseconds_mean",
                "gauge",
                "Mean tool call duration in microseconds.",
                |m| m.mean_duration_us,
            ),
            (
                "nebula_mcp_tool_duration_microseconds_max",
                "gauge",
                "Maximum observed tool call duration in microseconds.",
                |m| m.max_duration_us,
            ),
        ];

        for (name, kind, help, extract) in families {
            s.push_str(&format!("# HELP {name} {help}\n"));
            s.push_str(&format!("# TYPE {name} {kind}\n"));
            for m in &snap {
                s.push_str(&format!(
                    "{name}{{tool=\"{}\"}} {}\n",
                    escape_label(&m.tool),
                    extract(m)
                ));
            }
        }
        s
    }
}

/// Escape a Prometheus label value (`\`, `"`, newline).
fn escape_label(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Outcome classification for a completed call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The tool returned success.
    Success,
    /// The tool returned an error.
    Failure,
    /// The tool was cancelled or timed out.
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn records_and_aggregates() {
        let m = Metrics::new();
        m.record("fs.read", Outcome::Success, Duration::from_micros(100), 10);
        m.record("fs.read", Outcome::Failure, Duration::from_micros(300), 0);
        let snap = m.snapshot();
        assert_eq!(snap.len(), 1);
        let s = &snap[0];
        assert_eq!(s.calls, 2);
        assert_eq!(s.successes, 1);
        assert_eq!(s.failures, 1);
        assert_eq!(s.mean_duration_us, 200);
        assert_eq!(s.max_duration_us, 300);
        assert_eq!(s.output_bytes, 10);
    }

    #[test]
    fn prometheus_output_is_well_formed() {
        let m = Metrics::new();
        m.record("fs.read", Outcome::Success, Duration::from_micros(50), 5);
        let text = m.to_prometheus();
        assert!(text.contains("# TYPE nebula_mcp_tool_calls_total counter"));
        assert!(text.contains("nebula_mcp_tool_calls_total{tool=\"fs.read\"} 1"));
        assert!(text.contains("nebula_mcp_tool_duration_microseconds_max{tool=\"fs.read\"}"));
    }
}

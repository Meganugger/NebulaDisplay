//! Per-call execution context handed to every tool.
//!
//! A [`ToolContext`] bundles the resolved permission policy, the working
//! directory that relative paths resolve against, a cancellation token, the
//! shared metrics registry and a config snapshot. Tools should treat it as
//! read-only and never reach outside it for policy decisions.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::error::ToolError;
use crate::metrics::Metrics;
use crate::security::EffectivePolicy;

/// Context for a single tool invocation.
#[derive(Clone)]
pub struct ToolContext {
    /// Resolved permission policy for this tool.
    pub policy: Arc<EffectivePolicy>,
    /// Directory relative paths are resolved against (the server's configured
    /// workspace root).
    pub working_dir: PathBuf,
    /// Cancellation token tripped on client cancel or server shutdown.
    pub cancel: CancellationToken,
    /// Shared metrics registry.
    pub metrics: Metrics,
    /// Snapshot of the active configuration.
    pub config: Arc<Config>,
    /// Correlation id for logs/traces.
    pub request_id: String,
}

impl ToolContext {
    /// Validate a path argument against policy, resolving it relative to the
    /// working directory.
    pub fn resolve_path(&self, path: &str) -> Result<PathBuf, ToolError> {
        self.policy.check_path(Path::new(path), &self.working_dir)
    }

    /// Return an error if the call has been cancelled.
    pub fn ensure_active(&self) -> Result<(), ToolError> {
        if self.cancel.is_cancelled() {
            Err(ToolError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Effective timeout for this call, honouring an optional caller request.
    #[must_use]
    pub fn timeout(&self, requested_secs: Option<u64>) -> Duration {
        self.policy.effective_timeout(requested_secs)
    }

    /// Run a future under this context's cancellation + timeout, mapping the
    /// two failure modes onto [`ToolError::Cancelled`] / [`ToolError::Timeout`].
    pub async fn guarded<F, T>(&self, timeout: Duration, fut: F) -> Result<T, ToolError>
    where
        F: std::future::Future<Output = Result<T, ToolError>>,
    {
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => Err(ToolError::Cancelled),
            r = tokio::time::timeout(timeout, fut) => match r {
                Ok(inner) => inner,
                Err(_) => Err(ToolError::Timeout(timeout)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SecurityConfig;

    fn ctx() -> ToolContext {
        let base = SecurityConfig {
            allowed_paths: vec!["/work/**".into()],
            default_timeout_secs: 1,
            max_runtime_secs: 5,
            ..Default::default()
        };
        let policy = EffectivePolicy::build("t", &base, None).unwrap();
        ToolContext {
            policy: Arc::new(policy),
            working_dir: PathBuf::from("/work"),
            cancel: CancellationToken::new(),
            metrics: Metrics::new(),
            config: Arc::new(Config::default()),
            request_id: "req-1".into(),
        }
    }

    #[tokio::test]
    async fn guarded_times_out() {
        let c = ctx();
        let r: Result<(), ToolError> = c
            .guarded(Duration::from_millis(20), async {
                tokio::time::sleep(Duration::from_secs(10)).await;
                Ok(())
            })
            .await;
        assert!(matches!(r, Err(ToolError::Timeout(_))));
    }

    #[tokio::test]
    async fn guarded_cancels() {
        let c = ctx();
        c.cancel.cancel();
        let r: Result<(), ToolError> = c.guarded(Duration::from_secs(5), async { Ok(()) }).await;
        assert!(matches!(r, Err(ToolError::Cancelled)));
    }

    #[test]
    fn resolve_path_enforces_policy() {
        let c = ctx();
        assert!(c.resolve_path("src/main.rs").is_ok());
        assert!(c.resolve_path("/etc/passwd").is_err());
    }
}

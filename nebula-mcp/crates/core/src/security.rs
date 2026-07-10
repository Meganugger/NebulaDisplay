//! Permission policy engine.
//!
//! Every tool call runs under an [`EffectivePolicy`] derived by layering the
//! per-tool override (if any) on top of the global [`SecurityConfig`] baseline.
//! The policy is the single choke point for:
//!
//! * path access (allow/deny glob lists, lexical `..` containment),
//! * command allowlisting (basename match, Windows-insensitive),
//! * timeouts and maximum runtime,
//! * maximum captured output,
//! * elevation, network and destructive-operation gates.
//!
//! There is deliberately **no** "allow everything" escape hatch: an empty
//! allowlist denies rather than permits.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::config::{Config, SecurityConfig, ToolOverride};
use crate::error::ToolError;

/// A fully-resolved policy for a single tool.
#[derive(Debug, Clone)]
pub struct EffectivePolicy {
    tool: String,
    allowed: GlobSet,
    denied: GlobSet,
    allowed_has_patterns: bool,
    allowed_pattern_count: usize,
    allowed_commands: Vec<String>,
    timeout: Duration,
    max_runtime: Duration,
    max_output_bytes: usize,
    allow_elevated: bool,
    allow_network: bool,
    allow_destructive: bool,
}

impl EffectivePolicy {
    /// Resolve the effective policy for `tool` from `config`.
    pub fn resolve(config: &Config, tool: &str) -> Result<Self, ToolError> {
        let base = &config.security;
        let ov = config.tools.get(tool);
        Self::build(tool, base, ov)
    }

    /// Build a policy from an explicit baseline and optional override.
    /// Exposed for unit testing and programmatic construction.
    pub fn build(
        tool: &str,
        base: &SecurityConfig,
        ov: Option<&ToolOverride>,
    ) -> Result<Self, ToolError> {
        let mut allowed_patterns = base.allowed_paths.clone();
        let mut allowed_commands = base.allowed_commands.clone();
        let mut timeout_secs = base.default_timeout_secs;
        let mut max_output_bytes = base.max_output_bytes;
        let mut allow_destructive = base.allow_destructive;

        if let Some(o) = ov {
            if let Some(extra) = &o.allowed_paths {
                allowed_patterns.extend(extra.iter().cloned());
            }
            if let Some(extra) = &o.allowed_commands {
                allowed_commands.extend(extra.iter().cloned());
            }
            if let Some(t) = o.timeout_secs {
                timeout_secs = t;
            }
            if let Some(m) = o.max_output_bytes {
                max_output_bytes = m;
            }
            if let Some(d) = o.allow_destructive {
                allow_destructive = d;
            }
        }

        // Clamp the requested timeout to the absolute maximum runtime.
        let max_runtime = Duration::from_secs(base.max_runtime_secs.max(1));
        let timeout = Duration::from_secs(timeout_secs.max(1)).min(max_runtime);

        let allowed = build_globset(&allowed_patterns)?;
        let denied = build_globset(&base.denied_paths)?;

        Ok(Self {
            tool: tool.to_string(),
            allowed,
            denied,
            allowed_has_patterns: !allowed_patterns.is_empty(),
            allowed_pattern_count: allowed_patterns.len(),
            allowed_commands,
            timeout,
            max_runtime,
            max_output_bytes,
            allow_elevated: base.allow_elevated,
            allow_network: base.allow_network,
            allow_destructive,
        })
    }

    /// Name of the tool this policy applies to.
    #[must_use]
    pub fn tool(&self) -> &str {
        &self.tool
    }

    /// The resolved per-call timeout.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// The absolute maximum runtime ceiling.
    #[must_use]
    pub fn max_runtime(&self) -> Duration {
        self.max_runtime
    }

    /// Effective timeout, honouring a caller-requested value but never
    /// exceeding the configured maximum runtime.
    #[must_use]
    pub fn effective_timeout(&self, requested_secs: Option<u64>) -> Duration {
        match requested_secs {
            Some(s) => Duration::from_secs(s.max(1)).min(self.max_runtime),
            None => self.timeout,
        }
    }

    /// Maximum captured output per stream, in bytes.
    #[must_use]
    pub fn max_output_bytes(&self) -> usize {
        self.max_output_bytes
    }

    /// Whether network-reaching operations are permitted.
    #[must_use]
    pub fn allow_network(&self) -> bool {
        self.allow_network
    }

    /// Whether destructive operations are permitted.
    #[must_use]
    pub fn allow_destructive(&self) -> bool {
        self.allow_destructive
    }

    /// Whether elevated execution is permitted.
    #[must_use]
    pub fn allow_elevated(&self) -> bool {
        self.allow_elevated
    }

    /// The command allowlist (executable basenames).
    #[must_use]
    pub fn allowed_commands(&self) -> &[String] {
        &self.allowed_commands
    }

    /// Number of configured allowed-path glob patterns.
    #[must_use]
    pub fn allowed_pattern_count(&self) -> usize {
        self.allowed_pattern_count
    }

    /// Ensure elevation is permitted, returning an error otherwise.
    pub fn ensure_elevation_allowed(&self) -> Result<(), ToolError> {
        if self.allow_elevated {
            Ok(())
        } else {
            Err(ToolError::PermissionDenied(
                "elevated execution is disabled by policy".to_string(),
            ))
        }
    }

    /// Ensure destructive operations are permitted.
    pub fn ensure_destructive_allowed(&self, what: &str) -> Result<(), ToolError> {
        if self.allow_destructive {
            Ok(())
        } else {
            Err(ToolError::PermissionDenied(format!(
                "destructive operation '{what}' is disabled by policy"
            )))
        }
    }

    /// Ensure network operations are permitted.
    pub fn ensure_network_allowed(&self) -> Result<(), ToolError> {
        if self.allow_network {
            Ok(())
        } else {
            Err(ToolError::PermissionDenied(
                "network access is disabled by policy".to_string(),
            ))
        }
    }

    /// Validate and normalise a path against the allow/deny lists.
    ///
    /// Returns the normalised absolute path on success. The input may be
    /// relative; it is resolved against `base_dir`. `..` components that would
    /// escape the resolved root are rejected before any glob matching.
    pub fn check_path(&self, path: &Path, base_dir: &Path) -> Result<PathBuf, ToolError> {
        let normalized = normalize_path(path, base_dir)?;

        // Deny list always wins.
        if self.denied.is_match(&normalized) {
            return Err(ToolError::PermissionDenied(format!(
                "path '{}' matches a denied pattern",
                normalized.display()
            )));
        }

        if !self.allowed_has_patterns {
            return Err(ToolError::PermissionDenied(format!(
                "no allowed_paths configured; access to '{}' denied",
                normalized.display()
            )));
        }

        if !self.allowed.is_match(&normalized) {
            return Err(ToolError::PathNotAllowed {
                path: normalized.display().to_string(),
            });
        }

        Ok(normalized)
    }

    /// Validate an executable name/path against the command allowlist.
    ///
    /// Matching is by basename (without a trailing `.exe`), case-insensitive on
    /// Windows. Returns the original program string on success.
    pub fn check_command(&self, program: &str) -> Result<(), ToolError> {
        if self.allowed_commands.is_empty() {
            return Err(ToolError::CommandNotAllowed(program.to_string()));
        }
        let candidate = command_key(program);
        let permitted = self
            .allowed_commands
            .iter()
            .any(|c| command_key(c) == candidate);
        if permitted {
            Ok(())
        } else {
            Err(ToolError::CommandNotAllowed(program.to_string()))
        }
    }

    /// Truncate output to the configured maximum, returning the (possibly
    /// truncated) bytes and whether truncation occurred.
    #[must_use]
    pub fn clamp_output<'a>(&self, data: &'a [u8]) -> (&'a [u8], bool) {
        if data.len() > self.max_output_bytes {
            (&data[..self.max_output_bytes], true)
        } else {
            (data, false)
        }
    }
}

/// Reduce an executable path/name to a comparison key.
fn command_key(program: &str) -> String {
    let base = Path::new(program)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(program);
    let stem = base.strip_suffix(".exe").unwrap_or(base);
    if cfg!(windows) {
        stem.to_ascii_lowercase()
    } else {
        stem.to_string()
    }
}

/// Build a [`GlobSet`] from patterns, enabling literal-separator matching so
/// `**` behaves intuitively across path separators.
fn build_globset(patterns: &[String]) -> Result<GlobSet, ToolError> {
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        let glob = Glob::new(p)
            .map_err(|e| ToolError::Internal(format!("invalid glob pattern '{p}': {e}")))?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|e| ToolError::Internal(format!("building glob set: {e}")))
}

/// Lexically normalise `path` (resolving `.`/`..`) against `base_dir` without
/// touching the filesystem, then reject any residual parent traversal.
///
/// Filesystem-free normalisation avoids TOCTOU races and works for paths that
/// do not yet exist (e.g. a file about to be written).
pub fn normalize_path(path: &Path, base_dir: &Path) -> Result<PathBuf, ToolError> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    };

    let mut out = PathBuf::new();
    for comp in joined.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return Err(ToolError::PermissionDenied(format!(
                        "path '{}' traverses above the filesystem root",
                        path.display()
                    )));
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SecurityConfig;

    fn policy_with(allowed: &[&str], cmds: &[&str]) -> EffectivePolicy {
        let base = SecurityConfig {
            allowed_paths: allowed.iter().map(|s| s.to_string()).collect(),
            allowed_commands: cmds.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        EffectivePolicy::build("test.tool", &base, None).unwrap()
    }

    #[test]
    fn empty_allowlist_denies_everything() {
        let p = policy_with(&[], &[]);
        let err = p
            .check_path(Path::new("/work/a.txt"), Path::new("/work"))
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied(_)));
    }

    #[test]
    fn allowed_glob_permits_matching_path() {
        let p = policy_with(&["/work/**"], &[]);
        let ok = p
            .check_path(Path::new("/work/src/main.rs"), Path::new("/work"))
            .unwrap();
        assert_eq!(ok, PathBuf::from("/work/src/main.rs"));
    }

    #[test]
    fn denied_glob_overrides_allowed() {
        let base = SecurityConfig {
            allowed_paths: vec!["/work/**".into()],
            denied_paths: vec!["**/*.key".into()],
            ..Default::default()
        };
        let p = EffectivePolicy::build("t", &base, None).unwrap();
        let err = p
            .check_path(Path::new("/work/secret.key"), Path::new("/work"))
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied(_)));
    }

    #[test]
    fn parent_traversal_is_contained() {
        let p = policy_with(&["/work/**"], &[]);
        let err = p
            .check_path(Path::new("../etc/passwd"), Path::new("/work"))
            .unwrap_err();
        // Normalises to /etc/passwd which is outside /work/**.
        assert!(matches!(err, ToolError::PathNotAllowed { .. }));
    }

    #[test]
    fn relative_paths_resolve_against_base() {
        let p = policy_with(&["/work/**"], &[]);
        let ok = p
            .check_path(Path::new("src/lib.rs"), Path::new("/work"))
            .unwrap();
        assert_eq!(ok, PathBuf::from("/work/src/lib.rs"));
    }

    #[test]
    fn command_allowlist_matches_basename() {
        let p = policy_with(&[], &["git", "cargo"]);
        assert!(p.check_command("git").is_ok());
        assert!(p.check_command("/usr/bin/git").is_ok());
        assert!(p.check_command("rm").is_err());
    }

    #[test]
    fn empty_command_allowlist_denies() {
        let p = policy_with(&[], &[]);
        assert!(p.check_command("git").is_err());
    }

    #[test]
    fn output_clamped_to_limit() {
        let base = SecurityConfig {
            max_output_bytes: 4,
            ..Default::default()
        };
        let p = EffectivePolicy::build("t", &base, None).unwrap();
        let (out, truncated) = p.clamp_output(b"123456");
        assert_eq!(out, b"1234");
        assert!(truncated);
    }
}

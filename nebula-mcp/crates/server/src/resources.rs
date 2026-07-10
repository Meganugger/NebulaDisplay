//! MCP `resources/*` support: expose files under the workspace root as
//! resources, gated by the same path allow/deny policy as the filesystem tools,
//! plus subscriptions that emit `notifications/resources/updated` on change.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use nebula_mcp_core::config::Config;
use nebula_mcp_core::security::EffectivePolicy;
use nebula_mcp_core::ToolError;
use nebula_mcp_protocol::{ListResourcesResult, ReadResourceResult, Resource, ResourceContents};
use notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{new_debouncer, DebounceEventResult, Debouncer};
use tokio::sync::mpsc::UnboundedSender;

/// Maximum resources returned by a single `resources/list`.
const MAX_RESOURCES: usize = 500;

/// Resolve the policy used to gate resource access (baseline security config).
fn resource_policy(config: &Config) -> Result<EffectivePolicy, ToolError> {
    EffectivePolicy::resolve(config, "resources.read")
}

/// List files under `root` that are permitted by policy, as MCP resources.
pub fn list(config: &Config, root: &Path) -> Result<ListResourcesResult, ToolError> {
    let policy = resource_policy(config)?;
    let mut resources = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .max_depth(8)
        .follow_links(false)
        .into_iter()
        .flatten()
    {
        if resources.len() >= MAX_RESOURCES {
            break;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        // Only expose files the policy would allow a tool to read.
        if policy.check_path(path, root).is_err() {
            continue;
        }
        let size = entry.metadata().map(|m| m.len()).ok();
        resources.push(Resource {
            uri: path_to_uri(path),
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            description: None,
            mime_type: Some(guess_mime(path)),
            size,
        });
    }
    Ok(ListResourcesResult {
        resources,
        next_cursor: None,
    })
}

/// Read a single resource by URI, enforcing policy.
pub fn read(config: &Config, root: &Path, uri: &str) -> Result<ReadResourceResult, ToolError> {
    let policy = resource_policy(config)?;
    let path = uri_to_path(uri)?;
    let checked = policy.check_path(&path, root)?;
    let bytes = std::fs::read(&checked)
        .map_err(|e| ToolError::Io(format!("reading {}: {e}", checked.display())))?;

    // Enforce the output cap.
    let (bytes_ref, _) = policy.clamp_output(&bytes);
    let mime = guess_mime(&checked);
    let contents = match std::str::from_utf8(bytes_ref) {
        Ok(text) => ResourceContents {
            uri: uri.to_string(),
            mime_type: Some(mime),
            text: Some(text.to_string()),
            blob: None,
        },
        Err(_) => {
            use base64::Engine as _;
            ResourceContents {
                uri: uri.to_string(),
                mime_type: Some(mime),
                text: None,
                blob: Some(base64::engine::general_purpose::STANDARD.encode(bytes_ref)),
            }
        }
    };
    Ok(ReadResourceResult {
        contents: vec![contents],
    })
}

/// Build a `file://` URI from an absolute path.
fn path_to_uri(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    }
}

/// Parse a `file://` URI (or a bare path) into a path.
fn uri_to_path(uri: &str) -> Result<PathBuf, ToolError> {
    let stripped = uri
        .strip_prefix("file:///")
        .map(|s| {
            // Preserve a leading slash on Unix absolute paths.
            if cfg!(windows) {
                s.to_string()
            } else {
                format!("/{s}")
            }
        })
        .or_else(|| uri.strip_prefix("file://").map(|s| s.to_string()))
        .unwrap_or_else(|| uri.to_string());
    Ok(PathBuf::from(stripped))
}

/// Guess a MIME type from a file extension.
fn guess_mime(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mime = match ext.as_str() {
        "rs" | "toml" | "txt" | "md" | "log" | "cfg" | "ini" => "text/plain",
        "json" => "application/json",
        "yaml" | "yml" => "application/yaml",
        "xml" | "inf" => "application/xml",
        "html" | "htm" => "text/html",
        "csv" => "text/csv",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "sys" | "dll" | "exe" | "cat" => "application/octet-stream",
        _ => "text/plain",
    };
    mime.to_string()
}

/// Manages `resources/subscribe` watches. Each subscription watches a path and
/// emits `notifications/resources/updated` (via the server's notification
/// channel) when the file or directory changes.
#[derive(Default)]
pub struct SubscriptionManager {
    watchers: Mutex<HashMap<String, Debouncer<RecommendedWatcher>>>,
}

impl SubscriptionManager {
    /// Create an empty subscription manager.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe to a resource URI. The `notifier` receives a
    /// `notifications/resources/updated` frame whenever the target changes.
    pub fn subscribe(
        &self,
        config: &Config,
        root: &Path,
        uri: &str,
        notifier: UnboundedSender<serde_json::Value>,
    ) -> Result<(), ToolError> {
        let policy = resource_policy(config)?;
        let path = uri_to_path(uri)?;
        let checked = policy.check_path(&path, root)?;

        let uri_owned = uri.to_string();
        let mut debouncer = new_debouncer(
            Duration::from_millis(300),
            move |res: DebounceEventResult| {
                if res.is_ok() {
                    let msg = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/resources/updated",
                        "params": { "uri": uri_owned },
                    });
                    let _ = notifier.send(msg);
                }
            },
        )
        .map_err(|e| ToolError::Internal(format!("creating watcher: {e}")))?;

        let mode = if checked.is_dir() {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        debouncer
            .watcher()
            .watch(&checked, mode)
            .map_err(|e| ToolError::Io(format!("watching {}: {e}", checked.display())))?;

        self.watchers
            .lock()
            .unwrap()
            .insert(uri.to_string(), debouncer);
        Ok(())
    }

    /// Unsubscribe from a URI. Returns whether a subscription existed.
    pub fn unsubscribe(&self, uri: &str) -> bool {
        self.watchers.lock().unwrap().remove(uri).is_some()
    }

    /// Drop all subscriptions (used on shutdown).
    pub fn clear(&self) {
        self.watchers.lock().unwrap().clear();
    }

    /// Number of active subscriptions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.watchers.lock().unwrap().len()
    }

    /// Whether there are no active subscriptions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_roundtrip_unix() {
        if cfg!(windows) {
            return;
        }
        let uri = path_to_uri(Path::new("/work/src/main.rs"));
        assert_eq!(uri, "file:///work/src/main.rs");
        assert_eq!(
            uri_to_path(&uri).unwrap(),
            PathBuf::from("/work/src/main.rs")
        );
    }

    #[test]
    fn mime_guessing() {
        assert_eq!(guess_mime(Path::new("a.json")), "application/json");
        assert_eq!(guess_mime(Path::new("a.png")), "image/png");
    }
}

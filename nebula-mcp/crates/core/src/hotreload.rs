//! Configuration hot reload.
//!
//! Watches the config file (debounced) and swaps the [`ConfigStore`] contents
//! on change. A bad edit is logged and ignored, keeping the previous valid
//! config live so the server never crashes on a typo.

use std::path::Path;
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_mini::{new_debouncer, DebounceEventResult, Debouncer};

use crate::config::ConfigStore;
use crate::error::ToolError;

/// Handle that keeps the file watcher alive. Drop to stop watching.
pub struct ReloadHandle {
    _debouncer: Debouncer<notify::RecommendedWatcher>,
}

/// Begin watching the store's backing file for changes.
///
/// Returns an error if the store has no backing file or the watcher cannot be
/// created. On each debounced change the config is reloaded from disk; failures
/// are logged at `warn` and leave the current config in place.
pub fn watch(store: ConfigStore) -> Result<ReloadHandle, ToolError> {
    let path = store
        .source_path()
        .ok_or_else(|| ToolError::Internal("cannot watch a file-less config store".into()))?
        .to_path_buf();

    let store_for_cb = store.clone();
    let watched = path.clone();
    let mut debouncer = new_debouncer(
        Duration::from_millis(400),
        move |res: DebounceEventResult| match res {
            Ok(_events) => match store_for_cb.reload_from_disk() {
                Ok(()) => tracing::info!(path = %watched.display(), "configuration reloaded"),
                Err(e) => tracing::warn!(path = %watched.display(), error = %e, "config reload failed; keeping previous config"),
            },
            Err(e) => tracing::warn!(error = ?e, "config watch error"),
        },
    )
    .map_err(|e| ToolError::Internal(format!("creating config watcher: {e}")))?;

    // Watch the parent directory so atomic-rename saves (editor "write to temp
    // then rename") are still observed.
    let watch_target = path.parent().unwrap_or_else(|| Path::new("."));
    debouncer
        .watcher()
        .watch(watch_target, RecursiveMode::NonRecursive)
        .map_err(|e| ToolError::Internal(format!("watching {}: {e}", watch_target.display())))?;

    tracing::info!(path = %path.display(), "watching configuration for changes");
    Ok(ReloadHandle {
        _debouncer: debouncer,
    })
}

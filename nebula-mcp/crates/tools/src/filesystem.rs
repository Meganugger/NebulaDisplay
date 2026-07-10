//! Filesystem tools.
//!
//! Every path argument is resolved and policy-checked via
//! [`ToolContext::resolve_path`], so a tool can only ever touch a path inside a
//! configured allow-root and outside the deny list. Reads and writes are
//! async; directory walks run on the blocking pool and honour cancellation.

use std::path::Path;

use async_trait::async_trait;
use base64::Engine as _;
use nebula_mcp_core::{Result, Tool, ToolContext, ToolError};
use nebula_mcp_protocol::mcp::ToolAnnotations;
use nebula_mcp_protocol::{CallToolResult, Content};
use serde::Serialize;
use serde_json::{json, Value};

use crate::common::output::{json_result, json_value_result};
use crate::common::{Args, ObjectSchema};

const CATEGORY: &str = "filesystem";

/// Register all filesystem tools into `out`.
pub fn tools() -> Vec<std::sync::Arc<dyn Tool>> {
    use std::sync::Arc;
    vec![
        Arc::new(ReadFile),
        Arc::new(WriteFile),
        Arc::new(AppendFile),
        Arc::new(RenamePath),
        Arc::new(DeletePath),
        Arc::new(MovePath),
        Arc::new(CopyPath),
        Arc::new(SearchContent),
        Arc::new(GlobFiles),
        Arc::new(DirectoryTree),
        Arc::new(FileHash),
        Arc::new(FileMetadata),
        Arc::new(FilePermissions),
    ]
}

/// Read a UTF-8 text file, optionally a byte slice for large-file streaming.
struct ReadFile;

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "fs.read"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Read a file. Supports byte offset and maxBytes for streaming large files in chunks, and encoding=base64 for binary files."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string(
                "path",
                "File path (relative to workspace root or absolute).",
                true,
            )
            .integer(
                "offset",
                "Byte offset to start reading from (default 0).",
                false,
            )
            .integer(
                "maxBytes",
                "Maximum bytes to read (default: policy output limit).",
                false,
            )
            .enumerated(
                "encoding",
                "How to encode returned content: utf8 (default) or base64 (binary-safe).",
                &["utf8", "base64"],
                false,
            )
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        Some(ToolAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        })
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let path = ctx.resolve_path(a.str("path")?)?;
        let offset = a.u64_or("offset", 0)?;
        let max_bytes = a.u64_or("maxBytes", ctx.policy.max_output_bytes() as u64)? as usize;

        let meta = tokio::fs::metadata(&path)
            .await
            .map_err(|e| map_io("stat", &path, e))?;
        if meta.is_dir() {
            return Err(ToolError::InvalidArguments(format!(
                "'{}' is a directory, not a file",
                path.display()
            )));
        }
        let total = meta.len();

        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let mut file = tokio::fs::File::open(&path)
            .await
            .map_err(|e| map_io("open", &path, e))?;
        if offset > 0 {
            file.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(|e| map_io("seek", &path, e))?;
        }
        let mut buf = vec![0u8; max_bytes.min(total.saturating_sub(offset) as usize)];
        let mut read = 0usize;
        while read < buf.len() {
            let n = file
                .read(&mut buf[read..])
                .await
                .map_err(|e| map_io("read", &path, e))?;
            if n == 0 {
                break;
            }
            read += n;
        }
        buf.truncate(read);
        let encoding = a.str_or("encoding", "utf8")?;
        let content = match encoding {
            "base64" => base64::engine::general_purpose::STANDARD.encode(&buf),
            _ => String::from_utf8_lossy(&buf).into_owned(),
        };
        let end = offset + read as u64;

        Ok(json_value_result(json!({
            "path": path.display().to_string(),
            "content": content,
            "encoding": encoding,
            "bytesRead": read,
            "offset": offset,
            "endOffset": end,
            "fileSize": total,
            "eof": end >= total,
        })))
    }
}

/// Write (create or overwrite) a text file, creating parent dirs.
struct WriteFile;

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str {
        "fs.write"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Create or overwrite a file, creating parent directories. Use encoding=base64 to write binary content."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("path", "File path.", true)
            .string(
                "content",
                "Content to write (UTF-8 text, or base64 when encoding=base64).",
                true,
            )
            .enumerated(
                "encoding",
                "Interpretation of content: utf8 (default) or base64.",
                &["utf8", "base64"],
                false,
            )
            .boolean("createParents", "Create missing parent directories.", true)
            .build()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let path = ctx.resolve_path(a.str("path")?)?;
        let content = a.str("content")?;
        let bytes = match a.str_or("encoding", "utf8")? {
            "base64" => base64::engine::general_purpose::STANDARD
                .decode(content.trim())
                .map_err(|e| ToolError::InvalidArguments(format!("invalid base64 content: {e}")))?,
            _ => content.as_bytes().to_vec(),
        };
        if a.bool_or("createParents", true)? {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| map_io("create parent of", parent, e))?;
            }
        }
        tokio::fs::write(&path, &bytes)
            .await
            .map_err(|e| map_io("write", &path, e))?;
        Ok(json_value_result(json!({
            "path": path.display().to_string(),
            "bytesWritten": bytes.len(),
        })))
    }
}

/// Append text to a file.
struct AppendFile;

#[async_trait]
impl Tool for AppendFile {
    fn name(&self) -> &str {
        "fs.append"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Append UTF-8 text to a file, creating it if absent."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("path", "File path.", true)
            .string("content", "Text to append.", true)
            .build()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let path = ctx.resolve_path(a.str("path")?)?;
        let content = a.str("content")?;
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|e| map_io("open for append", &path, e))?;
        file.write_all(content.as_bytes())
            .await
            .map_err(|e| map_io("append", &path, e))?;
        Ok(json_value_result(json!({
            "path": path.display().to_string(),
            "bytesAppended": content.len(),
        })))
    }
}

/// Rename a path within the same parent (or to any allowed destination).
struct RenamePath;

#[async_trait]
impl Tool for RenamePath {
    fn name(&self) -> &str {
        "fs.rename"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Rename or move a file/directory (both paths must be within allowed roots)."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("from", "Existing path.", true)
            .string("to", "Destination path.", true)
            .build()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let from = ctx.resolve_path(a.str("from")?)?;
        let to = ctx.resolve_path(a.str("to")?)?;
        tokio::fs::rename(&from, &to)
            .await
            .map_err(|e| map_io("rename", &from, e))?;
        Ok(json_value_result(json!({
            "from": from.display().to_string(),
            "to": to.display().to_string(),
        })))
    }
}

/// Delete a file or directory (destructive; gated by policy).
struct DeletePath;

#[async_trait]
impl Tool for DeletePath {
    fn name(&self) -> &str {
        "fs.delete"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Delete a file or directory. Directory deletion requires recursive=true. Destructive."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("path", "Path to delete.", true)
            .boolean("recursive", "Allow recursive directory deletion.", false)
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        Some(ToolAnnotations {
            destructive_hint: Some(true),
            ..Default::default()
        })
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let path = ctx.resolve_path(a.str("path")?)?;
        let recursive = a.bool_or("recursive", false)?;
        ctx.policy.ensure_destructive_allowed("fs.delete")?;

        let meta = tokio::fs::symlink_metadata(&path)
            .await
            .map_err(|e| map_io("stat", &path, e))?;
        if meta.is_dir() {
            if recursive {
                tokio::fs::remove_dir_all(&path)
                    .await
                    .map_err(|e| map_io("remove dir", &path, e))?;
            } else {
                tokio::fs::remove_dir(&path)
                    .await
                    .map_err(|e| map_io("remove dir", &path, e))?;
            }
        } else {
            tokio::fs::remove_file(&path)
                .await
                .map_err(|e| map_io("remove file", &path, e))?;
        }
        Ok(json_value_result(json!({
            "path": path.display().to_string(),
            "deleted": true,
        })))
    }
}

/// Move a path (rename with cross-device copy fallback).
struct MovePath;

#[async_trait]
impl Tool for MovePath {
    fn name(&self) -> &str {
        "fs.move"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Move a file/directory, falling back to copy+delete across filesystems."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("from", "Source path.", true)
            .string("to", "Destination path.", true)
            .build()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let from = ctx.resolve_path(a.str("from")?)?;
        let to = ctx.resolve_path(a.str("to")?)?;
        if tokio::fs::rename(&from, &to).await.is_err() {
            // Cross-device: copy then delete.
            let meta = tokio::fs::symlink_metadata(&from)
                .await
                .map_err(|e| map_io("stat", &from, e))?;
            if meta.is_dir() {
                copy_dir_recursive(&from, &to).await?;
                tokio::fs::remove_dir_all(&from)
                    .await
                    .map_err(|e| map_io("remove source dir", &from, e))?;
            } else {
                tokio::fs::copy(&from, &to)
                    .await
                    .map_err(|e| map_io("copy", &from, e))?;
                tokio::fs::remove_file(&from)
                    .await
                    .map_err(|e| map_io("remove source", &from, e))?;
            }
        }
        Ok(json_value_result(json!({
            "from": from.display().to_string(),
            "to": to.display().to_string(),
        })))
    }
}

/// Copy a file or directory tree.
struct CopyPath;

#[async_trait]
impl Tool for CopyPath {
    fn name(&self) -> &str {
        "fs.copy"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Copy a file or directory tree to a new location."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("from", "Source path.", true)
            .string("to", "Destination path.", true)
            .build()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let from = ctx.resolve_path(a.str("from")?)?;
        let to = ctx.resolve_path(a.str("to")?)?;
        let meta = tokio::fs::symlink_metadata(&from)
            .await
            .map_err(|e| map_io("stat", &from, e))?;
        let bytes = if meta.is_dir() {
            copy_dir_recursive(&from, &to).await?
        } else {
            tokio::fs::copy(&from, &to)
                .await
                .map_err(|e| map_io("copy", &from, e))?
        };
        Ok(json_value_result(json!({
            "from": from.display().to_string(),
            "to": to.display().to_string(),
            "bytesCopied": bytes,
        })))
    }
}

/// Regex/substring content search across files (grep-like).
struct SearchContent;

#[async_trait]
impl Tool for SearchContent {
    fn name(&self) -> &str {
        "fs.search"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Search file contents by regular expression under a root directory, returning matches with line numbers."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("root", "Directory to search under.", true)
            .string("pattern", "Regular expression to match.", true)
            .string(
                "glob",
                "Optional filename glob filter (e.g. '*.rs').",
                false,
            )
            .boolean("ignoreCase", "Case-insensitive matching.", false)
            .integer(
                "maxResults",
                "Maximum matches to return (default 500).",
                false,
            )
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        Some(ToolAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        })
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let root = ctx.resolve_path(a.str("root")?)?;
        let pattern = a.str("pattern")?.to_string();
        let glob = a.opt_str("glob")?.map(str::to_string);
        let ignore_case = a.bool_or("ignoreCase", false)?;
        let max_results = a.u64_or("maxResults", 500)? as usize;
        let max_output = ctx.policy.max_output_bytes();
        let cancel = ctx.cancel.clone();

        let re = regex::RegexBuilder::new(&pattern)
            .case_insensitive(ignore_case)
            .build()
            .map_err(|e| ToolError::InvalidArguments(format!("invalid regex: {e}")))?;
        let matcher = glob
            .as_deref()
            .map(|g| {
                globset::Glob::new(g)
                    .map(|x| x.compile_matcher())
                    .map_err(|e| ToolError::InvalidArguments(format!("invalid glob: {e}")))
            })
            .transpose()?;

        let result = tokio::task::spawn_blocking(move || {
            #[derive(Serialize)]
            struct Match {
                file: String,
                line: usize,
                text: String,
            }
            let mut matches: Vec<Match> = Vec::new();
            let mut scanned = 0usize;
            let mut truncated = false;
            for entry in walkdir::WalkDir::new(&root).follow_links(false) {
                if cancel.is_cancelled() {
                    return Err(ToolError::Cancelled);
                }
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                if !entry.file_type().is_file() {
                    continue;
                }
                if let Some(m) = &matcher {
                    let name = entry.file_name().to_string_lossy();
                    if !m.is_match(name.as_ref()) {
                        continue;
                    }
                }
                let Ok(data) = std::fs::read(entry.path()) else {
                    continue;
                };
                if data.contains(&0) {
                    continue; // skip binary files
                }
                scanned += 1;
                let text = String::from_utf8_lossy(&data);
                for (i, line) in text.lines().enumerate() {
                    if re.is_match(line) {
                        matches.push(Match {
                            file: entry.path().display().to_string(),
                            line: i + 1,
                            text: line.chars().take(400).collect(),
                        });
                        if matches.len() >= max_results {
                            truncated = true;
                            break;
                        }
                    }
                }
                if truncated {
                    break;
                }
            }
            Ok(json!({
                "root": root.display().to_string(),
                "pattern": pattern,
                "filesScanned": scanned,
                "matchCount": matches.len(),
                "truncated": truncated,
                "matches": matches,
            }))
        })
        .await
        .map_err(|e| ToolError::Internal(format!("search task join: {e}")))??;

        let mut res = json_value_result(result);
        // Guard against a huge JSON body.
        let body_len = res.content.first().map(|c| match c {
            Content::Text { text } => text.len(),
            _ => 0,
        });
        if let Some(len) = body_len {
            if len > max_output {
                res = CallToolResult::error_text(format!(
                    "search result exceeded output limit ({len} > {max_output} bytes); narrow the pattern or glob"
                ));
            }
        }
        Ok(res)
    }
}

/// Glob for file paths under a root.
struct GlobFiles;

#[async_trait]
impl Tool for GlobFiles {
    fn name(&self) -> &str {
        "fs.glob"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "List files matching a glob pattern under a root directory."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("root", "Directory to search under.", true)
            .string("pattern", "Glob pattern, e.g. '**/*.rs'.", true)
            .integer(
                "maxResults",
                "Maximum paths to return (default 2000).",
                false,
            )
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        Some(ToolAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        })
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let root = ctx.resolve_path(a.str("root")?)?;
        let pattern = a.str("pattern")?.to_string();
        let max_results = a.u64_or("maxResults", 2000)? as usize;
        let cancel = ctx.cancel.clone();

        let matcher = globset::Glob::new(&pattern)
            .map_err(|e| ToolError::InvalidArguments(format!("invalid glob: {e}")))?
            .compile_matcher();

        let result = tokio::task::spawn_blocking(move || {
            let mut paths = Vec::new();
            let mut truncated = false;
            for entry in walkdir::WalkDir::new(&root).follow_links(false) {
                if cancel.is_cancelled() {
                    return Err(ToolError::Cancelled);
                }
                let Ok(entry) = entry else { continue };
                let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
                if matcher.is_match(rel) || matcher.is_match(entry.path()) {
                    paths.push(entry.path().display().to_string());
                    if paths.len() >= max_results {
                        truncated = true;
                        break;
                    }
                }
            }
            Ok(json!({
                "root": root.display().to_string(),
                "pattern": pattern,
                "count": paths.len(),
                "truncated": truncated,
                "paths": paths,
            }))
        })
        .await
        .map_err(|e| ToolError::Internal(format!("glob task join: {e}")))??;
        Ok(json_value_result(result))
    }
}

/// Directory tree listing to a bounded depth.
struct DirectoryTree;

#[async_trait]
impl Tool for DirectoryTree {
    fn name(&self) -> &str {
        "fs.tree"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Produce a directory tree (names, types, sizes) to a bounded depth."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("path", "Root directory.", true)
            .integer("maxDepth", "Maximum recursion depth (default 4).", false)
            .integer(
                "maxEntries",
                "Maximum entries to return (default 5000).",
                false,
            )
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        Some(ToolAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        })
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let root = ctx.resolve_path(a.str("path")?)?;
        let max_depth = a.u64_or("maxDepth", 4)? as usize;
        let max_entries = a.u64_or("maxEntries", 5000)? as usize;
        let cancel = ctx.cancel.clone();

        let result = tokio::task::spawn_blocking(move || {
            #[derive(Serialize)]
            struct Entry {
                path: String,
                #[serde(rename = "type")]
                kind: &'static str,
                depth: usize,
                size: u64,
            }
            let mut entries = Vec::new();
            let mut truncated = false;
            for entry in walkdir::WalkDir::new(&root)
                .max_depth(max_depth)
                .follow_links(false)
            {
                if cancel.is_cancelled() {
                    return Err(ToolError::Cancelled);
                }
                let Ok(entry) = entry else { continue };
                let ft = entry.file_type();
                let kind = if ft.is_dir() {
                    "dir"
                } else if ft.is_symlink() {
                    "symlink"
                } else {
                    "file"
                };
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                entries.push(Entry {
                    path: entry.path().display().to_string(),
                    kind,
                    depth: entry.depth(),
                    size,
                });
                if entries.len() >= max_entries {
                    truncated = true;
                    break;
                }
            }
            Ok(json!({
                "root": root.display().to_string(),
                "count": entries.len(),
                "truncated": truncated,
                "entries": entries,
            }))
        })
        .await
        .map_err(|e| ToolError::Internal(format!("tree task join: {e}")))??;
        Ok(json_value_result(result))
    }
}

/// SHA-256 (and size) of a file.
struct FileHash;

#[async_trait]
impl Tool for FileHash {
    fn name(&self) -> &str {
        "fs.hash"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Compute the SHA-256 digest and size of a file."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("path", "File path.", true)
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        Some(ToolAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        })
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let path = ctx.resolve_path(a.str("path")?)?;
        let cancel = ctx.cancel.clone();
        let result = tokio::task::spawn_blocking(move || {
            use sha2::{Digest, Sha256};
            use std::io::Read;
            let file = std::fs::File::open(&path).map_err(|e| map_io("open", &path, e))?;
            let mut reader = std::io::BufReader::new(file);
            let mut hasher = Sha256::new();
            let mut buf = [0u8; 64 * 1024];
            let mut size = 0u64;
            loop {
                if cancel.is_cancelled() {
                    return Err(ToolError::Cancelled);
                }
                let n = reader
                    .read(&mut buf)
                    .map_err(|e| map_io("read", &path, e))?;
                if n == 0 {
                    break;
                }
                size += n as u64;
                hasher.update(&buf[..n]);
            }
            Ok(json!({
                "path": path.display().to_string(),
                "sha256": hex::encode(hasher.finalize()),
                "size": size,
            }))
        })
        .await
        .map_err(|e| ToolError::Internal(format!("hash task join: {e}")))??;
        Ok(json_value_result(result))
    }
}

/// File/dir metadata.
struct FileMetadata;

#[async_trait]
impl Tool for FileMetadata {
    fn name(&self) -> &str {
        "fs.metadata"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Return metadata for a path: type, size, timestamps, and read-only status."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new().string("path", "Path.", true).build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        Some(ToolAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        })
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let path = ctx.resolve_path(a.str("path")?)?;
        let meta = tokio::fs::symlink_metadata(&path)
            .await
            .map_err(|e| map_io("stat", &path, e))?;
        Ok(json_result(&describe_metadata(&path, &meta)))
    }
}

/// File permission inspection/modification.
struct FilePermissions;

#[async_trait]
impl Tool for FilePermissions {
    fn name(&self) -> &str {
        "fs.permissions"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Get or set file permissions. On Unix accepts an octal 'mode'; on Windows toggles 'readonly'."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("path", "Path.", true)
            .string(
                "mode",
                "Unix octal mode to set, e.g. '0644' (optional).",
                false,
            )
            .boolean(
                "readonly",
                "Windows: set/clear the read-only attribute.",
                false,
            )
            .build()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let path = ctx.resolve_path(a.str("path")?)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode_str) = a.opt_str("mode")? {
                let digits = mode_str.trim_start_matches("0o");
                let mode = u32::from_str_radix(digits, 8).map_err(|_| {
                    ToolError::InvalidArguments(format!("invalid octal mode '{mode_str}'"))
                })?;
                let perms = std::fs::Permissions::from_mode(mode);
                tokio::fs::set_permissions(&path, perms)
                    .await
                    .map_err(|e| map_io("set permissions", &path, e))?;
            }
            let meta = tokio::fs::metadata(&path)
                .await
                .map_err(|e| map_io("stat", &path, e))?;
            let mode = meta.permissions().mode();
            return Ok(json_value_result(json!({
                "path": path.display().to_string(),
                "mode": format!("{:o}", mode & 0o7777),
                "readonly": meta.permissions().readonly(),
            })));
        }
        #[cfg(not(unix))]
        {
            if let Some(ro) = a.opt_value("readonly").and_then(|v| v.as_bool()) {
                let meta = tokio::fs::metadata(&path)
                    .await
                    .map_err(|e| map_io("stat", &path, e))?;
                let mut perms = meta.permissions();
                perms.set_readonly(ro);
                tokio::fs::set_permissions(&path, perms)
                    .await
                    .map_err(|e| map_io("set permissions", &path, e))?;
            }
            let meta = tokio::fs::metadata(&path)
                .await
                .map_err(|e| map_io("stat", &path, e))?;
            return Ok(json_value_result(json!({
                "path": path.display().to_string(),
                "readonly": meta.permissions().readonly(),
            })));
        }
    }
}

// ---- helpers ----

#[derive(Serialize)]
struct MetadataView {
    path: String,
    #[serde(rename = "type")]
    kind: &'static str,
    size: u64,
    readonly: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    modified_unix: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_unix: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    accessed_unix: Option<i64>,
}

fn describe_metadata(path: &Path, meta: &std::fs::Metadata) -> MetadataView {
    let kind = if meta.is_dir() {
        "dir"
    } else if meta.file_type().is_symlink() {
        "symlink"
    } else {
        "file"
    };
    let to_unix = |t: std::io::Result<std::time::SystemTime>| -> Option<i64> {
        t.ok()
            .and_then(|st| st.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
    };
    MetadataView {
        path: path.display().to_string(),
        kind,
        size: meta.len(),
        readonly: meta.permissions().readonly(),
        modified_unix: to_unix(meta.modified()),
        created_unix: to_unix(meta.created()),
        accessed_unix: to_unix(meta.accessed()),
    }
}

fn map_io(op: &str, path: &Path, e: std::io::Error) -> ToolError {
    ToolError::Io(format!("failed to {op} '{}': {e}", path.display()))
}

/// Recursively copy a directory tree, returning the total bytes copied.
fn copy_dir_recursive<'a>(
    from: &'a Path,
    to: &'a Path,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<u64>> + Send + 'a>> {
    Box::pin(async move {
        tokio::fs::create_dir_all(to)
            .await
            .map_err(|e| map_io("create dir", to, e))?;
        let mut total = 0u64;
        let mut rd = tokio::fs::read_dir(from)
            .await
            .map_err(|e| map_io("read dir", from, e))?;
        while let Some(entry) = rd
            .next_entry()
            .await
            .map_err(|e| map_io("iterate dir", from, e))?
        {
            let ft = entry
                .file_type()
                .await
                .map_err(|e| map_io("stat", from, e))?;
            let src = entry.path();
            let dst = to.join(entry.file_name());
            if ft.is_dir() {
                total += copy_dir_recursive(&src, &dst).await?;
            } else {
                total += tokio::fs::copy(&src, &dst)
                    .await
                    .map_err(|e| map_io("copy", &src, e))?;
            }
        }
        Ok(total)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_mcp_core::config::SecurityConfig;
    use nebula_mcp_core::security::EffectivePolicy;
    use nebula_mcp_core::Metrics;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn ctx_for(dir: &Path, destructive: bool) -> ToolContext {
        let base = SecurityConfig {
            allowed_paths: vec![format!("{}/**", dir.display()), dir.display().to_string()],
            allow_destructive: destructive,
            max_output_bytes: 1024 * 1024,
            default_timeout_secs: 30,
            max_runtime_secs: 60,
            ..Default::default()
        };
        let policy = EffectivePolicy::build("fs", &base, None).unwrap();
        ToolContext {
            policy: Arc::new(policy),
            working_dir: dir.to_path_buf(),
            cancel: CancellationToken::new(),
            metrics: Metrics::new(),
            config: Arc::new(Default::default()),
            request_id: "r".into(),
            progress: None,
        }
    }

    #[tokio::test]
    async fn write_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_for(dir.path(), false);
        WriteFile
            .call(&ctx, json!({"path": "a/b.txt", "content": "hello"}))
            .await
            .unwrap();
        let res = ReadFile
            .call(&ctx, json!({"path": "a/b.txt"}))
            .await
            .unwrap();
        let text = match &res.content[0] {
            Content::Text { text } => text,
            _ => panic!(),
        };
        assert!(text.contains("hello"));
    }

    #[tokio::test]
    async fn delete_requires_destructive_policy() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_for(dir.path(), false);
        WriteFile
            .call(&ctx, json!({"path": "x.txt", "content": "y"}))
            .await
            .unwrap();
        let err = DeletePath
            .call(&ctx, json!({"path": "x.txt"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied(_)));

        let ctx2 = ctx_for(dir.path(), true);
        DeletePath
            .call(&ctx2, json!({"path": "x.txt"}))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn hash_matches_known_value() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_for(dir.path(), false);
        WriteFile
            .call(&ctx, json!({"path": "h.txt", "content": "abc"}))
            .await
            .unwrap();
        let res = FileHash.call(&ctx, json!({"path": "h.txt"})).await.unwrap();
        let text = match &res.content[0] {
            Content::Text { text } => text.clone(),
            _ => panic!(),
        };
        // SHA-256("abc")
        assert!(text.contains("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"));
    }

    #[tokio::test]
    async fn search_finds_matches() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_for(dir.path(), false);
        WriteFile
            .call(
                &ctx,
                json!({"path": "code.rs", "content": "fn main() {}\nlet x = 1;"}),
            )
            .await
            .unwrap();
        let res = SearchContent
            .call(&ctx, json!({"root": ".", "pattern": "fn \\w+"}))
            .await
            .unwrap();
        let text = match &res.content[0] {
            Content::Text { text } => text.clone(),
            _ => panic!(),
        };
        assert!(text.contains("\"matchCount\": 1"));
    }

    #[tokio::test]
    async fn glob_lists_files() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_for(dir.path(), false);
        WriteFile
            .call(&ctx, json!({"path": "src/a.rs", "content": "x"}))
            .await
            .unwrap();
        WriteFile
            .call(&ctx, json!({"path": "src/b.txt", "content": "y"}))
            .await
            .unwrap();
        let res = GlobFiles
            .call(&ctx, json!({"root": ".", "pattern": "**/*.rs"}))
            .await
            .unwrap();
        let text = match &res.content[0] {
            Content::Text { text } => text.clone(),
            _ => panic!(),
        };
        assert!(text.contains("a.rs"));
        assert!(!text.contains("b.txt"));
    }

    #[tokio::test]
    async fn path_escape_denied() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_for(dir.path(), false);
        let err = ReadFile
            .call(&ctx, json!({"path": "/etc/passwd"}))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ToolError::PathNotAllowed { .. } | ToolError::PermissionDenied(_)
        ));
    }
}

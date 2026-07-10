//! Git tools.
//!
//! Each tool is a thin, typed wrapper over the `git` CLI (which must be on the
//! command allowlist). A generic [`GitTool`] carries a per-subcommand argument
//! builder so the full command surface is covered without a struct per verb.
//! All operations run with `git -C <repo>` and the repo path is policy-checked.

use std::sync::Arc;

use async_trait::async_trait;
use nebula_mcp_core::{Result, Tool, ToolContext, ToolError};
use nebula_mcp_protocol::mcp::ToolAnnotations;
use nebula_mcp_protocol::CallToolResult;
use serde_json::Value;

use crate::common::exec::{run_checked, CommandSpec};
use crate::common::output::exec_result;
use crate::common::{Args, ObjectSchema};

const CATEGORY: &str = "git";
const GIT: &str = "git";

/// Signature of a function that turns validated args into git arguments
/// (everything after `git -C <repo>`).
type ArgBuilder = fn(&Args) -> Result<Vec<String>>;

/// A generic git subcommand tool.
struct GitTool {
    name: &'static str,
    description: &'static str,
    schema: fn() -> Value,
    build: ArgBuilder,
    destructive: bool,
    read_only: bool,
}

#[async_trait]
impl Tool for GitTool {
    fn name(&self) -> &str {
        self.name
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        self.description
    }
    fn input_schema(&self) -> Value {
        (self.schema)()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        Some(ToolAnnotations {
            read_only_hint: Some(self.read_only),
            destructive_hint: Some(self.destructive),
            ..Default::default()
        })
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let repo = ctx.resolve_path(a.str_or("repo", ".")?)?;
        if self.destructive {
            ctx.policy.ensure_destructive_allowed(self.name)?;
        }
        let timeout = a.opt_u64("timeoutSecs")?;
        let git_args = (self.build)(&a)?;
        let mut full = vec!["-C".to_string(), repo.display().to_string()];
        full.extend(git_args);
        let display = format!("git {}", full.join(" "));
        let spec = CommandSpec::new(GIT, ctx.working_dir.clone(), ctx).args(full);
        let result = run_checked(ctx, spec, timeout).await?;
        Ok(exec_result(&display, &result))
    }
}

/// Build all git tools.
pub fn tools() -> Vec<Arc<dyn Tool>> {
    macro_rules! git_tool {
        ($name:literal, $desc:literal, $schema:expr, $build:expr, ro=$ro:literal, dstr=$dstr:literal) => {
            Arc::new(GitTool {
                name: $name,
                description: $desc,
                schema: $schema,
                build: $build,
                read_only: $ro,
                destructive: $dstr,
            }) as Arc<dyn Tool>
        };
    }

    vec![
        git_tool!(
            "git.status",
            "Show the working tree status (porcelain v2 with branch info).",
            schema_repo_only,
            |_a| Ok(vec!["status".into(), "--porcelain=v2".into(), "--branch".into()]),
            ro = true,
            dstr = false
        ),
        git_tool!(
            "git.diff",
            "Show changes. Optional 'from'/'to' revisions, 'path' filter, and 'staged' flag.",
            schema_diff,
            build_diff,
            ro = true,
            dstr = false
        ),
        git_tool!(
            "git.blame",
            "Show what revision and author last modified each line of a file.",
            schema_blame,
            |a| {
                let file = a.str("path")?;
                Ok(vec!["blame".into(), "--line-porcelain".into(), file.into()])
            },
            ro = true,
            dstr = false
        ),
        git_tool!(
            "git.log",
            "Show commit logs. Optional 'maxCount', 'path', and 'revRange'.",
            schema_log,
            build_log,
            ro = true,
            dstr = false
        ),
        git_tool!(
            "git.branch",
            "List, create or delete branches. Modes: list (default), create, delete.",
            schema_branch,
            build_branch,
            ro = false,
            dstr = false
        ),
        git_tool!(
            "git.checkout",
            "Switch branches or restore paths. Provide 'ref' and optional 'create'.",
            schema_checkout,
            build_checkout,
            ro = false,
            dstr = false
        ),
        git_tool!(
            "git.merge",
            "Merge a ref into the current branch.",
            schema_ref_required,
            |a| Ok(vec!["merge".into(), a.str("ref")?.into()]),
            ro = false,
            dstr = false
        ),
        git_tool!(
            "git.rebase",
            "Rebase the current branch onto 'ref' (add 'abort' or 'continue' to control an in-progress rebase).",
            schema_rebase,
            build_rebase,
            ro = false,
            dstr = true
        ),
        git_tool!(
            "git.stash",
            "Manage the stash. Modes: push (default), pop, list, drop, apply.",
            schema_stash,
            build_stash,
            ro = false,
            dstr = false
        ),
        git_tool!(
            "git.tag",
            "List or create tags. Provide 'name' to create, optional 'message' for an annotated tag.",
            schema_tag,
            build_tag,
            ro = false,
            dstr = false
        ),
        git_tool!(
            "git.bisect",
            "Drive a bisect session. Provide 'action' (start/good/bad/reset) and optional 'rev'.",
            schema_bisect,
            build_bisect,
            ro = false,
            dstr = false
        ),
        git_tool!(
            "git.commit",
            "Record staged changes. Provide 'message'; set 'all' to stage tracked changes first.",
            schema_commit,
            build_commit,
            ro = false,
            dstr = false
        ),
        git_tool!(
            "git.push",
            "Push to a remote. Optional 'remote' (default origin), 'branch', 'force', 'setUpstream'.",
            schema_push,
            build_push,
            ro = false,
            dstr = false
        ),
        git_tool!(
            "git.pull",
            "Pull from a remote. Optional 'remote', 'branch', 'rebase'.",
            schema_pull,
            build_pull,
            ro = false,
            dstr = false
        ),
        git_tool!(
            "git.fetch",
            "Fetch from a remote. Optional 'remote' (default origin), 'prune', 'all'.",
            schema_fetch,
            build_fetch,
            ro = true,
            dstr = false
        ),
        git_tool!(
            "git.reset",
            "Reset current HEAD. Modes: soft, mixed (default), hard. 'hard' is destructive.",
            schema_reset,
            build_reset,
            ro = false,
            dstr = true
        ),
        git_tool!(
            "git.clean",
            "Remove untracked files. Requires 'force'; add 'directories' for -d. Destructive.",
            schema_clean,
            build_clean,
            ro = false,
            dstr = true
        ),
        git_tool!(
            "git.submodule",
            "Run a submodule action: status (default), update, init, sync.",
            schema_submodule,
            build_submodule,
            ro = false,
            dstr = false
        ),
    ]
}

// ---- schema builders ----

fn repo_field(s: ObjectSchema) -> ObjectSchema {
    s.string("repo", "Repository path (default '.').", false)
        .integer("timeoutSecs", "Timeout override in seconds.", false)
}

fn schema_repo_only() -> Value {
    repo_field(ObjectSchema::new()).build()
}
fn schema_ref_required() -> Value {
    repo_field(ObjectSchema::new().string("ref", "Reference to operate on.", true)).build()
}
fn schema_diff() -> Value {
    repo_field(
        ObjectSchema::new()
            .string("from", "Base revision.", false)
            .string("to", "Target revision.", false)
            .string("path", "Limit to a path.", false)
            .boolean(
                "staged",
                "Diff the staged index instead of the worktree.",
                false,
            ),
    )
    .build()
}
fn schema_blame() -> Value {
    repo_field(ObjectSchema::new().string("path", "File to blame.", true)).build()
}
fn schema_log() -> Value {
    repo_field(
        ObjectSchema::new()
            .integer("maxCount", "Maximum commits (default 30).", false)
            .string("path", "Limit to a path.", false)
            .string("revRange", "Revision range, e.g. 'main..HEAD'.", false),
    )
    .build()
}
fn schema_branch() -> Value {
    repo_field(
        ObjectSchema::new()
            .enumerated("mode", "Operation.", &["list", "create", "delete"], false)
            .string("name", "Branch name (for create/delete).", false)
            .boolean("force", "Force delete.", false),
    )
    .build()
}
fn schema_checkout() -> Value {
    repo_field(
        ObjectSchema::new()
            .string("ref", "Branch, commit or path to checkout.", true)
            .boolean("create", "Create a new branch (-b).", false),
    )
    .build()
}
fn schema_rebase() -> Value {
    repo_field(
        ObjectSchema::new()
            .string("ref", "Upstream ref to rebase onto.", false)
            .enumerated(
                "control",
                "Control an in-progress rebase.",
                &["continue", "abort", "skip"],
                false,
            ),
    )
    .build()
}
fn schema_stash() -> Value {
    repo_field(
        ObjectSchema::new()
            .enumerated(
                "mode",
                "Stash action.",
                &["push", "pop", "list", "drop", "apply"],
                false,
            )
            .string("message", "Message for push.", false),
    )
    .build()
}
fn schema_tag() -> Value {
    repo_field(
        ObjectSchema::new()
            .string("name", "Tag name to create; omit to list.", false)
            .string("message", "Annotation message (annotated tag).", false),
    )
    .build()
}
fn schema_bisect() -> Value {
    repo_field(
        ObjectSchema::new()
            .enumerated(
                "action",
                "Bisect action.",
                &["start", "good", "bad", "reset"],
                true,
            )
            .string("rev", "Revision for good/bad.", false),
    )
    .build()
}
fn schema_commit() -> Value {
    repo_field(
        ObjectSchema::new()
            .string("message", "Commit message.", true)
            .boolean("all", "Stage all tracked changes first (-a).", false)
            .boolean("allowEmpty", "Allow an empty commit.", false),
    )
    .build()
}
fn schema_push() -> Value {
    repo_field(
        ObjectSchema::new()
            .string("remote", "Remote name (default origin).", false)
            .string("branch", "Branch to push.", false)
            .boolean("force", "Force with lease.", false)
            .boolean("setUpstream", "Set upstream (-u).", false),
    )
    .build()
}
fn schema_pull() -> Value {
    repo_field(
        ObjectSchema::new()
            .string("remote", "Remote name.", false)
            .string("branch", "Branch to pull.", false)
            .boolean("rebase", "Rebase instead of merge.", false),
    )
    .build()
}
fn schema_fetch() -> Value {
    repo_field(
        ObjectSchema::new()
            .string("remote", "Remote name (default origin).", false)
            .boolean("prune", "Prune deleted remote branches.", false)
            .boolean("all", "Fetch all remotes.", false),
    )
    .build()
}
fn schema_reset() -> Value {
    repo_field(
        ObjectSchema::new()
            .enumerated("mode", "Reset mode.", &["soft", "mixed", "hard"], false)
            .string("ref", "Target ref (default HEAD).", false),
    )
    .build()
}
fn schema_clean() -> Value {
    repo_field(
        ObjectSchema::new()
            .boolean("force", "Required to actually delete.", false)
            .boolean(
                "directories",
                "Also remove untracked directories (-d).",
                false,
            ),
    )
    .build()
}
fn schema_submodule() -> Value {
    repo_field(ObjectSchema::new().enumerated(
        "action",
        "Submodule action.",
        &["status", "update", "init", "sync"],
        false,
    ))
    .build()
}

// ---- arg builders ----

fn build_diff(a: &Args) -> Result<Vec<String>> {
    let mut v = vec!["diff".to_string()];
    if a.bool_or("staged", false)? {
        v.push("--staged".into());
    }
    if let Some(from) = a.opt_str("from")? {
        v.push(from.into());
    }
    if let Some(to) = a.opt_str("to")? {
        v.push(to.into());
    }
    if let Some(path) = a.opt_str("path")? {
        v.push("--".into());
        v.push(path.into());
    }
    Ok(v)
}

fn build_log(a: &Args) -> Result<Vec<String>> {
    let max = a.u64_or("maxCount", 30)?;
    let mut v = vec![
        "log".to_string(),
        format!("--max-count={max}"),
        "--date=iso-strict".into(),
        "--pretty=format:%H%x1f%an%x1f%ad%x1f%s".into(),
    ];
    if let Some(range) = a.opt_str("revRange")? {
        v.push(range.into());
    }
    if let Some(path) = a.opt_str("path")? {
        v.push("--".into());
        v.push(path.into());
    }
    Ok(v)
}

fn build_branch(a: &Args) -> Result<Vec<String>> {
    match a.str_or("mode", "list")? {
        "list" => Ok(vec![
            "branch".into(),
            "--all".into(),
            "--verbose".into(),
            "--no-color".into(),
        ]),
        "create" => Ok(vec!["branch".into(), require(a, "name")?]),
        "delete" => {
            let flag = if a.bool_or("force", false)? {
                "-D"
            } else {
                "-d"
            };
            Ok(vec!["branch".into(), flag.into(), require(a, "name")?])
        }
        other => Err(ToolError::InvalidArguments(format!(
            "unknown branch mode '{other}'"
        ))),
    }
}

fn build_checkout(a: &Args) -> Result<Vec<String>> {
    let mut v = vec!["checkout".to_string()];
    if a.bool_or("create", false)? {
        v.push("-b".into());
    }
    v.push(a.str("ref")?.into());
    Ok(v)
}

fn build_rebase(a: &Args) -> Result<Vec<String>> {
    if let Some(control) = a.opt_str("control")? {
        return Ok(vec!["rebase".into(), format!("--{control}")]);
    }
    let target = a
        .opt_str("ref")?
        .ok_or_else(|| ToolError::InvalidArguments("rebase requires 'ref' or 'control'".into()))?;
    Ok(vec!["rebase".into(), target.into()])
}

fn build_stash(a: &Args) -> Result<Vec<String>> {
    match a.str_or("mode", "push")? {
        "push" => {
            let mut v = vec!["stash".into(), "push".into()];
            if let Some(m) = a.opt_str("message")? {
                v.push("-m".into());
                v.push(m.into());
            }
            Ok(v)
        }
        "pop" => Ok(vec!["stash".into(), "pop".into()]),
        "list" => Ok(vec!["stash".into(), "list".into()]),
        "drop" => Ok(vec!["stash".into(), "drop".into()]),
        "apply" => Ok(vec!["stash".into(), "apply".into()]),
        other => Err(ToolError::InvalidArguments(format!(
            "unknown stash mode '{other}'"
        ))),
    }
}

fn build_tag(a: &Args) -> Result<Vec<String>> {
    match a.opt_str("name")? {
        None => Ok(vec!["tag".into(), "--list".into()]),
        Some(name) => {
            if let Some(msg) = a.opt_str("message")? {
                Ok(vec![
                    "tag".into(),
                    "-a".into(),
                    name.into(),
                    "-m".into(),
                    msg.into(),
                ])
            } else {
                Ok(vec!["tag".into(), name.into()])
            }
        }
    }
}

fn build_bisect(a: &Args) -> Result<Vec<String>> {
    let action = a.str("action")?;
    let mut v = vec!["bisect".into(), action.into()];
    if let Some(rev) = a.opt_str("rev")? {
        v.push(rev.into());
    }
    Ok(v)
}

fn build_commit(a: &Args) -> Result<Vec<String>> {
    let mut v = vec!["commit".to_string()];
    if a.bool_or("all", false)? {
        v.push("-a".into());
    }
    if a.bool_or("allowEmpty", false)? {
        v.push("--allow-empty".into());
    }
    v.push("-m".into());
    v.push(a.str("message")?.into());
    Ok(v)
}

fn build_push(a: &Args) -> Result<Vec<String>> {
    let mut v = vec!["push".to_string()];
    if a.bool_or("force", false)? {
        v.push("--force-with-lease".into());
    }
    if a.bool_or("setUpstream", false)? {
        v.push("-u".into());
    }
    v.push(a.str_or("remote", "origin")?.into());
    if let Some(b) = a.opt_str("branch")? {
        v.push(b.into());
    }
    Ok(v)
}

fn build_pull(a: &Args) -> Result<Vec<String>> {
    let mut v = vec!["pull".to_string()];
    if a.bool_or("rebase", false)? {
        v.push("--rebase".into());
    }
    if let Some(remote) = a.opt_str("remote")? {
        v.push(remote.into());
        if let Some(b) = a.opt_str("branch")? {
            v.push(b.into());
        }
    }
    Ok(v)
}

fn build_fetch(a: &Args) -> Result<Vec<String>> {
    let mut v = vec!["fetch".to_string()];
    if a.bool_or("prune", false)? {
        v.push("--prune".into());
    }
    if a.bool_or("all", false)? {
        v.push("--all".into());
    } else {
        v.push(a.str_or("remote", "origin")?.into());
    }
    Ok(v)
}

fn build_reset(a: &Args) -> Result<Vec<String>> {
    let mode = a.str_or("mode", "mixed")?;
    let mut v = vec!["reset".to_string(), format!("--{mode}")];
    v.push(a.str_or("ref", "HEAD")?.into());
    Ok(v)
}

fn build_clean(a: &Args) -> Result<Vec<String>> {
    if !a.bool_or("force", false)? {
        return Err(ToolError::InvalidArguments(
            "git.clean requires force=true".into(),
        ));
    }
    let flags = if a.bool_or("directories", false)? {
        "-fd"
    } else {
        "-f"
    };
    Ok(vec!["clean".into(), flags.into()])
}

fn build_submodule(a: &Args) -> Result<Vec<String>> {
    match a.str_or("action", "status")? {
        "status" => Ok(vec!["submodule".into(), "status".into()]),
        "update" => Ok(vec![
            "submodule".into(),
            "update".into(),
            "--init".into(),
            "--recursive".into(),
        ]),
        "init" => Ok(vec!["submodule".into(), "init".into()]),
        "sync" => Ok(vec![
            "submodule".into(),
            "sync".into(),
            "--recursive".into(),
        ]),
        other => Err(ToolError::InvalidArguments(format!(
            "unknown submodule action '{other}'"
        ))),
    }
}

fn require(a: &Args, key: &str) -> Result<String> {
    Ok(a.str(key)?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_mcp_core::config::SecurityConfig;
    use nebula_mcp_core::security::EffectivePolicy;
    use nebula_mcp_core::Metrics;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    fn ctx_in(dir: &std::path::Path) -> ToolContext {
        let base = SecurityConfig {
            allowed_paths: vec![format!("{}/**", dir.display()), dir.display().to_string()],
            allowed_commands: vec!["git".into()],
            default_timeout_secs: 30,
            max_runtime_secs: 60,
            max_output_bytes: 1 << 20,
            allow_destructive: true,
            ..Default::default()
        };
        let policy = EffectivePolicy::build("git", &base, None).unwrap();
        ToolContext {
            policy: Arc::new(policy),
            working_dir: dir.to_path_buf(),
            cancel: CancellationToken::new(),
            metrics: Metrics::new(),
            config: Arc::new(Default::default()),
            request_id: "r".into(),
        }
    }

    fn find(name: &str) -> Arc<dyn Tool> {
        tools().into_iter().find(|t| t.name() == name).unwrap()
    }

    #[test]
    fn arg_builders_are_correct() {
        let v = json!({"mode": "delete", "name": "feat", "force": true});
        let a = Args::new(&v).unwrap();
        assert_eq!(build_branch(&a).unwrap(), vec!["branch", "-D", "feat"]);

        let v = json!({"message": "msg", "all": true});
        let a = Args::new(&v).unwrap();
        assert_eq!(build_commit(&a).unwrap(), vec!["commit", "-a", "-m", "msg"]);

        let v = json!({"force": false});
        let a = Args::new(&v).unwrap();
        assert!(build_clean(&a).is_err());
    }

    #[tokio::test]
    async fn status_on_real_repo() {
        if which::which("git").is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        // init a repo
        let init = crate::common::exec::CommandSpec::new("git", dir.path(), &ctx).args(vec![
            "-C",
            &dir.path().display().to_string(),
            "init",
        ]);
        crate::common::exec::run_checked(&ctx, init, None)
            .await
            .unwrap();
        let res = find("git.status")
            .call(&ctx, json!({"repo": "."}))
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(false));
    }

    #[tokio::test]
    async fn reset_hard_denied_without_destructive() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_in(dir.path());
        // strip destructive
        let base = SecurityConfig {
            allowed_paths: vec![
                format!("{}/**", dir.path().display()),
                dir.path().display().to_string(),
            ],
            allowed_commands: vec!["git".into()],
            allow_destructive: false,
            ..Default::default()
        };
        ctx.policy = Arc::new(EffectivePolicy::build("git.reset", &base, None).unwrap());
        let err = find("git.reset")
            .call(&ctx, json!({"repo": ".", "mode": "hard"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied(_)));
    }
}

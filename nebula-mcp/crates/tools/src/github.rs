//! GitHub tools.
//!
//! Backed by the GitHub REST API (via the shared reqwest client) and, for
//! cloning, the `git` CLI. Authentication uses a token read from the
//! `GITHUB_TOKEN` or `GH_TOKEN` environment variable; the token value is never
//! echoed back in results. A generic [`github.request`](GitHubRequest) tool
//! exposes the whole API surface, while convenience tools cover the common
//! flows (PRs, issues, forks, releases, actions, branches, reviews, labels).

use std::sync::Arc;

use async_trait::async_trait;
use nebula_mcp_core::{Result, Tool, ToolContext, ToolError};
use nebula_mcp_protocol::mcp::ToolAnnotations;
use nebula_mcp_protocol::CallToolResult;
use serde_json::{json, Value};

use crate::common::exec::{run_checked, CommandSpec};
use crate::common::output::{exec_result, json_value_result};
use crate::common::{Args, ObjectSchema};
use crate::ToolServices;

const CATEGORY: &str = "github";
const API_BASE: &str = "https://api.github.com";

/// Build GitHub tools.
pub fn tools(services: &ToolServices) -> Vec<Arc<dyn Tool>> {
    let http = services.http.clone();
    vec![
        Arc::new(GitHubClone),
        Arc::new(GitHubRequest { http: http.clone() }),
        Arc::new(GitHubPrList { http: http.clone() }),
        Arc::new(GitHubPrCreate { http: http.clone() }),
        Arc::new(GitHubIssueList { http: http.clone() }),
        Arc::new(GitHubIssueCreate { http: http.clone() }),
        Arc::new(GitHubFork { http: http.clone() }),
        Arc::new(GitHubReleaseList { http: http.clone() }),
        Arc::new(GitHubReleaseCreate { http: http.clone() }),
        Arc::new(GitHubWorkflowRuns { http: http.clone() }),
        Arc::new(GitHubBranchList { http: http.clone() }),
        Arc::new(GitHubReviewCreate { http: http.clone() }),
        Arc::new(GitHubLabelsList { http }),
    ]
}

/// Read a GitHub token from the environment.
fn token() -> Option<String> {
    std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .ok()
        .filter(|s| !s.is_empty())
}

/// Perform an authenticated GitHub REST request.
async fn api(
    http: &reqwest::Client,
    ctx: &ToolContext,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> Result<Value> {
    ctx.policy.ensure_network_allowed()?;
    let url = if path.starts_with("http") {
        path.to_string()
    } else {
        format!("{API_BASE}/{}", path.trim_start_matches('/'))
    };
    let mut req = http
        .request(method, &url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(tok) = token() {
        req = req.bearer_auth(tok);
    }
    if let Some(b) = &body {
        req = req.json(b);
    }
    let timeout = ctx.timeout(None);
    let fut = async {
        let resp = req
            .send()
            .await
            .map_err(|e| ToolError::Execution(format!("github request failed: {e}")))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ToolError::Execution(format!("reading github response: {e}")))?;
        let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));
        if status.is_success() {
            Ok(parsed)
        } else {
            Err(ToolError::Execution(format!(
                "github API returned {}: {}",
                status,
                summarize(&parsed)
            )))
        }
    };
    ctx.guarded(timeout, fut).await
}

fn summarize(v: &Value) -> String {
    v.get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            let s = v.to_string();
            s.chars().take(500).collect()
        })
}

// ---- clone (git) ----

struct GitHubClone;

#[async_trait]
impl Tool for GitHubClone {
    fn name(&self) -> &str {
        "github.clone"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Clone a repository into a directory within an allowed root (uses the git CLI)."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .string("url", "Repository URL to clone.", true)
            .string(
                "dir",
                "Destination directory (within an allowed root).",
                true,
            )
            .integer("depth", "Optional shallow clone depth.", false)
            .build()
    }
    fn annotations(&self) -> Option<ToolAnnotations> {
        Some(ToolAnnotations {
            open_world_hint: Some(true),
            ..Default::default()
        })
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        ctx.policy.ensure_network_allowed()?;
        let url = a.str("url")?.to_string();
        let dir = ctx.resolve_path(a.str("dir")?)?;
        let mut git_args = vec!["clone".to_string()];
        if let Some(depth) = a.opt_u64("depth")? {
            git_args.push("--depth".into());
            git_args.push(depth.to_string());
        }
        git_args.push(url.clone());
        git_args.push(dir.display().to_string());
        let spec = CommandSpec::new("git", ctx.working_dir.clone(), ctx).args(git_args);
        let result = run_checked(ctx, spec, a.opt_u64("timeoutSecs")?).await?;
        Ok(exec_result(&format!("git clone {url}"), &result))
    }
}

// ---- generic request ----

struct GitHubRequest {
    http: reqwest::Client,
}

#[async_trait]
impl Tool for GitHubRequest {
    fn name(&self) -> &str {
        "github.request"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Make an authenticated GitHub REST API request. Covers any endpoint (PRs, issues, actions, releases, reviews, labels, ...)."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .enumerated(
                "method",
                "HTTP method.",
                &["GET", "POST", "PATCH", "PUT", "DELETE"],
                false,
            )
            .string("path", "API path, e.g. 'repos/owner/name/pulls'.", true)
            .prop(
                "body",
                json!({"type": "object", "description": "JSON body for write requests."}),
                false,
            )
            .build()
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
        let a = Args::new(&args)?;
        let method = parse_method(a.str_or("method", "GET")?)?;
        let path = a.str("path")?;
        let body = a.opt_value("body").cloned();
        let v = api(&self.http, ctx, method, path, body).await?;
        Ok(json_value_result(v))
    }
}

fn parse_method(m: &str) -> Result<reqwest::Method> {
    match m.to_ascii_uppercase().as_str() {
        "GET" => Ok(reqwest::Method::GET),
        "POST" => Ok(reqwest::Method::POST),
        "PATCH" => Ok(reqwest::Method::PATCH),
        "PUT" => Ok(reqwest::Method::PUT),
        "DELETE" => Ok(reqwest::Method::DELETE),
        other => Err(ToolError::InvalidArguments(format!(
            "unsupported HTTP method '{other}'"
        ))),
    }
}

// ---- convenience tools ----

macro_rules! http_tool {
    ($ty:ident, $name:literal, $cat_desc:literal, $schema:expr, $ro:literal, $run:expr) => {
        struct $ty {
            http: reqwest::Client,
        }
        #[async_trait]
        impl Tool for $ty {
            fn name(&self) -> &str {
                $name
            }
            fn category(&self) -> &str {
                CATEGORY
            }
            fn description(&self) -> &str {
                $cat_desc
            }
            fn input_schema(&self) -> Value {
                ($schema)()
            }
            fn annotations(&self) -> Option<ToolAnnotations> {
                Some(ToolAnnotations {
                    read_only_hint: Some($ro),
                    open_world_hint: Some(true),
                    ..Default::default()
                })
            }
            async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
                let a = Args::new(&args)?;
                let run: fn(
                    &reqwest::Client,
                    &ToolContext,
                    &Args,
                ) -> std::pin::Pin<
                    Box<dyn std::future::Future<Output = Result<Value>> + Send>,
                > = $run;
                let v = run(&self.http, ctx, &a).await?;
                Ok(json_value_result(v))
            }
        }
    };
}

fn schema_owner_repo() -> Value {
    ObjectSchema::new()
        .string("owner", "Repository owner.", true)
        .string("repo", "Repository name.", true)
        .build()
}

http_tool!(
    GitHubPrList,
    "github.pr_list",
    "List pull requests for a repository (optional 'state': open/closed/all).",
    || ObjectSchema::new()
        .string("owner", "Owner.", true)
        .string("repo", "Repo.", true)
        .enumerated("state", "State filter.", &["open", "closed", "all"], false)
        .build(),
    true,
    |http, ctx, a| {
        let http = http.clone();
        let ctx = ctx.clone();
        let owner = a.str("owner").map(str::to_string);
        let repo = a.str("repo").map(str::to_string);
        let state = a.str_or("state", "open").map(str::to_string);
        Box::pin(async move {
            let path = format!("repos/{}/{}/pulls?state={}", owner?, repo?, state?);
            api(&http, &ctx, reqwest::Method::GET, &path, None).await
        })
    }
);

http_tool!(
    GitHubPrCreate,
    "github.pr_create",
    "Open a pull request. Requires owner, repo, title, head, base.",
    || ObjectSchema::new()
        .string("owner", "Owner.", true)
        .string("repo", "Repo.", true)
        .string("title", "PR title.", true)
        .string("head", "Head branch.", true)
        .string("base", "Base branch.", true)
        .string("body", "PR description.", false)
        .boolean("draft", "Open as draft.", false)
        .build(),
    false,
    |http, ctx, a| {
        let http = http.clone();
        let ctx = ctx.clone();
        let owner = a.str("owner").map(str::to_string);
        let repo = a.str("repo").map(str::to_string);
        let title = a.str("title").map(str::to_string);
        let head = a.str("head").map(str::to_string);
        let base = a.str("base").map(str::to_string);
        let body = a.str_or("body", "").map(str::to_string);
        let draft = a.bool_or("draft", false);
        Box::pin(async move {
            let payload = json!({
                "title": title?, "head": head?, "base": base?,
                "body": body?, "draft": draft?,
            });
            let path = format!("repos/{}/{}/pulls", owner?, repo?);
            api(&http, &ctx, reqwest::Method::POST, &path, Some(payload)).await
        })
    }
);

http_tool!(
    GitHubIssueList,
    "github.issue_list",
    "List issues for a repository.",
    || ObjectSchema::new()
        .string("owner", "Owner.", true)
        .string("repo", "Repo.", true)
        .enumerated("state", "State filter.", &["open", "closed", "all"], false)
        .build(),
    true,
    |http, ctx, a| {
        let http = http.clone();
        let ctx = ctx.clone();
        let owner = a.str("owner").map(str::to_string);
        let repo = a.str("repo").map(str::to_string);
        let state = a.str_or("state", "open").map(str::to_string);
        Box::pin(async move {
            let path = format!("repos/{}/{}/issues?state={}", owner?, repo?, state?);
            api(&http, &ctx, reqwest::Method::GET, &path, None).await
        })
    }
);

http_tool!(
    GitHubIssueCreate,
    "github.issue_create",
    "Create an issue. Requires owner, repo, title.",
    || ObjectSchema::new()
        .string("owner", "Owner.", true)
        .string("repo", "Repo.", true)
        .string("title", "Issue title.", true)
        .string("body", "Issue body.", false)
        .string_array("labels", "Labels to apply.", false)
        .build(),
    false,
    |http, ctx, a| {
        let http = http.clone();
        let ctx = ctx.clone();
        let owner = a.str("owner").map(str::to_string);
        let repo = a.str("repo").map(str::to_string);
        let title = a.str("title").map(str::to_string);
        let body = a.str_or("body", "").map(str::to_string);
        let labels = a.opt_str_array("labels");
        Box::pin(async move {
            let payload = json!({"title": title?, "body": body?, "labels": labels?});
            let path = format!("repos/{}/{}/issues", owner?, repo?);
            api(&http, &ctx, reqwest::Method::POST, &path, Some(payload)).await
        })
    }
);

http_tool!(
    GitHubFork,
    "github.fork",
    "Fork a repository into the authenticated account or an org.",
    || ObjectSchema::new()
        .string("owner", "Owner.", true)
        .string("repo", "Repo.", true)
        .string("organization", "Target org (optional).", false)
        .build(),
    false,
    |http, ctx, a| {
        let http = http.clone();
        let ctx = ctx.clone();
        let owner = a.str("owner").map(str::to_string);
        let repo = a.str("repo").map(str::to_string);
        let org = a.opt_str("organization").map(|o| o.map(str::to_string));
        Box::pin(async move {
            let payload = match org? {
                Some(o) => json!({ "organization": o }),
                None => json!({}),
            };
            let path = format!("repos/{}/{}/forks", owner?, repo?);
            api(&http, &ctx, reqwest::Method::POST, &path, Some(payload)).await
        })
    }
);

http_tool!(
    GitHubReleaseList,
    "github.release_list",
    "List releases for a repository.",
    schema_owner_repo,
    true,
    |http, ctx, a| {
        let http = http.clone();
        let ctx = ctx.clone();
        let owner = a.str("owner").map(str::to_string);
        let repo = a.str("repo").map(str::to_string);
        Box::pin(async move {
            let path = format!("repos/{}/{}/releases", owner?, repo?);
            api(&http, &ctx, reqwest::Method::GET, &path, None).await
        })
    }
);

http_tool!(
    GitHubReleaseCreate,
    "github.release_create",
    "Create a release from a tag.",
    || ObjectSchema::new()
        .string("owner", "Owner.", true)
        .string("repo", "Repo.", true)
        .string("tag", "Tag name.", true)
        .string("name", "Release name.", false)
        .string("body", "Release notes.", false)
        .boolean("draft", "Create as draft.", false)
        .boolean("prerelease", "Mark as prerelease.", false)
        .build(),
    false,
    |http, ctx, a| {
        let http = http.clone();
        let ctx = ctx.clone();
        let owner = a.str("owner").map(str::to_string);
        let repo = a.str("repo").map(str::to_string);
        let tag = a.str("tag").map(str::to_string);
        let name = a.str_or("name", "").map(str::to_string);
        let body = a.str_or("body", "").map(str::to_string);
        let draft = a.bool_or("draft", false);
        let prerelease = a.bool_or("prerelease", false);
        Box::pin(async move {
            let payload = json!({
                "tag_name": tag?, "name": name?, "body": body?,
                "draft": draft?, "prerelease": prerelease?,
            });
            let path = format!("repos/{}/{}/releases", owner?, repo?);
            api(&http, &ctx, reqwest::Method::POST, &path, Some(payload)).await
        })
    }
);

http_tool!(
    GitHubWorkflowRuns,
    "github.workflow_runs",
    "List GitHub Actions workflow runs for a repository.",
    || ObjectSchema::new()
        .string("owner", "Owner.", true)
        .string("repo", "Repo.", true)
        .integer("perPage", "Results per page (default 20).", false)
        .build(),
    true,
    |http, ctx, a| {
        let http = http.clone();
        let ctx = ctx.clone();
        let owner = a.str("owner").map(str::to_string);
        let repo = a.str("repo").map(str::to_string);
        let per = a.u64_or("perPage", 20);
        Box::pin(async move {
            let path = format!("repos/{}/{}/actions/runs?per_page={}", owner?, repo?, per?);
            api(&http, &ctx, reqwest::Method::GET, &path, None).await
        })
    }
);

http_tool!(
    GitHubBranchList,
    "github.branch_list",
    "List branches for a repository.",
    schema_owner_repo,
    true,
    |http, ctx, a| {
        let http = http.clone();
        let ctx = ctx.clone();
        let owner = a.str("owner").map(str::to_string);
        let repo = a.str("repo").map(str::to_string);
        Box::pin(async move {
            let path = format!("repos/{}/{}/branches", owner?, repo?);
            api(&http, &ctx, reqwest::Method::GET, &path, None).await
        })
    }
);

http_tool!(
    GitHubReviewCreate,
    "github.review_create",
    "Submit a review on a pull request (event: APPROVE/REQUEST_CHANGES/COMMENT).",
    || ObjectSchema::new()
        .string("owner", "Owner.", true)
        .string("repo", "Repo.", true)
        .integer("pull", "Pull request number.", true)
        .enumerated(
            "event",
            "Review event.",
            &["APPROVE", "REQUEST_CHANGES", "COMMENT"],
            true
        )
        .string("body", "Review body.", false)
        .build(),
    false,
    |http, ctx, a| {
        let http = http.clone();
        let ctx = ctx.clone();
        let owner = a.str("owner").map(str::to_string);
        let repo = a.str("repo").map(str::to_string);
        let pull = a.opt_u64("pull").map(|o| o.unwrap_or(0));
        let event = a.str("event").map(str::to_string);
        let body = a.str_or("body", "").map(str::to_string);
        Box::pin(async move {
            let payload = json!({"event": event?, "body": body?});
            let path = format!("repos/{}/{}/pulls/{}/reviews", owner?, repo?, pull?);
            api(&http, &ctx, reqwest::Method::POST, &path, Some(payload)).await
        })
    }
);

http_tool!(
    GitHubLabelsList,
    "github.labels_list",
    "List labels defined in a repository.",
    schema_owner_repo,
    true,
    |http, ctx, a| {
        let http = http.clone();
        let ctx = ctx.clone();
        let owner = a.str("owner").map(str::to_string);
        let repo = a.str("repo").map(str::to_string);
        Box::pin(async move {
            let path = format!("repos/{}/{}/labels", owner?, repo?);
            api(&http, &ctx, reqwest::Method::GET, &path, None).await
        })
    }
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_parsing() {
        assert_eq!(parse_method("post").unwrap(), reqwest::Method::POST);
        assert!(parse_method("frobnicate").is_err());
    }

    #[test]
    fn summarize_prefers_message() {
        let v = json!({"message": "Not Found"});
        assert_eq!(summarize(&v), "Not Found");
    }

    #[test]
    fn all_github_tools_have_unique_names() {
        let services = crate::ToolServices::new();
        let names: Vec<String> = tools(&services)
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(names.len(), sorted.len());
    }
}

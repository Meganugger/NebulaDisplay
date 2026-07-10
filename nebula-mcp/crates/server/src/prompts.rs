//! MCP `prompts/*` support: a curated set of reusable engineering prompts that
//! guide the agent through common autonomous workflows using this server's
//! tools.

use nebula_mcp_core::ToolError;
use nebula_mcp_protocol::{
    Content, GetPromptResult, ListPromptsResult, Prompt, PromptArgument, PromptMessage,
};
use serde_json::Value;

/// Definition of a built-in prompt.
struct PromptDef {
    name: &'static str,
    description: &'static str,
    args: &'static [(&'static str, &'static str, bool)],
    render: fn(&Value) -> String,
}

fn arg<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or("")
}

const PROMPTS: &[PromptDef] = &[
    PromptDef {
        name: "triage_crash_dump",
        description: "Investigate a Windows crash dump end-to-end and propose a root cause.",
        args: &[("dumpPath", "Path to the .dmp file", true)],
        render: |a| {
            format!(
                "Investigate the crash dump at `{dump}`.\n\n\
                 1. Run `diagnostics.analyze_dump` on it (`!analyze -v`).\n\
                 2. Identify the faulting module, exception code and call stack.\n\
                 3. If it is a driver, correlate with `driver.logs` and `diagnostics.wer_reports`.\n\
                 4. Inspect the relevant source with `fs.read`/`fs.search`.\n\
                 5. Summarise the most likely root cause and a concrete fix.",
                dump = arg(a, "dumpPath")
            )
        },
    },
    PromptDef {
        name: "investigate_build_failure",
        description: "Reproduce and diagnose a failing build or test run.",
        args: &[
            ("repo", "Repository path", true),
            ("command", "Build/test command (program then args)", true),
        ],
        render: |a| {
            format!(
                "A build/test is failing in `{repo}`.\n\n\
                 1. Reproduce it with `terminal.run` using `{cmd}` (pass a progressToken to stream output).\n\
                 2. Read the error output; use `fs.search` to locate the offending symbols.\n\
                 3. Inspect the failing files with `fs.read` and recent history with `git.log`/`git.blame`.\n\
                 4. Propose and apply a minimal fix, then re-run the command to confirm it passes.",
                repo = arg(a, "repo"),
                cmd = arg(a, "command")
            )
        },
    },
    PromptDef {
        name: "review_pull_request",
        description: "Review a GitHub pull request and submit structured feedback.",
        args: &[
            ("owner", "Repository owner", true),
            ("repo", "Repository name", true),
            ("pull", "Pull request number", true),
        ],
        render: |a| {
            format!(
                "Review pull request #{pull} in {owner}/{repo}.\n\n\
                 1. Fetch it via `github.request` (`GET repos/{owner}/{repo}/pulls/{pull}` and its files).\n\
                 2. Assess correctness, tests, security and style.\n\
                 3. Submit the review with `github.review_create` \
                 (APPROVE / REQUEST_CHANGES / COMMENT) and a clear rationale.",
                owner = arg(a, "owner"),
                repo = arg(a, "repo"),
                pull = arg(a, "pull")
            )
        },
    },
    PromptDef {
        name: "bisect_regression",
        description: "Find the commit that introduced a regression using git bisect.",
        args: &[
            ("repo", "Repository path", true),
            ("good", "Known-good revision", true),
            ("bad", "Known-bad revision (e.g. HEAD)", true),
            ("test", "Command that fails on the regression", true),
        ],
        render: |a| {
            format!(
                "Find the commit that introduced a regression in `{repo}`.\n\n\
                 1. `git.bisect` start; mark `{good}` good and `{bad}` bad.\n\
                 2. At each step run `{test}` via `terminal.run` and mark good/bad accordingly.\n\
                 3. When bisect converges, `git.show` the culprit commit and explain the regression.",
                repo = arg(a, "repo"),
                good = arg(a, "good"),
                bad = arg(a, "bad"),
                test = arg(a, "test")
            )
        },
    },
    PromptDef {
        name: "diagnose_display_pipeline",
        description: "Diagnose a virtual-display / IddCx rendering or driver issue.",
        args: &[],
        render: |_a| {
            "Diagnose the display pipeline on this machine.\n\n\
             1. `display.query_config` and `display.monitors` for the active topology.\n\
             2. `display.dxgi_adapters` for GPU/output health.\n\
             3. `driver.iddcx_diagnostics` for indirect (virtual) display driver state.\n\
             4. `display.present_stats` for composition/timing.\n\
             5. If a virtual display misbehaves, check `driver.logs` and consider `driver.display_restart`.\n\
             6. Capture a frame with `display.duplicate_frame` to confirm what is actually on screen.\n\
             Summarise findings and the most likely fix."
                .to_string()
        },
    },
];

/// Build the `prompts/list` result.
pub fn list() -> ListPromptsResult {
    ListPromptsResult {
        prompts: PROMPTS
            .iter()
            .map(|p| Prompt {
                name: p.name.to_string(),
                description: Some(p.description.to_string()),
                arguments: p
                    .args
                    .iter()
                    .map(|(name, desc, req)| PromptArgument {
                        name: name.to_string(),
                        description: Some(desc.to_string()),
                        required: Some(*req),
                    })
                    .collect(),
            })
            .collect(),
    }
}

/// Build the `prompts/get` result for `name` with `arguments`.
pub fn get(name: &str, arguments: &Value) -> Result<GetPromptResult, ToolError> {
    let def = PROMPTS
        .iter()
        .find(|p| p.name == name)
        .ok_or_else(|| ToolError::InvalidArguments(format!("unknown prompt '{name}'")))?;
    // Validate required arguments are present.
    for (arg_name, _, required) in def.args {
        if *required && arguments.get(arg_name).and_then(Value::as_str).is_none() {
            return Err(ToolError::InvalidArguments(format!(
                "prompt '{name}' requires argument '{arg_name}'"
            )));
        }
    }
    let text = (def.render)(arguments);
    Ok(GetPromptResult {
        description: Some(def.description.to_string()),
        messages: vec![PromptMessage {
            role: "user".to_string(),
            content: Content::text(text),
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lists_prompts() {
        assert!(list().prompts.len() >= 5);
    }

    #[test]
    fn get_requires_arguments() {
        assert!(get("triage_crash_dump", &json!({})).is_err());
        let r = get("triage_crash_dump", &json!({"dumpPath": "/tmp/x.dmp"})).unwrap();
        assert!(matches!(&r.messages[0].content, Content::Text { .. }));
    }

    #[test]
    fn unknown_prompt_errors() {
        assert!(get("nope", &json!({})).is_err());
    }
}

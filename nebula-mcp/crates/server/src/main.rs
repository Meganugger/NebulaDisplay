//! NebulaDisplay MCP server binary.
//!
//! Speaks MCP over stdio (stdout is reserved for the protocol; all logs go to
//! stderr and, optionally, rotating files). See `nebula-mcp --help`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use nebula_mcp_core::config::{Config, ConfigStore};
use nebula_mcp_core::telemetry;
use nebula_mcp_server::server::Server;
use nebula_mcp_tools::{build_registry, ToolServices};
use tokio_util::sync::CancellationToken;

/// NebulaDisplay MCP server — a Windows autonomous-engineering backend.
#[derive(Parser, Debug)]
#[command(name = "nebula-mcp", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to the TOML configuration file.
    #[arg(short, long, global = true, env = "NEBULA_MCP_CONFIG")]
    config: Option<PathBuf>,

    /// Workspace root directory that relative tool paths resolve against.
    #[arg(short, long, global = true, env = "NEBULA_MCP_WORKDIR")]
    workdir: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the MCP server over stdio (default).
    Run,
    /// Print every tool with its description and category.
    ListTools,
    /// Print a documented default configuration to stdout.
    PrintConfig,
    /// Validate a configuration file and report a summary.
    ValidateConfig,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command.as_ref().unwrap_or(&Command::Run) {
        Command::PrintConfig => print_default_config(),
        Command::ListTools => list_tools(),
        Command::ValidateConfig => validate_config(cli.config.as_deref()),
        Command::Run => run(cli),
    }
}

/// Load configuration from disk, or fall back to the built-in default.
fn load_config(path: Option<&std::path::Path>) -> anyhow::Result<ConfigStore> {
    match path {
        Some(p) => ConfigStore::from_path(p)
            .with_context(|| format!("loading configuration from {}", p.display())),
        None => Ok(ConfigStore::new(Config::default())),
    }
}

fn print_default_config() -> anyhow::Result<()> {
    let text = Config::default()
        .to_toml_string()
        .context("serialising default configuration")?;
    println!("{text}");
    Ok(())
}

fn list_tools() -> anyhow::Result<()> {
    let services = ToolServices::new();
    let tools = nebula_mcp_tools::all_tools(&services);
    println!("{} tools registered:\n", tools.len());
    let mut by_category: std::collections::BTreeMap<&str, Vec<(&str, &str)>> =
        std::collections::BTreeMap::new();
    for t in &tools {
        by_category
            .entry(t.category())
            .or_default()
            .push((t.name(), t.description()));
    }
    for (cat, items) in by_category {
        println!("[{cat}]");
        for (name, desc) in items {
            println!("  {name:<28} {desc}");
        }
        println!();
    }
    Ok(())
}

fn validate_config(path: Option<&std::path::Path>) -> anyhow::Result<()> {
    let path = path.context("--config is required for validate-config")?;
    let store =
        ConfigStore::from_path(path).with_context(|| format!("loading {}", path.display()))?;
    let config = store.snapshot();
    println!("Configuration at {} is valid.", path.display());
    println!("  server.name          = {}", config.server.name);
    println!(
        "  allowed_paths        = {}",
        config.security.allowed_paths.len()
    );
    println!(
        "  allowed_commands     = {}",
        config.security.allowed_commands.len()
    );
    println!(
        "  allow_elevated       = {}",
        config.security.allow_elevated
    );
    println!(
        "  allow_destructive    = {}",
        config.security.allow_destructive
    );
    println!("  allow_network        = {}", config.security.allow_network);
    println!(
        "  default_timeout_secs = {}",
        config.security.default_timeout_secs
    );
    println!(
        "  max_runtime_secs     = {}",
        config.security.max_runtime_secs
    );
    Ok(())
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let config = load_config(cli.config.as_deref())?;

    // Initialise telemetry before building the runtime so early logs are captured.
    let (_telemetry, log_control) = telemetry::init(&config.snapshot().logging)
        .map_err(|e| anyhow::anyhow!("telemetry init failed: {e}"))?;

    let workdir = cli
        .workdir
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building Tokio runtime")?;

    runtime.block_on(async move {
        let services = ToolServices::new();
        let registry = build_registry(&services);
        tracing::info!(
            tools = registry.len(),
            workdir = %workdir.display(),
            config = ?cli.config,
            "starting NebulaDisplay MCP server"
        );

        let root_cancel = CancellationToken::new();

        // Optional config hot reload when backed by a file.
        let _reload = if config.source_path().is_some() {
            match nebula_mcp_core::hotreload::watch(config.clone()) {
                Ok(handle) => Some(handle),
                Err(e) => {
                    tracing::warn!(error = %e, "config hot reload unavailable");
                    None
                }
            }
        } else {
            None
        };

        // Wire OS signals to the root cancellation token.
        spawn_signal_handler(root_cancel.clone());

        let server = Arc::new(Server::new(
            registry,
            config.clone(),
            workdir,
            root_cancel.clone(),
        ));
        server.set_log_control(log_control);

        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        let result = server.clone().serve(stdin, stdout).await;

        if config.snapshot().logging.emit_metrics {
            for m in server.metrics().snapshot() {
                tracing::info!(
                    tool = %m.tool,
                    calls = m.calls,
                    successes = m.successes,
                    failures = m.failures,
                    cancellations = m.cancellations,
                    mean_us = m.mean_duration_us,
                    max_us = m.max_duration_us,
                    "tool metrics"
                );
            }
        }
        tracing::info!("server stopped");
        result
    })
}

/// Cancel the root token on Ctrl-C (and SIGTERM on Unix).
fn spawn_signal_handler(cancel: CancellationToken) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut term = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to install SIGTERM handler");
                    return;
                }
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => tracing::info!("received Ctrl-C"),
                _ = term.recv() => tracing::info!("received SIGTERM"),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("received Ctrl-C");
        }
        cancel.cancel();
    });
}

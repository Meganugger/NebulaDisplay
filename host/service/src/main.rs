//! `nebulad` — the NebulaDisplay host service binary.

use clap::Parser;
use nebulad::{capture, config, discovery, panel, server, state, util};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info};

#[derive(Parser, Debug)]
#[command(name = "nebulad", version, about = "NebulaDisplay host service")]
pub struct Args {
    /// Viewer HTTP/WebSocket port (LAN-facing).
    #[arg(long, default_value_t = ndsp_protocol::DEFAULT_PORT)]
    pub port: u16,
    /// Control panel port (bound to 127.0.0.1 only).
    #[arg(long, default_value_t = ndsp_protocol::DEFAULT_PANEL_PORT)]
    pub panel_port: u16,
    /// UDP discovery port (0 disables discovery).
    #[arg(long, default_value_t = ndsp_protocol::DEFAULT_DISCOVERY_PORT)]
    pub discovery_port: u16,
    /// Address to bind the viewer endpoint on.
    #[arg(long, default_value = "0.0.0.0")]
    pub bind: IpAddr,
    /// Host display name announced to viewers.
    #[arg(long)]
    pub name: Option<String>,
    /// Data directory for config/trust store (default: OS config dir).
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    /// Directory containing the built web viewer (index.html, panel.html, …).
    #[arg(long)]
    pub web_dir: Option<PathBuf>,
    /// Force the synthetic test-pattern source even on Windows.
    #[arg(long)]
    pub test_pattern: bool,
    /// Capture size for the test pattern source, e.g. 1280x720.
    #[arg(long, default_value = "1280x720")]
    pub capture_size: String,
    /// Which virtual display (IddCx driver monitor index) to stream when the
    /// driver exposes several. Ignored in mirror/test-pattern modes.
    #[arg(long, default_value_t = 0)]
    pub display_index: u32,
    /// Capture and stream host audio (same as `audio = true` in config).
    /// Viewers still opt in per session; off by default.
    #[arg(long)]
    pub audio: bool,
    /// Serve the viewer endpoint over HTTPS with a persistent self-signed
    /// certificate (fingerprint printed at startup for pinning).
    #[arg(long)]
    pub https: bool,
    /// Exit after N seconds (for smoke tests).
    #[arg(long)]
    pub exit_after: Option<u64>,
}

impl From<&Args> for config::LoadArgs {
    fn from(a: &Args) -> Self {
        config::LoadArgs {
            name: a.name.clone(),
            data_dir: a.data_dir.clone(),
            web_dir: a.web_dir.clone(),
        }
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,nebulad=debug".into()),
        )
        .init();

    let args = Args::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run(args))
}

async fn run(args: Args) -> anyhow::Result<()> {
    let mut cfg = config::Config::load(&(&args).into())?;
    if args.audio {
        cfg.file.audio = true;
    }
    if args.https {
        cfg.file.https = true;
    }
    info!(name = %cfg.name, data_dir = %cfg.data_dir.display(), "starting nebulad v{}", env!("CARGO_PKG_VERSION"));

    let state = Arc::new(state::AppState::new(cfg).await?);

    // Capture → broadcast pipeline.
    let (w, h) = util::parse_size(&args.capture_size)?;
    let source = capture::create_source(args.test_pattern, w, h, args.display_index);
    let capture_handle = tokio::spawn(capture::run_capture_loop(state.clone(), source));

    // Audio pipeline (WASAPI loopback / test tone), only when enabled in
    // config — and even then each viewer must opt in per session.
    if nebulad::audio::spawn_if_enabled(state.clone()) {
        state
            .audio_available
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    // Host clipboard watcher (Windows) — publishes host copies to granted
    // viewers. Grant checks happen per session; nothing leaves the machine
    // for devices without the clipboard permission.
    nebulad::clipboard::spawn_watcher(state.clone());

    // UDP discovery responder.
    if args.discovery_port != 0 {
        tokio::spawn(discovery::run(state.clone(), args.discovery_port));
    }

    // Loopback control panel.
    let panel_state = state.clone();
    let panel_port = args.panel_port;
    tokio::spawn(async move {
        if let Err(e) = panel::run(panel_state, panel_port).await {
            error!("panel server failed: {e:#}");
        }
    });

    // LAN-facing viewer endpoint (HTTP static + NDSP WebSocket).
    let server = tokio::spawn(server::run(state.clone(), args.bind, args.port));

    print_banner(&state, args.port, args.panel_port);

    if let Some(secs) = args.exit_after {
        tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
        info!("exit_after elapsed; shutting down");
        return Ok(());
    }

    tokio::select! {
        r = server => r??,
        r = capture_handle => r?,
        _ = tokio::signal::ctrl_c() => info!("ctrl-c received; shutting down"),
    }
    Ok(())
}

fn print_banner(state: &state::AppState, port: u16, panel_port: u16) {
    let pin = state.pins.current_pin();
    let ips = util::local_ips();
    let scheme = if state.cfg.file.https {
        "https"
    } else {
        "http"
    };
    println!("\n  NebulaDisplay host ready");
    println!("  ── Viewer URLs ─────────────────────────────");
    for ip in &ips {
        println!("     {scheme}://{ip}:{port}/");
    }
    if ips.is_empty() {
        println!("     {scheme}://<this-machine-ip>:{port}/");
    }
    if state.cfg.file.https {
        // Give the TLS listener a moment to publish the fingerprint.
        for _ in 0..50 {
            if state.tls_cert_fingerprint.lock().unwrap().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if let Some(fp) = state.tls_cert_fingerprint.lock().unwrap().clone() {
            println!("  ── HTTPS certificate SHA-256 (verify once) ─");
            println!("     {fp}");
        }
    }
    println!("  ── Pairing PIN (single-use) ────────────────");
    println!("     {pin}");
    println!("  ── Control panel (this machine only) ───────");
    println!("     http://127.0.0.1:{panel_port}/panel.html\n");
}

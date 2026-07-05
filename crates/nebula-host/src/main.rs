//! NebulaDisplay host service entry point.
//!
//! Responsibilities:
//! * Load (or create) the host configuration and trust store.
//! * Generate a persistent self-signed TLS certificate on first run.
//! * Start the HTTPS + WebSocket streaming server.
//! * Start the UDP LAN discovery responder.
//! * Expose the local control panel and the browser viewer.

use std::net::SocketAddr;
use std::sync::Arc;

#[cfg(feature = "tls")]
use tracing::error;
use tracing::{info, warn};

use nebula_host::config::Config;
use nebula_host::discovery;
use nebula_host::server;
use nebula_host::state::AppState;
#[cfg(feature = "tls")]
use nebula_host::tls;

#[cfg(windows)]
mod service_win;

const USAGE: &str = "\
nebula-host — NebulaDisplay host service

USAGE:
  nebula-host [OPTIONS]

OPTIONS:
  --port <PORT>       TCP port for HTTPS/WebSocket (default 38470)
  --bind <ADDR>       Bind address (default 0.0.0.0)
  --name <NAME>       Host display name shown to viewers
  --no-tls            Serve plain HTTP/WS (loopback testing ONLY; discovery
                      advertises the connection as insecure)
  --source <SRC>      Frame source: auto | screen | test  (default auto)
  --web-dir <DIR>     Directory containing the built web UI (viewer/web/dist)
  --config <FILE>     Config file path (default: platform config dir)
  --print-access      Print the viewer URL and current pairing PIN, then keep running
  --service           Run under the Windows Service Control Manager
  -h, --help          Show this help
";

#[derive(Debug, Default)]
struct CliArgs {
    port: Option<u16>,
    bind: Option<String>,
    name: Option<String>,
    no_tls: bool,
    source: Option<String>,
    web_dir: Option<String>,
    config: Option<String>,
    print_access: bool,
    service: bool,
}

fn parse_args() -> Result<CliArgs, String> {
    let mut out = CliArgs::default();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut take = |name: &str| it.next().ok_or(format!("missing value for {name}"));
        match a.as_str() {
            "--port" => {
                out.port = Some(
                    take("--port")?
                        .parse()
                        .map_err(|e| format!("--port: {e}"))?,
                )
            }
            "--bind" => out.bind = Some(take("--bind")?),
            "--name" => out.name = Some(take("--name")?),
            "--no-tls" => out.no_tls = true,
            "--source" => out.source = Some(take("--source")?),
            "--web-dir" => out.web_dir = Some(take("--web-dir")?),
            "--config" => out.config = Some(take("--config")?),
            "--print-access" => out.print_access = true,
            "--service" => out.service = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(out)
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nebula_host=info,info".into()),
        )
        .init();

    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    #[cfg(windows)]
    if args.service {
        return service_win::run_as_service();
    }
    #[cfg(not(windows))]
    if args.service {
        anyhow::bail!("--service is only supported on Windows");
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run(args))
}

/// Entry point used by the Windows service wrapper: default configuration
/// (reads the persisted config file like console mode does).
#[cfg(windows)]
pub async fn run_with_defaults() -> anyhow::Result<()> {
    run(CliArgs::default()).await
}

async fn run(args: CliArgs) -> anyhow::Result<()> {
    let config_path = args
        .config
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(Config::default_path);
    let mut config = Config::load_or_default(&config_path)?;
    if let Some(p) = args.port {
        config.port = p;
    }
    if let Some(b) = &args.bind {
        config.bind = b.clone();
    }
    if let Some(n) = &args.name {
        config.host_name = n.clone();
    }
    if args.no_tls {
        config.tls = false;
    }
    if let Some(s) = &args.source {
        config.frame_source = s.clone();
    }
    if let Some(w) = &args.web_dir {
        config.web_dir = Some(w.clone());
    }
    config
        .save(&config_path)
        .unwrap_or_else(|e| warn!("could not persist config: {e}"));

    #[cfg(feature = "tls")]
    let tls_material = if config.tls {
        match tls::load_or_generate(&config_path) {
            Ok(m) => Some(m),
            Err(e) => {
                error!(
                    "TLS setup failed ({e}); falling back to plain HTTP. Pass --no-tls to silence."
                );
                None
            }
        }
    } else {
        None
    };
    #[cfg(not(feature = "tls"))]
    let tls_material: Option<NoTls> = {
        if config.tls {
            warn!("this build was compiled without the 'tls' feature; serving plain HTTP");
        }
        None
    };

    let state = Arc::new(AppState::new(
        config.clone(),
        tls_material.as_ref().map(|m| m.fingerprint.clone()),
    ));

    // LAN discovery responder (UDP). Untrusted: only advertises name/port/fingerprint.
    {
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = discovery::run_responder(state).await {
                warn!("discovery responder stopped: {e}");
            }
        });
    }

    let addr: SocketAddr = format!("{}:{}", config.bind, config.port).parse()?;
    let app = server::router(state.clone());

    let scheme = if tls_material.is_some() {
        "https"
    } else {
        "http"
    };
    info!(
        "NebulaDisplay host '{}' listening on {scheme}://{addr}",
        config.host_name
    );
    info!(
        "Control panel: {scheme}://localhost:{}/  (host machine only)",
        config.port
    );
    info!(
        "Viewer:        {scheme}://<this-machine-ip>:{}/view",
        config.port
    );
    if let Some(m) = &tls_material {
        info!("TLS certificate fingerprint (SHA-256): {}", m.fingerprint);
    }
    if args.print_access {
        let pin = state.pairing.lock().unwrap().issue_pin();
        println!("VIEWER_URL={scheme}://<host-ip>:{}/view", config.port);
        println!("PAIRING_PIN={pin}");
    }

    match tls_material {
        #[cfg(feature = "tls")]
        Some(m) => {
            let rustls_config = axum_server::tls_rustls::RustlsConfig::from_pem(
                m.cert_pem.into_bytes(),
                m.key_pem.into_bytes(),
            )
            .await?;
            axum_server::bind_rustls(addr, rustls_config)
                .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                .await?;
        }
        #[cfg(not(feature = "tls"))]
        Some(_) => unreachable!(),
        None => {
            axum_server::bind(addr)
                .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                .await?;
        }
    }
    Ok(())
}

/// Placeholder type for builds without the `tls` feature.
#[cfg(not(feature = "tls"))]
struct NoTls {
    fingerprint: String,
}

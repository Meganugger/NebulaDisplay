//! nebulad library surface — lets integration tests (and future embedders,
//! e.g. the tray app) run a full host in-process.

pub mod adapt;
pub mod audio;
pub mod capture;
pub mod clipboard;
pub mod config;
pub mod discovery;
pub mod encode;
pub mod input;
pub mod pairing;
pub mod panel;
pub mod pin;
pub mod server;
pub mod session;
pub mod state;
#[cfg(feature = "https")]
pub mod tls;
pub mod trust;
pub mod util;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use config::{Config, FileConfig};
use state::AppState;

/// Options for an embedded/test host instance.
pub struct EmbeddedOptions {
    pub data_dir: std::path::PathBuf,
    pub name: String,
    pub capture: (u32, u32),
    pub max_fps: u32,
    /// Accept the legacy (pre-PAKE) pairing method.
    pub allow_legacy_pair: bool,
    /// Stream a synthetic test tone as audio (channel 3) for e2e tests.
    pub audio: bool,
    /// Serve the viewer endpoint over HTTPS (self-signed).
    pub https: bool,
}

impl Default for EmbeddedOptions {
    fn default() -> Self {
        Self {
            data_dir: std::env::temp_dir().join("ndsp-embedded"),
            name: "embedded-host".into(),
            capture: (320, 240),
            max_fps: 30,
            allow_legacy_pair: true,
            audio: false,
            https: false,
        }
    }
}

/// A running in-process host (for tests / embedding).
pub struct EmbeddedHost {
    pub state: Arc<AppState>,
    pub port: u16,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl EmbeddedHost {
    /// Start capture + viewer endpoint on an ephemeral port with the
    /// synthetic source. Returns once the socket is bound and accepting.
    pub async fn start(opts: EmbeddedOptions) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&opts.data_dir)?;
        let cfg = Config {
            name: opts.name,
            data_dir: opts.data_dir,
            web_dir: None,
            file: FileConfig {
                max_fps: opts.max_fps,
                allow_legacy_pair: opts.allow_legacy_pair,
                audio: opts.audio,
                https: opts.https,
                ..Default::default()
            },
        };
        let state = Arc::new(AppState::new(cfg).await?);

        // Embedded hosts are for tests: never touch real audio devices.
        if audio::spawn_test_tone_if_enabled(state.clone()) {
            state
                .audio_available
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }

        let source = capture::create_source(true, opts.capture.0, opts.capture.1, 0);
        let cap_state = state.clone();
        let cap = tokio::spawn(async move {
            capture::run_capture_loop(cap_state, source).await;
        });

        // Bind explicitly so we know the ephemeral port before returning.
        let listener =
            tokio::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                .await?;
        let port = listener.local_addr()?.port();
        state.set_serving_port(port);
        let srv_state = state.clone();
        let srv = tokio::spawn(async move {
            if let Err(e) = server::serve_on(srv_state, listener).await {
                tracing::error!("embedded server failed: {e:#}");
            }
        });

        Ok(Self {
            state,
            port,
            tasks: vec![cap, srv],
        })
    }

    pub async fn shutdown(self) {
        self.state.trigger_shutdown();
        for t in self.tasks {
            t.abort();
            let _ = t.await; // waits for the capture blocking thread to exit
        }
    }
}

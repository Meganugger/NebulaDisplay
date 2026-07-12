//! Network + decode worker thread for the desktop viewer.

use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};
use winit::event_loop::EventLoopProxy;

use ndsp_client::{connect, Auth, Incoming, Session};
use ndsp_protocol::messages::{ClientInfo, Codec, ControlMsg, InputEvent, InputMode, Profile};

use crate::decode::Decoder;
use crate::{store, Shared, UiWake};

pub struct NetArgs {
    pub host: String,
    pub pin: Option<String>,
    pub name: String,
    pub profile: String,
}

/// UI thread → network thread.
pub enum Outgoing {
    Input(InputEvent),
    SetInputMode(InputMode),
}

fn parse_profile(s: &str) -> Profile {
    match s {
        "video" => Profile::Video,
        "drawing" => Profile::Drawing,
        "gaming" => Profile::Gaming,
        _ => Profile::Office,
    }
}

pub fn run(
    args: NetArgs,
    shared: Arc<Shared>,
    proxy: EventLoopProxy<UiWake>,
    input_rx: Receiver<Outgoing>,
) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let result = rt.block_on(session_loop(&args, &shared, &proxy, input_rx));
    let reason = match result {
        Ok(reason) => reason,
        Err(e) => format!("{e:#}"),
    };
    let _ = proxy.send_event(UiWake::Disconnected(reason));
}

fn set_status(shared: &Shared, proxy: &EventLoopProxy<UiWake>, status: impl Into<String>) {
    *shared.status.lock().unwrap() = status.into();
    let _ = proxy.send_event(UiWake::Status);
}

async fn session_loop(
    args: &NetArgs,
    shared: &Shared,
    proxy: &EventLoopProxy<UiWake>,
    input_rx: Receiver<Outgoing>,
) -> anyhow::Result<String> {
    let (host, port) = match args.host.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>().unwrap_or(ndsp_protocol::DEFAULT_PORT),
        ),
        None => (args.host.clone(), ndsp_protocol::DEFAULT_PORT),
    };
    let host_key = format!("{host}:{port}");

    let client = ClientInfo {
        device_id: store::device_id(),
        name: args.name.clone(),
        platform: std::env::consts::OS.to_string(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        features: Vec::new(),
    };

    set_status(shared, proxy, format!("connecting to {host_key}…"));
    let stored = store::load(&host_key);
    let mut session: Session = match (&stored, &args.pin) {
        (Some(creds), _) => {
            match connect(&host, port, client.clone(), Auth::Token(creds), codecs()).await {
                Ok(s) => s,
                Err(e) if format!("{e:#}").contains("pair") => {
                    // Token stale → try PIN if we have one, else tell the user.
                    store::clear(&host_key);
                    let Some(pin) = &args.pin else {
                        anyhow::bail!(
                            "stored trust was rejected by the host — run again with --pin <PIN>"
                        );
                    };
                    connect(&host, port, client.clone(), Auth::Pake(pin), codecs()).await?
                }
                Err(e) => return Err(e),
            }
        }
        // First contact pairs via SPAKE2 PAKE (offline-grind-proof); pass
        // through the legacy PIN-HKDF path only for pre-PAKE hosts via
        // `Auth::Pin` if ever needed.
        (None, Some(pin)) => {
            connect(&host, port, client.clone(), Auth::Pake(pin), codecs()).await?
        }
        (None, None) => anyhow::bail!("first connection needs --pin <PIN shown on the host>"),
    };

    if let Some(creds) = &session.new_credentials {
        store::save(&host_key, creds);
        info!("paired and trusted; future connections won't need a PIN");
    }
    shared
        .input_allowed
        .store(session.input_allowed, Ordering::Relaxed);
    set_status(shared, proxy, format!("{host_key} · {:?}", session.codec));

    session
        .send(&ControlMsg::SetProfile {
            profile: parse_profile(&args.profile),
        })
        .await?;

    let mut decoder = Decoder::new();
    let mut last_ping = std::time::Instant::now() - Duration::from_secs(10);
    let mut input_mode = InputMode::ViewOnly;

    loop {
        // Drain UI → host traffic.
        while let Ok(out) = input_rx.try_recv() {
            match out {
                Outgoing::Input(ev) => {
                    if input_mode != InputMode::ViewOnly {
                        session
                            .send(&ControlMsg::Input { events: vec![ev] })
                            .await?;
                    }
                }
                Outgoing::SetInputMode(mode) => {
                    input_mode = mode;
                    session.send(&ControlMsg::SetInputMode { mode }).await?;
                }
            }
        }

        if last_ping.elapsed() > Duration::from_secs(1) {
            last_ping = std::time::Instant::now();
            let t0 = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros() as u64;
            session.send(&ControlMsg::Ping { t0_us: t0 }).await?;
        }

        let incoming = tokio::time::timeout(Duration::from_millis(50), session.recv()).await;
        match incoming {
            Err(_) => continue, // poll input again
            Ok(Err(e)) => return Ok(format!("connection error: {e:#}")),
            Ok(Ok(Incoming::Closed)) => return Ok("host closed the connection".into()),
            Ok(Ok(Incoming::Video(frame))) => match decoder.decode(&frame) {
                Ok(Some(rgba)) => {
                    *shared.latest.lock().unwrap() = Some(rgba);
                    let _ = proxy.send_event(UiWake::Frame);
                }
                Ok(None) => {}
                Err(e) => {
                    warn!("decode error: {e:#}");
                    session.send(&ControlMsg::RequestKeyframe).await?;
                }
            },
            Ok(Ok(Incoming::Control(msg))) => match msg {
                ControlMsg::InputGrant { allowed } => {
                    shared.input_allowed.store(allowed, Ordering::Relaxed);
                    set_status(
                        shared,
                        proxy,
                        if allowed {
                            "input granted by host"
                        } else {
                            "input revoked by host"
                        },
                    );
                }
                ControlMsg::Bye { reason } => return Ok(format!("host: {reason}")),
                _ => {}
            },
        }
    }
}

fn codecs() -> Vec<Codec> {
    #[cfg(feature = "h264")]
    return vec![Codec::H264, Codec::Jpeg];
    #[cfg(not(feature = "h264"))]
    vec![Codec::Jpeg]
}

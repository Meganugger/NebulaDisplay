//! HTTPS + WebSocket server: static web UI, admin API, and the NDSP
//! connection state machine.
//!
//! Security boundaries:
//! * `/api/admin/*` (pairing PINs, device management, config) is restricted
//!   to loopback connections — only someone at the host machine (or an
//!   authenticated remote-desktop session to it) can manage trust.
//! * `/ws` accepts LAN connections but streams nothing until the NDSP
//!   handshake authenticates the device (PIN pairing or stored token).
//! * Input events are dropped unless the host user granted that device
//!   input permission.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::connect_info::ConnectInfo;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{info, warn};

use nebula_proto::{caps, ControlMessage, DisplayMode, ErrorCode, VideoModeInfo, PROTOCOL_VERSION};

use crate::adaptive::AdaptiveController;
use crate::capture;
use crate::input::{create_injector, Injector};
use crate::pairing::PinCheck;
use crate::pipeline;
use crate::state::AppState;

/// Shown when the web UI bundle is missing (not built / wrong --web-dir).
const FALLBACK_PAGE: &str = include_str!("fallback.html");

pub fn router(state: Arc<AppState>) -> Router {
    let web_dir = resolve_web_dir(&state);
    let serve_ui: Router<Arc<AppState>> = match &web_dir {
        Some(dir) => {
            info!("serving web UI from {}", dir.display());
            Router::new().fallback_service(
                tower_http::services::ServeDir::new(dir)
                    .append_index_html_on_directories(true)
                    .fallback(tower_http::services::ServeFile::new(dir.join("index.html"))),
            )
        }
        None => {
            warn!("web UI bundle not found — serving fallback page (build viewer/web first)");
            Router::new().fallback(|| async { Html(FALLBACK_PAGE) })
        }
    };

    Router::new()
        .route("/ws", get(ws_upgrade))
        .route(
            "/view",
            get(|| async { axum::response::Redirect::permanent("/view/") }),
        )
        .route("/api/info", get(api_info))
        .route("/api/admin/status", get(admin_status))
        .route("/api/admin/pin", post(admin_new_pin))
        .route("/api/admin/devices", get(admin_devices))
        .route("/api/admin/devices/:id/input", post(admin_set_input))
        .route("/api/admin/devices/:id", delete(admin_revoke))
        .route("/api/admin/config", post(admin_update_config))
        .merge(serve_ui)
        .with_state(state)
}

fn resolve_web_dir(state: &AppState) -> Option<std::path::PathBuf> {
    let cfg = state.config();
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(dir) = cfg.web_dir {
        candidates.push(dir.into());
    }
    candidates.push("viewer/web/dist".into());
    candidates.push("web".into());
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("web"));
        }
    }
    candidates
        .into_iter()
        .find(|p| p.join("index.html").exists())
}

// ---------------------------------------------------------------------------
// Public + admin HTTP API
// ---------------------------------------------------------------------------

async fn api_info(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let cfg = state.config();
    Json(json!({
        "service": "nebuladisplay",
        "protocol_version": PROTOCOL_VERSION,
        "host_name": cfg.host_name,
        "tls_fingerprint": state.tls_fingerprint,
    }))
}

fn require_loopback(addr: &SocketAddr) -> Result<(), (StatusCode, &'static str)> {
    if addr.ip().is_loopback() {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            "admin API is restricted to the host machine",
        ))
    }
}

async fn admin_status(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if let Err(e) = require_loopback(&addr) {
        return e.into_response();
    }
    let cfg = state.config();
    let clients: Vec<_> = state.clients.lock().unwrap().values().cloned().collect();
    let lan_ips = lan_ips();
    let pin = state.pairing.lock().unwrap().current_pin();
    let driver_status = driver_status();
    let audio = crate::audio::availability().err();
    Json(json!({
        "host_name": cfg.host_name,
        "port": cfg.port,
        "tls": cfg.tls,
        "tls_fingerprint": state.tls_fingerprint,
        "uptime_secs": state.started_at.elapsed().as_secs(),
        "frame_source": cfg.frame_source,
        "driver": driver_status,
        "audio_unavailable_reason": audio,
        "audio_enabled": cfg.audio_enabled,
        "clipboard_enabled": cfg.clipboard_enabled,
        "discovery": cfg.discovery,
        "max_clients": cfg.max_clients,
        "clients": clients,
        "lan_ips": lan_ips,
        "active_pin": pin.map(|(p, ttl)| json!({"pin": p, "expires_in_secs": ttl.as_secs()})),
    }))
    .into_response()
}

/// Best-effort LAN IP discovery without extra dependencies: a UDP socket
/// "connected" to a public address reveals the outbound interface address.
/// No packets are sent (UDP connect only sets the destination).
fn lan_ips() -> Vec<String> {
    let mut ips = Vec::new();
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("192.0.2.1:80").is_ok() {
            if let Ok(addr) = sock.local_addr() {
                if !addr.ip().is_loopback() && !addr.ip().is_unspecified() {
                    ips.push(addr.ip().to_string());
                }
            }
        }
    }
    ips
}

/// Driver / capture backend availability for the diagnostics panel.
fn driver_status() -> serde_json::Value {
    #[cfg(windows)]
    {
        // The IddCx driver advertises itself through its shared-memory
        // section; probing it is cheap and does not disturb streaming.
        let virtual_ok = crate::capture::idd::VirtualMonitorSource::connect().is_ok();
        json!({
            "virtual_display_driver": if virtual_ok { "running" } else { "not_detected" },
            "capture_fallback": "dxgi_desktop_duplication",
            "mode": if virtual_ok { "extend+mirror" } else { "mirror_only" },
        })
    }
    #[cfg(not(windows))]
    {
        json!({
            "virtual_display_driver": "windows_only",
            "capture_fallback": "test_pattern",
            "mode": "demo",
        })
    }
}

async fn admin_new_pin(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if let Err(e) = require_loopback(&addr) {
        return e.into_response();
    }
    let pin = state.pairing.lock().unwrap().issue_pin();
    let cfg = state.config();
    // The QR payload viewers scan: where to connect + how to verify the host.
    let qr = json!({
        "v": 1,
        "kind": "nebuladisplay-pair",
        "port": cfg.port,
        "tls": cfg.tls,
        "fp": state.tls_fingerprint,
        "pin": pin,
    });
    Json(json!({
        "pin": pin,
        "expires_in_secs": crate::pairing::PIN_TTL.as_secs(),
        "qr_payload": qr.to_string(),
    }))
    .into_response()
}

async fn admin_devices(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if let Err(e) = require_loopback(&addr) {
        return e.into_response();
    }
    let list: Vec<_> = state
        .trust
        .lock()
        .unwrap()
        .list()
        .into_iter()
        .map(|(id, e)| {
            json!({
                "device_id": id,
                "name": e.name,
                "input_allowed": e.input_allowed,
                "paired_at_unix": e.paired_at_unix,
                "last_seen_unix": e.last_seen_unix,
            })
        })
        .collect();
    Json(json!({ "devices": list })).into_response()
}

#[derive(Deserialize)]
struct InputAllow {
    allowed: bool,
}

async fn admin_set_input(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<InputAllow>,
) -> impl IntoResponse {
    if let Err(e) = require_loopback(&addr) {
        return e.into_response();
    }
    let ok = state
        .trust
        .lock()
        .unwrap()
        .set_input_allowed(&id, body.allowed);
    if ok {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::NOT_FOUND, "unknown device").into_response()
    }
}

async fn admin_revoke(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = require_loopback(&addr) {
        return e.into_response();
    }
    let ok = state.trust.lock().unwrap().revoke(&id);
    if ok {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::NOT_FOUND, "unknown device").into_response()
    }
}

#[derive(Deserialize)]
struct ConfigPatch {
    audio_enabled: Option<bool>,
    clipboard_enabled: Option<bool>,
    discovery: Option<bool>,
    host_name: Option<String>,
    default_profile: Option<String>,
}

async fn admin_update_config(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    Json(patch): Json<ConfigPatch>,
) -> impl IntoResponse {
    if let Err(e) = require_loopback(&addr) {
        return e.into_response();
    }
    state.update_config(|c| {
        if let Some(v) = patch.audio_enabled {
            c.audio_enabled = v;
        }
        if let Some(v) = patch.clipboard_enabled {
            c.clipboard_enabled = v;
        }
        if let Some(v) = patch.discovery {
            c.discovery = v;
        }
        if let Some(v) = patch.host_name.clone() {
            c.host_name = v;
        }
        if let Some(v) = patch.default_profile.clone() {
            c.default_profile = v;
        }
    });
    let cfg = state.config();
    cfg.save(&crate::config::Config::default_path()).ok();
    StatusCode::NO_CONTENT.into_response()
}

// ---------------------------------------------------------------------------
// WebSocket: the NDSP state machine
// ---------------------------------------------------------------------------

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_connection(socket, addr, state))
}

struct ConnCtx {
    conn_id: u64,
    addr: SocketAddr,
    device_id: Option<String>,
    authorized: bool,
    input_allowed: bool,
    injector: Option<Box<dyn Injector>>,
    pipeline: Option<pipeline::PipelineHandle>,
    adaptive: Option<Arc<Mutex<AdaptiveController>>>,
    last_ping_sent: Option<(u64, Instant)>,
}

async fn handle_connection(socket: WebSocket, addr: SocketAddr, state: Arc<AppState>) {
    let conn_id = state.new_conn_id();
    info!("[conn {conn_id}] connection from {addr}");
    state.upsert_client(conn_id, |c| {
        c.remote_addr = addr.to_string();
    });

    let (mut sink, mut stream) = socket.split();
    // Outgoing multiplexer: protocol tasks push Messages here; one task owns
    // the sink. Bounded to apply backpressure to media, generous enough for
    // control traffic.
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(16);
    let sender_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let mut ctx = ConnCtx {
        conn_id,
        addr,
        device_id: None,
        authorized: false,
        input_allowed: false,
        injector: None,
        pipeline: None,
        adaptive: None,
        last_ping_sent: None,
    };

    let mut ticker = tokio::time::interval(Duration::from_secs(2));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;

            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        match ControlMessage::from_json(&text) {
                            Ok(msg) => {
                                if handle_control(&mut ctx, msg, &state, &out_tx).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                warn!("[conn {conn_id}] bad control message: {e}");
                                send_error(&out_tx, ErrorCode::Internal, "malformed control message").await;
                            }
                        }
                    }
                    Some(Ok(Message::Binary(_))) => {
                        // Clients do not send binary media to the host in v1.
                    }
                    Some(Ok(Message::Ping(p))) => { let _ = out_tx.send(Message::Pong(p)).await; }
                    Some(Ok(Message::Pong(_))) | Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                        if matches!(incoming, Some(Ok(Message::Pong(_)))) { continue; }
                        break;
                    }
                }
            }

            // Media packets from the pipeline (when streaming).
            pkt = async {
                match ctx.pipeline.as_mut() {
                    Some(p) => p.packets.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match pkt {
                    Some(bytes) => {
                        if out_tx.send(Message::Binary(bytes)).await.is_err() {
                            break;
                        }
                    }
                    None => {
                        // Pipeline ended (source failure); tell the client.
                        let _ = out_tx.send(Message::Text(
                            ControlMessage::SessionStop { reason: "frame source ended".into() }.to_json()
                        )).await;
                        ctx.pipeline = None;
                    }
                }
            }

            _ = ticker.tick() => {
                periodic(&mut ctx, &state, &out_tx).await;
            }
        }
    }

    // Teardown.
    if let Some(p) = &ctx.pipeline {
        p.stop.store(true, Ordering::Relaxed);
    }
    state.remove_client(conn_id);
    sender_task.abort();
    info!("[conn {conn_id}] disconnected");
}

async fn send_msg(out: &mpsc::Sender<Message>, msg: ControlMessage) -> Result<(), ()> {
    out.send(Message::Text(msg.to_json())).await.map_err(|_| ())
}

async fn send_error(out: &mpsc::Sender<Message>, code: ErrorCode, message: &str) {
    let _ = send_msg(
        out,
        ControlMessage::Error {
            code,
            message: message.into(),
        },
    )
    .await;
}

/// Periodic per-connection work: RTT ping and stats publication.
async fn periodic(ctx: &mut ConnCtx, state: &Arc<AppState>, out: &mpsc::Sender<Message>) {
    if !ctx.authorized {
        return;
    }
    // Host-initiated RTT probe.
    let t = Instant::now();
    let t_micros = state.started_at.elapsed().as_micros() as u64;
    ctx.last_ping_sent = Some((t_micros, t));
    let _ = send_msg(out, ControlMessage::Ping { t_micros }).await;

    // Publish stream stats to the client and the diagnostics panel.
    if let Some(p) = &ctx.pipeline {
        let stats = *p.stats.lock().unwrap();
        let _ = send_msg(out, ControlMessage::Stats(stats)).await;
        state.upsert_client(ctx.conn_id, |c| {
            c.streaming = true;
            c.fps = stats.fps;
            c.bitrate_kbps = stats.bitrate_kbps;
            c.rtt_ms = stats.rtt_ms;
            c.quality = stats.quality;
            c.frames_sent = stats.frames_sent;
            c.frames_dropped = stats.frames_dropped;
            c.width = stats.width;
            c.height = stats.height;
        });
    }
}

async fn handle_control(
    ctx: &mut ConnCtx,
    msg: ControlMessage,
    state: &Arc<AppState>,
    out: &mpsc::Sender<Message>,
) -> Result<(), ()> {
    match msg {
        ControlMessage::Hello {
            min_version,
            max_version,
            client_name,
            device_id,
            capabilities: _,
        } => {
            let Some(version) =
                nebula_proto::negotiate_version(min_version, max_version, 1, PROTOCOL_VERSION)
            else {
                send_error(
                    out,
                    ErrorCode::ProtocolMismatch,
                    "no shared protocol version",
                )
                .await;
                return Err(());
            };
            let cfg = state.config();
            let known = state.trust.lock().unwrap().contains(&device_id);
            ctx.device_id = Some(device_id.clone());
            state.upsert_client(ctx.conn_id, |c| {
                c.device_id = device_id.clone();
                c.name = client_name.clone();
            });
            let mut host_caps = vec![caps::VIDEO_MJPEG.to_string(), caps::INPUT.to_string()];
            #[cfg(windows)]
            host_caps.push(caps::CAPTURE_MIRROR.to_string());
            if cfg.audio_enabled && crate::audio::availability().is_ok() {
                host_caps.push(caps::AUDIO_PCM.to_string());
            }
            send_msg(
                out,
                ControlMessage::HelloAck {
                    version,
                    host_name: cfg.host_name,
                    capabilities: host_caps,
                    known_device: known,
                },
            )
            .await
        }

        ControlMessage::PairRequest { pin, device_name } => {
            let Some(device_id) = ctx.device_id.clone() else {
                send_error(out, ErrorCode::Internal, "hello first").await;
                return Err(());
            };
            let result = state.pairing.lock().unwrap().check_pin(&pin);
            match result {
                PinCheck::Ok => {
                    let token = state
                        .trust
                        .lock()
                        .unwrap()
                        .register(&device_id, &device_name);
                    ctx.authorized = true;
                    ctx.input_allowed = false;
                    info!("[conn {}] paired new device '{}'", ctx.conn_id, device_name);
                    state.upsert_client(ctx.conn_id, |c| {
                        c.authorized = true;
                        c.name = device_name.clone();
                    });
                    send_msg(out, ControlMessage::PairOk { token }).await
                }
                PinCheck::Wrong => {
                    warn!("[conn {}] wrong pairing PIN from {}", ctx.conn_id, ctx.addr);
                    send_error(out, ErrorCode::BadPin, "wrong PIN").await;
                    Ok(())
                }
                PinCheck::Expired | PinCheck::NoneActive => {
                    send_error(
                        out,
                        ErrorCode::PinExpired,
                        "no active pairing PIN — generate one on the host control panel",
                    )
                    .await;
                    Ok(())
                }
            }
        }

        ControlMessage::Auth { token } => {
            let Some(device_id) = ctx.device_id.clone() else {
                send_error(out, ErrorCode::Internal, "hello first").await;
                return Err(());
            };
            let ok = state.trust.lock().unwrap().verify(&device_id, &token);
            if ok {
                ctx.authorized = true;
                ctx.input_allowed = state.trust.lock().unwrap().input_allowed(&device_id);
                state.upsert_client(ctx.conn_id, |c| {
                    c.authorized = true;
                    c.input_allowed = ctx.input_allowed;
                });
                send_msg(
                    out,
                    ControlMessage::AuthOk {
                        input_allowed: ctx.input_allowed,
                    },
                )
                .await
            } else {
                warn!("[conn {}] bad token for device {}", ctx.conn_id, device_id);
                send_error(
                    out,
                    ErrorCode::BadToken,
                    "unknown device or bad token — pair again",
                )
                .await;
                Ok(())
            }
        }

        ControlMessage::SessionStart {
            mode,
            profile,
            preferred,
            viewport_width,
            viewport_height,
            codecs,
            want_audio,
        } => {
            if !ctx.authorized {
                send_error(out, ErrorCode::NotAuthorized, "authenticate first").await;
                return Ok(());
            }
            if ctx.pipeline.is_some() {
                send_error(
                    out,
                    ErrorCode::Busy,
                    "session already running on this connection",
                )
                .await;
                return Ok(());
            }
            let cfg = state.config();
            let active = state
                .clients
                .lock()
                .unwrap()
                .values()
                .filter(|c| c.streaming)
                .count();
            if active as u32 >= cfg.max_clients {
                send_error(out, ErrorCode::Busy, "maximum client count reached").await;
                return Ok(());
            }
            if !codecs.iter().any(|c| c == caps::VIDEO_MJPEG) {
                send_error(
                    out,
                    ErrorCode::UnsupportedCodec,
                    "host currently offers video/mjpeg",
                )
                .await;
                return Ok(());
            }

            // Pick stream dimensions: preferred mode > viewport > 1080p.
            let (w, h) = preferred
                .map(|m| (m.width, m.height))
                .or(if viewport_width > 0 && viewport_height > 0 {
                    Some((viewport_width.min(3840), viewport_height.min(2160)))
                } else {
                    None
                })
                .unwrap_or((1920, 1080));

            let source_kind = match mode {
                DisplayMode::Extend => "virtual",
                DisplayMode::Mirror => cfg.frame_source.as_str(),
            };
            let source = match capture::create_source(source_kind, w, h) {
                Ok(s) => s,
                Err(e) => {
                    // Extend mode without the driver → graceful fallback story.
                    warn!("[conn {}] source '{source_kind}' failed: {e}", ctx.conn_id);
                    match capture::create_source(&cfg.frame_source, w, h) {
                        Ok(s) => {
                            let _ = send_msg(
                                out,
                                ControlMessage::Error {
                                    code: ErrorCode::Internal,
                                    message: format!(
                                    "virtual monitor unavailable ({e}); falling back to mirror mode"
                                ),
                                },
                            )
                            .await;
                            s
                        }
                        Err(e2) => {
                            send_error(out, ErrorCode::Internal, &format!("no frame source: {e2}"))
                                .await;
                            return Ok(());
                        }
                    }
                }
            };
            let (sw, sh) = source.size();
            let effective_profile = profile;
            let adaptive = Arc::new(Mutex::new(AdaptiveController::new(effective_profile)));
            let handle = pipeline::spawn(source, adaptive.clone());
            ctx.adaptive = Some(adaptive);
            ctx.pipeline = Some(handle);

            let audio_on = want_audio && cfg.audio_enabled && crate::audio::availability().is_ok();
            state.upsert_client(ctx.conn_id, |c| {
                c.streaming = true;
                c.codec = "mjpeg".into();
            });
            info!(
                "[conn {}] session started: {}x{} {:?} profile={:?} audio={}",
                ctx.conn_id, sw, sh, mode, effective_profile, audio_on
            );
            send_msg(
                out,
                ControlMessage::SessionStarted {
                    codec: caps::VIDEO_MJPEG.into(),
                    mode: VideoModeInfo {
                        width: sw,
                        height: sh,
                        refresh_hz: cfg.max_fps,
                    },
                    display_mode: mode,
                    audio: audio_on,
                    monitor_index: 0,
                },
            )
            .await?;
            if audio_on {
                spawn_audio(out.clone());
            }
            Ok(())
        }

        ControlMessage::SessionStop { reason } => {
            info!("[conn {}] session stopped by client: {reason}", ctx.conn_id);
            if let Some(p) = ctx.pipeline.take() {
                p.stop.store(true, Ordering::Relaxed);
            }
            state.upsert_client(ctx.conn_id, |c| c.streaming = false);
            Ok(())
        }

        ControlMessage::ModeChange {
            preferred: _,
            profile,
        } => {
            if let (Some(a), Some(p)) = (&ctx.adaptive, profile) {
                a.lock().unwrap().set_profile(p);
            }
            Ok(())
        }

        ControlMessage::Input { events } => {
            if !ctx.authorized {
                return Ok(());
            }
            // Re-check the trust store each batch so a revocation on the
            // control panel takes effect immediately.
            let allowed = ctx
                .device_id
                .as_ref()
                .map(|id| state.trust.lock().unwrap().input_allowed(id))
                .unwrap_or(false);
            if allowed != ctx.input_allowed {
                ctx.input_allowed = allowed;
                let _ = send_msg(out, ControlMessage::InputPermission { allowed }).await;
            }
            if !allowed {
                return Ok(());
            }
            let injector = ctx.injector.get_or_insert_with(create_injector);
            for ev in &events {
                if let Err(e) = injector.inject(ev) {
                    warn!("[conn {}] input injection failed: {e}", ctx.conn_id);
                    break;
                }
            }
            Ok(())
        }

        ControlMessage::Ping { t_micros } => send_msg(out, ControlMessage::Pong { t_micros }).await,

        ControlMessage::Pong { t_micros } => {
            if let Some((sent_us, sent_at)) = ctx.last_ping_sent {
                if sent_us == t_micros {
                    let rtt = sent_at.elapsed().as_secs_f32() * 1e3;
                    if let Some(a) = &ctx.adaptive {
                        a.lock().unwrap().on_rtt(rtt);
                    }
                    if let Some(p) = &ctx.pipeline {
                        p.stats.lock().unwrap().rtt_ms = rtt;
                    }
                }
            }
            Ok(())
        }

        ControlMessage::Feedback(fb) => {
            if let Some(a) = &ctx.adaptive {
                a.lock().unwrap().on_feedback(&fb);
            }
            Ok(())
        }

        ControlMessage::ClipboardOffer { .. }
        | ControlMessage::ClipboardAccept {}
        | ControlMessage::ClipboardData { .. } => {
            let enabled = state.config().clipboard_enabled;
            if !enabled {
                send_error(
                    out,
                    ErrorCode::NotAuthorized,
                    "clipboard sync is disabled on the host (enable it on the control panel)",
                )
                .await;
            }
            // Clipboard bridging into the OS clipboard ships with the tray
            // integration; the permission gate and protocol are final.
            Ok(())
        }

        ControlMessage::Bye { .. } => Err(()),

        ControlMessage::Resume { .. } => {
            // Stateless resume: force a full frame; the client keeps its token.
            if let Some(p) = &ctx.pipeline {
                p.refresh.store(true, Ordering::Relaxed);
            }
            send_msg(out, ControlMessage::ResumeOk { from_frame: 0 }).await
        }

        // Host → client messages arriving at the host are protocol misuse; ignore.
        ControlMessage::HelloAck { .. }
        | ControlMessage::PairOk { .. }
        | ControlMessage::AuthOk { .. }
        | ControlMessage::SessionStarted { .. }
        | ControlMessage::InputPermission { .. }
        | ControlMessage::Stats(_)
        | ControlMessage::Error { .. }
        | ControlMessage::ResumeOk { .. } => Ok(()),
    }
}

/// Start streaming system audio to this client (Windows only).
fn spawn_audio(out: mpsc::Sender<Message>) {
    #[cfg(windows)]
    {
        std::thread::Builder::new()
            .name("nebula-audio".into())
            .spawn(move || {
                let mut capture = match crate::audio::WasapiLoopback::new() {
                    Ok(c) => c,
                    Err(e) => {
                        warn!("audio capture failed to start: {e}");
                        return;
                    }
                };
                let mut seq: u32 = 0;
                loop {
                    match capture.poll() {
                        Ok(Some(chunk)) => {
                            seq = seq.wrapping_add(1);
                            let pkt = chunk.to_packet(seq);
                            // Audio rides the same bounded channel; if the
                            // link is saturated we drop audio first.
                            if out.try_send(Message::Binary(pkt)).is_err() && out.is_closed() {
                                break;
                            }
                        }
                        Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                        Err(e) => {
                            warn!("audio capture error: {e}");
                            break;
                        }
                    }
                }
            })
            .ok();
    }
    #[cfg(not(windows))]
    {
        let _ = out;
    }
}

//! Loopback-only control panel API + static UI.
//!
//! Bound strictly to 127.0.0.1 — LAN peers can never reach pairing PINs,
//! grants, or trust management. The static panel page comes from the same
//! web dist as the viewer (`panel.html`).

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use qrcode::render::svg;
use qrcode::QrCode;
use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tower_http::services::ServeDir;
use tracing::info;

use crate::state::AppState;
use crate::util::local_ips;

pub async fn run(state: Arc<AppState>, port: u16) -> anyhow::Result<()> {
    let mut app = Router::new()
        .route("/api/status", get(status))
        .route("/api/pin/rotate", post(rotate_pin))
        .route("/api/grant", post(grant))
        .route("/api/revoke", post(revoke))
        .route("/api/audio", post(audio_toggle))
        .route("/api/audio-mute", post(audio_mute))
        .route("/api/qr.svg", get(qr_svg));

    if let Some(dir) = &state.cfg.web_dir {
        app = app.fallback_service(ServeDir::new(dir));
    }

    let app = app.with_state(state);
    let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("control panel on http://{addr}/panel.html (loopback only)");
    axum::serve(listener, app).await?;
    Ok(())
}

#[derive(Serialize)]
struct TrustedDeviceView {
    device_id: String,
    name: String,
    platform: String,
    created_unix: u64,
    last_seen_unix: u64,
    input_allowed: bool,
    clipboard_allowed: bool,
    online: bool,
}

#[derive(Serialize)]
struct ClientView {
    id: u64,
    device_id: String,
    name: String,
    platform: String,
    addr: String,
    connected_unix: u64,
    input_allowed: bool,
    clipboard_allowed: bool,
    /// Viewer opted into audio this session (privacy indicator).
    audio_active: bool,
    /// Panel-side mute for this client.
    audio_muted: bool,
    stats: ndsp_protocol::messages::ViewerStats,
}

#[derive(Serialize)]
struct StatusView {
    name: String,
    version: String,
    fingerprint: String,
    port: u16,
    pin: String,
    viewer_urls: Vec<String>,
    mode: ndsp_protocol::messages::DisplayMode,
    host_stats: ndsp_protocol::messages::HostStats,
    /// Host audio switch state + whether the pipeline can run at all.
    audio_enabled: bool,
    audio_available: bool,
    clients: Vec<ClientView>,
    trusted: Vec<TrustedDeviceView>,
}

async fn status(State(state): State<Arc<AppState>>) -> Json<StatusView> {
    let port = state.serving_port();
    let clients: Vec<ClientView> = state
        .clients
        .lock()
        .unwrap()
        .iter()
        .map(|(id, c)| ClientView {
            id: *id,
            device_id: c.device_id.clone(),
            name: c.name.clone(),
            platform: c.platform.clone(),
            addr: c.addr.to_string(),
            connected_unix: c.connected_unix,
            input_allowed: c.input_allowed.load(std::sync::atomic::Ordering::Relaxed),
            clipboard_allowed: c
                .clipboard_allowed
                .load(std::sync::atomic::Ordering::Relaxed),
            audio_active: c.audio_active.load(std::sync::atomic::Ordering::Relaxed),
            audio_muted: c.audio_muted.load(std::sync::atomic::Ordering::Relaxed),
            stats: c.stats.lock().unwrap().clone(),
        })
        .collect();
    let online: std::collections::HashSet<String> =
        clients.iter().map(|c| c.device_id.clone()).collect();
    // NOTE: trust tokens are intentionally never serialized here.
    let trusted = state
        .trust
        .lock()
        .unwrap()
        .list()
        .iter()
        .map(|d| TrustedDeviceView {
            device_id: d.device_id.clone(),
            name: d.name.clone(),
            platform: d.platform.clone(),
            created_unix: d.created_unix,
            last_seen_unix: d.last_seen_unix,
            input_allowed: d.input_allowed,
            clipboard_allowed: d.clipboard_allowed,
            online: online.contains(&d.device_id),
        })
        .collect();
    Json(StatusView {
        name: state.cfg.name.clone(),
        version: env!("CARGO_PKG_VERSION").into(),
        fingerprint: state.fingerprint.clone(),
        port,
        pin: state.pins.current_pin(),
        viewer_urls: local_ips()
            .iter()
            .map(|ip| format!("http://{ip}:{port}/"))
            .collect(),
        mode: *state.mode.lock().unwrap(),
        host_stats: state.host_stats.lock().unwrap().clone(),
        audio_enabled: state.audio_enabled(),
        audio_available: state.audio_available(),
        clients,
        trusted,
    })
}

async fn rotate_pin(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let pin = state.pins.rotate();
    Json(serde_json::json!({ "pin": pin }))
}

#[derive(Deserialize)]
struct GrantReq {
    device_id: String,
    allowed: bool,
    /// Which capability the grant targets. Default "input" (v0.4 clients).
    #[serde(default = "default_capability")]
    capability: String,
}

fn default_capability() -> String {
    "input".into()
}

async fn grant(State(state): State<Arc<AppState>>, Json(req): Json<GrantReq>) -> impl IntoResponse {
    let result = match req.capability.as_str() {
        "input" => state.set_input_grant(&req.device_id, req.allowed),
        "clipboard" => state.set_clipboard_grant(&req.device_id, req.allowed),
        other => {
            return (
                StatusCode::BAD_REQUEST,
                format!("unknown capability {other}"),
            )
                .into_response()
        }
    };
    match result {
        Ok(true) => (StatusCode::OK, "ok").into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "unknown device").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

#[derive(Deserialize)]
struct AudioToggleReq {
    enabled: bool,
}

/// Host-side global audio switch (config default; live-flippable here).
async fn audio_toggle(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AudioToggleReq>,
) -> impl IntoResponse {
    state.set_audio_enabled(req.enabled);
    info!(enabled = req.enabled, "panel toggled audio");
    (StatusCode::OK, "ok")
}

#[derive(Deserialize)]
struct AudioMuteReq {
    device_id: String,
    muted: bool,
}

async fn audio_mute(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AudioMuteReq>,
) -> impl IntoResponse {
    if state.set_audio_mute(&req.device_id, req.muted) {
        (StatusCode::OK, "ok").into_response()
    } else {
        (StatusCode::NOT_FOUND, "device not connected").into_response()
    }
}

#[derive(Deserialize)]
struct RevokeReq {
    device_id: String,
}

async fn revoke(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RevokeReq>,
) -> impl IntoResponse {
    match state.revoke_device(&req.device_id) {
        Ok(true) => (StatusCode::OK, "ok").into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "unknown device").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

/// QR payload: the viewer URL with PIN + fingerprint baked in, so scanning it
/// on a phone opens the viewer pre-filled and pairs in one tap.
async fn qr_svg(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let port = state.serving_port();
    let ip = local_ips()
        .first()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "HOST-IP".into());
    let pin = state.pins.current_pin();
    let url = format!(
        "http://{ip}:{port}/?pin={pin}&fp={}",
        &state.fingerprint[..16]
    );
    match QrCode::new(url.as_bytes()) {
        Ok(code) => {
            let svg = code
                .render::<svg::Color>()
                .min_dimensions(240, 240)
                .dark_color(svg::Color("#e6edf3"))
                .light_color(svg::Color("#00000000"))
                .build();
            ([(header::CONTENT_TYPE, "image/svg+xml")], svg).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("qr: {e}")).into_response(),
    }
}

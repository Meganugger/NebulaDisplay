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
    /// True when the viewer endpoint serves HTTPS/WSS.
    tls: bool,
    /// SHA-256 fingerprint of the TLS certificate (pin this in native
    /// viewers); `None` when TLS is off.
    tls_fingerprint: Option<String>,
    mode: ndsp_protocol::messages::DisplayMode,
    host_stats: ndsp_protocol::messages::HostStats,
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
    let tls = state.cfg.file.tls;
    let tls_fingerprint = if tls {
        crate::tls::load_or_create(&state.cfg.data_dir)
            .ok()
            .map(|i| i.fingerprint_hex)
    } else {
        None
    };
    let scheme = if tls { "https" } else { "http" };
    Json(StatusView {
        name: state.cfg.name.clone(),
        version: env!("CARGO_PKG_VERSION").into(),
        fingerprint: state.fingerprint.clone(),
        port,
        pin: state.pins.current_pin(),
        viewer_urls: local_ips()
            .iter()
            .map(|ip| format!("{scheme}://{ip}:{port}/"))
            .collect(),
        tls,
        tls_fingerprint,
        mode: *state.mode.lock().unwrap(),
        host_stats: state.host_stats.lock().unwrap().clone(),
        clients,
        trusted,
    })
}

async fn rotate_pin(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let pin = state.pins.rotate();
    Json(serde_json::json!({ "pin": pin }))
}

#[derive(Deserialize, Default, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
enum GrantCapability {
    #[default]
    Input,
    Clipboard,
}

#[derive(Deserialize)]
struct GrantReq {
    device_id: String,
    allowed: bool,
    /// Which capability to toggle; defaults to `input` (pre-clipboard panel
    /// builds keep working).
    #[serde(default)]
    capability: GrantCapability,
}

async fn grant(State(state): State<Arc<AppState>>, Json(req): Json<GrantReq>) -> impl IntoResponse {
    let result = match req.capability {
        GrantCapability::Input => state.set_input_grant(&req.device_id, req.allowed),
        GrantCapability::Clipboard => state.set_clipboard_grant(&req.device_id, req.allowed),
    };
    match result {
        Ok(true) => (StatusCode::OK, "ok").into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "unknown device").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
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
    let scheme = if state.cfg.file.tls { "https" } else { "http" };
    let url = format!(
        "{scheme}://{ip}:{port}/?pin={pin}&fp={}",
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

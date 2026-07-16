//! Loopback-only control panel API + static UI.
//!
//! Bound strictly to 127.0.0.1 — LAN peers can never reach pairing PINs,
//! grants, or trust management. The static panel page comes from the same
//! web dist as the viewer (`panel.html`).

use axum::{
    extract::{DefaultBodyLimit, Query, State},
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
        .route("/api/transfers", get(transfers_list))
        .route("/api/transfers/answer", post(transfers_answer))
        // The upload is streamed to a spool file with our own max_file_mb
        // cap — axum's default 2 MiB body limit must not apply here.
        .route(
            "/api/send-file",
            post(send_file).layer(DefaultBodyLimit::disable()),
        )
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
    audio_allowed: bool,
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
    audio_allowed: bool,
    /// The device is currently receiving host audio (the "listening"
    /// indicator required by the audio privacy design).
    audio_active: bool,
    stats: ndsp_protocol::messages::ViewerStats,
}

#[derive(Serialize)]
struct StatusView {
    name: String,
    version: String,
    fingerprint: String,
    /// SHA-256 fingerprint of the TLS cert when serving HTTPS.
    tls_fingerprint: Option<String>,
    audio_enabled: bool,
    port: u16,
    pin: String,
    viewer_urls: Vec<String>,
    /// File offers waiting for an accept/deny decision here.
    pending_transfers: Vec<crate::transfers::PendingOfferView>,
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
            clipboard_allowed: c
                .clipboard_allowed
                .load(std::sync::atomic::Ordering::Relaxed),
            audio_allowed: c.audio_allowed.load(std::sync::atomic::Ordering::Relaxed),
            audio_active: c.audio_active.load(std::sync::atomic::Ordering::Relaxed),
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
            audio_allowed: d.audio_allowed,
            online: online.contains(&d.device_id),
        })
        .collect();
    let scheme = state.viewer_scheme();
    Json(StatusView {
        name: state.cfg.name.clone(),
        version: env!("CARGO_PKG_VERSION").into(),
        fingerprint: state.fingerprint.clone(),
        tls_fingerprint: state.tls_fingerprint.lock().unwrap().clone(),
        audio_enabled: state.cfg.file.audio_enabled,
        port,
        pin: state.pins.current_pin(),
        viewer_urls: local_ips()
            .iter()
            .map(|ip| format!("{scheme}://{ip}:{port}/"))
            .collect(),
        pending_transfers: state.transfers.list(),
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

#[derive(Deserialize)]
struct GrantReq {
    device_id: String,
    allowed: bool,
    /// Which permission to change: "input" (default), "clipboard", "audio".
    #[serde(default)]
    kind: Option<String>,
}

async fn grant(State(state): State<Arc<AppState>>, Json(req): Json<GrantReq>) -> impl IntoResponse {
    let result = match req.kind.as_deref().unwrap_or("input") {
        "input" => state.set_input_grant(&req.device_id, req.allowed),
        "clipboard" => state.set_clipboard_grant(&req.device_id, req.allowed),
        "audio" => state.set_audio_grant(&req.device_id, req.allowed),
        other => {
            return (
                StatusCode::BAD_REQUEST,
                format!("unknown grant kind {other:?}"),
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

async fn transfers_list(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "pending": state.transfers.list(),
        "save_dir": state.cfg.file_transfer_dir().display().to_string(),
    }))
}

#[derive(Deserialize)]
struct TransferAnswerReq {
    id: String,
    accept: bool,
}

async fn transfers_answer(
    State(state): State<Arc<AppState>>,
    Json(req): Json<TransferAnswerReq>,
) -> impl IntoResponse {
    if state.transfers.answer(&req.id, req.accept) {
        (StatusCode::OK, "ok").into_response()
    } else {
        (StatusCode::NOT_FOUND, "unknown or expired transfer").into_response()
    }
}

#[derive(Deserialize)]
struct SendFileQuery {
    /// Live client id (from `/api/status`), not the device id — sending is
    /// an action on a *connected* session.
    client_id: u64,
    name: String,
}

/// Host→viewer file send (ROADMAP P2.15). The panel browser uploads the
/// file bytes (the user explicitly picked them — the service deliberately
/// exposes no "send an arbitrary host path" primitive to other local
/// processes); they are spooled + hashed, then offered to the viewer, which
/// must explicitly accept before anything is streamed.
async fn send_file(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SendFileQuery>,
    body: axum::body::Body,
) -> impl IntoResponse {
    use futures_util::StreamExt as _;
    use sha2::Digest as _;
    use tokio::io::AsyncWriteExt as _;

    let max = state.cfg.file.max_file_mb.saturating_mul(1024 * 1024);
    let dir = state.cfg.file_transfer_dir();
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("storage: {e}")).into_response();
    }
    let spool = dir.join(format!(".send-{}.tmp", uuid::Uuid::new_v4()));
    let mut file = match tokio::fs::File::create(&spool).await {
        Ok(f) => f,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("storage: {e}")).into_response()
        }
    };
    let discard = |spool: std::path::PathBuf| async move {
        let _ = tokio::fs::remove_file(&spool).await;
    };

    let mut hasher = sha2::Sha256::new();
    let mut size: u64 = 0;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                drop(file);
                discard(spool).await;
                return (StatusCode::BAD_REQUEST, format!("upload aborted: {e}")).into_response();
            }
        };
        size += chunk.len() as u64;
        if size > max {
            drop(file);
            discard(spool).await;
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                "file exceeds the max_file_mb limit",
            )
                .into_response();
        }
        hasher.update(&chunk);
        if let Err(e) = file.write_all(&chunk).await {
            drop(file);
            discard(spool).await;
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("write: {e}")).into_response();
        }
    }
    if size == 0 {
        drop(file);
        discard(spool).await;
        return (StatusCode::BAD_REQUEST, "empty file").into_response();
    }
    if let Err(e) = file.flush().await {
        drop(file);
        discard(spool).await;
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("flush: {e}")).into_response();
    }
    drop(file);
    let sha256_hex = hex::encode(hasher.finalize());
    match state.send_file_to_client(q.client_id, &q.name, size, &sha256_hex, spool.clone()) {
        Ok(id) => Json(serde_json::json!({ "id": id, "size_bytes": size })).into_response(),
        Err(e) => {
            discard(spool).await;
            (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response()
        }
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
        "{}://{ip}:{port}/?pin={pin}&fp={}",
        state.viewer_scheme(),
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

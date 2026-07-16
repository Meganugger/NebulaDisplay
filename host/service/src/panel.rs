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
use futures_util::StreamExt as _;
use qrcode::render::svg;
use qrcode::QrCode;
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tower_http::services::ServeDir;
use tracing::info;

use crate::state::AppState;
use crate::util::local_ips;

pub async fn run(state: Arc<AppState>, port: u16) -> anyhow::Result<()> {
    // Clear spooled host→viewer sends left over from a previous run.
    let outbox = state.cfg.data_dir.join("outbox");
    if let Ok(entries) = std::fs::read_dir(&outbox) {
        for e in entries.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }

    let mut app = Router::new()
        .route("/api/status", get(status))
        .route("/api/pin/rotate", post(rotate_pin))
        .route("/api/grant", post(grant))
        .route("/api/revoke", post(revoke))
        .route("/api/transfers", get(transfers_list))
        .route("/api/transfers/answer", post(transfers_answer))
        // File uploads are streamed to disk with the host's own size cap
        // (max_file_mb) enforced inside the handler.
        .route(
            "/api/send_file",
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
    /// A host→viewer file send to this device is in flight.
    sending_file: bool,
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
            sending_file: c.sending_file.load(std::sync::atomic::Ordering::Relaxed),
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
struct SendFileParams {
    device_id: String,
    name: String,
}

/// Host→viewer file send (ROADMAP P2.15): the panel streams the file body
/// here; it is spooled to `<data_dir>/outbox`, then offered to the viewer,
/// which must accept before any chunk flows. The session deletes the spool
/// when the transfer completes, is declined, or aborts.
async fn send_file(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SendFileParams>,
    body: axum::body::Body,
) -> impl IntoResponse {
    use std::sync::atomic::Ordering;
    let handle = state
        .clients
        .lock()
        .unwrap()
        .values()
        .find(|c| c.device_id == params.device_id)
        .cloned();
    let Some(handle) = handle else {
        return (StatusCode::NOT_FOUND, "device not connected".to_string()).into_response();
    };
    if handle.sending_file.load(Ordering::Relaxed) {
        return (
            StatusCode::CONFLICT,
            "a send to this device is already in progress".to_string(),
        )
            .into_response();
    }

    let max_bytes = state.cfg.file.max_file_mb.saturating_mul(1024 * 1024);
    let dir = state.cfg.data_dir.join("outbox");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("outbox: {e}")).into_response();
    }
    let id: String = {
        use rand::RngCore as _;
        let mut b = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut b);
        hex::encode(b)
    };
    let path = dir.join(format!("{id}.spool"));

    // Stream body → spool file, hashing as we go.
    let mut file = match tokio::fs::File::create(&path).await {
        Ok(f) => f,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("spool: {e}")).into_response()
        }
    };
    let mut hasher = sha2::Sha256::new();
    let mut size: u64 = 0;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                drop(file);
                let _ = tokio::fs::remove_file(&path).await;
                return (StatusCode::BAD_REQUEST, format!("upload aborted: {e}")).into_response();
            }
        };
        size += chunk.len() as u64;
        if size > max_bytes {
            drop(file);
            let _ = tokio::fs::remove_file(&path).await;
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "file exceeds the configured cap ({} MiB)",
                    state.cfg.file.max_file_mb
                ),
            )
                .into_response();
        }
        hasher.update(&chunk);
        if let Err(e) = tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await {
            drop(file);
            let _ = tokio::fs::remove_file(&path).await;
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("spool write: {e}"),
            )
                .into_response();
        }
    }
    if size == 0 {
        let _ = tokio::fs::remove_file(&path).await;
        return (StatusCode::BAD_REQUEST, "empty file".to_string()).into_response();
    }
    if let Err(e) = tokio::io::AsyncWriteExt::flush(&mut file).await {
        let _ = tokio::fs::remove_file(&path).await;
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("spool flush: {e}"),
        )
            .into_response();
    }
    drop(file);

    let sha256_hex = hex::encode(hasher.finalize());
    let cmd = crate::state::SessionCommand::SendFile {
        id: id.clone(),
        path: path.clone(),
        name: params.name,
        size_bytes: size,
        sha256_hex,
    };
    if handle.commands.send(cmd).await.is_err() {
        let _ = tokio::fs::remove_file(&path).await;
        return (StatusCode::GONE, "device disconnected".to_string()).into_response();
    }
    info!(device = %params.device_id, %id, size, "file spooled; offered to viewer");
    Json(serde_json::json!({ "id": id, "size_bytes": size })).into_response()
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

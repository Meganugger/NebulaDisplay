//! LAN-facing viewer endpoint: serves the web viewer statics and the NDSP
//! WebSocket. The plaintext phase of each socket is driven by the
//! [`crate::pairing::ServerHandshake`] state machine; successful auth hands
//! the socket to [`crate::session::run`].

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, State,
    },
    response::{Html, IntoResponse},
    routing::{any, get},
    Router,
};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tower_http::services::ServeDir;
use tracing::{debug, info, warn};

use crate::input::create_sink;
use crate::pairing::ServerHandshake;
use crate::state::AppState;

/// The whole plaintext handshake must finish within this budget.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

pub async fn run(state: Arc<AppState>, bind: IpAddr, port: u16) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(SocketAddr::new(bind, port)).await?;
    serve_on(state, listener).await
}

/// Build the viewer router (shared by the plaintext and TLS front ends).
fn build_app(state: Arc<AppState>) -> Router {
    let input_sink: Arc<dyn crate::input::InputSink> = Arc::from(create_sink(state.clone()));

    let mut app = Router::new()
        .route(ndsp_protocol::WS_PATH, any(ws_handler))
        .route("/healthz", get(|| async { "ok" }));

    match &state.cfg.web_dir {
        Some(dir) => {
            info!("serving web viewer from {}", dir.display());
            app = app.fallback_service(ServeDir::new(dir));
        }
        None => {
            warn!("web viewer dist not found — serving setup instructions instead");
            app = app.fallback(get(no_viewer_page));
        }
    }

    app.with_state((state, input_sink))
}

/// Serve on an already-bound listener (embedded/test mode uses ephemeral ports).
pub async fn serve_on(
    state: Arc<AppState>,
    listener: tokio::net::TcpListener,
) -> anyhow::Result<()> {
    let local = listener.local_addr()?;
    state.set_serving_port(local.port());
    let app = build_app(state);
    info!("viewer endpoint listening on {local}");
    // TCP_NODELAY: Nagle would coalesce small control/video writes with up
    // to ~40 ms of delayed-ACK interaction — poison for input echo latency.
    use axum::serve::ListenerExt;
    let listener = listener.tap_io(|tcp| {
        if let Err(e) = tcp.set_nodelay(true) {
            tracing::debug!("set_nodelay failed: {e}");
        }
    });
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

/// Serve the same app over TLS (`--https`) with the persisted self-signed
/// certificate (see `crate::tls`). Enables secure-context browser features
/// (WebCodecs H.264/Opus, WebCrypto, clipboard API) on LAN addresses and
/// gives the served viewer code integrity via fingerprint pinning.
pub async fn run_tls(
    state: Arc<AppState>,
    bind: IpAddr,
    port: u16,
    material: &crate::tls::TlsMaterial,
) -> anyhow::Result<()> {
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use tokio_rustls::rustls::ServerConfig;

    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut material.cert_pem.as_bytes()).collect::<Result<_, _>>()?;
    anyhow::ensure!(!certs.is_empty(), "no certificate in tls-cert.pem");
    let key: PrivateKeyDer<'static> =
        rustls_pemfile::private_key(&mut material.key_pem.as_bytes())?
            .ok_or_else(|| anyhow::anyhow!("no private key in tls-key.pem"))?;

    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()]; // WS upgrade needs 1.1
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));

    let listener = tokio::net::TcpListener::bind(SocketAddr::new(bind, port)).await?;
    let local = listener.local_addr()?;
    state.set_serving_port(local.port());
    let app = build_app(state);
    info!("viewer endpoint listening on {local} (TLS)");

    loop {
        let (tcp, addr) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                debug!("tcp accept error: {e}");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        // TCP_NODELAY: same latency rationale as the plaintext path.
        if let Err(e) = tcp.set_nodelay(true) {
            debug!("set_nodelay failed: {e}");
        }
        let acceptor = acceptor.clone();
        let app = app.clone();
        // Per-connection task: a stalled TLS handshake can't wedge accepts.
        tokio::spawn(async move {
            let tls = match tokio::time::timeout(Duration::from_secs(5), acceptor.accept(tcp)).await
            {
                Ok(Ok(tls)) => tls,
                Ok(Err(e)) => {
                    debug!(%addr, "tls handshake failed: {e}");
                    return;
                }
                Err(_) => {
                    debug!(%addr, "tls handshake timed out");
                    return;
                }
            };
            // Provide the peer address exactly the way
            // `into_make_service_with_connect_info` would — the ConnectInfo
            // extractor falls back to this request extension.
            let svc = app.layer(axum::Extension(axum::extract::ConnectInfo(addr)));
            let hyper_svc = hyper_util::service::TowerToHyperService::new(svc);
            let builder =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
            if let Err(e) = builder
                .serve_connection_with_upgrades(hyper_util::rt::TokioIo::new(tls), hyper_svc)
                .await
            {
                debug!(%addr, "tls connection ended: {e}");
            }
        });
    }
}

async fn no_viewer_page() -> impl IntoResponse {
    Html(
        "<!doctype html><meta charset=utf-8><title>NebulaDisplay</title>\
         <body style=\"font-family:system-ui;background:#101418;color:#e6edf3;display:grid;place-items:center;height:100vh;margin:0\">\
         <div style=\"max-width:40rem\"><h1>NebulaDisplay host is running</h1>\
         <p>The web viewer bundle was not found. Build it once:</p>\
         <pre style=\"background:#1c2128;padding:1rem;border-radius:8px\">cd viewer/web\nnpm install\nnpm run build</pre>\
         <p>then restart <code>nebulad</code> (or pass <code>--web-dir path/to/viewer/web/dist</code>).</p></div>",
    )
}

type Ctx = (Arc<AppState>, Arc<dyn crate::input::InputSink>);

async fn ws_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State((state, input_sink)): State<Ctx>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(state, input_sink, socket, addr))
}

async fn handle_socket(
    state: Arc<AppState>,
    input_sink: Arc<dyn crate::input::InputSink>,
    mut socket: WebSocket,
    addr: SocketAddr,
) {
    debug!(%addr, "websocket connected; starting handshake");
    let auth = match tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        drive_handshake(&state, &mut socket, addr),
    )
    .await
    {
        Ok(Some(auth)) => auth,
        Ok(None) => return,
        Err(_) => {
            warn!(%addr, "handshake timed out");
            return;
        }
    };
    crate::session::run(state, socket, auth, addr, input_sink).await;
}

async fn drive_handshake(
    state: &Arc<AppState>,
    socket: &mut WebSocket,
    addr: SocketAddr,
) -> Option<crate::pairing::AuthComplete> {
    use futures_util::StreamExt;
    let mut hs = ServerHandshake::new(state.clone(), addr.ip());
    while let Some(Ok(msg)) = socket.next().await {
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => return None,
            Message::Ping(_) | Message::Pong(_) => continue,
            _ => {
                warn!(%addr, "non-text message during handshake");
                return None;
            }
        };
        let ctl = match ndsp_protocol::messages::ControlMsg::from_json(&text) {
            Ok(c) => c,
            Err(e) => {
                debug!(%addr, "bad handshake json: {e}");
                return None;
            }
        };
        let step = hs.process(ctl);
        for reply in &step.replies {
            let json = reply.to_json().expect("reply serialization");
            if socket.send(Message::Text(json.into())).await.is_err() {
                return None;
            }
        }
        if let Some(reason) = step.reject {
            debug!(%addr, %reason, "handshake rejected");
            let _ = socket.send(Message::Close(None)).await;
            return None;
        }
        if step.complete.is_some() {
            return step.complete;
        }
    }
    None
}

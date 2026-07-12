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

/// Serve on an already-bound listener (embedded/test mode uses ephemeral ports).
pub async fn serve_on(
    state: Arc<AppState>,
    listener: tokio::net::TcpListener,
) -> anyhow::Result<()> {
    let local = listener.local_addr()?;
    state.set_serving_port(local.port());
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

    let tls_enabled = state.cfg.file.tls;
    let data_dir = state.cfg.data_dir.clone();
    let app = app.with_state((state, input_sink));

    if tls_enabled {
        let identity = crate::tls::load_or_create(&data_dir)?;
        info!(
            fingerprint = %identity.fingerprint_hex,
            "viewer endpoint listening on {local} (HTTPS/WSS, self-signed — pin this fingerprint)"
        );
        return serve_tls(app, listener, identity).await;
    }

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

/// TLS accept loop: rustls handshake per connection, then hand the stream to
/// hyper with WebSocket upgrade support. Mirrors what `axum::serve` does for
/// plain TCP (including TCP_NODELAY and per-connection tasks).
async fn serve_tls(
    app: Router<()>,
    listener: tokio::net::TcpListener,
    identity: crate::tls::TlsIdentity,
) -> anyhow::Result<()> {
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use tower::Service;

    let acceptor = tokio_rustls::TlsAcceptor::from(identity.config.clone());
    let mut make_service = app.into_make_service_with_connect_info::<SocketAddr>();
    loop {
        let (tcp, remote_addr) = listener.accept().await?;
        if let Err(e) = tcp.set_nodelay(true) {
            debug!("set_nodelay failed: {e}");
        }
        let tower_service = match make_service.call(remote_addr).await {
            Ok(svc) => svc,
            Err(infallible) => match infallible {},
        };
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(tcp).await {
                Ok(s) => s,
                Err(e) => {
                    debug!(%remote_addr, "TLS handshake failed: {e}");
                    return;
                }
            };
            let hyper_service = hyper::service::service_fn(
                move |request: hyper::Request<hyper::body::Incoming>| {
                    tower_service.clone().call(request)
                },
            );
            if let Err(e) = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(TokioIo::new(tls_stream), hyper_service)
                .await
            {
                debug!(%remote_addr, "connection error: {e}");
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

//! QUIC transport endpoint (ROADMAP P1.5).
//!
//! Listens on the same port number as the TCP viewer endpoint, UDP side.
//! Each connection speaks the identical NDSP handshake as the WebSocket
//! path (driven by the same transport-agnostic
//! [`crate::pairing::ServerHandshake`]), then runs the identical
//! [`crate::session`] — only the byte transport differs (see
//! [`crate::transport`] for the stream/frame mapping and the security
//! rationale for the self-signed TLS certificate).
//!
//! What QUIC buys over TCP+WS: no head-of-line blocking across video
//! frames on lossy links (a lost packet delays only its own frame's
//! stream), plus slightly cheaper connection establishment. On clean LAN
//! the latency is on par with TCP_NODELAY — see `docs/ROADMAP.md`'s 2026-07
//! assessment; this transport exists for the lossy-Wi-Fi case.

use quinn::crypto::rustls::QuicServerConfig;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::pairing::ServerHandshake;
use crate::state::AppState;
use crate::transport::{read_frame, write_frame, Transport, FRAME_TEXT, MAX_CONTROL_FRAME};

/// Handshake budget (mirrors the WebSocket path).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
/// NDSP-over-QUIC ALPN.
pub const ALPN: &[u8] = b"ndsp/1";

/// Bind the QUIC endpoint and serve connections until shutdown.
pub async fn run(
    state: Arc<AppState>,
    input_sink: Arc<dyn crate::input::InputSink>,
    bind: IpAddr,
    port: u16,
) -> anyhow::Result<()> {
    let endpoint = make_endpoint(&state, SocketAddr::new(bind, port))?;
    serve_on(state, input_sink, endpoint).await
}

/// Build the server endpoint with the host's persistent self-signed cert.
pub fn make_endpoint(state: &AppState, addr: SocketAddr) -> anyhow::Result<quinn::Endpoint> {
    let material = crate::tls::load_or_create(&state.cfg.data_dir)?;
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut material.cert_pem.as_bytes()).collect::<Result<_, _>>()?;
    anyhow::ensure!(!certs.is_empty(), "no certificate in tls-cert.pem");
    let key: rustls::pki_types::PrivateKeyDer<'static> =
        rustls_pemfile::private_key(&mut material.key_pem.as_bytes())?
            .ok_or_else(|| anyhow::anyhow!("no private key in tls-key.pem"))?;

    let mut crypto = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_no_client_auth()
    .with_single_cert(certs, key)?;
    crypto.alpn_protocols = vec![ALPN.to_vec()];

    let mut server_config =
        quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(crypto)?));
    let transport = Arc::get_mut(&mut server_config.transport).expect("fresh config");
    transport.max_idle_timeout(Some(Duration::from_secs(30).try_into()?));
    transport.keep_alive_interval(Some(Duration::from_secs(5)));

    Ok(quinn::Endpoint::server(server_config, addr)?)
}

/// Accept-loop over an already-bound endpoint (embedded/test mode).
pub async fn serve_on(
    state: Arc<AppState>,
    input_sink: Arc<dyn crate::input::InputSink>,
    endpoint: quinn::Endpoint,
) -> anyhow::Result<()> {
    info!(
        "QUIC viewer endpoint listening on {} (udp)",
        endpoint.local_addr()?
    );
    while let Some(incoming) = endpoint.accept().await {
        let state = state.clone();
        let input_sink = input_sink.clone();
        tokio::spawn(async move {
            match incoming.await {
                Ok(conn) => handle_conn(state, input_sink, conn).await,
                Err(e) => debug!("quic connection failed: {e}"),
            }
        });
    }
    Ok(())
}

async fn handle_conn(
    state: Arc<AppState>,
    input_sink: Arc<dyn crate::input::InputSink>,
    conn: quinn::Connection,
) {
    let addr = conn.remote_address();
    debug!(%addr, "quic connected; awaiting control stream");
    let accepted = tokio::time::timeout(HANDSHAKE_TIMEOUT, conn.accept_bi()).await;
    let (mut send, mut recv) = match accepted {
        Ok(Ok(streams)) => streams,
        Ok(Err(e)) => {
            debug!(%addr, "quic control stream failed: {e}");
            return;
        }
        Err(_) => {
            warn!(%addr, "quic client opened no control stream");
            return;
        }
    };

    let auth = match tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        drive_handshake(&state, &mut send, &mut recv, addr),
    )
    .await
    {
        Ok(Some(auth)) => auth,
        Ok(None) => return,
        Err(_) => {
            warn!(%addr, "quic handshake timed out");
            return;
        }
    };

    crate::session::run(
        state,
        Transport::Quic { conn, send, recv },
        auth,
        addr,
        input_sink,
    )
    .await;
}

/// The same handshake the WS path drives, over type-0 control frames.
async fn drive_handshake(
    state: &Arc<AppState>,
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
    addr: SocketAddr,
) -> Option<crate::pairing::AuthComplete> {
    let mut hs = ServerHandshake::new(state.clone(), addr.ip());
    loop {
        let (frame_type, payload) = match read_frame(recv, MAX_CONTROL_FRAME).await {
            Ok(Some(f)) => f,
            Ok(None) => return None,
            Err(e) => {
                debug!(%addr, "quic handshake read failed: {e:#}");
                return None;
            }
        };
        if frame_type != FRAME_TEXT {
            warn!(%addr, "non-text frame during quic handshake");
            return None;
        }
        let ctl = match std::str::from_utf8(&payload)
            .ok()
            .and_then(|s| ndsp_protocol::messages::ControlMsg::from_json(s).ok())
        {
            Some(c) => c,
            None => {
                debug!(%addr, "bad quic handshake json");
                return None;
            }
        };
        let step = hs.process(ctl);
        for reply in &step.replies {
            let json = reply.to_json().expect("reply serialization");
            if write_frame(send, FRAME_TEXT, json.as_bytes())
                .await
                .is_err()
            {
                return None;
            }
        }
        if let Some(reason) = step.reject {
            debug!(%addr, %reason, "quic handshake rejected");
            return None;
        }
        if step.complete.is_some() {
            return step.complete;
        }
    }
}

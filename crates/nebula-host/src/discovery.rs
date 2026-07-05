//! LAN discovery: a tiny UDP request/response beacon.
//!
//! Viewers broadcast `NDSP-DISCOVER-1` to UDP port 38471; the host answers
//! with a small JSON document. Discovery is **untrusted by design** — it only
//! advertises the host's name, port, protocol version, and TLS fingerprint.
//! A discovered host still requires PIN pairing before anything is streamed,
//! so a rogue responder cannot take over a viewer; the worst it can do is
//! advertise itself, at which point pairing fails without the host PIN.
//!
//! Web viewers cannot send UDP; they use QR/manual connect instead.

use std::sync::Arc;

use serde::Serialize;
use tokio::net::UdpSocket;
use tracing::{debug, info};

use crate::state::AppState;

#[derive(Serialize)]
struct DiscoveryReply<'a> {
    service: &'static str,
    version: u16,
    name: &'a str,
    port: u16,
    tls: bool,
    /// SHA-256 fingerprint of the TLS certificate, for viewer-side pinning.
    tls_fingerprint: Option<&'a str>,
}

pub async fn run_responder(state: Arc<AppState>) -> anyhow::Result<()> {
    if !state.config().discovery {
        info!("LAN discovery disabled by config");
        return Ok(());
    }
    let socket = UdpSocket::bind(("0.0.0.0", nebula_proto::DISCOVERY_PORT)).await?;
    info!(
        "LAN discovery listening on udp/{}",
        nebula_proto::DISCOVERY_PORT
    );
    let mut buf = [0u8; 512];
    loop {
        let (n, peer) = socket.recv_from(&mut buf).await?;
        let msg = String::from_utf8_lossy(&buf[..n]);
        if !msg.starts_with(nebula_proto::DISCOVERY_MAGIC) {
            continue;
        }
        let cfg = state.config();
        let reply = DiscoveryReply {
            service: "nebuladisplay",
            version: nebula_proto::PROTOCOL_VERSION,
            name: &cfg.host_name,
            port: cfg.port,
            tls: cfg.tls,
            tls_fingerprint: state.tls_fingerprint.as_deref(),
        };
        let json = serde_json::to_vec(&reply)?;
        debug!("discovery probe from {peer}");
        socket.send_to(&json, peer).await.ok();
    }
}

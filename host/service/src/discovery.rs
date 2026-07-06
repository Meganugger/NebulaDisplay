//! UDP discovery responder. Answers `NDSP-DISCOVER/1` probes with a JSON
//! beacon. Conveys location only — never trust (see protocol docs).

use ndsp_protocol::discovery::{Beacon, PROBE};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tracing::{info, warn};

use crate::state::AppState;

pub async fn run(state: Arc<AppState>, port: u16) {
    let sock = match UdpSocket::bind(("0.0.0.0", port)).await {
        Ok(s) => s,
        Err(e) => {
            warn!("discovery disabled: cannot bind UDP {port}: {e}");
            return;
        }
    };
    info!("discovery listening on udp/{port}");
    let mut buf = [0u8; 512];
    loop {
        let Ok((n, from)) = sock.recv_from(&mut buf).await else {
            continue;
        };
        if &buf[..n.min(PROBE.len())] != PROBE {
            continue;
        }
        let beacon = Beacon {
            service: "ndsp".into(),
            protocol: ndsp_protocol::PROTOCOL_VERSION,
            name: state.cfg.name.clone(),
            port: state.serving_port(),
            fingerprint: state.fingerprint.clone(),
        };
        if let Err(e) = sock.send_to(&beacon.to_bytes(), from).await {
            warn!("discovery reply to {from} failed: {e}");
        }
    }
}

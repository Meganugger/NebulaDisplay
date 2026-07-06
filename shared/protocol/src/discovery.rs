//! LAN discovery beacon (UDP).
//!
//! Viewers broadcast a probe on [`crate::DEFAULT_DISCOVERY_PORT`]; hosts reply
//! with a JSON beacon. **Discovery conveys zero trust** — it only tells a
//! viewer where a host *might* be. Pairing/token auth is always required
//! before anything sensitive flows, so a rogue beacon can at worst advertise
//! itself, never silently take over a viewer.

use serde::{Deserialize, Serialize};

/// Magic prefix of a discovery probe datagram.
pub const PROBE: &[u8] = b"NDSP-DISCOVER/1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Beacon {
    /// Always "ndsp".
    pub service: String,
    pub protocol: u16,
    /// Host display name.
    pub name: String,
    /// TCP port of the viewer HTTP/WS endpoint.
    pub port: u16,
    /// SHA-256 hex fingerprint of the host identity key. Viewers that paired
    /// before can recognize the same host across IP changes.
    pub fingerprint: String,
}

impl Beacon {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("beacon serialization cannot fail")
    }
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let beacon: Beacon = serde_json::from_slice(b).ok()?;
        (beacon.service == "ndsp").then_some(beacon)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beacon_roundtrip() {
        let b = Beacon {
            service: "ndsp".into(),
            protocol: 1,
            name: "Desk PC".into(),
            port: 41800,
            fingerprint: "ab".repeat(32),
        };
        assert_eq!(Beacon::from_bytes(&b.to_bytes()).unwrap(), b);
    }

    #[test]
    fn non_ndsp_rejected() {
        assert!(Beacon::from_bytes(
            br#"{"service":"other","protocol":1,"name":"x","port":1,"fingerprint":""}"#
        )
        .is_none());
        assert!(Beacon::from_bytes(b"garbage").is_none());
    }
}

//! # NebulaDisplay Stream Protocol (NDSP)
//!
//! NDSP is the original, versioned wire protocol used between a NebulaDisplay
//! host (the machine whose desktop is extended/mirrored) and viewers
//! (browser, desktop, Android, iOS).
//!
//! ## Transport model
//!
//! NDSP runs over a single ordered, reliable, message-oriented transport —
//! WebSocket (TLS by default) in v1, with QUIC/WebTransport as a planned
//! additive transport. Two kinds of transport messages exist:
//!
//! * **Text messages** carry JSON-encoded [`ControlMessage`] values
//!   (handshake, pairing, session control, input, stats, errors).
//! * **Binary messages** carry media packets with a compact fixed header:
//!   [`VideoPacket`] and [`AudioPacket`]. Media stays binary because it is
//!   high-rate and latency-sensitive; control stays JSON because it is
//!   low-rate and benefits from easy debugging and forward compatibility
//!   (unknown JSON fields are ignored).
//!
//! ## Versioning & compatibility
//!
//! Every connection starts with `Hello` / `HelloAck` carrying
//! `protocol_version` (single integer, currently [`PROTOCOL_VERSION`]) and a
//! capability list. Rules:
//!
//! * The host answers with the highest version it shares with the client.
//! * Adding new JSON message types or fields is backward compatible
//!   (receivers must ignore unknown `type`s and fields).
//! * Binary packet layouts are frozen per `packet_version` (first byte after
//!   the channel byte); new layouts get a new packet version.
//!
//! ## Security model (summary — see docs/SECURITY.md)
//!
//! Discovery is untrusted. Nothing is streamed and no input is accepted
//! until the client either (a) proves knowledge of a short-lived pairing PIN
//! displayed on the host, receiving a long-lived random device token, or
//! (b) presents a previously issued token. Input injection additionally
//! requires the host user to have enabled input for that device.

pub mod control;
pub mod packet;

pub use control::*;
pub use packet::*;

/// Current protocol version. Bump on breaking changes only.
pub const PROTOCOL_VERSION: u16 = 1;

/// Default TCP port for the host's HTTPS + WebSocket endpoint.
pub const DEFAULT_PORT: u16 = 38470;

/// Default UDP port for LAN discovery.
pub const DISCOVERY_PORT: u16 = 38471;

/// Magic string a discovery probe must start with.
pub const DISCOVERY_MAGIC: &str = "NDSP-DISCOVER-1";

/// Negotiate a protocol version. Returns `None` when there is no overlap.
pub fn negotiate_version(
    client_min: u16,
    client_max: u16,
    host_min: u16,
    host_max: u16,
) -> Option<u16> {
    let lo = client_min.max(host_min);
    let hi = client_max.min(host_max);
    if lo <= hi {
        Some(hi)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_negotiation() {
        assert_eq!(negotiate_version(1, 1, 1, 1), Some(1));
        assert_eq!(negotiate_version(1, 3, 2, 5), Some(3));
        assert_eq!(negotiate_version(4, 6, 1, 3), None);
        assert_eq!(negotiate_version(1, 2, 2, 9), Some(2));
    }
}

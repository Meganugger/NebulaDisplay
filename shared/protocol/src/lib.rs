//! # NDSP — NebulaDisplay Stream Protocol
//!
//! Versioned, clean-room protocol for local virtual-monitor / remote-display
//! streaming. Design goals:
//!
//! * **Local-first**: no cloud, no accounts. Discovery is separate from trust.
//! * **Encrypted by default**: an ECDH(P-256) + PIN-bound HKDF handshake
//!   establishes an AES-256-GCM session key. Everything after authentication
//!   travels inside encrypted envelopes, even on plain WebSocket transports.
//! * **Versioned**: every `Hello` carries `PROTOCOL_VERSION`; peers negotiate
//!   down to the highest common version. v1 is the baseline described in
//!   `docs/PROTOCOL.md`.
//!
//! Layer map:
//!
//! ```text
//! transport   WebSocket (binary) — QUIC/WebTransport planned
//! envelope    [chan u8][counter u64 BE][AES-256-GCM ciphertext+tag]
//! channels    1 = control (JSON ControlMsg)   2 = video   3 = audio (reserved)
//! ```

pub mod crypto;
pub mod discovery;
pub mod envelope;
pub mod files;
pub mod media;
pub mod messages;
pub mod spake2;

/// Current protocol version. Bump on breaking changes; peers negotiate
/// `min(client, server)` and refuse to talk below [`MIN_PROTOCOL_VERSION`].
pub const PROTOCOL_VERSION: u16 = 1;
/// Oldest version this implementation still speaks.
pub const MIN_PROTOCOL_VERSION: u16 = 1;

/// Default TCP port for the viewer HTTP + WebSocket endpoint.
pub const DEFAULT_PORT: u16 = 41800;
/// Default loopback-only control-panel port.
pub const DEFAULT_PANEL_PORT: u16 = 41888;
/// Default UDP discovery port.
pub const DEFAULT_DISCOVERY_PORT: u16 = 41799;

/// WebSocket route on the host that speaks NDSP.
pub const WS_PATH: &str = "/ndsp";

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("malformed frame: {0}")]
    Malformed(&'static str),
    #[error("unsupported protocol version {0} (supported {min}..={max})", min = MIN_PROTOCOL_VERSION, max = PROTOCOL_VERSION)]
    Version(u16),
    #[error("crypto failure: {0}")]
    Crypto(&'static str),
    #[error("replayed or out-of-order envelope counter")]
    Replay,
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, ProtocolError>;

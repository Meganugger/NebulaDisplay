//! Session transport abstraction: WebSocket (TCP) and QUIC (ROADMAP P1.5)
//! behind lane-oriented sinks so `session.rs` stays transport-agnostic.
//!
//! # NDSP-over-QUIC wire mapping (ALPN `ndsp/1`)
//!
//! * **Control** — the client-opened bidirectional stream. Frames are
//!   `[type u8][len u32 BE][payload]`; type 0 = plaintext handshake JSON,
//!   type 1 = encrypted envelope. After `AuthOk` only type 1 is legal.
//! * **Audio** — one server-opened unidirectional stream, first byte `'A'`,
//!   then `[len u32 BE][envelope]` frames in order (audio wants ordering;
//!   an in-lane stall is concealed by the client jitter buffer).
//! * **Video** — a **fresh server-opened unidirectional stream per frame**:
//!   `'V'`, one `[len u32 BE][envelope]`, FIN. Frames can complete out of
//!   order on loss; the envelope layer's per-channel monotonic counter
//!   check makes receivers drop late (stale) frames — exactly the
//!   latest-only semantics the WS path gets from its send slot, but without
//!   head-of-line blocking behind a lost packet.
//!
//! QUIC's TLS certificate is the host's persistent self-signed cert and is
//! *not* what protects the session: exactly like `ws://`, all authenticity
//! and confidentiality come from the NDSP layer (SPAKE2/token handshake +
//! per-session AES-256-GCM envelopes). Clients therefore skip TLS cert
//! verification; see `docs/SECURITY.md`.

use axum::extract::ws::{Message, WebSocket};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};

/// Max accepted control-stream frame (file chunks are ≤ 256 KiB raw →
/// ~342 KiB base64 + JSON overhead; 4 MiB leaves generous headroom).
pub const MAX_CONTROL_FRAME: usize = 4 * 1024 * 1024;

/// Handshake frame carrying plaintext JSON.
pub const FRAME_TEXT: u8 = 0;
/// Frame carrying an encrypted envelope.
pub const FRAME_ENVELOPE: u8 = 1;

/// Stream tag bytes for server-opened unidirectional streams.
pub const UNI_VIDEO: u8 = b'V';
pub const UNI_AUDIO: u8 = b'A';

/// An authenticated connection, ready to become a session.
// Boxing the WS variant would only relocate one short-lived value; the
// enum exists for a handful of instants per connection.
#[allow(clippy::large_enum_variant)]
pub enum Transport {
    Ws(WebSocket),
    Quic {
        conn: quinn::Connection,
        send: quinn::SendStream,
        recv: quinn::RecvStream,
    },
}

impl Transport {
    /// Split into the writer-owned sink and the pump-owned stream.
    pub fn split(self) -> (SessionSink, SessionStream) {
        match self {
            Transport::Ws(socket) => {
                let (tx, rx) = socket.split();
                (SessionSink::Ws(tx), SessionStream::Ws(rx))
            }
            Transport::Quic { conn, send, recv } => (
                SessionSink::Quic {
                    conn,
                    control: send,
                    audio: None,
                },
                SessionStream::Quic(recv),
            ),
        }
    }
}

/// Outbound half. All sends return `Err(())` when the peer is gone.
pub enum SessionSink {
    Ws(SplitSink<WebSocket, Message>),
    Quic {
        conn: quinn::Connection,
        control: quinn::SendStream,
        /// Lazily-opened ordered audio lane.
        audio: Option<quinn::SendStream>,
    },
}

impl SessionSink {
    pub async fn send_control(&mut self, envelope: Vec<u8>) -> Result<(), ()> {
        match self {
            SessionSink::Ws(tx) => tx
                .send(Message::Binary(envelope.into()))
                .await
                .map_err(|_| ()),
            SessionSink::Quic { control, .. } => {
                write_frame(control, FRAME_ENVELOPE, &envelope).await
            }
        }
    }

    pub async fn send_audio(&mut self, envelope: Vec<u8>) -> Result<(), ()> {
        match self {
            SessionSink::Ws(tx) => tx
                .send(Message::Binary(envelope.into()))
                .await
                .map_err(|_| ()),
            SessionSink::Quic { conn, audio, .. } => {
                if audio.is_none() {
                    let mut s = conn.open_uni().await.map_err(|_| ())?;
                    s.write_all(&[UNI_AUDIO]).await.map_err(|_| ())?;
                    *audio = Some(s);
                }
                let s = audio.as_mut().expect("just opened");
                let len = (envelope.len() as u32).to_be_bytes();
                if s.write_all(&len).await.is_err() || s.write_all(&envelope).await.is_err() {
                    // Audio lane broke (flow control reset); drop the lane —
                    // it re-opens on the next block. Connection errors will
                    // surface on the control lane.
                    *audio = None;
                }
                Ok(())
            }
        }
    }

    pub async fn send_video(&mut self, envelope: Vec<u8>) -> Result<(), ()> {
        match self {
            SessionSink::Ws(tx) => tx
                .send(Message::Binary(envelope.into()))
                .await
                .map_err(|_| ()),
            SessionSink::Quic { conn, .. } => {
                let mut s = conn.open_uni().await.map_err(|_| ())?;
                let mut head = Vec::with_capacity(5);
                head.push(UNI_VIDEO);
                head.extend_from_slice(&(envelope.len() as u32).to_be_bytes());
                s.write_all(&head).await.map_err(|_| ())?;
                s.write_all(&envelope).await.map_err(|_| ())?;
                s.finish().map_err(|_| ())?;
                Ok(())
            }
        }
    }

    pub async fn close(&mut self) {
        match self {
            SessionSink::Ws(tx) => {
                let _ = tx.close().await;
            }
            SessionSink::Quic { conn, .. } => {
                conn.close(0u32.into(), b"bye");
            }
        }
    }
}

/// Inbound events, normalized across transports.
pub enum Inbound {
    /// One encrypted envelope.
    Envelope(Vec<u8>),
    /// Peer went away (close frame, connection loss, stream FIN).
    Closed,
    /// Protocol violation (e.g. plaintext after auth) — close the session.
    Violation(&'static str),
}

/// Receiving half owned by the session pump. Clients only ever send on the
/// control lane, so QUIC needs nothing beyond the bidirectional stream.
pub enum SessionStream {
    Ws(SplitStream<WebSocket>),
    Quic(quinn::RecvStream),
}

impl SessionStream {
    pub async fn next(&mut self) -> Inbound {
        match self {
            SessionStream::Ws(rx) => loop {
                match rx.next().await {
                    Some(Ok(Message::Binary(data))) => return Inbound::Envelope(data.to_vec()),
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return Inbound::Closed,
                    Some(Ok(Message::Text(_))) => {
                        return Inbound::Violation("plaintext message after auth")
                    }
                    Some(Ok(_)) => continue, // ping/pong
                }
            },
            SessionStream::Quic(recv) => match read_frame(recv, MAX_CONTROL_FRAME).await {
                Ok(Some((FRAME_ENVELOPE, payload))) => Inbound::Envelope(payload),
                Ok(Some((FRAME_TEXT, _))) => Inbound::Violation("plaintext frame after auth"),
                Ok(Some(_)) => Inbound::Violation("unknown control frame type"),
                Ok(None) | Err(_) => Inbound::Closed,
            },
        }
    }
}

/// Write one `[type][len][payload]` frame on a QUIC stream.
pub async fn write_frame(
    stream: &mut quinn::SendStream,
    frame_type: u8,
    payload: &[u8],
) -> Result<(), ()> {
    let mut head = [0u8; 5];
    head[0] = frame_type;
    head[1..5].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    stream.write_all(&head).await.map_err(|_| ())?;
    stream.write_all(payload).await.map_err(|_| ())?;
    Ok(())
}

/// Read one `[type][len][payload]` frame. `Ok(None)` = clean FIN before a
/// new frame; errors on truncation, oversize, or connection loss.
pub async fn read_frame(
    stream: &mut quinn::RecvStream,
    max_len: usize,
) -> anyhow::Result<Option<(u8, Vec<u8>)>> {
    let mut head = [0u8; 5];
    match stream.read_exact(&mut head).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(head[1..5].try_into().unwrap()) as usize;
    anyhow::ensure!(len <= max_len, "frame length {len} exceeds cap {max_len}");
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    Ok(Some((head[0], payload)))
}

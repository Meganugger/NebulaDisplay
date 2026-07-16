//! Control-channel messages (JSON, `snake_case`-tagged).
//!
//! Pre-authentication messages (`Hello`, `Pair*`) travel as plaintext WebSocket
//! text frames. Everything else must be wrapped in an encrypted
//! [`crate::envelope`] on channel 1.

use serde::{Deserialize, Serialize};

/// Who a peer is. Sent in `Hello`; echoed to the panel UI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientInfo {
    /// Stable, client-generated device id (UUID). Not a secret.
    pub device_id: String,
    /// Human-readable name shown in the panel ("Pixel 8", "Firefox on laptop").
    pub name: String,
    /// e.g. "web", "windows", "android", "ios", "desktop"
    pub platform: String,
    /// Client app version string.
    pub app_version: String,
    /// Optional feature flags ("cursor" → renders the host cursor from
    /// CursorShape/CursorPos messages instead of expecting it baked into the
    /// video). Unknown flags are ignored; absent = no optional features.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerInfo {
    pub name: String,
    pub app_version: String,
    /// SHA-256 fingerprint (hex) of the host identity key — lets a returning
    /// client detect a different machine squatting on the same address.
    pub fingerprint: String,
}

/// How the client wants to authenticate this connection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum AuthMethod {
    /// First contact, legacy scheme: PIN-bound HKDF pairing. Kept for
    /// viewers that cannot do curve arithmetic outside their platform
    /// crypto APIs (mobile apps); can be disabled host-side
    /// (`allow_legacy_pairing = false`).
    Pair,
    /// First contact, current scheme: SPAKE2 (PAKE) pairing — the recorded
    /// transcript cannot be ground offline against the PIN.
    PairSpake2,
    /// Returning device: prove possession of a previously issued trust token.
    /// The token itself is never sent; see `TokenProof`.
    Token { device_id: String },
}

/// Streaming quality presets. The server maps these to encoder/pacing params.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    /// Battery/CPU friendly: 30 fps cap, modest bitrate.
    #[default]
    Office,
    /// Smooth motion: 60 fps target, generous bitrate, larger jitter budget.
    Video,
    /// Stylus/drawing: latency over fidelity, 60 fps, small frames.
    Drawing,
    /// Ultra-low latency: minimal buffering, aggressive keyframes.
    Gaming,
}

/// Video codecs a peer can produce/consume.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    Jpeg,
    H264,
    Hevc,
    Av1,
}

/// Audio payload formats a viewer can play (JSON mirror of
/// [`crate::media::AudioCodec`]).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AudioWireCodec {
    #[default]
    Opus,
    /// Uncompressed s16le — for viewers without an Opus decoder
    /// (web viewers on insecure origins have no WebCodecs).
    Pcm,
}

/// Hard cap on a single clipboard payload (either direction).
pub const CLIPBOARD_MAX_BYTES: usize = 256 * 1024;
/// Raw bytes per file-transfer chunk (base64 on the wire).
pub const FILE_CHUNK_BYTES: usize = 256 * 1024;

/// Input capabilities / modes a viewer may request.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum InputMode {
    #[default]
    ViewOnly,
    Touchpad,
    DirectTouch,
    KeyboardMouse,
    DrawingTablet,
}

/// A single input event, normalized to the remote display surface.
/// Coordinates are `0.0..=1.0` relative to the streamed monitor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputEvent {
    MouseMove {
        x: f32,
        y: f32,
    },
    /// button: 0 = left, 1 = middle, 2 = right, 3 = x1, 4 = x2
    MouseButton {
        button: u8,
        pressed: bool,
    },
    Wheel {
        dx: f32,
        dy: f32,
    },
    /// `code` is a W3C `KeyboardEvent.code` string ("KeyA", "Enter", ...) —
    /// a *physical* key position. `key` is the layout-resolved
    /// `KeyboardEvent.key` value ("a", "é", "Enter", …) when the viewer has
    /// one; hosts prefer it for printable characters so a French viewer
    /// typing on a US-layout host still produces the right glyphs.
    Key {
        code: String,
        pressed: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
    Touch {
        id: u32,
        phase: TouchPhase,
        x: f32,
        y: f32,
        pressure: f32,
    },
    /// Stylus with pressure/tilt where the viewer platform exposes them.
    /// `pressure` is 0..1; `tilt_x`/`tilt_y` are normalized -1..1
    /// (±1 = ±90°). Hosts with Windows Ink inject these as true pen input.
    Pen {
        phase: TouchPhase,
        x: f32,
        y: f32,
        pressure: f32,
        tilt_x: f32,
        tilt_y: f32,
    },
    /// IME/committed text that has no single key code.
    Text {
        text: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TouchPhase {
    Start,
    Move,
    End,
    Cancel,
}

/// Client → server runtime stats used for adaptation + panel display.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ViewerStats {
    pub fps_decoded: f32,
    pub decode_ms_avg: f32,
    /// Frames waiting to be decoded/presented client-side.
    pub queue_depth: u32,
    pub frames_dropped: u32,
    /// Most recent RTT the client measured via Ping/Pong, in ms.
    pub rtt_ms: f32,
    /// Measured end-to-end latency (capture timestamp → presentation), ms.
    pub e2e_latency_ms: f32,
    /// Capture-timestamp → envelope-arrival (host pipeline + network), ms.
    /// Measured against the synced clock. 0 until the clock is synced.
    #[serde(default)]
    pub net_ms_avg: f32,
    /// Decode-completion → paint wait (presentation scheduling), ms.
    #[serde(default)]
    pub present_wait_ms_avg: f32,
}

/// Server → client/panel stats.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct HostStats {
    pub capture_fps: f32,
    pub encode_ms_avg: f32,
    pub target_bitrate_kbps: u32,
    pub actual_bitrate_kbps: u32,
    pub frames_sent: u64,
    pub frames_skipped: u64,
    pub clients: u32,
    /// Age of the captured frame when its encode started (capture → encode
    /// scheduling wait), ms.
    #[serde(default)]
    pub capture_age_ms_avg: f32,
    /// Color-conversion share of `encode_ms_avg` (BGRA → I420 etc.), ms.
    #[serde(default)]
    pub convert_ms_avg: f32,
    /// Encrypt + socket write time per video frame, ms.
    #[serde(default)]
    pub seal_send_ms_avg: f32,
}

/// One display mode the host offers for a (virtual) monitor.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

/// All control-plane messages. `#[serde(tag = "type")]` keeps the wire format
/// self-describing and forward-extensible (unknown fields are ignored).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMsg {
    // ---- plaintext phase -------------------------------------------------
    Hello {
        protocol: u16,
        client: ClientInfo,
        auth: AuthMethod,
        /// Codecs the viewer can decode, in preference order.
        codecs: Vec<Codec>,
    },
    HelloAck {
        protocol: u16,
        server: ServerInfo,
        /// True → proceed with PairStart; false → proceed with TokenProof.
        pairing_required: bool,
        /// Random per-connection nonce (base64, 16 bytes) bound into both
        /// the pairing transcript and token proofs to prevent replay.
        connection_nonce: String,
    },
    /// Client ephemeral P-256 public key (base64 SEC1 compressed).
    PairStart {
        client_pubkey: String,
    },
    /// Server ephemeral key + HKDF salt (both base64).
    PairChallenge {
        server_pubkey: String,
        salt: String,
    },
    /// AES-GCM(seal) of `"ndsp-confirm-v1" || connection_nonce` under the
    /// PIN-bound pairing key. Proves the client knew the PIN.
    PairConfirm {
        sealed: String,
    },
    /// On success carries the sealed long-term trust token for this device.
    PairResult {
        ok: bool,
        sealed_token: Option<String>,
        error: Option<String>,
    },
    /// SPAKE2 pairing, message 1 (client → server): the client's masked
    /// share `pA` (base64, uncompressed SEC1). See `crypto::spake2`.
    Spake2Start {
        share: String,
    },
    /// SPAKE2 pairing, message 2 (server → client): the server's masked
    /// share `pB` (base64, uncompressed SEC1).
    Spake2Challenge {
        share: String,
    },
    /// SPAKE2 pairing, message 3 (client → server): the client's
    /// transcript confirmation MAC (base64). Proves the client knew the PIN.
    Spake2Confirm {
        mac: String,
    },
    /// SPAKE2 pairing, message 4 (server → client). On success `mac` is the
    /// server's confirmation (mutual authentication — verify before trusting
    /// anything!) and `sealed_token` carries the device trust token sealed
    /// under the SPAKE2 token key.
    Spake2Result {
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mac: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sealed_token: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Proof of trust-token possession, bound to this handshake's transcript:
    /// base64(SHA-256(token || connection_nonce || client_pubkey || server_pubkey)).
    /// Requires a preceding PairStart/PairChallenge ephemeral exchange (which
    /// also yields the session key), so an active MITM substituting keys
    /// invalidates the proof.
    TokenProof {
        proof: String,
    },
    /// Ends the plaintext phase. After this, both sides switch to envelopes.
    AuthOk {
        /// Codec the server selected for this session.
        codec: Codec,
        /// Initial mode being streamed.
        mode: DisplayMode,
        /// Whether this device is currently allowed to inject input.
        input_allowed: bool,
    },
    AuthErr {
        error: String,
    },

    // ---- encrypted phase (channel 1) -------------------------------------
    /// Clock sync + liveness. `t0_us` is the sender's monotonic-ish
    /// microsecond clock (unix epoch based).
    Ping {
        t0_us: u64,
    },
    Pong {
        t0_us: u64,
        t1_us: u64,
    },
    /// Viewer requests a quality profile.
    SetProfile {
        profile: Profile,
    },
    /// Viewer requests an input mode (grant still enforced server-side).
    SetInputMode {
        mode: InputMode,
    },
    /// Viewer asks for a fresh keyframe (e.g. after decode error).
    RequestKeyframe,
    /// Batched input events.
    Input {
        events: Vec<InputEvent>,
    },
    /// Periodic client stats (also drives adaptation).
    Stats {
        stats: ViewerStats,
    },
    /// Periodic host stats for overlays.
    HostStats {
        stats: HostStats,
    },
    /// Server informs the viewer its input grant changed (panel toggle).
    InputGrant {
        allowed: bool,
    },
    /// Server is about to switch modes (resolution change etc.).
    ModeChange {
        mode: DisplayMode,
    },
    /// Host cursor image changed. Sent to clients that advertised the
    /// "cursor" feature; `rgba` is base64 of tightly packed RGBA8.
    CursorShape {
        width: u16,
        height: u16,
        hot_x: u16,
        hot_y: u16,
        rgba: String,
    },
    /// Host cursor moved / visibility changed. Coordinates are normalized
    /// (0..1) against the captured surface — the same space input events use.
    /// Rides the control channel, so it is never queued behind video frames.
    CursorPos {
        x: f32,
        y: f32,
        visible: bool,
    },
    /// Viewer enables/disables the audio stream for itself (off by default;
    /// the host only streams when the device's audio permission also allows
    /// it). `codec` picks the payload format the viewer can play.
    SetAudio {
        enabled: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codec: Option<AudioWireCodec>,
    },
    /// Server informs the viewer its audio permission changed (panel toggle).
    AudioGrant {
        allowed: bool,
    },
    /// Clipboard text, either direction. Only honored when the device's
    /// clipboard permission is granted (deny by default) and the payload is
    /// within [`CLIPBOARD_MAX_BYTES`].
    Clipboard {
        text: String,
    },
    /// Server informs the viewer its clipboard permission changed.
    ClipboardGrant {
        allowed: bool,
    },
    /// Viewer offers a file to the host. The host queues it for an explicit
    /// per-transfer accept in the control panel — nothing is written before
    /// the user says yes. `sha256` is hex of the file content.
    FileOffer {
        id: String,
        name: String,
        size_bytes: u64,
        sha256: String,
    },
    /// Host's answer to a `FileOffer` (after the panel decision).
    FileAnswer {
        id: String,
        accept: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// One file chunk (base64), ≤ [`FILE_CHUNK_BYTES`] raw bytes, sent in
    /// order after an accepting `FileAnswer`.
    FileChunk {
        id: String,
        seq: u32,
        data: String,
    },
    /// Sender finished; receiver verifies size + sha256 and answers with
    /// `FileDone` (ok) or `FileAbort`.
    FileEnd {
        id: String,
    },
    /// Receiver confirms a completed, verified transfer.
    FileDone {
        id: String,
    },
    /// Either side cancels a transfer.
    FileAbort {
        id: String,
        reason: String,
    },
    /// Graceful shutdown/teardown with reason.
    Bye {
        reason: String,
    },
    /// Non-fatal error report either direction.
    Error {
        code: String,
        message: String,
    },
}

impl ControlMsg {
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }
    pub fn from_json(s: &str) -> serde_json::Result<Self> {
        serde_json::from_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_roundtrip() {
        let msg = ControlMsg::Input {
            events: vec![
                InputEvent::MouseMove { x: 0.5, y: 0.25 },
                InputEvent::Key {
                    code: "KeyA".into(),
                    pressed: true,
                    key: Some("a".into()),
                },
                InputEvent::Touch {
                    id: 1,
                    phase: TouchPhase::Start,
                    x: 0.1,
                    y: 0.9,
                    pressure: 0.7,
                },
            ],
        };
        let json = msg.to_json().unwrap();
        assert_eq!(ControlMsg::from_json(&json).unwrap(), msg);
    }

    #[test]
    fn tagged_format_is_stable() {
        let json = ControlMsg::Ping { t0_us: 42 }.to_json().unwrap();
        assert_eq!(json, r#"{"type":"ping","t0_us":42}"#);
    }

    #[test]
    fn key_event_without_layout_key_still_parses() {
        // v0.2 viewers send Key without the `key` field — must keep working.
        let msg = ControlMsg::from_json(
            r#"{"type":"input","events":[{"kind":"key","code":"KeyQ","pressed":true}]}"#,
        )
        .unwrap();
        assert_eq!(
            msg,
            ControlMsg::Input {
                events: vec![InputEvent::Key {
                    code: "KeyQ".into(),
                    pressed: true,
                    key: None,
                }],
            }
        );
    }

    #[test]
    fn spake2_and_feature_messages_roundtrip() {
        for msg in [
            ControlMsg::Spake2Start {
                share: "c0ffee".into(),
            },
            ControlMsg::Spake2Challenge {
                share: "beef".into(),
            },
            ControlMsg::Spake2Confirm { mac: "abcd".into() },
            ControlMsg::Spake2Result {
                ok: true,
                mac: Some("m".into()),
                sealed_token: Some("t".into()),
                error: None,
            },
            ControlMsg::SetAudio {
                enabled: true,
                codec: Some(AudioWireCodec::Pcm),
            },
            ControlMsg::AudioGrant { allowed: false },
            ControlMsg::Clipboard {
                text: "hello".into(),
            },
            ControlMsg::ClipboardGrant { allowed: true },
            ControlMsg::FileOffer {
                id: "x1".into(),
                name: "report.pdf".into(),
                size_bytes: 1024,
                sha256: "00".repeat(32),
            },
            ControlMsg::FileAnswer {
                id: "x1".into(),
                accept: false,
                reason: Some("declined".into()),
            },
            ControlMsg::FileChunk {
                id: "x1".into(),
                seq: 3,
                data: "QUJD".into(),
            },
            ControlMsg::FileEnd { id: "x1".into() },
            ControlMsg::FileDone { id: "x1".into() },
            ControlMsg::FileAbort {
                id: "x1".into(),
                reason: "cancelled".into(),
            },
        ] {
            let json = msg.to_json().unwrap();
            assert_eq!(ControlMsg::from_json(&json).unwrap(), msg, "{json}");
        }
    }

    #[test]
    fn auth_method_wire_tags_are_stable() {
        assert_eq!(
            serde_json::to_string(&AuthMethod::PairSpake2).unwrap(),
            r#"{"method":"pair_spake2"}"#
        );
        assert_eq!(
            serde_json::to_string(&AuthMethod::Pair).unwrap(),
            r#"{"method":"pair"}"#
        );
    }

    #[test]
    fn unknown_fields_ignored_for_forward_compat() {
        let msg =
            ControlMsg::from_json(r#"{"type":"ping","t0_us":7,"future_field":true}"#).unwrap();
        assert_eq!(msg, ControlMsg::Ping { t0_us: 7 });
    }
}

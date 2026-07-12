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
    /// First contact, legacy: PIN-bound-HKDF pairing (see `crate::crypto`).
    /// A recorded transcript can be PIN-ground offline — kept only for older
    /// clients and can be disabled host-side (`allow_legacy_pair = false`).
    Pair,
    /// First contact, preferred: SPAKE2 PAKE pairing (see `crate::pake`).
    /// Same four-message flow as `Pair`; passive transcripts reveal nothing.
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
    /// a *physical* key position. `key` optionally carries the layout-aware
    /// `KeyboardEvent.key` value ("a", "é", "Enter", …) so hosts can inject
    /// the intended character when the physical position has no mapping
    /// (non-US layouts, exotic keys). Absent on older clients.
    Key {
        code: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
        pressed: bool,
    },
    Touch {
        id: u32,
        phase: TouchPhase,
        x: f32,
        y: f32,
        pressure: f32,
    },
    /// Stylus with pressure/tilt where the viewer platform exposes them.
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
    /// Clipboard content, either direction. Only text for now (`mime` is
    /// forward-extensible). Both sides enforce [`MAX_CLIPBOARD_BYTES`] and
    /// the per-device clipboard grant (deny by default, like input).
    Clipboard {
        /// Currently always `"text/plain"`.
        mime: String,
        /// The clipboard text (UTF-8).
        data: String,
    },
    /// Server informs the viewer its clipboard grant changed (panel toggle).
    ClipboardGrant {
        allowed: bool,
    },
    /// Viewer requests audio streaming on/off for this session (channel 3).
    /// The host only streams when audio capture is enabled host-side *and*
    /// the client asked for it — audio is opt-in on both ends.
    SetAudio {
        enabled: bool,
    },
    /// Server → client: audio availability/state for this session.
    /// `available` — host has an audio source configured; `active` — frames
    /// are currently being streamed to *this* client.
    AudioStatus {
        available: bool,
        active: bool,
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

/// Upper bound for a single [`ControlMsg::Clipboard`] `data` payload (bytes
/// of UTF-8). Enforced by both peers: oversize events are dropped with an
/// `Error { code: "clipboard_too_large" }`, never truncated silently.
pub const MAX_CLIPBOARD_BYTES: usize = 256 * 1024;

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
                    key: Some("a".into()),
                    pressed: true,
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
    fn unknown_fields_ignored_for_forward_compat() {
        let msg =
            ControlMsg::from_json(r#"{"type":"ping","t0_us":7,"future_field":true}"#).unwrap();
        assert_eq!(msg, ControlMsg::Ping { t0_us: 7 });
    }

    #[test]
    fn key_event_without_layout_key_still_parses() {
        // Older clients send only `code` — must keep deserializing.
        let msg = ControlMsg::from_json(
            r#"{"type":"input","events":[{"kind":"key","code":"KeyA","pressed":true}]}"#,
        )
        .unwrap();
        assert_eq!(
            msg,
            ControlMsg::Input {
                events: vec![InputEvent::Key {
                    code: "KeyA".into(),
                    key: None,
                    pressed: true,
                }],
            }
        );
        // And when we serialize without a key, the field is omitted (older
        // servers must not choke on an unexpected null). Note `"kind":"key"`
        // is the enum tag — only the `"key":` *field* must be absent.
        assert!(!msg.to_json().unwrap().contains("\"key\":"));
    }

    #[test]
    fn spake2_auth_method_wire_format() {
        let json = serde_json::to_string(&AuthMethod::PairSpake2).unwrap();
        assert_eq!(json, r#"{"method":"pair_spake2"}"#);
    }

    #[test]
    fn clipboard_and_audio_roundtrip() {
        for msg in [
            ControlMsg::Clipboard {
                mime: "text/plain".into(),
                data: "héllo".into(),
            },
            ControlMsg::ClipboardGrant { allowed: true },
            ControlMsg::SetAudio { enabled: true },
            ControlMsg::AudioStatus {
                available: true,
                active: false,
            },
        ] {
            let json = msg.to_json().unwrap();
            assert_eq!(ControlMsg::from_json(&json).unwrap(), msg);
        }
    }
}

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
    /// First contact, legacy method: PIN-bound HKDF pairing. Kept for older
    /// viewers; a recorded transcript is offline-grindable, so new clients
    /// should use [`AuthMethod::PairPake`].
    Pair,
    /// First contact, current method: SPAKE2 (P-256) PIN pairing. The same
    /// `PairStart`/`PairChallenge`/`PairConfirm` flow, but the exchanged
    /// points are password-blinded so recorded transcripts cannot be ground
    /// offline (see `crypto::pake`).
    PairPake,
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
    /// the *physical* key position. `key` optionally carries the W3C
    /// `KeyboardEvent.key` value ("a", "A", "é", "Enter", ...) — the
    /// *layout-resolved* meaning on the viewer — so the host can pick the
    /// best injection strategy when host and viewer keyboard layouts differ.
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
        /// Whether this device is currently allowed to sync clipboard text.
        /// Deny by default; toggled per device in the host panel.
        #[serde(default)]
        clipboard_allowed: bool,
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
    /// Clipboard text sync, either direction. Only honored when the device's
    /// clipboard grant is on (deny by default); both ends enforce
    /// [`MAX_CLIPBOARD_TEXT_BYTES`](crate::MAX_CLIPBOARD_TEXT_BYTES).
    Clipboard {
        text: String,
    },
    /// Server informs the viewer its clipboard grant changed (panel toggle).
    ClipboardGrant {
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
    fn unknown_fields_ignored_for_forward_compat() {
        let msg =
            ControlMsg::from_json(r#"{"type":"ping","t0_us":7,"future_field":true}"#).unwrap();
        assert_eq!(msg, ControlMsg::Ping { t0_us: 7 });
    }

    /// Old viewers send `key` events without the layout field, and old hosts
    /// must accept `auth_ok` without `clipboard_allowed` — both directions of
    /// the v0.4 wire format keep parsing.
    #[test]
    fn v04_wire_compat_for_new_optional_fields() {
        let msg = ControlMsg::from_json(
            r#"{"type":"input","events":[{"kind":"key","code":"KeyA","pressed":true}]}"#,
        )
        .unwrap();
        assert_eq!(
            msg,
            ControlMsg::Input {
                events: vec![InputEvent::Key {
                    code: "KeyA".into(),
                    pressed: true,
                    key: None,
                }],
            }
        );
        // Key without layout info serializes without the field (old hosts
        // that reject unknown fields would still be fine — but don't emit it).
        let json = msg.to_json().unwrap();
        assert!(
            !json.contains("\"key\":"),
            "absent key must be omitted: {json}"
        );

        let auth_ok = ControlMsg::from_json(
            r#"{"type":"auth_ok","codec":"jpeg","mode":{"width":1,"height":1,"refresh_hz":60},"input_allowed":false}"#,
        )
        .unwrap();
        assert!(matches!(
            auth_ok,
            ControlMsg::AuthOk {
                clipboard_allowed: false,
                ..
            }
        ));
    }

    #[test]
    fn auth_method_pake_tag() {
        let json = serde_json::to_string(&AuthMethod::PairPake).unwrap();
        assert_eq!(json, r#"{"method":"pair_pake"}"#);
    }
}

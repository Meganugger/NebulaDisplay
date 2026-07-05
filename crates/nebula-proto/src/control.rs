//! JSON control-plane messages.
//!
//! All control messages are serialized as JSON objects with a `type` tag.
//! Receivers MUST ignore unknown message types and unknown fields — this is
//! the primary forward-compatibility mechanism of NDSP.

use serde::{Deserialize, Serialize};

/// Capabilities a peer can advertise in `Hello` / `HelloAck`.
///
/// Capabilities are free-form strings so new ones can be introduced without
/// a protocol version bump. Well-known values are defined as constants.
pub mod caps {
    /// Peer can decode/encode Motion-JPEG (dirty-rect composited).
    pub const VIDEO_MJPEG: &str = "video/mjpeg";
    /// Peer can decode/encode H.264 (WebCodecs / MediaCodec / VideoToolbox / MF).
    pub const VIDEO_H264: &str = "video/h264";
    /// Peer can decode/encode HEVC.
    pub const VIDEO_HEVC: &str = "video/hevc";
    /// Peer can decode/encode AV1.
    pub const VIDEO_AV1: &str = "video/av1";
    /// Peer supports Opus audio.
    pub const AUDIO_OPUS: &str = "audio/opus";
    /// Peer supports raw PCM (s16le) audio.
    pub const AUDIO_PCM: &str = "audio/pcm-s16le";
    /// Viewer can send input events (mouse/keyboard/touch/stylus).
    pub const INPUT: &str = "input";
    /// Viewer supports clipboard sync (explicit permission still required).
    pub const CLIPBOARD: &str = "clipboard";
    /// Host exposes a real virtual monitor (IddCx driver present).
    pub const VIRTUAL_DISPLAY: &str = "virtual-display";
    /// Host is running in capture-only mirror fallback mode.
    pub const CAPTURE_MIRROR: &str = "capture-mirror";
}

/// Why a session exists: mirror an existing screen or extend the desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayMode {
    /// Duplicate an existing monitor.
    Mirror,
    /// Attach a new virtual monitor (requires the IddCx driver on Windows).
    Extend,
}

/// A display mode a virtual monitor can be set to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoModeInfo {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

/// Performance profile presets. Tunables live host-side; the name is the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    /// Battery/CPU friendly: lower FPS cap, aggressive dirty-rect reuse.
    Office,
    /// Smooth full-frame updates for media playback.
    Video,
    /// Tuned for stylus/drawing: low latency, high chroma quality.
    Drawing,
    /// Ultra-low latency, highest FPS the link sustains.
    Gaming,
    #[default]
    Balanced,
}

/// Input event kinds a viewer may send. Coordinates are normalized to
/// `0..=1` in stream space so they are resolution-independent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputEvent {
    MouseMove {
        x: f64,
        y: f64,
    },
    MouseButton {
        button: MouseButton,
        down: bool,
        x: f64,
        y: f64,
    },
    MouseWheel {
        dx: f64,
        dy: f64,
    },
    Key {
        code: String,
        down: bool,
    },
    Touch {
        id: u32,
        phase: TouchPhase,
        x: f64,
        y: f64,
        pressure: Option<f64>,
    },
    Stylus {
        x: f64,
        y: f64,
        pressure: f64,
        tilt_x: Option<f64>,
        tilt_y: Option<f64>,
        down: bool,
        eraser: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TouchPhase {
    Down,
    Move,
    Up,
    Cancel,
}

/// Periodic client → host quality feedback used by the adaptive controller.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct ClientFeedback {
    /// Highest frame id fully decoded and presented.
    pub last_presented_frame: u32,
    /// Frames the client dropped (decode overload / late arrival) since last report.
    pub dropped_frames: u32,
    /// Client-measured decode time, milliseconds (rolling average).
    pub decode_ms: f32,
    /// Client-side present queue depth in frames (jitter buffer occupancy).
    pub queue_depth: u32,
}

/// Live stream statistics, host → client, also mirrored on the host
/// diagnostics panel.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct StreamStats {
    pub fps: f32,
    pub bitrate_kbps: f32,
    pub encode_ms: f32,
    pub capture_ms: f32,
    pub rtt_ms: f32,
    pub quality: u8,
    pub frames_sent: u64,
    pub frames_dropped: u64,
    pub width: u32,
    pub height: u32,
}

/// All JSON control messages. Tagged with `type` on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMessage {
    // ------------------------------------------------------------------
    // Handshake
    // ------------------------------------------------------------------
    /// First message, client → host.
    Hello {
        min_version: u16,
        max_version: u16,
        /// Human-readable client name, e.g. "Web viewer (Chrome 126)".
        client_name: String,
        /// Stable random ID generated by the client on first run.
        device_id: String,
        capabilities: Vec<String>,
    },
    /// Host reply. After this the client must `Auth` or `PairRequest`.
    HelloAck {
        version: u16,
        host_name: String,
        capabilities: Vec<String>,
        /// Whether this device_id is already trusted (token still required).
        known_device: bool,
    },

    // ------------------------------------------------------------------
    // Pairing & authentication
    // ------------------------------------------------------------------
    /// Client proves knowledge of the PIN currently displayed on the host.
    PairRequest {
        pin: String,
        device_name: String,
    },
    /// Pairing succeeded; `token` must be stored securely by the client.
    PairOk {
        token: String,
    },
    /// Authenticate with a previously issued token.
    Auth {
        token: String,
    },
    AuthOk {
        /// Whether the host user has enabled input injection for this device.
        input_allowed: bool,
    },

    // ------------------------------------------------------------------
    // Session control
    // ------------------------------------------------------------------
    /// Ask the host to start streaming.
    SessionStart {
        mode: DisplayMode,
        profile: Profile,
        /// Preferred mode; the host picks the closest supported one.
        preferred: Option<VideoModeInfo>,
        /// Viewer surface size in physical pixels (used to pick a mode).
        viewport_width: u32,
        viewport_height: u32,
        /// Codecs the viewer accepts, in preference order.
        codecs: Vec<String>,
        /// Opt into audio (host may still refuse; off by default).
        want_audio: bool,
    },
    /// Host confirms; video packets will follow on the binary channel.
    SessionStarted {
        codec: String,
        mode: VideoModeInfo,
        display_mode: DisplayMode,
        audio: bool,
        /// Which monitor index is being streamed (for multi-monitor hosts).
        monitor_index: u32,
    },
    /// Either side stops the stream (connection stays up).
    SessionStop {
        reason: String,
    },
    /// Client asks for a different mode/profile mid-session.
    ModeChange {
        preferred: Option<VideoModeInfo>,
        profile: Option<Profile>,
    },

    // ------------------------------------------------------------------
    // Input
    // ------------------------------------------------------------------
    /// Batched input events (client → host). Rejected unless authorized.
    Input {
        events: Vec<InputEvent>,
    },
    /// Host informs the viewer input permission changed.
    InputPermission {
        allowed: bool,
    },

    // ------------------------------------------------------------------
    // Clipboard (explicit permission; disabled by default)
    // ------------------------------------------------------------------
    ClipboardOffer {
        mime: String,
        size: u64,
    },
    ClipboardAccept {},
    ClipboardData {
        mime: String,
        data_base64: String,
    },

    // ------------------------------------------------------------------
    // Liveness, stats, adaptive feedback
    // ------------------------------------------------------------------
    Ping {
        t_micros: u64,
    },
    Pong {
        t_micros: u64,
    },
    Feedback(ClientFeedback),
    Stats(StreamStats),

    // ------------------------------------------------------------------
    // Errors & lifecycle
    // ------------------------------------------------------------------
    Error {
        code: ErrorCode,
        message: String,
    },
    /// Graceful close; `resume_token` allows fast reconnect.
    Bye {
        resume_token: Option<String>,
    },
    /// Fast re-attach after a network blip.
    Resume {
        resume_token: String,
        last_frame: u32,
    },
    ResumeOk {
        from_frame: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    ProtocolMismatch,
    BadPin,
    PinExpired,
    BadToken,
    NotAuthorized,
    InputDenied,
    Busy,
    Internal,
    UnsupportedCodec,
}

impl ControlMessage {
    /// Serialize to the JSON wire form.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("control messages always serialize")
    }

    /// Parse from the JSON wire form.
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_hello() {
        let m = ControlMessage::Hello {
            min_version: 1,
            max_version: 1,
            client_name: "test".into(),
            device_id: "abc".into(),
            capabilities: vec![caps::VIDEO_MJPEG.into(), caps::INPUT.into()],
        };
        let j = m.to_json();
        assert_eq!(ControlMessage::from_json(&j).unwrap(), m);
    }

    #[test]
    fn round_trip_input_batch() {
        let m = ControlMessage::Input {
            events: vec![
                InputEvent::MouseMove { x: 0.5, y: 0.25 },
                InputEvent::Touch {
                    id: 1,
                    phase: TouchPhase::Down,
                    x: 0.1,
                    y: 0.9,
                    pressure: Some(0.7),
                },
                InputEvent::Stylus {
                    x: 0.3,
                    y: 0.3,
                    pressure: 0.5,
                    tilt_x: Some(0.1),
                    tilt_y: None,
                    down: true,
                    eraser: false,
                },
            ],
        };
        let j = m.to_json();
        assert_eq!(ControlMessage::from_json(&j).unwrap(), m);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        // Forward compatibility: an older peer must parse messages that
        // contain fields added in newer protocol revisions.
        let j = r#"{"type":"pair_ok","token":"t","added_in_v9":true}"#;
        let m = ControlMessage::from_json(j).unwrap();
        assert_eq!(m, ControlMessage::PairOk { token: "t".into() });
    }

    #[test]
    fn unknown_type_is_error_not_panic() {
        assert!(ControlMessage::from_json(r#"{"type":"from_the_future"}"#).is_err());
    }

    #[test]
    fn wire_shape_is_stable() {
        // Guard the JSON shape so accidental refactors don't break clients.
        let m = ControlMessage::Auth { token: "x".into() };
        assert_eq!(m.to_json(), r#"{"type":"auth","token":"x"}"#);
    }
}

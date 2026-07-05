//! End-to-end integration test: boots the real host server in-process and
//! drives it with a real WebSocket client through the full NDSP flow —
//! hello → (failed + successful) pairing → token auth → session start →
//! video packets → input permission → feedback/stats.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use nebula_host::config::Config;
use nebula_host::server;
use nebula_host::state::AppState;
use nebula_proto::{
    caps, ControlMessage, DisplayMode, ErrorCode, Profile, VideoPacket, PROTOCOL_VERSION,
};
use tokio_tungstenite::tungstenite::Message as WsMessage;

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn start_test_server() -> (Arc<AppState>, SocketAddr) {
    let config = Config {
        tls: false,
        frame_source: "test".into(),
        discovery: false,
        ..Config::default()
    };
    let state = Arc::new(AppState::for_tests(config));
    let app = server::router(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    // Give the acceptor a beat.
    tokio::time::sleep(Duration::from_millis(50)).await;
    (state, addr)
}

async fn connect(addr: SocketAddr) -> WsStream {
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("websocket connect");
    ws
}

async fn send(ws: &mut WsStream, msg: ControlMessage) {
    ws.send(WsMessage::Text(msg.to_json())).await.unwrap();
}

/// Receive the next *control* message, skipping binary media.
async fn recv_control(ws: &mut WsStream) -> ControlMessage {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for control message")
            .expect("stream ended")
            .expect("ws error");
        match msg {
            WsMessage::Text(t) => {
                return ControlMessage::from_json(&t).expect("valid control json")
            }
            WsMessage::Binary(_) => continue,
            WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
            other => panic!("unexpected ws message: {other:?}"),
        }
    }
}

/// Receive the next binary (media) message.
async fn recv_binary(ws: &mut WsStream) -> Vec<u8> {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for media")
            .expect("stream ended")
            .expect("ws error");
        match msg {
            WsMessage::Binary(b) => return b,
            _ => continue,
        }
    }
}

fn hello(device_id: &str) -> ControlMessage {
    ControlMessage::Hello {
        min_version: 1,
        max_version: PROTOCOL_VERSION,
        client_name: "e2e-test".into(),
        device_id: device_id.into(),
        capabilities: vec![caps::VIDEO_MJPEG.into(), caps::INPUT.into()],
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn full_pairing_streaming_and_input_flow() {
    let (state, addr) = start_test_server().await;
    let mut ws = connect(addr).await;

    // --- Handshake ---
    send(&mut ws, hello("device-e2e")).await;
    let ack = recv_control(&mut ws).await;
    let ControlMessage::HelloAck {
        version,
        known_device,
        capabilities,
        ..
    } = ack
    else {
        panic!("expected HelloAck, got {ack:?}");
    };
    assert_eq!(version, PROTOCOL_VERSION);
    assert!(!known_device);
    assert!(capabilities.iter().any(|c| c == caps::VIDEO_MJPEG));

    // --- Streaming before auth must be refused ---
    send(
        &mut ws,
        ControlMessage::SessionStart {
            mode: DisplayMode::Mirror,
            profile: Profile::Balanced,
            preferred: None,
            viewport_width: 640,
            viewport_height: 480,
            codecs: vec![caps::VIDEO_MJPEG.into()],
            want_audio: false,
        },
    )
    .await;
    let refused = recv_control(&mut ws).await;
    assert!(
        matches!(
            refused,
            ControlMessage::Error {
                code: ErrorCode::NotAuthorized,
                ..
            }
        ),
        "unauthenticated session start must be refused, got {refused:?}"
    );

    // --- Wrong PIN is rejected ---
    let real_pin = state.pairing.lock().unwrap().issue_pin();
    let wrong_pin = if real_pin == "000000" {
        "111111"
    } else {
        "000000"
    };
    send(
        &mut ws,
        ControlMessage::PairRequest {
            pin: wrong_pin.into(),
            device_name: "E2E".into(),
        },
    )
    .await;
    let bad = recv_control(&mut ws).await;
    assert!(matches!(
        bad,
        ControlMessage::Error {
            code: ErrorCode::BadPin,
            ..
        }
    ));

    // --- Correct PIN pairs and yields a token ---
    send(
        &mut ws,
        ControlMessage::PairRequest {
            pin: real_pin,
            device_name: "E2E".into(),
        },
    )
    .await;
    let paired = recv_control(&mut ws).await;
    let ControlMessage::PairOk { token } = paired else {
        panic!("expected PairOk, got {paired:?}");
    };
    assert_eq!(token.len(), 64, "token must be 32 random bytes hex-encoded");

    // --- Start a session; verify video flows and is decodable ---
    send(
        &mut ws,
        ControlMessage::SessionStart {
            mode: DisplayMode::Mirror,
            profile: Profile::Balanced,
            preferred: None,
            viewport_width: 640,
            viewport_height: 480,
            codecs: vec![caps::VIDEO_MJPEG.into()],
            want_audio: false,
        },
    )
    .await;
    let started = recv_control(&mut ws).await;
    let ControlMessage::SessionStarted {
        codec, mode, audio, ..
    } = started
    else {
        panic!("expected SessionStarted, got {started:?}");
    };
    assert_eq!(codec, caps::VIDEO_MJPEG);
    assert!(!audio, "audio must be off by default");
    assert!(mode.width > 0 && mode.height > 0);

    let first = recv_binary(&mut ws).await;
    let video = VideoPacket::decode(&first).expect("valid video packet");
    assert!(video.full_frame, "first frame must be full");
    assert_eq!(&video.payload[..2], &[0xFF, 0xD8], "payload must be JPEG");

    // More frames keep coming (animation in the test source).
    let second = recv_binary(&mut ws).await;
    let v2 = VideoPacket::decode(&second).unwrap();
    assert!(v2.frame_id > video.frame_id);

    // --- Input: denied by default, works after the host grants it ---
    send(
        &mut ws,
        ControlMessage::Input {
            events: vec![nebula_proto::InputEvent::MouseMove { x: 0.5, y: 0.5 }],
        },
    )
    .await;
    // (Silently dropped; no error is required. Now grant input.)
    assert!(state
        .trust
        .lock()
        .unwrap()
        .set_input_allowed("device-e2e", true));

    // Send feedback + a ping to exercise adaptive/stats paths.
    send(
        &mut ws,
        ControlMessage::Feedback(nebula_proto::ClientFeedback {
            last_presented_frame: v2.frame_id,
            dropped_frames: 0,
            decode_ms: 4.0,
            queue_depth: 1,
        }),
    )
    .await;
    send(&mut ws, ControlMessage::Ping { t_micros: 42 }).await;

    // We must observe: a Pong (echo), an InputPermission{allowed:true} after
    // the next input batch, and periodic Stats.
    send(
        &mut ws,
        ControlMessage::Input {
            events: vec![nebula_proto::InputEvent::MouseMove { x: 0.25, y: 0.75 }],
        },
    )
    .await;

    let mut saw_pong = false;
    let mut saw_permission = false;
    let mut saw_stats = false;
    for _ in 0..30 {
        let msg = recv_control(&mut ws).await;
        match msg {
            ControlMessage::Pong { t_micros: 42 } => saw_pong = true,
            ControlMessage::InputPermission { allowed: true } => saw_permission = true,
            ControlMessage::Stats(s) => {
                saw_stats = true;
                assert!(s.width > 0);
            }
            _ => {}
        }
        if saw_pong && saw_permission && saw_stats {
            break;
        }
    }
    assert!(saw_pong, "host must echo pings");
    assert!(
        saw_permission,
        "host must notify when input becomes allowed"
    );
    assert!(saw_stats, "host must publish stream stats");

    // --- Reconnect with the stored token (no PIN this time) ---
    send(&mut ws, ControlMessage::Bye { resume_token: None }).await;
    drop(ws);

    let mut ws2 = connect(addr).await;
    send(&mut ws2, hello("device-e2e")).await;
    let ack2 = recv_control(&mut ws2).await;
    assert!(matches!(
        ack2,
        ControlMessage::HelloAck {
            known_device: true,
            ..
        }
    ));
    send(&mut ws2, ControlMessage::Auth { token }).await;
    let authed = recv_control(&mut ws2).await;
    assert!(
        matches!(
            authed,
            ControlMessage::AuthOk {
                input_allowed: true
            }
        ),
        "token auth must succeed with input allowed, got {authed:?}"
    );

    // Bad token on a fresh connection must fail.
    let mut ws3 = connect(addr).await;
    send(&mut ws3, hello("device-e2e")).await;
    recv_control(&mut ws3).await;
    send(
        &mut ws3,
        ControlMessage::Auth {
            token: "deadbeef".into(),
        },
    )
    .await;
    let denied = recv_control(&mut ws3).await;
    assert!(matches!(
        denied,
        ControlMessage::Error {
            code: ErrorCode::BadToken,
            ..
        }
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_api_serves_status_and_devices_on_loopback() {
    let (state, addr) = start_test_server().await;
    state.trust.lock().unwrap().register("dev-x", "Tablet");

    let client = reqwest::Client::new();
    let status: serde_json::Value = client
        .get(format!("http://{addr}/api/admin/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["frame_source"], "test");
    assert_eq!(status["audio_enabled"], false);

    let devices: serde_json::Value = client
        .get(format!("http://{addr}/api/admin/devices"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(devices["devices"][0]["name"], "Tablet");
    assert_eq!(devices["devices"][0]["input_allowed"], false);

    // PIN issuance via admin API.
    let pin: serde_json::Value = client
        .post(format!("http://{addr}/api/admin/pin"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(pin["pin"].as_str().unwrap().len(), 6);
    assert!(pin["qr_payload"]
        .as_str()
        .unwrap()
        .contains("nebuladisplay-pair"));

    // Public info endpoint.
    let info: serde_json::Value = client
        .get(format!("http://{addr}/api/info"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["service"], "nebuladisplay");
}

//! End-to-end integration tests: real host (capture → encode → encrypt →
//! WebSocket) against the real client SDK over loopback TCP.

use ndsp_client::{connect, Auth, Incoming};
use ndsp_protocol::messages::{Codec, ControlMsg, InputEvent, Profile, ViewerStats};
use nebulad::{EmbeddedHost, EmbeddedOptions};
use std::time::Duration;

/// `Session` deliberately has no Debug (it holds key material); unwrap errors manually.
fn must_fail<T>(r: anyhow::Result<T>, ctx: &str) -> anyhow::Error {
    match r {
        Ok(_) => panic!("{ctx}: expected failure but succeeded"),
        Err(e) => e,
    }
}

fn client_info(name: &str) -> ndsp_protocol::messages::ClientInfo {
    ndsp_client::default_client_info(name, "test")
}

async fn start_host(tag: &str) -> EmbeddedHost {
    let dir = std::env::temp_dir().join(format!("ndsp-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    EmbeddedHost::start(EmbeddedOptions {
        data_dir: dir,
        name: format!("e2e-host-{tag}"),
        capture: (320, 240),
        max_fps: 30,
    })
    .await
    .expect("host starts")
}

#[tokio::test(flavor = "multi_thread")]
async fn pairing_streams_video_and_ping_pong_works() {
    let host = start_host("stream").await;
    let pin = host.state.pins.current_pin();

    let mut session = connect(
        "127.0.0.1",
        host.port,
        client_info("Viewer A"),
        Auth::Pin(&pin),
        vec![Codec::H264, Codec::Jpeg],
    )
    .await
    .expect("pairing with correct PIN succeeds");

    assert!(
        session.new_credentials.is_some(),
        "pairing must issue credentials"
    );
    assert!(!session.input_allowed, "input must be denied by default");
    assert_eq!(session.mode.width, 320);

    // Clock sync ping.
    session
        .send(&ControlMsg::Ping { t0_us: 12345 })
        .await
        .unwrap();

    let mut frames = Vec::new();
    let mut got_pong = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while frames.len() < 5 {
        let item = tokio::time::timeout_at(deadline, session.recv())
            .await
            .expect("timed out waiting for frames")
            .expect("recv ok");
        match item {
            Incoming::Video(f) => frames.push(f),
            Incoming::Control(ControlMsg::Pong { t0_us, t1_us }) => {
                assert_eq!(t0_us, 12345);
                assert!(t1_us > 0);
                got_pong = true;
            }
            Incoming::Control(_) => {}
            Incoming::Closed => panic!("server closed unexpectedly"),
        }
    }
    assert!(got_pong, "server must answer pings");

    // First frame decodable on its own; sequence strictly increasing; payloads change.
    assert!(frames[0].keyframe, "first frame must be a keyframe");
    for w in frames.windows(2) {
        assert!(w[1].seq > w[0].seq, "sequence must increase");
    }
    assert!(
        frames.iter().any(|f| f.payload != frames[0].payload),
        "stream must contain changing content"
    );
    #[cfg(feature = "h264")]
    assert_eq!(frames[0].codec, Codec::H264, "H264 preferred when offered");

    session.close().await;
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_pin_is_rejected_and_rotates_pin() {
    let host = start_host("wrongpin").await;
    let real_pin = host.state.pins.current_pin();
    let wrong_pin = if real_pin == "000000" {
        "000001".to_string()
    } else {
        "000000".to_string()
    };

    let err = must_fail(
        connect(
            "127.0.0.1",
            host.port,
            client_info("Evil"),
            Auth::Pin(&wrong_pin),
            vec![Codec::Jpeg],
        )
        .await,
        "wrong PIN",
    );
    assert!(
        format!("{err:#}").to_lowercase().contains("pin"),
        "error should mention PIN: {err:#}"
    );

    // Failed attempt must rotate the PIN (anti-grinding).
    assert_ne!(
        host.state.pins.current_pin(),
        real_pin,
        "PIN must rotate after failure"
    );
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn token_reconnect_and_input_grant_flow() {
    let host = start_host("token").await;
    let pin = host.state.pins.current_pin();
    let info = client_info("Tablet");

    // Pair once.
    let session = connect(
        "127.0.0.1",
        host.port,
        info.clone(),
        Auth::Pin(&pin),
        vec![Codec::Jpeg],
    )
    .await
    .expect("pairing ok");
    let creds = session.new_credentials.clone().expect("credentials issued");
    session.close().await;

    // Reconnect with the token — no PIN needed.
    let mut session = connect(
        "127.0.0.1",
        host.port,
        info.clone(),
        Auth::Token(&creds),
        vec![Codec::Jpeg],
    )
    .await
    .expect("token reconnect ok");
    assert!(session.new_credentials.is_none());
    assert!(!session.input_allowed);

    // Input events while denied are dropped server-side (session survives).
    session
        .send(&ControlMsg::Input {
            events: vec![InputEvent::MouseMove { x: 0.5, y: 0.5 }],
        })
        .await
        .unwrap();

    // Host grants input (panel action) → client is notified live.
    tokio::time::sleep(Duration::from_millis(200)).await; // session registers
    assert!(host.state.set_input_grant(&info.device_id, true).unwrap());

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut granted = false;
    while !granted {
        match tokio::time::timeout_at(deadline, session.recv())
            .await
            .expect("grant timeout")
            .unwrap()
        {
            Incoming::Control(ControlMsg::InputGrant { allowed }) => granted = allowed,
            Incoming::Closed => panic!("closed while waiting for grant"),
            _ => {}
        }
    }

    // Profile switch + stats + keyframe request round trip without dropping.
    session
        .send(&ControlMsg::SetProfile {
            profile: Profile::Gaming,
        })
        .await
        .unwrap();
    session
        .send(&ControlMsg::Stats {
            stats: ViewerStats {
                rtt_ms: 4.0,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    session.send(&ControlMsg::RequestKeyframe).await.unwrap();
    let mut saw_frame_after = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !saw_frame_after {
        match tokio::time::timeout_at(deadline, session.recv())
            .await
            .expect("frame timeout")
            .unwrap()
        {
            Incoming::Video(f) => saw_frame_after = f.keyframe,
            Incoming::Closed => panic!("closed"),
            _ => {}
        }
    }

    // Revoking kicks the live session.
    assert!(host.state.revoke_device(&info.device_id).unwrap());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(deadline, session.recv())
            .await
            .expect("kick timeout")
        {
            Ok(Incoming::Control(ControlMsg::Bye { .. })) | Ok(Incoming::Closed) => break,
            Ok(_) => {}
            Err(_) => break, // server may close abruptly after Bye
        }
    }

    // And the token no longer works.
    let err = must_fail(
        connect(
            "127.0.0.1",
            host.port,
            info,
            Auth::Token(&creds),
            vec![Codec::Jpeg],
        )
        .await,
        "revoked token",
    );
    assert!(
        format!("{err:#}").contains("pair"),
        "should hint re-pairing: {err:#}"
    );
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn fingerprint_mismatch_blocks_token_send() {
    let host = start_host("fp").await;
    let pin = host.state.pins.current_pin();
    let info = client_info("Laptop");
    let session = connect(
        "127.0.0.1",
        host.port,
        info.clone(),
        Auth::Pin(&pin),
        vec![Codec::Jpeg],
    )
    .await
    .unwrap();
    let mut creds = session.new_credentials.clone().unwrap();
    session.close().await;

    creds.host_fingerprint = "deadbeef".repeat(8);
    let err = must_fail(
        connect(
            "127.0.0.1",
            host.port,
            info,
            Auth::Token(&creds),
            vec![Codec::Jpeg],
        )
        .await,
        "impostor host",
    );
    assert!(format!("{err:#}").contains("fingerprint"));
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn discovery_answers_probes_with_beacon() {
    use ndsp_protocol::discovery::{Beacon, PROBE};
    let host = start_host("disco").await;
    // Bind the responder on an ephemeral UDP port.
    let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let disco_addr = sock.local_addr().unwrap();
    let state = host.state.clone();
    tokio::spawn(nebulad::discovery::serve(state, sock));

    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.send_to(PROBE, disco_addr).await.unwrap();
    let mut buf = [0u8; 512];
    let (n, _) = tokio::time::timeout(Duration::from_secs(5), client.recv_from(&mut buf))
        .await
        .expect("beacon timeout")
        .unwrap();
    let beacon = Beacon::from_bytes(&buf[..n]).expect("valid beacon");
    assert_eq!(beacon.service, "ndsp");
    assert_eq!(beacon.port, host.port);
    assert_eq!(beacon.fingerprint.len(), 64);
    assert!(beacon.name.contains("e2e-host-disco"));

    // Garbage probes are ignored (no reply).
    client.send_to(b"NOT-A-PROBE", disco_addr).await.unwrap();
    let r = tokio::time::timeout(Duration::from_millis(500), client.recv_from(&mut buf)).await;
    assert!(r.is_err(), "garbage must not be answered");
    host.shutdown().await;
}

/// Regression test for the v0.2 pacing bug: the session loop re-armed its
/// pacing sleep on *every* inbound message, so continuous touch input
/// (60–240 Hz) starved the video path completely — the "video freezes and
/// input lags seconds behind while dragging" failure. Video and input are
/// now independent pipelines; a sustained input flood must not stop frames.
#[tokio::test(flavor = "multi_thread")]
async fn video_keeps_flowing_under_input_flood() {
    let host = start_host("flood").await;
    let pin = host.state.pins.current_pin();
    let info = client_info("Flood phone");

    let mut session = connect(
        "127.0.0.1",
        host.port,
        info.clone(),
        Auth::Pin(&pin),
        vec![Codec::H264, Codec::Jpeg],
    )
    .await
    .expect("pairing ok");

    // Grant input + switch out of view-only so events actually hit the sink.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(host.state.set_input_grant(&info.device_id, true).unwrap());
    session
        .send(&ControlMsg::SetInputMode {
            mode: ndsp_protocol::messages::InputMode::DirectTouch,
        })
        .await
        .unwrap();

    // Flood: ~240 Hz touch-move batches for 3 seconds while receiving.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut frames: u32 = 0;
    let mut i: u32 = 0;
    loop {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        let t = (i as f32 / 240.0).fract();
        session
            .send(&ControlMsg::Input {
                events: vec![InputEvent::Touch {
                    id: 1,
                    phase: ndsp_protocol::messages::TouchPhase::Move,
                    x: t,
                    y: 1.0 - t,
                    pressure: 1.0,
                }],
            })
            .await
            .unwrap();
        i += 1;
        // Drain anything pending without blocking the flood cadence.
        loop {
            match tokio::time::timeout(Duration::from_micros(100), session.recv()).await {
                Ok(Ok(Incoming::Video(_))) => frames += 1,
                Ok(Ok(Incoming::Control(_))) => {}
                Ok(Ok(Incoming::Closed)) => panic!("server closed during input flood"),
                Ok(Err(e)) => panic!("recv error during flood: {e:#}"),
                Err(_) => break, // nothing pending
            }
        }
        tokio::time::sleep(Duration::from_micros(4_000)).await; // ≈240 Hz
    }
    // Collect stragglers.
    let drain_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while frames < 30 {
        match tokio::time::timeout_at(drain_deadline, session.recv()).await {
            Ok(Ok(Incoming::Video(_))) => frames += 1,
            Ok(Ok(_)) => {}
            _ => break,
        }
    }
    // At the host's 30 fps cap, 3 s of streaming should deliver ~90 frames.
    // Require at least a third — anything near zero means input starved video.
    // Measured on this harness: the v0.2 shared-loop design delivered 30
    // frames here (input kept resetting the pacing sleep); the split
    // pipeline delivers ~94. Require 60 so a pacing regression fails loudly.
    eprintln!("input-flood test: {frames} frames in 3s");
    assert!(
        frames >= 60,
        "video starved under input flood: only {frames} frames in 3s (expect ~90)"
    );

    session.close().await;
    host.shutdown().await;
}

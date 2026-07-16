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
        ..Default::default()
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
            Incoming::Control(_) | Incoming::Audio(_) => {}
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
                Ok(Ok(Incoming::Control(_) | Incoming::Audio(_))) => {}
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

// ---------------------------------------------------------------------------
// v0.5 features: PAKE gating, clipboard sync, audio opt-in
// ---------------------------------------------------------------------------

async fn start_host_opts(tag: &str, opts: impl FnOnce(&mut EmbeddedOptions)) -> EmbeddedHost {
    let dir = std::env::temp_dir().join(format!("ndsp-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut o = EmbeddedOptions {
        data_dir: dir,
        name: format!("e2e-host-{tag}"),
        capture: (320, 240),
        max_fps: 30,
        ..Default::default()
    };
    opts(&mut o);
    EmbeddedHost::start(o).await.expect("host starts")
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_pin_pairing_works_when_enabled_and_is_refused_when_disabled() {
    // Default host: legacy path still accepted (Android/iOS viewers).
    let host = start_host("legacy-on").await;
    let pin = host.state.pins.current_pin();
    let session = connect(
        "127.0.0.1",
        host.port,
        client_info("Old Phone"),
        Auth::PinLegacy(&pin),
        vec![Codec::Jpeg],
    )
    .await
    .expect("legacy pairing accepted by default");
    assert!(session.new_credentials.is_some());
    session.close().await;
    host.shutdown().await;

    // Hardened host: legacy pairing is refused before any PIN processing.
    let host = start_host_opts("legacy-off", |o| o.legacy_pin_pairing = false).await;
    let pin = host.state.pins.current_pin();
    let err = must_fail(
        connect(
            "127.0.0.1",
            host.port,
            client_info("Old Phone"),
            Auth::PinLegacy(&pin),
            vec![Codec::Jpeg],
        )
        .await,
        "legacy pairing on hardened host",
    );
    assert!(
        format!("{err:#}").contains("PAKE"),
        "error should point at PAKE requirement: {err:#}"
    );
    // PAKE pairing still works on the hardened host, of course.
    let pin = host.state.pins.current_pin();
    let session = connect(
        "127.0.0.1",
        host.port,
        client_info("New Viewer"),
        Auth::Pin(&pin),
        vec![Codec::Jpeg],
    )
    .await
    .expect("PAKE pairing on hardened host");
    session.close().await;
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn clipboard_sync_respects_grant_and_caps() {
    let host = start_host("clipboard").await;
    let pin = host.state.pins.current_pin();
    let mut info = client_info("Clipboard Viewer");
    info.features = vec!["clipboard".into()];

    let mut session = connect(
        "127.0.0.1",
        host.port,
        info.clone(),
        Auth::Pin(&pin),
        vec![Codec::Jpeg],
    )
    .await
    .expect("pairing ok");
    assert!(
        !session.clipboard_allowed,
        "clipboard must be denied by default"
    );

    // 1. Without a grant, client pushes are dropped by the host.
    session
        .send(&ControlMsg::Clipboard {
            text: "stolen?".into(),
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        host.state.clipboard.get_text(),
        None,
        "ungranted clipboard push must not reach the host clipboard"
    );

    // 2. Grant clipboard from the panel → live ClipboardGrant notification.
    assert!(host
        .state
        .set_clipboard_grant(&info.device_id, true)
        .unwrap());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(deadline, session.recv())
            .await
            .expect("grant timeout")
            .unwrap()
        {
            Incoming::Control(ControlMsg::ClipboardGrant { allowed }) => {
                assert!(allowed);
                break;
            }
            Incoming::Closed => panic!("closed while waiting for clipboard grant"),
            _ => {}
        }
    }

    // 3. Client → host now lands on the host clipboard.
    session
        .send(&ControlMsg::Clipboard {
            text: "from viewer 📋".into(),
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if host.state.clipboard.get_text().as_deref() == Some("from viewer 📋") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "granted clipboard push never reached the host clipboard"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // 4. Host → client: a host-side copy is pushed to granted viewers…
    host.state.clipboard.set_text("from host");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(deadline, session.recv())
            .await
            .expect("host clipboard timeout")
            .unwrap()
        {
            Incoming::Control(ControlMsg::Clipboard { text }) => {
                assert_eq!(text, "from host");
                break;
            }
            Incoming::Closed => panic!("closed while waiting for host clipboard"),
            _ => {}
        }
    }

    // 5. …but an oversized client push is rejected by the size cap.
    let huge = "x".repeat(ndsp_protocol::MAX_CLIPBOARD_BYTES + 1);
    session
        .send(&ControlMsg::Clipboard { text: huge })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        host.state.clipboard.get_text().as_deref(),
        Some("from host"),
        "oversized clipboard push must be dropped"
    );

    session.close().await;
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn audio_streams_only_after_explicit_opt_in() {
    let host = start_host_opts("audio", |o| o.audio_test_tone = true).await;
    let pin = host.state.pins.current_pin();

    let mut session = connect(
        "127.0.0.1",
        host.port,
        client_info("Audio Viewer"),
        Auth::Pin(&pin),
        vec![Codec::Jpeg],
    )
    .await
    .expect("pairing ok");
    assert!(
        session.audio_available,
        "host with audio pipeline must advertise audio_available"
    );

    // Off by default: half a second of streaming must carry zero audio.
    let silent_until = tokio::time::Instant::now() + Duration::from_millis(500);
    loop {
        match tokio::time::timeout_at(silent_until, session.recv()).await {
            Ok(Ok(Incoming::Audio(_))) => panic!("audio arrived before opt-in"),
            Ok(Ok(Incoming::Closed)) => panic!("closed unexpectedly"),
            Ok(Ok(_)) => {}
            Ok(Err(e)) => panic!("recv: {e:#}"),
            Err(_) => break,
        }
    }

    // Opt in → 48 kHz stereo Opus packets with sane sequencing arrive.
    session
        .send(&ControlMsg::SetAudio { enabled: true })
        .await
        .unwrap();
    let mut audio_frames = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while audio_frames.len() < 10 {
        match tokio::time::timeout_at(deadline, session.recv())
            .await
            .expect("timed out waiting for audio")
            .expect("recv ok")
        {
            Incoming::Audio(f) => audio_frames.push(f),
            Incoming::Closed => panic!("closed while streaming audio"),
            _ => {}
        }
    }
    for f in &audio_frames {
        assert_eq!(f.codec, ndsp_protocol::media::AUDIO_CODEC_OPUS);
        assert_eq!(f.sample_rate, 48_000);
        assert_eq!(f.channels, 2);
        assert!(!f.payload.is_empty() && f.payload.len() < 1500);
    }
    for w in audio_frames.windows(2) {
        assert!(w[1].seq > w[0].seq, "audio seq must be increasing");
        assert!(w[1].timestamp_us >= w[0].timestamp_us);
    }

    // Opt back out → audio stops (allow in-flight packets to drain).
    session
        .send(&ControlMsg::SetAudio { enabled: false })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Drain whatever was in flight…
    while let Ok(Ok(_)) = tokio::time::timeout(Duration::from_millis(10), session.recv()).await {}
    // …then verify nothing new arrives.
    let quiet_until = tokio::time::Instant::now() + Duration::from_millis(500);
    loop {
        match tokio::time::timeout_at(quiet_until, session.recv()).await {
            Ok(Ok(Incoming::Audio(_))) => panic!("audio kept flowing after opt-out"),
            Ok(Ok(_)) => {}
            Ok(Err(e)) => panic!("recv: {e:#}"),
            Err(_) => break,
        }
    }

    session.close().await;
    host.shutdown().await;
}

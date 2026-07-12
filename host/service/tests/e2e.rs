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
    start_host_cfg(tag, Default::default()).await
}

async fn start_host_cfg(tag: &str, file: nebulad::config::FileConfig) -> EmbeddedHost {
    let dir = std::env::temp_dir().join(format!("ndsp-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    EmbeddedHost::start(EmbeddedOptions {
        data_dir: dir,
        name: format!("e2e-host-{tag}"),
        capture: (320, 240),
        max_fps: 30,
        file,
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

#[tokio::test(flavor = "multi_thread")]
async fn legacy_pre_pake_client_can_still_pair() {
    let host = start_host("legacy").await;
    let pin = host.state.pins.current_pin();
    let session = ndsp_client::connect_opts(
        "127.0.0.1",
        host.port,
        client_info("Old viewer"),
        Auth::Pin(&pin),
        vec![Codec::Jpeg],
        ndsp_client::ConnectOptions { use_pake: false },
    )
    .await
    .expect("legacy pairing must keep working by default");
    assert!(session.new_credentials.is_some());
    session.close().await;
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn require_pake_rejects_legacy_pairing() {
    let host = start_host_cfg(
        "pakeonly",
        nebulad::config::FileConfig {
            require_pake: true,
            ..Default::default()
        },
    )
    .await;
    let pin = host.state.pins.current_pin();

    // Legacy client is refused outright…
    let err = must_fail(
        ndsp_client::connect_opts(
            "127.0.0.1",
            host.port,
            client_info("Old viewer"),
            Auth::Pin(&pin),
            vec![Codec::Jpeg],
            ndsp_client::ConnectOptions { use_pake: false },
        )
        .await,
        "legacy pairing under require_pake",
    );
    assert!(
        format!("{err:#}").to_lowercase().contains("pake"),
        "error should mention PAKE: {err:#}"
    );

    // …while a PAKE client pairs fine.
    let session = connect(
        "127.0.0.1",
        host.port,
        client_info("New viewer"),
        Auth::Pin(&pin),
        vec![Codec::Jpeg],
    )
    .await
    .expect("PAKE pairing succeeds under require_pake");
    assert!(session.new_credentials.is_some());
    session.close().await;
    host.shutdown().await;
}

// ---------------------------------------------------------------------------
// Clipboard sync (roadmap P2.9)
// ---------------------------------------------------------------------------

fn client_info_with(name: &str, features: &[&str]) -> ndsp_protocol::messages::ClientInfo {
    let mut ci = ndsp_client::default_client_info(name, "test");
    ci.features = features.iter().map(|s| s.to_string()).collect();
    ci
}

/// Drain the session until `pred` matches a control message or the timeout
/// elapses. Returns the matching message.
async fn wait_for_control(
    session: &mut ndsp_client::Session,
    timeout: Duration,
    mut pred: impl FnMut(&ControlMsg) -> bool,
) -> Option<ControlMsg> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match tokio::time::timeout_at(deadline, session.recv()).await {
            Ok(Ok(Incoming::Control(msg))) if pred(&msg) => return Some(msg),
            Ok(Ok(Incoming::Closed)) => return None,
            Ok(Ok(_)) => continue,
            Ok(Err(_)) | Err(_) => return None,
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn clipboard_sync_requires_grant_and_flows_both_ways() {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    let host = start_host("clipboard").await;
    let pin = host.state.pins.current_pin();
    let ci = client_info_with("Clip viewer", &["clipboard"]);
    let device_id = ci.device_id.clone();
    let mut session = connect(
        "127.0.0.1",
        host.port,
        ci,
        Auth::Pin(&pin),
        vec![Codec::Jpeg],
    )
    .await
    .expect("pairing succeeds");
    assert!(!session.clipboard_allowed, "clipboard denied by default");

    // Without a grant, a host copy must NOT reach the viewer…
    host.state
        .clipboard
        .set_text("secret-before-grant")
        .unwrap();
    let leaked = wait_for_control(&mut session, Duration::from_millis(1200), |m| {
        matches!(m, ControlMsg::ClipboardData { .. })
    })
    .await;
    assert!(leaked.is_none(), "clipboard leaked without grant");

    // …and a viewer paste must not land on the host.
    session
        .send(&ControlMsg::ClipboardData {
            format: "text".into(),
            data: B64.encode("evil-before-grant"),
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        host.state.clipboard.get_text().as_deref(),
        Some("secret-before-grant"),
        "ungranted viewer must not write the host clipboard"
    );

    // Grant clipboard → viewer is notified.
    assert!(host.state.set_clipboard_grant(&device_id, true).unwrap());
    let granted = wait_for_control(&mut session, Duration::from_secs(5), |m| {
        matches!(m, ControlMsg::ClipboardGrant { allowed: true })
    })
    .await;
    assert!(granted.is_some(), "expected clipboard_grant message");

    // Host copy → viewer receives it.
    host.state.clipboard.set_text("hello from host").unwrap();
    let data = wait_for_control(&mut session, Duration::from_secs(5), |m| {
        matches!(m, ControlMsg::ClipboardData { .. })
    })
    .await
    .expect("host clipboard must reach granted viewer");
    let ControlMsg::ClipboardData { format, data } = data else {
        unreachable!()
    };
    assert_eq!(format, "text");
    assert_eq!(B64.decode(data).unwrap(), b"hello from host");

    // Viewer copy → host clipboard updated.
    session
        .send(&ControlMsg::ClipboardData {
            format: "text".into(),
            data: B64.encode("hello from viewer"),
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if host.state.clipboard.get_text().as_deref() == Some("hello from viewer") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "viewer clipboard never reached the host"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Size cap enforced (default 256 KiB).
    session
        .send(&ControlMsg::ClipboardData {
            format: "text".into(),
            data: B64.encode(vec![b'x'; 300 * 1024]),
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        host.state.clipboard.get_text().as_deref(),
        Some("hello from viewer"),
        "oversized clipboard payload must be dropped"
    );

    session.close().await;
    host.shutdown().await;
}

// ---------------------------------------------------------------------------
// File drop (roadmap P2.10)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn file_drop_requires_explicit_accept_and_verifies_hash() {
    use sha2::{Digest, Sha256};
    let host = start_host("filedrop").await;
    let pin = host.state.pins.current_pin();
    let ci = client_info_with("Drop viewer", &["file_drop"]);
    let mut session = connect(
        "127.0.0.1",
        host.port,
        ci,
        Auth::Pin(&pin),
        vec![Codec::Jpeg],
    )
    .await
    .expect("pairing succeeds");

    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let sha = hex::encode(Sha256::digest(&payload));

    // Chunks before acceptance must be ignored (no partial file appears).
    session
        .send_file_chunk(&ndsp_protocol::media::FileChunk {
            transfer_id: 9,
            offset: 0,
            data: vec![1, 2, 3],
        })
        .await
        .unwrap();

    session
        .send(&ControlMsg::FileOffer {
            transfer_id: 1,
            name: "../evil/../e2e-drop.bin".into(), // sanitization exercised
            size: payload.len() as u64,
            sha256: sha.clone(),
        })
        .await
        .unwrap();

    // Offer shows up for the panel.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let (client_id, transfer_id) = loop {
        {
            let pending = host.state.pending_files.lock().unwrap();
            if let Some(p) = pending.first() {
                assert_eq!(p.file_name, "e2e-drop.bin", "name must be sanitized");
                assert_eq!(p.size, payload.len() as u64);
                break (p.client_id, p.transfer_id);
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "offer never reached the panel queue"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    // Host user accepts in the panel.
    assert!(host.state.decide_file(client_id, transfer_id, true));
    let accepted = wait_for_control(&mut session, Duration::from_secs(5), |m| {
        matches!(m, ControlMsg::FileAccept { transfer_id: 1 })
    })
    .await;
    assert!(accepted.is_some(), "expected file_accept");

    // Stream the chunks.
    let mut offset = 0u64;
    for part in payload.chunks(16_384) {
        session
            .send_file_chunk(&ndsp_protocol::media::FileChunk {
                transfer_id: 1,
                offset,
                data: part.to_vec(),
            })
            .await
            .unwrap();
        offset += part.len() as u64;
    }
    let done = wait_for_control(&mut session, Duration::from_secs(10), |m| {
        matches!(m, ControlMsg::FileDone { transfer_id: 1, .. })
    })
    .await
    .expect("expected file_done");
    let ControlMsg::FileDone { ok, error, .. } = done else {
        unreachable!()
    };
    assert!(ok, "transfer must succeed: {error:?}");

    // File landed, hash-verified, under the sanitized name.
    let dir = host.state.cfg.data_dir.join("downloads");
    let written = std::fs::read(dir.join("e2e-drop.bin")).expect("file must exist");
    assert_eq!(written, payload);

    // A declined offer is rejected.
    session
        .send(&ControlMsg::FileOffer {
            transfer_id: 2,
            name: "nope.txt".into(),
            size: 10,
            sha256: sha,
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let found = host
            .state
            .pending_files
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.transfer_id == 2);
        if found {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(host.state.decide_file(client_id, 2, false));
    let rejected = wait_for_control(&mut session, Duration::from_secs(5), |m| {
        matches!(m, ControlMsg::FileReject { transfer_id: 2, .. })
    })
    .await;
    assert!(rejected.is_some(), "expected file_reject");

    session.close().await;
    host.shutdown().await;
}

// ---------------------------------------------------------------------------
// Audio (roadmap P2.8)
// ---------------------------------------------------------------------------

#[cfg(feature = "audio")]
#[tokio::test(flavor = "multi_thread")]
async fn audio_disabled_by_default_and_streams_opus_when_enabled() {
    // Default host: audio off → explicit error.
    let host = start_host("audio-off").await;
    let pin = host.state.pins.current_pin();
    let mut session = connect(
        "127.0.0.1",
        host.port,
        client_info_with("Audio viewer", &["audio"]),
        Auth::Pin(&pin),
        vec![Codec::Jpeg],
    )
    .await
    .expect("pairing succeeds");
    assert!(!session.audio_available);
    session
        .send(&ControlMsg::SetAudio { enabled: true })
        .await
        .unwrap();
    let err = wait_for_control(
        &mut session,
        Duration::from_secs(5),
        |m| matches!(m, ControlMsg::Error { code, .. } if code == "audio_disabled"),
    )
    .await;
    assert!(err.is_some(), "expected audio_disabled error");
    session.close().await;
    host.shutdown().await;

    // Audio-enabled host: opt in → AudioStart + decodable Opus frames.
    let host = start_host_cfg(
        "audio-on",
        nebulad::config::FileConfig {
            audio: true,
            ..Default::default()
        },
    )
    .await;
    let pin = host.state.pins.current_pin();
    let mut session = connect(
        "127.0.0.1",
        host.port,
        client_info_with("Audio viewer", &["audio"]),
        Auth::Pin(&pin),
        vec![Codec::Jpeg],
    )
    .await
    .expect("pairing succeeds");
    assert!(session.audio_available);
    session
        .send(&ControlMsg::SetAudio { enabled: true })
        .await
        .unwrap();

    let started = wait_for_control(&mut session, Duration::from_secs(5), |m| {
        matches!(
            m,
            ControlMsg::AudioStart {
                codec: ndsp_protocol::messages::AudioCodec::Opus,
                sample_rate: 48000,
                channels: 2,
            }
        )
    })
    .await;
    assert!(started.is_some(), "expected audio_start");

    // Collect audio frames and decode them with a real Opus decoder.
    let mut frames = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while frames.len() < 10 {
        match tokio::time::timeout_at(deadline, session.recv()).await {
            Ok(Ok(Incoming::Audio(f))) => frames.push(f),
            Ok(Ok(Incoming::Closed)) => panic!("closed while waiting for audio"),
            Ok(Ok(_)) => {}
            Ok(Err(e)) => panic!("recv: {e:#}"),
            Err(_) => panic!("timed out waiting for audio frames (got {})", frames.len()),
        }
    }
    for w in frames.windows(2) {
        assert_eq!(w[1].seq, w[0].seq + 1, "audio seq must be contiguous");
    }
    let mut dec = opus::Decoder::new(48000, opus::Channels::Stereo).unwrap();
    let mut pcm = vec![0f32; 1920];
    let n = dec
        .decode_float(&frames[0].payload, &mut pcm, false)
        .unwrap();
    assert_eq!(n, 960, "20 ms @ 48 kHz");
    assert!(
        pcm.iter().any(|s| s.abs() > 0.01),
        "test tone must be audible"
    );

    // Panel indicator on.
    assert!(host
        .state
        .clients
        .lock()
        .unwrap()
        .values()
        .any(|c| c.audio_on.load(std::sync::atomic::Ordering::Relaxed)));

    // Opt out → AudioStop and the stream ceases.
    session
        .send(&ControlMsg::SetAudio { enabled: false })
        .await
        .unwrap();
    let stopped = wait_for_control(&mut session, Duration::from_secs(5), |m| {
        matches!(m, ControlMsg::AudioStop)
    })
    .await;
    assert!(stopped.is_some(), "expected audio_stop");
    // Drain in-flight frames, then verify silence.
    tokio::time::sleep(Duration::from_millis(300)).await;
    while let Ok(Ok(item)) = tokio::time::timeout(Duration::from_millis(10), session.recv()).await {
        if matches!(item, Incoming::Closed) {
            break;
        }
    }
    let extra = tokio::time::timeout(Duration::from_millis(600), async {
        loop {
            if let Incoming::Audio(_) = session.recv().await.unwrap() {
                return;
            }
        }
    })
    .await;
    assert!(
        extra.is_err(),
        "audio frames must stop after SetAudio(false)"
    );

    session.close().await;
    host.shutdown().await;
}

// ---------------------------------------------------------------------------
// Optional HTTPS (roadmap P1.7)
// ---------------------------------------------------------------------------

#[cfg(feature = "tls")]
#[tokio::test(flavor = "multi_thread")]
async fn https_serves_viewer_with_persistent_self_signed_cert() {
    use std::sync::Arc as StdArc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = std::env::temp_dir().join(format!("ndsp-e2e-https-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Cert material persists across "restarts".
    let m1 = nebulad::tls::load_or_create(&dir).unwrap();
    let m2 = nebulad::tls::load_or_create(&dir).unwrap();
    assert_eq!(m1.fingerprint, m2.fingerprint);

    // Boot a TLS host on a free port.
    let port = {
        let sock = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        sock.local_addr().unwrap().port()
    };
    let cfg = nebulad::config::Config {
        name: "https-host".into(),
        data_dir: dir.clone(),
        web_dir: None,
        file: nebulad::config::FileConfig {
            https: true,
            ..Default::default()
        },
    };
    let state = StdArc::new(nebulad::state::AppState::new(cfg).await.unwrap());
    let srv_state = state.clone();
    tokio::spawn(async move {
        let _ = nebulad::server::run(srv_state, "127.0.0.1".parse().unwrap(), port).await;
    });

    // TLS client that pins nothing (test) but records the presented cert.
    #[derive(Debug)]
    struct NoVerify;
    impl rustls::client::danger::ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    let tls_cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(StdArc::new(NoVerify))
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(StdArc::new(tls_cfg));

    // Retry until the server is up.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let body = loop {
        let attempt = async {
            let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
            let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
            let mut tls = connector.connect(name, tcp).await?;
            tls.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .await?;
            let mut buf = Vec::new();
            tls.read_to_end(&mut buf).await?;
            anyhow::Ok(String::from_utf8_lossy(&buf).to_string())
        };
        match attempt.await {
            Ok(b) => break b,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => panic!("HTTPS endpoint never came up: {e:#}"),
        }
    };
    assert!(body.starts_with("HTTP/1.1 200"), "{body}");
    assert!(body.ends_with("ok"), "{body}");

    state.trigger_shutdown();
}

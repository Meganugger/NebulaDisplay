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
    start_host_with(tag, Default::default()).await
}

async fn start_host_with(tag: &str, file: nebulad::config::FileConfig) -> EmbeddedHost {
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
            Incoming::Audio(_) => {}
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
                Ok(Ok(Incoming::Audio(_))) => {}
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

// ---------------------------------------------------------------------------
// v0.5 features: SPAKE2 pairing edges, audio, clipboard, file transfers
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn legacy_pin_pairing_still_supported_and_can_be_disabled() {
    // Default host accepts the legacy scheme (mobile viewers).
    let host = start_host("legacy").await;
    let pin = host.state.pins.current_pin();
    let session = connect(
        "127.0.0.1",
        host.port,
        client_info("Old Phone"),
        Auth::PinLegacy(&pin),
        vec![Codec::Jpeg],
    )
    .await
    .expect("legacy pairing works by default");
    assert!(session.new_credentials.is_some());
    session.close().await;
    host.shutdown().await;

    // A host with allow_legacy_pairing = false rejects it with a clear error.
    let host = start_host_with(
        "legacy-off",
        nebulad::config::FileConfig {
            allow_legacy_pairing: false,
            ..Default::default()
        },
    )
    .await;
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
        "legacy pairing on a SPAKE2-only host",
    );
    assert!(
        format!("{err:#}").contains("SPAKE2"),
        "error should point at the fix: {err:#}"
    );
    // ...while SPAKE2 pairing on the same host succeeds.
    let pin = host.state.pins.current_pin();
    connect(
        "127.0.0.1",
        host.port,
        client_info("New Tablet"),
        Auth::Pin(&pin),
        vec![Codec::Jpeg],
    )
    .await
    .expect("SPAKE2 pairing works with legacy disabled")
    .close()
    .await;
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn spake2_wrong_pin_rejected_and_rotates() {
    let host = start_host("spake2-wrong").await;
    let real = host.state.pins.current_pin();
    let wrong = if real == "000000" { "000001" } else { "000000" };
    let err = must_fail(
        connect(
            "127.0.0.1",
            host.port,
            client_info("Guesser"),
            Auth::Pin(wrong),
            vec![Codec::Jpeg],
        )
        .await,
        "SPAKE2 with wrong PIN",
    );
    assert!(
        format!("{err:#}").to_lowercase().contains("pin"),
        "error should mention the PIN: {err:#}"
    );
    assert_ne!(
        host.state.pins.current_pin(),
        real,
        "failed SPAKE2 attempt must rotate the PIN"
    );
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn audio_opt_in_streams_opus_and_panel_mute_stops_it() {
    use ndsp_protocol::media::AudioCodec;
    use ndsp_protocol::messages::AudioWireCodec;

    let host = start_host("audio").await;
    let pin = host.state.pins.current_pin();
    let info = client_info("Listener");
    let mut session = connect(
        "127.0.0.1",
        host.port,
        info.clone(),
        Auth::Pin(&pin),
        vec![Codec::Jpeg],
    )
    .await
    .expect("pairing ok");

    // No audio arrives before opting in (off by default).
    let deadline = tokio::time::Instant::now() + Duration::from_millis(700);
    loop {
        match tokio::time::timeout_at(deadline, session.recv()).await {
            Ok(Ok(Incoming::Audio(_))) => panic!("audio must be off by default"),
            Ok(Ok(_)) => {}
            Ok(Err(e)) => panic!("recv: {e:#}"),
            Err(_) => break,
        }
    }
    assert_eq!(host.state.audio_listener_count(), 0);

    // Opt in → Opus packets in the pipeline's fixed format.
    session
        .send(&ControlMsg::SetAudio {
            enabled: true,
            codec: Some(AudioWireCodec::Opus),
        })
        .await
        .unwrap();
    let mut opus_frames = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while opus_frames.len() < 20 {
        match tokio::time::timeout_at(deadline, session.recv())
            .await
            .expect("timed out waiting for audio")
            .expect("recv ok")
        {
            Incoming::Audio(a) => opus_frames.push(a),
            Incoming::Closed => panic!("closed while streaming audio"),
            _ => {}
        }
    }
    for a in &opus_frames {
        assert_eq!(a.codec, AudioCodec::Opus);
        assert_eq!(a.sample_rate, 48_000);
        assert_eq!(a.channels, 2);
        assert!(!a.payload.is_empty() && a.payload.len() < 1500);
        assert!(a.timestamp_us > 0);
    }
    assert!(
        opus_frames.windows(2).all(|w| w[1].seq > w[0].seq),
        "audio seq must increase"
    );
    assert_eq!(host.state.audio_listener_count(), 1);

    // Panel mutes the device → stream stops + client is notified.
    assert!(host.state.set_audio_grant(&info.device_id, false).unwrap());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut got_grant_msg = false;
    loop {
        match tokio::time::timeout_at(deadline, session.recv()).await {
            Ok(Ok(Incoming::Control(ControlMsg::AudioGrant { allowed }))) => {
                assert!(!allowed);
                got_grant_msg = true;
                break;
            }
            Ok(Ok(_)) => {}
            _ => break,
        }
    }
    assert!(got_grant_msg, "client must be told about the mute");
    // Drain the writer queue, then confirm silence.
    tokio::time::sleep(Duration::from_millis(300)).await;
    while let Ok(Ok(i)) = tokio::time::timeout(Duration::from_millis(10), session.recv()).await {
        drop(i);
    }
    let deadline = tokio::time::Instant::now() + Duration::from_millis(600);
    loop {
        match tokio::time::timeout_at(deadline, session.recv()).await {
            Ok(Ok(Incoming::Audio(_))) => panic!("audio must stop after panel mute"),
            Ok(Ok(_)) => {}
            _ => break,
        }
    }
    assert_eq!(host.state.audio_listener_count(), 0);
    session.close().await;
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn audio_pcm_codec_variant_streams_raw_samples() {
    use ndsp_protocol::media::AudioCodec;
    use ndsp_protocol::messages::AudioWireCodec;

    let host = start_host("audio-pcm").await;
    let pin = host.state.pins.current_pin();
    let mut session = connect(
        "127.0.0.1",
        host.port,
        client_info("Insecure-Origin Browser"),
        Auth::Pin(&pin),
        vec![Codec::Jpeg],
    )
    .await
    .unwrap();
    session
        .send(&ControlMsg::SetAudio {
            enabled: true,
            codec: Some(AudioWireCodec::Pcm),
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(deadline, session.recv())
            .await
            .expect("timed out waiting for pcm audio")
            .unwrap()
        {
            Incoming::Audio(a) => {
                assert_eq!(a.codec, AudioCodec::PcmS16le);
                // 10 ms of 48 kHz stereo s16 = 480 * 2 * 2 bytes.
                assert_eq!(a.payload.len(), 480 * 2 * 2);
                // The test tone must be non-silent.
                assert!(a.payload.iter().any(|&b| b != 0));
                break;
            }
            Incoming::Closed => panic!("closed"),
            _ => {}
        }
    }
    session.close().await;
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn clipboard_requires_grant_and_syncs_both_ways() {
    let host = start_host("clipboard").await;
    let pin = host.state.pins.current_pin();
    let info = client_info("Clipboard Tablet");
    let mut session = connect(
        "127.0.0.1",
        host.port,
        info.clone(),
        Auth::Pin(&pin),
        vec![Codec::Jpeg],
    )
    .await
    .unwrap();

    // Without a grant, viewer→host clipboard is dropped.
    session
        .send(&ControlMsg::Clipboard {
            text: "should be dropped".into(),
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        host.state.clipboard.get_text(),
        None,
        "clipboard must be deny-by-default"
    );

    // Grant from the panel → client notified, then sync works both ways.
    assert!(host
        .state
        .set_clipboard_grant(&info.device_id, true)
        .unwrap());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match tokio::time::timeout_at(deadline, session.recv())
            .await
            .expect("grant notify timeout")
            .unwrap()
        {
            Incoming::Control(ControlMsg::ClipboardGrant { allowed }) => {
                assert!(allowed);
                break;
            }
            Incoming::Closed => panic!("closed"),
            _ => {}
        }
    }

    // viewer → host
    session
        .send(&ControlMsg::Clipboard {
            text: "hello from the tablet 📋".into(),
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if host.state.clipboard.get_text().as_deref() == Some("hello from the tablet 📋") {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("viewer clipboard never reached the host");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // host → viewer (watcher polls the backend and must skip the echo of
    // what the viewer just set, then pick up this genuine host-side copy).
    host.state.clipboard.set_text("host copied this").unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(deadline, session.recv())
            .await
            .expect("host clipboard never arrived")
            .unwrap()
        {
            Incoming::Control(ControlMsg::Clipboard { text }) => {
                assert_eq!(text, "host copied this");
                break;
            }
            Incoming::Closed => panic!("closed"),
            _ => {}
        }
    }
    session.close().await;
    host.shutdown().await;
}

async fn pump_transfer(session: &mut ndsp_client::Session, content: &[u8], id: &str) -> ControlMsg {
    use sha2::Digest as _;
    let sha = hex::encode(sha2::Sha256::digest(content));
    session
        .send(&ControlMsg::FileOffer {
            id: id.into(),
            name: "notes/../secret/../../report.txt".into(), // sanitizer bait
            size_bytes: content.len() as u64,
            sha256: sha,
        })
        .await
        .unwrap();
    // Wait for the host's answer (panel decision happens host-side).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let accepted = loop {
        match tokio::time::timeout_at(deadline, session.recv())
            .await
            .expect("no answer to file offer")
            .unwrap()
        {
            Incoming::Control(ControlMsg::FileAnswer {
                id: rid, accept, ..
            }) if rid == id => break accept,
            Incoming::Closed => panic!("closed"),
            _ => {}
        }
    };
    if !accepted {
        return ControlMsg::FileAnswer {
            id: id.into(),
            accept: false,
            reason: None,
        };
    }
    use base64::Engine as _;
    for (seq, chunk) in content.chunks(64 * 1024).enumerate() {
        session
            .send(&ControlMsg::FileChunk {
                id: id.into(),
                seq: seq as u32,
                data: base64::engine::general_purpose::STANDARD.encode(chunk),
            })
            .await
            .unwrap();
    }
    session
        .send(&ControlMsg::FileEnd { id: id.into() })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(deadline, session.recv())
            .await
            .expect("no completion for file transfer")
            .unwrap()
        {
            Incoming::Control(m @ ControlMsg::FileDone { .. })
            | Incoming::Control(m @ ControlMsg::FileAbort { .. }) => return m,
            Incoming::Closed => panic!("closed"),
            _ => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn file_drop_needs_panel_accept_and_verifies_hash() {
    let host = start_host("filedrop").await;
    let pin = host.state.pins.current_pin();
    let mut session = connect(
        "127.0.0.1",
        host.port,
        client_info("Dropper"),
        Auth::Pin(&pin),
        vec![Codec::Jpeg],
    )
    .await
    .unwrap();

    let content: Vec<u8> = (0..200_000u32).map(|i| (i * 7) as u8).collect();

    // Deny path: the panel says no → nothing is written.
    let denier = tokio::spawn({
        let state = host.state.clone();
        async move {
            for _ in 0..100 {
                if state.transfers.answer("deny-me-1", false) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!("offer never appeared in the transfer manager");
        }
    });
    let answer = pump_transfer(&mut session, &content, "deny-me-1").await;
    denier.await.unwrap();
    assert!(
        matches!(answer, ControlMsg::FileAnswer { accept: false, .. }),
        "denied transfer must not proceed"
    );
    let dir = host.state.cfg.file_transfer_dir();
    assert!(
        !dir.exists() || std::fs::read_dir(&dir).unwrap().next().is_none(),
        "nothing may be written for a denied transfer"
    );

    // Accept path: file lands with sanitized name and exact content.
    let accepter = tokio::spawn({
        let state = host.state.clone();
        async move {
            for _ in 0..100 {
                // The panel list must show the *sanitized* name.
                if let Some(o) = state.transfers.list().first() {
                    assert_eq!(o.name, "report.txt");
                    assert_eq!(o.size_bytes, 200_000);
                    assert!(state.transfers.answer(&o.id.clone(), true));
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!("offer never appeared");
        }
    });
    let answer = pump_transfer(&mut session, &content, "accept-me-1").await;
    accepter.await.unwrap();
    assert!(
        matches!(answer, ControlMsg::FileDone { .. }),
        "accepted transfer must complete: {answer:?}"
    );
    let received = std::fs::read(dir.join("report.txt")).expect("file must exist");
    assert_eq!(received, content, "content must survive bit-exact");
    // No stray .part files.
    assert!(std::fs::read_dir(&dir).unwrap().all(|e| !e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .ends_with(".part")));

    session.close().await;
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn corrupted_file_transfer_is_rejected_and_cleaned_up() {
    use base64::Engine as _;
    let host = start_host("filedrop-bad").await;
    let pin = host.state.pins.current_pin();
    let mut session = connect(
        "127.0.0.1",
        host.port,
        client_info("Corruptor"),
        Auth::Pin(&pin),
        vec![Codec::Jpeg],
    )
    .await
    .unwrap();

    // Offer with a hash that will NOT match the data we send.
    session
        .send(&ControlMsg::FileOffer {
            id: "bad-hash-1".into(),
            name: "evil.bin".into(),
            size_bytes: 4,
            sha256: "00".repeat(32),
        })
        .await
        .unwrap();
    let accepter = {
        let state = host.state.clone();
        async move {
            for _ in 0..100 {
                if state.transfers.answer("bad-hash-1", true) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!("offer never appeared");
        }
    };
    accepter.await;
    // Consume the accept, send mismatching data, expect FileAbort.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Incoming::Control(ControlMsg::FileAnswer { accept, .. }) =
            tokio::time::timeout_at(deadline, session.recv())
                .await
                .expect("no file answer")
                .unwrap()
        {
            assert!(accept);
            break;
        }
    }
    session
        .send(&ControlMsg::FileChunk {
            id: "bad-hash-1".into(),
            seq: 0,
            data: base64::engine::general_purpose::STANDARD.encode(b"data"),
        })
        .await
        .unwrap();
    session
        .send(&ControlMsg::FileEnd {
            id: "bad-hash-1".into(),
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(deadline, session.recv())
            .await
            .expect("no abort for corrupted transfer")
            .unwrap()
        {
            Incoming::Control(ControlMsg::FileAbort { reason, .. }) => {
                assert!(reason.contains("sha256"), "reason: {reason}");
                break;
            }
            Incoming::Closed => panic!("closed"),
            _ => {}
        }
    }
    let dir = host.state.cfg.file_transfer_dir();
    assert!(
        !dir.exists() || std::fs::read_dir(&dir).unwrap().next().is_none(),
        "corrupted transfer must leave no files behind"
    );
    session.close().await;
    host.shutdown().await;
}

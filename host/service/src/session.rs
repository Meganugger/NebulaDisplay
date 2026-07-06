//! Post-authentication client session.
//!
//! # Latency architecture
//!
//! Input must never wait behind video, so the session is split into four
//! independent tasks connected only by lock-free/latest-only channels:
//!
//! ```text
//!  ws_rx ─► pump ───────► input_sink.apply()          (immediate, no queue)
//!               └───────► pong / controller updates   (immediate)
//!  capture watch ─► video task: pace → encode ─► video slot (latest-only)
//!  control mpsc ──► writer task (control preempts video) ─► ws_tx
//!  supervisor: grants / kick / liveness / host stats
//! ```
//!
//! * The **pump** decrypts and *applies input synchronously* the moment an
//!   envelope arrives — it never touches the encoder or the socket sink.
//!   Pings are answered from here too, so RTT measurements are clean and
//!   the adaptive controller isn't fooled by encode/send time.
//! * The **video task** paces on a monotonic interval that is *never reset
//!   by unrelated events* (the v0.2 loop re-armed its sleep on every inbound
//!   message, which starved video entirely under continuous touch input —
//!   the root cause of the "seconds of latency while dragging" bug).
//! * The **writer** owns the socket sink and the [`Sealer`]. Control
//!   messages preempt video; video frames flow through a latest-only slot,
//!   so a slow socket drops stale frames instead of queueing them.

use axum::extract::ws::{Message, WebSocket};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use ndsp_protocol::{
    envelope::{Channel, Direction, Opener, Sealer},
    media::VideoFrame,
    messages::{ControlMsg, InputMode, Profile},
};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use crate::adapt::AdaptiveController;
use crate::input::InputSink;
use crate::pairing::AuthComplete;
use crate::state::{AppState, ClientHandle, SessionCommand};
use crate::util::now_us;

/// Liveness: drop the session if nothing (not even pings) arrives for this long.
const RECV_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the server pushes HostStats to the viewer overlay.
const HOST_STATS_INTERVAL: Duration = Duration::from_secs(2);

/// State shared between the session's tasks (atomics only — the single
/// mutex-guarded item, the adaptive controller, is touched briefly and
/// never across an await point).
struct Shared {
    input_allowed: Arc<AtomicBool>,
    /// `InputMode` as u8 (see [`mode_to_u8`]).
    input_mode: AtomicU32,
    force_keyframe: AtomicBool,
    ctl: Mutex<AdaptiveController>,
    /// ms since session epoch of the last inbound message (liveness).
    last_recv_ms: AtomicU64,
    epoch: Instant,
    // -- perf counters flushed into HostStats by the supervisor --
    bytes_sent: AtomicU64,
    frames_sent: AtomicU64,
    frames_skipped: AtomicU64,
    /// EMA of encode time in µs.
    encode_us_avg: AtomicU64,
}

impl Shared {
    fn touch_recv(&self) {
        self.last_recv_ms
            .store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
    }
    fn recv_age(&self) -> Duration {
        let last = self.last_recv_ms.load(Ordering::Relaxed);
        self.epoch
            .elapsed()
            .saturating_sub(Duration::from_millis(last))
    }
}

fn mode_to_u8(m: InputMode) -> u32 {
    match m {
        InputMode::ViewOnly => 0,
        InputMode::Touchpad => 1,
        InputMode::DirectTouch => 2,
        InputMode::KeyboardMouse => 3,
        InputMode::DrawingTablet => 4,
    }
}

pub async fn run(
    state: Arc<AppState>,
    socket: WebSocket,
    auth: AuthComplete,
    addr: SocketAddr,
    input_sink: Arc<dyn InputSink>,
) {
    let (ws_tx, ws_rx) = socket.split();
    let session_key = auth.session_key;
    let sealer = Sealer::new(&session_key, Direction::ServerToClient);
    let opener = Opener::new(&session_key, Direction::ClientToServer);

    let encoder = match crate::encode::create(auth.codec) {
        Ok(e) => e,
        Err(e) => {
            warn!("encoder init failed: {e:#}");
            return;
        }
    };

    let input_allowed = Arc::new(AtomicBool::new(auth.input_allowed));
    let shared = Arc::new(Shared {
        input_allowed: input_allowed.clone(),
        input_mode: AtomicU32::new(mode_to_u8(InputMode::ViewOnly)),
        force_keyframe: AtomicBool::new(true),
        ctl: Mutex::new(AdaptiveController::new(Profile::Office)),
        last_recv_ms: AtomicU64::new(0),
        epoch: Instant::now(),
        bytes_sent: AtomicU64::new(0),
        frames_sent: AtomicU64::new(0),
        frames_skipped: AtomicU64::new(0),
        encode_us_avg: AtomicU64::new(0),
    });

    let (cmd_tx, mut cmd_rx) = mpsc::channel::<SessionCommand>(8);
    let handle = Arc::new(ClientHandle {
        device_id: auth.client.device_id.clone(),
        name: auth.client.name.clone(),
        platform: auth.client.platform.clone(),
        addr,
        connected_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        input_allowed: input_allowed.clone(),
        stats: Mutex::new(Default::default()),
        commands: cmd_tx,
    });
    let client_id = state.register_client(handle.clone());
    info!(client_id, device = %auth.client.device_id, name = %auth.client.name, %addr, codec = ?auth.codec, newly_paired = auth.newly_paired, "session started");

    // Control messages to send (pongs, grants, stats, bye). Small + rare;
    // the writer drains these before video.
    let (ctl_tx, ctl_rx) = mpsc::channel::<ControlMsg>(32);
    // Latest-only encoded video slot: a stale frame is overwritten, never
    // queued. (seq, frame) — seq lets the video task detect unconsumed slots.
    let (vid_tx, vid_rx) = watch::channel::<Option<(u64, Arc<VideoFrame>)>>(None);
    let consumed_seq = Arc::new(AtomicU64::new(0));

    let writer = tokio::spawn(writer_task(
        ws_tx,
        sealer,
        ctl_rx,
        vid_rx,
        shared.clone(),
        consumed_seq.clone(),
    ));
    let pump = tokio::spawn(incoming_pump(
        ws_rx,
        opener,
        shared.clone(),
        ctl_tx.clone(),
        input_sink,
        handle.clone(),
    ));
    let video = tokio::spawn(video_task(
        state.clone(),
        encoder,
        shared.clone(),
        vid_tx,
        consumed_seq,
    ));

    // ---- supervisor ----
    let mut host_stats_timer = tokio::time::interval(HOST_STATS_INTERVAL);
    host_stats_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut pump = pump;
    let mut bytes_window_start = Instant::now();
    loop {
        tokio::select! {
            Some(cmd) = cmd_rx.recv() => match cmd {
                SessionCommand::SetInputGrant(allowed) => {
                    input_allowed.store(allowed, Ordering::Relaxed);
                    if ctl_tx.send(ControlMsg::InputGrant { allowed }).await.is_err() { break; }
                }
                SessionCommand::Kick { reason } => {
                    let _ = ctl_tx.send(ControlMsg::Bye { reason }).await;
                    // Give the writer a moment to flush the Bye.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    break;
                }
            },

            _ = &mut pump => break, // client went away / protocol violation

            _ = host_stats_timer.tick() => {
                if shared.recv_age() > RECV_TIMEOUT {
                    warn!(client_id, "client unresponsive; closing");
                    break;
                }
                let stats = {
                    let mut hs = state.host_stats.lock().unwrap();
                    {
                        let ctl = shared.ctl.lock().unwrap();
                        hs.target_bitrate_kbps = ctl.bitrate_kbps();
                    }
                    hs.encode_ms_avg =
                        shared.encode_us_avg.load(Ordering::Relaxed) as f32 / 1000.0;
                    hs.frames_sent = shared.frames_sent.load(Ordering::Relaxed);
                    hs.frames_skipped = shared.frames_skipped.load(Ordering::Relaxed);
                    let elapsed = bytes_window_start.elapsed().as_secs_f64();
                    if elapsed > 0.5 {
                        let bytes = shared.bytes_sent.swap(0, Ordering::Relaxed);
                        hs.actual_bitrate_kbps = ((bytes as f64 * 8.0 / 1000.0) / elapsed) as u32;
                        bytes_window_start = Instant::now();
                    }
                    hs.clone()
                };
                if ctl_tx.send(ControlMsg::HostStats { stats }).await.is_err() { break; }
            }
        }
    }

    state.unregister_client(client_id);
    pump.abort();
    video.abort();
    drop(ctl_tx); // writer exits once both inputs are gone
    writer.abort();
    info!(client_id, "session ended");
}

/// Owns the socket sink. Control preempts video; video is latest-only.
async fn writer_task(
    mut ws_tx: SplitSink<WebSocket, Message>,
    mut sealer: Sealer,
    mut ctl_rx: mpsc::Receiver<ControlMsg>,
    mut vid_rx: watch::Receiver<Option<(u64, Arc<VideoFrame>)>>,
    shared: Arc<Shared>,
    consumed_seq: Arc<AtomicU64>,
) {
    loop {
        tokio::select! {
            biased;

            msg = ctl_rx.recv() => {
                let Some(msg) = msg else { break };
                let json = msg.to_json().expect("control message serialization");
                let envelope = sealer.seal(Channel::Control, json.as_bytes());
                if ws_tx.send(Message::Binary(envelope.into())).await.is_err() { break; }
            }

            changed = vid_rx.changed() => {
                if changed.is_err() { break; }
                let frame = {
                    let borrowed = vid_rx.borrow_and_update();
                    borrowed.clone()
                };
                let Some((seq, vf)) = frame else { continue };
                consumed_seq.store(seq, Ordering::Release);
                let envelope = sealer.seal(Channel::Video, &vf.encode());
                shared.bytes_sent.fetch_add(envelope.len() as u64, Ordering::Relaxed);
                let t_send = Instant::now();
                if ws_tx.send(Message::Binary(envelope.into())).await.is_err() { break; }
                shared.frames_sent.fetch_add(1, Ordering::Relaxed);
                // A send that takes longer than a frame period means the TCP
                // buffer is full — a direct congestion signal.
                let budget = {
                    let ctl = shared.ctl.lock().unwrap();
                    Duration::from_secs_f64(1.0 / ctl.fps().max(1) as f64)
                };
                if t_send.elapsed() > budget {
                    shared.ctl.lock().unwrap().on_send_backlog();
                }
            }
        }
    }
    let _ = ws_tx.close().await;
}

/// Event-driven encode loop with a pacing floor.
///
/// Waits for a *new captured frame* (zero CPU while the screen is idle) and
/// encodes it immediately — unless that would exceed the target frame rate,
/// in which case it sleeps only the remainder of the frame budget. Frames
/// that arrive during that sleep are coalesced by the capture watch channel,
/// so the newest frame is always the one encoded (latest-frame semantics).
/// Compared to a free-running ticker this removes an average of half a frame
/// interval of "wait for the next tick" latency from every frame.
async fn video_task(
    state: Arc<AppState>,
    mut encoder: Box<dyn crate::encode::Encoder>,
    shared: Arc<Shared>,
    vid_tx: watch::Sender<Option<(u64, Arc<VideoFrame>)>>,
    consumed_seq: Arc<AtomicU64>,
) {
    let mut frames = state.frame_tx.subscribe();
    let mut last_captured_seq: u64 = 0;
    let mut out_seq: u64 = 0;
    let mut frame_seq16: u32 = 0;
    let mut last_encode_at = Instant::now() - Duration::from_secs(1);

    loop {
        if frames.changed().await.is_err() {
            break; // capture loop gone
        }

        let (bitrate, fps) = {
            let mut ctl = shared.ctl.lock().unwrap();
            ctl.on_tick();
            (ctl.bitrate_kbps(), ctl.fps())
        };

        // Pacing floor: never exceed the adaptive frame rate.
        let min_interval = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
        let since = last_encode_at.elapsed();
        if since < min_interval {
            tokio::time::sleep(min_interval - since).await;
        }

        // Newest captured frame (frames that landed during the pacing sleep
        // are coalesced — we always encode the latest).
        let frame = {
            let borrowed = frames.borrow_and_update();
            match borrowed.as_ref() {
                Some(f) if f.seq > last_captured_seq => Some(f.clone()),
                _ => None,
            }
        };
        let Some(frame) = frame else { continue };
        last_captured_seq = frame.seq;
        last_encode_at = Instant::now();

        let fk = shared.force_keyframe.swap(false, Ordering::Relaxed);
        let t_enc = Instant::now();
        let encoded = tokio::task::block_in_place(|| encoder.encode(&frame, fk, bitrate, fps));
        let encoded = match encoded {
            Ok(e) => e,
            Err(e) => {
                warn!("encode failed: {e:#}");
                continue;
            }
        };
        // EMA (α = 0.1) over encode time, stored as µs for atomic transport.
        let enc_us = t_enc.elapsed().as_micros() as u64;
        let prev = shared.encode_us_avg.load(Ordering::Relaxed);
        let ema = if prev == 0 {
            enc_us
        } else {
            (prev * 9 + enc_us) / 10
        };
        shared.encode_us_avg.store(ema, Ordering::Relaxed);

        if encoded.payload.is_empty() {
            continue; // encoder skipped this frame for rate control
        }
        if fk && !encoded.keyframe {
            // The request raced the encoder's own scheduling — retry next frame.
            shared.force_keyframe.store(true, Ordering::Relaxed);
        }

        frame_seq16 = frame_seq16.wrapping_add(1);
        let vf = VideoFrame {
            codec: encoded.codec,
            keyframe: encoded.keyframe,
            seq: frame_seq16,
            timestamp_us: frame.timestamp_us,
            width: frame.width as u16,
            height: frame.height as u16,
            payload: encoded.payload,
        };

        // Did the writer consume the previous slot? If not, the socket can't
        // keep up — the overwritten frame is a skip + congestion signal.
        if out_seq > 0 && consumed_seq.load(Ordering::Acquire) < out_seq {
            shared.frames_skipped.fetch_add(1, Ordering::Relaxed);
            shared.ctl.lock().unwrap().on_send_backlog();
            // Keyframes must survive: if we're about to overwrite an unsent
            // keyframe with a delta, re-request one so decoders can sync.
            if let Some((_, prev_vf)) = vid_tx.borrow().as_ref() {
                if prev_vf.keyframe && !vf.keyframe {
                    shared.force_keyframe.store(true, Ordering::Relaxed);
                }
            }
        }
        out_seq += 1;
        if vid_tx.send(Some((out_seq, Arc::new(vf)))).is_err() {
            break; // writer gone
        }
    }
}

/// Reads, decrypts and reacts to incoming envelopes.
///
/// Latency-critical handling happens *right here* — input events go straight
/// to the injection sink and pings are answered immediately, so neither ever
/// waits behind an encode or a video send.
async fn incoming_pump(
    mut ws_rx: SplitStream<WebSocket>,
    mut opener: Opener,
    shared: Arc<Shared>,
    ctl_tx: mpsc::Sender<ControlMsg>,
    input_sink: Arc<dyn InputSink>,
    handle: Arc<ClientHandle>,
) {
    while let Some(Ok(msg)) = ws_rx.next().await {
        match msg {
            Message::Binary(data) => match opener.open(&data) {
                Ok((Channel::Control, plaintext)) => {
                    shared.touch_recv();
                    let Some(ctl) = std::str::from_utf8(&plaintext)
                        .ok()
                        .and_then(|s| ControlMsg::from_json(s).ok())
                    else {
                        warn!("undecodable control payload; dropping");
                        continue;
                    };
                    match ctl {
                        ControlMsg::Input { events } => {
                            let allowed = shared.input_allowed.load(Ordering::Relaxed);
                            let mode = shared.input_mode.load(Ordering::Relaxed);
                            if allowed && mode != 0 {
                                input_sink.apply(&events);
                            } else if !events.is_empty() {
                                debug!(
                                    "dropping {} input events (no grant / view-only)",
                                    events.len()
                                );
                            }
                        }
                        ControlMsg::Ping { t0_us } => {
                            // try_send: a full control queue means the writer
                            // is wedged; a dropped pong is stale anyway.
                            let _ = ctl_tx.try_send(ControlMsg::Pong {
                                t0_us,
                                t1_us: now_us(),
                            });
                        }
                        ControlMsg::Stats { stats } => {
                            let mut ctl = shared.ctl.lock().unwrap();
                            ctl.on_rtt_sample(stats.rtt_ms);
                            ctl.on_viewer_stats(&stats);
                            drop(ctl);
                            *handle.stats.lock().unwrap() = stats;
                        }
                        ControlMsg::SetProfile { profile } => {
                            debug!(?profile, "client set profile");
                            shared.ctl.lock().unwrap().set_profile(profile);
                        }
                        ControlMsg::SetInputMode { mode } => {
                            shared.input_mode.store(mode_to_u8(mode), Ordering::Relaxed);
                        }
                        ControlMsg::RequestKeyframe => {
                            shared.force_keyframe.store(true, Ordering::Relaxed);
                        }
                        ControlMsg::Bye { reason } => {
                            info!(%reason, "client said bye");
                            return;
                        }
                        other => debug!(?other, "ignoring unexpected control message"),
                    }
                }
                Ok((chan, _)) => debug!(?chan, "unexpected inbound channel; dropping"),
                Err(e) => {
                    warn!("envelope rejected: {e}; closing session");
                    return;
                }
            },
            Message::Close(_) => return,
            // Plaintext frames after auth are a protocol violation.
            Message::Text(_) => {
                warn!("plaintext message after auth; closing");
                return;
            }
            _ => {}
        }
    }
}

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
use base64::Engine as _;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use ndsp_protocol::{
    envelope::{Channel, Direction, Opener, Sealer},
    media::{AudioCodec, AudioFrame, VideoFrame},
    messages::{AudioWireCodec, ControlMsg, InputMode, Profile, FILE_CHUNK_BYTES},
};
use sha2::Digest as _;
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
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
/// Per-session audio switchboard: the *viewer* opts in (`enabled`), the
/// *panel* permits (`allowed`); packets flow only when both hold, and the
/// global capture loop's listener count tracks that conjunction exactly.
struct AudioCtl {
    enabled: bool,
    allowed: bool,
    codec: AudioWireCodec,
    /// Whether this session currently counts toward `state.audio_listeners`.
    counted: bool,
}

/// One in-flight viewer→host file transfer (panel-accepted).
struct ActiveTransfer {
    id: String,
    file: std::fs::File,
    part_path: PathBuf,
    final_name: String,
    expected_size: u64,
    expected_sha256: String,
    hasher: sha2::Sha256,
    received: u64,
    next_seq: u32,
}

impl ActiveTransfer {
    /// Abort cleanup: close + delete the partial file.
    fn discard(self) {
        drop(self.file);
        let _ = std::fs::remove_file(&self.part_path);
    }
}

/// One in-flight host→viewer file send (panel-initiated, ROADMAP P2.15).
/// Nothing is streamed until the *viewer* explicitly accepts the offer.
struct OutgoingFile {
    id: String,
    /// Session-owned spool file. While `sending` the streaming task owns it
    /// (and deletes it on every exit path); before that, whoever clears the
    /// entry deletes it.
    path: PathBuf,
    offered_at: Instant,
    /// The viewer accepted and the streaming task is running.
    sending: bool,
    /// Tells a running streaming task to stop (viewer abort / teardown).
    cancel: Arc<AtomicBool>,
}

struct Shared {
    input_allowed: Arc<AtomicBool>,
    clipboard_allowed: Arc<AtomicBool>,
    /// Panel indicator: this session is actively receiving audio.
    audio_active: Arc<AtomicBool>,
    audio: Mutex<AudioCtl>,
    /// The panel-accepted transfer currently receiving chunks (max one).
    transfer: Mutex<Option<ActiveTransfer>>,
    /// The host→viewer transfer currently offered/streaming (max one).
    outgoing: Mutex<Option<OutgoingFile>>,
    /// An offer is pending a panel decision (rate-limits offer spam).
    offer_pending: AtomicBool,
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
    /// EMA of the color-conversion share of encode time in µs.
    convert_us_avg: AtomicU64,
    /// EMA of capture→encode-start age in µs (scheduling wait).
    capture_age_us_avg: AtomicU64,
    /// EMA of seal+socket-send time per video frame in µs.
    seal_send_us_avg: AtomicU64,
}

/// EMA with α = 0.1 over µs samples stored in relaxed atomics.
fn ema_update(cell: &AtomicU64, sample_us: u64) {
    let prev = cell.load(Ordering::Relaxed);
    let ema = if prev == 0 {
        sample_us
    } else {
        (prev * 9 + sample_us) / 10
    };
    cell.store(ema, Ordering::Relaxed);
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

impl Shared {
    /// Recompute the audio conjunction and keep the global listener count +
    /// panel indicator in sync. Returns the new active state.
    fn sync_audio(&self, state: &AppState) -> bool {
        let mut a = self.audio.lock().unwrap();
        let active = a.enabled && a.allowed;
        if active != a.counted {
            if active {
                state.audio_listener_add();
            } else {
                state.audio_listener_remove();
            }
            a.counted = active;
            self.audio_active.store(active, Ordering::Relaxed);
        }
        active
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

    let mode = *state.mode.lock().unwrap();
    let encoder = match crate::encode::create(auth.codec, mode) {
        Ok(e) => e,
        Err(e) => {
            warn!("encoder init failed: {e:#}");
            return;
        }
    };

    let input_allowed = Arc::new(AtomicBool::new(auth.input_allowed));
    let clipboard_allowed = Arc::new(AtomicBool::new(auth.clipboard_allowed));
    let audio_allowed_flag = Arc::new(AtomicBool::new(auth.audio_allowed));
    let audio_active = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(Shared {
        input_allowed: input_allowed.clone(),
        clipboard_allowed: clipboard_allowed.clone(),
        audio_active: audio_active.clone(),
        audio: Mutex::new(AudioCtl {
            enabled: false,
            allowed: auth.audio_allowed,
            codec: AudioWireCodec::Opus,
            counted: false,
        }),
        transfer: Mutex::new(None),
        outgoing: Mutex::new(None),
        offer_pending: AtomicBool::new(false),
        input_mode: AtomicU32::new(mode_to_u8(InputMode::ViewOnly)),
        force_keyframe: AtomicBool::new(true),
        ctl: Mutex::new(AdaptiveController::new(Profile::Office, auth.codec)),
        last_recv_ms: AtomicU64::new(0),
        epoch: Instant::now(),
        bytes_sent: AtomicU64::new(0),
        frames_sent: AtomicU64::new(0),
        frames_skipped: AtomicU64::new(0),
        encode_us_avg: AtomicU64::new(0),
        convert_us_avg: AtomicU64::new(0),
        capture_age_us_avg: AtomicU64::new(0),
        seal_send_us_avg: AtomicU64::new(0),
    });

    let supports_cursor = auth.client.features.iter().any(|f| f == "cursor");
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
        clipboard_allowed: clipboard_allowed.clone(),
        audio_allowed: audio_allowed_flag.clone(),
        audio_active: audio_active.clone(),
        supports_cursor,
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
    // Audio lane: short bounded queue. Audio must stay continuous (unlike
    // video, stale packets are not overwritten) but a wedged socket drops
    // rather than queues — the web jitter buffer conceals isolated gaps.
    let (aud_tx, aud_rx) = mpsc::channel::<AudioFrame>(16);
    // Bulk file lane (host→viewer sends): lowest writer priority, tightly
    // bounded so the streaming task paces on real socket drain instead of
    // queueing megabytes — a file transfer never starves video or audio.
    let (file_tx, file_rx) = mpsc::channel::<ControlMsg>(4);

    let writer = tokio::spawn(writer_task(
        ws_tx,
        sealer,
        ctl_rx,
        aud_rx,
        vid_rx,
        file_rx,
        shared.clone(),
        consumed_seq.clone(),
    ));
    let pump = tokio::spawn(incoming_pump(
        state.clone(),
        ws_rx,
        opener,
        shared.clone(),
        ctl_tx.clone(),
        file_tx,
        input_sink,
        handle.clone(),
    ));
    let audio = tokio::spawn(audio_task(state.clone(), shared.clone(), aud_tx));
    let video = tokio::spawn(video_task(
        state.clone(),
        encoder,
        auth.codec,
        shared.clone(),
        vid_tx,
        consumed_seq,
    ));
    // Host clipboard changes → granted clients (control channel).
    let mut clipboard_rx = state.clipboard_tx.subscribe();
    clipboard_rx.mark_unchanged();

    // ---- supervisor ----
    let mut host_stats_timer = tokio::time::interval(HOST_STATS_INTERVAL);
    host_stats_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut pump = pump;
    let mut bytes_window_start = Instant::now();
    // Cursor forwarding (only for cursor-capable clients): shape image on
    // change, then positions. Control channel → never queued behind video.
    let mut cursor_rx = state.cursor_tx.subscribe();
    // Deliver the current cursor state right away (a client connecting while
    // the mouse is idle should not wait for the first physical move).
    cursor_rx.mark_changed();
    let mut sent_shape_seq: u64 = 0;
    let mut sent_cursor_hidden = false;
    loop {
        tokio::select! {
            Some(cmd) = cmd_rx.recv() => match cmd {
                SessionCommand::SetInputGrant(allowed) => {
                    input_allowed.store(allowed, Ordering::Relaxed);
                    if ctl_tx.send(ControlMsg::InputGrant { allowed }).await.is_err() { break; }
                }
                SessionCommand::SetClipboardGrant(allowed) => {
                    clipboard_allowed.store(allowed, Ordering::Relaxed);
                    if ctl_tx.send(ControlMsg::ClipboardGrant { allowed }).await.is_err() { break; }
                }
                SessionCommand::SetAudioGrant(allowed) => {
                    audio_allowed_flag.store(allowed, Ordering::Relaxed);
                    shared.audio.lock().unwrap().allowed = allowed;
                    shared.sync_audio(&state);
                    if ctl_tx.send(ControlMsg::AudioGrant { allowed }).await.is_err() { break; }
                }
                SessionCommand::AnswerFileOffer { id, accept, name, size_bytes, sha256_hex } => {
                    shared.offer_pending.store(false, Ordering::Relaxed);
                    let msg = if accept {
                        match open_transfer(&state, &id, &name, size_bytes, &sha256_hex) {
                            Ok(t) => {
                                *shared.transfer.lock().unwrap() = Some(t);
                                info!(%id, %name, size_bytes, "file transfer accepted; receiving");
                                ControlMsg::FileAnswer { id, accept: true, reason: None }
                            }
                            Err(e) => {
                                warn!(%id, "cannot open transfer destination: {e:#}");
                                ControlMsg::FileAnswer {
                                    id,
                                    accept: false,
                                    reason: Some("host storage error".into()),
                                }
                            }
                        }
                    } else {
                        info!(%id, "file transfer declined");
                        ControlMsg::FileAnswer { id, accept: false, reason: Some("declined on the host".into()) }
                    };
                    if ctl_tx.send(msg).await.is_err() { break; }
                }
                SessionCommand::SendFile { id, name, size_bytes, sha256_hex, path } => {
                    // One outgoing transfer at a time (mirrors the incoming rule).
                    let refused = {
                        let mut out = shared.outgoing.lock().unwrap();
                        if out.is_some() {
                            true
                        } else {
                            *out = Some(OutgoingFile {
                                id: id.clone(),
                                path: path.clone(),
                                offered_at: Instant::now(),
                                sending: false,
                                cancel: Arc::new(AtomicBool::new(false)),
                            });
                            false
                        }
                    };
                    if refused {
                        warn!(%id, "host→viewer send refused: another outgoing transfer is active");
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    info!(%id, %name, size_bytes, "offering file to viewer");
                    let offer = ControlMsg::FileOffer { id, name, size_bytes, sha256: sha256_hex };
                    if ctl_tx.send(offer).await.is_err() { break; }
                }
                SessionCommand::Kick { reason } => {
                    let _ = ctl_tx.send(ControlMsg::Bye { reason }).await;
                    // Give the writer a moment to flush the Bye.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    break;
                }
            },

            _ = &mut pump => break, // client went away / protocol violation

            changed = clipboard_rx.changed() => {
                if changed.is_err() { continue; }
                if !clipboard_allowed.load(Ordering::Relaxed) { continue; }
                let (seq, text) = {
                    let borrowed = clipboard_rx.borrow_and_update();
                    (borrowed.0, borrowed.1.clone())
                };
                if seq == 0 { continue; }
                if ctl_tx.send(ControlMsg::Clipboard { text: (*text).clone() }).await.is_err() { break; }
            }

            changed = cursor_rx.changed(), if supports_cursor => {
                if changed.is_err() { continue; }
                let cs = cursor_rx.borrow_and_update().clone();
                if cs.seq == 0 { continue; }
                if cs.composited {
                    // Cursor is baked into video while a legacy client is
                    // connected — hide the overlay (once).
                    if !sent_cursor_hidden {
                        sent_cursor_hidden = true;
                        if ctl_tx.send(ControlMsg::CursorPos { x: cs.x, y: cs.y, visible: false }).await.is_err() { break; }
                    }
                    continue;
                }
                sent_cursor_hidden = false;
                if cs.shape_seq != sent_shape_seq {
                    if let Some(shape) = &cs.shape {
                        sent_shape_seq = cs.shape_seq;
                        use base64::Engine as _;
                        let rgba = base64::engine::general_purpose::STANDARD.encode(&shape.rgba);
                        let msg = ControlMsg::CursorShape {
                            width: shape.width,
                            height: shape.height,
                            hot_x: shape.hot_x,
                            hot_y: shape.hot_y,
                            rgba,
                        };
                        if ctl_tx.send(msg).await.is_err() { break; }
                    }
                }
                if ctl_tx.send(ControlMsg::CursorPos { x: cs.x, y: cs.y, visible: cs.visible }).await.is_err() { break; }
            }

            _ = host_stats_timer.tick() => {
                if shared.recv_age() > RECV_TIMEOUT {
                    warn!(client_id, "client unresponsive; closing");
                    break;
                }
                // Expire an outgoing file offer the viewer never answered.
                let expired = {
                    let mut out = shared.outgoing.lock().unwrap();
                    match out.as_ref() {
                        Some(t)
                            if !t.sending
                                && t.offered_at.elapsed() > crate::transfers::OFFER_TTL =>
                        {
                            out.take()
                        }
                        _ => None,
                    }
                };
                if let Some(t) = expired {
                    warn!(id = %t.id, "outgoing file offer expired without an answer");
                    let _ = std::fs::remove_file(&t.path);
                    let abort = ControlMsg::FileAbort { id: t.id, reason: "offer expired".into() };
                    if ctl_tx.send(abort).await.is_err() { break; }
                }
                let stats = {
                    let mut hs = state.host_stats.lock().unwrap();
                    {
                        let ctl = shared.ctl.lock().unwrap();
                        hs.target_bitrate_kbps = ctl.bitrate_kbps();
                    }
                    hs.encode_ms_avg =
                        shared.encode_us_avg.load(Ordering::Relaxed) as f32 / 1000.0;
                    hs.convert_ms_avg =
                        shared.convert_us_avg.load(Ordering::Relaxed) as f32 / 1000.0;
                    hs.capture_age_ms_avg =
                        shared.capture_age_us_avg.load(Ordering::Relaxed) as f32 / 1000.0;
                    hs.seal_send_ms_avg =
                        shared.seal_send_us_avg.load(Ordering::Relaxed) as f32 / 1000.0;
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
    state.transfers.drop_for_device(&auth.client.device_id);
    if let Some(t) = shared.transfer.lock().unwrap().take() {
        warn!(id = %t.id, "session ended mid-transfer; discarding partial file");
        t.discard();
    }
    if let Some(t) = shared.outgoing.lock().unwrap().take() {
        t.cancel.store(true, Ordering::Relaxed);
        if !t.sending {
            // Not yet streaming → nobody else owns the spool file.
            let _ = std::fs::remove_file(&t.path);
        }
    }
    {
        // Release our audio listener slot if we held one.
        let mut a = shared.audio.lock().unwrap();
        a.enabled = false;
        if a.counted {
            state.audio_listener_remove();
            a.counted = false;
        }
    }
    pump.abort();
    video.abort();
    audio.abort();
    drop(ctl_tx); // writer exits once both inputs are gone
    writer.abort();
    info!(client_id, "session ended");
}

/// Owns the socket sink. Control preempts audio preempts video preempts
/// bulk file chunks; video is latest-only, audio is a short FIFO
/// (continuity matters), control is rare, and file chunks only flow when
/// nothing latency-sensitive is waiting.
#[allow(clippy::too_many_arguments)]
async fn writer_task(
    mut ws_tx: SplitSink<WebSocket, Message>,
    mut sealer: Sealer,
    mut ctl_rx: mpsc::Receiver<ControlMsg>,
    mut aud_rx: mpsc::Receiver<AudioFrame>,
    mut vid_rx: watch::Receiver<Option<(u64, Arc<VideoFrame>)>>,
    mut file_rx: mpsc::Receiver<ControlMsg>,
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

            frame = aud_rx.recv() => {
                let Some(af) = frame else { break };
                let envelope = sealer.seal_parts(Channel::Audio, &[&af.header(), &af.payload]);
                shared.bytes_sent.fetch_add(envelope.len() as u64, Ordering::Relaxed);
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
                let t_seal = Instant::now();
                // In-place seal of header ‖ payload: one allocation, no
                // intermediate full-frame concatenation copy.
                let envelope =
                    sealer.seal_parts(Channel::Video, &[&vf.header(), &vf.payload]);
                shared.bytes_sent.fetch_add(envelope.len() as u64, Ordering::Relaxed);
                let t_send = Instant::now();
                if ws_tx.send(Message::Binary(envelope.into())).await.is_err() { break; }
                ema_update(&shared.seal_send_us_avg, t_seal.elapsed().as_micros() as u64);
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

            msg = file_rx.recv() => {
                let Some(msg) = msg else { break };
                let json = msg.to_json().expect("file message serialization");
                let envelope = sealer.seal(Channel::Control, json.as_bytes());
                shared.bytes_sent.fetch_add(envelope.len() as u64, Ordering::Relaxed);
                if ws_tx.send(Message::Binary(envelope.into())).await.is_err() { break; }
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
    codec: ndsp_protocol::messages::Codec,
    shared: Arc<Shared>,
    vid_tx: watch::Sender<Option<(u64, Arc<VideoFrame>)>>,
    consumed_seq: Arc<AtomicU64>,
) {
    let mut frames = state.frame_tx.subscribe();
    let mut encoder_size: (u32, u32) = (0, 0);
    let mut last_captured_seq: u64 = 0;
    let mut out_seq: u64 = 0;
    let mut frame_seq16: u32 = 0;
    let mut last_encode_at = Instant::now() - Duration::from_secs(1);

    loop {
        // Wake on a new captured frame — or, rarely, on a pending keyframe
        // request while the screen is static (capture publishes nothing for
        // an unchanged screen, but a resyncing decoder still needs its IDR;
        // without this arm it would wait for on-screen activity).
        let keyframe_pending = shared.force_keyframe.load(Ordering::Relaxed);
        tokio::select! {
            changed = frames.changed() => {
                if changed.is_err() {
                    break; // capture loop gone
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(250)), if keyframe_pending => {}
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
            // Rate-capped means the pipeline has headroom — so prefer
            // encoding content captured *after* the pacing gate opened.
            // Without this, the frame picked here is on average half a
            // capture interval old (measured: 9 ms at a 30 fps cap with
            // 60 Hz capture). Bounded wait so a screen going static (or a
            // pending keyframe) can't stall the loop.
            let fresh_wait = Duration::from_millis(17).min(min_interval);
            let stale = frames
                .borrow()
                .as_ref()
                .is_none_or(|f| now_us().saturating_sub(f.timestamp_us) > 4_000);
            if stale {
                let _ = tokio::time::timeout(fresh_wait, frames.changed()).await;
            }
        }

        // Newest captured frame (frames that landed during the pacing sleep
        // are coalesced — we always encode the latest). A pending keyframe
        // request may re-encode the current frame even without a new one.
        let want_key_now = shared.force_keyframe.load(Ordering::Relaxed);
        let frame = {
            let borrowed = frames.borrow_and_update();
            match borrowed.as_ref() {
                Some(f) if f.seq > last_captured_seq || want_key_now => Some(f.clone()),
                _ => None,
            }
        };
        let Some(frame) = frame else { continue };
        last_captured_seq = frame.seq;
        last_encode_at = Instant::now();

        // Resolution change (mode switch / rotation): recreate the encoder.
        // Hardware MF encoders are fixed-resolution; the software encoders
        // could adapt internally, but a single recreate path is simpler and
        // this is a rare, user-visible event anyway.
        if encoder_size != (frame.width, frame.height) {
            if encoder_size != (0, 0) {
                info!(
                    w = frame.width,
                    h = frame.height,
                    "capture resolution changed; recreating encoder"
                );
                match crate::encode::create(
                    codec,
                    ndsp_protocol::messages::DisplayMode {
                        width: frame.width,
                        height: frame.height,
                        refresh_hz: 60,
                    },
                ) {
                    Ok(e) => {
                        encoder = e;
                        shared.force_keyframe.store(true, Ordering::Relaxed);
                    }
                    Err(e) => {
                        warn!("encoder recreate failed: {e:#}");
                        continue;
                    }
                }
            }
            encoder_size = (frame.width, frame.height);
        }

        let fk = shared.force_keyframe.swap(false, Ordering::Relaxed);
        // Capture → encode-start scheduling wait (frame age at encode time).
        ema_update(
            &shared.capture_age_us_avg,
            now_us().saturating_sub(frame.timestamp_us),
        );
        let t_enc = Instant::now();
        let encoded = tokio::task::block_in_place(|| encoder.encode(&frame, fk, bitrate, fps));
        let encoded = match encoded {
            Ok(e) => e,
            Err(e) => {
                warn!("encode failed: {e:#}");
                continue;
            }
        };
        ema_update(&shared.encode_us_avg, t_enc.elapsed().as_micros() as u64);
        ema_update(&shared.convert_us_avg, encoded.convert_us as u64);

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

/// Forwards global audio blocks to this client while its audio is active,
/// in the payload format the client asked for.
async fn audio_task(state: Arc<AppState>, shared: Arc<Shared>, aud_tx: mpsc::Sender<AudioFrame>) {
    let mut rx = state.audio_tx.subscribe();
    loop {
        let block = match rx.recv().await {
            Ok(b) => b,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                debug!(missed = n, "audio subscriber lagged; skipping");
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        };
        let codec = {
            let a = shared.audio.lock().unwrap();
            if !(a.enabled && a.allowed) {
                continue;
            }
            a.codec
        };
        let (codec, payload) = match codec {
            AudioWireCodec::Opus => (AudioCodec::Opus, block.opus.clone()),
            AudioWireCodec::Pcm => {
                let mut bytes = Vec::with_capacity(block.pcm.len() * 2);
                for s in &block.pcm {
                    bytes.extend_from_slice(&s.to_le_bytes());
                }
                (AudioCodec::PcmS16le, bytes)
            }
        };
        let af = AudioFrame {
            codec,
            channels: crate::audio::CHANNELS,
            seq: block.seq,
            timestamp_us: block.timestamp_us,
            sample_rate: crate::audio::SAMPLE_RATE,
            payload,
        };
        // Full lane = wedged socket; drop (the jitter buffer conceals it).
        if let Err(mpsc::error::TrySendError::Closed(_)) = aud_tx.try_send(af) {
            return;
        }
    }
}

/// Create the `.part` file for a panel-accepted transfer.
fn open_transfer(
    state: &AppState,
    id: &str,
    name: &str,
    size_bytes: u64,
    sha256_hex: &str,
) -> anyhow::Result<ActiveTransfer> {
    let dir = state.cfg.file_transfer_dir();
    std::fs::create_dir_all(&dir)?;
    let part_path = dir.join(format!(".{}.part", id));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part_path)?;
    Ok(ActiveTransfer {
        id: id.to_string(),
        file,
        part_path,
        final_name: name.to_string(),
        expected_size: size_bytes,
        expected_sha256: sha256_hex.to_ascii_lowercase(),
        hasher: sha2::Sha256::new(),
        received: 0,
        next_seq: 0,
    })
}

/// Reads, decrypts and reacts to incoming envelopes.
///
/// Latency-critical handling happens *right here* — input events go straight
/// to the injection sink and pings are answered immediately, so neither ever
/// waits behind an encode or a video send.
#[allow(clippy::too_many_arguments)]
async fn incoming_pump(
    state: Arc<AppState>,
    mut ws_rx: SplitStream<WebSocket>,
    mut opener: Opener,
    shared: Arc<Shared>,
    ctl_tx: mpsc::Sender<ControlMsg>,
    file_tx: mpsc::Sender<ControlMsg>,
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
                        ControlMsg::SetAudio { enabled, codec } => {
                            let allowed = {
                                let mut a = shared.audio.lock().unwrap();
                                a.enabled = enabled && state.cfg.file.audio_enabled;
                                if let Some(c) = codec {
                                    a.codec = c;
                                }
                                a.allowed
                            };
                            let active = shared.sync_audio(&state);
                            debug!(enabled, ?codec, active, "viewer audio toggle");
                            if enabled && !allowed {
                                let _ = ctl_tx.try_send(ControlMsg::AudioGrant { allowed: false });
                            }
                            if enabled && !state.cfg.file.audio_enabled {
                                let _ = ctl_tx.try_send(ControlMsg::Error {
                                    code: "audio_disabled".into(),
                                    message: "audio is disabled on this host".into(),
                                });
                            }
                        }
                        ControlMsg::Clipboard { text } => {
                            if !shared.clipboard_allowed.load(Ordering::Relaxed) {
                                debug!("dropping clipboard payload (no grant)");
                                continue;
                            }
                            let clipboard = state.clipboard.clone();
                            let sync = state.clipboard_sync.clone();
                            // arboard may block briefly (X11 owners etc.).
                            let applied = tokio::task::spawn_blocking(move || {
                                sync.apply_from_viewer(clipboard.as_ref(), &text)
                            })
                            .await;
                            match applied {
                                // Metadata only — clipboard *content* never
                                // hits the logs.
                                Ok(Ok(true)) => debug!("applied viewer clipboard"),
                                Ok(Ok(false)) => {
                                    let _ = ctl_tx.try_send(ControlMsg::Error {
                                        code: "clipboard_too_large".into(),
                                        message: "clipboard payload exceeds the size cap".into(),
                                    });
                                }
                                Ok(Err(e)) => warn!("host clipboard write failed: {e:#}"),
                                Err(e) => warn!("clipboard task join error: {e}"),
                            }
                        }
                        ControlMsg::FileOffer {
                            id,
                            name,
                            size_bytes,
                            sha256,
                        } => {
                            let deny = |reason: &str| ControlMsg::FileAnswer {
                                id: id.clone(),
                                accept: false,
                                reason: Some(reason.into()),
                            };
                            let max = state.cfg.file.max_file_mb.saturating_mul(1024 * 1024);
                            let sane_id = !id.is_empty()
                                && id.len() <= 64
                                && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
                            let sane_hash =
                                sha256.len() == 64 && sha256.chars().all(|c| c.is_ascii_hexdigit());
                            let msg = if !sane_id || !sane_hash {
                                Some(deny("malformed offer"))
                            } else if size_bytes == 0 || size_bytes > max {
                                Some(deny("file exceeds the host's size limit"))
                            } else if shared.transfer.lock().unwrap().is_some()
                                || shared.offer_pending.load(Ordering::Relaxed)
                            {
                                Some(deny("another transfer is already in progress"))
                            } else {
                                let clean = crate::transfers::sanitize_filename(&name);
                                let registered =
                                    state.transfers.register(crate::transfers::PendingOffer {
                                        id: id.clone(),
                                        device_id: handle.device_id.clone(),
                                        device_name: handle.name.clone(),
                                        name: clean,
                                        size_bytes,
                                        sha256_hex: sha256.to_ascii_lowercase(),
                                        offered_at: Instant::now(),
                                        offered_unix: crate::transfers::now_unix(),
                                        session: handle.commands.clone(),
                                    });
                                if registered {
                                    shared.offer_pending.store(true, Ordering::Relaxed);
                                    None // wait for the panel decision
                                } else {
                                    Some(deny("duplicate offer id"))
                                }
                            };
                            if let Some(msg) = msg {
                                if ctl_tx.send(msg).await.is_err() {
                                    return;
                                }
                            }
                        }
                        ControlMsg::FileChunk { id, seq, data } => {
                            // Scope the lock tightly: the guard must be gone
                            // before any await (Send-ness of the future).
                            let abort: Option<String> = {
                                let mut guard = shared.transfer.lock().unwrap();
                                let abort = match guard.as_mut() {
                                    Some(t) if t.id == id => apply_chunk(t, seq, &data).err(),
                                    _ => Some("no such transfer".to_string()),
                                };
                                if abort.is_some() {
                                    if let Some(t) = guard.take() {
                                        t.discard();
                                    }
                                }
                                abort
                            };
                            if let Some(reason) = abort {
                                warn!(%id, %reason, "file transfer aborted");
                                let _ = ctl_tx.send(ControlMsg::FileAbort { id, reason }).await;
                            }
                        }
                        ControlMsg::FileEnd { id } => {
                            let taken = {
                                let mut guard = shared.transfer.lock().unwrap();
                                match guard.as_ref() {
                                    Some(t) if t.id == id => guard.take(),
                                    _ => None,
                                }
                            };
                            let Some(t) = taken else {
                                let _ = ctl_tx
                                    .send(ControlMsg::FileAbort {
                                        id,
                                        reason: "no such transfer".into(),
                                    })
                                    .await;
                                continue;
                            };
                            let msg = match finalize_transfer(&state, t) {
                                Ok(path) => {
                                    info!(%id, path = %path.display(), "file transfer complete");
                                    ControlMsg::FileDone { id }
                                }
                                Err(reason) => {
                                    warn!(%id, %reason, "file transfer failed verification");
                                    ControlMsg::FileAbort { id, reason }
                                }
                            };
                            if ctl_tx.send(msg).await.is_err() {
                                return;
                            }
                        }
                        ControlMsg::FileAnswer { id, accept, .. } => {
                            // The viewer's decision on a host→viewer offer.
                            let start: Option<(PathBuf, Arc<AtomicBool>)> = {
                                let mut guard = shared.outgoing.lock().unwrap();
                                match guard.as_mut() {
                                    Some(t) if t.id == id && !t.sending => {
                                        if accept {
                                            t.sending = true;
                                            Some((t.path.clone(), t.cancel.clone()))
                                        } else {
                                            let t = guard.take().expect("matched above");
                                            let _ = std::fs::remove_file(&t.path);
                                            info!(%id, "viewer declined the file");
                                            None
                                        }
                                    }
                                    _ => {
                                        debug!(%id, "file answer for unknown transfer; ignoring");
                                        None
                                    }
                                }
                            };
                            if let Some((path, cancel)) = start {
                                info!(%id, "viewer accepted; streaming file");
                                tokio::spawn(stream_outgoing(id, path, cancel, file_tx.clone()));
                            }
                        }
                        ControlMsg::FileDone { id } => {
                            // Receiver-side verification succeeded.
                            let done = {
                                let mut guard = shared.outgoing.lock().unwrap();
                                match guard.as_ref() {
                                    Some(t) if t.id == id => guard.take(),
                                    _ => None,
                                }
                            };
                            if done.is_some() {
                                info!(%id, "viewer verified the sent file");
                            }
                        }
                        ControlMsg::FileAbort { id, reason } => {
                            // Incoming (viewer→host) transfer?
                            {
                                let mut guard = shared.transfer.lock().unwrap();
                                if guard.as_ref().is_some_and(|t| t.id == id) {
                                    info!(%id, %reason, "viewer cancelled file transfer");
                                    if let Some(t) = guard.take() {
                                        t.discard();
                                    }
                                    continue;
                                }
                            }
                            // Outgoing (host→viewer) transfer?
                            let out = {
                                let mut guard = shared.outgoing.lock().unwrap();
                                match guard.as_ref() {
                                    Some(t) if t.id == id => guard.take(),
                                    _ => None,
                                }
                            };
                            if let Some(t) = out {
                                info!(%id, %reason, "viewer aborted host→viewer transfer");
                                t.cancel.store(true, Ordering::Relaxed);
                                if !t.sending {
                                    let _ = std::fs::remove_file(&t.path);
                                }
                            }
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

/// Validate + append one chunk to an active transfer. `Err(reason)` aborts.
fn apply_chunk(t: &mut ActiveTransfer, seq: u32, data_b64: &str) -> Result<(), String> {
    if seq != t.next_seq {
        return Err(format!(
            "out-of-order chunk (expected {}, got {seq})",
            t.next_seq
        ));
    }
    let data = base64::engine::general_purpose::STANDARD
        .decode(data_b64)
        .map_err(|_| "bad chunk encoding".to_string())?;
    if data.is_empty() || data.len() > FILE_CHUNK_BYTES {
        return Err("chunk size out of bounds".to_string());
    }
    if t.received + data.len() as u64 > t.expected_size {
        return Err("more data than offered".to_string());
    }
    t.file
        .write_all(&data)
        .map_err(|e| format!("write failed: {e}"))?;
    t.hasher.update(&data);
    t.received += data.len() as u64;
    t.next_seq += 1;
    Ok(())
}

/// Streams an accepted host→viewer file through the writer's lowest-
/// priority lane (bulk chunks never starve control/audio/video; the tightly
/// bounded lane paces reads on actual socket drain). Owns the spool file
/// from the moment the transfer entered `sending` — deletes it on every
/// exit path (success, viewer abort, session gone, read error).
async fn stream_outgoing(
    id: String,
    path: PathBuf,
    cancel: Arc<AtomicBool>,
    file_tx: mpsc::Sender<ControlMsg>,
) {
    use tokio::io::AsyncReadExt as _;
    let result: Result<(), String> = async {
        let mut file = tokio::fs::File::open(&path)
            .await
            .map_err(|e| format!("open failed: {e}"))?;
        let mut buf = vec![0u8; FILE_CHUNK_BYTES];
        let mut seq: u32 = 0;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Ok(()); // viewer aborted / session tearing down
            }
            let mut filled = 0;
            while filled < buf.len() {
                let n = file
                    .read(&mut buf[filled..])
                    .await
                    .map_err(|e| format!("read failed: {e}"))?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                break;
            }
            let data = base64::engine::general_purpose::STANDARD.encode(&buf[..filled]);
            let chunk = ControlMsg::FileChunk {
                id: id.clone(),
                seq,
                data,
            };
            file_tx
                .send(chunk)
                .await
                .map_err(|_| "session closed".to_string())?;
            seq += 1;
            if filled < buf.len() {
                break; // EOF reached mid-buffer
            }
        }
        file_tx
            .send(ControlMsg::FileEnd { id: id.clone() })
            .await
            .map_err(|_| "session closed".to_string())?;
        Ok(())
    }
    .await;
    if let Err(reason) = result {
        warn!(%id, %reason, "host→viewer file stream failed");
        let _ = file_tx
            .send(ControlMsg::FileAbort {
                id,
                reason: "host-side read error".into(),
            })
            .await;
    }
    let _ = tokio::fs::remove_file(&path).await;
}

/// Verify a finished transfer and move it to its final destination.
fn finalize_transfer(state: &AppState, mut t: ActiveTransfer) -> Result<PathBuf, String> {
    if t.received != t.expected_size {
        let r = format!(
            "size mismatch ({} of {} bytes)",
            t.received, t.expected_size
        );
        t.discard();
        return Err(r);
    }
    let digest = hex::encode(std::mem::take(&mut t.hasher).finalize());
    if digest != t.expected_sha256 {
        t.discard();
        return Err("sha256 mismatch — file corrupted in transit".to_string());
    }
    if let Err(e) = t.file.flush().and_then(|_| t.file.sync_all()) {
        let r = format!("flush failed: {e}");
        t.discard();
        return Err(r);
    }
    let dest = crate::transfers::unique_destination(&state.cfg.file_transfer_dir(), &t.final_name);
    drop(t.file);
    match std::fs::rename(&t.part_path, &dest) {
        Ok(()) => Ok(dest),
        Err(e) => {
            let _ = std::fs::remove_file(&t.part_path);
            Err(format!("rename failed: {e}"))
        }
    }
}

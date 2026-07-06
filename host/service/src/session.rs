//! Post-authentication client session: paced encode/send loop, encrypted
//! control handling, input bridging, adaptation, health tracking.

use axum::extract::ws::{Message, WebSocket};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use ndsp_protocol::{
    envelope::{Channel, Direction, Opener, Sealer},
    media::VideoFrame,
    messages::{ControlMsg, InputMode, Profile},
};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
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

pub async fn run(
    state: Arc<AppState>,
    socket: WebSocket,
    auth: AuthComplete,
    addr: SocketAddr,
    input_sink: Arc<dyn InputSink>,
) {
    let (ws_tx, ws_rx) = socket.split();
    let session_key = auth.session_key;
    let mut sealer = Sealer::new(&session_key, Direction::ServerToClient);
    let opener = Opener::new(&session_key, Direction::ClientToServer);

    let mut encoder = match crate::encode::create(auth.codec) {
        Ok(e) => e,
        Err(e) => {
            warn!("encoder init failed: {e:#}");
            return;
        }
    };

    let input_allowed = Arc::new(AtomicBool::new(auth.input_allowed));
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

    // Incoming pump: decrypt + parse on a dedicated task, forward ControlMsg.
    let (in_tx, mut in_rx) = mpsc::channel::<ControlMsg>(64);
    let pump = tokio::spawn(incoming_pump(ws_rx, opener, in_tx));

    let mut ctl = AdaptiveController::new(Profile::Office);
    let mut ws_tx = ws_tx;
    let mut frames = state.frame_tx.subscribe();
    let mut last_sent_seq: u64 = 0;
    let mut force_keyframe = true;
    let mut last_recv = Instant::now();
    let mut host_stats_timer = tokio::time::interval(HOST_STATS_INTERVAL);
    let mut input_mode = InputMode::ViewOnly;
    let mut frame_seq: u32 = 0;
    let mut encode_ms_avg = 0.0f32;
    let mut bytes_window: u64 = 0;
    let mut bytes_window_start = Instant::now();

    'session: loop {
        let frame_interval = Duration::from_secs_f64(1.0 / ctl.fps().max(1) as f64);
        let pace = tokio::time::sleep(frame_interval);
        tokio::select! {
            biased;

            Some(cmd) = cmd_rx.recv() => match cmd {
                SessionCommand::SetInputGrant(allowed) => {
                    input_allowed.store(allowed, Ordering::Relaxed);
                    let msg = ControlMsg::InputGrant { allowed };
                    if send_control(&mut ws_tx, &mut sealer, &msg).await.is_err() { break 'session; }
                }
                SessionCommand::Kick { reason } => {
                    let _ = send_control(&mut ws_tx, &mut sealer, &ControlMsg::Bye { reason }).await;
                    break 'session;
                }
            },

            msg = in_rx.recv() => {
                let Some(msg) = msg else { break 'session };
                last_recv = Instant::now();
                match msg {
                    ControlMsg::Ping { t0_us } => {
                        let pong = ControlMsg::Pong { t0_us, t1_us: now_us() };
                        if send_control(&mut ws_tx, &mut sealer, &pong).await.is_err() { break 'session; }
                    }
                    ControlMsg::Stats { stats } => {
                        ctl.on_rtt_sample(stats.rtt_ms);
                        ctl.on_viewer_stats(&stats);
                        *handle.stats.lock().unwrap() = stats;
                    }
                    ControlMsg::SetProfile { profile } => {
                        debug!(?profile, "client set profile");
                        ctl.set_profile(profile);
                    }
                    ControlMsg::SetInputMode { mode } => {
                        input_mode = mode;
                    }
                    ControlMsg::RequestKeyframe => {
                        force_keyframe = true;
                    }
                    ControlMsg::Input { events } => {
                        if input_allowed.load(Ordering::Relaxed) && input_mode != InputMode::ViewOnly {
                            input_sink.apply(&events);
                        } else if !events.is_empty() {
                            debug!("dropping {} input events (no grant / view-only)", events.len());
                        }
                    }
                    ControlMsg::Bye { reason } => {
                        info!(client_id, %reason, "client said bye");
                        break 'session;
                    }
                    other => debug!(?other, "ignoring unexpected control message"),
                }
            }

            _ = host_stats_timer.tick() => {
                if last_recv.elapsed() > RECV_TIMEOUT {
                    warn!(client_id, "client unresponsive; closing");
                    break 'session;
                }
                let stats = {
                    let mut hs = state.host_stats.lock().unwrap();
                    hs.target_bitrate_kbps = ctl.bitrate_kbps();
                    hs.encode_ms_avg = encode_ms_avg;
                    let elapsed = bytes_window_start.elapsed().as_secs_f64();
                    if elapsed > 0.5 {
                        hs.actual_bitrate_kbps = ((bytes_window as f64 * 8.0 / 1000.0) / elapsed) as u32;
                        bytes_window = 0;
                        bytes_window_start = Instant::now();
                    }
                    hs.clone()
                };
                let msg = ControlMsg::HostStats { stats };
                if send_control(&mut ws_tx, &mut sealer, &msg).await.is_err() { break 'session; }
            }

            _ = pace => {
                // Grab the newest frame if it advanced past what we sent.
                let frame = {
                    let borrowed = frames.borrow_and_update();
                    match borrowed.as_ref() {
                        Some(f) if f.seq > last_sent_seq => Some(f.clone()),
                        _ => None,
                    }
                };
                if let Some(frame) = frame {
                    last_sent_seq = frame.seq;
                    let t_enc = Instant::now();
                    let fk = force_keyframe;
                    force_keyframe = false;
                    let encoded = tokio::task::block_in_place(|| {
                        encoder.encode(&frame, fk, ctl.bitrate_kbps(), ctl.fps())
                    });
                    let encoded = match encoded {
                        Ok(e) => e,
                        Err(e) => {
                            warn!("encode failed: {e:#}");
                            continue;
                        }
                    };
                    encode_ms_avg = encode_ms_avg * 0.9 + t_enc.elapsed().as_secs_f32() * 1000.0 * 0.1;
                    if encoded.payload.is_empty() {
                        continue; // encoder skipped this frame for rate control
                    }

                    frame_seq = frame_seq.wrapping_add(1);
                    let vf = VideoFrame {
                        codec: encoded.codec,
                        keyframe: encoded.keyframe,
                        seq: frame_seq,
                        timestamp_us: frame.timestamp_us,
                        width: frame.width as u16,
                        height: frame.height as u16,
                        payload: encoded.payload,
                    };
                    let envelope = sealer.seal(Channel::Video, &vf.encode());
                    bytes_window += envelope.len() as u64;
                    let t_send = Instant::now();
                    if ws_tx.send(Message::Binary(envelope.into())).await.is_err() {
                        break 'session;
                    }
                    // Slow sends == TCP backpressure == congestion signal.
                    if t_send.elapsed() > frame_interval + frame_interval / 2 {
                        ctl.on_send_backlog();
                        state.host_stats.lock().unwrap().frames_skipped += 1;
                    } else {
                        state.host_stats.lock().unwrap().frames_sent += 1;
                    }
                    ctl.maybe_increase();
                }
            }
        }
    }

    state.unregister_client(client_id);
    pump.abort();
    let _ = ws_tx.close().await;
    info!(client_id, "session ended");
}

async fn send_control(
    ws_tx: &mut SplitSink<WebSocket, Message>,
    sealer: &mut Sealer,
    msg: &ControlMsg,
) -> Result<(), axum::Error> {
    let json = msg.to_json().expect("control message serialization");
    let envelope = sealer.seal(Channel::Control, json.as_bytes());
    ws_tx.send(Message::Binary(envelope.into())).await
}

/// Reads, decrypts and parses incoming envelopes into control messages.
async fn incoming_pump(
    mut ws_rx: SplitStream<WebSocket>,
    mut opener: Opener,
    out: mpsc::Sender<ControlMsg>,
) {
    while let Some(Ok(msg)) = ws_rx.next().await {
        match msg {
            Message::Binary(data) => match opener.open(&data) {
                Ok((Channel::Control, plaintext)) => {
                    match std::str::from_utf8(&plaintext)
                        .ok()
                        .and_then(|s| ControlMsg::from_json(s).ok())
                    {
                        Some(ctl) => {
                            if out.send(ctl).await.is_err() {
                                return;
                            }
                        }
                        None => warn!("undecodable control payload; dropping"),
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

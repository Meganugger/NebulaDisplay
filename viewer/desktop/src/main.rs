//! NebulaDisplay native desktop viewer.
//!
//! Portable, admin-free, single binary. Connects to a host, decodes
//! H.264/JPEG on a worker thread, presents via softbuffer, forwards
//! mouse/keyboard when an input mode is enabled (I toggles control,
//! F9 toggles listening to the host's audio).
//!
//! Usage:
//!   nebula-viewer --host 192.168.1.20:41800 --pin 123456    # first pairing
//!   nebula-viewer --host 192.168.1.20:41800                 # trusted reconnect

mod audio;
mod decode;
mod net;
mod receive;
mod store;

use clap::Parser;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::window::{Window, WindowId};

use ndsp_protocol::messages::{InputEvent, InputMode};

#[derive(Parser, Debug)]
#[command(
    name = "nebula-viewer",
    version,
    about = "NebulaDisplay desktop viewer"
)]
struct Args {
    /// Host address, e.g. 192.168.1.20:41800 (port optional, default 41800).
    #[arg(long)]
    host: String,
    /// Pairing PIN (only needed the first time).
    #[arg(long)]
    pin: Option<String>,
    /// Name shown on the host's panel.
    #[arg(long, default_value = "Desktop viewer")]
    name: String,
    /// Quality profile: office | video | drawing | gaming
    #[arg(long, default_value = "office")]
    profile: String,
    /// Accept files the host sends, saving them into this directory
    /// (host→viewer file send is declined when unset).
    #[arg(long)]
    receive_dir: Option<std::path::PathBuf>,
}

/// One decoded RGBA frame ready for presentation.
pub struct RgbaFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub timestamp_us: u64,
}

/// Shared between the network/decode thread and the UI thread.
#[derive(Default)]
pub struct Shared {
    pub latest: Mutex<Option<RgbaFrame>>,
    pub status: Mutex<String>,
    pub input_allowed: std::sync::atomic::AtomicBool,
}

#[derive(Debug)]
pub enum UiWake {
    Frame,
    Status,
    Disconnected(String),
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();

    let shared = Arc::new(Shared::default());
    let event_loop: EventLoop<UiWake> = EventLoop::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    // Network + decode worker.
    let (input_tx, input_rx) = std::sync::mpsc::channel::<net::Outgoing>();
    {
        let shared = shared.clone();
        let args_net = net::NetArgs {
            host: args.host.clone(),
            pin: args.pin.clone(),
            name: args.name.clone(),
            receive_dir: args.receive_dir.clone(),
            profile: args.profile.clone(),
        };
        std::thread::spawn(move || net::run(args_net, shared, proxy, input_rx));
    }

    let mut app = App {
        shared,
        window: None,
        surface: None,
        input_tx,
        mode: InputMode::ViewOnly,
        audio_on: false,
        mouse_pos: (0.0, 0.0),
        stream_size: (1280, 720),
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

struct App {
    shared: Arc<Shared>,
    window: Option<Arc<Window>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    input_tx: std::sync::mpsc::Sender<net::Outgoing>,
    mode: InputMode,
    audio_on: bool,
    mouse_pos: (f32, f32),
    stream_size: (u32, u32),
}

impl App {
    fn send_input(&self, ev: InputEvent) {
        if self.mode != InputMode::ViewOnly {
            let _ = self.input_tx.send(net::Outgoing::Input(ev));
        }
    }

    fn title(&self) -> String {
        let status = self.shared.status.lock().unwrap().clone();
        let mode = match self.mode {
            InputMode::ViewOnly => "view-only (press I to control)",
            InputMode::KeyboardMouse => "controlling (press I to stop)",
            _ => "input on",
        };
        format!("NebulaDisplay — {status} — {mode}")
    }

    fn redraw(&mut self) {
        let (Some(window), Some(surface)) = (&self.window, &mut self.surface) else {
            return;
        };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return;
        };
        if surface.resize(w, h).is_err() {
            return;
        }
        let Ok(mut buf) = surface.buffer_mut() else {
            return;
        };
        let guard = self.shared.latest.lock().unwrap();
        if let Some(frame) = guard.as_ref() {
            self.stream_size = (frame.width, frame.height);
            // Nearest-neighbor letterboxed scale RGBA → 0RGB u32.
            let (sw, sh) = (size.width as usize, size.height as usize);
            let (fw, fh) = (frame.width as usize, frame.height as usize);
            let scale = ((sw as f64 / fw as f64).min(sh as f64 / fh as f64)).max(0.0001);
            let (dw, dh) = ((fw as f64 * scale) as usize, (fh as f64 * scale) as usize);
            let (ox, oy) = ((sw - dw) / 2, (sh - dh) / 2);
            buf.fill(0);
            for dy in 0..dh {
                let sy = (dy * fh / dh.max(1)).min(fh - 1);
                let src_row = &frame.rgba[sy * fw * 4..(sy + 1) * fw * 4];
                let dst_row = &mut buf[(oy + dy) * sw + ox..(oy + dy) * sw + ox + dw];
                for (dx, px) in dst_row.iter_mut().enumerate() {
                    let sx = (dx * fw / dw.max(1)).min(fw - 1);
                    let p = &src_row[sx * 4..sx * 4 + 4];
                    *px = ((p[0] as u32) << 16) | ((p[1] as u32) << 8) | (p[2] as u32);
                }
            }
        }
        drop(guard);
        let _ = buf.present();
    }
}

impl ApplicationHandler<UiWake> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("NebulaDisplay — connecting…")
                        .with_inner_size(LogicalSize::new(1280.0, 720.0)),
                )
                .expect("window creation"),
        );
        let context = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        self.surface = Some(softbuffer::Surface::new(&context, window.clone()).expect("surface"));
        self.window = Some(window);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UiWake) {
        match event {
            UiWake::Frame => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            UiWake::Status => {
                if let Some(w) = &self.window {
                    w.set_title(&self.title());
                }
            }
            UiWake::Disconnected(reason) => {
                eprintln!("disconnected: {reason}");
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::CursorMoved { position, .. } => {
                if let Some(window) = &self.window {
                    let size = window.inner_size();
                    // Map through the same letterbox transform as redraw().
                    let (fw, fh) = (self.stream_size.0 as f64, self.stream_size.1 as f64);
                    let scale = (size.width as f64 / fw).min(size.height as f64 / fh);
                    let (dw, dh) = (fw * scale, fh * scale);
                    let ox = (size.width as f64 - dw) / 2.0;
                    let oy = (size.height as f64 - dh) / 2.0;
                    let x = ((position.x - ox) / dw.max(1.0)).clamp(0.0, 1.0) as f32;
                    let y = ((position.y - oy) / dh.max(1.0)).clamp(0.0, 1.0) as f32;
                    self.mouse_pos = (x, y);
                    self.send_input(InputEvent::MouseMove { x, y });
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let button = match button {
                    MouseButton::Left => 0,
                    MouseButton::Middle => 1,
                    MouseButton::Right => 2,
                    MouseButton::Back => 3,
                    MouseButton::Forward => 4,
                    MouseButton::Other(_) => return,
                };
                self.send_input(InputEvent::MouseButton {
                    button,
                    pressed: state == ElementState::Pressed,
                });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x, y),
                    MouseScrollDelta::PixelDelta(p) => ((p.x / 100.0) as f32, (p.y / 100.0) as f32),
                };
                self.send_input(InputEvent::Wheel { dx, dy: -dy });
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let PhysicalKey::Code(code) = event.physical_key else {
                    return;
                };
                // F9 toggles audio locally (never forwarded).
                if code == winit::keyboard::KeyCode::F9
                    && event.state == ElementState::Pressed
                    && !event.repeat
                {
                    self.audio_on = !self.audio_on;
                    let _ = self.input_tx.send(net::Outgoing::SetAudio(self.audio_on));
                    return;
                }
                // I toggles input mode locally (never forwarded).
                if code == winit::keyboard::KeyCode::KeyI
                    && event.state == ElementState::Pressed
                    && !event.repeat
                {
                    self.mode = match self.mode {
                        InputMode::ViewOnly => InputMode::KeyboardMouse,
                        _ => InputMode::ViewOnly,
                    };
                    let _ = self.input_tx.send(net::Outgoing::SetInputMode(self.mode));
                    if let Some(w) = &self.window {
                        w.set_title(&self.title());
                    }
                    return;
                }
                let w3c = keycode_to_w3c(code);
                if let Some(codestr) = w3c {
                    // Layout-resolved character (winit logical key) so the
                    // host can honor the viewer's keyboard layout.
                    let key = match &event.logical_key {
                        winit::keyboard::Key::Character(c) => Some(c.to_string()),
                        _ => None,
                    };
                    self.send_input(InputEvent::Key {
                        code: codestr.to_string(),
                        pressed: event.state == ElementState::Pressed,
                        key,
                    });
                }
            }
            _ => {}
        }
    }
}

/// winit `KeyCode` → W3C `KeyboardEvent.code` string (NDSP's wire format).
/// winit's KeyCode is itself modeled on W3C codes, so this is mostly 1:1.
fn keycode_to_w3c(code: winit::keyboard::KeyCode) -> Option<&'static str> {
    use winit::keyboard::KeyCode as K;
    Some(match code {
        K::KeyA => "KeyA",
        K::KeyB => "KeyB",
        K::KeyC => "KeyC",
        K::KeyD => "KeyD",
        K::KeyE => "KeyE",
        K::KeyF => "KeyF",
        K::KeyG => "KeyG",
        K::KeyH => "KeyH",
        K::KeyI => "KeyI",
        K::KeyJ => "KeyJ",
        K::KeyK => "KeyK",
        K::KeyL => "KeyL",
        K::KeyM => "KeyM",
        K::KeyN => "KeyN",
        K::KeyO => "KeyO",
        K::KeyP => "KeyP",
        K::KeyQ => "KeyQ",
        K::KeyR => "KeyR",
        K::KeyS => "KeyS",
        K::KeyT => "KeyT",
        K::KeyU => "KeyU",
        K::KeyV => "KeyV",
        K::KeyW => "KeyW",
        K::KeyX => "KeyX",
        K::KeyY => "KeyY",
        K::KeyZ => "KeyZ",
        K::Digit0 => "Digit0",
        K::Digit1 => "Digit1",
        K::Digit2 => "Digit2",
        K::Digit3 => "Digit3",
        K::Digit4 => "Digit4",
        K::Digit5 => "Digit5",
        K::Digit6 => "Digit6",
        K::Digit7 => "Digit7",
        K::Digit8 => "Digit8",
        K::Digit9 => "Digit9",
        K::Enter => "Enter",
        K::Space => "Space",
        K::Backspace => "Backspace",
        K::Tab => "Tab",
        K::Escape => "Escape",
        K::ArrowUp => "ArrowUp",
        K::ArrowDown => "ArrowDown",
        K::ArrowLeft => "ArrowLeft",
        K::ArrowRight => "ArrowRight",
        K::Home => "Home",
        K::End => "End",
        K::PageUp => "PageUp",
        K::PageDown => "PageDown",
        K::Insert => "Insert",
        K::Delete => "Delete",
        K::Minus => "Minus",
        K::Equal => "Equal",
        K::BracketLeft => "BracketLeft",
        K::BracketRight => "BracketRight",
        K::Backslash => "Backslash",
        K::Semicolon => "Semicolon",
        K::Quote => "Quote",
        K::Backquote => "Backquote",
        K::Comma => "Comma",
        K::Period => "Period",
        K::Slash => "Slash",
        K::CapsLock => "CapsLock",
        K::ShiftLeft => "ShiftLeft",
        K::ShiftRight => "ShiftRight",
        K::ControlLeft => "ControlLeft",
        K::ControlRight => "ControlRight",
        K::AltLeft => "AltLeft",
        K::AltRight => "AltRight",
        K::SuperLeft => "MetaLeft",
        K::SuperRight => "MetaRight",
        K::F1 => "F1",
        K::F2 => "F2",
        K::F3 => "F3",
        K::F4 => "F4",
        K::F5 => "F5",
        K::F6 => "F6",
        K::F7 => "F7",
        K::F8 => "F8",
        K::F9 => "F9",
        K::F10 => "F10",
        K::F11 => "F11",
        K::F12 => "F12",
        _ => return None,
    })
}

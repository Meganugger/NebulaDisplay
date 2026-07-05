//! Windows input injection via `SendInput`.
//!
//! * Mouse events use absolute coordinates in the 0..65535 virtual-desktop
//!   space `SendInput` expects, computed from the normalized stream
//!   coordinates.
//! * Touch is mapped to mouse in v1 (down → left-button press at position,
//!   move → drag, up → release). Native `InjectTouchInput` multi-touch is a
//!   planned upgrade and slot in behind the same trait.
//! * Stylus maps to mouse move + button, so drawing works everywhere even
//!   before native pen injection (Windows Pointer Injection API) lands.

#![cfg(windows)]

use nebula_proto::{InputEvent, MouseButton, TouchPhase};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{XBUTTON1, XBUTTON2};

use super::{keymap, Injector};

pub struct SendInputInjector {
    /// Track primary-touch state so touch → mouse mapping is stateless for
    /// callers.
    touch_active: Option<u32>,
}

impl SendInputInjector {
    pub fn new() -> Self {
        Self { touch_active: None }
    }

    fn send(&self, inputs: &[INPUT]) -> anyhow::Result<()> {
        let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent as usize != inputs.len() {
            anyhow::bail!("SendInput injected {sent}/{} events", inputs.len());
        }
        Ok(())
    }

    fn mouse(&self, flags: MOUSE_EVENT_FLAGS, x: f64, y: f64, data: i32) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: (x.clamp(0.0, 1.0) * 65535.0) as i32,
                    dy: (y.clamp(0.0, 1.0) * 65535.0) as i32,
                    mouseData: data as u32,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn key(&self, vk: VIRTUAL_KEY, down: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: if down {
                        Default::default()
                    } else {
                        KEYEVENTF_KEYUP
                    },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn move_abs(&self, x: f64, y: f64) -> INPUT {
        self.mouse(
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            x,
            y,
            0,
        )
    }
}

impl Injector for SendInputInjector {
    fn inject(&mut self, event: &InputEvent) -> anyhow::Result<()> {
        match event {
            InputEvent::MouseMove { x, y } => self.send(&[self.move_abs(*x, *y)]),
            InputEvent::MouseButton { button, down, x, y } => {
                let (flags, data) = match (button, down) {
                    (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                    (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                    (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                    (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                    (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                    (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                    (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1 as i32),
                    (MouseButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1 as i32),
                    (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2 as i32),
                    (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2 as i32),
                };
                self.send(&[
                    self.move_abs(*x, *y),
                    self.mouse(
                        flags | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                        *x,
                        *y,
                        data,
                    ),
                ])
            }
            InputEvent::MouseWheel { dx, dy } => {
                let mut inputs = Vec::with_capacity(2);
                if dy.abs() > f64::EPSILON {
                    inputs.push(self.mouse(MOUSEEVENTF_WHEEL, 0.0, 0.0, (-dy * 120.0) as i32));
                }
                if dx.abs() > f64::EPSILON {
                    inputs.push(self.mouse(MOUSEEVENTF_HWHEEL, 0.0, 0.0, (dx * 120.0) as i32));
                }
                if inputs.is_empty() {
                    return Ok(());
                }
                self.send(&inputs)
            }
            InputEvent::Key { code, down } => match keymap::code_to_vk(code) {
                Some(vk) => self.send(&[self.key(vk, *down)]),
                None => {
                    tracing::debug!("no VK mapping for key code '{code}'");
                    Ok(())
                }
            },
            InputEvent::Touch {
                id, phase, x, y, ..
            } => {
                // v1: primary-touch → mouse mapping (see module docs).
                match phase {
                    TouchPhase::Down if self.touch_active.is_none() => {
                        self.touch_active = Some(*id);
                        self.send(&[
                            self.move_abs(*x, *y),
                            self.mouse(
                                MOUSEEVENTF_LEFTDOWN
                                    | MOUSEEVENTF_ABSOLUTE
                                    | MOUSEEVENTF_VIRTUALDESK,
                                *x,
                                *y,
                                0,
                            ),
                        ])
                    }
                    TouchPhase::Move if self.touch_active == Some(*id) => {
                        self.send(&[self.move_abs(*x, *y)])
                    }
                    TouchPhase::Up | TouchPhase::Cancel if self.touch_active == Some(*id) => {
                        self.touch_active = None;
                        self.send(&[self.mouse(
                            MOUSEEVENTF_LEFTUP | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                            *x,
                            *y,
                            0,
                        )])
                    }
                    _ => Ok(()), // secondary touches ignored in v1
                }
            }
            InputEvent::Stylus { x, y, down, .. } => {
                let mut inputs = vec![self.move_abs(*x, *y)];
                // Track pen contact transitions via touch_active slot id max.
                const PEN_ID: u32 = u32::MAX;
                match (down, self.touch_active == Some(PEN_ID)) {
                    (true, false) => {
                        self.touch_active = Some(PEN_ID);
                        inputs.push(self.mouse(
                            MOUSEEVENTF_LEFTDOWN | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                            *x,
                            *y,
                            0,
                        ));
                    }
                    (false, true) => {
                        self.touch_active = None;
                        inputs.push(self.mouse(
                            MOUSEEVENTF_LEFTUP | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                            *x,
                            *y,
                            0,
                        ));
                    }
                    _ => {}
                }
                self.send(&inputs)
            }
        }
    }

    fn describe(&self) -> String {
        "Windows SendInput injector".into()
    }
}

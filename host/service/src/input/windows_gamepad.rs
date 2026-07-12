//! Gamepad forwarding via `Windows.UI.Input.Preview.Injection`.
//!
//! Viewers poll the W3C Gamepad API ("standard" mapping) and send state
//! snapshots; this injects them as a virtual gamepad the OS exposes through
//! `Windows.Gaming.Input`. Kernel-driver approaches (ViGEm-style) are out of
//! clean-room scope — apps reading raw XInput/DirectInput won't see this
//! device, but UWP/GameInput consumers will.

use ndsp_protocol::messages::InputEvent;
use windows::Gaming::Input::GamepadButtons;
use windows::UI::Input::Preview::Injection::{InjectedInputGamepadInfo, InputInjector};

pub struct GamepadInjector {
    injector: InputInjector,
}

// COM agility: InputInjector is used only behind the sink's Mutex.
unsafe impl Send for GamepadInjector {}

/// W3C standard-mapping button index (bit in `InputEvent::Gamepad.buttons`)
/// → `GamepadButtons` flag. Triggers (bits 6/7) travel as analog values.
const BUTTON_MAP: &[(u32, u32)] = &[
    (0, GamepadButtons::A.0),
    (1, GamepadButtons::B.0),
    (2, GamepadButtons::X.0),
    (3, GamepadButtons::Y.0),
    (4, GamepadButtons::LeftShoulder.0),
    (5, GamepadButtons::RightShoulder.0),
    (8, GamepadButtons::View.0),
    (9, GamepadButtons::Menu.0),
    (10, GamepadButtons::LeftThumbstick.0),
    (11, GamepadButtons::RightThumbstick.0),
    (12, GamepadButtons::DPadUp.0),
    (13, GamepadButtons::DPadDown.0),
    (14, GamepadButtons::DPadLeft.0),
    (15, GamepadButtons::DPadRight.0),
];

impl GamepadInjector {
    pub fn new() -> anyhow::Result<Self> {
        let injector =
            InputInjector::TryCreate().map_err(|e| anyhow::anyhow!("InputInjector: {e}"))?;
        injector
            .InitializeGamepadInjection()
            .map_err(|e| anyhow::anyhow!("InitializeGamepadInjection: {e}"))?;
        Ok(Self { injector })
    }

    pub fn inject(&self, ev: &InputEvent) {
        let InputEvent::Gamepad {
            buttons,
            left_x,
            left_y,
            right_x,
            right_y,
            left_trigger,
            right_trigger,
        } = ev
        else {
            return;
        };
        let mut flags = 0u32;
        for &(bit, flag) in BUTTON_MAP {
            if buttons & (1 << bit) != 0 {
                flags |= flag;
            }
        }
        let info = match InjectedInputGamepadInfo::new() {
            Ok(i) => i,
            Err(e) => {
                tracing::debug!("gamepad info alloc failed: {e}");
                return;
            }
        };
        let clamp1 = |v: f32| v.clamp(-1.0, 1.0) as f64;
        let clamp01 = |v: f32| v.clamp(0.0, 1.0) as f64;
        let _ = info.SetButtons(GamepadButtons(flags));
        let _ = info.SetLeftThumbstickX(clamp1(*left_x));
        // Gamepad API Y axes are inverted relative to Gaming.Input.
        let _ = info.SetLeftThumbstickY(clamp1(-*left_y));
        let _ = info.SetRightThumbstickX(clamp1(*right_x));
        let _ = info.SetRightThumbstickY(clamp1(-*right_y));
        let _ = info.SetLeftTrigger(clamp01(*left_trigger));
        let _ = info.SetRightTrigger(clamp01(*right_trigger));
        if let Err(e) = self.injector.InjectGamepadInput(&info) {
            tracing::debug!("gamepad injection failed: {e}");
        }
    }
}

impl Drop for GamepadInjector {
    fn drop(&mut self) {
        let _ = self.injector.UninitializeGamepadInjection();
    }
}

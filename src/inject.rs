//! Synthetic input.
//!
//! Every event we emit is tagged in `dwExtraInfo` with `TAG`, so the capture
//! side can tell our own output apart from a real human press. Without that a
//! trigger bound to left-click would retrigger itself forever.
//!
//! Honest limitation: `SendInput` events are visible to applications as
//! injected (they arrive with a null raw-input device handle), and plenty of
//! games filter exactly that. Making output indistinguishable from real
//! hardware needs a virtual HID device, which means a signed kernel driver.

use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    SendInput, VIRTUAL_KEY,
};

/// "HIDF" -- marks an event as ours.
pub const TAG: usize = 0x4849_4446;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Btn {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

impl Btn {
    /// Kept as the canonical button set for UI pickers.
    #[allow(dead_code)]
    pub const ALL: [Btn; 5] = [Btn::Left, Btn::Right, Btn::Middle, Btn::X1, Btn::X2];

    pub fn name(self) -> &'static str {
        match self {
            Btn::Left => "Left",
            Btn::Right => "Right",
            Btn::Middle => "Middle",
            Btn::X1 => "X1 (back)",
            Btn::X2 => "X2 (fwd)",
        }
    }

    fn event(self, down: bool) -> (MOUSE_EVENT_FLAGS, u32) {
        match self {
            Btn::Left => (
                if down {
                    MOUSEEVENTF_LEFTDOWN
                } else {
                    MOUSEEVENTF_LEFTUP
                },
                0,
            ),
            Btn::Right => (
                if down {
                    MOUSEEVENTF_RIGHTDOWN
                } else {
                    MOUSEEVENTF_RIGHTUP
                },
                0,
            ),
            Btn::Middle => (
                if down {
                    MOUSEEVENTF_MIDDLEDOWN
                } else {
                    MOUSEEVENTF_MIDDLEUP
                },
                0,
            ),
            Btn::X1 => (
                if down {
                    MOUSEEVENTF_XDOWN
                } else {
                    MOUSEEVENTF_XUP
                },
                1,
            ),
            Btn::X2 => (
                if down {
                    MOUSEEVENTF_XDOWN
                } else {
                    MOUSEEVENTF_XUP
                },
                2,
            ),
        }
    }
}

#[inline]
fn send_mouse(flags: MOUSE_EVENT_FLAGS, data: u32, dx: i32, dy: i32) {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: TAG,
            },
        },
    };
    unsafe {
        SendInput(&[input], core::mem::size_of::<INPUT>() as i32);
    }
}

#[inline]
pub fn mouse(btn: Btn, down: bool) {
    let (flags, data) = btn.event(down);
    send_mouse(flags, data, 0, 0);
}

#[inline]
pub fn move_rel(dx: i32, dy: i32) {
    send_mouse(MOUSEEVENTF_MOVE, 0, dx, dy);
}

#[inline]
pub fn wheel(delta: i32) {
    send_mouse(MOUSEEVENTF_WHEEL, delta as u32, 0, 0);
}

/// A zero-delta move: costs exactly what a real click event costs but changes
/// nothing. Used by the engine benchmark so it can measure the full loop
/// including injection without spraying clicks at the desktop.
#[inline]
pub fn nop() {
    send_mouse(MOUSEEVENTF_MOVE, 0, 0, 0);
}

/// Send by scancode rather than virtual key. Games reading DirectInput or raw
/// scancodes often ignore VK-only events, so scancode is the safer default.
#[inline]
pub fn key_scan(scancode: u16, down: bool) {
    let mut flags = KEYEVENTF_SCANCODE;
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    send_key(VIRTUAL_KEY(0), scancode, flags);
}

#[inline]
pub fn key_vk(vk: u16, down: bool) {
    let flags = if down {
        KEYBD_EVENT_FLAGS(0)
    } else {
        KEYEVENTF_KEYUP
    };
    send_key(VIRTUAL_KEY(vk), 0, flags);
}

#[inline]
fn send_key(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) {
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: TAG,
            },
        },
    };
    unsafe {
        SendInput(&[input], core::mem::size_of::<INPUT>() as i32);
    }
}

//! Device discovery and button learning.
//!
//! The problem this solves: you cannot assume "button 4 is the side button."
//! Only the five standard buttons have a fixed place in `RAWMOUSE`. Everything
//! past that arrives on whatever channel the vendor chose -- a keyboard
//! scancode, a Consumer-page usage, or a vendor-defined page -- and on some
//! mice the extra buttons emit nothing at all until the OEM software has
//! flashed them.
//!
//! So instead of guessing, we *learn*. For generic HID reports we keep the
//! previous report per device and XOR against it: every bit that flips is a
//! candidate press, identified by (device, byte, bit). That finds buttons on
//! any device without parsing a single HID report descriptor.
//!
//! Hard constraint worth knowing: Raw Input can *identify* the device but
//! cannot *block* the event. Binding a button as a macro trigger works fine.
//! Stopping the original click from also reaching the foreground app needs
//! either a `WH_MOUSE_LL` hook (standard buttons only) or HIDHide.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

/// Diagnostics. These exist because "no events surfaced" has two very different
/// causes -- a working filter, or a dead pipeline -- and a test that cannot tell
/// them apart is worse than no test.
pub static RAW_MSGS: AtomicU64 = AtomicU64::new(0);
pub static INJECTED_DROPPED: AtomicU64 = AtomicU64::new(0);
pub static SURFACED: AtomicU64 = AtomicU64::new(0);
pub static REG_OK: AtomicU64 = AtomicU64::new(0);
pub static REG_FAIL: AtomicU64 = AtomicU64::new(0);
pub static WINDOW_OK: AtomicBool = AtomicBool::new(false);

pub fn diag() -> String {
    format!(
        "window={} reg_ok={} reg_fail={} wm_input={} injected_dropped={} surfaced={}",
        WINDOW_OK.load(Ordering::Relaxed),
        REG_OK.load(Ordering::Relaxed),
        REG_FAIL.load(Ordering::Relaxed),
        RAW_MSGS.load(Ordering::Relaxed),
        INJECTED_DROPPED.load(Ordering::Relaxed),
        SURFACED.load(Ordering::Relaxed),
    )
}

use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::{
    GetRawInputData, GetRawInputDeviceInfoW, GetRawInputDeviceList, HRAWINPUT, RAWINPUT,
    RAWINPUTDEVICE, RAWINPUTDEVICE_FLAGS, RAWINPUTDEVICELIST, RAWINPUTHEADER, RID_DEVICE_INFO,
    RID_INPUT, RIDEV_INPUTSINK, RIDI_DEVICEINFO, RIDI_DEVICENAME, RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, HWND_MESSAGE, MSG,
    PostMessageW, PostQuitMessage, RegisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_INPUT,
    WM_INPUT_DEVICE_CHANGE, WNDCLASSW,
};
use windows::core::PCWSTR;

const RIDEV_REMOVE: RAWINPUTDEVICE_FLAGS = RAWINPUTDEVICE_FLAGS(0x0000_0001);
const RIDEV_DEVNOTIFY: RAWINPUTDEVICE_FLAGS = RAWINPUTDEVICE_FLAGS(0x0000_2000);

const MSG_SET_CAPTURE: u32 = WM_APP + 1;
const MSG_QUIT: u32 = WM_APP + 2;

/// A pressable thing, identified by whatever channel it actually arrives on.
/// This is what a macro trigger binds to.
#[derive(Clone, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
pub enum Signal {
    /// One of the five standard buttons in `RAWMOUSE`, per device.
    MouseButton { device: i64, index: u8 },
    /// Wheel notch. `up` distinguishes direction.
    Wheel { device: i64, up: bool },
    /// A key, identified by scancode (stable across layouts) plus VK for display.
    Key { device: i64, scancode: u16, vk: u16 },
    /// A bit in a generic HID report -- this is the catch-all that finds side
    /// buttons, DPI buttons and vendor-specific extras.
    HidBit {
        device: i64,
        page: u16,
        usage: u16,
        byte: u16,
        bit: u8,
    },
}

/// HID usage pages we refuse to listen to.
///
/// 0x0D is Digitizer -- touchscreens and precision touchpads. They stream
/// contact coordinates continuously, so a report bit-diff turns every pixel of
/// finger movement into a phantom press. On this machine four collections sit on
/// that page and between them they drown out every real button. They are pointing
/// devices; they are never macro buttons.
pub fn page_is_noise(page: u16) -> bool {
    page == 0x000D
}

impl Signal {
    pub fn device(&self) -> i64 {
        match *self {
            Signal::MouseButton { device, .. }
            | Signal::Wheel { device, .. }
            | Signal::Key { device, .. }
            | Signal::HidBit { device, .. } => device,
        }
    }

    /// Short human label. Deliberately concrete about the channel, because
    /// "Button 4" and "keyboard scancode 0x05 from the mouse" behave very
    /// differently and the user needs to see which one they bound.
    pub fn label(&self) -> String {
        match *self {
            // Name buttons 4/5 by what they physically are. "Button 4" is
            // meaningless next to an OEM tool that numbers the same switch 8.
            Signal::MouseButton { index, .. } => match index {
                1 => "Left click".into(),
                2 => "Right click".into(),
                3 => "Middle click".into(),
                4 => "Side back (X1)".into(),
                5 => "Side forward (X2)".into(),
                n => format!("Button {n}"),
            },
            Signal::Wheel { up, .. } => {
                if up {
                    "Wheel up".into()
                } else {
                    "Wheel down".into()
                }
            }
            Signal::Key { scancode, vk, .. } => {
                format!("Key sc{scancode:#04x} vk{vk:#04x}")
            }
            Signal::HidBit {
                page,
                usage,
                byte,
                bit,
                ..
            } => format!("HID {page:#06x}/{usage:#04x} byte{byte}.bit{bit}"),
        }
    }

    /// Whether a `WH_MOUSE_LL` hook could suppress the original event. Only the
    /// five standard buttons and the wheel go through that hook. Unused until
    /// suppression is implemented; kept because it documents the boundary.
    #[allow(dead_code)]
    pub fn suppressible(&self) -> bool {
        matches!(self, Signal::MouseButton { .. } | Signal::Wheel { .. })
    }
}

#[derive(Clone, Debug)]
pub struct Event {
    pub signal: Signal,
    pub down: bool,
    pub qpc: i64,
}

#[derive(Clone, Debug)]
pub struct Device {
    pub handle: i64,
    pub kind: &'static str,
    pub path: String,
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub buttons: u32,
    pub usage_page: u16,
    pub usage: u16,
}

impl Device {
    pub fn label(&self) -> String {
        match (self.vid, self.pid) {
            (Some(v), Some(p)) => format!("{} {:04X}:{:04X}", self.kind, v, p),
            _ => format!("{} (built-in)", self.kind),
        }
    }
}

pub enum Msg {
    Devices(Vec<Device>),
    Input(Event),
}

pub struct RawInput {
    hwnd: i64,
    pub rx: Receiver<Msg>,
    pub devices: Vec<Device>,
    capture: bool,
}

impl RawInput {
    pub fn start(ctx: egui::Context) -> Self {
        let (tx, rx) = channel();
        let (hwnd_tx, hwnd_rx) = channel();

        std::thread::Builder::new()
            .name("hidforge-rawinput".into())
            .spawn(move || pump(tx, hwnd_tx, ctx))
            .expect("spawn rawinput thread");

        let hwnd = hwnd_rx.recv().unwrap_or(0);
        let mut me = Self {
            hwnd,
            rx,
            devices: Vec::new(),
            capture: false,
        };
        me.devices = enumerate();
        me
    }

    pub fn capturing(&self) -> bool {
        self.capture
    }

    /// Capture is off by default and only switched on while binding or while a
    /// trigger is armed. A 1000 Hz mouse otherwise generates 1000 `WM_INPUT`
    /// messages a second for motion we do not care about -- small, but not free,
    /// and this app is supposed to be cheap on weak hardware.
    pub fn set_capture(&mut self, on: bool) {
        if self.capture == on || self.hwnd == 0 {
            return;
        }
        self.capture = on;
        unsafe {
            let _ = PostMessageW(
                Some(HWND(self.hwnd as *mut _)),
                MSG_SET_CAPTURE,
                WPARAM(on as usize),
                LPARAM(0),
            );
        }
    }

    /// Drain pending messages. Returns input events; device lists are absorbed.
    pub fn drain(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Devices(d) => self.devices = d,
                Msg::Input(e) => out.push(e),
            }
        }
        out
    }
}

impl Drop for RawInput {
    fn drop(&mut self) {
        if self.hwnd != 0 {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(self.hwnd as *mut _)),
                    MSG_QUIT,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }
}

// ------------------------------------------------------------- enumeration

fn parse_vid_pid(path: &str) -> (Option<u16>, Option<u16>) {
    let up = path.to_ascii_uppercase();
    let get = |key: &str| {
        up.find(key).and_then(|i| {
            let s = &up[i + key.len()..];
            let hex: String = s.chars().take(4).collect();
            u16::from_str_radix(&hex, 16).ok()
        })
    };
    (get("VID_"), get("PID_"))
}

pub fn enumerate() -> Vec<Device> {
    let mut n: u32 = 0;
    let sz = core::mem::size_of::<RAWINPUTDEVICELIST>() as u32;
    unsafe { GetRawInputDeviceList(None, &mut n, sz) };
    if n == 0 {
        return Vec::new();
    }
    let mut list = vec![RAWINPUTDEVICELIST::default(); n as usize];
    let got = unsafe { GetRawInputDeviceList(Some(list.as_mut_ptr()), &mut n, sz) };
    if got == u32::MAX {
        return Vec::new();
    }
    list.truncate(got as usize);

    let mut out = Vec::new();
    for e in list {
        let h = e.hDevice;
        let path = device_path(h);
        let (vid, pid) = parse_vid_pid(&path);
        let kind = match e.dwType.0 {
            0 => "Mouse",
            1 => "Keyboard",
            _ => "HID",
        };
        let mut d = Device {
            handle: h.0 as i64,
            kind,
            path,
            vid,
            pid,
            buttons: 0,
            usage_page: 0,
            usage: 0,
        };
        let mut info = RID_DEVICE_INFO {
            cbSize: core::mem::size_of::<RID_DEVICE_INFO>() as u32,
            ..Default::default()
        };
        let mut isz = info.cbSize;
        let r = unsafe {
            GetRawInputDeviceInfoW(
                Some(h),
                RIDI_DEVICEINFO,
                Some(&mut info as *mut _ as *mut _),
                &mut isz,
            )
        };
        if r != u32::MAX {
            unsafe {
                match e.dwType.0 {
                    0 => d.buttons = info.Anonymous.mouse.dwNumberOfButtons,
                    1 => d.buttons = info.Anonymous.keyboard.dwNumberOfKeysTotal,
                    _ => {
                        d.usage_page = info.Anonymous.hid.usUsagePage;
                        d.usage = info.Anonymous.hid.usUsage;
                    }
                }
            }
        }
        out.push(d);
    }
    out
}

fn device_path(h: HANDLE) -> String {
    let mut n: u32 = 0;
    unsafe { GetRawInputDeviceInfoW(Some(h), RIDI_DEVICENAME, None, &mut n) };
    if n == 0 || n > 4096 {
        return String::new();
    }
    let mut buf = vec![0u16; n as usize + 1];
    let r = unsafe {
        GetRawInputDeviceInfoW(
            Some(h),
            RIDI_DEVICENAME,
            Some(buf.as_mut_ptr() as *mut _),
            &mut n,
        )
    };
    if r == u32::MAX {
        return String::new();
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

// ------------------------------------------------------------- message pump

/// Per-thread state, reachable from the window procedure.
struct Pump {
    tx: Sender<Msg>,
    ctx: egui::Context,
    last: HashMap<i64, Vec<u8>>,
    /// device handle -> (usage page, usage), so a HID bit can be labelled with
    /// the page it actually came from instead of a hardcoded zero.
    pages: HashMap<i64, (u16, u16)>,
    /// Fingerprint of the device set at the time we last registered.
    reg_sig: u64,
    buf: Vec<u8>,
}

impl Pump {
    fn refresh_pages(&mut self) {
        self.pages.clear();
        for d in enumerate() {
            self.pages.insert(d.handle, (d.usage_page, d.usage));
        }
    }
}

thread_local! {
    static PUMP: std::cell::RefCell<Option<Pump>> = const { std::cell::RefCell::new(None) };
}

fn pump(tx: Sender<Msg>, hwnd_tx: Sender<i64>, ctx: egui::Context) {
    unsafe {
        let hinst = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let class: Vec<u16> = "HidForgeRawInput\0".encode_utf16().collect();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wc);

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(class.as_ptr()),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinst.into()),
            None,
        );

        let hwnd = match hwnd {
            Ok(h) => h,
            Err(_) => {
                let _ = hwnd_tx.send(0);
                return;
            }
        };

        PUMP.with(|p| {
            *p.borrow_mut() = Some(Pump {
                tx,
                ctx,
                last: HashMap::new(),
                pages: HashMap::new(),
                reg_sig: 0,
                // Generous: an oversized HID report makes GetRawInputData fail
                // outright, which silently drops that device's events.
                buf: vec![0u8; 8192],
            })
        });

        WINDOW_OK.store(true, Ordering::Relaxed);
        let _ = hwnd_tx.send(hwnd.0 as i64);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = DispatchMessageW(&msg);
        }
    }
}

/// Build the registration list: mouse, keyboard, consumer page, plus every HID
/// collection currently attached. That last part is what catches vendor pages.
fn registration_set(hwnd: HWND, remove: bool) -> Vec<RAWINPUTDEVICE> {
    let mut pairs: Vec<(u16, u16)> = vec![(0x01, 0x02), (0x01, 0x06), (0x0C, 0x01)];
    for d in enumerate() {
        if d.kind == "HID" && d.usage_page != 0 && !page_is_noise(d.usage_page) {
            let p = (d.usage_page, d.usage);
            if !pairs.contains(&p) {
                pairs.push(p);
            }
        }
    }
    pairs
        .into_iter()
        .map(|(page, usage)| RAWINPUTDEVICE {
            usUsagePage: page,
            usUsage: usage,
            dwFlags: if remove {
                RIDEV_REMOVE
            } else {
                RIDEV_INPUTSINK | RIDEV_DEVNOTIFY
            },
            // RIDEV_REMOVE requires a null target.
            hwndTarget: if remove {
                HWND(std::ptr::null_mut())
            } else {
                hwnd
            },
        })
        .collect()
}

/// Re-entrancy guard. Registering emits device-change notifications, so without
/// this the handler for those notifications can call back into registration.
static REGISTERING: AtomicBool = AtomicBool::new(false);

/// Cheap fingerprint of the attached device set: if this is unchanged there is
/// nothing new to register, so a device-change notification can be ignored.
fn device_signature(devs: &[Device]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for d in devs {
        for b in d.handle.to_le_bytes() {
            h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
        }
        for b in d
            .usage_page
            .to_le_bytes()
            .iter()
            .chain(d.usage.to_le_bytes().iter())
        {
            h = (h ^ *b as u64).wrapping_mul(0x100_0000_01b3);
        }
    }
    h
}

/// Register (or remove) the whole set in ONE call, which is the documented
/// pattern. Registering pair-by-pair also works but makes a partial failure much
/// harder to see, and we want `reg_fail` to mean something.
fn register_all(hwnd: HWND, remove: bool) {
    let devs = registration_set(hwnd, remove);
    let sz = core::mem::size_of::<RAWINPUTDEVICE>() as u32;
    match unsafe { RegisterRawInputDevices(&devs, sz) } {
        Ok(()) => {
            REG_OK.fetch_add(devs.len() as u64, Ordering::Relaxed);
        }
        Err(_) => {
            // Fall back to one-at-a-time so one rejected usage pair does not cost
            // us every other device.
            for d in &devs {
                if unsafe { RegisterRawInputDevices(core::slice::from_ref(d), sz) }.is_ok() {
                    REG_OK.fetch_add(1, Ordering::Relaxed);
                } else {
                    REG_FAIL.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        MSG_SET_CAPTURE => {
            let on = wp.0 != 0;
            if !REGISTERING.swap(true, Ordering::SeqCst) {
                register_all(hwnd, !on);
                REGISTERING.store(false, Ordering::SeqCst);
            }
            PUMP.with(|p| {
                if let Some(pp) = p.borrow_mut().as_mut() {
                    let devs = enumerate();
                    pp.reg_sig = device_signature(&devs);
                    pp.refresh_pages();
                    if on {
                        let _ = pp.tx.send(Msg::Devices(devs));
                    }
                }
            });
            LRESULT(0)
        }
        MSG_QUIT => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        WM_INPUT_DEVICE_CHANGE => {
            // DO NOT re-register unconditionally here.
            //
            // RIDEV_DEVNOTIFY means registering *itself* produces
            // WM_INPUT_DEVICE_CHANGE messages. Re-registering in response is a
            // feedback loop: it ran ~160k registrations in three seconds, pinned
            // this thread, and starved WM_INPUT so real button presses were
            // almost never processed. That was the actual cause of "keys only
            // register if I keep pressing", the random hits, and bindings never
            // taking.
            //
            // So: only re-register when the set of usage pairs genuinely changed
            // (a new device with a new vendor page), and never re-enter.
            PUMP.with(|p| {
                if let Some(pp) = p.borrow_mut().as_mut() {
                    let devs = enumerate();
                    let sig = device_signature(&devs);
                    let changed = pp.reg_sig != sig;
                    pp.refresh_pages();
                    let _ = pp.tx.send(Msg::Devices(devs));
                    pp.ctx.request_repaint();
                    if changed && !REGISTERING.swap(true, Ordering::SeqCst) {
                        pp.reg_sig = sig;
                        register_all(hwnd, false);
                        REGISTERING.store(false, Ordering::SeqCst);
                    }
                }
            });
            LRESULT(0)
        }
        WM_INPUT => {
            PUMP.with(|p| {
                if let Some(pp) = p.borrow_mut().as_mut() {
                    unsafe { on_input(pp, HRAWINPUT(lp.0 as *mut _)) };
                }
            });
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

unsafe fn on_input(p: &mut Pump, h: HRAWINPUT) {
    let hdr = core::mem::size_of::<RAWINPUTHEADER>() as u32;
    let mut size: u32 = p.buf.len() as u32;
    let got = unsafe {
        GetRawInputData(
            h,
            RID_INPUT,
            Some(p.buf.as_mut_ptr() as *mut _),
            &mut size,
            hdr,
        )
    };
    if got == u32::MAX || got == 0 {
        return;
    }
    RAW_MSGS.fetch_add(1, Ordering::Relaxed);

    let ri = unsafe { &*(p.buf.as_ptr() as *const RAWINPUT) };
    let dev = ri.header.hDevice.0 as i64;

    // Injected input arrives with a null device handle. Dropping it is what
    // stops our own output from retriggering a binding or inflating a CPS test.
    if dev == 0 {
        INJECTED_DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    }

    let now = crate::clock::qpc();
    let emit = |signal: Signal, down: bool| {
        SURFACED.fetch_add(1, Ordering::Relaxed);
        let _ = p.tx.send(Msg::Input(Event {
            signal,
            down,
            qpc: now,
        }));
    };
    let mut any = false;

    match ri.header.dwType {
        0 => {
            let m = unsafe { &ri.data.mouse };
            let f = unsafe { m.Anonymous.Anonymous.usButtonFlags };
            const DOWN_UP: [(u16, u16, u8); 5] = [
                (0x0001, 0x0002, 1),
                (0x0004, 0x0008, 2),
                (0x0010, 0x0020, 3),
                (0x0040, 0x0080, 4),
                (0x0100, 0x0200, 5),
            ];
            for (dn, up, idx) in DOWN_UP {
                if f & dn != 0 {
                    emit(
                        Signal::MouseButton {
                            device: dev,
                            index: idx,
                        },
                        true,
                    );
                    any = true;
                }
                if f & up != 0 {
                    emit(
                        Signal::MouseButton {
                            device: dev,
                            index: idx,
                        },
                        false,
                    );
                    any = true;
                }
            }
            if f & 0x0400 != 0 {
                let delta = unsafe { m.Anonymous.Anonymous.usButtonData } as i16;
                let s = Signal::Wheel {
                    device: dev,
                    up: delta > 0,
                };
                // A wheel notch has no physical release, so emit a down/up pair.
                // Emitting only DOWN (as this used to) meant a trigger bound to
                // the wheel armed and never disarmed -- 74 "Wheel down" events in
                // the probe with not one release among them.
                emit(s.clone(), true);
                emit(s, false);
                any = true;
            }
            // Pure motion falls through with `any == false`: no event, no
            // repaint, no cost beyond this branch.
        }
        1 => {
            let k = unsafe { &ri.data.keyboard };
            // 0xFF make-code is a driver overrun marker, not a key.
            if k.MakeCode != 0xFF {
                let down = k.Flags & 0x01 == 0;
                emit(
                    Signal::Key {
                        device: dev,
                        scancode: k.MakeCode,
                        vk: k.VKey,
                    },
                    down,
                );
                any = true;
            }
        }
        _ => {
            // Second line of defence: even if a digitizer somehow got registered,
            // never turn its coordinate churn into button presses.
            let (page, usage) = p.pages.get(&dev).copied().unwrap_or((0, 0));
            if page_is_noise(page) {
                return;
            }

            let hid = unsafe { &ri.data.hid };
            let len = (hid.dwSizeHid as usize).saturating_mul(hid.dwCount as usize);
            if len == 0 || len > 4096 {
                return;
            }
            let base = (&hid.bRawData) as *const u8;
            let cur = unsafe { core::slice::from_raw_parts(base, len) };

            let prev = p.last.entry(dev).or_insert_with(|| vec![0u8; len]);
            if prev.len() != len {
                prev.resize(len, 0);
            }

            // XOR the report against the previous one: every flipped bit is a
            // candidate button edge. No HID descriptor parsing required, which
            // is why this works on devices we have never seen.
            for i in 0..len {
                let diff = prev[i] ^ cur[i];
                if diff == 0 {
                    continue;
                }
                for bit in 0..8u8 {
                    if diff & (1 << bit) != 0 {
                        let down = cur[i] & (1 << bit) != 0;
                        emit(
                            Signal::HidBit {
                                device: dev,
                                page,
                                usage,
                                byte: i as u16,
                                bit,
                            },
                            down,
                        );
                        any = true;
                    }
                }
            }
            prev.copy_from_slice(cur);
        }
    }

    if any {
        p.ctx.request_repaint();
    }
}

//! System tray icon.
//!
//! The app is meant to sit out of the way with macros still armed, so closing the
//! window hides it instead of quitting. Runs its own message-only window on its
//! own thread; requests reach the UI through atomics plus `request_repaint`,
//! because a hidden viewport produces no frames on its own.

use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DefWindowProcW, DestroyMenu, DispatchMessageW, GetCursorPos,
    GetMessageW, HMENU, HWND_MESSAGE, IDI_APPLICATION, LoadIconW, MF_SEPARATOR, MF_STRING, MSG,
    RegisterClassW, SetForegroundWindow, TPM_BOTTOMALIGN, TPM_RIGHTALIGN, TrackPopupMenu,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_COMMAND, WM_LBUTTONUP, WM_RBUTTONDBLCLK,
    WM_RBUTTONUP, WNDCLASSW,
};
use windows::core::PCWSTR;

const MSG_TRAY: u32 = WM_APP + 20;
const ID_SHOW: usize = 1;
const ID_STOP: usize = 2;
const ID_QUIT: usize = 3;

/// Requests raised by the tray, consumed by the UI thread each frame.
pub static WANT_SHOW: AtomicBool = AtomicBool::new(false);
pub static WANT_STOP: AtomicBool = AtomicBool::new(false);
pub static WANT_QUIT: AtomicBool = AtomicBool::new(false);
pub static ACTIVE: AtomicBool = AtomicBool::new(false);

thread_local! {
    static CTX: std::cell::RefCell<Option<egui::Context>> =
        const { std::cell::RefCell::new(None) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn start(ctx: egui::Context) {
    std::thread::Builder::new()
        .name("hidforge-tray".into())
        .spawn(move || run(ctx))
        .ok();
}

fn run(ctx: egui::Context) {
    unsafe {
        let hinst = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let class = wide("HidForgeTray");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wc);

        let Ok(hwnd) = create_message_window(&class, hinst) else {
            return;
        };

        CTX.with(|c| *c.borrow_mut() = Some(ctx));

        let mut nid = NOTIFYICONDATAW {
            cbSize: core::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: MSG_TRAY,
            hIcon: LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
            ..Default::default()
        };
        let tip = wide("HidForge — click to open");
        nid.szTip[..tip.len().min(127)].copy_from_slice(&tip[..tip.len().min(127)]);

        if Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
            ACTIVE.store(true, Ordering::Relaxed);
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = DispatchMessageW(&msg);
        }

        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        ACTIVE.store(false, Ordering::Relaxed);
    }
}

unsafe fn create_message_window(
    class: &[u16],
    hinst: windows::Win32::Foundation::HMODULE,
) -> windows::core::Result<HWND> {
    use windows::Win32::UI::WindowsAndMessaging::CreateWindowExW;
    unsafe {
        CreateWindowExW(
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
        )
    }
}

fn wake() {
    CTX.with(|c| {
        if let Some(ctx) = c.borrow().as_ref() {
            ctx.request_repaint();
        }
    });
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        MSG_TRAY => {
            // The mouse event is in the LOW word of lParam for this callback.
            match (lp.0 as u32) & 0xFFFF {
                WM_LBUTTONUP => {
                    WANT_SHOW.store(true, Ordering::Relaxed);
                    wake();
                }
                WM_RBUTTONUP | WM_RBUTTONDBLCLK => unsafe { menu(hwnd) },
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            match wp.0 & 0xFFFF {
                ID_SHOW => WANT_SHOW.store(true, Ordering::Relaxed),
                ID_STOP => WANT_STOP.store(true, Ordering::Relaxed),
                ID_QUIT => WANT_QUIT.store(true, Ordering::Relaxed),
                _ => {}
            }
            wake();
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

unsafe fn menu(hwnd: HWND) {
    unsafe {
        let Ok(m) = CreatePopupMenu() else { return };
        let show = wide("Open HidForge");
        let stop = wide("Stop all macros (Esc)");
        let quit = wide("Quit");
        let _ = AppendMenuW(m, MF_STRING, ID_SHOW, PCWSTR(show.as_ptr()));
        let _ = AppendMenuW(m, MF_STRING, ID_STOP, PCWSTR(stop.as_ptr()));
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(m, MF_STRING, ID_QUIT, PCWSTR(quit.as_ptr()));

        let mut pt = Default::default();
        let _ = GetCursorPos(&mut pt);
        // Required, or the menu will not dismiss when clicked away from.
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(
            m,
            TPM_RIGHTALIGN | TPM_BOTTOMALIGN,
            pt.x,
            pt.y,
            Some(0),
            hwnd,
            None,
        );
        let _ = DestroyMenu(HMENU(m.0));
    }
}

//! System tray icon: pause/resume, reload config, start-with-Windows and
//! exit, so the release build (which has no console) is manageable without
//! memorizing hotkeys. Left-clicking the icon opens the workspace overview.

use crate::wm::Command;
use std::cell::Cell;
use std::ffi::c_void;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, GetCursorPos,
    LoadIconW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow,
    TrackPopupMenu, IDI_APPLICATION, MF_CHECKED, MF_SEPARATOR, MF_STRING, MF_UNCHECKED,
    TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WM_APP, WM_LBUTTONUP,
    WM_RBUTTONUP, WNDCLASSW, WS_POPUP,
};

const TRAY_MSG: u32 = WM_APP + 2;
const RUN_SUBKEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("wtm");

const CMD_PAUSE: usize = 1;
const CMD_RELOAD: usize = 2;
const CMD_AUTOSTART: usize = 3;
const CMD_EXIT: usize = 4;

thread_local! {
    static TRAY_HWND: Cell<isize> = const { Cell::new(0) };
    /// Explorer's "TaskbarCreated" broadcast; re-add the icon when it restarts.
    static TASKBAR_CREATED: Cell<u32> = const { Cell::new(0) };
}

unsafe extern "system" fn tray_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == TRAY_MSG {
        match lparam.0 as u32 {
            WM_RBUTTONUP => show_menu(hwnd),
            WM_LBUTTONUP => crate::overview::toggle(),
            _ => {}
        }
        return LRESULT(0);
    }
    if msg != 0 && msg == TASKBAR_CREATED.with(|t| t.get()) {
        add_icon(hwnd);
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe fn add_icon(hwnd: HWND) {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: TRAY_MSG,
        hIcon: LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
        ..Default::default()
    };
    let tip: Vec<u16> = "wtm — tiling window manager".encode_utf16().collect();
    let n = tip.len().min(nid.szTip.len() - 1);
    nid.szTip[..n].copy_from_slice(&tip[..n]);
    let _ = Shell_NotifyIconW(NIM_ADD, &nid);
}

pub fn init() {
    unsafe {
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        let class = WNDCLASSW {
            lpfnWndProc: Some(tray_proc),
            hInstance: hinstance.into(),
            lpszClassName: w!("wtm_tray"),
            ..Default::default()
        };
        RegisterClassW(&class);
        TASKBAR_CREATED.with(|t| t.set(RegisterWindowMessageW(w!("TaskbarCreated"))));
        if let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("wtm_tray"),
            w!("wtm tray"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance.into()),
            None,
        ) {
            TRAY_HWND.with(|t| t.set(hwnd.0 as isize));
            add_icon(hwnd);
        }
    }
}

pub fn destroy() {
    TRAY_HWND.with(|t| {
        let raw = t.get();
        if raw != 0 {
            let nid = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: HWND(raw as *mut c_void),
                uID: 1,
                ..Default::default()
            };
            let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) };
            t.set(0);
        }
    });
}

unsafe fn show_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else { return };
    let paused = crate::with_wm(|wm| wm.is_paused()).unwrap_or(false);
    let auto = autostart_enabled();
    let check = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
    let _ = AppendMenuW(menu, MF_STRING | check(paused), CMD_PAUSE, w!("Pause tiling"));
    let _ = AppendMenuW(menu, MF_STRING, CMD_RELOAD, w!("Reload config"));
    let _ = AppendMenuW(menu, MF_STRING | check(auto), CMD_AUTOSTART, w!("Start with Windows"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, CMD_EXIT, w!("Exit wtm"));
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // Required dance so the menu dismisses when clicking elsewhere.
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
        pt.x,
        pt.y,
        None,
        hwnd,
        None,
    );
    let _ = DestroyMenu(menu);
    match cmd.0 as usize {
        CMD_PAUSE => {
            crate::with_wm(|wm| wm.handle_command(Command::TogglePause));
            crate::bar::invalidate_all();
            crate::border::update();
        }
        CMD_RELOAD => crate::force_config_reload(),
        CMD_AUTOSTART => set_autostart(!auto),
        CMD_EXIT => PostQuitMessage(0),
        _ => {}
    }
}

// -------------------------------------------------------------- autostart

fn autostart_enabled() -> bool {
    unsafe {
        RegGetValueW(HKEY_CURRENT_USER, RUN_SUBKEY, RUN_VALUE, RRF_RT_REG_SZ, None, None, None)
            .is_ok()
    }
}

fn set_autostart(on: bool) {
    unsafe {
        if on {
            let Ok(exe) = std::env::current_exe() else { return };
            let cmd = format!("\"{}\"", exe.display());
            let wide: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
            let status = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_SUBKEY,
                RUN_VALUE,
                REG_SZ.0,
                Some(wide.as_ptr() as *const c_void),
                (wide.len() * 2) as u32,
            );
            if status.is_ok() {
                crate::logln!("wtm: autostart enabled ({cmd})");
            } else {
                crate::logln!("wtm: could not enable autostart: {status:?}");
            }
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_SUBKEY, RUN_VALUE);
            crate::logln!("wtm: autostart disabled");
        }
    }
}

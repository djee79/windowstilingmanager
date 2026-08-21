//! Focus follows mouse, Hyprland-style: hovering over a managed window makes
//! it the active window without clicking.
//!
//! A low-level mouse hook (WH_MOUSE_LL, delivered through this thread's
//! message loop) watches for movement. The callback sits in the system's
//! input path and must return immediately, so it only arms a one-shot thread
//! timer; the actual hit-test + focus happens back in the message loop, at
//! most once every SETTLE_MS while the pointer is moving.

use crate::window::Window;
use std::cell::Cell;
use std::ffi::c_void;
use windows::Win32::Foundation::{LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON, VK_XBUTTON1, VK_XBUTTON2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetAncestor, GetCursorPos, GetForegroundWindow, GetGUIThreadInfo,
    GetWindowThreadProcessId, KillTimer, SetTimer, SetWindowsHookExW, UnhookWindowsHookEx,
    WindowFromPoint, GA_ROOT, GUITHREADINFO, GUI_INMENUMODE, GUI_INMOVESIZE,
    GUI_POPUPMENUMODE, GUI_SYSTEMMENUMODE, HHOOK, WH_MOUSE_LL, WM_MOUSEMOVE,
};

/// Coalesce mouse-move bursts: hit-test at most this often.
const SETTLE_MS: u32 = 50;

thread_local! {
    static HOOK: Cell<isize> = const { Cell::new(0) };
    static TIMER_ID: Cell<usize> = const { Cell::new(0) };
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && wparam.0 as u32 == WM_MOUSEMOVE {
        // One pending timer at a time; never do real work in the hook.
        TIMER_ID.with(|t| {
            if t.get() == 0 {
                t.set(SetTimer(None, 0, SETTLE_MS, None));
            }
        });
    }
    CallNextHookEx(None, code, wparam, lparam)
}

pub fn init(enabled: bool) {
    if !enabled || HOOK.with(|c| c.get()) != 0 {
        return;
    }
    match unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), None, 0) } {
        Ok(h) => HOOK.with(|c| c.set(h.0 as isize)),
        Err(e) => crate::logln!("wtm: focus_follows_mouse hook failed: {e}"),
    }
}

/// Config hot-reload: install or remove the hook to match the setting.
pub fn set_enabled(on: bool) {
    let installed = HOOK.with(|c| c.get() != 0);
    if on && !installed {
        init(true);
    } else if !on && installed {
        destroy();
    }
}

pub fn destroy() {
    HOOK.with(|c| {
        if c.get() != 0 {
            let _ = unsafe { UnhookWindowsHookEx(HHOOK(c.get() as *mut c_void)) };
            c.set(0);
        }
    });
    TIMER_ID.with(|t| {
        if t.get() != 0 {
            let _ = unsafe { KillTimer(None, t.get()) };
            t.set(0);
        }
    });
}

pub fn is_focus_timer(id: usize) -> bool {
    id != 0 && TIMER_ID.with(|t| t.get()) == id
}

/// The settle timer fired: focus the managed window under the cursor if it
/// isn't the foreground window already. Runs on the main message loop.
pub fn tick() {
    TIMER_ID.with(|t| {
        let _ = unsafe { KillTimer(None, t.get()) };
        t.set(0);
    });
    unsafe {
        // Never steal focus mid-click/drag (rearranging windows, selecting
        // text across a window edge, scrollbar drags)…
        let buttons = [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2];
        if buttons.iter().any(|vk| GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000 != 0) {
            return;
        }
        // …nor while the foreground thread shows a menu or is in a move/size
        // modal loop — activating another window would rip those apart.
        let mut gui =
            GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
        if GetGUIThreadInfo(0, &mut gui).is_ok()
            && (gui.flags.0
                & (GUI_INMENUMODE.0 | GUI_INMOVESIZE.0 | GUI_POPUPMENUMODE.0 | GUI_SYSTEMMENUMODE.0))
                != 0
        {
            return;
        }
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let hit = GetAncestor(WindowFromPoint(pt), GA_ROOT);
        let fg = GetForegroundWindow();
        if hit.is_invalid() || hit == fg {
            return;
        }
        // Don't pull focus away from wtm's own UI (help panel, launcher).
        let mut fg_pid = 0u32;
        GetWindowThreadProcessId(fg, Some(&mut fg_pid));
        if fg_pid == std::process::id() {
            return;
        }
        // A dialog owned by the hovered window keeps focus — stealing it
        // would raise the owner over its own delete-confirmation prompt.
        // root_owner walks GW_OWNER by hand; GA_ROOTOWNER misses owned
        // non-popup windows like docking panes.
        if Window::from_hwnd(fg).root_owner() == Window::from_hwnd(hit) {
            return;
        }
        let w = Window::from_hwnd(hit);
        if crate::with_wm(|wm| wm.hover_focus_target(w)).unwrap_or(false) {
            // The EVENT_SYSTEM_FOREGROUND hook then repaints borders + bar.
            w.focus();
        }
    }
}

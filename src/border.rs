//! Thick focus border: a topmost, click-through frame window drawn around
//! the focused window. DWM's native accent border (DWMWA_BORDER_COLOR) is
//! fixed at ~1px, so for a visible perimeter we draw our own — the same
//! approach komorebi uses. The window's region is a hollow rectangle, so
//! only the frame itself exists; clicks pass through everywhere.

use crate::config::{parse_colorref, Config};
use std::cell::Cell;
use std::ffi::c_void;
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CombineRgn, CreateRectRgn, CreateRoundRectRgn, CreateSolidBrush, DeleteObject, InvalidateRect,
    SetWindowRgn, HBRUSH, HRGN, RGN_DIFF,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, SetClassLongPtrW,
    SetLayeredWindowAttributes, SetWindowPos, ShowWindow, GCLP_HBRBACKGROUND, HWND_TOPMOST,
    LWA_ALPHA, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

thread_local! {
    static BORDER: Cell<isize> = const { Cell::new(0) };
    static THICKNESS: Cell<i32> = const { Cell::new(0) };
    static RADIUS: Cell<i32> = const { Cell::new(8) };
}

unsafe extern "system" fn border_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // The class background brush paints the accent color; nothing else to do.
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

// The window is created even with thickness 0 (update() just hides it), so
// the appearance panel can turn the frame on later without a restart.
pub fn init(cfg: &Config) {
    THICKNESS.with(|t| t.set(cfg.border_thickness.max(0)));
    RADIUS.with(|r| r.set(cfg.border_corner_radius.max(0)));
    unsafe {
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        let class = WNDCLASSW {
            lpfnWndProc: Some(border_proc),
            hInstance: hinstance.into(),
            lpszClassName: w!("wtm_border"),
            hbrBackground: CreateSolidBrush(COLORREF(parse_colorref(&cfg.active_border_color))),
            ..Default::default()
        };
        RegisterClassW(&class);
        if let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT,
            w!("wtm_border"),
            w!("wtm border"),
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
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
            BORDER.with(|b| b.set(hwnd.0 as isize));
        }
    }
}

/// Reposition (or hide) the frame around the currently focused window.
/// Must be called OUTSIDE any with_wm borrow — it takes its own.
pub fn update() {
    let raw = BORDER.with(|b| b.get());
    if raw == 0 {
        return;
    }
    let hwnd = HWND(raw as *mut c_void);
    let target = crate::with_wm(|wm| wm.border_target()).flatten();
    let t = THICKNESS.with(|t| t.get());
    unsafe {
        match target {
            None => {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            Some(_) if t <= 0 => {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            Some(r) => {
                let (w, h) = (r.w + 2 * t, r.h + 2 * t);
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    r.x - t,
                    r.y - t,
                    w,
                    h,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
                // Hollow region: only the frame is part of the window. The
                // corners are rounded to match Windows 11's ~8px window
                // radius: inner curve = window radius, outer = radius +
                // thickness, so the band stays even around the corner.
                let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
                let r = (RADIUS.with(|r| r.get()) as f32 * scale) as i32;
                let (outer, inner): (HRGN, HRGN) = if r > 0 {
                    (
                        CreateRoundRectRgn(0, 0, w, h, 2 * (r + t), 2 * (r + t)),
                        CreateRoundRectRgn(t, t, w - t, h - t, 2 * r, 2 * r),
                    )
                } else {
                    (CreateRectRgn(0, 0, w, h), CreateRectRgn(t, t, w - t, h - t))
                };
                CombineRgn(Some(outer), Some(outer), Some(inner), RGN_DIFF);
                let _ = DeleteObject(inner.into());
                SetWindowRgn(hwnd, Some(outer), true); // system now owns `outer`
            }
        }
    }
}

/// Live style change from the bar's appearance panel: new thickness (0
/// hides the frame) and fill color. The color lives in the window class's
/// background brush, so swap the brush and force a repaint.
pub fn set_radius(r: i32) {
    RADIUS.with(|c| c.set(r.max(0)));
}

pub fn set_style(thickness: i32, color: u32) {
    THICKNESS.with(|t| t.set(thickness.max(0)));
    BORDER.with(|b| {
        let raw = b.get();
        if raw == 0 {
            return;
        }
        unsafe {
            let hwnd = HWND(raw as *mut c_void);
            let brush = CreateSolidBrush(COLORREF(color));
            let old = SetClassLongPtrW(hwnd, GCLP_HBRBACKGROUND, brush.0 as isize);
            if old != 0 {
                let _ = DeleteObject(HBRUSH(old as *mut c_void).into());
            }
            let _ = InvalidateRect(Some(hwnd), None, true);
        }
    });
    update();
}

pub fn destroy() {
    BORDER.with(|b| {
        let raw = b.get();
        if raw != 0 {
            let _ = unsafe { DestroyWindow(HWND(raw as *mut c_void)) };
            b.set(0);
        }
    });
}

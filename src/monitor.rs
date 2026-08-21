//! Monitor enumeration. Returns each monitor's handle and *work area*
//! (the screen minus the taskbar), primary monitor first.

use crate::layout::Rect;
use windows::core::BOOL;
use windows::Win32::Foundation::{LPARAM, POINT, RECT, TRUE};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, MonitorFromPoint, HDC, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

const MONITORINFOF_PRIMARY: u32 = 1;

pub struct MonitorInfo {
    pub handle: isize,
    /// Full monitor rectangle, including taskbar/appbar areas.
    pub bounds: Rect,
    pub work_area: Rect,
    pub primary: bool,
}

unsafe extern "system" fn enum_proc(
    hmon: HMONITOR,
    _hdc: HDC,
    _clip: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let list = &mut *(lparam.0 as *mut Vec<MonitorInfo>);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(hmon, &mut info).as_bool() {
        let b = info.rcMonitor;
        let r = info.rcWork;
        list.push(MonitorInfo {
            handle: hmon.0 as isize,
            bounds: Rect { x: b.left, y: b.top, w: b.right - b.left, h: b.bottom - b.top },
            work_area: Rect { x: r.left, y: r.top, w: r.right - r.left, h: r.bottom - r.top },
            primary: (info.dwFlags & MONITORINFOF_PRIMARY) != 0,
        });
    }
    TRUE
}

pub fn enumerate() -> Vec<MonitorInfo> {
    let mut list: Vec<MonitorInfo> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(enum_proc),
            LPARAM(&mut list as *mut Vec<MonitorInfo> as isize),
        );
    }
    list.sort_by_key(|m| !m.primary); // primary first
    list
}

/// The monitor the user is on: the one under the mouse cursor, falling back
/// to the primary. Panels (launcher, help, settings, …) open here.
pub fn active() -> Option<MonitorInfo> {
    let list = enumerate();
    let mut pt = POINT::default();
    let _ = unsafe { GetCursorPos(&mut pt) };
    let hmon = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTOPRIMARY) };
    let handle = hmon.0 as isize;
    let idx = list.iter().position(|m| m.handle == handle).unwrap_or(0);
    list.into_iter().nth(idx)
}

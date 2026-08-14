//! A thin, copyable wrapper around an HWND with all the Win32 calls the
//! window manager needs. Everything here is best-effort: windows die at any
//! moment, so failures are swallowed and callers re-validate with is_valid().

use crate::config::Config;
use crate::layout::Rect;
use std::ffi::c_void;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_CLOAKED,
    DWMWA_COLOR_DEFAULT, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{keybd_event, KEYBD_EVENT_FLAGS};
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetClassNameW, GetForegroundWindow, GetWindowLongPtrW, GetWindowRect,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, IsZoomed,
    PostMessageW, SetForegroundWindow, SetWindowPos, ShowWindow, GA_ROOT, GWL_EXSTYLE, GWL_STYLE,
    HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE,
    SW_RESTORE, SW_SHOWNOACTIVATE, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_CLOSE, WS_CAPTION, WS_CHILD, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_THICKFRAME,
};
use windows::core::PWSTR;

/// Stored as a raw pointer value so it is Copy + Eq + Hash and safe to keep
/// in collections after the window dies.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Window(pub isize);

impl Window {
    pub fn from_hwnd(hwnd: HWND) -> Self {
        Window(hwnd.0 as isize)
    }

    pub fn hwnd(&self) -> HWND {
        HWND(self.0 as *mut c_void)
    }

    pub fn foreground() -> Option<Window> {
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.is_invalid() {
            None
        } else {
            Some(Window::from_hwnd(hwnd))
        }
    }

    pub fn is_valid(&self) -> bool {
        unsafe { IsWindow(Some(self.hwnd())).as_bool() }
    }

    pub fn is_visible(&self) -> bool {
        unsafe { IsWindowVisible(self.hwnd()).as_bool() }
    }

    pub fn is_minimized(&self) -> bool {
        unsafe { IsIconic(self.hwnd()).as_bool() }
    }

    pub fn title(&self) -> String {
        let mut buf = [0u16; 512];
        let len = unsafe { GetWindowTextW(self.hwnd(), &mut buf) };
        String::from_utf16_lossy(&buf[..len.max(0) as usize])
    }

    pub fn class(&self) -> String {
        let mut buf = [0u16; 256];
        let len = unsafe { GetClassNameW(self.hwnd(), &mut buf) };
        String::from_utf16_lossy(&buf[..len.max(0) as usize])
    }

    /// Executable file name (e.g. "firefox.exe"), if we can open the process.
    pub fn exe(&self) -> Option<String> {
        unsafe {
            let mut pid = 0u32;
            GetWindowThreadProcessId(self.hwnd(), Some(&mut pid));
            if pid == 0 {
                return None;
            }
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut len,
            );
            let _ = CloseHandle(handle);
            ok.ok()?;
            let path = String::from_utf16_lossy(&buf[..len as usize]);
            path.rsplit(['\\', '/']).next().map(str::to_string)
        }
    }

    fn style(&self) -> WINDOW_STYLE {
        WINDOW_STYLE(unsafe { GetWindowLongPtrW(self.hwnd(), GWL_STYLE) } as u32)
    }

    fn ex_style(&self) -> WINDOW_EX_STYLE {
        WINDOW_EX_STYLE(unsafe { GetWindowLongPtrW(self.hwnd(), GWL_EXSTYLE) } as u32)
    }

    /// DWM "cloaked" windows are invisible even though IsWindowVisible says
    /// otherwise (UWP apps, windows on other virtual desktops).
    pub fn is_cloaked(&self) -> bool {
        let mut cloaked = 0u32;
        let ok = unsafe {
            DwmGetWindowAttribute(
                self.hwnd(),
                DWMWA_CLOAKED,
                &mut cloaked as *mut u32 as *mut c_void,
                std::mem::size_of::<u32>() as u32,
            )
        };
        ok.is_ok() && cloaked != 0
    }

    /// Should the tiler manage this window at all?
    pub fn is_manageable(&self, cfg: &Config) -> bool {
        if !self.is_valid() || !self.is_visible() || self.is_minimized() || self.is_cloaked() {
            return false;
        }
        // Top-level windows only.
        if unsafe { GetAncestor(self.hwnd(), GA_ROOT) } != self.hwnd() {
            return false;
        }
        let style = self.style();
        if style.contains(WS_CHILD) || !style.contains(WS_CAPTION) {
            return false;
        }
        let ex = self.ex_style();
        if ex.contains(WS_EX_TOOLWINDOW) || ex.contains(WS_EX_NOACTIVATE) {
            return false;
        }
        let title = self.title();
        if title.is_empty() {
            return false;
        }
        if cfg.ignore_titles.iter().any(|t| title.contains(t)) {
            return false;
        }
        let class = self.class();
        if cfg.ignore_classes.iter().any(|c| c == &class) {
            return false;
        }
        if let Some(exe) = self.exe() {
            if cfg.ignore_exes.iter().any(|e| e.eq_ignore_ascii_case(&exe)) {
                return false;
            }
        }
        true
    }

    /// Non-resizable windows (dialogs, splash screens) float instead of tile.
    pub fn should_float(&self, cfg: &Config) -> bool {
        if !self.style().contains(WS_THICKFRAME) {
            return true;
        }
        let class = self.class();
        if cfg.float_classes.iter().any(|c| c == &class) {
            return true;
        }
        let title = self.title();
        cfg.float_titles.iter().any(|t| title.contains(t))
    }

    pub fn rect(&self) -> Rect {
        let mut r = RECT::default();
        let _ = unsafe { GetWindowRect(self.hwnd(), &mut r) };
        Rect { x: r.left, y: r.top, w: r.right - r.left, h: r.bottom - r.top }
    }

    /// The window's *visible* frame (DWM extended frame bounds), excluding
    /// the invisible resize shadow that GetWindowRect includes.
    pub fn visible_rect(&self) -> Rect {
        let mut wr = RECT::default();
        let _ = unsafe { GetWindowRect(self.hwnd(), &mut wr) };
        let mut fr = wr;
        let _ = unsafe {
            DwmGetWindowAttribute(
                self.hwnd(),
                DWMWA_EXTENDED_FRAME_BOUNDS,
                &mut fr as *mut RECT as *mut c_void,
                std::mem::size_of::<RECT>() as u32,
            )
        };
        Rect { x: fr.left, y: fr.top, w: fr.right - fr.left, h: fr.bottom - fr.top }
    }

    /// Per-edge deltas between the window rect and its visible frame
    /// (left, top, right, bottom). Stable per window; ~7px sides on Win11.
    pub fn frame_offsets(&self) -> (i32, i32, i32, i32) {
        let mut wr = RECT::default();
        let _ = unsafe { GetWindowRect(self.hwnd(), &mut wr) };
        let mut fr = wr;
        let _ = unsafe {
            DwmGetWindowAttribute(
                self.hwnd(),
                DWMWA_EXTENDED_FRAME_BOUNDS,
                &mut fr as *mut RECT as *mut c_void,
                std::mem::size_of::<RECT>() as u32,
            )
        };
        (fr.left - wr.left, fr.top - wr.top, wr.right - fr.right, wr.bottom - fr.bottom)
    }

    /// Un-maximize if needed so SetWindowPos can take effect.
    pub fn prepare_for_move(&self) {
        unsafe {
            if IsZoomed(self.hwnd()).as_bool() {
                let _ = ShowWindow(self.hwnd(), SW_RESTORE);
            }
        }
    }

    /// Place the *visible* frame on `target`, using precomputed offsets
    /// (avoids two extra syscalls per animation frame).
    pub fn place_visible(&self, target: Rect, offsets: (i32, i32, i32, i32)) {
        let (l, t, r, b) = offsets;
        let _ = unsafe {
            SetWindowPos(
                self.hwnd(),
                None,
                target.x - l,
                target.y - t,
                target.w + l + r,
                target.h + t + b,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
        };
    }

    /// Move/size immediately so the visible frame lands exactly on `target`.
    pub fn apply_rect(&self, target: Rect) {
        self.prepare_for_move();
        self.place_visible(target, self.frame_offsets());
    }

    /// Put the window above (or back below) the topmost band — used for
    /// fullscreen, which must cover the (topmost) status bar.
    pub fn set_topmost(&self, on: bool) {
        let after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
        let _ = unsafe {
            SetWindowPos(self.hwnd(), Some(after), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE)
        };
    }

    pub fn hide(&self) {
        let _ = unsafe { ShowWindow(self.hwnd(), SW_HIDE) };
    }

    pub fn show(&self) {
        let _ = unsafe { ShowWindow(self.hwnd(), SW_SHOWNOACTIVATE) };
    }

    pub fn focus(&self) {
        unsafe {
            let hwnd = self.hwnd();
            if self.is_minimized() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            // Windows refuses SetForegroundWindow from a background process
            // and flashes the taskbar button instead. A zero-key keyboard
            // event marks our process as the last input source, which lifts
            // the foreground lock (same trick komorebi uses).
            keybd_event(0, 0, KEYBD_EVENT_FLAGS(0), 0);
            let _ = SetForegroundWindow(hwnd);
            if GetForegroundWindow() != hwnd {
                // Stubborn case: temporarily attach our input queue to the
                // current foreground thread and borrow its focus rights.
                let fg = GetForegroundWindow();
                let fg_thread = GetWindowThreadProcessId(fg, None);
                let me = GetCurrentThreadId();
                if fg_thread != 0 && fg_thread != me {
                    let _ = AttachThreadInput(me, fg_thread, true);
                    let _ = SetForegroundWindow(hwnd);
                    let _ = AttachThreadInput(me, fg_thread, false);
                }
            }
        }
    }

    pub fn close(&self) {
        let _ = unsafe { PostMessageW(Some(self.hwnd()), WM_CLOSE, WPARAM(0), LPARAM(0)) };
    }

    /// Some(colorref) paints the Windows 11 frame border; None restores the
    /// system default.
    pub fn set_border_color(&self, color: Option<u32>) {
        let value: u32 = color.unwrap_or(DWMWA_COLOR_DEFAULT);
        let _ = unsafe {
            DwmSetWindowAttribute(
                self.hwnd(),
                DWMWA_BORDER_COLOR,
                &value as *const u32 as *const c_void,
                std::mem::size_of::<u32>() as u32,
            )
        };
    }

    pub fn monitor(&self) -> isize {
        unsafe { MonitorFromWindow(self.hwnd(), MONITOR_DEFAULTTONEAREST).0 as isize }
    }
}

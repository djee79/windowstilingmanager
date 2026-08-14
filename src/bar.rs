//! Waybar-style status bar + interactive keybindings panel.
//!
//! The bar: one AppBar window per monitor showing the nine workspaces
//! (click to switch), the focused window title, a clock, and a "?" button.
//! Registered via SHAppBarMessage so it *reserves* its strip of screen like
//! waybar's exclusive zone.
//!
//! The "?" panel is live: it lists the actual configured chords, filters as
//! you type, and clicking a row lets you press a new shortcut which is
//! re-registered immediately and saved back to config.toml.

use crate::config::{parse_colorref, Config, LauncherEntry};
use crate::keys;
use crate::monitor;
use crate::window::Window;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::c_void;
use windows::core::{w, PCWSTR};
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows::core::PWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW,
    CreateSolidBrush, DeleteDC, DeleteObject, EndPaint, FillRect, GetDC,
    GetTextExtentPoint32W, InvalidateRect, ReleaseDC, SelectObject, SetBkMode, SetTextColor,
    DrawTextW, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_RIGHT, DT_SINGLELINE, DT_VCENTER,
    FW_BOLD, FW_NORMAL, HDC, HFONT, PAINTSTRUCT, SRCCOPY, TRANSPARENT, CLEARTYPE_QUALITY,
    DEFAULT_CHARSET, FONT_OUTPUT_PRECISION, FONT_CLIP_PRECISION,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_ESCAPE, VK_LWIN, VK_MENU,
    VK_RETURN, VK_RWIN, VK_SHIFT, VK_UP,
};
use windows::Win32::UI::Shell::{
    SHAppBarMessage, ShellExecuteW, ABE_TOP, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS,
    APPBARDATA,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetWindowLongPtrW, GetWindowRect,
    KillTimer, LoadCursorW, RegisterClassW, SetTimer, SetWindowLongPtrW, SetWindowPos,
    CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, HWND_TOPMOST, IDC_ARROW, SWP_NOACTIVATE,
    SWP_SHOWWINDOW, SW_SHOWNORMAL, WM_CHAR, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN,
    WM_KILLFOCUS, WM_LBUTTONDOWN, WM_PAINT, WM_SYSCHAR, WM_SYSKEYDOWN, WM_TIMER, WNDCLASSW,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};

#[derive(Clone)]
struct BarStyle {
    height: i32,
    bg: u32,
    fg: u32,
    accent: u32,
    cell_empty: u32,
    cell_occupied: u32,
}

struct BarWindow {
    hwnd: isize,
}

#[derive(Clone)]
struct HelpEntry {
    action: String,
    chord: String,
    desc: String,
}

#[derive(Default)]
struct HelpState {
    hwnd: isize,
    query: String,
    /// Action currently being rebound (waiting for a key press).
    rebinding: Option<String>,
    /// Status/error line shown at the bottom of the panel.
    message: String,
}

/// Rows of the settings (≡) panel.
enum SettingsItem {
    Ws(usize),
    RulesHeader,
    Rule(String, usize),
    NoRules,
}

#[derive(Default)]
struct SettingsState {
    hwnd: isize,
    /// Workspace index whose name is being typed.
    editing: Option<usize>,
    buffer: String,
}

#[derive(Default)]
struct LauncherState {
    hwnd: isize,
    query: String,
    selected: usize,
    /// Path picked via Browse…, waiting for the user to type a display name.
    adding: Option<String>,
    name_buf: String,
    /// The modal file dialog steals focus; don't self-close while it's up.
    dialog_open: bool,
    /// Config index of the entry whose shortcut is being captured.
    capturing: Option<usize>,
    /// Status/error line (conflicts, confirmations).
    message: String,
}

thread_local! {
    static BARS: RefCell<Vec<BarWindow>> = const { RefCell::new(Vec::new()) };
    static STYLE: RefCell<Option<BarStyle>> = const { RefCell::new(None) };
    static KEYBINDS: RefCell<Vec<HelpEntry>> = const { RefCell::new(Vec::new()) };
    static HELP: RefCell<HelpState> = RefCell::new(HelpState::default());
    static SETTINGS: RefCell<SettingsState> = RefCell::new(SettingsState::default());
    static LAUNCHER: RefCell<LauncherState> = RefCell::new(LauncherState::default());
}

fn style() -> BarStyle {
    STYLE.with(|s| s.borrow().clone()).unwrap_or(BarStyle {
        height: 32,
        bg: 0x251818,
        fg: 0xf4d6cd,
        accent: 0x00F7A27A,
        cell_empty: 0x443131,
        cell_occupied: 0x5a4745,
    })
}

/// Rebuild the panel's entry list from the active keybindings, in the
/// canonical ACTIONS order.
pub fn set_keybinds(bindings: &BTreeMap<String, String>) {
    KEYBINDS.with(|k| {
        *k.borrow_mut() = keys::ACTIONS
            .iter()
            .map(|(action, default_chord, desc)| HelpEntry {
                action: action.to_string(),
                chord: bindings
                    .get(*action)
                    .cloned()
                    .unwrap_or_else(|| default_chord.to_string()),
                desc: desc.to_string(),
            })
            .collect();
    });
}

// ------------------------------------------------------------------- layout

struct BarLayout {
    cell_w: i32,
    cell_h: i32,
    gap: i32,
    pad: i32,
    help_w: i32,
    clock_w: i32,
    /// Cap on how much width a workspace name may add to its cell.
    name_max: i32,
}

fn bar_layout(scale: f32, bar_h: i32) -> BarLayout {
    let s = |v: i32| (v as f32 * scale) as i32;
    BarLayout {
        cell_w: s(26),
        cell_h: bar_h - s(10),
        gap: s(4),
        pad: s(8),
        help_w: s(36),
        clock_w: s(56),
        name_max: s(150),
    }
}

// ------------------------------------------------------------------ create

pub fn init(cfg: &Config) {
    set_keybinds(&cfg.keybindings);
    STYLE.with(|s| {
        *s.borrow_mut() = Some(BarStyle {
            height: cfg.bar_height,
            bg: parse_colorref(&cfg.bar_background),
            fg: parse_colorref(&cfg.bar_foreground),
            accent: parse_colorref(&cfg.active_border_color),
            cell_empty: 0x443131,
            cell_occupied: 0x5a4745,
        });
    });
    unsafe {
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        for (name, proc) in [
            (w!("wtm_bar"), bar_proc as _),
            (w!("wtm_help"), help_proc as _),
            (w!("wtm_settings"), settings_proc as _),
            (w!("wtm_launcher"), launcher_proc as _),
        ] {
            let class = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(proc),
                hInstance: hinstance.into(),
                lpszClassName: name,
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                ..Default::default()
            };
            RegisterClassW(&class);
        }
    }
    if !cfg.bar_enabled {
        return;
    }
    for m in monitor::enumerate() {
        create_bar(m.handle, m.bounds.x, m.bounds.y, m.bounds.w);
    }
}

fn create_bar(monitor: isize, x: i32, y: i32, w: i32) {
    unsafe {
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            w!("wtm_bar"),
            w!("wtm bar"),
            WS_POPUP,
            x,
            y,
            w,
            style().height,
            None,
            None,
            Some(hinstance.into()),
            None,
        ) else {
            crate::logln!("wtm: failed to create bar window");
            return;
        };
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, monitor);
        let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
        let h = (style().height as f32 * scale) as i32;

        // Reserve the strip: the shell shrinks this monitor's work area.
        let mut abd = APPBARDATA {
            cbSize: std::mem::size_of::<APPBARDATA>() as u32,
            hWnd: hwnd,
            uCallbackMessage: 0,
            uEdge: ABE_TOP,
            rc: RECT { left: x, top: y, right: x + w, bottom: y + h },
            lParam: LPARAM(0),
        };
        SHAppBarMessage(ABM_NEW, &mut abd);
        SHAppBarMessage(ABM_QUERYPOS, &mut abd);
        abd.rc.bottom = abd.rc.top + h;
        SHAppBarMessage(ABM_SETPOS, &mut abd);
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            abd.rc.left,
            abd.rc.top,
            abd.rc.right - abd.rc.left,
            abd.rc.bottom - abd.rc.top,
            SWP_SHOWWINDOW | SWP_NOACTIVATE,
        );
        SetTimer(Some(hwnd), 1, 1000, None); // clock refresh
        BARS.with(|b| b.borrow_mut().push(BarWindow { hwnd: hwnd.0 as isize }));
    }
}

pub fn invalidate_all() {
    BARS.with(|b| {
        for bar in b.borrow().iter() {
            let _ = unsafe { InvalidateRect(Some(HWND(bar.hwnd as *mut c_void)), None, false) };
        }
    });
}

pub fn destroy_all() {
    BARS.with(|b| {
        for bar in b.borrow_mut().drain(..) {
            unsafe {
                let hwnd = HWND(bar.hwnd as *mut c_void);
                let mut abd = APPBARDATA {
                    cbSize: std::mem::size_of::<APPBARDATA>() as u32,
                    hWnd: hwnd,
                    ..Default::default()
                };
                SHAppBarMessage(ABM_REMOVE, &mut abd);
                let _ = DestroyWindow(hwnd);
            }
        }
    });
    close_help();
    close_settings();
    close_launcher();
}

/// A small outlined "button" (used for the shortcut chips).
unsafe fn draw_chip(mem: HDC, rect: RECT, bg: u32, border: u32) {
    fill(mem, &rect, border);
    let inner = RECT {
        left: rect.left + 1,
        top: rect.top + 1,
        right: rect.right - 1,
        bottom: rect.bottom - 1,
    };
    fill(mem, &inner, bg);
}

/// Width of a string in the current font of `hdc`.
unsafe fn text_width(hdc: HDC, text: &str) -> i32 {
    let wide: Vec<u16> = text.encode_utf16().collect();
    let mut size = SIZE::default();
    let _ = GetTextExtentPoint32W(hdc, &wide, &mut size);
    size.cx
}

/// (x, width) of each workspace cell; named cells widen to show their name
/// (capped at name_max, painted with an ellipsis when longer).
fn cell_spans(
    l: &BarLayout,
    snap: &crate::wm::BarSnapshot,
    mut measure: impl FnMut(&str) -> i32,
) -> Vec<(i32, i32)> {
    let mut spans = Vec::with_capacity(snap.occupied.len());
    let mut x = l.pad;
    for i in 0..snap.occupied.len() {
        let mut cw = l.cell_w;
        if let Some(name) = snap.names.get(i) {
            if !name.is_empty() {
                cw += measure(name).min(l.name_max) + l.gap * 2;
            }
        }
        spans.push((x, cw));
        x += cw + l.gap;
    }
    spans
}

// ----------------------------------------------------------------- bar proc

unsafe extern "system" fn bar_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint_bar(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1), // double-buffered; skip background erase
        WM_TIMER => {
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            on_bar_click(hwnd, x);
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = KillTimer(Some(hwnd), 1);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn on_bar_click(hwnd: HWND, x: i32) {
    let mut rc = RECT::default();
    let _ = unsafe { GetClientRect(hwnd, &mut rc) };
    let scale = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0;
    let l = bar_layout(scale, rc.bottom);
    if x >= rc.right - l.help_w {
        toggle_help_panel();
        return;
    }
    if x >= rc.right - 2 * l.help_w - l.gap {
        toggle_settings_panel();
        return;
    }
    if x >= rc.right - 3 * l.help_w - 2 * l.gap {
        toggle_launcher_panel();
        return;
    }
    let monitor = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
    if let Some(idx) = cell_index_at(hwnd, x) {
        crate::with_wm(|wm| {
            let mi = wm.monitor_index_of_handle(monitor);
            wm.switch_workspace_on(mi, idx);
        });
        invalidate_all();
        crate::border::update();
    }
}

/// Which workspace cell (if any) sits at client-x `x` on this bar. Rebuilds
/// the same spans the painter used, measuring names with the same font.
fn cell_index_at(hwnd: HWND, x: i32) -> Option<usize> {
    let mut rc = RECT::default();
    let _ = unsafe { GetClientRect(hwnd, &mut rc) };
    let scale = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0;
    let l = bar_layout(scale, rc.bottom);
    let monitor = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
    let snap = crate::with_wm(|wm| wm.bar_snapshot(monitor)).flatten()?;
    let spans = unsafe {
        let hdc = GetDC(Some(hwnd));
        let font = make_font((rc.bottom - l.pad) * 55 / 100, FW_NORMAL.0 as i32);
        let old = SelectObject(hdc, font.into());
        let spans = cell_spans(&l, &snap, |t| text_width(hdc, t));
        SelectObject(hdc, old);
        let _ = DeleteObject(font.into());
        ReleaseDC(Some(hwnd), hdc);
        spans
    };
    spans.iter().position(|(cx, cw)| x >= *cx && x < cx + cw)
}

/// Screen-point -> (monitor handle, workspace index) if the point is over a
/// bar's workspace cell. Used to drop dragged windows onto workspaces.
/// The hit zone extends half a bar-height below the bar, because during a
/// drag the cursor sits at the grab point, usually a little under the strip.
pub fn workspace_cell_at_point(px: i32, py: i32) -> Option<(isize, usize)> {
    let bars: Vec<isize> = BARS.with(|b| b.borrow().iter().map(|bar| bar.hwnd).collect());
    for raw in bars {
        let hwnd = HWND(raw as *mut c_void);
        let mut wr = RECT::default();
        let _ = unsafe { GetWindowRect(hwnd, &mut wr) };
        let slack = (wr.bottom - wr.top) / 2;
        if px >= wr.left && px < wr.right && py >= wr.top && py < wr.bottom + slack {
            let monitor = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
            return cell_index_at(hwnd, px - wr.left).map(|i| (monitor, i));
        }
    }
    None
}

// ------------------------------------------------------------ bar painting

struct Buffered {
    hdc: HDC,
    mem: HDC,
    bmp: windows::Win32::Graphics::Gdi::HBITMAP,
    old_bmp: windows::Win32::Graphics::Gdi::HGDIOBJ,
    ps: PAINTSTRUCT,
    hwnd: HWND,
    w: i32,
    h: i32,
}

unsafe fn begin_buffered(hwnd: HWND) -> Buffered {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let mem = CreateCompatibleDC(Some(hdc));
    let bmp = CreateCompatibleBitmap(hdc, rc.right, rc.bottom);
    let old_bmp = SelectObject(mem, bmp.into());
    SetBkMode(mem, TRANSPARENT);
    Buffered { hdc, mem, bmp, old_bmp, ps, hwnd, w: rc.right, h: rc.bottom }
}

unsafe fn end_buffered(b: Buffered) {
    let _ = BitBlt(b.hdc, 0, 0, b.w, b.h, Some(b.mem), 0, 0, SRCCOPY);
    SelectObject(b.mem, b.old_bmp);
    let _ = DeleteObject(b.bmp.into());
    let _ = DeleteDC(b.mem);
    let _ = EndPaint(b.hwnd, &b.ps);
}

unsafe fn fill(mem: HDC, rect: &RECT, color: u32) {
    let brush = CreateSolidBrush(COLORREF(color));
    FillRect(mem, rect, brush);
    let _ = DeleteObject(brush.into());
}

unsafe fn make_font(height: i32, weight: i32) -> HFONT {
    CreateFontW(
        -height, 0, 0, 0, weight, 0, 0, 0,
        DEFAULT_CHARSET, FONT_OUTPUT_PRECISION(0), FONT_CLIP_PRECISION(0),
        CLEARTYPE_QUALITY, 0, w!("Segoe UI"),
    )
}

unsafe fn draw_text(mem: HDC, text: &str, rect: RECT, color: u32, format: windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT) {
    SetTextColor(mem, COLORREF(color));
    let mut buf: Vec<u16> = text.encode_utf16().collect();
    let mut rc = rect;
    DrawTextW(mem, &mut buf, &mut rc, format);
}

unsafe fn paint_bar(hwnd: HWND) {
    let b = begin_buffered(hwnd);
    let (mem, w, h) = (b.mem, b.w, b.h);
    let st = style();
    let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
    let l = bar_layout(scale, h);

    fill(mem, &RECT { left: 0, top: 0, right: w, bottom: h }, st.bg);
    let font = make_font((h - l.pad) * 55 / 100, FW_NORMAL.0 as i32);
    let old_font = SelectObject(mem, font.into());

    let monitor = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    let snap = crate::with_wm(|wm| wm.bar_snapshot(monitor)).flatten();

    // Workspace cells (the active one widens to show its name)
    let top = (h - l.cell_h) / 2;
    let mut cells_end = l.pad;
    if let Some(s) = &snap {
        let spans = cell_spans(&l, s, |t| unsafe { text_width(mem, t) });
        for (i, (cx, cw)) in spans.iter().enumerate() {
            let cell = RECT { left: *cx, top, right: cx + cw, bottom: top + l.cell_h };
            let (fill_c, text_c) = if s.active == i {
                (st.accent, st.bg)
            } else if s.occupied[i] {
                (st.cell_occupied, st.fg)
            } else {
                (st.cell_empty, st.fg)
            };
            fill(mem, &cell, fill_c);
            let name = s.names.get(i).map(String::as_str).unwrap_or("");
            let label = if name.is_empty() {
                format!("{}", i + 1)
            } else {
                format!("{}  {}", i + 1, name)
            };
            draw_text(mem, &label, cell, text_c, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
            cells_end = cx + cw;
        }
    }
    let cells_end = cells_end + l.pad;

    // Right side: [clock][»][≡][?]
    let buttons_left = w - 3 * l.help_w - 2 * l.gap;
    let t = GetLocalTime();
    let crc = RECT { left: buttons_left - l.clock_w, top: 0, right: buttons_left - l.pad / 2, bottom: h };
    draw_text(mem, &format!("{:02}:{:02}", t.wHour, t.wMinute), crc, st.fg, DT_RIGHT | DT_VCENTER | DT_SINGLELINE);

    // Focused window title (or paused notice)
    let title = match &snap {
        Some(s) if s.paused => "⏸  paused — resume with the toggle_pause key".to_string(),
        Some(s) => s.title.clone(),
        None => String::new(),
    };
    if !title.is_empty() {
        let trc = RECT { left: cells_end, top: 0, right: buttons_left - l.clock_w - l.pad, bottom: h };
        draw_text(mem, &title, trc, st.fg, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
    }

    // "»" launcher, "≡" settings and "?" help buttons
    let launcher_cell = RECT { left: buttons_left, top, right: w - 2 * l.help_w - 2 * l.gap, bottom: top + l.cell_h };
    fill(mem, &launcher_cell, st.cell_empty);
    draw_text(mem, "»", launcher_cell, st.accent, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    let settings_cell = RECT { left: w - 2 * l.help_w - l.gap, top, right: w - l.help_w - l.gap, bottom: top + l.cell_h };
    fill(mem, &settings_cell, st.cell_empty);
    draw_text(mem, "≡", settings_cell, st.accent, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    let help_cell = RECT { left: w - l.help_w, top, right: w - l.pad / 2, bottom: top + l.cell_h };
    fill(mem, &help_cell, st.cell_empty);
    draw_text(mem, "?", help_cell, st.accent, DT_CENTER | DT_VCENTER | DT_SINGLELINE);

    SelectObject(mem, old_font);
    let _ = DeleteObject(font.into());
    end_buffered(b);
}

// ---------------------------------------------------------- help panel core

fn close_help() {
    HELP.with(|h| {
        let mut state = h.borrow_mut();
        if state.hwnd != 0 {
            let hwnd = state.hwnd;
            *state = HelpState::default();
            drop(state);
            let _ = unsafe { DestroyWindow(HWND(hwnd as *mut c_void)) };
        }
    });
}

fn help_hwnd() -> Option<HWND> {
    HELP.with(|h| {
        let hwnd = h.borrow().hwnd;
        (hwnd != 0).then(|| HWND(hwnd as *mut c_void))
    })
}

fn invalidate_help() {
    if let Some(hwnd) = help_hwnd() {
        let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
    }
}

/// Panel row geometry — shared by paint and click hit-testing.
struct PanelMetrics {
    pad: i32,
    line_h: i32,
    rows_top: i32,
}

fn panel_metrics(scale: f32) -> PanelMetrics {
    let s = |v: i32| (v as f32 * scale) as i32;
    let pad = s(20);
    let line_h = s(26);
    // title row + search row + a small separator
    PanelMetrics { pad, line_h, rows_top: pad + line_h * 2 + s(8) }
}

fn filtered_entries() -> Vec<HelpEntry> {
    let query = HELP.with(|h| h.borrow().query.clone());
    KEYBINDS.with(|k| {
        k.borrow()
            .iter()
            .filter(|e| {
                query.is_empty()
                    || fuzzy_match(&format!("{} {} {}", e.action, e.chord, e.desc), &query)
            })
            .cloned()
            .collect()
    })
}

/// Lazy subsequence match, case-insensitive: "swl" hits "swap_left".
fn fuzzy_match(haystack: &str, needle: &str) -> bool {
    let mut hay = haystack.chars().flat_map(|c| c.to_lowercase());
    needle
        .chars()
        .flat_map(|c| c.to_lowercase())
        .filter(|c| !c.is_whitespace())
        .all(|n| hay.any(|h| h == n))
}

/// Open or close the keybindings panel (bar "?" button or show_help key).
pub fn toggle_help_panel() {
    if help_hwnd().is_some() {
        close_help();
        return;
    }
    let monitors = monitor::enumerate();
    let Some(m) = monitors.first() else { return };
    unsafe {
        let entries = KEYBINDS.with(|k| k.borrow().len()) as i32;
        // Estimate scale from a bar if present; corrected visually by DPI-aware fonts.
        let scale = BARS.with(|b| {
            b.borrow()
                .first()
                .map(|bar| GetDpiForWindow(HWND(bar.hwnd as *mut c_void)) as f32 / 96.0)
                .unwrap_or(1.0)
        });
        let s = |v: i32| (v as f32 * scale) as i32;
        let pm = panel_metrics(scale);
        let w = s(600);
        let h = (pm.rows_top + pm.line_h * (entries + 2) + pm.pad).min(m.bounds.h - s(80));
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        if let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            w!("wtm_help"),
            w!("wtm keybindings"),
            WS_POPUP | WS_VISIBLE,
            m.bounds.x + (m.bounds.w - w) / 2,
            m.bounds.y + (m.bounds.h - h) / 2,
            w,
            h,
            None,
            None,
            Some(hinstance.into()),
            None,
        ) {
            HELP.with(|st| st.borrow_mut().hwnd = hwnd.0 as isize);
            // Give it keyboard focus so search & rebinding work.
            Window::from_hwnd(hwnd).focus();
        }
    }
}

// ------------------------------------------------------------ help wndproc

fn key_down(vk: u32) -> bool {
    (unsafe { GetKeyState(vk as i32) } as u16 & 0x8000) != 0
}

unsafe extern "system" fn help_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint_help(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_LBUTTONDOWN => {
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            on_panel_click(hwnd, y);
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            on_panel_key(wparam.0 as u32);
            LRESULT(0)
        }
        WM_CHAR => {
            on_panel_char(wparam.0 as u32);
            LRESULT(0)
        }
        WM_SYSCHAR => LRESULT(0),
        WM_KILLFOCUS => {
            close_help();
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn on_panel_click(hwnd: HWND, y: i32) {
    let scale = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0;
    let pm = panel_metrics(scale);
    if y < pm.rows_top {
        return;
    }
    let idx = ((y - pm.rows_top) / pm.line_h) as usize;
    let entries = filtered_entries();
    if let Some(entry) = entries.get(idx) {
        HELP.with(|h| {
            let mut st = h.borrow_mut();
            st.rebinding = Some(entry.action.clone());
            st.message.clear();
        });
        invalidate_help();
    }
}

fn on_panel_char(ch: u32) {
    let rebinding = HELP.with(|h| h.borrow().rebinding.is_some());
    if rebinding {
        return; // chord capture happens on WM_KEYDOWN
    }
    HELP.with(|h| {
        let mut st = h.borrow_mut();
        match ch {
            0x08 => {
                st.query.pop();
            }
            0x20..=0x7E => st.query.push(char::from_u32(ch).unwrap_or(' ')),
            _ => return,
        }
        st.message.clear();
    });
    invalidate_help();
}

fn on_panel_key(vk: u32) {
    let rebinding = HELP.with(|h| h.borrow().rebinding.clone());
    match rebinding {
        None => {
            if vk == VK_ESCAPE.0 as u32 {
                close_help();
            }
        }
        Some(action) => {
            if vk == VK_ESCAPE.0 as u32 {
                HELP.with(|h| {
                    let mut st = h.borrow_mut();
                    st.rebinding = None;
                    st.message.clear();
                });
                invalidate_help();
                return;
            }
            // Wait for a non-modifier key; read modifiers live.
            if matches!(vk, x if x == VK_SHIFT.0 as u32 || x == VK_CONTROL.0 as u32
                || x == VK_MENU.0 as u32 || x == VK_LWIN.0 as u32 || x == VK_RWIN.0 as u32)
            {
                return;
            }
            apply_rebind(&action, vk);
        }
    }
}

fn apply_rebind(action: &str, vk: u32) {
    let set_message = |msg: &str| {
        HELP.with(|h| h.borrow_mut().message = msg.to_string());
        invalidate_help();
    };
    let alt = key_down(VK_MENU.0 as u32);
    let ctrl = key_down(VK_CONTROL.0 as u32);
    let shift = key_down(VK_SHIFT.0 as u32);
    let win = key_down(VK_LWIN.0 as u32) || key_down(VK_RWIN.0 as u32);
    if !(alt || ctrl || win) {
        set_message("use at least one of Alt / Ctrl / Win");
        return;
    }
    let numbered = keys::is_numbered(action);
    let Some(chord) = keys::format_captured(alt, ctrl, shift, win, vk, numbered) else {
        set_message("that key is not supported");
        return;
    };
    if keys::parse_chord(&keys::expand(&chord, 1)).is_none() {
        set_message("that key is not supported");
        return;
    }
    // Conflict check against every other binding.
    let conflict = KEYBINDS.with(|k| {
        k.borrow()
            .iter()
            .find(|e| e.action != action && keys::chords_overlap(&chord, &e.chord))
            .map(|e| e.action.clone())
    });
    if let Some(other) = conflict {
        set_message(&format!("conflicts with {other}"));
        return;
    }
    // Commit: panel list, live config, config file, hotkey registration.
    KEYBINDS.with(|k| {
        for e in k.borrow_mut().iter_mut() {
            if e.action == action {
                e.chord = chord.clone();
            }
        }
    });
    crate::with_wm(|wm| {
        wm.config.keybindings.insert(action.to_string(), chord.clone());
    });
    let saved = match crate::config::save_keybinding(action, &chord) {
        Ok(()) => format!("{action} = {chord}  ✓ saved"),
        Err(e) => format!("{action} = {chord}  (config not saved: {e})"),
    };
    crate::reregister_hotkeys();
    HELP.with(|h| {
        let mut st = h.borrow_mut();
        st.rebinding = None;
        st.message = saved;
    });
    invalidate_help();
}

// --------------------------------------------------------- launcher panel

fn close_launcher() {
    LAUNCHER.with(|s| {
        let mut st = s.borrow_mut();
        if st.hwnd != 0 {
            let hwnd = st.hwnd;
            *st = LauncherState::default();
            drop(st);
            let _ = unsafe { DestroyWindow(HWND(hwnd as *mut c_void)) };
        }
    });
}

fn launcher_hwnd() -> Option<HWND> {
    LAUNCHER.with(|s| {
        let hwnd = s.borrow().hwnd;
        (hwnd != 0).then(|| HWND(hwnd as *mut c_void))
    })
}

fn invalidate_launcher() {
    if let Some(hwnd) = launcher_hwnd() {
        let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
    }
}

fn launcher_entries() -> Vec<LauncherEntry> {
    crate::with_wm(|wm| wm.config.launcher.clone()).unwrap_or_default()
}

/// Filtered entries paired with their index in config.launcher (needed so
/// key assignment and removal survive filtering).
fn launcher_filtered() -> Vec<(usize, LauncherEntry)> {
    let query = LAUNCHER.with(|s| s.borrow().query.clone());
    launcher_entries()
        .into_iter()
        .enumerate()
        .filter(|(_, e)| {
            query.is_empty() || fuzzy_match(&format!("{} {}", e.name, e.command), &query)
        })
        .collect()
}

/// Launch config.launcher[i] — used by per-app global shortcuts.
pub fn launch_index(i: usize) {
    let entry = crate::with_wm(|wm| wm.config.launcher.get(i).cloned()).flatten();
    if let Some(e) = entry {
        crate::logln!("wtm: launching {}", e.name);
        run_command(&e.command, &e.args, &e.dir);
    }
}

/// Expand %ENV_VARS% in a path/command string.
fn expand_env(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    let mut buf = vec![0u16; 2048];
    let n = unsafe { ExpandEnvironmentStringsW(PCWSTR(wide.as_ptr()), Some(&mut buf)) };
    if n == 0 || n as usize > buf.len() {
        return s.to_string();
    }
    String::from_utf16_lossy(&buf[..n as usize - 1])
}

fn run_command(command: &str, args: &str, dir: &str) {
    let file: Vec<u16> =
        expand_env(command).encode_utf16().chain(std::iter::once(0)).collect();
    let args_w: Vec<u16> = expand_env(args).encode_utf16().chain(std::iter::once(0)).collect();
    let dir_w: Vec<u16> = expand_env(dir).encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(file.as_ptr()),
            if args.is_empty() { PCWSTR::null() } else { PCWSTR(args_w.as_ptr()) },
            if dir.is_empty() { PCWSTR::null() } else { PCWSTR(dir_w.as_ptr()) },
            SW_SHOWNORMAL,
        );
    }
}

fn launcher_launch(entry: &LauncherEntry) {
    crate::logln!("wtm: launching {}", entry.name);
    run_command(&entry.command, &entry.args, &entry.dir);
    close_launcher();
}

/// Standard Windows file-open dialog (modal). Returns the chosen path.
fn pick_file(owner: HWND) -> Option<String> {
    let filter: Vec<u16> = "Programs & scripts (*.exe;*.bat;*.cmd;*.lnk)\0*.exe;*.bat;*.cmd;*.lnk\0All files (*.*)\0*.*\0\0"
        .encode_utf16()
        .collect();
    let mut buf = [0u16; 2048];
    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner,
        lpstrFilter: PCWSTR(filter.as_ptr()),
        lpstrFile: PWSTR(buf.as_mut_ptr()),
        nMaxFile: buf.len() as u32,
        lpstrTitle: w!("Choose an application, script or file"),
        Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
        ..Default::default()
    };
    let ok = unsafe { GetOpenFileNameW(&mut ofn) }.as_bool();
    if !ok {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
    (len > 0).then(|| String::from_utf16_lossy(&buf[..len]))
}

/// "C:\Projects\ProjectA\evars.bat" -> "evars" (default display name).
fn file_stem(path: &str) -> String {
    let base = path.rsplit(['\\', '/']).next().unwrap_or(path);
    base.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(base).to_string()
}

fn add_app_flow() {
    let Some(hwnd) = launcher_hwnd() else { return };
    LAUNCHER.with(|s| s.borrow_mut().dialog_open = true);
    let picked = pick_file(hwnd);
    LAUNCHER.with(|s| s.borrow_mut().dialog_open = false);
    if let Some(h) = launcher_hwnd() {
        Window::from_hwnd(h).focus();
    }
    if let Some(path) = picked {
        LAUNCHER.with(|s| {
            let mut st = s.borrow_mut();
            st.name_buf = file_stem(&path);
            st.adding = Some(path);
        });
    }
    invalidate_launcher();
}

fn commit_new_app() {
    let (path, name) = LAUNCHER.with(|s| {
        let st = s.borrow();
        (st.adding.clone(), st.name_buf.trim().to_string())
    });
    let Some(path) = path else { return };
    let name = if name.is_empty() { file_stem(&path) } else { name };
    crate::with_wm(|wm| {
        wm.config.launcher.push(LauncherEntry {
            name,
            command: path,
            args: String::new(),
            dir: String::new(),
            key: String::new(),
        });
        if let Err(e) = crate::config::save_launcher(&wm.config.launcher) {
            crate::logln!("wtm: could not save config: {e}");
        }
    });
    LAUNCHER.with(|s| {
        let mut st = s.borrow_mut();
        st.adding = None;
        st.name_buf.clear();
        st.query.clear();
        st.selected = 0;
        st.message = "added — click its key chip to assign a shortcut".into();
    });
    crate::reregister_hotkeys();
    invalidate_launcher();
}

fn remove_entry(entry: &LauncherEntry) {
    crate::with_wm(|wm| {
        wm.config
            .launcher
            .retain(|e| !(e.name == entry.name && e.command == entry.command));
        if let Err(e) = crate::config::save_launcher(&wm.config.launcher) {
            crate::logln!("wtm: could not save config: {e}");
        }
    });
    LAUNCHER.with(|s| s.borrow_mut().selected = 0);
    crate::reregister_hotkeys(); // Launch(i) indices shifted
    invalidate_launcher();
}

pub fn toggle_launcher_panel() {
    if launcher_hwnd().is_some() {
        close_launcher();
        return;
    }
    let monitors = monitor::enumerate();
    let Some(m) = monitors.first() else { return };
    unsafe {
        let scale = BARS.with(|b| {
            b.borrow()
                .first()
                .map(|bar| GetDpiForWindow(HWND(bar.hwnd as *mut c_void)) as f32 / 96.0)
                .unwrap_or(1.0)
        });
        let s = |v: i32| (v as f32 * scale) as i32;
        let line_h = s(28);
        let rows = (launcher_entries().len() as i32).clamp(1, 14);
        let w = s(560);
        let h = s(20) * 2 + line_h * (rows + 2);
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        if let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            w!("wtm_launcher"),
            w!("wtm launcher"),
            WS_POPUP | WS_VISIBLE,
            m.bounds.x + (m.bounds.w - w) / 2,
            m.bounds.y + m.bounds.h / 5, // rofi-style: upper third
            w,
            h,
            None,
            None,
            Some(hinstance.into()),
            None,
        ) {
            LAUNCHER.with(|st| st.borrow_mut().hwnd = hwnd.0 as isize);
            Window::from_hwnd(hwnd).focus();
        }
    }
}

unsafe extern "system" fn launcher_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint_launcher(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            on_launcher_click(hwnd, x, y);
            LRESULT(0)
        }
        // Alt-modified keys arrive as WM_SYSKEYDOWN, not WM_KEYDOWN —
        // both must feed the shortcut capture.
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            on_launcher_key(wparam.0 as u32);
            LRESULT(0)
        }
        WM_CHAR => {
            on_launcher_char(wparam.0 as u32);
            LRESULT(0)
        }
        WM_SYSCHAR => LRESULT(0), // swallow Alt+letter menu beeps
        WM_KILLFOCUS => {
            if !LAUNCHER.with(|s| s.borrow().dialog_open) {
                close_launcher();
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn on_launcher_click(hwnd: HWND, x: i32, y: i32) {
    if LAUNCHER.with(|s| s.borrow().adding.is_some()) {
        return; // naming in progress; keyboard only
    }
    let mut rc = RECT::default();
    let _ = unsafe { GetClientRect(hwnd, &mut rc) };
    let scale = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0;
    let s = |v: i32| (v as f32 * scale) as i32;
    let rows_top = s(20) + s(28) + s(6);
    if y < rows_top {
        return;
    }
    let idx = ((y - rows_top) / s(28)) as usize;
    let filtered = launcher_filtered();
    let query = LAUNCHER.with(|s| s.borrow().query.trim().to_string());
    // Mirrors the painter: entries, then a "run query" hint row when the
    // filter is empty, then the "+ add app…" row.
    let hint_rows = usize::from(filtered.is_empty() && !query.is_empty());
    if idx < filtered.len() {
        let (cfg_idx, entry) = &filtered[idx];
        if x >= rc.right - s(44) {
            remove_entry(entry);
        } else if x >= rc.right - s(160) {
            // The key chip: capture a new shortcut for this entry.
            LAUNCHER.with(|st| {
                let mut st = st.borrow_mut();
                st.capturing = Some(*cfg_idx);
                st.message.clear();
            });
            invalidate_launcher();
        } else {
            launcher_launch(entry);
        }
    } else if idx == filtered.len() + hint_rows {
        add_app_flow(); // the "+ add app…" row
    }
}

/// A shortcut capture for launcher entry `idx`: Esc cancels, Backspace or
/// Delete clears the shortcut, anything else with Alt/Ctrl/Win assigns it.
fn handle_key_capture(idx: usize, vk: u32) {
    let finish = |message: String| {
        LAUNCHER.with(|s| {
            let mut st = s.borrow_mut();
            st.capturing = None;
            st.message = message;
        });
        invalidate_launcher();
    };
    if vk == VK_ESCAPE.0 as u32 {
        finish(String::new());
        return;
    }
    if vk == VK_BACK.0 as u32 || vk == VK_DELETE.0 as u32 {
        crate::with_wm(|wm| {
            if let Some(e) = wm.config.launcher.get_mut(idx) {
                e.key.clear();
            }
            let _ = crate::config::save_launcher(&wm.config.launcher);
        });
        crate::reregister_hotkeys();
        finish("shortcut cleared".into());
        return;
    }
    if matches!(vk, x if x == VK_SHIFT.0 as u32 || x == VK_CONTROL.0 as u32
        || x == VK_MENU.0 as u32 || x == VK_LWIN.0 as u32 || x == VK_RWIN.0 as u32)
    {
        return; // wait for the non-modifier key
    }
    let alt = key_down(VK_MENU.0 as u32);
    let ctrl = key_down(VK_CONTROL.0 as u32);
    let shift = key_down(VK_SHIFT.0 as u32);
    let win = key_down(VK_LWIN.0 as u32) || key_down(VK_RWIN.0 as u32);
    if !(alt || ctrl || win) {
        LAUNCHER.with(|s| s.borrow_mut().message = "use at least one of Alt / Ctrl / Win".into());
        invalidate_launcher();
        return;
    }
    let Some(chord) = keys::format_captured(alt, ctrl, shift, win, vk, false) else {
        LAUNCHER.with(|s| s.borrow_mut().message = "that key is not supported".into());
        invalidate_launcher();
        return;
    };
    // Conflicts against action bindings and other app shortcuts.
    let conflict = crate::with_wm(|wm| {
        for (action, c) in &wm.config.keybindings {
            if keys::chords_overlap(c, &chord) {
                return Some(action.clone());
            }
        }
        for (j, e) in wm.config.launcher.iter().enumerate() {
            if j != idx && !e.key.is_empty() && keys::chords_overlap(&e.key, &chord) {
                return Some(e.name.clone());
            }
        }
        None
    })
    .flatten();
    if let Some(other) = conflict {
        LAUNCHER.with(|s| s.borrow_mut().message = format!("conflicts with {other}"));
        invalidate_launcher();
        return;
    }
    let saved = crate::with_wm(|wm| {
        let name = wm.config.launcher.get(idx).map(|e| e.name.clone()).unwrap_or_default();
        if let Some(e) = wm.config.launcher.get_mut(idx) {
            e.key = chord.clone();
        }
        if let Err(e) = crate::config::save_launcher(&wm.config.launcher) {
            crate::logln!("wtm: could not save config: {e}");
        }
        name
    })
    .unwrap_or_default();
    crate::reregister_hotkeys();
    finish(format!("{chord}  →  {saved}  ✓"));
}

fn on_launcher_char(ch: u32) {
    if LAUNCHER.with(|s| s.borrow().capturing.is_some()) {
        return; // shortcut capture happens on WM_KEYDOWN
    }
    LAUNCHER.with(|s| {
        let mut st = s.borrow_mut();
        st.message.clear();
        let buf = if st.adding.is_some() { &mut st.name_buf } else { &mut st.query };
        match ch {
            0x08 => {
                buf.pop();
            }
            c if c >= 0x20 => {
                if let Some(c) = char::from_u32(c) {
                    buf.push(c);
                }
            }
            _ => return,
        }
        if st.adding.is_none() {
            st.selected = 0;
        }
    });
    invalidate_launcher();
}

fn on_launcher_key(vk: u32) {
    if let Some(idx) = LAUNCHER.with(|s| s.borrow().capturing) {
        handle_key_capture(idx, vk);
        return;
    }
    // Naming a newly browsed app: Enter saves, Esc cancels.
    if LAUNCHER.with(|s| s.borrow().adding.is_some()) {
        if vk == VK_RETURN.0 as u32 {
            commit_new_app();
        } else if vk == VK_ESCAPE.0 as u32 {
            LAUNCHER.with(|s| {
                let mut st = s.borrow_mut();
                st.adding = None;
                st.name_buf.clear();
            });
            invalidate_launcher();
        }
        return;
    }
    if vk == VK_ESCAPE.0 as u32 {
        close_launcher();
        return;
    }
    let filtered = launcher_filtered();
    if vk == VK_DOWN.0 as u32 || vk == VK_UP.0 as u32 {
        LAUNCHER.with(|s| {
            let mut st = s.borrow_mut();
            let n = filtered.len().max(1) as i32;
            let dir = if vk == VK_DOWN.0 as u32 { 1 } else { -1 };
            st.selected = (st.selected as i32 + dir).rem_euclid(n) as usize;
        });
        invalidate_launcher();
        return;
    }
    if vk == VK_RETURN.0 as u32 {
        let selected = LAUNCHER.with(|s| s.borrow().selected);
        if let Some((_, entry)) = filtered.get(selected) {
            launcher_launch(entry);
        } else {
            // No match: run whatever was typed ("notepad", a path, a URL).
            let query = LAUNCHER.with(|s| s.borrow().query.clone());
            if !query.trim().is_empty() {
                crate::logln!("wtm: running \"{query}\"");
                run_command(query.trim(), "", "");
                close_launcher();
            }
        }
    }
}

unsafe fn paint_launcher(hwnd: HWND) {
    let b = begin_buffered(hwnd);
    let (mem, w, h) = (b.mem, b.w, b.h);
    let st = style();
    let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
    let s = |v: i32| (v as f32 * scale) as i32;
    let (pad, line_h) = (s(20), s(28));

    fill(mem, &RECT { left: 0, top: 0, right: w, bottom: h }, st.bg);
    for edge in [
        RECT { left: 0, top: 0, right: w, bottom: s(2) },
        RECT { left: 0, top: h - s(2), right: w, bottom: h },
        RECT { left: 0, top: 0, right: s(2), bottom: h },
        RECT { left: w - s(2), top: 0, right: w, bottom: h },
    ] {
        fill(mem, &edge, st.accent);
    }

    let bold = make_font(s(16), FW_BOLD.0 as i32);
    let normal = make_font(s(15), FW_NORMAL.0 as i32);
    let old_font = SelectObject(mem, bold.into());

    let (query, selected, adding, name_buf, capturing, message) = LAUNCHER.with(|s| {
        let st = s.borrow();
        (
            st.query.clone(),
            st.selected,
            st.adding.clone(),
            st.name_buf.clone(),
            st.capturing,
            st.message.clone(),
        )
    });

    // Naming mode: browsed a file, now typing its display name.
    if let Some(path) = &adding {
        SelectObject(mem, bold.into());
        let nrc = RECT { left: pad, top: pad, right: w - pad, bottom: pad + line_h };
        draw_text(mem, &format!("name:  {name_buf}_"), nrc, st.fg, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
        SelectObject(mem, normal.into());
        let prc = RECT { left: pad, top: pad + line_h, right: w - pad, bottom: pad + line_h * 2 };
        draw_text(mem, path, prc, st.cell_occupied, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
        let hrc = RECT { left: pad, top: h - line_h - s(6), right: w - pad, bottom: h - s(6) };
        draw_text(mem, "Enter saves · Esc cancels", hrc, st.cell_occupied, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
        SelectObject(mem, old_font);
        let _ = DeleteObject(bold.into());
        let _ = DeleteObject(normal.into());
        end_buffered(b);
        return;
    }

    // Search line
    let src = RECT { left: pad, top: pad, right: w - pad, bottom: pad + line_h };
    if query.is_empty() {
        draw_text(mem, "»  type to search apps…", src, st.cell_occupied, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    } else {
        draw_text(mem, &format!("»  {query}_"), src, st.fg, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    }

    let entries = launcher_filtered();
    let rows_top = pad + line_h + s(6);
    let mut y = rows_top;
    for (i, (cfg_idx, e)) in entries.iter().enumerate() {
        if y + line_h > h - s(8) {
            break;
        }
        if i == selected {
            let row = RECT { left: s(6), top: y, right: w - s(6), bottom: y + line_h };
            fill(mem, &row, st.cell_occupied);
        }
        SelectObject(mem, bold.into());
        let nrc = RECT { left: pad, top: y, right: w * 2 / 5, bottom: y + line_h };
        draw_text(mem, &e.name, nrc, st.accent, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
        SelectObject(mem, normal.into());
        let crc = RECT { left: w * 2 / 5 + s(8), top: y, right: w - s(164), bottom: y + line_h };
        draw_text(mem, &e.command, crc, st.fg, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
        // Key chip: an outlined button so it reads as clickable on any row.
        let krc = RECT { left: w - s(158), top: y + s(3), right: w - s(48), bottom: y + line_h - s(3) };
        let (label, text_color, border_color) = if capturing == Some(*cfg_idx) {
            ("press keys…".to_string(), st.accent, st.accent)
        } else if !e.key.is_empty() {
            (e.key.clone(), st.fg, st.fg)
        } else {
            ("+ set key".to_string(), st.accent, st.cell_occupied)
        };
        draw_chip(mem, krc, st.bg, border_color);
        draw_text(mem, &label, krc, text_color, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
        let xrc = RECT { left: w - s(44), top: y, right: w - pad, bottom: y + line_h };
        draw_text(mem, "✕", xrc, st.fg, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        y += line_h;
    }
    if entries.is_empty() && !query.trim().is_empty() {
        SelectObject(mem, normal.into());
        let erc = RECT { left: pad, top: y, right: w - pad, bottom: y + line_h };
        draw_text(mem, &format!("↵  run \"{}\"", query.trim()), erc, st.fg, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
        y += line_h;
    }
    // "+ add app…" row (opens the file browser)
    if y + line_h <= h - s(8) {
        SelectObject(mem, normal.into());
        let arc = RECT { left: pad, top: y, right: w - pad, bottom: y + line_h };
        draw_text(mem, "+  add app (browse…)", arc, st.accent, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
        if !message.is_empty() {
            draw_text(mem, &message, arc, st.fg, DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
        }
    }

    SelectObject(mem, old_font);
    let _ = DeleteObject(bold.into());
    let _ = DeleteObject(normal.into());
    end_buffered(b);
}

// ---------------------------------------------------------- settings panel

fn close_settings() {
    SETTINGS.with(|s| {
        let mut st = s.borrow_mut();
        if st.hwnd != 0 {
            let hwnd = st.hwnd;
            *st = SettingsState::default();
            drop(st);
            let _ = unsafe { DestroyWindow(HWND(hwnd as *mut c_void)) };
        }
    });
}

fn settings_hwnd() -> Option<HWND> {
    SETTINGS.with(|s| {
        let hwnd = s.borrow().hwnd;
        (hwnd != 0).then(|| HWND(hwnd as *mut c_void))
    })
}

fn invalidate_settings() {
    if let Some(hwnd) = settings_hwnd() {
        let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
    }
}

fn settings_items() -> Vec<SettingsItem> {
    let (count, rules) =
        crate::with_wm(|wm| (wm.config.workspaces, wm.config.app_rules.clone()))
            .unwrap_or((9, Default::default()));
    let mut items: Vec<SettingsItem> = (0..count).map(SettingsItem::Ws).collect();
    items.push(SettingsItem::RulesHeader);
    if rules.is_empty() {
        items.push(SettingsItem::NoRules);
    } else {
        for (exe, n) in rules {
            items.push(SettingsItem::Rule(exe, n));
        }
    }
    items
}

fn settings_rows_top(scale: f32) -> i32 {
    let s = |v: i32| (v as f32 * scale) as i32;
    s(20) + s(26) + s(8) // pad + title row + separator
}

pub fn toggle_settings_panel() {
    if settings_hwnd().is_some() {
        close_settings();
        return;
    }
    let monitors = monitor::enumerate();
    let Some(m) = monitors.first() else { return };
    unsafe {
        let scale = BARS.with(|b| {
            b.borrow()
                .first()
                .map(|bar| GetDpiForWindow(HWND(bar.hwnd as *mut c_void)) as f32 / 96.0)
                .unwrap_or(1.0)
        });
        let s = |v: i32| (v as f32 * scale) as i32;
        let line_h = s(26);
        let items = settings_items().len() as i32;
        let w = s(520);
        let h = (settings_rows_top(scale) + line_h * (items + 2) + s(20)).min(m.bounds.h - s(80));
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        if let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            w!("wtm_settings"),
            w!("wtm settings"),
            WS_POPUP | WS_VISIBLE,
            m.bounds.x + (m.bounds.w - w) / 2,
            m.bounds.y + (m.bounds.h - h) / 2,
            w,
            h,
            None,
            None,
            Some(hinstance.into()),
            None,
        ) {
            SETTINGS.with(|st| st.borrow_mut().hwnd = hwnd.0 as isize);
            Window::from_hwnd(hwnd).focus();
        }
    }
}

unsafe extern "system" fn settings_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint_settings(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_LBUTTONDOWN => {
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            on_settings_click(hwnd, y);
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            on_settings_key(wparam.0 as u32);
            LRESULT(0)
        }
        WM_CHAR => {
            on_settings_char(wparam.0 as u32);
            LRESULT(0)
        }
        WM_SYSCHAR => LRESULT(0),
        WM_KILLFOCUS => {
            close_settings();
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn on_settings_click(hwnd: HWND, y: i32) {
    let scale = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0;
    let line_h = (26.0 * scale) as i32;
    let top = settings_rows_top(scale);
    if y < top {
        return;
    }
    let idx = ((y - top) / line_h) as usize;
    match settings_items().get(idx) {
        Some(SettingsItem::Ws(i)) => {
            let i = *i;
            let current = crate::with_wm(|wm| {
                wm.config.workspace_names.get(i).cloned().unwrap_or_default()
            })
            .unwrap_or_default();
            SETTINGS.with(|s| {
                let mut st = s.borrow_mut();
                st.editing = Some(i);
                st.buffer = current;
            });
            invalidate_settings();
        }
        Some(SettingsItem::Rule(exe, _)) => {
            let exe = exe.clone();
            crate::with_wm(|wm| {
                wm.config.app_rules.remove(&exe);
            });
            if let Err(e) = crate::config::save_app_rule(&exe, None) {
                crate::logln!("wtm: could not save config: {e}");
            }
            invalidate_settings();
        }
        _ => {}
    }
}

fn on_settings_char(ch: u32) {
    let editing = SETTINGS.with(|s| s.borrow().editing.is_some());
    if !editing {
        return;
    }
    SETTINGS.with(|s| {
        let mut st = s.borrow_mut();
        match ch {
            0x08 => {
                st.buffer.pop();
            }
            0x0D => return, // Enter handled on WM_KEYDOWN
            c if c >= 0x20 => {
                if let Some(c) = char::from_u32(c) {
                    st.buffer.push(c);
                }
            }
            _ => return,
        }
    });
    invalidate_settings();
}

fn on_settings_key(vk: u32) {
    let editing = SETTINGS.with(|s| s.borrow().editing);
    if vk == VK_ESCAPE.0 as u32 {
        if editing.is_some() {
            SETTINGS.with(|s| {
                let mut st = s.borrow_mut();
                st.editing = None;
                st.buffer.clear();
            });
            invalidate_settings();
        } else {
            close_settings();
        }
        return;
    }
    if vk == VK_RETURN.0 as u32 {
        if let Some(i) = editing {
            let name = SETTINGS.with(|s| s.borrow().buffer.trim().to_string());
            crate::with_wm(|wm| {
                let count = wm.config.workspaces;
                if wm.config.workspace_names.len() < count {
                    wm.config.workspace_names.resize(count, String::new());
                }
                if i < wm.config.workspace_names.len() {
                    wm.config.workspace_names[i] = name;
                }
                if let Err(e) = crate::config::save_workspace_names(&wm.config.workspace_names) {
                    crate::logln!("wtm: could not save config: {e}");
                }
            });
            SETTINGS.with(|s| {
                let mut st = s.borrow_mut();
                st.editing = None;
                st.buffer.clear();
            });
            invalidate_settings();
            invalidate_all(); // bar shows the new name
        }
    }
}

unsafe fn paint_settings(hwnd: HWND) {
    let b = begin_buffered(hwnd);
    let (mem, w, h) = (b.mem, b.w, b.h);
    let st = style();
    let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
    let s = |v: i32| (v as f32 * scale) as i32;
    let (pad, line_h) = (s(20), s(26));

    fill(mem, &RECT { left: 0, top: 0, right: w, bottom: h }, st.bg);
    for edge in [
        RECT { left: 0, top: 0, right: w, bottom: s(2) },
        RECT { left: 0, top: h - s(2), right: w, bottom: h },
        RECT { left: 0, top: 0, right: s(2), bottom: h },
        RECT { left: w - s(2), top: 0, right: w, bottom: h },
    ] {
        fill(mem, &edge, st.accent);
    }

    let bold = make_font(s(15), FW_BOLD.0 as i32);
    let normal = make_font(s(15), FW_NORMAL.0 as i32);
    let old_font = SelectObject(mem, bold.into());

    let trc = RECT { left: pad, top: pad, right: w - pad, bottom: pad + line_h };
    draw_text(mem, "wtm settings", trc, st.accent, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    SelectObject(mem, normal.into());
    draw_text(mem, "click a workspace to name it", trc, st.fg, DT_RIGHT | DT_VCENTER | DT_SINGLELINE);

    let (editing, buffer) =
        SETTINGS.with(|s| (s.borrow().editing, s.borrow().buffer.clone()));
    let names = crate::with_wm(|wm| wm.config.workspace_names.clone()).unwrap_or_default();

    let mut y = settings_rows_top(scale);
    for item in settings_items() {
        if y + line_h > h - line_h {
            break;
        }
        match item {
            SettingsItem::Ws(i) => {
                let is_editing = editing == Some(i);
                if is_editing {
                    let row = RECT { left: s(6), top: y, right: w - s(6), bottom: y + line_h };
                    fill(mem, &row, st.cell_empty);
                }
                SelectObject(mem, bold.into());
                let nrc = RECT { left: pad, top: y, right: pad + s(36), bottom: y + line_h };
                draw_text(mem, &format!("{}", i + 1), nrc, st.accent, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
                SelectObject(mem, normal.into());
                let vrc = RECT { left: pad + s(44), top: y, right: w - pad, bottom: y + line_h };
                if is_editing {
                    draw_text(mem, &format!("{buffer}_"), vrc, st.fg, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
                } else {
                    let name = names.get(i).map(String::as_str).unwrap_or("");
                    if name.is_empty() {
                        draw_text(mem, "(unnamed)", vrc, st.cell_occupied, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
                    } else {
                        draw_text(mem, name, vrc, st.fg, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
                    }
                }
            }
            SettingsItem::RulesHeader => {
                SelectObject(mem, bold.into());
                let rrc = RECT { left: pad, top: y, right: w - pad, bottom: y + line_h };
                draw_text(mem, "pinned apps", rrc, st.accent, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
            }
            SettingsItem::Rule(exe, n) => {
                SelectObject(mem, normal.into());
                let lrc = RECT { left: pad, top: y, right: w - pad - s(120), bottom: y + line_h };
                draw_text(mem, &format!("{exe}  →  workspace {n}"), lrc, st.fg, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
                let xrc = RECT { left: w - pad - s(120), top: y, right: w - pad, bottom: y + line_h };
                draw_text(mem, "click to remove", xrc, st.cell_occupied, DT_RIGHT | DT_VCENTER | DT_SINGLELINE);
            }
            SettingsItem::NoRules => {
                SelectObject(mem, normal.into());
                let rrc = RECT { left: pad, top: y, right: w - pad, bottom: y + line_h };
                draw_text(mem, "none — focus an app and press the pin_app key to pin it", rrc, st.cell_occupied, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
            }
        }
        y += line_h;
    }

    let frc = RECT { left: pad, top: h - line_h - s(6), right: w - pad, bottom: h - s(6) };
    draw_text(mem, "Enter saves · Esc closes", frc, st.cell_occupied, DT_LEFT | DT_VCENTER | DT_SINGLELINE);

    SelectObject(mem, old_font);
    let _ = DeleteObject(bold.into());
    let _ = DeleteObject(normal.into());
    end_buffered(b);
}

// ---------------------------------------------------------- panel painting

unsafe fn paint_help(hwnd: HWND) {
    let b = begin_buffered(hwnd);
    let (mem, w, h) = (b.mem, b.w, b.h);
    let st = style();
    let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
    let s = |v: i32| (v as f32 * scale) as i32;
    let pm = panel_metrics(scale);

    fill(mem, &RECT { left: 0, top: 0, right: w, bottom: h }, st.bg);
    for edge in [
        RECT { left: 0, top: 0, right: w, bottom: s(2) },
        RECT { left: 0, top: h - s(2), right: w, bottom: h },
        RECT { left: 0, top: 0, right: s(2), bottom: h },
        RECT { left: w - s(2), top: 0, right: w, bottom: h },
    ] {
        fill(mem, &edge, st.accent);
    }

    let bold = make_font(s(15), FW_BOLD.0 as i32);
    let normal = make_font(s(15), FW_NORMAL.0 as i32);
    let old_font = SelectObject(mem, bold.into());

    let (query, rebinding, message) = HELP.with(|hs| {
        let stt = hs.borrow();
        (stt.query.clone(), stt.rebinding.clone(), stt.message.clone())
    });

    // Title
    let trc = RECT { left: pm.pad, top: pm.pad, right: w - pm.pad, bottom: pm.pad + pm.line_h };
    draw_text(mem, "wtm keybindings", trc, st.accent, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    draw_text(mem, "click a row, then press the new shortcut", trc, st.fg, DT_RIGHT | DT_VCENTER | DT_SINGLELINE);

    // Search line
    SelectObject(mem, normal.into());
    let src = RECT {
        left: pm.pad,
        top: pm.pad + pm.line_h,
        right: w - pm.pad,
        bottom: pm.pad + pm.line_h * 2,
    };
    if query.is_empty() {
        draw_text(mem, "type to search…", src, st.cell_occupied, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    } else {
        draw_text(mem, &format!("search: {query}_"), src, st.fg, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    }

    // Rows
    let entries = filtered_entries();
    let mut y = pm.rows_top;
    for entry in &entries {
        if y + pm.line_h > h - pm.line_h {
            break; // clipped by panel height; search narrows the list
        }
        let is_rebinding = rebinding.as_deref() == Some(entry.action.as_str());
        if is_rebinding {
            let row = RECT { left: s(6), top: y, right: w - s(6), bottom: y + pm.line_h };
            fill(mem, &row, st.cell_empty);
        }
        SelectObject(mem, bold.into());
        let krc = RECT { left: pm.pad, top: y, right: pm.pad + s(210), bottom: y + pm.line_h };
        let chord_text = if is_rebinding { "press keys…" } else { entry.chord.as_str() };
        draw_text(mem, chord_text, krc, st.accent, DT_LEFT | DT_VCENTER | DT_SINGLELINE);

        SelectObject(mem, normal.into());
        let drc = RECT { left: pm.pad + s(220), top: y, right: w - pm.pad, bottom: y + pm.line_h };
        draw_text(mem, &entry.desc, drc, st.fg, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
        y += pm.line_h;
    }
    if entries.is_empty() {
        let erc = RECT { left: pm.pad, top: y, right: w - pm.pad, bottom: y + pm.line_h };
        draw_text(mem, "no match", erc, st.cell_occupied, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    }

    // Footer: status message or hint
    let frc = RECT { left: pm.pad, top: h - pm.line_h - s(6), right: w - pm.pad, bottom: h - s(6) };
    if !message.is_empty() {
        draw_text(mem, &message, frc, st.accent, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
    } else {
        draw_text(mem, "Esc closes · drag a window onto another to swap", frc, st.cell_occupied, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    }

    SelectObject(mem, old_font);
    let _ = DeleteObject(bold.into());
    let _ = DeleteObject(normal.into());
    end_buffered(b);
}

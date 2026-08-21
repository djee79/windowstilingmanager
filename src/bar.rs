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
use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use windows::core::{w, PCWSTR};
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::UI::Controls::Dialogs::{
    ChooseColorW, GetOpenFileNameW, CC_ANYCOLOR, CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW,
    OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows::core::PWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    DWM_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW,
    CreateRoundRectRgn, CreateSolidBrush, DeleteDC, DeleteObject, EndPaint, FillRect, FillRgn,
    GetDC,
    GetTextExtentPoint32W, GetTextFaceW, InvalidateRect, ReleaseDC, SelectObject, SetBkMode,
    SetTextColor,
    DrawTextW, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_RIGHT, DT_SINGLELINE, DT_VCENTER,
    FW_BOLD, FW_NORMAL, HDC, HFONT, HMONITOR, PAINTSTRUCT, SRCCOPY, TRANSPARENT, CLEARTYPE_QUALITY,
    DEFAULT_CHARSET, FONT_OUTPUT_PRECISION, FONT_CLIP_PRECISION,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_ESCAPE, VK_LWIN, VK_MENU,
    VK_RETURN, VK_RWIN, VK_SHIFT, VK_UP,
};
use windows::Win32::UI::Shell::{
    SHAppBarMessage, ShellExecuteW, ABE_TOP, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS,
    APPBARDATA,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyWindow, DrawIconEx, GetClientRect,
    GetWindowLongPtrW, GetWindowRect, KillTimer, LoadCursorW, RegisterClassW, SetTimer,
    SetWindowLongPtrW, SetWindowPos, CS_HREDRAW, CS_VREDRAW, DI_NORMAL, GWLP_USERDATA, HICON,
    HWND_TOPMOST, IDC_ARROW, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_SHOWNORMAL, WM_CHAR, WM_DESTROY,
    WM_ERASEBKGND, WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_PAINT, WM_SYSCHAR, WM_SYSKEYDOWN,
    WM_TIMER, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    SetLayeredWindowAttributes, LWA_ALPHA, WM_MOUSEWHEEL, WS_EX_LAYERED,
};

#[derive(Clone)]
pub(crate) struct BarStyle {
    pub(crate) height: i32,
    pub(crate) bg: u32,
    pub(crate) fg: u32,
    pub(crate) accent: u32,
    pub(crate) cell_empty: u32,
    pub(crate) cell_occupied: u32,
    pub(crate) alpha: u8,
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
    CalHeader,
    CalOff,
    CalOutlook,
    CalIcs,
}

#[derive(Default)]
struct SettingsState {
    hwnd: isize,
    /// Workspace index whose name is being typed.
    editing: Option<usize>,
    /// Typing an .ics path/URL for the meetings calendar.
    editing_cal: bool,
    buffer: String,
}

#[derive(Default)]
struct SwitcherState {
    hwnd: isize,
    query: String,
    selected: usize,
}

#[derive(Default)]
struct ScanState {
    hwnd: isize,
    query: String,
    /// First visible row (mouse-wheel scrolling).
    offset: usize,
    /// Start Menu mode: (name, .lnk path). Bookmarks mode: (title, URL).
    apps: Vec<(String, String)>,
    /// Bookmarks mode: rows become web-app entries in the chosen browser.
    bookmarks: bool,
    /// Detected browsers with "{url}" placeholder args, and the current pick.
    browsers: Vec<BrowserChoice>,
    browser_idx: usize,
}

/// Accent color presets offered by the appearance panel.
const PALETTE: [&str; 10] = [
    "#7aa2f7", "#89dceb", "#94e2d5", "#a6e3a1", "#f9e2af",
    "#fab387", "#f38ba8", "#f5c2e7", "#cba6f7", "#cdd6f4",
];

#[derive(Clone)]
struct BrowserChoice {
    label: String,
    /// Browser exe; empty = hand the URL to the system default browser.
    exe: String,
    /// Ready-to-save argument string (e.g. "--app=https://…").
    args: String,
}

#[derive(Default)]
struct LauncherState {
    hwnd: isize,
    query: String,
    selected: usize,
    /// First visible row (wheel / arrow scrolling).
    offset: usize,
    /// Config index of the entry whose shortcut is being captured.
    capturing: Option<usize>,
    /// Status/error line (conflicts, confirmations).
    message: String,
}

/// The manage-apps (✎) panel: add/scan/import flows plus per-entry editing
/// (group, browser for web apps, removal).
#[derive(Default)]
struct MgrState {
    hwnd: isize,
    offset: usize,
    /// Path picked via Browse…, waiting for the user to type a display name.
    adding: Option<String>,
    /// Args saved along with `adding` (web apps: --app=URL).
    adding_args: String,
    name_buf: String,
    /// Web-app wizard step 1: the URL being typed.
    web_url: Option<String>,
    /// Web-app wizard step 2: pick a browser for this URL.
    web_pick: Option<(String, Vec<BrowserChoice>)>,
    /// Config index whose group is being typed.
    group_edit: Option<usize>,
    group_buf: String,
    /// The modal file dialog steals focus; don't self-close while it's up.
    dialog_open: bool,
    message: String,
}

/// One paintable launcher row: a group header or an entry.
/// Entry carries (ordinal in navigation order, config index, entry,
/// indented-under-a-group-header).
enum LRow {
    Header(String),
    Entry(usize, usize, LauncherEntry, bool),
}

thread_local! {
    static BARS: RefCell<Vec<BarWindow>> = const { RefCell::new(Vec::new()) };
    static STYLE: RefCell<Option<BarStyle>> = const { RefCell::new(None) };
    static KEYBINDS: RefCell<Vec<HelpEntry>> = const { RefCell::new(Vec::new()) };
    static HELP: RefCell<HelpState> = RefCell::new(HelpState::default());
    static SETTINGS: RefCell<SettingsState> = RefCell::new(SettingsState::default());
    static LAUNCHER: RefCell<LauncherState> = RefCell::new(LauncherState::default());
    static TWEAKS: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    static SWITCHER: RefCell<SwitcherState> = RefCell::new(SwitcherState::default());
    static SCAN: RefCell<ScanState> = RefCell::new(ScanState::default());
    static MGR: RefCell<MgrState> = RefCell::new(MgrState::default());
    /// The modal color dialog steals focus; don't self-close while it's up.
    static TWEAKS_DIALOG: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The color dialog's 16 "custom colors" slots, kept for the session.
    static CUSTOM_COLORS: std::cell::Cell<[COLORREF; 16]> =
        const { std::cell::Cell::new([COLORREF(0x00FFFFFF); 16]) };
}

thread_local! {
    /// hwnd -> (HICON as isize, owned). Cleared wholesale when it grows
    /// stale; borrowed class icons are never destroyed, extracted ones are.
    static ICON_CACHE: RefCell<HashMap<isize, (isize, bool)>> = RefCell::new(HashMap::new());
}

/// Cached best-effort app icon for a window (shared by the bar cells and
/// the workspace overview).
pub(crate) fn icon_for(w: Window) -> Option<HICON> {
    ICON_CACHE.with(|c| {
        let mut map = c.borrow_mut();
        if map.len() > 256 {
            for (_, (h, owned)) in map.drain() {
                if owned && h != 0 {
                    let _ = unsafe { DestroyIcon(HICON(h as *mut c_void)) };
                }
            }
        }
        let entry = map
            .entry(w.0)
            .or_insert_with(|| w.app_icon().map(|(h, o)| (h.0 as isize, o)).unwrap_or((0, false)));
        (entry.0 != 0).then(|| HICON(entry.0 as *mut c_void))
    })
}

/// Live accent change from the appearance panel: bar highlight + repaint.
pub fn set_accent(color: u32) {
    STYLE.with(|s| {
        if let Some(st) = s.borrow_mut().as_mut() {
            st.accent = color;
        }
    });
    invalidate_all();
}

pub(crate) fn style() -> BarStyle {
    STYLE.with(|s| s.borrow().clone()).unwrap_or(BarStyle {
        height: 32,
        bg: 0x251818,
        fg: 0xf4d6cd,
        accent: 0x00F7A27A,
        cell_empty: 0x443131,
        cell_occupied: 0x5a4745,
        alpha: 255,
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
    /// App icon size / spacing inside workspace cells.
    icon: i32,
    icon_pad: i32,
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
        icon: s(16),
        icon_pad: s(3),
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
            alpha: cfg.bar_alpha,
        });
    });
    unsafe {
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        for (name, proc) in [
            (w!("wtm_bar"), bar_proc as _),
            (w!("wtm_help"), help_proc as _),
            (w!("wtm_settings"), settings_proc as _),
            (w!("wtm_launcher"), launcher_proc as _),
            (w!("wtm_tweaks"), tweaks_proc as _),
            (w!("wtm_switcher"), switcher_proc as _),
            (w!("wtm_appscan"), scan_proc as _),
            (w!("wtm_appmgr"), mgr_proc as _),
            (w!("wtm_agenda"), agenda_proc as _),
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
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_LAYERED,
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
        // Subtle glass: whole-window alpha from bar_alpha (255 = opaque).
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), style().alpha, LWA_ALPHA);
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
    close_tweaks();
    close_switcher();
    close_scan();
    close_mgr();
    close_agenda();
}

/// CF_UNICODETEXT clipboard contents, for Ctrl+V in panel text fields.
fn clipboard_text() -> Option<String> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    unsafe {
        OpenClipboard(None).ok()?;
        let text = GetClipboardData(13) // CF_UNICODETEXT
            .ok()
            .and_then(|h| {
                let hg = HGLOBAL(h.0);
                let ptr = GlobalLock(hg) as *const u16;
                if ptr.is_null() {
                    return None;
                }
                let mut len = 0usize;
                while *ptr.add(len) != 0 {
                    len += 1;
                }
                let s = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
                let _ = GlobalUnlock(hg);
                Some(s)
            });
        let _ = CloseClipboard();
        text
    }
}

/// Append the clipboard's first line to a single-line input buffer.
fn push_clipboard(buf: &mut String) {
    if let Some(t) = clipboard_text() {
        let clean: String =
            t.lines().next().unwrap_or("").trim().chars().filter(|c| !c.is_control()).collect();
        buf.push_str(&clean);
    }
}

/// Windows 11 rounded corners on a popup panel.
pub(crate) unsafe fn round_corners(hwnd: HWND) {
    let pref = DWMWCP_ROUND;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_WINDOW_CORNER_PREFERENCE,
        &pref as *const _ as *const c_void,
        std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
    );
}

/// A small outlined "button" (used for the shortcut chips).
unsafe fn draw_chip(mem: HDC, rect: RECT, bg: u32, border: u32) {
    fill_round(mem, &rect, border, 6);
    let inner = RECT {
        left: rect.left + 1,
        top: rect.top + 1,
        right: rect.right - 1,
        bottom: rect.bottom - 1,
    };
    fill_round(mem, &inner, bg, 5);
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
        let nicons = snap.cell_windows.get(i).map(|v| v.len()).unwrap_or(0) as i32;
        cw += nicons * (l.icon + l.icon_pad);
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
        toggle_tweaks_panel();
        return;
    }
    if x >= rc.right - 4 * l.help_w - 3 * l.gap {
        toggle_mgr_panel();
        return;
    }
    if x >= rc.right - 5 * l.help_w - 4 * l.gap {
        toggle_launcher_panel();
        return;
    }
    let chip = CAL_CHIP.with(|c| c.borrow().get(&(hwnd.0 as isize)).copied());
    if let Some((cl, cr)) = chip {
        if x >= cl && x < cr {
            toggle_agenda_panel();
            return;
        }
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

pub(crate) struct Buffered {
    hdc: HDC,
    pub(crate) mem: HDC,
    bmp: windows::Win32::Graphics::Gdi::HBITMAP,
    old_bmp: windows::Win32::Graphics::Gdi::HGDIOBJ,
    ps: PAINTSTRUCT,
    hwnd: HWND,
    pub(crate) w: i32,
    pub(crate) h: i32,
}

pub(crate) unsafe fn begin_buffered(hwnd: HWND) -> Buffered {
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

pub(crate) unsafe fn end_buffered(b: Buffered) {
    let _ = BitBlt(b.hdc, 0, 0, b.w, b.h, Some(b.mem), 0, 0, SRCCOPY);
    SelectObject(b.mem, b.old_bmp);
    let _ = DeleteObject(b.bmp.into());
    let _ = DeleteDC(b.mem);
    let _ = EndPaint(b.hwnd, &b.ps);
}

pub(crate) unsafe fn fill(mem: HDC, rect: &RECT, color: u32) {
    let brush = CreateSolidBrush(COLORREF(color));
    FillRect(mem, rect, brush);
    let _ = DeleteObject(brush.into());
}

/// Rounded-rectangle fill, for the Windows 11 look on cells and chips.
pub(crate) unsafe fn fill_round(mem: HDC, rect: &RECT, color: u32, radius: i32) {
    let brush = CreateSolidBrush(COLORREF(color));
    let rgn = CreateRoundRectRgn(rect.left, rect.top, rect.right, rect.bottom, radius * 2, radius * 2);
    let _ = FillRgn(mem, rgn, brush);
    let _ = DeleteObject(rgn.into());
    let _ = DeleteObject(brush.into());
}

pub(crate) unsafe fn make_font(height: i32, weight: i32) -> HFONT {
    CreateFontW(
        -height, 0, 0, 0, weight, 0, 0, 0,
        DEFAULT_CHARSET, FONT_OUTPUT_PRECISION(0), FONT_CLIP_PRECISION(0),
        CLEARTYPE_QUALITY, 0, w!("Segoe UI"),
    )
}

// Bar button glyphs from the system icon font (shared codepoints between
// Segoe Fluent Icons and Segoe MDL2 Assets).
const ICON_LAUNCHER: &str = "\u{E71D}"; // AllApps grid
const ICON_MANAGE: &str = "\u{E70F}"; // Edit pencil (manage apps)
const ICON_PALETTE: &str = "\u{E790}"; // Color palette
const ICON_SETTINGS: &str = "\u{E713}"; // gear
const ICON_HELP: &str = "\u{E897}"; // question mark

/// Icon font for the bar buttons: Segoe Fluent Icons (Windows 11), falling
/// back to Segoe MDL2 Assets (Windows 10). None if neither is installed —
/// GDI would silently substitute a text font and render tofu boxes, so the
/// face is verified by selecting the font and reading it back.
unsafe fn make_icon_font(mem: HDC, height: i32) -> Option<HFONT> {
    for face in [w!("Segoe Fluent Icons"), w!("Segoe MDL2 Assets")] {
        let font = CreateFontW(
            -height, 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0,
            DEFAULT_CHARSET, FONT_OUTPUT_PRECISION(0), FONT_CLIP_PRECISION(0),
            CLEARTYPE_QUALITY, 0, face,
        );
        let old = SelectObject(mem, font.into());
        let mut buf = [0u16; 64];
        let n = GetTextFaceW(mem, Some(&mut buf)).max(0) as usize;
        let actual = String::from_utf16_lossy(&buf[..n.min(buf.len())]);
        let actual = actual.trim_end_matches('\0');
        SelectObject(mem, old);
        if actual.eq_ignore_ascii_case(&face.to_string().unwrap_or_default()) {
            return Some(font);
        }
        let _ = DeleteObject(font.into());
    }
    None
}

pub(crate) unsafe fn draw_text(mem: HDC, text: &str, rect: RECT, color: u32, format: windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT) {
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
            fill_round(mem, &cell, fill_c, (l.cell_h / 5).max(4));
            // Number, then the workspace's app icons, then the name.
            let nrc = RECT { left: *cx, top, right: cx + l.cell_w, bottom: top + l.cell_h };
            draw_text(mem, &format!("{}", i + 1), nrc, text_c, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            let mut ix = cx + l.cell_w;
            for w in s.cell_windows.get(i).map(Vec::as_slice).unwrap_or_default() {
                if let Some(icon) = icon_for(*w) {
                    let _ = DrawIconEx(
                        mem,
                        ix,
                        top + (l.cell_h - l.icon) / 2,
                        icon,
                        l.icon,
                        l.icon,
                        0,
                        None,
                        DI_NORMAL,
                    );
                }
                ix += l.icon + l.icon_pad;
            }
            let name = s.names.get(i).map(String::as_str).unwrap_or("");
            if !name.is_empty() {
                let trc = RECT { left: ix + l.gap, top, right: cx + cw - l.gap, bottom: top + l.cell_h };
                draw_text(mem, name, trc, text_c, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
            }
            cells_end = cx + cw;
        }
    }
    let cells_end = cells_end + l.pad;

    // Right side: [clock][launcher][manage][appearance][settings][help]
    let buttons_left = w - 5 * l.help_w - 4 * l.gap;
    let t = GetLocalTime();
    let crc = RECT { left: buttons_left - l.clock_w, top: 0, right: buttons_left - l.pad / 2, bottom: h };
    draw_text(mem, &format!("{:02}:{:02}", t.wHour, t.wMinute), crc, st.fg, DT_RIGHT | DT_VCENTER | DT_SINGLELINE);

    // Next-meeting chip (calendar feature; absent when disabled or idle).
    let mut title_right = buttons_left - l.clock_w - l.pad;
    CAL_CHIP.with(|c| c.borrow_mut().remove(&(hwnd.0 as isize)));
    if let Some(m) = crate::calendar::next_meeting() {
        let now = crate::calendar::now_epoch();
        let label = if m.start <= now {
            format!("● {}", m.subject)
        } else {
            let mins = (m.start - now).div_ceil(60);
            if mins <= 90 {
                format!("{} {} · {}m", m.start_hm, m.subject, mins)
            } else {
                format!("{} {}", m.start_hm, m.subject)
            }
        };
        let scale_px = |v: i32| (v as f32 * scale) as i32;
        let tw = text_width(mem, &label).min(scale_px(280));
        let right = buttons_left - l.clock_w - l.pad;
        let left = right - tw - 2 * l.gap;
        let cell = RECT { left, top, right, bottom: top + l.cell_h };
        fill_round(mem, &cell, st.cell_occupied, (l.cell_h / 5).max(4));
        // Accent once it's imminent (10 min) or running.
        let soon = m.start <= now + 600;
        let trc = RECT { left: left + l.gap, top, right: right - l.gap, bottom: top + l.cell_h };
        draw_text(
            mem,
            &label,
            trc,
            if soon { st.accent } else { st.fg },
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
        CAL_CHIP.with(|c| c.borrow_mut().insert(hwnd.0 as isize, (left, right)));
        title_right = left - l.pad;
    }

    // Focused window title (or paused notice)
    let title = match &snap {
        Some(s) if s.paused => "⏸  paused — resume with the toggle_pause key".to_string(),
        Some(s) => s.title.clone(),
        None => String::new(),
    };
    if !title.is_empty() {
        let trc = RECT { left: cells_end, top: 0, right: title_right, bottom: h };
        draw_text(mem, &title, trc, st.fg, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
    }

    // Launcher, appearance, settings and help buttons: proper icon-font
    // glyphs when the system icon font exists, text fallback otherwise.
    let icon_font = make_icon_font(mem, l.cell_h * 68 / 100);
    let icons = if icon_font.is_some() {
        [ICON_LAUNCHER, ICON_MANAGE, ICON_PALETTE, ICON_SETTINGS, ICON_HELP]
    } else {
        ["»", "✎", "◐", "≡", "?"]
    };
    if let Some(f) = icon_font {
        SelectObject(mem, f.into());
    }
    // Buttons packed from the right; index 0 is the leftmost (launcher).
    let radius = (l.cell_h / 5).max(4);
    for (k, glyph) in icons.iter().enumerate() {
        let i = (icons.len() - 1 - k) as i32; // 0 = rightmost (help)
        let left = w - (i + 1) * l.help_w - i * l.gap;
        let right = if i == 0 { w - l.pad / 2 } else { left + l.help_w };
        let cell = RECT { left, top, right, bottom: top + l.cell_h };
        fill_round(mem, &cell, st.cell_occupied, radius);
        draw_text(mem, glyph, cell, st.accent, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    }

    SelectObject(mem, old_font);
    if let Some(f) = icon_font {
        let _ = DeleteObject(f.into());
    }
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

/// Effective DPI scale of a monitor, for sizing panels before they exist.
fn monitor_scale(handle: isize) -> f32 {
    let (mut dx, mut dy) = (96u32, 96u32);
    let hmon = HMONITOR(handle as *mut c_void);
    match unsafe { GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) } {
        Ok(()) => dx as f32 / 96.0,
        Err(_) => 1.0,
    }
}

/// Open or close the keybindings panel (bar "?" button or show_help key).
pub fn toggle_help_panel() {
    if help_hwnd().is_some() {
        close_help();
        return;
    }
    let Some(m) = monitor::active() else { return };
    unsafe {
        let entries = KEYBINDS.with(|k| k.borrow().len()) as i32;
        let scale = monitor_scale(m.handle);
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
            round_corners(hwnd);
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

// ---------------------------------------------------------- agenda panel

thread_local! {
    /// Per-bar (hwnd -> left..right) rect of the next-meeting chip, written
    /// by the painter so clicks can hit-test the same pixels.
    static CAL_CHIP: RefCell<HashMap<isize, (i32, i32)>> = RefCell::new(HashMap::new());
    static AGENDA: RefCell<isize> = const { RefCell::new(0) };
}

fn agenda_hwnd() -> Option<HWND> {
    AGENDA.with(|a| {
        let h = *a.borrow();
        (h != 0).then(|| HWND(h as *mut c_void))
    })
}

fn close_agenda() {
    if let Some(hwnd) = agenda_hwnd() {
        AGENDA.with(|a| *a.borrow_mut() = 0);
        let _ = unsafe { DestroyWindow(hwnd) };
    }
}

/// Upcoming meetings, opened from the bar's calendar chip.
pub fn toggle_agenda_panel() {
    if agenda_hwnd().is_some() {
        close_agenda();
        return;
    }
    let rows = (crate::calendar::agenda().len() as i32).clamp(1, 14);
    let Some(m) = monitor::active() else { return };
    unsafe {
        let scale = monitor_scale(m.handle);
        let s = |v: i32| (v as f32 * scale) as i32;
        let line_h = s(34);
        let w = s(480);
        let h = s(20) * 2 + line_h * (rows + 1); // rows + footer
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        if let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            w!("wtm_agenda"),
            w!("wtm agenda"),
            WS_POPUP | WS_VISIBLE,
            m.bounds.x + (m.bounds.w - w) / 2,
            m.bounds.y + m.bounds.h / 5,
            w,
            h,
            None,
            None,
            Some(hinstance.into()),
            None,
        ) {
            AGENDA.with(|a| *a.borrow_mut() = hwnd.0 as isize);
            round_corners(hwnd);
            Window::from_hwnd(hwnd).focus();
        }
    }
}

unsafe extern "system" fn agenda_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint_agenda(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let y = (lparam.0 >> 16) as i16 as i32;
            on_agenda_click(hwnd, y);
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 == 0x1B => {
            close_agenda();
            LRESULT(0)
        }
        WM_KILLFOCUS => {
            close_agenda();
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn on_agenda_click(hwnd: HWND, y: i32) {
    let scale = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0;
    let s = |v: i32| (v as f32 * scale) as i32;
    if y < s(20) {
        return;
    }
    let idx = ((y - s(20)) / s(34)) as usize;
    let list = crate::calendar::agenda();
    let Some(m) = list.get(idx) else { return };
    if m.join_url.is_empty() {
        return;
    }
    crate::logln!("wtm: joining \"{}\"", m.subject);
    run_command(&m.join_url, "", "");
    close_agenda();
}

unsafe fn paint_agenda(hwnd: HWND) {
    let b = begin_buffered(hwnd);
    let (mem, w, h) = (b.mem, b.w, b.h);
    let st = style();
    let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
    let s = |v: i32| (v as f32 * scale) as i32;
    let (pad, line_h) = (s(20), s(34));

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
    let normal = make_font(s(14), FW_NORMAL.0 as i32);
    let old_font = SelectObject(mem, bold.into());

    let list = crate::calendar::agenda();
    let now = crate::calendar::now_epoch();
    let mut y = pad;
    if list.is_empty() {
        let rc = RECT { left: pad, top: y, right: w - pad, bottom: y + line_h };
        draw_text(mem, "no upcoming meetings", rc, st.cell_occupied, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    }
    for m in &list {
        if y + line_h > h - line_h {
            break;
        }
        let ongoing = m.start <= now;
        if ongoing {
            let row = RECT { left: s(6), top: y, right: w - s(6), bottom: y + line_h };
            fill_round(mem, &row, st.cell_occupied, s(6));
        }
        SelectObject(mem, bold.into());
        let trc = RECT { left: pad, top: y, right: pad + s(100), bottom: y + line_h };
        draw_text(
            mem,
            &format!("{}–{}", m.start_hm, m.end_hm),
            trc,
            if ongoing { st.accent } else { st.fg },
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
        let src = RECT { left: pad + s(104), top: y, right: w - s(96), bottom: y + line_h };
        draw_text(mem, &m.subject, src, st.fg, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
        SelectObject(mem, normal.into());
        // Right column: a join affordance when the meeting has a Teams
        // link, otherwise the location.
        let (label, color) = if !m.join_url.is_empty() {
            ("join ⇗".to_string(), st.accent)
        } else {
            (m.location.clone(), st.cell_occupied)
        };
        if !label.is_empty() {
            let lrc = RECT { left: w - s(92), top: y, right: w - pad, bottom: y + line_h };
            draw_text(mem, &label, lrc, color, DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
        }
        y += line_h;
    }
    SelectObject(mem, normal.into());
    let frc = RECT { left: pad, top: h - line_h, right: w - pad, bottom: h - s(6) };
    draw_text(
        mem,
        "click a meeting to join · Esc closes",
        frc,
        st.cell_occupied,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );

    SelectObject(mem, old_font);
    let _ = DeleteObject(bold.into());
    let _ = DeleteObject(normal.into());
    end_buffered(b);
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

// ------------------------------------------------------- launch frecency

thread_local! {
    /// usage key -> (launch count, last-launch epoch secs). Lazily loaded
    /// from %LOCALAPPDATA%\wtm\usage.tsv, written back on every launch.
    static USAGE: RefCell<Option<HashMap<String, (u32, u64)>>> = const { RefCell::new(None) };
}

fn usage_path() -> Option<std::path::PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(|d| std::path::PathBuf::from(d).join("wtm").join("usage.tsv"))
}

/// Entries are keyed by what they run, so renames/reorders keep history.
fn usage_key(e: &LauncherEntry) -> String {
    format!("{} {}", e.command, e.args).trim().to_lowercase()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn with_usage<R>(f: impl FnOnce(&mut HashMap<String, (u32, u64)>) -> R) -> R {
    USAGE.with(|u| {
        let mut slot = u.borrow_mut();
        let map = slot.get_or_insert_with(|| {
            let mut m = HashMap::new();
            if let Some(text) = usage_path().and_then(|p| std::fs::read_to_string(p).ok()) {
                for line in text.lines() {
                    let mut it = line.splitn(3, '\t');
                    if let (Some(c), Some(t), Some(k)) = (it.next(), it.next(), it.next()) {
                        if let (Ok(c), Ok(t)) = (c.parse(), t.parse()) {
                            m.insert(k.to_string(), (c, t));
                        }
                    }
                }
            }
            m
        });
        f(map)
    })
}

fn record_launch(e: &LauncherEntry) {
    with_usage(|m| {
        let ent = m.entry(usage_key(e)).or_insert((0, 0));
        ent.0 = ent.0.saturating_add(1);
        ent.1 = now_secs();
        if let Some(p) = usage_path() {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let mut out = String::new();
            for (k, (c, t)) in m.iter() {
                out.push_str(&format!("{c}\t{t}\t{k}\n"));
            }
            let _ = std::fs::write(p, out);
        }
    });
}

/// Frecency: launch count weighted by how recently it was last used.
fn usage_score(e: &LauncherEntry) -> u64 {
    with_usage(|m| {
        let Some(&(count, last)) = m.get(&usage_key(e)) else { return 0 };
        let age = now_secs().saturating_sub(last);
        let recency: u64 = if age < 3600 {
            8
        } else if age < 86_400 {
            4
        } else if age < 7 * 86_400 {
            2
        } else {
            1
        };
        u64::from(count.min(100)) * recency
    })
}

/// Filtered entries paired with their index in config.launcher (needed so
/// key assignment and removal survive filtering). Ranked like a good
/// launcher should: name-prefix beats substring beats fuzzy; within a rank
/// the most-used-recently entries first, then alphabetical. Browsing with
/// no query gives your daily drivers on top, everything else A→Z below.
fn launcher_filtered() -> Vec<(usize, LauncherEntry)> {
    let query = LAUNCHER.with(|s| s.borrow().query.trim().to_lowercase());
    let mut ranked: Vec<(u32, u64, String, usize, LauncherEntry)> = launcher_entries()
        .into_iter()
        .enumerate()
        .filter_map(|(i, e)| {
            let name = e.name.to_lowercase();
            let group = e.group.to_lowercase();
            let rank = if query.is_empty() {
                0
            } else if name.starts_with(&query) {
                0
            } else if !group.is_empty() && group.starts_with(&query) {
                1 // "web" pulls up the whole web group
            } else if name.contains(&query) {
                2
            } else if !group.is_empty() && group.contains(&query) {
                2
            } else if fuzzy_match(&format!("{} {} {}", e.name, e.group, e.command), &query) {
                3
            } else {
                return None;
            };
            Some((rank, usage_score(&e), name, i, e))
        })
        .collect();
    ranked.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
    ranked.into_iter().map(|(_, _, _, i, e)| (i, e)).collect()
}

/// How many rows fit in the launcher panel, keeping the footer visible.
/// Shared by paint, clicks, wheel and arrow scrolling.
fn launcher_visible_rows(client_h: i32, scale: f32) -> usize {
    let s = |v: i32| (v as f32 * scale) as i32;
    let rows_top = s(20) + s(28) + s(6);
    (((client_h - s(8) - s(28)) - rows_top) / s(28)).max(1) as usize
}

/// The launcher's display rows. With a query: a flat ranked list. While
/// browsing: ungrouped entries first (frecency order — daily drivers on
/// top), then each group alphabetically under an accent header.
fn launcher_rows() -> Vec<LRow> {
    let query = LAUNCHER.with(|s| s.borrow().query.trim().to_string());
    let filtered = launcher_filtered();
    if !query.is_empty() {
        return filtered
            .into_iter()
            .enumerate()
            .map(|(o, (i, e))| LRow::Entry(o, i, e, false))
            .collect();
    }
    let mut rows = Vec::new();
    let mut ord = 0usize;
    let mut groups: Vec<String> = filtered
        .iter()
        .filter(|(_, e)| !e.group.is_empty())
        .map(|(_, e)| e.group.to_lowercase())
        .collect();
    groups.sort();
    groups.dedup();
    let has_ungrouped = filtered.iter().any(|(_, e)| e.group.is_empty());
    // Start-Menu-style "frequent": the top launches by frecency, whatever
    // section they otherwise live in. `filtered` is already score-sorted.
    if !groups.is_empty() {
        let top: Vec<&(usize, LauncherEntry)> =
            filtered.iter().filter(|(_, e)| usage_score(e) > 0).take(5).collect();
        if top.len() >= 2 {
            rows.push(LRow::Header("frequent".to_string()));
            for (i, e) in top {
                rows.push(LRow::Entry(ord, *i, e.clone(), false));
                ord += 1;
            }
        }
    }
    // Once any group exists, the loose entries get a section of their own
    // so the whole list reads as organized sections.
    if has_ungrouped && !groups.is_empty() {
        rows.push(LRow::Header("ungrouped".to_string()));
    }
    for (i, e) in filtered.iter().filter(|(_, e)| e.group.is_empty()) {
        rows.push(LRow::Entry(ord, *i, e.clone(), false));
        ord += 1;
    }
    for g in groups {
        rows.push(LRow::Header(g.clone()));
        for (i, e) in filtered.iter().filter(|(_, e)| e.group.eq_ignore_ascii_case(&g)) {
            rows.push(LRow::Entry(ord, *i, e.clone(), true));
            ord += 1;
        }
    }
    rows
}

/// Entries in on-screen order — what ↑↓ selection and Enter navigate.
fn launcher_nav(rows: &[LRow]) -> Vec<(usize, LauncherEntry)> {
    rows.iter()
        .filter_map(|r| match r {
            LRow::Entry(_, i, e, _) => Some((*i, e.clone())),
            LRow::Header(_) => None,
        })
        .collect()
}

/// Row index showing the entry with navigation ordinal `ord`.
fn launcher_row_of(rows: &[LRow], ord: usize) -> usize {
    rows.iter()
        .position(|r| matches!(r, LRow::Entry(o, _, _, _) if *o == ord))
        .unwrap_or(0)
}

thread_local! {
    /// path -> small shell icon (owned), for launcher rows.
    static PATH_ICONS: RefCell<HashMap<String, isize>> = RefCell::new(HashMap::new());
}

/// Small (16px) shell icon for an exe/lnk/document path, cached per path.
fn icon_for_path(path: &str) -> Option<HICON> {
    use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
    use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_SMALLICON};
    PATH_ICONS.with(|c| {
        let mut map = c.borrow_mut();
        let raw = *map.entry(path.to_string()).or_insert_with(|| unsafe {
            let expanded = expand_env(path);
            let wide: Vec<u16> = expanded.encode_utf16().chain(std::iter::once(0)).collect();
            let mut info = SHFILEINFOW::default();
            let ok = SHGetFileInfoW(
                PCWSTR(wide.as_ptr()),
                FILE_FLAGS_AND_ATTRIBUTES(0),
                Some(&mut info),
                std::mem::size_of::<SHFILEINFOW>() as u32,
                SHGFI_ICON | SHGFI_SMALLICON,
            );
            if ok != 0 && !info.hIcon.is_invalid() { info.hIcon.0 as isize } else { 0 }
        });
        (raw != 0).then(|| HICON(raw as *mut c_void))
    })
}

/// Launch config.launcher[i] — used by per-app global shortcuts.
pub fn launch_index(i: usize) {
    let entry = crate::with_wm(|wm| wm.config.launcher.get(i).cloned()).flatten();
    if let Some(e) = entry {
        crate::logln!("wtm: launching {}", e.name);
        record_launch(&e);
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
    record_launch(entry);
    run_command(&entry.command, &entry.args, &entry.dir);
    close_launcher();
}

/// Standard Windows file-open dialog (modal). Returns the chosen path.
/// `filter` uses the OPENFILENAME format with embedded NULs.
fn pick_file(owner: HWND, filter: &str, title: &str) -> Option<String> {
    let filter: Vec<u16> = filter.encode_utf16().collect();
    let title: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let mut buf = [0u16; 2048];
    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner,
        lpstrFilter: PCWSTR(filter.as_ptr()),
        lpstrFile: PWSTR(buf.as_mut_ptr()),
        nMaxFile: buf.len() as u32,
        lpstrTitle: PCWSTR(title.as_ptr()),
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
    let Some(hwnd) = mgr_hwnd() else { return };
    MGR.with(|s| s.borrow_mut().dialog_open = true);
    let picked = pick_file(
        hwnd,
        "Programs & scripts (*.exe;*.bat;*.cmd;*.lnk)\0*.exe;*.bat;*.cmd;*.lnk\0All files (*.*)\0*.*\0\0",
        "Choose an application, script or file",
    );
    MGR.with(|s| s.borrow_mut().dialog_open = false);
    if let Some(h) = mgr_hwnd() {
        Window::from_hwnd(h).focus();
    }
    if let Some(path) = picked {
        MGR.with(|s| {
            let mut st = s.borrow_mut();
            st.name_buf = file_stem(&path);
            st.adding = Some(path);
        });
    }
    invalidate_mgr();
}

fn commit_new_app() {
    let (path, args, name) = MGR.with(|s| {
        let st = s.borrow();
        (st.adding.clone(), st.adding_args.clone(), st.name_buf.trim().to_string())
    });
    let Some(path) = path else { return };
    let name = if name.is_empty() { file_stem(&path) } else { name };
    crate::with_wm(|wm| {
        wm.config.launcher.push(LauncherEntry {
            name,
            command: path,
            args,
            dir: String::new(),
            key: String::new(),
            group: String::new(),
        });
        if let Err(e) = crate::config::save_launcher(&wm.config.launcher) {
            crate::logln!("wtm: could not save config: {e}");
        }
    });
    MGR.with(|s| {
        let mut st = s.borrow_mut();
        st.adding = None;
        st.adding_args.clear();
        st.name_buf.clear();
        st.message = "added — assign a shortcut from the launcher's key chip".into();
    });
    crate::reregister_hotkeys();
    invalidate_mgr();
    invalidate_launcher();
}

// Web apps: a URL + a browser in app mode becomes a regular launcher entry
// (command = browser exe, args = --app=URL), so shortcuts just work.

fn normalize_url(raw: &str) -> String {
    let t = raw.trim();
    if t.contains("://") {
        t.to_string()
    } else {
        format!("https://{t}")
    }
}

/// "https://grafana.mycorp.com/d/abc" -> "grafana.mycorp.com".
fn url_display_name(url: &str) -> String {
    let no_scheme = url.split("://").nth(1).unwrap_or(url);
    let host = no_scheme.split(['/', '?']).next().unwrap_or(no_scheme);
    host.trim_start_matches("www.").to_string()
}

/// Browsers found on this machine, each with the args that open `url` as an
/// app-style window. The system default browser is always the last option.
fn web_browser_choices(url: &str) -> Vec<BrowserChoice> {
    let pf = std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".into());
    let pf86 =
        std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| "C:\\Program Files (x86)".into());
    let lad = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let mut out = Vec::new();
    let chromium: [(&str, Vec<String>); 3] = [
        ("Brave — app window", vec![
            format!("{pf}\\BraveSoftware\\Brave-Browser\\Application\\brave.exe"),
            format!("{lad}\\BraveSoftware\\Brave-Browser\\Application\\brave.exe"),
        ]),
        ("Edge — app window", vec![
            format!("{pf86}\\Microsoft\\Edge\\Application\\msedge.exe"),
            format!("{pf}\\Microsoft\\Edge\\Application\\msedge.exe"),
        ]),
        ("Chrome — app window", vec![
            format!("{pf}\\Google\\Chrome\\Application\\chrome.exe"),
            format!("{pf86}\\Google\\Chrome\\Application\\chrome.exe"),
            format!("{lad}\\Google\\Chrome\\Application\\chrome.exe"),
        ]),
    ];
    for (label, paths) in chromium {
        if let Some(p) = paths.into_iter().find(|p| std::path::Path::new(p).exists()) {
            out.push(BrowserChoice {
                label: label.to_string(),
                exe: p,
                args: format!("--app={url}"),
            });
        }
    }
    let firefox = [
        format!("{pf}\\Mozilla Firefox\\firefox.exe"),
        format!("{pf86}\\Mozilla Firefox\\firefox.exe"),
    ];
    if let Some(p) = firefox.into_iter().find(|p| std::path::Path::new(p).exists()) {
        out.push(BrowserChoice {
            label: "Firefox — new window".to_string(),
            exe: p,
            args: format!("-new-window {url}"),
        });
    }
    out.push(BrowserChoice {
        label: "Default browser — normal tab".to_string(),
        exe: String::new(),
        args: String::new(),
    });
    out
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

/// Parse a browser bookmarks export (the Netscape bookmark HTML format
/// Firefox/Edge/Chrome all write): every `<A HREF="…">title</A>` becomes
/// (title, url). ASCII-lowercase scanning keeps byte offsets valid.
fn parse_bookmarks_html(text: &str) -> Vec<(String, String)> {
    let low = text.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(a) = low[pos..].find("<a ") {
        let start = pos + a;
        let Some(tag_len) = low[start..].find('>') else { break };
        let tag_end = start + tag_len;
        let href = low[start..tag_end].find("href=\"").and_then(|h| {
            let vs = start + h + 6;
            text[vs..tag_end].find('"').map(|e| decode_entities(&text[vs..vs + e]))
        });
        let text_start = tag_end + 1;
        let title = low[text_start..]
            .find("</a>")
            .map(|e| decode_entities(text[text_start..text_start + e].trim()));
        if let (Some(url), Some(title)) = (href, title) {
            if url.starts_with("http") && !title.is_empty() {
                out.push((title, url));
            }
        }
        pos = tag_end + 1;
    }
    out
}

/// Browser chosen: hand off to the normal naming step, pre-filled.
fn pick_web_browser(url: &str, c: &BrowserChoice) {
    MGR.with(|s| {
        let mut st = s.borrow_mut();
        st.web_pick = None;
        if c.exe.is_empty() {
            st.adding = Some(url.to_string());
            st.adding_args = String::new();
        } else {
            st.adding = Some(c.exe.clone());
            st.adding_args = c.args.clone();
        }
        st.name_buf = url_display_name(url);
    });
    invalidate_mgr();
}

/// The URL a web-app entry opens, however it is wired to its browser.
fn entry_url(e: &LauncherEntry) -> Option<String> {
    if e.command.starts_with("http://") || e.command.starts_with("https://") {
        return Some(e.command.clone());
    }
    if let Some(u) = e.args.split("--app=").nth(1) {
        return Some(u.trim().to_string());
    }
    if let Some(u) = e.args.split("-new-window ").nth(1) {
        return Some(u.trim().to_string());
    }
    None
}

/// Short browser tag for a web-app entry ("Brave", "Default", …).
fn entry_browser_label(e: &LauncherEntry) -> String {
    let exe = e.command.rsplit(['\\', '/']).next().unwrap_or("").to_lowercase();
    match exe.as_str() {
        "brave.exe" => "Brave".to_string(),
        "msedge.exe" => "Edge".to_string(),
        "chrome.exe" => "Chrome".to_string(),
        "firefox.exe" => "Firefox".to_string(),
        _ => "Default".to_string(),
    }
}

/// Rewire a web-app entry to the next detected browser (wraps around).
fn cycle_entry_browser(cfg_idx: usize) {
    crate::with_wm(|wm| {
        let Some(e) = wm.config.launcher.get_mut(cfg_idx) else { return };
        let Some(url) = entry_url(e) else { return };
        let choices = web_browser_choices("{url}");
        let cur = choices
            .iter()
            .position(|c| !c.exe.is_empty() && e.command.eq_ignore_ascii_case(&c.exe))
            .unwrap_or(choices.len() - 1); // URL-command entries = "Default"
        let next = &choices[(cur + 1) % choices.len()];
        if next.exe.is_empty() {
            e.command = url;
            e.args.clear();
        } else {
            e.command = next.exe.clone();
            e.args = next.args.replace("{url}", &url);
        }
        if let Err(err) = crate::config::save_launcher(&wm.config.launcher) {
            crate::logln!("wtm: could not save config: {err}");
        }
    });
    invalidate_mgr();
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
    invalidate_mgr();
}

pub fn toggle_launcher_panel() {
    if launcher_hwnd().is_some() {
        close_launcher();
        return;
    }
    let Some(m) = monitor::active() else { return };
    unsafe {
        let scale = monitor_scale(m.handle);
        let s = |v: i32| (v as f32 * scale) as i32;
        let line_h = s(28);
        // Grow with the list (entries + group headers), up to ~70% of the
        // monitor. Adding/organizing lives in the ✎ manage panel now.
        let max_rows = ((m.bounds.h * 7 / 10 - s(40)) / line_h - 2).max(8);
        let rows = (launcher_rows().len() as i32).clamp(1, max_rows);
        let w = s(560);
        let h = s(20) * 2 + line_h * (rows + 2); // search + rows + footer
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
            round_corners(hwnd);
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
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let visible = launcher_visible_rows(rc.bottom, GetDpiForWindow(hwnd) as f32 / 96.0);
            let n = launcher_rows().len();
            LAUNCHER.with(|s| {
                let mut st = s.borrow_mut();
                let max = n.saturating_sub(visible) as i32;
                st.offset = (st.offset as i32 - delta / 120 * 3).clamp(0, max) as usize;
            });
            invalidate_launcher();
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
            close_launcher();
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn on_launcher_click(hwnd: HWND, x: i32, y: i32) {
    let mut rc = RECT::default();
    let _ = unsafe { GetClientRect(hwnd, &mut rc) };
    let scale = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0;
    let s = |v: i32| (v as f32 * scale) as i32;
    let rows_top = s(20) + s(28) + s(6);
    if y < rows_top {
        return;
    }
    let idx = ((y - rows_top) / s(28)) as usize;
    if idx >= launcher_visible_rows(rc.bottom, scale) {
        return; // footer area
    }
    let rows = launcher_rows();
    let offset = LAUNCHER.with(|st| st.borrow().offset).min(rows.len().saturating_sub(1));
    let Some(LRow::Entry(_, cfg_idx, entry, _)) = rows.get(offset + idx) else { return };
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
        match ch {
            0x08 => {
                st.query.pop();
            }
            0x16 => push_clipboard(&mut st.query), // Ctrl+V
            c if c >= 0x20 => {
                if let Some(c) = char::from_u32(c) {
                    st.query.push(c);
                }
            }
            _ => return,
        }
        st.selected = 0;
        st.offset = 0;
    });
    invalidate_launcher();
}

fn on_launcher_key(vk: u32) {
    if let Some(idx) = LAUNCHER.with(|s| s.borrow().capturing) {
        handle_key_capture(idx, vk);
        return;
    }
    if vk == VK_ESCAPE.0 as u32 {
        close_launcher();
        return;
    }
    let rows = launcher_rows();
    let nav = launcher_nav(&rows);
    if vk == VK_DOWN.0 as u32 || vk == VK_UP.0 as u32 {
        // Wrap the selection through entries (headers are skipped), then
        // scroll its row into view — with the group header if adjacent.
        let visible = launcher_hwnd()
            .map(|hwnd| {
                let mut rc = RECT::default();
                let _ = unsafe { GetClientRect(hwnd, &mut rc) };
                launcher_visible_rows(rc.bottom, unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0)
            })
            .unwrap_or(usize::MAX);
        LAUNCHER.with(|s| {
            let mut st = s.borrow_mut();
            let n = nav.len().max(1) as i32;
            let dir = if vk == VK_DOWN.0 as u32 { 1 } else { -1 };
            st.selected = (st.selected as i32 + dir).rem_euclid(n) as usize;
            let mut row = launcher_row_of(&rows, st.selected);
            if row > 0 && matches!(rows.get(row - 1), Some(LRow::Header(_))) {
                row -= 1;
            }
            if row < st.offset {
                st.offset = row;
            } else if row >= st.offset + visible {
                st.offset = row + 1 - visible;
            }
        });
        invalidate_launcher();
        return;
    }
    if vk == VK_RETURN.0 as u32 {
        let selected = LAUNCHER.with(|s| s.borrow().selected);
        if let Some((_, entry)) = nav.get(selected) {
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
    let small = make_font(s(12), FW_BOLD.0 as i32); // group section headers
    let old_font = SelectObject(mem, bold.into());

    let (query, selected, capturing, message, offset) = LAUNCHER.with(|s| {
        let st = s.borrow();
        (st.query.clone(), st.selected, st.capturing, st.message.clone(), st.offset)
    });

    // Search line
    let src = RECT { left: pad, top: pad, right: w - pad, bottom: pad + line_h };
    if query.is_empty() {
        draw_text(mem, "»  type to search apps…", src, st.cell_occupied, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    } else {
        draw_text(mem, &format!("»  {query}_"), src, st.fg, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    }

    let rows = launcher_rows();
    let rows_top = pad + line_h + s(6);
    let visible = launcher_visible_rows(h, scale);
    let offset = offset.min(rows.len().saturating_sub(1));
    let mut y = rows_top;
    for row in rows.iter().skip(offset).take(visible) {
        if y + line_h > h - s(8) {
            break;
        }
        match row {
            LRow::Header(g) => {
                // Section header: small caps + a rule line to the edge, so
                // it reads as a divider, not another app.
                SelectObject(mem, small.into());
                let label = g.to_uppercase();
                let hrc = RECT { left: pad, top: y, right: w - pad, bottom: y + line_h };
                let dim = g == "ungrouped";
                let color = if dim { st.cell_occupied } else { st.accent };
                draw_text(mem, &label, hrc, color, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
                let tw = text_width(mem, &label);
                let ly = y + line_h * 6 / 10;
                let lrc = RECT { left: pad + tw + s(10), top: ly, right: w - pad, bottom: ly + s(1).max(1) };
                fill(mem, &lrc, st.cell_occupied);
            }
            LRow::Entry(ord, cfg_idx, e, indented) => {
                if *ord == selected {
                    let row = RECT { left: s(6), top: y, right: w - s(6), bottom: y + line_h };
                    fill_round(mem, &row, st.cell_occupied, s(6));
                }
                // Group members are indented under their header.
                let indent = if *indented { s(16) } else { 0 };
                if let Some(icon) = icon_for_path(&e.command) {
                    let isz = s(16);
                    let _ = DrawIconEx(mem, pad + indent, y + (line_h - isz) / 2, icon, isz, isz, 0, None, DI_NORMAL);
                }
                SelectObject(mem, bold.into());
                let nrc = RECT { left: pad + indent + s(24), top: y, right: w * 2 / 5, bottom: y + line_h };
                draw_text(mem, &e.name, nrc, st.fg, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
                SelectObject(mem, normal.into());
                let crc = RECT { left: w * 2 / 5 + s(8), top: y, right: w - s(164), bottom: y + line_h };
                draw_text(mem, &e.command, crc, st.cell_occupied, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
                // Key chip: an outlined button, clickable on any row.
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
            }
        }
        y += line_h;
    }
    if rows.is_empty() && !query.trim().is_empty() {
        SelectObject(mem, normal.into());
        let erc = RECT { left: pad, top: y, right: w - pad, bottom: y + line_h };
        draw_text(mem, &format!("↵  run \"{}\"", query.trim()), erc, st.fg, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
    }

    // Footer: status message > overflow hint > where-to-manage hint.
    SelectObject(mem, normal.into());
    let frc = RECT { left: pad, top: h - line_h - s(6), right: w - pad, bottom: h - s(6) };
    let footer = if !message.is_empty() {
        message
    } else if rows.len() > visible {
        format!("{} more — scroll or type to filter", rows.len() - visible)
    } else {
        "add & organize apps from the bar's ✎ button".to_string()
    };
    draw_text(mem, &footer, frc, st.cell_occupied, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);

    SelectObject(mem, old_font);
    let _ = DeleteObject(bold.into());
    let _ = DeleteObject(normal.into());
    let _ = DeleteObject(small.into());
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
    items.push(SettingsItem::CalHeader);
    items.push(SettingsItem::CalOff);
    items.push(SettingsItem::CalOutlook);
    items.push(SettingsItem::CalIcs);
    items
}

/// Apply + persist a new meetings-calendar source from the settings panel.
fn set_calendar_source(source: &str) {
    let refresh = crate::with_wm(|wm| {
        wm.config.calendar_source = source.to_string();
        wm.config.calendar_refresh_min
    })
    .unwrap_or(5);
    crate::calendar::configure(source, refresh);
    if let Err(e) = crate::config::save_calendar_source(source) {
        crate::logln!("wtm: could not save config: {e}");
    }
    invalidate_settings();
    invalidate_all(); // the bar chip appears/disappears
}

fn settings_rows_top(scale: f32) -> i32 {
    let s = |v: i32| (v as f32 * scale) as i32;
    s(20) + s(26) + s(8) // pad + title row + separator
}

// ------------------------------------------------- manage-apps (✎) panel

fn close_mgr() {
    MGR.with(|s| {
        let mut st = s.borrow_mut();
        if st.hwnd != 0 {
            let hwnd = st.hwnd;
            *st = MgrState::default();
            drop(st);
            let _ = unsafe { DestroyWindow(HWND(hwnd as *mut c_void)) };
        }
    });
}

fn mgr_hwnd() -> Option<HWND> {
    MGR.with(|s| {
        let hwnd = s.borrow().hwnd;
        (hwnd != 0).then(|| HWND(hwnd as *mut c_void))
    })
}

fn invalidate_mgr() {
    if let Some(hwnd) = mgr_hwnd() {
        let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
    }
}

const MGR_ACTIONS: [&str; 4] = [
    "+  add app (browse…)",
    "⊞  scan installed apps…",
    "🌐  add web app (URL)…",
    "★  import bookmarks (.html)…",
];

fn mgr_rows_top(scale: f32) -> i32 {
    let s = |v: i32| (v as f32 * scale) as i32;
    // title + 4 action rows + separator gap
    s(20) + s(28) + s(6) + 4 * s(28) + s(6)
}

pub fn toggle_mgr_panel() {
    if mgr_hwnd().is_some() {
        close_mgr();
        return;
    }
    let Some(m) = monitor::active() else { return };
    unsafe {
        let scale = monitor_scale(m.handle);
        let s = |v: i32| (v as f32 * scale) as i32;
        let w = s(640);
        let h = (m.bounds.h * 3 / 4).min(s(760));
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        if let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            w!("wtm_appmgr"),
            w!("wtm manage apps"),
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
            MGR.with(|st| st.borrow_mut().hwnd = hwnd.0 as isize);
            round_corners(hwnd);
            Window::from_hwnd(hwnd).focus();
        }
    }
}

unsafe extern "system" fn mgr_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint_mgr(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            on_mgr_click(hwnd, x, y);
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
            MGR.with(|s| {
                let mut st = s.borrow_mut();
                let max = launcher_entries().len().saturating_sub(3) as i32;
                st.offset = (st.offset as i32 - delta / 120 * 3).clamp(0, max) as usize;
            });
            invalidate_mgr();
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            on_mgr_key(wparam.0 as u32);
            LRESULT(0)
        }
        WM_CHAR => {
            on_mgr_char(wparam.0 as u32);
            LRESULT(0)
        }
        WM_SYSCHAR => LRESULT(0),
        WM_KILLFOCUS => {
            if !MGR.with(|s| s.borrow().dialog_open) {
                close_mgr();
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn on_mgr_click(hwnd: HWND, x: i32, y: i32) {
    let mut rc = RECT::default();
    let _ = unsafe { GetClientRect(hwnd, &mut rc) };
    let scale = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0;
    let s = |v: i32| (v as f32 * scale) as i32;
    if MGR.with(|st| st.borrow().adding.is_some() || st.borrow().web_url.is_some()) {
        return; // typing modes are keyboard-only
    }
    // Browser pick list is clickable.
    if let Some((url, choices)) = MGR.with(|st| st.borrow().web_pick.clone()) {
        let rows_top = s(20) + s(28) + s(6);
        if y >= rows_top {
            let idx = ((y - rows_top) / s(28)) as usize;
            if let Some(c) = choices.get(idx) {
                pick_web_browser(&url, c);
            }
        }
        return;
    }
    let actions_top = s(20) + s(28) + s(6);
    // The four action rows.
    if y >= actions_top && y < actions_top + 4 * s(28) {
        match ((y - actions_top) / s(28)) as usize {
            0 => add_app_flow(),
            1 => {
                close_mgr();
                toggle_scan_panel();
            }
            2 => {
                MGR.with(|st| {
                    let mut st = st.borrow_mut();
                    st.web_url = Some(String::new());
                    st.message.clear();
                });
                invalidate_mgr();
            }
            3 => import_bookmarks_flow(),
            _ => {}
        }
        return;
    }
    // Entry rows.
    let rows_top = mgr_rows_top(scale);
    if y < rows_top {
        return;
    }
    let entries = launcher_entries();
    let offset = MGR.with(|st| st.borrow().offset).min(entries.len().saturating_sub(1));
    let idx = offset + ((y - rows_top) / s(28)) as usize;
    let Some(e) = entries.get(idx) else { return };
    if x >= rc.right - s(44) {
        remove_entry(e);
    } else if x >= rc.right - s(190) && x < rc.right - s(48) && entry_url(e).is_some() {
        cycle_entry_browser(idx);
    } else if x >= rc.right - s(330) && x < rc.right - s(195) {
        MGR.with(|st| {
            let mut st = st.borrow_mut();
            st.group_edit = Some(idx);
            st.group_buf = e.group.clone();
            st.message.clear();
        });
        invalidate_mgr();
    }
}

fn on_mgr_char(ch: u32) {
    MGR.with(|s| {
        let mut st = s.borrow_mut();
        let buf = if st.web_url.is_some() {
            st.web_url.as_mut().unwrap()
        } else if st.adding.is_some() {
            &mut st.name_buf
        } else if st.group_edit.is_some() {
            &mut st.group_buf
        } else {
            return;
        };
        match ch {
            0x08 => {
                buf.pop();
            }
            0x16 => push_clipboard(buf), // Ctrl+V
            c if c >= 0x20 => {
                if let Some(c) = char::from_u32(c) {
                    buf.push(c);
                }
            }
            _ => return,
        }
    });
    invalidate_mgr();
}

fn on_mgr_key(vk: u32) {
    // Web wizard step 1: Enter continues, Esc cancels.
    if let Some(buf) = MGR.with(|s| s.borrow().web_url.clone()) {
        if vk == VK_RETURN.0 as u32 && !buf.trim().is_empty() {
            let url = normalize_url(&buf);
            let choices = web_browser_choices(&url);
            MGR.with(|s| {
                let mut st = s.borrow_mut();
                st.web_url = None;
                st.web_pick = Some((url, choices));
            });
            invalidate_mgr();
        } else if vk == VK_ESCAPE.0 as u32 {
            MGR.with(|s| s.borrow_mut().web_url = None);
            invalidate_mgr();
        }
        return;
    }
    // Step 2: digits pick a browser.
    if let Some((url, choices)) = MGR.with(|s| s.borrow().web_pick.clone()) {
        if vk == VK_ESCAPE.0 as u32 {
            MGR.with(|s| s.borrow_mut().web_pick = None);
            invalidate_mgr();
        } else if (0x31..=0x39).contains(&vk) {
            if let Some(c) = choices.get((vk - 0x31) as usize) {
                pick_web_browser(&url, c);
            }
        }
        return;
    }
    // Naming a new app: Enter saves, Esc cancels.
    if MGR.with(|s| s.borrow().adding.is_some()) {
        if vk == VK_RETURN.0 as u32 {
            commit_new_app();
        } else if vk == VK_ESCAPE.0 as u32 {
            MGR.with(|s| {
                let mut st = s.borrow_mut();
                st.adding = None;
                st.adding_args.clear();
                st.name_buf.clear();
            });
            invalidate_mgr();
        }
        return;
    }
    // Typing a group name: Enter saves (empty clears), Esc cancels.
    if let Some(idx) = MGR.with(|s| s.borrow().group_edit) {
        if vk == VK_RETURN.0 as u32 {
            let group = MGR.with(|s| s.borrow().group_buf.trim().to_lowercase());
            crate::with_wm(|wm| {
                if let Some(e) = wm.config.launcher.get_mut(idx) {
                    e.group = group;
                }
                if let Err(err) = crate::config::save_launcher(&wm.config.launcher) {
                    crate::logln!("wtm: could not save config: {err}");
                }
            });
            MGR.with(|s| {
                let mut st = s.borrow_mut();
                st.group_edit = None;
                st.group_buf.clear();
            });
            invalidate_mgr();
            invalidate_launcher();
        } else if vk == VK_ESCAPE.0 as u32 {
            MGR.with(|s| {
                let mut st = s.borrow_mut();
                st.group_edit = None;
                st.group_buf.clear();
            });
            invalidate_mgr();
        }
        return;
    }
    if vk == VK_ESCAPE.0 as u32 {
        close_mgr();
    }
}

unsafe fn paint_mgr(hwnd: HWND) {
    let b = begin_buffered(hwnd);
    let (mem, w, h) = (b.mem, b.w, b.h);
    let st = style();
    let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
    let s = |v: i32| (v as f32 * scale) as i32;
    let (pad, line_h) = (s(20), s(28));
    let left_vc = DT_LEFT | DT_VCENTER | DT_SINGLELINE;

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

    let (adding, adding_args, name_buf, web_url, web_pick, group_edit, group_buf, offset, message) =
        MGR.with(|s| {
            let st = s.borrow();
            (
                st.adding.clone(),
                st.adding_args.clone(),
                st.name_buf.clone(),
                st.web_url.clone(),
                st.web_pick.clone(),
                st.group_edit,
                st.group_buf.clone(),
                st.offset,
                st.message.clone(),
            )
        });

    // Wizard modes take over the whole panel.
    if let Some(url) = &web_url {
        let nrc = RECT { left: pad, top: pad, right: w - pad, bottom: pad + line_h };
        draw_text(mem, &format!("web app URL:  {url}_"), nrc, st.fg, left_vc);
        SelectObject(mem, normal.into());
        let prc = RECT { left: pad, top: pad + line_h, right: w - pad, bottom: pad + line_h * 2 };
        draw_text(mem, "e.g. grafana.mycorp.com — https:// is assumed", prc, st.cell_occupied, left_vc);
        let hrc = RECT { left: pad, top: h - line_h - s(6), right: w - pad, bottom: h - s(6) };
        draw_text(mem, "Enter continues · Esc cancels · Ctrl+V pastes", hrc, st.cell_occupied, left_vc);
        SelectObject(mem, old_font);
        let _ = DeleteObject(bold.into());
        let _ = DeleteObject(normal.into());
        end_buffered(b);
        return;
    }
    if let Some((url, choices)) = &web_pick {
        let nrc = RECT { left: pad, top: pad, right: w - pad, bottom: pad + line_h };
        draw_text(mem, &format!("open {url} with:"), nrc, st.fg, left_vc | DT_END_ELLIPSIS);
        let mut y = pad + line_h + s(6);
        for (i, c) in choices.iter().enumerate() {
            if y + line_h > h - line_h {
                break;
            }
            SelectObject(mem, bold.into());
            let krc = RECT { left: pad, top: y, right: pad + s(28), bottom: y + line_h };
            draw_text(mem, &format!("{}", i + 1), krc, st.accent, left_vc);
            SelectObject(mem, normal.into());
            let lrc = RECT { left: pad + s(32), top: y, right: w - pad, bottom: y + line_h };
            draw_text(mem, &c.label, lrc, st.fg, left_vc | DT_END_ELLIPSIS);
            y += line_h;
        }
        let hrc = RECT { left: pad, top: h - line_h - s(6), right: w - pad, bottom: h - s(6) };
        draw_text(mem, "click or press the number · Esc cancels", hrc, st.cell_occupied, left_vc);
        SelectObject(mem, old_font);
        let _ = DeleteObject(bold.into());
        let _ = DeleteObject(normal.into());
        end_buffered(b);
        return;
    }
    if let Some(path) = &adding {
        let nrc = RECT { left: pad, top: pad, right: w - pad, bottom: pad + line_h };
        draw_text(mem, &format!("name:  {name_buf}_"), nrc, st.fg, left_vc);
        SelectObject(mem, normal.into());
        let cmdline =
            if adding_args.is_empty() { path.clone() } else { format!("{path} {adding_args}") };
        let prc = RECT { left: pad, top: pad + line_h, right: w - pad, bottom: pad + line_h * 2 };
        draw_text(mem, &cmdline, prc, st.cell_occupied, left_vc | DT_END_ELLIPSIS);
        let hrc = RECT { left: pad, top: h - line_h - s(6), right: w - pad, bottom: h - s(6) };
        draw_text(mem, "Enter saves · Esc cancels", hrc, st.cell_occupied, left_vc);
        SelectObject(mem, old_font);
        let _ = DeleteObject(bold.into());
        let _ = DeleteObject(normal.into());
        end_buffered(b);
        return;
    }

    // Title + the four action rows.
    let trc = RECT { left: pad, top: pad, right: w - pad, bottom: pad + line_h };
    draw_text(mem, "manage apps", trc, st.accent, left_vc);
    SelectObject(mem, normal.into());
    draw_text(mem, "click a field to edit it", trc, st.cell_occupied, DT_RIGHT | DT_VCENTER | DT_SINGLELINE);
    let actions_top = pad + line_h + s(6);
    for (i, label) in MGR_ACTIONS.iter().enumerate() {
        let arc = RECT {
            left: pad,
            top: actions_top + i as i32 * line_h,
            right: w - pad,
            bottom: actions_top + (i as i32 + 1) * line_h,
        };
        draw_text(mem, label, arc, st.accent, left_vc);
    }

    // Entry rows: icon · name · group chip · browser chip (web) · ✕.
    let entries = launcher_entries();
    let rows_top = mgr_rows_top(scale);
    let offset = offset.min(entries.len().saturating_sub(1));
    let mut y = rows_top;
    let mut shown = 0usize;
    for (idx, e) in entries.iter().enumerate().skip(offset) {
        if y + line_h > h - line_h - s(6) {
            break;
        }
        if let Some(icon) = icon_for_path(&e.command) {
            let isz = s(16);
            let _ = DrawIconEx(mem, pad, y + (line_h - isz) / 2, icon, isz, isz, 0, None, DI_NORMAL);
        }
        SelectObject(mem, bold.into());
        let nrc = RECT { left: pad + s(24), top: y, right: w - s(340), bottom: y + line_h };
        draw_text(mem, &e.name, nrc, st.fg, left_vc | DT_END_ELLIPSIS);
        SelectObject(mem, normal.into());
        // Group chip
        let grc = RECT { left: w - s(330), top: y + s(3), right: w - s(195), bottom: y + line_h - s(3) };
        let (glabel, gcolor) = if group_edit == Some(idx) {
            (format!("{group_buf}_"), st.accent)
        } else if e.group.is_empty() {
            ("+ group".to_string(), st.cell_occupied)
        } else {
            (e.group.clone(), st.fg)
        };
        draw_chip(mem, grc, st.bg, if group_edit == Some(idx) { st.accent } else { st.cell_occupied });
        draw_text(mem, &glabel, grc, gcolor, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
        // Browser chip (web apps only)
        if entry_url(e).is_some() {
            let brc = RECT { left: w - s(190), top: y + s(3), right: w - s(48), bottom: y + line_h - s(3) };
            draw_chip(mem, brc, st.bg, st.cell_occupied);
            draw_text(
                mem,
                &format!("🌐 {}", entry_browser_label(e)),
                brc,
                st.fg,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
            );
        }
        let xrc = RECT { left: w - s(44), top: y, right: w - pad, bottom: y + line_h };
        draw_text(mem, "✕", xrc, st.fg, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        y += line_h;
        shown += 1;
    }
    if entries.is_empty() {
        let erc = RECT { left: pad, top: y, right: w - pad, bottom: y + line_h };
        draw_text(mem, "no apps yet — use the actions above", erc, st.cell_occupied, left_vc);
    }

    let remaining = entries.len().saturating_sub(offset + shown);
    let footer = if !message.is_empty() {
        message
    } else if remaining > 0 {
        format!("{remaining} more — wheel scrolls · Esc closes")
    } else {
        "group chip types a category · 🌐 chip cycles the browser · Esc closes".to_string()
    };
    let frc = RECT { left: pad, top: h - line_h - s(6), right: w - pad, bottom: h - s(6) };
    draw_text(mem, &footer, frc, st.cell_occupied, left_vc | DT_END_ELLIPSIS);

    SelectObject(mem, old_font);
    let _ = DeleteObject(bold.into());
    let _ = DeleteObject(normal.into());
    end_buffered(b);
}

// -------------------------------------------- installed-apps scanner panel

fn close_scan() {
    SCAN.with(|s| {
        let mut st = s.borrow_mut();
        if st.hwnd != 0 {
            let hwnd = st.hwnd;
            *st = ScanState::default();
            drop(st);
            let _ = unsafe { DestroyWindow(HWND(hwnd as *mut c_void)) };
        }
    });
}

fn scan_hwnd() -> Option<HWND> {
    SCAN.with(|s| {
        let hwnd = s.borrow().hwnd;
        (hwnd != 0).then(|| HWND(hwnd as *mut c_void))
    })
}

fn invalidate_scan() {
    if let Some(hwnd) = scan_hwnd() {
        let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
    }
}

fn collect_lnk(dir: &std::path::Path, depth: u32, out: &mut Vec<(String, String)>) {
    if depth > 4 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_lnk(&p, depth + 1, out);
        } else if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk")) {
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                let low = stem.to_lowercase();
                if low.contains("uninstall") || low.contains("désinstall") {
                    continue; // nobody wants the uninstaller in a launcher
                }
                out.push((stem.to_string(), p.to_string_lossy().to_string()));
            }
        }
    }
}

/// Every app the Start Menu knows about, as (name, .lnk path) — the same
/// shortcuts the Start Menu launches, so ShellExecute runs them as-is.
fn scan_start_menu() -> Vec<(String, String)> {
    let mut raw = Vec::new();
    let roots = [
        std::env::var_os("APPDATA").map(|d| {
            std::path::PathBuf::from(d).join("Microsoft\\Windows\\Start Menu\\Programs")
        }),
        std::env::var_os("ProgramData").map(|d| {
            std::path::PathBuf::from(d).join("Microsoft\\Windows\\Start Menu\\Programs")
        }),
    ];
    for root in roots.into_iter().flatten() {
        collect_lnk(&root, 0, &mut raw);
    }
    // Dedupe by name; the per-user Start Menu (scanned first) wins.
    let mut seen: HashMap<String, ()> = HashMap::new();
    let mut out: Vec<(String, String)> = Vec::new();
    for (name, path) in raw {
        if seen.insert(name.to_lowercase(), ()).is_none() {
            out.push((name, path));
        }
    }
    out.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    out
}

pub fn toggle_scan_panel() {
    if scan_hwnd().is_some() {
        close_scan();
        return;
    }
    let apps = scan_start_menu();
    crate::logln!("wtm: found {} Start Menu apps", apps.len());
    open_scan(apps, false);
}

/// Pick a bookmarks .html export and open the picker over its links.
fn import_bookmarks_flow() {
    let Some(hwnd) = mgr_hwnd() else { return };
    MGR.with(|s| s.borrow_mut().dialog_open = true);
    let picked = pick_file(
        hwnd,
        "Bookmark exports (*.html;*.htm)\0*.html;*.htm\0All files (*.*)\0*.*\0\0",
        "Choose a bookmarks export (.html)",
    );
    MGR.with(|s| s.borrow_mut().dialog_open = false);
    let Some(path) = picked else {
        if let Some(h) = mgr_hwnd() {
            Window::from_hwnd(h).focus();
        }
        return;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        crate::logln!("wtm: could not read {path}");
        return;
    };
    let marks = parse_bookmarks_html(&text);
    crate::logln!("wtm: {} bookmarks in {path}", marks.len());
    if marks.is_empty() {
        MGR.with(|s| s.borrow_mut().message = "no bookmarks found in that file".into());
        invalidate_mgr();
        if let Some(h) = mgr_hwnd() {
            Window::from_hwnd(h).focus();
        }
        return;
    }
    close_mgr();
    open_scan(marks, true);
}

fn open_scan(apps: Vec<(String, String)>, bookmarks: bool) {
    let Some(m) = monitor::active() else { return };
    unsafe {
        let scale = monitor_scale(m.handle);
        let s = |v: i32| (v as f32 * scale) as i32;
        let w = s(560);
        let h = (m.bounds.h * 3 / 4).min(s(720));
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        if let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            w!("wtm_appscan"),
            w!("wtm installed apps"),
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
            SCAN.with(|st| {
                let mut st = st.borrow_mut();
                st.hwnd = hwnd.0 as isize;
                st.apps = apps;
                st.bookmarks = bookmarks;
                st.browsers = if bookmarks { web_browser_choices("{url}") } else { Vec::new() };
                st.browser_idx = 0;
            });
            round_corners(hwnd);
            Window::from_hwnd(hwnd).focus();
        }
    }
}

/// Y where the scan panel's rows start; bookmarks mode adds a browser row.
fn scan_rows_top(scale: f32, bookmarks: bool) -> i32 {
    let s = |v: i32| (v as f32 * scale) as i32;
    s(20) + s(26) * if bookmarks { 3 } else { 2 }
}

/// Click on a bookmark: add it as a web app in the chosen browser, or
/// remove the matching entry if it is already in the launcher.
fn scan_toggle_bookmark(title: &str, url: &str) {
    let choice = SCAN.with(|s| {
        let st = s.borrow();
        st.browsers.get(st.browser_idx).cloned()
    });
    let Some(c) = choice else { return };
    crate::with_wm(|wm| {
        let ul = url.to_lowercase();
        if let Some(pos) = wm.config.launcher.iter().position(|e| {
            e.command.eq_ignore_ascii_case(url) || e.args.to_lowercase().contains(&ul)
        }) {
            wm.config.launcher.remove(pos);
        } else {
            let (command, args) = if c.exe.is_empty() {
                (url.to_string(), String::new())
            } else {
                (c.exe.clone(), c.args.replace("{url}", url))
            };
            wm.config.launcher.push(LauncherEntry {
                name: title.to_string(),
                command,
                args,
                dir: String::new(),
                key: String::new(),
                group: String::new(),
            });
        }
        if let Err(e) = crate::config::save_launcher(&wm.config.launcher) {
            crate::logln!("wtm: could not save config: {e}");
        }
    });
    crate::reregister_hotkeys();
    invalidate_scan();
    invalidate_launcher();
    invalidate_mgr();
}

fn scan_filtered() -> Vec<(String, String)> {
    SCAN.with(|s| {
        let st = s.borrow();
        st.apps
            .iter()
            .filter(|(n, _)| st.query.is_empty() || fuzzy_match(n, &st.query))
            .cloned()
            .collect()
    })
}

/// Click on a scanned app: add it to the launcher, or remove it if it is
/// already there (matched by command path).
fn scan_toggle_entry(name: &str, path: &str) {
    crate::with_wm(|wm| {
        if let Some(pos) =
            wm.config.launcher.iter().position(|e| e.command.eq_ignore_ascii_case(path))
        {
            wm.config.launcher.remove(pos);
        } else {
            wm.config.launcher.push(LauncherEntry {
                name: name.to_string(),
                command: path.to_string(),
                args: String::new(),
                dir: String::new(),
                key: String::new(),
                group: String::new(),
            });
        }
        if let Err(e) = crate::config::save_launcher(&wm.config.launcher) {
            crate::logln!("wtm: could not save config: {e}");
        }
    });
    crate::reregister_hotkeys(); // Launch(i) indices shift on removal
    invalidate_scan();
    invalidate_launcher();
    invalidate_mgr();
}

unsafe extern "system" fn scan_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint_scan(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_LBUTTONDOWN => {
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
            let s = |v: i32| (v as f32 * scale) as i32;
            let bookmarks = SCAN.with(|st| st.borrow().bookmarks);
            let rows_top = scan_rows_top(scale, bookmarks);
            // Bookmarks mode: the "open with" row cycles through browsers.
            if bookmarks && y >= rows_top - s(26) && y < rows_top {
                SCAN.with(|st| {
                    let mut st = st.borrow_mut();
                    if !st.browsers.is_empty() {
                        st.browser_idx = (st.browser_idx + 1) % st.browsers.len();
                    }
                });
                invalidate_scan();
            } else if y >= rows_top {
                let offset = SCAN.with(|st| st.borrow().offset);
                let idx = offset + ((y - rows_top) / s(26)) as usize;
                let filtered = scan_filtered();
                if let Some((name, path)) = filtered.get(idx) {
                    if bookmarks {
                        scan_toggle_bookmark(name, path);
                    } else {
                        scan_toggle_entry(name, path);
                    }
                }
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
            SCAN.with(|s| {
                let mut st = s.borrow_mut();
                let max = st.apps.len().saturating_sub(5) as i32;
                st.offset =
                    (st.offset as i32 - delta / 120 * 3).clamp(0, max) as usize;
            });
            invalidate_scan();
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            if wparam.0 as u32 == VK_ESCAPE.0 as u32 {
                close_scan();
            }
            LRESULT(0)
        }
        WM_CHAR => {
            SCAN.with(|s| {
                let mut st = s.borrow_mut();
                match wparam.0 as u32 {
                    0x08 => {
                        st.query.pop();
                    }
                    0x16 => push_clipboard(&mut st.query), // Ctrl+V
                    c if c >= 0x20 => {
                        if let Some(c) = char::from_u32(c) {
                            st.query.push(c);
                        }
                    }
                    _ => return,
                }
                st.offset = 0;
            });
            invalidate_scan();
            LRESULT(0)
        }
        WM_SYSCHAR => LRESULT(0),
        WM_KILLFOCUS => {
            close_scan();
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn paint_scan(hwnd: HWND) {
    let b = begin_buffered(hwnd);
    let (mem, w, h) = (b.mem, b.w, b.h);
    let st = style();
    let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
    let s = |v: i32| (v as f32 * scale) as i32;
    let pad = s(20);
    let (line_h, row_h) = (s(26), s(26));

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
    let left_vc = DT_LEFT | DT_VCENTER | DT_SINGLELINE;

    let (query, offset, total, bookmarks, browsers, browser_idx) = SCAN.with(|s| {
        let st = s.borrow();
        (
            st.query.clone(),
            st.offset,
            st.apps.len(),
            st.bookmarks,
            st.browsers.clone(),
            st.browser_idx,
        )
    });

    let title =
        if bookmarks { format!("bookmarks ({total})") } else { format!("installed apps ({total})") };
    let trc = RECT { left: pad, top: pad, right: w - pad, bottom: pad + line_h };
    draw_text(mem, &title, trc, st.accent, left_vc);
    draw_text(mem, "click to add / remove", trc, st.fg, DT_RIGHT | DT_VCENTER | DT_SINGLELINE);

    SelectObject(mem, normal.into());
    let src = RECT { left: pad, top: pad + line_h, right: w - pad, bottom: pad + line_h * 2 };
    if query.is_empty() {
        draw_text(mem, "type to filter…", src, st.cell_occupied, left_vc);
    } else {
        draw_text(mem, &format!("filter: {query}_"), src, st.fg, left_vc);
    }

    // Bookmarks mode: which browser new entries open with.
    if bookmarks {
        let brc = RECT { left: pad, top: pad + line_h * 2, right: w - pad, bottom: pad + line_h * 3 };
        let label = browsers
            .get(browser_idx)
            .map(|c| c.label.clone())
            .unwrap_or_else(|| "?".to_string());
        draw_text(mem, &format!("open with:  {label}"), brc, st.accent, left_vc);
        draw_text(mem, "click to change", brc, st.cell_occupied, DT_RIGHT | DT_VCENTER | DT_SINGLELINE);
    }

    // What the launcher already holds, for the ✓ markers: exact command
    // match for .lnk entries, URL-in-args match for web apps.
    let existing: Vec<(String, String)> = crate::with_wm(|wm| {
        wm.config
            .launcher
            .iter()
            .map(|e| (e.command.to_lowercase(), e.args.to_lowercase()))
            .collect()
    })
    .unwrap_or_default();

    let filtered = scan_filtered();
    let offset = offset.min(filtered.len().saturating_sub(1));
    let rows_top = scan_rows_top(scale, bookmarks);
    let mut y = rows_top;
    let mut shown = 0usize;
    for (name, path) in filtered.iter().skip(offset) {
        if y + row_h > h - line_h - s(6) {
            break;
        }
        let pl = path.to_lowercase();
        let added = existing
            .iter()
            .any(|(cmd, args)| *cmd == pl || (bookmarks && !pl.is_empty() && args.contains(&pl)));
        let nrc = RECT { left: pad, top: y, right: w - pad - s(120), bottom: y + row_h };
        draw_text(mem, name, nrc, st.fg, left_vc | DT_END_ELLIPSIS);
        let mrc = RECT { left: w - pad - s(120), top: y, right: w - pad, bottom: y + row_h };
        if added {
            draw_text(mem, "✓ in launcher", mrc, st.accent, DT_RIGHT | DT_VCENTER | DT_SINGLELINE);
        } else {
            draw_text(mem, "+ add", mrc, st.cell_occupied, DT_RIGHT | DT_VCENTER | DT_SINGLELINE);
        }
        y += row_h;
        shown += 1;
    }
    if filtered.is_empty() {
        let erc = RECT { left: pad, top: y, right: w - pad, bottom: y + row_h };
        draw_text(mem, "no match", erc, st.cell_occupied, left_vc);
    }

    let remaining = filtered.len().saturating_sub(offset + shown);
    let footer = if remaining > 0 {
        format!("{remaining} more — scroll or type to filter · Esc closes")
    } else {
        "wheel scrolls · type to filter · Esc closes".to_string()
    };
    let frc = RECT { left: pad, top: h - line_h - s(6), right: w - pad, bottom: h - s(6) };
    draw_text(mem, &footer, frc, st.cell_occupied, left_vc);

    SelectObject(mem, old_font);
    let _ = DeleteObject(bold.into());
    let _ = DeleteObject(normal.into());
    end_buffered(b);
}

// ------------------------------------------------- fuzzy window switcher

fn close_switcher() {
    SWITCHER.with(|s| {
        let mut st = s.borrow_mut();
        if st.hwnd != 0 {
            let hwnd = st.hwnd;
            *st = SwitcherState::default();
            drop(st);
            let _ = unsafe { DestroyWindow(HWND(hwnd as *mut c_void)) };
        }
    });
}

fn switcher_hwnd() -> Option<HWND> {
    SWITCHER.with(|s| {
        let hwnd = s.borrow().hwnd;
        (hwnd != 0).then(|| HWND(hwnd as *mut c_void))
    })
}

fn invalidate_switcher() {
    if let Some(hwnd) = switcher_hwnd() {
        let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
    }
}

/// All managed windows matching the query, in stable workspace order.
fn switcher_entries(query: &str) -> Vec<crate::wm::SwitchEntry> {
    let all = crate::with_wm(|wm| wm.window_list()).unwrap_or_default();
    if query.is_empty() {
        return all;
    }
    all.into_iter()
        .filter(|e| {
            let hay = format!("{} {} {}", e.place, e.window.title(), e.window.exe().unwrap_or_default());
            fuzzy_match(&hay, query)
        })
        .collect()
}

pub fn toggle_switcher_panel() {
    if switcher_hwnd().is_some() {
        close_switcher();
        return;
    }
    let Some(m) = monitor::active() else { return };
    unsafe {
        let scale = monitor_scale(m.handle);
        let s = |v: i32| (v as f32 * scale) as i32;
        let w = s(600);
        let h = (s(20) + s(26) * 2 + s(28) * 14 + s(40)).min(m.bounds.h - s(120));
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        if let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            w!("wtm_switcher"),
            w!("wtm windows"),
            WS_POPUP | WS_VISIBLE,
            m.bounds.x + (m.bounds.w - w) / 2,
            m.bounds.y + (m.bounds.h - h) / 3,
            w,
            h,
            None,
            None,
            Some(hinstance.into()),
            None,
        ) {
            SWITCHER.with(|s| s.borrow_mut().hwnd = hwnd.0 as isize);
            round_corners(hwnd);
            Window::from_hwnd(hwnd).focus();
        }
    }
}

unsafe extern "system" fn switcher_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint_switcher(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_LBUTTONDOWN => {
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
            let s = |v: i32| (v as f32 * scale) as i32;
            let rows_top = s(20) + s(26) * 2;
            if y >= rows_top {
                let idx = ((y - rows_top) / s(28)) as usize;
                switcher_activate(idx);
            }
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            on_switcher_key(wparam.0 as u32);
            LRESULT(0)
        }
        WM_CHAR => {
            on_switcher_char(wparam.0 as u32);
            LRESULT(0)
        }
        WM_SYSCHAR => LRESULT(0),
        WM_KILLFOCUS => {
            close_switcher();
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn switcher_activate(idx: usize) {
    let query = SWITCHER.with(|s| s.borrow().query.clone());
    let entries = switcher_entries(&query);
    let Some(e) = entries.get(idx) else { return };
    let w = e.window;
    close_switcher();
    crate::with_wm(|wm| wm.activate_window(w));
    invalidate_all();
    crate::border::update();
}

fn on_switcher_key(vk: u32) {
    if vk == VK_ESCAPE.0 as u32 {
        close_switcher();
        return;
    }
    if vk == VK_RETURN.0 as u32 {
        let sel = SWITCHER.with(|s| s.borrow().selected);
        switcher_activate(sel);
        return;
    }
    if vk == VK_UP.0 as u32 || vk == VK_DOWN.0 as u32 {
        let query = SWITCHER.with(|s| s.borrow().query.clone());
        let count = switcher_entries(&query).len();
        if count == 0 {
            return;
        }
        SWITCHER.with(|s| {
            let mut st = s.borrow_mut();
            let dir: i32 = if vk == VK_DOWN.0 as u32 { 1 } else { -1 };
            st.selected = (st.selected as i32 + dir).rem_euclid(count as i32) as usize;
        });
        invalidate_switcher();
    }
}

fn on_switcher_char(ch: u32) {
    SWITCHER.with(|s| {
        let mut st = s.borrow_mut();
        match ch {
            0x08 => {
                st.query.pop();
            }
            0x0D => return,
            0x16 => push_clipboard(&mut st.query), // Ctrl+V
            c if c >= 0x20 => {
                if let Some(c) = char::from_u32(c) {
                    st.query.push(c);
                }
            }
            _ => return,
        }
        st.selected = 0;
    });
    invalidate_switcher();
}

unsafe fn paint_switcher(hwnd: HWND) {
    let b = begin_buffered(hwnd);
    let (mem, w, h) = (b.mem, b.w, b.h);
    let st = style();
    let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
    let s = |v: i32| (v as f32 * scale) as i32;
    let pad = s(20);
    let (line_h, row_h) = (s(26), s(28));

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
    let left_vc = DT_LEFT | DT_VCENTER | DT_SINGLELINE;

    let (query, selected) = SWITCHER.with(|s| (s.borrow().query.clone(), s.borrow().selected));

    let trc = RECT { left: pad, top: pad, right: w - pad, bottom: pad + line_h };
    draw_text(mem, "windows", trc, st.accent, left_vc);
    draw_text(mem, "Enter or click to jump", trc, st.fg, DT_RIGHT | DT_VCENTER | DT_SINGLELINE);

    SelectObject(mem, normal.into());
    let src = RECT { left: pad, top: pad + line_h, right: w - pad, bottom: pad + line_h * 2 };
    if query.is_empty() {
        draw_text(mem, "type to filter…", src, st.cell_occupied, left_vc);
    } else {
        draw_text(mem, &format!("filter: {query}_"), src, st.fg, left_vc);
    }

    let entries = switcher_entries(&query);
    let rows_top = pad + line_h * 2;
    let mut y = rows_top;
    for (i, e) in entries.iter().enumerate() {
        if y + row_h > h - line_h {
            break;
        }
        if i == selected {
            let row = RECT { left: s(6), top: y, right: w - s(6), bottom: y + row_h };
            fill_round(mem, &row, st.cell_empty, s(6));
        }
        // Location tag
        SelectObject(mem, bold.into());
        let prc = RECT { left: pad, top: y, right: pad + s(150), bottom: y + row_h };
        draw_text(mem, &e.place, prc, st.accent, left_vc | DT_END_ELLIPSIS);
        // Icon
        let icon_sz = s(16);
        if let Some(icon) = icon_for(e.window) {
            let _ = DrawIconEx(
                mem,
                pad + s(158),
                y + (row_h - icon_sz) / 2,
                icon,
                icon_sz,
                icon_sz,
                0,
                None,
                DI_NORMAL,
            );
        }
        // Title
        SelectObject(mem, normal.into());
        let trc = RECT { left: pad + s(158) + icon_sz + s(8), top: y, right: w - pad, bottom: y + row_h };
        draw_text(mem, &e.window.title(), trc, st.fg, left_vc | DT_END_ELLIPSIS);
        y += row_h;
    }
    if entries.is_empty() {
        let erc = RECT { left: pad, top: y, right: w - pad, bottom: y + row_h };
        draw_text(mem, "no match", erc, st.cell_occupied, left_vc);
    }

    let frc = RECT { left: pad, top: h - line_h - s(6), right: w - pad, bottom: h - s(6) };
    draw_text(mem, "↑↓ select · Enter jumps · Esc closes", frc, st.cell_occupied, left_vc);

    SelectObject(mem, old_font);
    let _ = DeleteObject(bold.into());
    let _ = DeleteObject(normal.into());
    end_buffered(b);
}

// --------------------------------------------------- appearance (🎨) panel

fn close_tweaks() {
    TWEAKS.with(|t| {
        let raw = t.get();
        if raw != 0 {
            t.set(0);
            let _ = unsafe { DestroyWindow(HWND(raw as *mut c_void)) };
        }
    });
}

fn tweaks_hwnd() -> Option<HWND> {
    TWEAKS.with(|t| {
        let raw = t.get();
        (raw != 0).then(|| HWND(raw as *mut c_void))
    })
}

fn invalidate_tweaks() {
    if let Some(hwnd) = tweaks_hwnd() {
        let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
    }
}

const TWEAKS_LINE: i32 = 34;

fn tweaks_rows_top(scale: f32) -> i32 {
    let s = |v: i32| (v as f32 * scale) as i32;
    s(20) + s(26) + s(8) // pad + title row + separator
}

/// The [−] value [+] stepper boxes of a row, right-aligned in the panel.
fn stepper_rects(w: i32, y: i32, scale: f32) -> (RECT, RECT, RECT) {
    let s = |v: i32| (v as f32 * scale) as i32;
    let (box_w, box_h) = (s(28), s(24));
    let top = y + (s(TWEAKS_LINE) - box_h) / 2;
    let plus = RECT { left: w - s(20) - box_w, top, right: w - s(20), bottom: top + box_h };
    let val = RECT { left: plus.left - s(56), top, right: plus.left, bottom: top + box_h };
    let minus = RECT { left: val.left - box_w, top, right: val.left, bottom: top + box_h };
    (minus, val, plus)
}

fn swatch_rect(y: i32, scale: f32, i: usize) -> RECT {
    let s = |v: i32| (v as f32 * scale) as i32;
    let (sw, gap) = (s(24), s(6));
    let top = y + (s(TWEAKS_LINE) - sw) / 2;
    let left = s(20) + i as i32 * (sw + gap);
    RECT { left, top, right: left + sw, bottom: top + sw }
}

/// The "custom…" chip on the accent-color row, opening the full RGB picker.
fn custom_chip_rect(w: i32, y: i32, scale: f32) -> RECT {
    let s = |v: i32| (v as f32 * scale) as i32;
    let box_h = s(24);
    let top = y + (s(TWEAKS_LINE) - box_h) / 2;
    RECT { left: w - s(20) - s(84), top, right: w - s(20), bottom: top + box_h }
}

/// Full RGB picker (the classic Windows color dialog, opened fully expanded).
fn open_custom_color(hwnd: HWND) {
    let current = crate::with_wm(|wm| wm.config.active_border_color.clone()).unwrap_or_default();
    let mut custom = CUSTOM_COLORS.with(|c| c.get());
    let mut cc = CHOOSECOLORW {
        lStructSize: std::mem::size_of::<CHOOSECOLORW>() as u32,
        hwndOwner: hwnd,
        rgbResult: COLORREF(parse_colorref(&current)),
        lpCustColors: custom.as_mut_ptr(),
        Flags: CC_FULLOPEN | CC_RGBINIT | CC_ANYCOLOR,
        ..Default::default()
    };
    TWEAKS_DIALOG.with(|d| d.set(true));
    let ok = unsafe { ChooseColorW(&mut cc) }.as_bool();
    TWEAKS_DIALOG.with(|d| d.set(false));
    CUSTOM_COLORS.with(|c| c.set(custom));
    if let Some(h) = tweaks_hwnd() {
        Window::from_hwnd(h).focus();
    }
    if ok {
        // COLORREF is 0x00BBGGRR; the config stores "#RRGGBB".
        let c = cc.rgbResult.0;
        let hex = format!("#{:02x}{:02x}{:02x}", c & 0xFF, (c >> 8) & 0xFF, (c >> 16) & 0xFF);
        tweaks_set_color(&hex);
    }
}

pub fn toggle_tweaks_panel() {
    if tweaks_hwnd().is_some() {
        close_tweaks();
        return;
    }
    let Some(m) = monitor::active() else { return };
    unsafe {
        let scale = monitor_scale(m.handle);
        let s = |v: i32| (v as f32 * scale) as i32;
        let w = s(348);
        let h = tweaks_rows_top(scale) + 4 * s(TWEAKS_LINE) + s(44);
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        if let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            w!("wtm_tweaks"),
            w!("wtm appearance"),
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
            TWEAKS.with(|t| t.set(hwnd.0 as isize));
            round_corners(hwnd);
            Window::from_hwnd(hwnd).focus();
        }
    }
}

unsafe extern "system" fn tweaks_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint_tweaks(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            on_tweaks_click(hwnd, x, y);
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            if wparam.0 as u32 == VK_ESCAPE.0 as u32 {
                close_tweaks();
            }
            LRESULT(0)
        }
        WM_SYSCHAR => LRESULT(0),
        WM_KILLFOCUS => {
            if !TWEAKS_DIALOG.with(|d| d.get()) {
                close_tweaks();
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn on_tweaks_click(hwnd: HWND, x: i32, y: i32) {
    let mut rc = RECT::default();
    let _ = unsafe { GetClientRect(hwnd, &mut rc) };
    let scale = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0;
    let s = |v: i32| (v as f32 * scale) as i32;
    let rows_top = tweaks_rows_top(scale);
    let hit = |r: &RECT| x >= r.left && x < r.right && y >= r.top && y < r.bottom;

    // Row 0: workspace count, row 1: frame thickness.
    for row in 0..2 {
        let ry = rows_top + row * s(TWEAKS_LINE);
        let (minus, _, plus) = stepper_rects(rc.right, ry, scale);
        let delta = if hit(&minus) { -1 } else if hit(&plus) { 1 } else { continue };
        if row == 0 {
            tweaks_adjust_workspaces(delta);
        } else {
            tweaks_adjust_thickness(delta);
        }
        return;
    }
    // Row 2: the "custom…" chip opens the full RGB picker.
    let chip = custom_chip_rect(rc.right, rows_top + 2 * s(TWEAKS_LINE), scale);
    if hit(&chip) {
        open_custom_color(hwnd);
        return;
    }
    // Row 3: preset color swatches.
    let ry = rows_top + 3 * s(TWEAKS_LINE);
    for (i, hex) in PALETTE.iter().enumerate() {
        if hit(&swatch_rect(ry, scale, i)) {
            tweaks_set_color(hex);
            return;
        }
    }
}

fn tweaks_adjust_workspaces(delta: i32) {
    let cur = crate::with_wm(|wm| wm.config.workspaces).unwrap_or(9) as i32;
    let new = (cur + delta).clamp(1, 24) as usize;
    let applied = crate::with_wm(|wm| wm.set_workspace_count(new)).unwrap_or(false);
    if applied {
        if let Err(e) = crate::config::save_workspaces(new) {
            crate::logln!("wtm: could not save config: {e}");
        }
        crate::reregister_hotkeys(); // the Alt+0 → workspace 10 chord comes and goes
        invalidate_all();
        crate::border::update();
        invalidate_tweaks();
    }
}

fn tweaks_adjust_thickness(delta: i32) {
    let (cur, color) = crate::with_wm(|wm| {
        (wm.config.border_thickness, wm.config.active_border_color.clone())
    })
    .unwrap_or((3, String::new()));
    let new = (cur + delta).clamp(0, 12);
    if new == cur {
        return;
    }
    crate::with_wm(|wm| wm.config.border_thickness = new);
    crate::border::set_style(new, parse_colorref(&color));
    if let Err(e) = crate::config::save_border_thickness(new) {
        crate::logln!("wtm: could not save config: {e}");
    }
    invalidate_tweaks();
}

fn tweaks_set_color(hex: &str) {
    crate::with_wm(|wm| wm.set_accent_color(hex));
    let thickness = crate::with_wm(|wm| wm.config.border_thickness).unwrap_or(3);
    let color = parse_colorref(hex);
    crate::border::set_style(thickness, color);
    set_accent(color); // bar highlight + repaint of every bar
    if let Err(e) = crate::config::save_border_color(hex) {
        crate::logln!("wtm: could not save config: {e}");
    }
    invalidate_tweaks();
}

unsafe fn draw_stepper(mem: HDC, st: &BarStyle, w: i32, y: i32, scale: f32, value: &str) {
    let (minus, val, plus) = stepper_rects(w, y, scale);
    let center = DT_CENTER | DT_VCENTER | DT_SINGLELINE;
    draw_chip(mem, minus, st.cell_empty, st.cell_occupied);
    draw_text(mem, "−", minus, st.accent, center);
    draw_text(mem, value, val, st.fg, center);
    draw_chip(mem, plus, st.cell_empty, st.cell_occupied);
    draw_text(mem, "+", plus, st.accent, center);
}

unsafe fn paint_tweaks(hwnd: HWND) {
    let b = begin_buffered(hwnd);
    let (mem, w, h) = (b.mem, b.w, b.h);
    let st = style();
    let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
    let s = |v: i32| (v as f32 * scale) as i32;
    let pad = s(20);

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
    let left_vc = DT_LEFT | DT_VCENTER | DT_SINGLELINE;

    let trc = RECT { left: pad, top: pad, right: w - pad, bottom: pad + s(26) };
    draw_text(mem, "wtm appearance", trc, st.accent, left_vc);
    SelectObject(mem, normal.into());
    draw_text(mem, "applies instantly", trc, st.cell_occupied, DT_RIGHT | DT_VCENTER | DT_SINGLELINE);

    let (ws_count, thickness, color_hex) = crate::with_wm(|wm| {
        (wm.config.workspaces, wm.config.border_thickness, wm.config.active_border_color.clone())
    })
    .unwrap_or((9, 3, String::new()));

    let rows_top = tweaks_rows_top(scale);
    let row_rc = |row: i32| RECT {
        left: pad,
        top: rows_top + row * s(TWEAKS_LINE),
        right: w - pad,
        bottom: rows_top + (row + 1) * s(TWEAKS_LINE),
    };

    draw_text(mem, "workspaces", row_rc(0), st.fg, left_vc);
    draw_stepper(mem, &st, w, row_rc(0).top, scale, &ws_count.to_string());

    draw_text(mem, "focus frame", row_rc(1), st.fg, left_vc);
    let tval = if thickness == 0 { "off".to_string() } else { format!("{thickness} px") };
    draw_stepper(mem, &st, w, row_rc(1).top, scale, &tval);

    draw_text(mem, "accent color", row_rc(2), st.fg, left_vc);
    let chip = custom_chip_rect(w, row_rc(2).top, scale);
    draw_chip(mem, chip, st.cell_empty, st.cell_occupied);
    draw_text(mem, "custom…", chip, st.accent, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    let hex_rc = RECT { left: pad, top: row_rc(2).top, right: chip.left - s(10), bottom: row_rc(2).bottom };
    draw_text(mem, &color_hex, hex_rc, st.cell_occupied, DT_RIGHT | DT_VCENTER | DT_SINGLELINE);
    let sy = row_rc(3).top;
    for (i, hex) in PALETTE.iter().enumerate() {
        let r = swatch_rect(sy, scale, i);
        if hex.eq_ignore_ascii_case(&color_hex) {
            let ring = RECT { left: r.left - s(3), top: r.top - s(3), right: r.right + s(3), bottom: r.bottom + s(3) };
            fill(mem, &ring, st.fg);
        }
        fill(mem, &r, parse_colorref(hex));
    }

    let frc = RECT { left: pad, top: h - s(26) - s(6), right: w - pad, bottom: h - s(6) };
    draw_text(mem, "saved to config.toml · Esc closes", frc, st.cell_occupied, left_vc);

    SelectObject(mem, old_font);
    let _ = DeleteObject(bold.into());
    let _ = DeleteObject(normal.into());
    end_buffered(b);
}

pub fn toggle_settings_panel() {
    if settings_hwnd().is_some() {
        close_settings();
        return;
    }
    let Some(m) = monitor::active() else { return };
    unsafe {
        let scale = monitor_scale(m.handle);
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
            round_corners(hwnd);
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
                st.editing_cal = false;
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
        Some(SettingsItem::CalOff) => set_calendar_source(""),
        Some(SettingsItem::CalOutlook) => set_calendar_source("outlook"),
        Some(SettingsItem::CalIcs) => {
            let current = crate::with_wm(|wm| wm.config.calendar_source.clone())
                .unwrap_or_default();
            SETTINGS.with(|s| {
                let mut st = s.borrow_mut();
                st.editing = None;
                st.editing_cal = true;
                st.buffer = if current.eq_ignore_ascii_case("outlook") { String::new() } else { current };
            });
            invalidate_settings();
        }
        _ => {}
    }
}

fn on_settings_char(ch: u32) {
    let editing = SETTINGS.with(|s| {
        let st = s.borrow();
        st.editing.is_some() || st.editing_cal
    });
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
            0x16 => {
                // Ctrl+V: paths and URLs are pasted, not typed.
                if let Some(t) = clipboard_text() {
                    st.buffer.push_str(t.trim());
                }
            }
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
    let (editing, editing_cal) =
        SETTINGS.with(|s| (s.borrow().editing, s.borrow().editing_cal));
    if vk == VK_ESCAPE.0 as u32 {
        if editing.is_some() || editing_cal {
            SETTINGS.with(|s| {
                let mut st = s.borrow_mut();
                st.editing = None;
                st.editing_cal = false;
                st.buffer.clear();
            });
            invalidate_settings();
        } else {
            close_settings();
        }
        return;
    }
    if vk == VK_RETURN.0 as u32 {
        if editing_cal {
            let source = SETTINGS.with(|s| s.borrow().buffer.trim().to_string());
            SETTINGS.with(|s| {
                let mut st = s.borrow_mut();
                st.editing_cal = false;
                st.buffer.clear();
            });
            set_calendar_source(&source);
            return;
        }
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

    let (editing, editing_cal, buffer) = SETTINGS.with(|s| {
        let st = s.borrow();
        (st.editing, st.editing_cal, st.buffer.clone())
    });
    let (names, cal_source) = crate::with_wm(|wm| {
        (wm.config.workspace_names.clone(), wm.config.calendar_source.clone())
    })
    .unwrap_or_default();

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
            SettingsItem::CalHeader => {
                SelectObject(mem, bold.into());
                let rrc = RECT { left: pad, top: y, right: w - pad, bottom: y + line_h };
                draw_text(mem, "meetings calendar", rrc, st.accent, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
                SelectObject(mem, normal.into());
                draw_text(mem, "shows your next meeting next to the clock", rrc, st.cell_occupied, DT_RIGHT | DT_VCENTER | DT_SINGLELINE);
            }
            SettingsItem::CalOff => {
                SelectObject(mem, normal.into());
                let on = cal_source.is_empty();
                let rrc = RECT { left: pad, top: y, right: w - pad, bottom: y + line_h };
                let mark = if on { "●" } else { "○" };
                draw_text(mem, &format!("{mark}  off"), rrc, if on { st.fg } else { st.cell_occupied }, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
            }
            SettingsItem::CalOutlook => {
                SelectObject(mem, normal.into());
                let on = cal_source.eq_ignore_ascii_case("outlook");
                let rrc = RECT { left: pad, top: y, right: w - pad, bottom: y + line_h };
                let mark = if on { "●" } else { "○" };
                draw_text(mem, &format!("{mark}  outlook — classic Outlook calendar (Teams meetings)"), rrc, if on { st.fg } else { st.cell_occupied }, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
            }
            SettingsItem::CalIcs => {
                SelectObject(mem, normal.into());
                let custom = !cal_source.is_empty() && !cal_source.eq_ignore_ascii_case("outlook");
                if editing_cal {
                    let row = RECT { left: s(6), top: y, right: w - s(6), bottom: y + line_h };
                    fill(mem, &row, st.cell_empty);
                }
                let mark = if custom { "●" } else { "○" };
                let lrc = RECT { left: pad, top: y, right: pad + s(140), bottom: y + line_h };
                draw_text(mem, &format!("{mark}  .ics file or URL:"), lrc, if custom { st.fg } else { st.cell_occupied }, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
                let vrc = RECT { left: pad + s(148), top: y, right: w - pad, bottom: y + line_h };
                if editing_cal {
                    draw_text(mem, &format!("{buffer}_"), vrc, st.fg, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
                } else if custom {
                    draw_text(mem, &cal_source, vrc, st.fg, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
                } else {
                    draw_text(mem, "click to type or paste (Ctrl+V), Enter saves", vrc, st.cell_occupied, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_netscape_bookmarks() {
        let html = r##"<DL><p>
<DT><A HREF="https://grafana.example.com/d/1?a=1&amp;b=2" ADD_DATE="123">Grafana &amp; Friends</A>
<DT><a href="http://youtube.com">YouTube</a>
<DT><A HREF="place:folder=TOOLBAR">ignored non-http</A>
</DL>"##;
        let marks = parse_bookmarks_html(html);
        assert_eq!(marks.len(), 2);
        assert_eq!(marks[0].0, "Grafana & Friends");
        assert_eq!(marks[0].1, "https://grafana.example.com/d/1?a=1&b=2");
        assert_eq!(marks[1], ("YouTube".to_string(), "http://youtube.com".to_string()));
    }

    #[test]
    fn url_normalization_and_names() {
        assert_eq!(normalize_url("grafana.corp.com/d/1"), "https://grafana.corp.com/d/1");
        assert_eq!(normalize_url("http://a.b"), "http://a.b");
        assert_eq!(url_display_name("https://www.youtube.com/feed"), "youtube.com");
    }
}

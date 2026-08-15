//! Workspace overview (Exposé): a grid over the focused monitor's work area
//! showing every workspace as a miniature of its real layout, with app
//! icons and window titles. Click a window to jump straight to it, click a
//! card's empty space to switch workspaces, Esc or focus-loss dismisses.
//!
//! Cards are drawn from the layout data itself (not screen captures), so
//! hidden workspaces render exactly as faithfully as the visible one.

use crate::bar;
use crate::layout::Rect;
use crate::window::Window;
use crate::wm::OverviewSnapshot;
use std::cell::Cell;
use std::ffi::c_void;
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, SelectObject, DT_CENTER, DT_END_ELLIPSIS, DT_SINGLELINE, DT_VCENTER, FW_BOLD,
    FW_NORMAL,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    VK_DOWN, VK_ESCAPE, VK_LEFT, VK_RETURN, VK_RIGHT, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DrawIconEx, GetClientRect, GetWindowLongPtrW,
    LoadCursorW, RegisterClassW, SetWindowLongPtrW, CS_HREDRAW, CS_VREDRAW, DI_NORMAL,
    GWLP_USERDATA, IDC_ARROW, WM_ERASEBKGND, WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_PAINT,
    WM_SYSCHAR, WM_SYSKEYDOWN, WNDCLASSW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};

thread_local! {
    static OVERVIEW: Cell<isize> = const { Cell::new(0) };
    /// Card highlighted for keyboard navigation (arrows + Enter).
    static SELECTED: Cell<usize> = const { Cell::new(0) };
}

pub fn init() {
    unsafe {
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(overview_proc),
            hInstance: hinstance.into(),
            lpszClassName: w!("wtm_overview"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&class);
    }
}

pub fn toggle() {
    if OVERVIEW.with(|o| o.get()) != 0 {
        close();
    } else {
        open();
    }
}

pub fn close() {
    OVERVIEW.with(|o| {
        let raw = o.get();
        if raw != 0 {
            o.set(0);
            let _ = unsafe { DestroyWindow(HWND(raw as *mut c_void)) };
        }
    });
}

fn open() {
    let Some((handle, wa, active)) = crate::with_wm(|wm| {
        let mi = wm.focused_monitor_index();
        wm.monitors.get(mi).map(|m| (m.handle, m.work_area, m.active))
    })
    .flatten() else {
        return;
    };
    SELECTED.with(|s| s.set(active));
    unsafe {
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        if let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            w!("wtm_overview"),
            w!("wtm overview"),
            WS_POPUP | WS_VISIBLE,
            wa.x,
            wa.y,
            wa.w,
            wa.h,
            None,
            None,
            Some(hinstance.into()),
            None,
        ) {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, handle);
            OVERVIEW.with(|o| o.set(hwnd.0 as isize));
            bar::round_corners(hwnd);
            Window::from_hwnd(hwnd).focus();
        }
    }
}

unsafe extern "system" fn overview_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            on_click(hwnd, x, y);
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            on_key(hwnd, wparam.0 as u32);
            LRESULT(0)
        }
        WM_SYSCHAR => LRESULT(0),
        WM_KILLFOCUS => {
            close();
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ---------------------------------------------------------------- geometry

/// Card rectangles for `n` workspaces in a `cw`×`ch` client area.
fn cards(cw: i32, ch: i32, n: usize, scale: f32) -> Vec<RECT> {
    let s = |v: i32| (v as f32 * scale) as i32;
    let (margin, gap, footer) = (s(28), s(18), s(30));
    let cols = (n as f32).sqrt().ceil().max(1.0) as i32;
    let rows = (n as i32 + cols - 1) / cols;
    let card_w = ((cw - 2 * margin - (cols - 1) * gap) / cols).max(50);
    let card_h = ((ch - 2 * margin - footer - (rows - 1) * gap) / rows).max(50);
    (0..n as i32)
        .map(|i| {
            let (r, c) = (i / cols, i % cols);
            RECT {
                left: margin + c * (card_w + gap),
                top: margin + r * (card_h + gap),
                right: margin + c * (card_w + gap) + card_w,
                bottom: margin + r * (card_h + gap) + card_h,
            }
        })
        .collect()
}

/// The miniature drawing area inside a card: below the header, letterboxed
/// to the source (monitor work area) aspect ratio.
fn body_rect(card: RECT, src: Rect, scale: f32) -> RECT {
    let s = |v: i32| (v as f32 * scale) as i32;
    let (header, pad) = (s(24), s(8));
    let iw = (card.right - card.left - 2 * pad).max(1);
    let ih = (card.bottom - card.top - header - 2 * pad).max(1);
    let k = (iw as f32 / src.w.max(1) as f32).min(ih as f32 / src.h.max(1) as f32);
    let (bw, bh) = ((src.w as f32 * k) as i32, (src.h as f32 * k) as i32);
    let bx = card.left + pad + (iw - bw) / 2;
    let by = card.top + header + pad + (ih - bh) / 2;
    RECT { left: bx, top: by, right: bx + bw, bottom: by + bh }
}

/// Map a window rect from source space into a card body. Empty when the
/// window lies entirely outside the source area (stray floats).
fn map_rect(r: Rect, src: Rect, body: RECT) -> Option<RECT> {
    let x1 = r.x.max(src.x);
    let y1 = r.y.max(src.y);
    let x2 = (r.x + r.w).min(src.x + src.w);
    let y2 = (r.y + r.h).min(src.y + src.h);
    if x2 <= x1 || y2 <= y1 {
        return None;
    }
    let (bw, bh) = (body.right - body.left, body.bottom - body.top);
    let fx = |v: i32| body.left + ((v - src.x) as i64 * bw as i64 / src.w.max(1) as i64) as i32;
    let fy = |v: i32| body.top + ((v - src.y) as i64 * bh as i64 / src.h.max(1) as i64) as i32;
    Some(RECT { left: fx(x1), top: fy(y1), right: fx(x2), bottom: fy(y2) })
}

fn snapshot_for(hwnd: HWND) -> Option<OverviewSnapshot> {
    let monitor = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
    crate::with_wm(|wm| wm.overview_snapshot(monitor)).flatten()
}

// --------------------------------------------------------------- keyboard

fn on_key(hwnd: HWND, vk: u32) {
    if vk == VK_ESCAPE.0 as u32 {
        close();
        return;
    }
    if vk == VK_RETURN.0 as u32 {
        jump_to(hwnd, SELECTED.with(|s| s.get()));
        return;
    }
    // Digits jump straight to that workspace (1-9, 0 = 10).
    if (0x30..=0x39).contains(&vk) {
        let idx = if vk == 0x30 { 9 } else { (vk - 0x31) as usize };
        let n = snapshot_for(hwnd).map(|s| s.workspaces.len()).unwrap_or(0);
        if idx < n {
            jump_to(hwnd, idx);
        }
        return;
    }
    let dir = match vk as u16 {
        v if v == VK_LEFT.0 => (0, -1),
        v if v == VK_RIGHT.0 => (0, 1),
        v if v == VK_UP.0 => (-1, 0),
        v if v == VK_DOWN.0 => (1, 0),
        _ => return,
    };
    let Some(snap) = snapshot_for(hwnd) else { return };
    let n = snap.workspaces.len() as i32;
    if n == 0 {
        return;
    }
    // Same grid the painter uses.
    let cols = (n as f32).sqrt().ceil().max(1.0) as i32;
    let rows = (n + cols - 1) / cols;
    let cur = SELECTED.with(|s| s.get()) as i32;
    let (r, c) = (cur / cols, cur % cols);
    let (r, c) = ((r + dir.0).rem_euclid(rows), (c + dir.1).rem_euclid(cols));
    let idx = (r * cols + c).min(n - 1); // the last row may be short
    SELECTED.with(|s| s.set(idx as usize));
    let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
}

/// Switch to workspace `idx` on the overview's monitor and dismiss.
fn jump_to(hwnd: HWND, idx: usize) {
    let monitor = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
    close();
    crate::with_wm(|wm| {
        let mi = wm.monitor_index_of_handle(monitor);
        wm.switch_workspace_on(mi, idx);
    });
    bar::invalidate_all();
    crate::border::update();
}

// ---------------------------------------------------------------- painting

unsafe fn paint(hwnd: HWND) {
    let b = bar::begin_buffered(hwnd);
    let (mem, w, h) = (b.mem, b.w, b.h);
    let st = bar::style();
    let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
    let s = |v: i32| (v as f32 * scale) as i32;

    bar::fill(mem, &RECT { left: 0, top: 0, right: w, bottom: h }, st.bg);

    let Some(snap) = snapshot_for(hwnd) else {
        bar::end_buffered(b);
        return;
    };
    let n = snap.workspaces.len();
    let grid = cards(w, h, n, scale);
    let focused = Window::foreground();

    let bold = bar::make_font(s(14), FW_BOLD.0 as i32);
    let normal = bar::make_font(s(11), FW_NORMAL.0 as i32);
    let old_font = SelectObject(mem, bold.into());
    let center = DT_CENTER | DT_VCENTER | DT_SINGLELINE;

    let selected = SELECTED.with(|sel| sel.get());
    for (i, card) in grid.iter().enumerate() {
        let active = i == snap.active;
        // Keyboard selection: a bright halo around the card.
        if i == selected {
            let halo = RECT {
                left: card.left - s(4),
                top: card.top - s(4),
                right: card.right + s(4),
                bottom: card.bottom + s(4),
            };
            bar::fill(mem, &halo, st.fg);
        }
        let ring = if active { st.accent } else { st.cell_empty };
        bar::fill(mem, card, ring);
        let t = s(2);
        let interior =
            RECT { left: card.left + t, top: card.top + t, right: card.right - t, bottom: card.bottom - t };
        bar::fill(mem, &interior, if active { st.cell_empty } else { st.bg });

        // Header: "n · name"
        SelectObject(mem, bold.into());
        let name = snap.names.get(i).map(String::as_str).unwrap_or("");
        let label = if name.is_empty() {
            format!("{}", i + 1)
        } else {
            format!("{} · {}", i + 1, name)
        };
        let hrc = RECT {
            left: card.left + s(10),
            top: card.top + s(2),
            right: card.right - s(10),
            bottom: card.top + s(24),
        };
        bar::draw_text(mem, &label, hrc, if active { st.accent } else { st.fg },
            DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);

        // Miniature layout
        let body = body_rect(*card, snap.source, scale);
        for (win, r) in &snap.workspaces[i] {
            let Some(m) = map_rect(*r, snap.source, body) else { continue };
            let is_focused = focused == Some(*win);
            bar::fill(mem, &m, if is_focused { st.accent } else { st.cell_occupied });
            let t = if is_focused { s(2) } else { 1 };
            let inner = RECT { left: m.left + t, top: m.top + t, right: m.right - t, bottom: m.bottom - t };
            if inner.right > inner.left && inner.bottom > inner.top {
                bar::fill(mem, &inner, st.cell_occupied);
            }
            let (mw, mh) = (m.right - m.left, m.bottom - m.top);
            if mw >= s(22) && mh >= s(22) {
                let sz = (mw.min(mh) / 2).clamp(s(16), s(48));
                if let Some(icon) = bar::icon_for(*win) {
                    let _ = DrawIconEx(
                        mem,
                        m.left + (mw - sz) / 2,
                        m.top + (mh - sz) / 2 - if mh >= s(52) { s(8) } else { 0 },
                        icon,
                        sz,
                        sz,
                        0,
                        None,
                        DI_NORMAL,
                    );
                }
            }
            if mw >= s(80) && mh >= s(52) {
                SelectObject(mem, normal.into());
                let trc = RECT {
                    left: m.left + s(4),
                    top: m.bottom - s(18),
                    right: m.right - s(4),
                    bottom: m.bottom - s(2),
                };
                bar::draw_text(mem, &win.title(), trc, st.fg, center | DT_END_ELLIPSIS);
            }
        }
    }

    SelectObject(mem, normal.into());
    let frc = RECT { left: 0, top: h - s(26), right: w, bottom: h - s(4) };
    bar::draw_text(
        mem,
        "←↑↓→ + Enter or digits to jump · click a window · Esc closes",
        frc,
        st.cell_occupied,
        center,
    );

    SelectObject(mem, old_font);
    let _ = DeleteObject(bold.into());
    let _ = DeleteObject(normal.into());
    bar::end_buffered(b);
}

// ----------------------------------------------------------------- clicks

fn on_click(hwnd: HWND, x: i32, y: i32) {
    let mut rc = RECT::default();
    let _ = unsafe { GetClientRect(hwnd, &mut rc) };
    let scale = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0;
    let monitor = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
    let Some(snap) = snapshot_for(hwnd) else { return };
    let grid = cards(rc.right, rc.bottom, snap.workspaces.len(), scale);
    let hit = |r: &RECT| x >= r.left && x < r.right && y >= r.top && y < r.bottom;

    let Some(ci) = grid.iter().position(hit) else { return };
    // Topmost window under the click: floats are drawn last, so scan back.
    let body = body_rect(grid[ci], snap.source, scale);
    let target_win = snap.workspaces[ci]
        .iter()
        .rev()
        .find(|(_, r)| map_rect(*r, snap.source, body).is_some_and(|m| hit(&m)))
        .map(|(w, _)| *w);

    close();
    crate::with_wm(|wm| {
        let mi = wm.monitor_index_of_handle(monitor);
        wm.switch_workspace_on(mi, ci);
    });
    if let Some(w) = target_win {
        w.focus();
    }
    bar::invalidate_all();
    crate::border::update();
}

//! The window manager itself: monitors → workspaces → windows, plus the
//! reactions to system events and user hotkeys. Everything runs on the main
//! thread, so there is no locking anywhere.

use crate::config::{parse_colorref, Config};
use crate::layout::{dwindle, Rect};
use crate::monitor;
use crate::window::Window;
use std::collections::HashSet;
use std::io::Write as _;
use std::path::PathBuf;
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

/// System events forwarded from the WinEvent hook.
#[derive(Debug, Clone, Copy)]
pub enum WmEvent {
    /// Window became visible (shown or uncloaked).
    Shown(Window),
    /// Window restored from minimize — re-managed, but app rules are NOT
    /// applied (restoring must never teleport a window away).
    Restored(Window),
    /// Window went away (hidden, cloaked or destroyed).
    Hidden(Window),
    Destroyed(Window),
    Foreground(#[allow(dead_code)] Window),
    MinimizeStart(Window),
    /// The user finished dragging or resizing a window with the mouse.
    /// Carries the bar workspace cell under the cursor, if any, so a window
    /// dropped onto the bar is sent to that workspace.
    MoveSizeEnd(Window, Option<(isize, usize)>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// User commands bound to hotkeys.
#[derive(Debug, Clone, Copy)]
pub enum Command {
    FocusNext,
    FocusPrev,
    FocusDir(Dir),
    SwapNext,
    SwapPrev,
    SwapDir(Dir),
    Promote,
    GrowWindow,
    ShrinkWindow,
    ToggleFloat,
    ToggleMonocle,
    ToggleFullscreen,
    CloseWindow,
    SwitchWorkspace(usize),
    MoveToWorkspace(usize),
    NextWorkspace,
    PrevWorkspace,
    MoveNextWorkspace,
    MovePrevWorkspace,
    Retile,
    TogglePause,
    ShowHelp,
    Launcher,
    /// Launch config.launcher[i] via its assigned shortcut.
    Launch(usize),
    PinApp,
    Exit,
}

#[derive(Default)]
pub struct Workspace {
    /// Layout order; index 0 is the "master" (largest) slot.
    pub tiled: Vec<Window>,
    pub floating: Vec<Window>,
    /// Per-split ratios for the dwindle spiral; ratios[i] is the share
    /// window i keeps when it splits off the remaining space.
    pub ratios: Vec<f32>,
    pub monocle: bool,
    /// Window covering the whole monitor (over the bar), if any.
    pub fullscreen: Option<Window>,
}

impl Workspace {
    fn all_windows(&self) -> impl Iterator<Item = Window> + '_ {
        self.tiled.iter().chain(self.floating.iter()).copied()
    }

    fn remove(&mut self, w: Window) -> bool {
        let before = self.tiled.len() + self.floating.len();
        self.tiled.retain(|x| *x != w);
        self.floating.retain(|x| *x != w);
        before != self.tiled.len() + self.floating.len()
    }
}

pub struct MonitorState {
    pub handle: isize,
    /// Full monitor rectangle (fullscreen covers this, bar included).
    pub bounds: Rect,
    pub work_area: Rect,
    pub workspaces: Vec<Workspace>,
    pub active: usize,
}

pub struct BarSnapshot {
    pub active: usize,
    /// One entry per workspace; empty string where unnamed.
    pub names: Vec<String>,
    pub occupied: Vec<bool>,
    pub title: String,
    pub paused: bool,
}

pub struct WindowManager {
    pub config: Config,
    pub monitors: Vec<MonitorState>,
    /// Windows *we* hid for workspace switching — their HIDE events must not
    /// be mistaken for the app closing its own window.
    hidden_by_us: HashSet<isize>,
    border_color: u32,
    paused: bool,
}

impl WindowManager {
    pub fn new(config: Config) -> Self {
        let border_color = parse_colorref(&config.active_border_color);
        let monitors = monitor::enumerate()
            .into_iter()
            .map(|m| MonitorState {
                handle: m.handle,
                bounds: m.bounds,
                work_area: m.work_area,
                workspaces: (0..config.workspaces).map(|_| Workspace::default()).collect(),
                active: 0,
            })
            .collect();
        WindowManager { config, monitors, hidden_by_us: HashSet::new(), border_color, paused: false }
    }

    // ---------------------------------------------------------------- lookup

    fn find(&self, w: Window) -> Option<(usize, usize)> {
        for (mi, mon) in self.monitors.iter().enumerate() {
            for (wi, ws) in mon.workspaces.iter().enumerate() {
                if ws.tiled.contains(&w) || ws.floating.contains(&w) {
                    return Some((mi, wi));
                }
            }
        }
        None
    }

    fn is_managed(&self, w: Window) -> bool {
        self.find(w).is_some()
    }

    pub fn monitor_index_of_handle(&self, handle: isize) -> usize {
        self.monitors.iter().position(|m| m.handle == handle).unwrap_or(0)
    }

    /// Monitor the user is working on: the foreground window's monitor if we
    /// manage it, otherwise the monitor under the mouse cursor.
    fn focused_monitor(&self) -> usize {
        if let Some(w) = Window::foreground() {
            if self.is_managed(w) {
                return self.monitor_index_of_handle(w.monitor());
            }
        }
        let mut pt = POINT::default();
        let _ = unsafe { GetCursorPos(&mut pt) };
        let hmon = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTOPRIMARY) };
        self.monitor_index_of_handle(hmon.0 as isize)
    }

    /// The foreground window, if it lives in some workspace we manage.
    fn focused_window(&self) -> Option<(Window, usize, usize)> {
        let w = Window::foreground()?;
        let (mi, wi) = self.find(w)?;
        Some((w, mi, wi))
    }

    // ------------------------------------------------------------ management

    /// Start managing a window, placing it after the focused window so new
    /// windows split the focused slot, like Hyprland's dwindle layout.
    /// With `apply_rules`, a pinned app's new window goes to its home
    /// workspace instead (hidden there if that workspace isn't active).
    fn manage(&mut self, w: Window, apply_rules: bool) {
        if self.is_managed(w) || self.paused {
            return;
        }
        let mi = self.monitor_index_of_handle(w.monitor());
        let floats = w.should_float(&self.config);
        let focused = Window::foreground();
        let active = self.monitors[mi].active;
        let mut target = active;
        if apply_rules && !self.config.app_rules.is_empty() {
            if let Some(exe) = w.exe() {
                if let Some(&n) = self.config.app_rules.get(&exe.to_lowercase()) {
                    if n >= 1 && n <= self.monitors[mi].workspaces.len() {
                        target = n - 1;
                    }
                }
            }
        }
        let ws = &mut self.monitors[mi].workspaces[target];
        if floats {
            ws.floating.push(w);
        } else {
            let insert_at = if target == active {
                focused
                    .and_then(|f| ws.tiled.iter().position(|x| *x == f))
                    .map(|i| i + 1)
                    .unwrap_or(ws.tiled.len())
            } else {
                ws.tiled.len()
            };
            ws.tiled.insert(insert_at, w);
        }
        if target != active {
            self.hidden_by_us.insert(w.0);
            w.hide();
            self.persist_hidden();
        } else {
            self.retile_monitor(mi);
        }
        self.update_borders();
    }

    fn unmanage(&mut self, w: Window) {
        if let Some((mi, wi)) = self.find(w) {
            if self.monitors[mi].workspaces[wi].fullscreen == Some(w) {
                self.monitors[mi].workspaces[wi].fullscreen = None;
                w.set_topmost(false);
            }
            self.monitors[mi].workspaces[wi].remove(w);
            w.set_border_color(None);
            self.retile_monitor(mi);
            self.update_borders();
        }
        self.hidden_by_us.remove(&w.0);
        self.persist_hidden();
    }

    /// Initial adoption of everything already on screen. EnumWindows hands us
    /// windows in Z-order (topmost first), which makes a pleasant layout.
    pub fn adopt_existing_windows(&mut self, windows: Vec<Window>) {
        for w in windows {
            if !w.is_manageable(&self.config) || self.is_managed(w) {
                continue;
            }
            let mi = self.monitor_index_of_handle(w.monitor());
            let mon = &mut self.monitors[mi];
            let ws = &mut mon.workspaces[mon.active];
            if w.should_float(&self.config) {
                ws.floating.push(w);
            } else {
                ws.tiled.push(w);
            }
        }
        self.retile_all();
        self.update_borders();
    }

    // --------------------------------------------------------------- tiling

    fn retile_monitor(&mut self, mi: usize) {
        if self.paused {
            return;
        }
        let outer = self.config.outer_gap;
        let inner = self.config.inner_gap;
        let mon = &mut self.monitors[mi];
        let ws = &mut mon.workspaces[mon.active];
        ws.tiled.retain(|w| w.is_valid());
        ws.floating.retain(|w| w.is_valid());
        let anim = self.config.animation_ms;
        let bounds = mon.bounds;
        // Drop fullscreen state if its window left the workspace.
        if let Some(fw) = ws.fullscreen {
            if !ws.tiled.contains(&fw) && !ws.floating.contains(&fw) {
                ws.fullscreen = None;
            }
        }
        let area = mon.work_area.shrink(outer);
        if ws.monocle {
            for w in &ws.tiled {
                if !w.is_minimized() {
                    crate::animate::set_target(*w, area, anim);
                }
            }
        } else {
            let needed = ws.tiled.len().saturating_sub(1);
            if ws.ratios.len() < needed {
                ws.ratios.resize(needed, self.config.split_ratio);
            }
            let rects = dwindle(area, ws.tiled.len(), &ws.ratios, inner);
            for (w, r) in ws.tiled.iter().zip(rects) {
                crate::animate::set_target(*w, r, anim);
            }
        }
        if let Some(fw) = ws.fullscreen {
            crate::animate::set_target(fw, bounds, anim);
        }
    }

    pub fn retile_all(&mut self) {
        for mi in 0..self.monitors.len() {
            self.retile_monitor(mi);
        }
    }

    /// Paint the accent border on the focused window, default on the rest.
    fn update_borders(&self) {
        if self.paused {
            return;
        }
        let focused = Window::foreground();
        for mon in &self.monitors {
            let ws = &mon.workspaces[mon.active];
            for w in ws.all_windows() {
                w.set_border_color((Some(w) == focused).then_some(self.border_color));
            }
        }
    }

    // --------------------------------------------------------------- events

    pub fn handle_event(&mut self, ev: WmEvent) {
        match ev {
            WmEvent::Shown(w) => {
                self.hidden_by_us.remove(&w.0);
                if !self.is_managed(w) && w.is_manageable(&self.config) {
                    self.manage(w, true);
                }
            }
            WmEvent::Restored(w) => {
                self.hidden_by_us.remove(&w.0);
                if !self.is_managed(w) && w.is_manageable(&self.config) {
                    self.manage(w, false);
                }
            }
            WmEvent::Hidden(w) => {
                // Ignore hides we caused ourselves during workspace switches.
                if !self.hidden_by_us.contains(&w.0) && self.is_managed(w) {
                    self.unmanage(w);
                }
            }
            WmEvent::Destroyed(w) => {
                if self.is_managed(w) {
                    self.unmanage(w);
                } else {
                    self.hidden_by_us.remove(&w.0);
                }
            }
            WmEvent::MinimizeStart(w) => {
                if self.is_managed(w) {
                    self.unmanage(w);
                }
            }
            WmEvent::Foreground(_) => {
                self.update_borders();
            }
            WmEvent::MoveSizeEnd(w, drop) => self.on_drag_end(w, drop),
        }
    }

    /// After a mouse drag: dropped on a bar workspace cell, the window is
    /// sent to that workspace; dropped onto another tiled window, they swap
    /// slots; either way everything snaps back into place.
    fn on_drag_end(&mut self, w: Window, drop: Option<(isize, usize)>) {
        if let Some((mon_handle, target)) = drop {
            if self.is_managed(w) {
                self.send_window_to(w, mon_handle, target);
                return;
            }
        }
        let Some((mi, wi)) = self.find(w) else { return };
        let mon = &self.monitors[mi];
        if wi != mon.active {
            return;
        }
        let is_tiled = mon.workspaces[wi].tiled.contains(&w);
        if is_tiled {
            let mut pt = POINT::default();
            let _ = unsafe { GetCursorPos(&mut pt) };
            let ws = &mut self.monitors[mi].workspaces[wi];
            let src = ws.tiled.iter().position(|x| *x == w).unwrap();
            let target = ws
                .tiled
                .iter()
                .position(|other| *other != w && other.rect().contains(pt.x, pt.y));
            if let Some(dst) = target {
                ws.tiled.swap(src, dst);
            }
            self.retile_monitor(mi);
        }
    }

    // ------------------------------------------------------------- commands

    /// Returns false when the user asked to exit.
    pub fn handle_command(&mut self, cmd: Command) -> bool {
        // Set WTM_DEBUG=1 to trace hotkey delivery.
        if std::env::var_os("WTM_DEBUG").is_some() {
            crate::logln!("wtm: command {cmd:?} (paused={})", self.paused);
        }
        match cmd {
            Command::Exit => return false,
            Command::ShowHelp | Command::Launcher | Command::Launch(_) => {} // handled by the bar
            Command::TogglePause => self.toggle_pause(),
            _ if self.paused => {}
            Command::FocusNext => self.focus_neighbor(1),
            Command::FocusPrev => self.focus_neighbor(-1),
            Command::FocusDir(dir) => self.focus_dir(dir),
            Command::SwapNext => self.swap_neighbor(1),
            Command::SwapPrev => self.swap_neighbor(-1),
            Command::SwapDir(dir) => self.swap_dir(dir),
            Command::Promote => self.promote(),
            Command::GrowWindow => self.resize_focused(0.05),
            Command::ShrinkWindow => self.resize_focused(-0.05),
            Command::ToggleFloat => self.toggle_float(),
            Command::ToggleMonocle => self.toggle_monocle(),
            Command::ToggleFullscreen => self.toggle_fullscreen(),
            Command::CloseWindow => {
                if let Some((w, _, _)) = self.focused_window() {
                    w.close();
                }
            }
            Command::SwitchWorkspace(n) => self.switch_workspace(n),
            Command::MoveToWorkspace(n) => self.move_to_workspace(n),
            Command::NextWorkspace => self.cycle_workspace(1),
            Command::PrevWorkspace => self.cycle_workspace(-1),
            Command::MoveNextWorkspace => self.move_cycle(1),
            Command::MovePrevWorkspace => self.move_cycle(-1),
            Command::PinApp => self.pin_app(),
            Command::Retile => {
                self.refresh_monitors();
                self.rescan();
                self.retile_all();
                self.update_borders();
            }
        }
        true
    }

    fn ordered_windows(&self, mi: usize) -> Vec<Window> {
        let mon = &self.monitors[mi];
        let ws = &mon.workspaces[mon.active];
        ws.all_windows().collect()
    }

    fn focus_neighbor(&mut self, dir: i32) {
        let mi = self.focused_monitor();
        let order = self.ordered_windows(mi);
        if order.is_empty() {
            return;
        }
        let cur = Window::foreground()
            .and_then(|f| order.iter().position(|x| *x == f))
            .unwrap_or(0) as i32;
        let next = (cur + dir).rem_euclid(order.len() as i32) as usize;
        order[next].focus();
        self.update_borders();
    }

    /// Score a candidate window for directional navigation: distance along
    /// the direction of travel, with sideways misalignment penalized so the
    /// visually "straight ahead" window wins. None if it's the wrong way.
    fn dir_score(from: &Window, to: &Window, dir: Dir) -> Option<i64> {
        Self::dir_score_rects(from.rect(), to.rect(), dir)
    }

    fn dir_score_rects(fr: Rect, tr: Rect, dir: Dir) -> Option<i64> {
        let (fx, fy) = (fr.x + fr.w / 2, fr.y + fr.h / 2);
        let (cx, cy) = (tr.x + tr.w / 2, tr.y + tr.h / 2);
        let (primary, ortho) = match dir {
            Dir::Left => (fx - cx, (cy - fy).abs()),
            Dir::Right => (cx - fx, (cy - fy).abs()),
            Dir::Up => (fy - cy, (cx - fx).abs()),
            Dir::Down => (cy - fy, (cx - fx).abs()),
        };
        (primary > 0).then(|| primary as i64 + 3 * ortho as i64)
    }

    /// Hyprland-style directional focus. Candidates come from every
    /// monitor's *active* workspace, so Alt+Right at the right edge of one
    /// screen lands on the next monitor.
    fn focus_dir(&mut self, dir: Dir) {
        let focused = Window::foreground().filter(|w| self.is_managed(*w));
        let Some(f) = focused else {
            let mi = self.focused_monitor();
            if let Some(w) = self.ordered_windows(mi).first() {
                w.focus();
                self.update_borders();
            }
            return;
        };
        let best = self
            .monitors
            .iter()
            .flat_map(|mon| mon.workspaces[mon.active].all_windows())
            .filter(|w| *w != f)
            .filter_map(|w| Self::dir_score(&f, &w, dir).map(|s| (s, w)))
            .min_by_key(|(s, _)| *s);
        if let Some((_, w)) = best {
            w.focus();
            self.update_borders();
        }
    }

    /// Swap the focused window with its geometric neighbor. With no neighbor
    /// in that direction, push the window to the adjacent monitor instead
    /// (Hyprland's movewindow-across-monitors behavior).
    fn swap_dir(&mut self, dir: Dir) {
        let Some((f, mi, wi)) = self.focused_window() else { return };
        let ws = &self.monitors[mi].workspaces[wi];
        if let Some(src) = ws.tiled.iter().position(|x| *x == f) {
            let best = ws
                .tiled
                .iter()
                .enumerate()
                .filter(|(_, w)| **w != f)
                .filter_map(|(i, w)| Self::dir_score(&f, w, dir).map(|s| (s, i)))
                .min_by_key(|(s, _)| *s);
            if let Some((_, dst)) = best {
                self.monitors[mi].workspaces[wi].tiled.swap(src, dst);
                self.retile_monitor(mi);
                return;
            }
        } else if !ws.floating.contains(&f) {
            return;
        }
        if let Some(tmi) = self.monitor_in_dir(mi, dir) {
            self.move_window_to_monitor(f, mi, wi, tmi);
        }
    }

    fn monitor_in_dir(&self, mi: usize, dir: Dir) -> Option<usize> {
        let from = self.monitors[mi].work_area;
        self.monitors
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != mi)
            .filter_map(|(i, m)| Self::dir_score_rects(from, m.work_area, dir).map(|s| (s, i)))
            .min_by_key(|(s, _)| *s)
            .map(|(_, i)| i)
    }

    fn move_window_to_monitor(&mut self, w: Window, mi: usize, wi: usize, tmi: usize) {
        let was_floating = self.monitors[mi].workspaces[wi].floating.contains(&w);
        self.monitors[mi].workspaces[wi].remove(w);
        let target_ws = self.monitors[tmi].active;
        if was_floating {
            self.monitors[tmi].workspaces[target_ws].floating.push(w);
            let wa = self.monitors[tmi].work_area;
            let (fw, fh) = (wa.w * 3 / 5, wa.h * 3 / 5);
            let centered =
                Rect { x: wa.x + (wa.w - fw) / 2, y: wa.y + (wa.h - fh) / 2, w: fw, h: fh };
            crate::animate::set_target(w, centered, self.config.animation_ms);
        } else {
            self.monitors[tmi].workspaces[target_ws].tiled.push(w);
        }
        self.retile_monitor(mi);
        self.retile_monitor(tmi);
        w.focus();
        self.update_borders();
    }

    fn swap_neighbor(&mut self, dir: i32) {
        let Some((w, mi, wi)) = self.focused_window() else { return };
        let ws = &mut self.monitors[mi].workspaces[wi];
        let Some(cur) = ws.tiled.iter().position(|x| *x == w) else { return };
        if ws.tiled.len() < 2 {
            return;
        }
        let next = (cur as i32 + dir).rem_euclid(ws.tiled.len() as i32) as usize;
        ws.tiled.swap(cur, next);
        self.retile_monitor(mi);
    }

    /// Move the focused window into the master (largest) slot.
    fn promote(&mut self) {
        let Some((w, mi, wi)) = self.focused_window() else { return };
        let ws = &mut self.monitors[mi].workspaces[wi];
        if let Some(cur) = ws.tiled.iter().position(|x| *x == w) {
            let win = ws.tiled.remove(cur);
            ws.tiled.insert(0, win);
            self.retile_monitor(mi);
        }
    }

    /// Grow/shrink the focused window. For a tiled window this adjusts the
    /// divider that gave the window its share of space (each split in the
    /// spiral has its own ratio). Floating windows scale about their center.
    fn resize_focused(&mut self, delta: f32) {
        let Some((w, mi, wi)) = self.focused_window() else { return };
        let ws = &mut self.monitors[mi].workspaces[wi];
        if let Some(i) = ws.tiled.iter().position(|x| *x == w) {
            let n = ws.tiled.len();
            if n < 2 {
                return;
            }
            if ws.ratios.len() < n - 1 {
                ws.ratios.resize(n - 1, self.config.split_ratio);
            }
            // Window i owns split i (it keeps `ratio` of the space there);
            // the last window is the leftover of split i-1, so invert.
            let (idx, d) = if i + 1 < n { (i, delta) } else { (i - 1, -delta) };
            ws.ratios[idx] = (ws.ratios[idx] + d).clamp(0.15, 0.85);
            self.retile_monitor(mi);
        } else if ws.floating.contains(&w) {
            let r = w.visible_rect();
            let f = if delta > 0.0 { 1.08 } else { 1.0 / 1.08 };
            let (nw, nh) = (((r.w as f32 * f) as i32).max(200), ((r.h as f32 * f) as i32).max(150));
            let target =
                Rect { x: r.x - (nw - r.w) / 2, y: r.y - (nh - r.h) / 2, w: nw, h: nh };
            crate::animate::set_target(w, target, self.config.animation_ms);
        }
    }

    fn toggle_float(&mut self) {
        let Some((w, mi, wi)) = self.focused_window() else { return };
        let ws = &mut self.monitors[mi].workspaces[wi];
        if let Some(cur) = ws.tiled.iter().position(|x| *x == w) {
            ws.tiled.remove(cur);
            ws.floating.push(w);
            // Center it at ~60% of the work area so it visibly pops out.
            let wa = self.monitors[mi].work_area;
            let (fw, fh) = (wa.w * 3 / 5, wa.h * 3 / 5);
            let centered =
                Rect { x: wa.x + (wa.w - fw) / 2, y: wa.y + (wa.h - fh) / 2, w: fw, h: fh };
            crate::animate::set_target(w, centered, self.config.animation_ms);
        } else if let Some(cur) = ws.floating.iter().position(|x| *x == w) {
            ws.floating.remove(cur);
            ws.tiled.push(w);
        }
        self.retile_monitor(mi);
    }

    /// Fullscreen covers the entire monitor, bar included. The window is
    /// lifted into the topmost band so it beats the (also topmost) bar.
    fn toggle_fullscreen(&mut self) {
        let Some((w, mi, wi)) = self.focused_window() else { return };
        let ws = &mut self.monitors[mi].workspaces[wi];
        if ws.fullscreen == Some(w) {
            ws.fullscreen = None;
            w.set_topmost(false);
        } else {
            if let Some(old) = ws.fullscreen {
                old.set_topmost(false);
            }
            ws.fullscreen = Some(w);
            w.set_topmost(true);
        }
        self.retile_monitor(mi);
        self.update_borders();
    }

    fn toggle_monocle(&mut self) {
        let mi = self.focused_monitor();
        let mon = &mut self.monitors[mi];
        let ws = &mut mon.workspaces[mon.active];
        ws.monocle = !ws.monocle;
        self.retile_monitor(mi);
    }

    fn switch_workspace(&mut self, target: usize) {
        let mi = self.focused_monitor();
        self.switch_workspace_on(mi, target);
    }

    /// Step to the next/previous workspace on the focused monitor, wrapping.
    fn cycle_workspace(&mut self, dir: i32) {
        let mi = self.focused_monitor();
        let count = self.monitors[mi].workspaces.len() as i32;
        let target = (self.monitors[mi].active as i32 + dir).rem_euclid(count) as usize;
        self.switch_workspace_on(mi, target);
    }

    /// Public so bar clicks can switch a specific monitor's workspace.
    pub fn switch_workspace_on(&mut self, mi: usize, target: usize) {
        if self.paused || mi >= self.monitors.len() {
            return;
        }
        let mon = &mut self.monitors[mi];
        if target >= mon.workspaces.len() || target == mon.active {
            return;
        }
        for w in mon.workspaces[mon.active].all_windows().collect::<Vec<_>>() {
            self.hidden_by_us.insert(w.0);
            w.hide();
        }
        let mon = &mut self.monitors[mi];
        mon.active = target;
        let to_show: Vec<Window> = mon.workspaces[target].all_windows().collect();
        for w in &to_show {
            self.hidden_by_us.remove(&w.0);
            w.show();
        }
        self.persist_hidden();
        self.retile_monitor(mi);
        if let Some(first) = self.monitors[mi].workspaces[target].all_windows().next() {
            first.focus();
        }
        self.update_borders();
    }

    fn move_to_workspace(&mut self, target: usize) {
        let Some((w, mi, wi)) = self.focused_window() else { return };
        let mon = &mut self.monitors[mi];
        if target >= mon.workspaces.len() || target == wi {
            return;
        }
        let was_tiled = mon.workspaces[wi].tiled.contains(&w);
        mon.workspaces[wi].remove(w);
        if was_tiled {
            mon.workspaces[target].tiled.push(w);
        } else {
            mon.workspaces[target].floating.push(w);
        }
        if target != mon.active {
            if self.config.follow_moved_window {
                // Bring the workspace to us instead of hiding the window.
                self.switch_workspace_on(mi, target);
                w.focus();
            } else {
                self.hidden_by_us.insert(w.0);
                w.hide();
                self.persist_hidden();
            }
        }
        self.retile_monitor(mi);
        self.update_borders();
    }

    /// Toggle an app rule: "windows of this exe open on this workspace".
    /// Pressed on an already-pinned combination, it removes the rule.
    fn pin_app(&mut self) {
        let Some((w, _mi, wi)) = self.focused_window() else { return };
        let Some(exe) = w.exe() else { return };
        let exe = exe.to_lowercase();
        let n = wi + 1;
        if self.config.app_rules.get(&exe) == Some(&n) {
            self.config.app_rules.remove(&exe);
            if let Err(e) = crate::config::save_app_rule(&exe, None) {
                crate::logln!("wtm: could not save config: {e}");
            }
            crate::logln!("wtm: unpinned {exe}");
        } else {
            self.config.app_rules.insert(exe.clone(), n);
            if let Err(e) = crate::config::save_app_rule(&exe, Some(n)) {
                crate::logln!("wtm: could not save config: {e}");
            }
            crate::logln!("wtm: pinned {exe} -> workspace {n}");
        }
    }

    /// Send a window to a specific monitor's workspace (bar-cell drop).
    fn send_window_to(&mut self, w: Window, mon_handle: isize, target: usize) {
        let Some((mi, wi)) = self.find(w) else { return };
        let tmi = self.monitor_index_of_handle(mon_handle);
        if target >= self.monitors[tmi].workspaces.len() || (tmi == mi && target == wi) {
            self.retile_monitor(mi); // dropped on its own cell: just snap back
            return;
        }
        let was_floating = self.monitors[mi].workspaces[wi].floating.contains(&w);
        self.monitors[mi].workspaces[wi].remove(w);
        let ws = &mut self.monitors[tmi].workspaces[target];
        if was_floating {
            ws.floating.push(w);
        } else {
            ws.tiled.push(w);
        }
        crate::logln!("wtm: sent \"{}\" to workspace {}", w.title(), target + 1);
        if self.config.follow_moved_window {
            self.switch_workspace_on(tmi, target);
            w.focus();
        } else if target != self.monitors[tmi].active {
            self.hidden_by_us.insert(w.0);
            w.hide();
            self.persist_hidden();
        }
        self.retile_monitor(mi);
        self.retile_monitor(tmi);
        self.update_borders();
    }

    /// Carry the focused window to the next/previous workspace (wrapping).
    fn move_cycle(&mut self, dir: i32) {
        let Some((_, mi, wi)) = self.focused_window() else { return };
        let count = self.monitors[mi].workspaces.len() as i32;
        let target = (wi as i32 + dir).rem_euclid(count) as usize;
        self.move_to_workspace(target);
    }

    fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        if self.paused {
            for mon in &self.monitors {
                for ws in &mon.workspaces {
                    for w in ws.all_windows() {
                        w.set_border_color(None);
                    }
                }
            }
            crate::logln!("wtm: paused — windows are free-floating (Alt+P to resume)");
        } else {
            self.rescan();
            self.retile_all();
            self.update_borders();
            crate::logln!("wtm: resumed");
        }
    }

    /// Pick up windows that appeared while we were paused or that we missed.
    fn rescan(&mut self) {
        for w in crate::enum_top_level_windows() {
            if !self.is_managed(w) && w.is_manageable(&self.config) {
                let mi = self.monitor_index_of_handle(w.monitor());
                let mon = &mut self.monitors[mi];
                let ws = &mut mon.workspaces[mon.active];
                if w.should_float(&self.config) {
                    ws.floating.push(w);
                } else {
                    ws.tiled.push(w);
                }
            }
        }
    }

    /// Where the thick focus-border frame should sit — None hides it
    /// (nothing focused, paused, fullscreen, or window not on screen).
    pub fn border_target(&self) -> Option<Rect> {
        if self.paused {
            return None;
        }
        let w = Window::foreground()?;
        let (mi, wi) = self.find(w)?;
        if wi != self.monitors[mi].active || !w.is_visible() || w.is_minimized() {
            return None;
        }
        if self.monitors[mi].workspaces[wi].fullscreen == Some(w) {
            return None;
        }
        Some(w.visible_rect())
    }

    /// Everything the status bar needs to render one monitor.
    pub fn bar_snapshot(&self, monitor_handle: isize) -> Option<BarSnapshot> {
        let mon = self.monitors.iter().find(|m| m.handle == monitor_handle)?;
        let occupied: Vec<bool> = mon
            .workspaces
            .iter()
            .map(|ws| !ws.tiled.is_empty() || !ws.floating.is_empty())
            .collect();
        let title = Window::foreground()
            .filter(|w| self.is_managed(*w))
            .map(|w| w.title())
            .unwrap_or_default();
        let names: Vec<String> = (0..mon.workspaces.len())
            .map(|i| self.config.workspace_names.get(i).cloned().unwrap_or_default())
            .collect();
        Some(BarSnapshot { active: mon.active, names, occupied, title, paused: self.paused })
    }

    /// Full reaction to a display-configuration change (docking, resolution,
    /// monitor added/removed). Returns true when the monitor topology
    /// actually changed — the caller then rebuilds the bars.
    pub fn handle_display_change(&mut self) -> bool {
        let fresh = monitor::enumerate();
        let changed = fresh.len() != self.monitors.len()
            || fresh.iter().any(|f| {
                !self.monitors.iter().any(|m| m.handle == f.handle && m.bounds == f.bounds)
            });
        if !changed {
            // Same screens; only the work area moved (taskbar, appbars).
            self.refresh_monitors();
            self.retile_all();
            self.update_borders();
            return false;
        }
        // Rebuild the monitor list, carrying workspaces over by handle.
        let mut new_monitors: Vec<MonitorState> = fresh
            .iter()
            .map(|f| MonitorState {
                handle: f.handle,
                bounds: f.bounds,
                work_area: f.work_area,
                workspaces: (0..self.config.workspaces).map(|_| Workspace::default()).collect(),
                active: 0,
            })
            .collect();
        let old = std::mem::take(&mut self.monitors);
        let mut orphans = Vec::new();
        for om in old {
            match new_monitors.iter_mut().find(|n| n.handle == om.handle) {
                Some(nm) => {
                    nm.workspaces = om.workspaces;
                    nm.active = om.active.min(self.config.workspaces - 1);
                }
                None => orphans.push(om),
            }
        }
        // A monitor disappeared: its windows merge into the first monitor,
        // workspace i -> workspace i.
        for om in orphans {
            for (i, ws) in om.workspaces.into_iter().enumerate() {
                if let Some(fw) = ws.fullscreen {
                    fw.set_topmost(false);
                }
                if let Some(target) = new_monitors.get_mut(0) {
                    let idx = i.min(target.workspaces.len() - 1);
                    target.workspaces[idx].tiled.extend(ws.tiled);
                    target.workspaces[idx].floating.extend(ws.floating);
                }
            }
        }
        self.monitors = new_monitors;
        self.sync_visibility();
        self.retile_all();
        self.update_borders();
        crate::logln!("wtm: display layout changed — {} monitor(s)", self.monitors.len());
        true
    }

    /// After monitors are reshuffled, make window visibility match reality:
    /// windows in active workspaces get shown, the rest hidden.
    fn sync_visibility(&mut self) {
        for mi in 0..self.monitors.len() {
            let active = self.monitors[mi].active;
            for wi in 0..self.monitors[mi].workspaces.len() {
                let windows: Vec<Window> =
                    self.monitors[mi].workspaces[wi].all_windows().collect();
                for w in windows {
                    if !w.is_valid() {
                        continue;
                    }
                    if wi == active {
                        if self.hidden_by_us.remove(&w.0) {
                            w.show();
                        }
                    } else if w.is_visible() {
                        self.hidden_by_us.insert(w.0);
                        w.hide();
                    }
                }
            }
        }
        self.persist_hidden();
    }

    /// Re-read monitor geometry (docking, resolution changes) while keeping
    /// windows in their workspaces. Extra monitors' windows collapse onto the
    /// first monitor if a monitor disappeared.
    pub fn refresh_monitors(&mut self) {
        let fresh = monitor::enumerate();
        for m in &mut self.monitors {
            if let Some(f) = fresh.iter().find(|f| f.handle == m.handle) {
                m.bounds = f.bounds;
                m.work_area = f.work_area;
            }
        }
    }

    // ------------------------------------------------------ crash insurance

    fn state_file() -> Option<PathBuf> {
        std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("wtm-hidden-windows.txt"))
    }

    /// Record which windows we've hidden. If wtm dies without cleanup, the
    /// next start un-hides them so the user never loses windows.
    fn persist_hidden(&self) {
        let Some(path) = Self::state_file() else { return };
        if self.hidden_by_us.is_empty() {
            let _ = std::fs::remove_file(path);
            return;
        }
        if let Ok(mut f) = std::fs::File::create(path) {
            for h in &self.hidden_by_us {
                let _ = writeln!(f, "{h}");
            }
        }
    }

    pub fn restore_orphans() {
        let Some(path) = Self::state_file() else { return };
        let Ok(text) = std::fs::read_to_string(&path) else { return };
        for line in text.lines() {
            if let Ok(raw) = line.trim().parse::<isize>() {
                let w = Window(raw);
                if w.is_valid() && !w.is_visible() {
                    w.show();
                }
            }
        }
        let _ = std::fs::remove_file(path);
    }

    /// Show everything we hid and drop all borders. Called on clean exit.
    pub fn cleanup(&mut self) {
        for mon in &self.monitors {
            for ws in &mon.workspaces {
                if let Some(fw) = ws.fullscreen {
                    fw.set_topmost(false);
                }
                for w in ws.all_windows() {
                    if self.hidden_by_us.contains(&w.0) {
                        w.show();
                    }
                    w.set_border_color(None);
                }
            }
        }
        self.hidden_by_us.clear();
        self.persist_hidden();
    }
}

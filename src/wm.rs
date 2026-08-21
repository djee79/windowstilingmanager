//! The window manager itself: monitors → workspaces → windows, plus the
//! reactions to system events and user hotkeys. Everything runs on the main
//! thread, so there is no locking anywhere.

use crate::config::{parse_colorref, Config};
use crate::layout::{dwindle, Rect};
use crate::monitor;
use crate::window::Window;
use std::collections::{HashMap, HashSet};
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
    Foreground(Window),
    /// Window title changed — late-titled windows get adopted here.
    Retitled(Window),
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
    Overview,
    WindowSwitcher,
    /// Show/hide the scratchpad windows as floating topmost overlays.
    ScratchToggle,
    /// Send the focused window to the scratchpad.
    ScratchSend,
    /// Focus the next scratchpad window (summons the scratchpad if hidden).
    ScratchCycle,
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

/// One row of the fuzzy window switcher.
pub struct SwitchEntry {
    pub window: Window,
    /// Where it lives: workspace name/number, monitor tag, or "scratchpad".
    pub place: String,
}

pub struct OverviewSnapshot {
    /// Coordinate space of the item rects (the monitor's work area).
    pub source: Rect,
    pub active: usize,
    pub names: Vec<String>,
    /// Per workspace: (window, rect it occupies in `source` space).
    pub workspaces: Vec<Vec<(Window, Rect)>>,
}

pub struct BarSnapshot {
    pub active: usize,
    /// One entry per workspace; empty string where unnamed.
    pub names: Vec<String>,
    pub occupied: Vec<bool>,
    /// Up to 5 windows per workspace, for the little cell icons.
    pub cell_windows: Vec<Vec<Window>>,
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
    /// The scratchpad: windows parked outside every workspace, toggled as
    /// floating topmost overlays (Hyprland's special workspace).
    scratch: Vec<Window>,
    scratch_shown: bool,
    /// Last size/position the user gave each scratchpad window, so a
    /// resized scratch terminal stays that size across toggles.
    scratch_rects: HashMap<isize, Rect>,
    /// Unmanaged owned windows (tool palettes, modeless dialogs) hidden along
    /// with their managed owner, keyed by the owner — shown again together.
    companions_hidden: HashMap<isize, Vec<Window>>,
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
        WindowManager {
            config,
            monitors,
            hidden_by_us: HashSet::new(),
            border_color,
            paused: false,
            scratch: Vec::new(),
            scratch_shown: false,
            scratch_rects: HashMap::new(),
            companions_hidden: HashMap::new(),
        }
    }

    /// Hide a managed window for a workspace change, taking its unmanaged
    /// owned windows along (an app's floating tool palettes and modeless
    /// dialogs stay glued to their owner instead of lingering on screen).
    fn hide_managed(&mut self, w: Window) {
        // Ask the app to leave ribbon/menu accelerator mode: KeyTips pop up
        // on Alt-down, before our hotkey even fires, and a hidden ribbon
        // never gets around to taking its badges down on its own.
        w.cancel_modes();
        let thread = w.thread_id();
        let mut companions: Vec<Window> = Vec::new();
        for c in crate::enum_top_level_windows() {
            if c == w || !c.is_visible() || self.is_managed(c) {
                continue;
            }
            if c.is_transient_popup() {
                // Overlay badges (key tips, tooltips) are often unowned, so
                // match them by thread — and hide them for good: the app
                // recreates them on demand, re-showing one paints a stale
                // artifact.
                if c.root_owner() == w || (thread != 0 && c.thread_id() == thread) {
                    c.hide();
                }
            } else if c.root_owner() == w {
                companions.push(c);
            }
        }
        for c in &companions {
            self.hidden_by_us.insert(c.0);
            c.hide();
        }
        if !companions.is_empty() {
            self.companions_hidden.insert(w.0, companions);
        }
        self.hidden_by_us.insert(w.0);
        w.hide();
    }

    /// Undo hide_managed: show the window and whatever we hid with it.
    fn show_managed(&mut self, w: Window) {
        self.hidden_by_us.remove(&w.0);
        w.show();
        if let Some(companions) = self.companions_hidden.remove(&w.0) {
            for c in companions {
                self.hidden_by_us.remove(&c.0);
                if c.is_valid() {
                    c.show();
                }
            }
        }
    }

    /// Show any companions still hidden for `w` (owner going away).
    fn release_companions(&mut self, w: Window) {
        if let Some(companions) = self.companions_hidden.remove(&w.0) {
            for c in companions {
                if self.hidden_by_us.remove(&c.0) && c.is_valid() {
                    c.show();
                }
            }
        }
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

    fn in_scratch(&self, w: Window) -> bool {
        self.scratch.contains(&w)
    }

    fn is_managed(&self, w: Window) -> bool {
        self.find(w).is_some() || self.in_scratch(w)
    }

    /// Should focus-follows-mouse activate this window when hovered?
    pub fn hover_focus_target(&self, w: Window) -> bool {
        !self.paused && self.is_managed(w)
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
            // Most-specific rule wins: AppUserModelID (per web app), then
            // exe name so plain "brave.exe"-style rules still match.
            let rule = w
                .rule_key()
                .and_then(|k| self.config.app_rules.get(&k))
                .or_else(|| {
                    w.exe().and_then(|e| self.config.app_rules.get(&e.to_lowercase()))
                });
            if let Some(&n) = rule {
                if n >= 1 && n <= self.monitors[mi].workspaces.len() {
                    target = n - 1;
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
            self.hide_managed(w);
            self.persist_hidden();
        } else {
            self.retile_monitor(mi);
        }
        self.update_borders();
    }

    fn unmanage(&mut self, w: Window) {
        self.scratch.retain(|x| *x != w);
        self.scratch_rects.remove(&w.0);
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
        self.release_companions(w);
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
        // Smart gaps: a lone window fills the work area edge to edge.
        let solo = self.config.smart_gaps
            && ws.fullscreen.is_none()
            && ws.floating.is_empty()
            && ws.tiled.len() == 1;
        let area = if solo { mon.work_area } else { mon.work_area.shrink(outer) };
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
        self.raise_floats(mi);
    }

    /// Keep floating windows (dialogs, palettes) above the tiled layer,
    /// Hyprland-style — a delete-confirmation must never sink behind the
    /// window that spawned it.
    fn raise_floats(&self, mi: usize) {
        let mon = &self.monitors[mi];
        for w in &mon.workspaces[mon.active].floating {
            if w.is_visible() && !w.is_minimized() {
                w.raise();
            }
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
        for w in &self.scratch {
            if w.is_visible() {
                w.set_border_color((Some(*w) == focused).then_some(self.border_color));
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
            WmEvent::Foreground(w) => {
                // Second chance for windows we missed at SHOW time (e.g. the
                // title arrived late): adopt them when they take focus.
                if !self.is_managed(w) && w.is_manageable(&self.config) {
                    self.manage(w, true);
                }
                // Focusing a tiled window raises it — put the floating layer
                // (dialogs) back on top so they stay visible and clickable.
                if let Some((mi, wi)) = self.find(w) {
                    if wi == self.monitors[mi].active
                        && self.monitors[mi].workspaces[wi].tiled.contains(&w)
                    {
                        self.raise_floats(mi);
                    }
                }
                self.update_borders();
            }
            WmEvent::Retitled(w) => {
                if !self.is_managed(w) && w.is_manageable(&self.config) {
                    self.manage(w, true);
                }
            }
            WmEvent::MoveSizeEnd(w, drop) => self.on_drag_end(w, drop),
        }
    }

    /// After a mouse drag: dropped on a bar workspace cell, the window is
    /// sent to that workspace; resized by its edges, the layout's split
    /// ratios follow the drag; dropped onto another tiled window, they swap
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
            if self.try_drag_resize(mi, wi, w) {
                self.retile_monitor(mi);
                return;
            }
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

    /// If the drag changed the window's *size* (edge/corner resize rather
    /// than a move), fold the new edges back into the dwindle ratios so the
    /// splits follow the mouse. Returns true when a resize was applied.
    fn try_drag_resize(&mut self, mi: usize, wi: usize, w: Window) -> bool {
        let (outer, inner, split) =
            (self.config.outer_gap, self.config.inner_gap, self.config.split_ratio);
        let work_area = self.monitors[mi].work_area;
        let ws = &mut self.monitors[mi].workspaces[wi];
        let n = ws.tiled.len();
        if ws.monocle || n < 2 {
            return false;
        }
        if ws.ratios.len() < n - 1 {
            ws.ratios.resize(n - 1, split);
        }
        let area = work_area.shrink(outer);
        let Some(idx) = ws.tiled.iter().position(|x| *x == w) else { return false };
        let expected = crate::layout::dwindle(area, n, &ws.ratios, inner)[idx];
        let actual = w.visible_rect();
        if (actual.w - expected.w).abs() <= 10 && (actual.h - expected.h).abs() <= 10 {
            return false; // pure move: let the swap logic handle it
        }
        crate::layout::resize_ratios(area, n, &mut ws.ratios, inner, idx, actual);
        true
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
            // Handled by the bar / overview windows, not the manager.
            Command::ShowHelp
            | Command::Launcher
            | Command::Launch(_)
            | Command::Overview
            | Command::WindowSwitcher => {}
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
            Command::ScratchToggle => self.scratch_toggle(),
            Command::ScratchSend => self.scratch_send(),
            Command::ScratchCycle => self.scratch_cycle(),
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
        // Scratchpad windows scale about their center, like floating ones.
        if let Some(w) = Window::foreground() {
            if self.in_scratch(w) {
                self.scale_about_center(w, delta);
                return;
            }
        }
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
            self.scale_about_center(w, delta);
        }
    }

    fn scale_about_center(&self, w: Window, delta: f32) {
        let r = w.visible_rect();
        let f = if delta > 0.0 { 1.08 } else { 1.0 / 1.08 };
        let (nw, nh) = (((r.w as f32 * f) as i32).max(200), ((r.h as f32 * f) as i32).max(150));
        let target = Rect { x: r.x - (nw - r.w) / 2, y: r.y - (nh - r.h) / 2, w: nw, h: nh };
        crate::animate::set_target(w, target, self.config.animation_ms);
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
        if let Some(w) = Window::foreground() {
            if self.in_scratch(w) {
                self.scratch_toggle_fullscreen(w);
                return;
            }
        }
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
            self.hide_managed(w);
        }
        let mon = &mut self.monitors[mi];
        mon.active = target;
        let to_show: Vec<Window> = mon.workspaces[target].all_windows().collect();
        for w in &to_show {
            self.show_managed(*w);
        }
        self.persist_hidden();
        self.retile_monitor(mi);
        if let Some(first) = self.monitors[mi].workspaces[target].all_windows().next() {
            first.focus();
        } else if Window::foreground().is_some_and(|f| self.hidden_by_us.contains(&f.0)) {
            // Empty workspace: don't leave keyboard focus on the window we
            // just hid — keystrokes (and the next hotkey's modifier mask)
            // would keep flowing to an invisible app.
            Window::focus_shell();
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
                self.hide_managed(w);
                self.persist_hidden();
            }
        }
        self.retile_monitor(mi);
        self.update_borders();
    }

    /// Live-change the number of workspaces per monitor (appearance panel).
    /// Shrinking merges the removed workspaces' windows into the last
    /// remaining one. Returns true when the change was applied.
    pub fn set_workspace_count(&mut self, n: usize) -> bool {
        let n = n.clamp(1, 24);
        if self.paused || n == self.config.workspaces {
            return false;
        }
        // Bring every monitor's active workspace into range first, so the
        // usual switch path un-hides windows before their workspace is cut.
        for mi in 0..self.monitors.len() {
            if self.monitors[mi].active >= n {
                self.switch_workspace_on(mi, n - 1);
            }
        }
        for mon in &mut self.monitors {
            if mon.workspaces.len() < n {
                mon.workspaces.resize_with(n, Workspace::default);
            } else {
                let removed: Vec<Workspace> = mon.workspaces.drain(n..).collect();
                for ws in removed {
                    if let Some(fw) = ws.fullscreen {
                        fw.set_topmost(false);
                    }
                    mon.workspaces[n - 1].tiled.extend(ws.tiled);
                    mon.workspaces[n - 1].floating.extend(ws.floating);
                }
            }
        }
        self.config.workspaces = n;
        self.sync_visibility();
        self.retile_all();
        self.update_borders();
        crate::logln!("wtm: workspace count set to {n}");
        true
    }

    /// Live-change the accent color: config, thin DWM borders. The thick
    /// frame and the bar pick it up through their own set_* calls.
    pub fn set_accent_color(&mut self, hex: &str) {
        self.config.active_border_color = hex.to_string();
        self.border_color = parse_colorref(hex);
        self.update_borders();
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Swap in a freshly loaded config (hot-reload). Workspace count changes
    /// go through the normal merge path; everything else applies directly.
    pub fn apply_config(&mut self, mut cfg: Config) {
        let n = cfg.workspaces;
        cfg.workspaces = self.config.workspaces;
        self.config = cfg;
        self.border_color = parse_colorref(&self.config.active_border_color);
        self.set_workspace_count(n);
        self.retile_all();
        self.update_borders();
    }

    /// Park the focused window in the scratchpad: out of every workspace,
    /// hidden until the scratchpad is toggled up. Pressed on a window that
    /// is already in the scratchpad, it pulls it back out instead.
    fn scratch_send(&mut self) {
        if let Some(w) = Window::foreground() {
            if self.in_scratch(w) {
                self.scratch_pull(w);
                return;
            }
        }
        let Some((w, mi, wi)) = self.focused_window() else { return };
        if self.monitors[mi].workspaces[wi].fullscreen == Some(w) {
            self.monitors[mi].workspaces[wi].fullscreen = None;
            w.set_topmost(false);
        }
        self.monitors[mi].workspaces[wi].remove(w);
        self.scratch.push(w);
        self.hide_managed(w);
        self.persist_hidden();
        self.retile_monitor(mi);
        self.update_borders();
        crate::logln!("wtm: \"{}\" sent to the scratchpad (same key pulls it back out)", w.title());
    }

    /// Return a scratchpad window to the focused monitor's active workspace.
    fn scratch_pull(&mut self, w: Window) {
        self.scratch.retain(|x| *x != w);
        self.scratch_rects.remove(&w.0);
        w.set_topmost(false);
        if !w.is_visible() {
            self.show_managed(w);
        } else {
            self.hidden_by_us.remove(&w.0);
        }
        let mi = self.focused_monitor();
        let mon = &mut self.monitors[mi];
        let ws = &mut mon.workspaces[mon.active];
        if w.should_float(&self.config) {
            ws.floating.push(w);
        } else {
            ws.tiled.push(w);
        }
        if self.scratch.is_empty() {
            self.scratch_shown = false;
        }
        self.persist_hidden();
        self.retile_monitor(mi);
        w.focus();
        self.update_borders();
        crate::logln!("wtm: \"{}\" pulled out of the scratchpad", w.title());
    }

    /// Toggle the scratchpad: show every parked window as a floating,
    /// topmost cascade on the focused monitor, or tuck them all away again.
    fn scratch_toggle(&mut self) {
        self.scratch.retain(|w| w.is_valid());
        if self.scratch.is_empty() {
            self.scratch_shown = false;
            crate::logln!("wtm: scratchpad is empty — send a window with the send_scratchpad key");
            return;
        }
        if self.scratch_shown {
            for w in self.scratch.clone() {
                if w.is_visible() {
                    // Remember the size the user left it at.
                    self.scratch_rects.insert(w.0, w.visible_rect());
                    w.set_topmost(false);
                    self.hide_managed(w);
                }
            }
            self.scratch_shown = false;
            self.persist_hidden();
            let mi = self.focused_monitor();
            if let Some(first) = self.ordered_windows(mi).first() {
                first.focus();
            }
        } else {
            let mi = self.focused_monitor();
            let wa = self.monitors[mi].work_area;
            let list = self.scratch.clone();
            for (i, w) in list.iter().enumerate() {
                self.show_managed(*w);
                let off = (i as i32) * 40;
                // Keep each window's remembered size; center it (cascaded)
                // on whatever monitor the user is on right now.
                let (tw, th) = self
                    .scratch_rects
                    .get(&w.0)
                    .map(|r| (r.w.min(wa.w), r.h.min(wa.h)))
                    .unwrap_or((wa.w * 3 / 5, wa.h * 3 / 5));
                let target = Rect {
                    x: wa.x + (wa.w - tw) / 2 + off,
                    y: wa.y + (wa.h - th) / 2 + off,
                    w: tw,
                    h: th,
                };
                w.set_topmost(true);
                crate::animate::set_target(*w, target, self.config.animation_ms);
            }
            self.scratch_shown = true;
            self.persist_hidden();
            if let Some(last) = list.last() {
                last.focus();
            }
        }
        self.update_borders();
    }

    /// Cycle keyboard focus through the scratchpad windows, summoning the
    /// scratchpad first if it's hidden.
    fn scratch_cycle(&mut self) {
        self.scratch.retain(|w| w.is_valid());
        if self.scratch.is_empty() {
            crate::logln!("wtm: scratchpad is empty — send a window with the send_scratchpad key");
            return;
        }
        if !self.scratch_shown {
            self.scratch_toggle();
            return;
        }
        let cur = Window::foreground().and_then(|f| self.scratch.iter().position(|x| *x == f));
        let next = cur.map(|i| (i + 1) % self.scratch.len()).unwrap_or(0);
        let w = self.scratch[next];
        if !w.is_visible() {
            self.show_managed(w);
            w.set_topmost(true);
        }
        w.focus();
        self.update_borders();
    }

    /// Fullscreen for a scratchpad window: bounce between the whole monitor
    /// (it's already topmost, so it covers the bar) and its remembered size.
    fn scratch_toggle_fullscreen(&mut self, w: Window) {
        let mi = self.monitor_index_of_handle(w.monitor());
        let bounds = self.monitors[mi].bounds;
        let wa = self.monitors[mi].work_area;
        let cur = w.visible_rect();
        let anim = self.config.animation_ms;
        let is_fs = (cur.w - bounds.w).abs() < 60 && (cur.h - bounds.h).abs() < 60;
        if is_fs {
            let (tw, th) = self
                .scratch_rects
                .get(&w.0)
                .map(|r| (r.w.min(wa.w), r.h.min(wa.h)))
                // A remembered fullscreen-sized rect is no restore target.
                .filter(|&(tw, th)| (tw - bounds.w).abs() >= 60 || (th - bounds.h).abs() >= 60)
                .unwrap_or((wa.w * 3 / 5, wa.h * 3 / 5));
            let target =
                Rect { x: wa.x + (wa.w - tw) / 2, y: wa.y + (wa.h - th) / 2, w: tw, h: th };
            crate::animate::set_target(w, target, anim);
        } else {
            self.scratch_rects.insert(w.0, cur);
            crate::animate::set_target(w, bounds, anim);
        }
        self.update_borders();
    }

    /// Toggle an app rule: "windows of this app open on this workspace".
    /// Keyed by AppUserModelID when the window has one (so each browser web
    /// app is its own pin), else by exe name. Pressed on an already-pinned
    /// combination, it removes the rule.
    fn pin_app(&mut self) {
        let Some((w, _mi, wi)) = self.focused_window() else { return };
        let Some(key) = w.rule_key() else { return };
        let n = wi + 1;
        if self.config.app_rules.get(&key) == Some(&n) {
            self.config.app_rules.remove(&key);
            if let Err(e) = crate::config::save_app_rule(&key, None) {
                crate::logln!("wtm: could not save config: {e}");
            }
            crate::logln!("wtm: unpinned {key}");
        } else {
            self.config.app_rules.insert(key.clone(), n);
            if let Err(e) = crate::config::save_app_rule(&key, Some(n)) {
                crate::logln!("wtm: could not save config: {e}");
            }
            crate::logln!("wtm: pinned {key} -> workspace {n}");
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
            self.hide_managed(w);
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
        if self.in_scratch(w) {
            return (w.is_visible() && !w.is_minimized()).then(|| w.visible_rect());
        }
        let (mi, wi) = self.find(w)?;
        if wi != self.monitors[mi].active || !w.is_visible() || w.is_minimized() {
            return None;
        }
        if self.monitors[mi].workspaces[wi].fullscreen == Some(w) {
            return None;
        }
        // Smart gaps: no frame either — the lone window IS the workspace.
        let ws = &self.monitors[mi].workspaces[wi];
        if self.config.smart_gaps
            && ws.fullscreen.is_none()
            && ws.floating.is_empty()
            && ws.tiled.len() == 1
            && ws.tiled[0] == w
        {
            return None;
        }
        Some(w.visible_rect())
    }

    /// Every managed window with a human-readable location tag, for the
    /// fuzzy window switcher.
    pub fn window_list(&self) -> Vec<SwitchEntry> {
        let multi = self.monitors.len() > 1;
        let mut out = Vec::new();
        for (mi, mon) in self.monitors.iter().enumerate() {
            for (wi, ws) in mon.workspaces.iter().enumerate() {
                for w in ws.all_windows() {
                    if !w.is_valid() {
                        continue;
                    }
                    let name = self
                        .config
                        .workspace_names
                        .get(wi)
                        .filter(|n| !n.is_empty())
                        .cloned()
                        .unwrap_or_else(|| format!("ws {}", wi + 1));
                    let place = if multi { format!("M{} · {}", mi + 1, name) } else { name };
                    out.push(SwitchEntry { window: w, place });
                }
            }
        }
        for w in &self.scratch {
            if w.is_valid() {
                out.push(SwitchEntry { window: *w, place: "scratchpad".to_string() });
            }
        }
        out
    }

    /// Jump to a window wherever it lives: switch its workspace in if
    /// needed (or pop it out of the scratchpad), then focus it.
    pub fn activate_window(&mut self, w: Window) {
        if let Some((mi, wi)) = self.find(w) {
            if wi != self.monitors[mi].active {
                self.switch_workspace_on(mi, wi);
            }
            w.focus();
        } else if self.in_scratch(w) {
            self.show_managed(w);
            w.set_topmost(true);
            self.scratch_shown = true;
            self.persist_hidden();
            w.focus();
        }
        self.update_borders();
    }

    /// Public alias for the overview: which monitor is the user working on.
    pub fn focused_monitor_index(&self) -> usize {
        self.focused_monitor()
    }

    /// Everything the workspace overview needs for one monitor: each
    /// workspace's windows with the rects they occupy (or would occupy) in
    /// `source` coordinates, so cards can be drawn as faithful miniatures.
    pub fn overview_snapshot(&self, monitor_handle: isize) -> Option<OverviewSnapshot> {
        let mi = self.monitor_index_of_handle(monitor_handle);
        let mon = self.monitors.get(mi)?;
        let area = mon.work_area.shrink(self.config.outer_gap);
        let mut workspaces = Vec::with_capacity(mon.workspaces.len());
        for ws in &mon.workspaces {
            let mut items: Vec<(Window, Rect)> = Vec::new();
            if ws.monocle {
                // Monocle: show the stack as a cascade so every window peeks out.
                for (i, w) in ws.tiled.iter().enumerate() {
                    let off = (i as i32) * 16;
                    items.push((
                        *w,
                        Rect {
                            x: area.x + off,
                            y: area.y + off,
                            w: (area.w - 2 * off).max(60),
                            h: (area.h - 2 * off).max(60),
                        },
                    ));
                }
            } else {
                let rects = dwindle(area, ws.tiled.len(), &ws.ratios, self.config.inner_gap);
                items.extend(ws.tiled.iter().copied().zip(rects));
            }
            for f in &ws.floating {
                items.push((*f, f.visible_rect()));
            }
            workspaces.push(items);
        }
        let names = (0..mon.workspaces.len())
            .map(|i| self.config.workspace_names.get(i).cloned().unwrap_or_default())
            .collect();
        Some(OverviewSnapshot { source: mon.work_area, active: mon.active, names, workspaces })
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
        let cell_windows: Vec<Vec<Window>> = mon
            .workspaces
            .iter()
            .map(|ws| ws.all_windows().filter(|w| w.is_valid()).take(5).collect())
            .collect();
        Some(BarSnapshot { active: mon.active, names, occupied, cell_windows, title, paused: self.paused })
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
                        if self.hidden_by_us.contains(&w.0) {
                            self.show_managed(w);
                        }
                    } else if w.is_visible() {
                        self.hide_managed(w);
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
        for w in &self.scratch {
            if self.hidden_by_us.contains(&w.0) {
                w.show();
            }
            w.set_topmost(false);
            w.set_border_color(None);
        }
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
        for (_, companions) in self.companions_hidden.drain() {
            for c in companions {
                if c.is_valid() {
                    c.show();
                }
            }
        }
        self.hidden_by_us.clear();
        self.persist_hidden();
    }
}

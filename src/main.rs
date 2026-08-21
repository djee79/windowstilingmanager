//! wtm — a Hyprland-inspired tiling window manager for Windows 11.
//!
//! Single-threaded by design: WinEvent hooks (WINEVENT_OUTOFCONTEXT) and
//! WM_HOTKEY messages are both delivered through this thread's message loop,
//! so the WindowManager lives in a thread_local and needs no locks.

// Release builds run as a background app: no console window; diagnostics go
// to %LOCALAPPDATA%\wtm\wtm.log. Debug builds keep the console.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod animate;
mod bar;
mod border;
mod config;
mod keys;
mod layout;
mod logger;
mod monitor;
mod mouse;
mod overview;
mod tray;
mod window;
mod wm;

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use window::Window;
use windows::core::{w, BOOL};
use windows::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, TRUE, WPARAM,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentThreadId, Sleep};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, RegisterHotKey, SendInput, UnregisterHotKey, INPUT, INPUT_0, INPUT_KEYBOARD,
    KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_LWIN, VK_MENU, VK_RWIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, EnumWindows, GetCursorPos, GetMessageW,
    KillTimer, PostThreadMessageW, RegisterClassW, SetTimer, TranslateMessage, CHILDID_SELF,
    EVENT_OBJECT_CLOAKED, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_OBJECT_NAMECHANGE,
    EVENT_OBJECT_SHOW, EVENT_OBJECT_UNCLOAKED, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND,
    EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MOVESIZEEND, MSG, OBJID_WINDOW, SPI_SETWORKAREA,
    WINDOW_EX_STYLE, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_APP, WM_DISPLAYCHANGE,
    WM_ENDSESSION, WM_HOTKEY, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSW, WS_POPUP,
};
use wm::{Command, WindowManager, WmEvent};

thread_local! {
    static WM: RefCell<Option<WindowManager>> = const { RefCell::new(None) };
}

static MAIN_THREAD_ID: AtomicU32 = AtomicU32::new(0);
const WM_APP_EXIT: u32 = WM_APP + 1;

/// try_borrow_mut guards against reentrancy if a hook ever fires while we're
/// already inside the manager (shouldn't happen out-of-context, but cheap).
pub fn with_wm<R>(f: impl FnOnce(&mut WindowManager) -> R) -> Option<R> {
    WM.with(|cell| {
        cell.try_borrow_mut()
            .ok()
            .and_then(|mut slot| slot.as_mut().map(f))
    })
}

// ------------------------------------------------------------------- events

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if hwnd.is_invalid() || id_object != OBJID_WINDOW.0 || id_child != CHILDID_SELF as i32 {
        return;
    }
    let w = Window::from_hwnd(hwnd);
    let ev = match event {
        EVENT_OBJECT_SHOW | EVENT_OBJECT_UNCLOAKED => WmEvent::Shown(w),
        // Firefox & friends show their window first and set the title later,
        // failing the is_manageable title check at SHOW time. A title change
        // is a second chance to adopt them.
        EVENT_OBJECT_NAMECHANGE => WmEvent::Retitled(w),
        EVENT_SYSTEM_MINIMIZEEND => WmEvent::Restored(w),
        EVENT_OBJECT_HIDE | EVENT_OBJECT_CLOAKED => WmEvent::Hidden(w),
        EVENT_OBJECT_DESTROY => WmEvent::Destroyed(w),
        EVENT_SYSTEM_FOREGROUND => WmEvent::Foreground(w),
        EVENT_SYSTEM_MINIMIZESTART => WmEvent::MinimizeStart(w),
        EVENT_SYSTEM_MOVESIZEEND => {
            // A window dropped on a bar workspace cell goes to that workspace.
            // Try the cursor first; if it missed the strip, try the dragged
            // window's own top edge — users aim the window, not the pointer.
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let win_top = w.visible_rect().y;
            let drop = bar::workspace_cell_at_point(pt.x, pt.y)
                .or_else(|| bar::workspace_cell_at_point(pt.x, win_top + 8))
                .or_else(|| bar::workspace_cell_at_point(pt.x, win_top + 30));
            if std::env::var_os("WTM_DEBUG").is_some() {
                crate::logln!(
                    "wtm: drag end at ({}, {}), window top {}, drop {:?}",
                    pt.x, pt.y, win_top, drop
                );
            }
            WmEvent::MoveSizeEnd(w, drop)
        }
        _ => return,
    };
    with_wm(|wm| wm.handle_event(ev));
    bar::invalidate_all();
    border::update();
}

fn install_hooks() -> Vec<HWINEVENTHOOK> {
    let ranges = [
        (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
        (EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZEEND),
        (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
        (EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE), // covers DESTROY, SHOW, HIDE
        (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
        (EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_NAMECHANGE),
    ];
    ranges
        .iter()
        .map(|&(lo, hi)| unsafe {
            SetWinEventHook(
                lo,
                hi,
                None,
                Some(win_event_proc),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        })
        .collect()
}

// ------------------------------------------------------------------ hotkeys

thread_local! {
    static HOTKEYS: RefCell<HashMap<i32, Command>> = RefCell::new(HashMap::new());
}

/// (Re)register every hotkey from the WindowManager's current keybindings.
/// Also called by the bar after the user rebinds a key in the help panel.
pub fn reregister_hotkeys() {
    let (bindings, ws_count, launcher) = with_wm(|wm| {
        (wm.config.keybindings.clone(), wm.config.workspaces, wm.config.launcher.clone())
    })
    .unwrap_or_default();
    HOTKEYS.with(|cell| {
        let mut map = cell.borrow_mut();
        for id in map.keys() {
            let _ = unsafe { UnregisterHotKey(None, *id) };
        }
        map.clear();
        let mut next_id = 1i32;
        let mut failures = 0usize;
        let mut register = |map: &mut HashMap<i32, Command>, chord: &str, cmd: Command| {
            let Some((mods, vk)) = keys::parse_chord(chord) else {
                crate::logln!("wtm: invalid chord {chord:?} in keybindings");
                failures += 1;
                return;
            };
            match unsafe { RegisterHotKey(None, next_id, mods, vk) } {
                Ok(()) => {
                    map.insert(next_id, cmd);
                    next_id += 1;
                }
                Err(e) => {
                    crate::logln!("wtm: could not register {chord:?}: {e}");
                    failures += 1;
                }
            }
        };
        for (action, chord) in &bindings {
            if chord.contains("{n}") {
                for d in 1..=9usize {
                    if let Some(cmd) = keys::action_to_command(action, Some(d - 1)) {
                        register(&mut map, &keys::expand(chord, d), cmd);
                    }
                }
                // The 0 key reaches workspace 10 when that many exist.
                if ws_count >= 10 {
                    if let Some(cmd) = keys::action_to_command(action, Some(9)) {
                        register(&mut map, &keys::expand(chord, 0), cmd);
                    }
                }
            } else if let Some(cmd) = keys::action_to_command(action, None) {
                register(&mut map, chord, cmd);
            } else {
                crate::logln!("wtm: unknown action {action:?} in keybindings");
            }
        }
        // Per-app launch shortcuts assigned in the launcher GUI.
        for (i, entry) in launcher.iter().enumerate() {
            if !entry.key.is_empty() {
                register(&mut map, &entry.key, Command::Launch(i));
            }
        }
        crate::logln!("wtm: {} hotkeys registered, {} failed", map.len(), failures);
    });
}

/// RegisterHotKey swallows the combo key but the foreground app still sees a
/// "clean" Alt (or Win) press and release, which puts ribbon apps like AVEVA
/// E3D into keyboard-accelerator mode (KeyTips) and Windows into Start-menu
/// mode. While the modifier is still held after one of our hotkeys fired,
/// inject a press of an unassigned virtual key (0xE8 — the same masking
/// trick AutoHotkey uses) so the app sees Alt+<nothing meaningful> instead
/// of a lone Alt tap.
fn mask_hotkey_modifiers() {
    unsafe {
        let held = [VK_MENU, VK_LWIN, VK_RWIN]
            .iter()
            .any(|vk| GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000 != 0);
        if !held {
            return;
        }
        let key = |flags: KEYBD_EVENT_FLAGS| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0xE8),
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let inputs = [key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)];
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

fn unregister_all_hotkeys() {
    HOTKEYS.with(|cell| {
        for id in cell.borrow_mut().drain().map(|(id, _)| id) {
            let _ = unsafe { UnregisterHotKey(None, id) };
        }
    });
}

// -------------------------------------------------------------------- misc

unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let list = &mut *(lparam.0 as *mut Vec<Window>);
    list.push(Window::from_hwnd(hwnd));
    TRUE
}

/// All top-level windows, topmost first.
pub fn enum_top_level_windows() -> Vec<Window> {
    let mut list: Vec<Window> = Vec::new();
    unsafe {
        let _ = EnumWindows(
            Some(enum_windows_proc),
            LPARAM(&mut list as *mut Vec<Window> as isize),
        );
    }
    list
}

// ---------------------------------------------------------- display changes

/// Hidden window that receives WM_DISPLAYCHANGE / work-area broadcasts.
/// Windows sends these in bursts (and our own AppBars trigger work-area
/// changes too), so reactions are debounced through a 500ms timer, and bars
/// are only rebuilt when the monitor topology truly changed.
unsafe extern "system" fn events_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_DISPLAYCHANGE => {
            SetTimer(Some(hwnd), 1, 500, None);
            LRESULT(0)
        }
        WM_SETTINGCHANGE if wparam.0 == SPI_SETWORKAREA.0 as usize => {
            SetTimer(Some(hwnd), 1, 500, None);
            LRESULT(0)
        }
        WM_TIMER => {
            match wparam.0 {
                1 => {
                    let _ = KillTimer(Some(hwnd), 1);
                    on_display_settled();
                }
                2 => poll_config_reload(), // periodic; never killed
                _ => {}
            }
            LRESULT(0)
        }
        // Logoff/shutdown: un-hide everything and clear the hidden-window
        // journal so no stale handles get "restored" on the next boot.
        WM_ENDSESSION if wparam.0 != 0 => {
            with_wm(|wm| wm.cleanup());
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn on_display_settled() {
    let topology_changed = with_wm(|wm| wm.handle_display_change()).unwrap_or(false);
    if topology_changed {
        // Monitor set changed: bars must be recreated on the new monitors.
        // Their AppBars reshape the work areas, so refresh and retile again.
        if let Some(cfg) = with_wm(|wm| wm.config.clone()) {
            bar::destroy_all();
            bar::init(&cfg);
        }
        with_wm(|wm| {
            wm.refresh_monitors();
            wm.retile_all();
        });
    }
    bar::invalidate_all();
    border::update();
}

fn create_events_window() {
    unsafe {
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        let class = WNDCLASSW {
            lpfnWndProc: Some(events_proc),
            hInstance: hinstance.into(),
            lpszClassName: w!("wtm_events"),
            ..Default::default()
        };
        RegisterClassW(&class);
        if let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("wtm_events"),
            w!("wtm events"),
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
            // Config hot-reload: poll config.toml's mtime every 1.5s.
            SetTimer(Some(hwnd), 2, 1500, None);
        }
    }
}

// ------------------------------------------------------- config hot-reload

thread_local! {
    static CONFIG_MTIME: RefCell<Option<std::time::SystemTime>> = const { RefCell::new(None) };
}

fn config_mtime() -> Option<std::time::SystemTime> {
    config::config_path()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
}

fn poll_config_reload() {
    let m = config_mtime();
    let changed = CONFIG_MTIME.with(|c| {
        let mut slot = c.borrow_mut();
        let changed = slot.is_some() && *slot != m && m.is_some();
        *slot = m;
        changed
    });
    if !changed {
        return;
    }
    let Some(cfg) = config::try_load() else {
        crate::logln!("wtm: config.toml changed but did not parse — keeping current settings");
        return;
    };
    // Our own panel saves also touch the file; skip when nothing differs.
    if with_wm(|wm| wm.config == cfg).unwrap_or(true) {
        return;
    }
    crate::logln!("wtm: config.toml changed — reloading");
    apply_new_config(cfg);
}

/// Apply a fresh Config to every subsystem. Also used by the tray's
/// "Reload config" item.
pub fn apply_new_config(cfg: config::Config) {
    with_wm(|wm| wm.apply_config(cfg.clone()));
    bar::set_keybinds(&cfg.keybindings);
    overview::close();
    bar::destroy_all();
    bar::init(&cfg);
    // The bars' AppBars just reshaped the work areas.
    with_wm(|wm| {
        wm.refresh_monitors();
        wm.retile_all();
    });
    border::set_radius(cfg.border_corner_radius);
    border::set_style(
        cfg.border_thickness,
        config::parse_colorref(&cfg.active_border_color),
    );
    mouse::set_enabled(cfg.focus_follows_mouse);
    reregister_hotkeys();
    bar::invalidate_all();
    border::update();
}

pub fn force_config_reload() {
    match config::try_load() {
        Some(cfg) => {
            crate::logln!("wtm: reloading config");
            apply_new_config(cfg);
        }
        None => crate::logln!("wtm: config.toml missing or malformed — nothing reloaded"),
    }
}

/// Ctrl+C / console-close: ask the main thread to clean up, and give it a
/// moment to un-hide windows before the process dies.
unsafe extern "system" fn ctrl_handler(_kind: u32) -> BOOL {
    let tid = MAIN_THREAD_ID.load(Ordering::SeqCst);
    let _ = PostThreadMessageW(tid, WM_APP_EXIT, WPARAM(0), LPARAM(0));
    Sleep(1500);
    TRUE
}

fn main() {
    logger::init();
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // Shell property-store reads (Window::app_id) need COM on this thread.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        // Single instance.
        let _mutex = CreateMutexW(None, true, w!("Local\\wtm-tiling-wm"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            crate::logln!("wtm: already running");
            return;
        }

        MAIN_THREAD_ID.store(GetCurrentThreadId(), Ordering::SeqCst);
        let _ = SetConsoleCtrlHandler(Some(ctrl_handler), true);
    }

    // If a previous instance died without cleanup, bring its windows back.
    WindowManager::restore_orphans();

    let cfg = config::load();
    if let Some(p) = config::config_path() {
        crate::logln!("wtm: config: {} {}", p.display(), if p.exists() { "" } else { "(defaults)" });
    }
    CONFIG_MTIME.with(|c| *c.borrow_mut() = config_mtime());

    let manager = WindowManager::new(cfg.clone());
    crate::logln!("wtm: managing {} monitor(s)", manager.monitors.len());
    WM.with(|cell| *cell.borrow_mut() = Some(manager));

    // Bars register as AppBars, which shrinks the monitors' work areas —
    // refresh geometry before the first tiling pass so nothing overlaps.
    bar::init(&cfg);
    border::init(&cfg);
    overview::init();
    tray::init();
    create_events_window();
    with_wm(|wm| {
        wm.refresh_monitors();
        wm.adopt_existing_windows(enum_top_level_windows());
    });
    border::update();

    let hooks = install_hooks();
    mouse::init(cfg.focus_follows_mouse);
    reregister_hotkeys();
    crate::logln!("wtm: running — Alt+Shift+E to exit, Alt+P to pause, Alt+/ for help");

    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            match msg.message {
                WM_HOTKEY => {
                    let cmd =
                        HOTKEYS.with(|h| h.borrow().get(&(msg.wParam.0 as i32)).copied());
                    if cmd.is_some() {
                        mask_hotkey_modifiers();
                    }
                    match cmd {
                        Some(Command::ShowHelp) => bar::toggle_help_panel(),
                        Some(Command::Launcher) => bar::toggle_launcher_panel(),
                        Some(Command::Launch(i)) => bar::launch_index(i),
                        Some(Command::Overview) => overview::toggle(),
                        Some(Command::WindowSwitcher) => bar::toggle_switcher_panel(),
                        Some(cmd) => {
                            let keep_going =
                                with_wm(|wm| wm.handle_command(cmd)).unwrap_or(true);
                            bar::invalidate_all();
                            border::update();
                            if !keep_going {
                                break;
                            }
                        }
                        None => {}
                    }
                }
                // Thread timers (hwnd == 0) drive the window animations.
                WM_TIMER if msg.hwnd.is_invalid() && animate::is_anim_timer(msg.wParam.0) => {
                    animate::tick();
                }
                // …and the focus-follows-mouse settle delay.
                WM_TIMER if msg.hwnd.is_invalid() && mouse::is_focus_timer(msg.wParam.0) => {
                    mouse::tick();
                }
                WM_APP_EXIT => break,
                _ => {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }

        unregister_all_hotkeys();
        mouse::destroy();
        for hook in hooks {
            let _ = UnhookWinEvent(hook);
        }
    }

    tray::destroy();
    border::destroy();
    bar::destroy_all();
    with_wm(|wm| wm.cleanup());
    crate::logln!("wtm: exited cleanly");
}

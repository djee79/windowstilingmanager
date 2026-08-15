//! User configuration, loaded from %APPDATA%\wtm\config.toml.
//! Every field has a sensible default so the file is optional.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// One entry in the Alt+Space application launcher.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LauncherEntry {
    /// Display name, e.g. "E3D — ProjectA (local)".
    pub name: String,
    /// Executable, .bat, .lnk or document path. %ENV_VARS% are expanded.
    pub command: String,
    /// Optional command-line arguments.
    #[serde(default)]
    pub args: String,
    /// Optional working directory.
    #[serde(default)]
    pub dir: String,
    /// Optional global shortcut, e.g. "win+enter". Assignable from the
    /// launcher GUI (click the key chip on a row, press the chord).
    #[serde(default)]
    pub key: String,
    /// Optional group name; the launcher shows grouped entries under
    /// section headers when browsing. Editable from the manage-apps panel.
    #[serde(default)]
    pub group: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Gap between the screen work-area edge and windows, in pixels.
    pub outer_gap: i32,
    /// Gap between adjacent tiled windows, in pixels.
    pub inner_gap: i32,
    /// Share of space the earlier window keeps at each dwindle split.
    pub split_ratio: f32,
    /// Accent border color of the focused window, "#RRGGBB".
    pub active_border_color: String,
    /// Thickness of the focus border frame in pixels; 0 disables it
    /// (falls back to the thin DWM accent border only).
    pub border_thickness: i32,
    /// Corner radius of the focus frame, matching Windows 11's rounded
    /// window corners (~8). 0 gives square corners.
    pub border_corner_radius: i32,
    /// Number of workspaces per monitor (1-24).
    pub workspaces: usize,
    /// Optional workspace names shown on the bar; index 0 = workspace 1.
    /// Editable from the bar's settings (≡) panel.
    pub workspace_names: Vec<String>,
    /// Opt-in auto-placement: exe name -> workspace number (1-based).
    /// *New* windows of that app open in that workspace; existing windows
    /// are never moved. Created/removed with the pin_app key or the ≡ panel;
    /// empty table = feature entirely off.
    pub app_rules: BTreeMap<String, usize>,
    /// Show the status bar at the top of each monitor.
    pub bar_enabled: bool,
    /// Bar height in logical pixels (scaled by monitor DPI).
    pub bar_height: i32,
    /// Bar background color, "#RRGGBB".
    pub bar_background: String,
    /// Bar text color, "#RRGGBB".
    pub bar_foreground: String,
    /// Bar opacity, 0-255. 255 = fully opaque; ~235 gives a subtle glass
    /// look over whatever is behind the bar.
    pub bar_alpha: u8,
    /// Window-move animation duration in milliseconds; 0 disables.
    pub animation_ms: u32,
    /// After moving a window to another workspace, switch to it too.
    pub follow_moved_window: bool,
    /// Focus follows the mouse: hovering over a managed window activates it
    /// without clicking (Hyprland-style).
    pub focus_follows_mouse: bool,
    /// A lone window on a workspace fills the whole work area with no gaps
    /// and no focus frame (Hyprland's no_gaps_when_only).
    pub smart_gaps: bool,
    /// action -> chord overrides, e.g. `focus_left = "ctrl+alt+left"`.
    /// Missing actions keep their defaults (see keys::ACTIONS).
    pub keybindings: BTreeMap<String, String>,
    /// Apps offered by the Alt+Space launcher, in [[launcher]] order.
    pub launcher: Vec<LauncherEntry>,
    /// Window classes that should float instead of tile (exact match).
    pub float_classes: Vec<String>,
    /// Title substrings that should float.
    pub float_titles: Vec<String>,
    /// Window classes never to manage (exact match).
    pub ignore_classes: Vec<String>,
    /// Executable names never to manage, e.g. "EpicGamesLauncher.exe".
    pub ignore_exes: Vec<String>,
    /// Title substrings never to manage.
    pub ignore_titles: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            outer_gap: 10,
            inner_gap: 10,
            split_ratio: 0.5,
            active_border_color: "#7aa2f7".to_string(),
            border_thickness: 3,
            border_corner_radius: 8,
            workspaces: 9,
            workspace_names: Vec::new(),
            app_rules: BTreeMap::new(),
            bar_enabled: true,
            bar_height: 32,
            bar_background: "#181825".to_string(),
            bar_foreground: "#cdd6f4".to_string(),
            bar_alpha: 235,
            animation_ms: 150,
            follow_moved_window: true,
            focus_follows_mouse: true,
            smart_gaps: true,
            keybindings: default_keybindings(),
            launcher: Vec::new(),
            float_classes: vec![
                "#32770".to_string(), // classic Win32 dialog class
                "OperationStatusWindow".to_string(), // Explorer copy dialogs
            ],
            float_titles: vec![],
            ignore_classes: vec![
                "Progman".to_string(),
                "WorkerW".to_string(),
                "Shell_TrayWnd".to_string(),
                "Shell_SecondaryTrayWnd".to_string(),
                "Windows.UI.Core.CoreWindow".to_string(),
                "XamlExplorerHostIslandWindow".to_string(),
                "Xaml_WindowedPopupClass".to_string(),
                "TaskListThumbnailWnd".to_string(),
                "TopLevelWindowForOverflowXamlIsland".to_string(),
            ],
            ignore_exes: vec![
                "SearchHost.exe".to_string(),
                "StartMenuExperienceHost.exe".to_string(),
                "ShellExperienceHost.exe".to_string(),
                "TextInputHost.exe".to_string(),
                "LockApp.exe".to_string(),
            ],
            ignore_titles: vec![],
        }
    }
}

pub fn default_keybindings() -> BTreeMap<String, String> {
    crate::keys::ACTIONS
        .iter()
        .map(|(action, chord, _)| (action.to_string(), chord.to_string()))
        .collect()
}

pub fn config_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("wtm").join("config.toml"))
}

/// Parse the config file. None when the file is missing or malformed —
/// the hot-reload path uses this so a half-written file (editor mid-save)
/// never blows away the current settings.
pub fn try_load() -> Option<Config> {
    let path = config_path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    match toml::from_str::<Config>(&text) {
        Ok(mut cfg) => {
            // Migrate action names from older versions.
            for (old, new) in [("shrink_ratio", "shrink_window"), ("grow_ratio", "grow_window")] {
                if let Some(chord) = cfg.keybindings.remove(old) {
                    cfg.keybindings.entry(new.to_string()).or_insert(chord);
                }
            }
            // A user [keybindings] table only lists overrides; fill the
            // rest from defaults so every action stays bound.
            let mut merged = default_keybindings();
            merged.extend(cfg.keybindings);
            cfg.keybindings = merged;
            cfg.workspaces = cfg.workspaces.clamp(1, 24);
            cfg.app_rules = cfg
                .app_rules
                .into_iter()
                .map(|(k, v)| (k.to_lowercase(), v))
                .collect();
            Some(cfg)
        }
        Err(e) => {
            crate::logln!("wtm: error in {}: {e}", path.display());
            None
        }
    }
}

/// Load the config file, falling back to defaults. A malformed file is
/// reported in the log but never prevents startup.
pub fn load() -> Config {
    let exists = config_path().map(|p| p.exists()).unwrap_or(false);
    match try_load() {
        Some(cfg) => cfg,
        None => {
            if exists {
                crate::logln!("wtm: continuing with default configuration");
            }
            Config::default()
        }
    }
}

/// Load-mutate-write config.toml, keeping every other setting in the file
/// intact (comments/formatting are not preserved).
fn update_config_file(mutate: impl FnOnce(&mut toml::Table)) -> Result<(), String> {
    let path = config_path().ok_or("APPDATA not set")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut table: toml::Table = text.parse().map_err(|e| format!("{e}"))?;
    mutate(&mut table);
    let out = toml::to_string_pretty(&table).map_err(|e| e.to_string())?;
    std::fs::write(&path, out).map_err(|e| e.to_string())
}

fn table_entry<'t>(table: &'t mut toml::Table, section: &str) -> Option<&'t mut toml::Table> {
    match table
        .entry(section)
        .or_insert_with(|| toml::Value::Table(Default::default()))
    {
        toml::Value::Table(t) => Some(t),
        _ => None,
    }
}

pub fn save_keybinding(action: &str, chord: &str) -> Result<(), String> {
    update_config_file(|table| {
        if let Some(t) = table_entry(table, "keybindings") {
            t.insert(action.to_string(), toml::Value::String(chord.to_string()));
        }
    })
}

pub fn save_workspace_names(names: &[String]) -> Result<(), String> {
    update_config_file(|table| {
        table.insert(
            "workspace_names".to_string(),
            toml::Value::Array(names.iter().map(|n| toml::Value::String(n.clone())).collect()),
        );
    })
}

/// Persist the whole launcher list (add/remove from the GUI).
pub fn save_launcher(entries: &[LauncherEntry]) -> Result<(), String> {
    update_config_file(|table| {
        let arr = entries
            .iter()
            .map(|e| {
                let mut t = toml::Table::new();
                t.insert("name".into(), toml::Value::String(e.name.clone()));
                t.insert("command".into(), toml::Value::String(e.command.clone()));
                if !e.args.is_empty() {
                    t.insert("args".into(), toml::Value::String(e.args.clone()));
                }
                if !e.dir.is_empty() {
                    t.insert("dir".into(), toml::Value::String(e.dir.clone()));
                }
                if !e.key.is_empty() {
                    t.insert("key".into(), toml::Value::String(e.key.clone()));
                }
                if !e.group.is_empty() {
                    t.insert("group".into(), toml::Value::String(e.group.clone()));
                }
                toml::Value::Table(t)
            })
            .collect();
        table.insert("launcher".to_string(), toml::Value::Array(arr));
    })
}

pub fn save_workspaces(n: usize) -> Result<(), String> {
    update_config_file(|table| {
        table.insert("workspaces".to_string(), toml::Value::Integer(n as i64));
    })
}

pub fn save_border_thickness(t: i32) -> Result<(), String> {
    update_config_file(|table| {
        table.insert("border_thickness".to_string(), toml::Value::Integer(t as i64));
    })
}

pub fn save_border_color(hex: &str) -> Result<(), String> {
    update_config_file(|table| {
        table.insert(
            "active_border_color".to_string(),
            toml::Value::String(hex.to_string()),
        );
    })
}

/// Some(n) adds/updates the rule for `exe`; None removes it.
pub fn save_app_rule(exe: &str, workspace: Option<usize>) -> Result<(), String> {
    update_config_file(|table| {
        if let Some(t) = table_entry(table, "app_rules") {
            match workspace {
                Some(n) => {
                    t.insert(exe.to_string(), toml::Value::Integer(n as i64));
                }
                None => {
                    t.remove(exe);
                }
            }
        }
    })
}

/// "#RRGGBB" -> Win32 COLORREF (0x00BBGGRR). Falls back to a blue accent.
pub fn parse_colorref(hex: &str) -> u32 {
    let s = hex.trim_start_matches('#');
    if s.len() == 6 {
        if let Ok(rgb) = u32::from_str_radix(s, 16) {
            let r = (rgb >> 16) & 0xFF;
            let g = (rgb >> 8) & 0xFF;
            let b = rgb & 0xFF;
            return (b << 16) | (g << 8) | r;
        }
    }
    0x00F7A27A // default #7aa2f7 as COLORREF
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colorref_swaps_channels() {
        assert_eq!(parse_colorref("#FF0000"), 0x0000FF); // red -> low byte
        assert_eq!(parse_colorref("#0000FF"), 0xFF0000); // blue -> high byte
    }
}

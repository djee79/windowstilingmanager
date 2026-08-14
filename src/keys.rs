//! Keybinding actions and chord parsing. The single source of truth shared
//! by the config defaults, hotkey registration, and the help/rebind panel.
//!
//! A chord looks like "alt+shift+left" or "ctrl+alt+{n}" — `{n}` expands to
//! the digits 1..9 for the workspace actions.

use crate::wm::{Command, Dir};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
};

/// (action, default chord, description)
pub const ACTIONS: &[(&str, &str, &str)] = &[
    ("focus_left", "alt+left", "Focus window to the left"),
    ("focus_right", "alt+right", "Focus window to the right"),
    ("focus_up", "alt+up", "Focus window above"),
    ("focus_down", "alt+down", "Focus window below"),
    ("swap_left", "alt+shift+left", "Move window left (or to next monitor)"),
    ("swap_right", "alt+shift+right", "Move window right (or to next monitor)"),
    ("swap_up", "alt+shift+up", "Move window up (or to next monitor)"),
    ("swap_down", "alt+shift+down", "Move window down (or to next monitor)"),
    ("focus_next", "alt+j", "Focus next window (cycle)"),
    ("focus_prev", "alt+k", "Focus previous window (cycle)"),
    ("swap_next", "alt+shift+j", "Swap with next window"),
    ("swap_prev", "alt+shift+k", "Swap with previous window"),
    ("promote", "alt+enter", "Promote window to master slot"),
    ("shrink_window", "alt+h", "Shrink focused window"),
    ("grow_window", "alt+l", "Grow focused window"),
    ("toggle_float", "alt+t", "Toggle floating"),
    ("toggle_monocle", "alt+f", "Toggle monocle layout"),
    ("toggle_fullscreen", "alt+shift+f", "Toggle fullscreen (covers the bar)"),
    ("close_window", "alt+q", "Close focused window"),
    ("switch_workspace", "alt+{n}", "Switch to workspace n (0 = 10)"),
    ("move_to_workspace", "alt+shift+{n}", "Move window to workspace n"),
    ("next_workspace", "alt+period", "Switch to next workspace"),
    ("prev_workspace", "alt+comma", "Switch to previous workspace"),
    ("move_next_workspace", "alt+shift+period", "Carry window to next workspace"),
    ("move_prev_workspace", "alt+shift+comma", "Carry window to previous workspace"),
    ("retile", "alt+shift+r", "Rescan and retile"),
    ("toggle_pause", "alt+p", "Pause / resume tiling"),
    ("pin_app", "alt+shift+p", "Pin/unpin app: its new windows open here"),
    ("show_help", "alt+slash", "Show keybindings panel"),
    ("launcher", "alt+space", "Open the application launcher"),
    ("exit", "alt+shift+e", "Exit wtm"),
];

/// Does this action use the `{n}` digit placeholder?
pub fn is_numbered(action: &str) -> bool {
    ACTIONS
        .iter()
        .find(|(a, _, _)| *a == action)
        .map(|(_, c, _)| c.contains("{n}"))
        .unwrap_or(false)
}

pub fn action_to_command(action: &str, digit: Option<usize>) -> Option<Command> {
    Some(match action {
        "focus_left" => Command::FocusDir(Dir::Left),
        "focus_right" => Command::FocusDir(Dir::Right),
        "focus_up" => Command::FocusDir(Dir::Up),
        "focus_down" => Command::FocusDir(Dir::Down),
        "swap_left" => Command::SwapDir(Dir::Left),
        "swap_right" => Command::SwapDir(Dir::Right),
        "swap_up" => Command::SwapDir(Dir::Up),
        "swap_down" => Command::SwapDir(Dir::Down),
        "focus_next" => Command::FocusNext,
        "focus_prev" => Command::FocusPrev,
        "swap_next" => Command::SwapNext,
        "swap_prev" => Command::SwapPrev,
        "promote" => Command::Promote,
        "shrink_window" => Command::ShrinkWindow,
        "grow_window" => Command::GrowWindow,
        "toggle_float" => Command::ToggleFloat,
        "toggle_monocle" => Command::ToggleMonocle,
        "toggle_fullscreen" => Command::ToggleFullscreen,
        "close_window" => Command::CloseWindow,
        "switch_workspace" => Command::SwitchWorkspace(digit?),
        "move_to_workspace" => Command::MoveToWorkspace(digit?),
        "next_workspace" => Command::NextWorkspace,
        "prev_workspace" => Command::PrevWorkspace,
        "move_next_workspace" => Command::MoveNextWorkspace,
        "move_prev_workspace" => Command::MovePrevWorkspace,
        "retile" => Command::Retile,
        "toggle_pause" => Command::TogglePause,
        "pin_app" => Command::PinApp,
        "show_help" => Command::ShowHelp,
        "launcher" => Command::Launcher,
        "exit" => Command::Exit,
        _ => return None,
    })
}

// --------------------------------------------------------------- key names

const NAMED_KEYS: &[(&str, u32)] = &[
    ("left", 0x25),
    ("up", 0x26),
    ("right", 0x27),
    ("down", 0x28),
    ("enter", 0x0D),
    ("return", 0x0D),
    ("space", 0x20),
    ("tab", 0x09),
    ("backspace", 0x08),
    ("insert", 0x2D),
    ("delete", 0x2E),
    ("home", 0x24),
    ("end", 0x23),
    ("pageup", 0x21),
    ("pagedown", 0x22),
    ("comma", 0xBC),
    ("period", 0xBE),
    ("slash", 0xBF),
    ("semicolon", 0xBA),
    ("quote", 0xDE),
    ("minus", 0xBD),
    ("equals", 0xBB),
    ("backtick", 0xC0),
    ("lbracket", 0xDB),
    ("rbracket", 0xDD),
    ("backslash", 0xDC),
    ("f1", 0x70),
    ("f2", 0x71),
    ("f3", 0x72),
    ("f4", 0x73),
    ("f5", 0x74),
    ("f6", 0x75),
    ("f7", 0x76),
    ("f8", 0x77),
    ("f9", 0x78),
    ("f10", 0x79),
    ("f11", 0x7A),
    ("f12", 0x7B),
];

fn name_to_vk(name: &str) -> Option<u32> {
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_lowercase() {
            return Some(c.to_ascii_uppercase() as u32);
        }
        if c.is_ascii_digit() {
            return Some(c as u32);
        }
    }
    NAMED_KEYS.iter().find(|(n, _)| *n == name).map(|(_, vk)| *vk)
}

pub fn vk_to_name(vk: u32) -> Option<String> {
    match vk {
        0x41..=0x5A => Some(((vk as u8 as char).to_ascii_lowercase()).to_string()),
        0x30..=0x39 => Some((vk as u8 as char).to_string()),
        _ => NAMED_KEYS
            .iter()
            .find(|(n, v)| *v == vk && *n != "return") // canonical names only
            .map(|(n, _)| n.to_string()),
    }
}

// ----------------------------------------------------------------- parsing

/// Parse a concrete chord ("{n}" must already be expanded).
pub fn parse_chord(chord: &str) -> Option<(HOT_KEY_MODIFIERS, u32)> {
    let mut mods = MOD_NOREPEAT;
    let mut key = None;
    for tok in chord.to_ascii_lowercase().split('+').map(str::trim) {
        match tok {
            "alt" => mods = mods | MOD_ALT,
            "ctrl" | "control" => mods = mods | MOD_CONTROL,
            "shift" => mods = mods | MOD_SHIFT,
            "win" | "super" => mods = mods | MOD_WIN,
            k => {
                if key.replace(name_to_vk(k)?).is_some() {
                    return None; // two non-modifier keys
                }
            }
        }
    }
    Some((mods, key?))
}

pub fn expand(chord: &str, digit: usize) -> String {
    chord.replace("{n}", &digit.to_string())
}

/// Build a normalized chord string from a captured key press.
/// `numbered` produces a "{n}" template (the pressed key is discarded).
pub fn format_captured(
    alt: bool,
    ctrl: bool,
    shift: bool,
    win: bool,
    vk: u32,
    numbered: bool,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if ctrl {
        parts.push("ctrl".into());
    }
    if win {
        parts.push("win".into());
    }
    if alt {
        parts.push("alt".into());
    }
    if shift {
        parts.push("shift".into());
    }
    parts.push(if numbered { "{n}".into() } else { vk_to_name(vk)? });
    Some(parts.join("+"))
}

/// Would two chords collide once `{n}` placeholders are expanded?
pub fn chords_overlap(a: &str, b: &str) -> bool {
    let concrete = |c: &str| -> Vec<(u32, u32)> {
        let list = if c.contains("{n}") {
            (1..=9).map(|d| expand(c, d)).collect::<Vec<_>>()
        } else {
            vec![c.to_string()]
        };
        list.iter()
            .filter_map(|s| parse_chord(s))
            .map(|(m, v)| (m.0, v))
            .collect()
    };
    let bs = concrete(b);
    concrete(a).iter().any(|x| bs.contains(x))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_chords() {
        let (m, vk) = parse_chord("alt+shift+left").unwrap();
        assert_eq!(vk, 0x25);
        assert!(m.contains(MOD_ALT) && m.contains(MOD_SHIFT));
        assert_eq!(parse_chord("alt+j").unwrap().1, 'J' as u32);
        assert!(parse_chord("alt+j+k").is_none());
        assert!(parse_chord("alt").is_none());
    }

    #[test]
    fn numbered_expansion() {
        assert_eq!(expand("alt+{n}", 3), "alt+3");
        assert!(parse_chord(&expand("alt+shift+{n}", 9)).is_some());
    }

    #[test]
    fn overlap_detection() {
        assert!(chords_overlap("alt+j", "alt+j"));
        assert!(!chords_overlap("alt+j", "alt+shift+j"));
        assert!(chords_overlap("alt+5", "alt+{n}"));
        assert!(!chords_overlap("ctrl+5", "alt+{n}"));
    }

    #[test]
    fn every_default_parses_and_maps() {
        for (action, chord, _) in ACTIONS {
            let concrete = expand(chord, 1);
            assert!(parse_chord(&concrete).is_some(), "bad default: {chord}");
            assert!(
                action_to_command(action, Some(0)).is_some(),
                "unmapped action: {action}"
            );
        }
    }

    #[test]
    fn no_default_conflicts() {
        for (i, (_, a, _)) in ACTIONS.iter().enumerate() {
            for (_, b, _) in ACTIONS.iter().skip(i + 1) {
                assert!(!chords_overlap(a, b), "{a} conflicts with {b}");
            }
        }
    }

    #[test]
    fn captured_chord_round_trips() {
        let c = format_captured(true, false, true, false, 0x25, false).unwrap();
        assert_eq!(c, "alt+shift+left");
        assert!(parse_chord(&c).is_some());
        let n = format_captured(true, true, false, false, 0x35, true).unwrap();
        assert_eq!(n, "ctrl+alt+{n}");
    }
}

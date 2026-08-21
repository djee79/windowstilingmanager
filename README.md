# wtm — a Hyprland-inspired tiling window manager for Windows 11

`wtm` automatically tiles your windows in a Hyprland-style **dwindle** (spiral)
layout, gives you 9 workspaces per monitor, paints an accent border on the
focused window, and draws a waybar-style status bar at the top of each monitor
(clickable workspaces, focused window title, clock, and a `?` button showing
all keybindings). It is a single-binary Rust program with no dependencies
beyond Win32 itself.

The bar is a shell **AppBar** (`SHAppBarMessage`), so it reserves its strip of
screen the same way the taskbar does — tiled and maximized windows never
overlap it, like waybar's exclusive zone.

Unlike Hyprland, which *is* the compositor, a Windows tiler manages other
programs' windows from the outside through Win32 APIs — the same approach used
by komorebi and GlazeWM:

- `SetWinEventHook` — react to windows opening, closing, focusing, minimizing
- `RegisterHotKey` — global keybindings
- `SetWindowPos` — place windows (with DWM shadow compensation so gaps are exact)
- `ShowWindow` hide/show — workspace switching
- `DwmSetWindowAttribute` — focused-window accent border

## Build & run

```powershell
cargo build --release
cargo run --release     # windows tile immediately; Alt+Shift+E exits
```

Release builds run as a **background app**: no console window, diagnostics
in `%LOCALAPPDATA%\wtm\wtm.log` (auto-rotated, one `.old` kept). Debug
builds (`cargo run`) keep the console for development. To start with
Windows, put a shortcut to the release exe in `shell:startup`.

No admin rights needed (but windows of elevated apps can't be managed from a
non-elevated wtm). First run retiles everything currently on screen.

Tip: if the checkout lives on a network share, create an untracked
`.cargo/config.toml` pointing `[build] target-dir` at a local disk —
cargo writes thousands of intermediate files and NAS builds are slow.

## Keybindings

All bindings are configurable — either in the `[keybindings]` section of the
config file, or interactively: open the `?` panel (bar button or `Alt+/`),
type to fuzzy-search, click a row, press the new shortcut. The change is
saved to the config and active immediately.

Defaults:

| Keys | Action |
|---|---|
| `Alt+←↑↓→` | Focus window in that direction (crosses monitors) |
| `Alt+Shift+←↑↓→` | Swap window in that direction; at the screen edge, push it to the next monitor |
| `Alt+J` / `Alt+K` | Focus next / previous window (cycle) |
| `Alt+Shift+J` / `Alt+Shift+K` | Swap with next / previous window |
| `Alt+Enter` | Promote focused window to the master (largest) slot |
| `Alt+H` / `Alt+L` | Shrink / grow the focused window (its own divider; floating windows scale) |
| `Alt+T` | Toggle floating for the focused window |
| `Alt+V` | Flip the split at the focused window: side-by-side ⇄ stacked (Hyprland's `togglesplit`) |
| `Alt+F` | Toggle monocle (focused workspace: every window full-size) |
| `Alt+Shift+F` | Toggle fullscreen (covers the whole monitor, bar included) |
| `Alt+Q` | Close the focused window |
| `Alt+1..9` (`Alt+0` = 10) | Switch workspace on the current monitor |
| `Alt+,` / `Alt+.` | Previous / next workspace (reaches any count) |
| `Alt+Shift+,` / `Alt+Shift+.` | Carry the focused window to the previous / next workspace |
| `Alt+Shift+1..9` | Move focused window to workspace (and follow it) |
| `Alt+/` | Toggle the keybindings panel |
| `Alt+Space` | Open the application launcher (fuzzy search, Enter launches) |
| `Alt+P` | Pause/resume tiling |
| `Alt+Shift+P` | Pin/unpin the focused app: its *new* windows open on this workspace |
| `Alt+Shift+R` | Rescan windows & retile (also refreshes monitor geometry) |
| `Alt+Shift+E` | Exit (restores all hidden windows) |

Dragging a tiled window onto another one swaps their slots. Dragging any
window onto a workspace cell on the bar sends it to that workspace — the
no-memorization way to reach workspace 10+ (works across monitors too).

## Command line (hyprctl-style)

Every action can also be driven from a terminal or script — `wtm <action>`
sends the command to the running instance:

```
wtm toggle_split          # flip the focused window's split direction
wtm switch_workspace 3    # workspace numbers are 1-based, like the bar
wtm scratchpad
wtm list                  # all actions, their arguments and current keys
```

## More goodies

- **Drag to resize** — grab any tiled window's edge or corner with the
  mouse; the layout's split ratios follow the drag instead of snapping back.
- **Workspace overview** (`Alt+`` ` or left-click the tray icon) — an
  Exposé-style grid of every workspace drawn as a miniature of its layout
  with app icons. Click a window to jump straight to it.
- **Window switcher** (`Alt+W`) — fuzzy-search every managed window across
  all workspaces and monitors; Enter jumps to it, wherever it lives.
- **Scratchpad** (`Alt+S` / `Alt+Shift+S`) — park windows outside the
  workspaces and summon them anywhere as a floating topmost cascade
  (Hyprland's special workspace). `Alt+Shift+S` on a scratchpad window
  pulls it back out into the current workspace. Each window keeps the size
  you give it (mouse resize or the grow/shrink keys), `Alt+Shift+F`
  fullscreens it over everything, and `Alt+N` cycles focus through the
  scratchpad windows. `Alt+Ctrl+S` pins the focused app to the scratchpad:
  its windows (KeePass, a drop-down terminal) always open there from then
  on, listed in `scratch_apps` in the config.
- **Meeting chip** (opt-in) — set `calendar_source = "outlook"` (classic
  Outlook via COM: offline, no cloud, no OAuth — your Teams meetings live in
  the Outlook calendar) or point it at any `.ics` file/URL (Google, Proton,
  Nextcloud). The bar shows your next meeting with a live countdown next to
  the clock; click it for the agenda, click a meeting to join its Teams
  link. With no source configured, nothing runs at all.
- **Config hot-reload** — edit `config.toml` in any editor and wtm applies
  it live within ~2 seconds: colors, gaps, workspaces, keybindings, bars.
- **Smart gaps** — a lone window fills its workspace edge-to-edge with no
  gaps and no focus frame.
- **Tray icon** — pause/resume, reload config, a start-with-Windows toggle,
  and exit; the bar itself is subtly translucent (`bar_alpha`) and all
  panels get Windows 11 rounded corners. Workspace cells show the app icons
  of the windows living there.

The bar's ✎ button opens the manage-apps panel: add an app (browse), scan
every Start Menu app, add a web app from a URL + browser (`--app=` mode),
or import a browser bookmarks export in bulk. Each entry there has a group
chip (type a category — the launcher shows grouped entries under section
headers while browsing) and, for web apps, a 🌐 chip that cycles which
browser opens it. Ctrl+V pastes into any panel text field. Results are ranked (prefix > substring > fuzzy) and
boosted by frecency — the apps you launch most (and most recently) float to
the top, so Alt+Space → Enter fires your daily driver.

## Configuration

Copy `config.example.toml` to `%APPDATA%\wtm\config.toml`. Gaps, split ratio,
border color, animation duration, float rules, ignore rules and every
keybinding are configurable; see the comments in the example file.

The bar's palette button opens an appearance panel for the settings you'll
want to tweak on a whim: workspace count, focus-frame thickness (0 = off) and
accent color — 10 one-click presets plus a "custom…" button that opens the
full RGB color picker. Changes apply instantly and are saved to
`config.toml`.

Windows animate to their slots over `animation_ms` (default 150ms, ease-out;
set 0 for instant). `follow_moved_window = false` restores "send silently"
behavior for the move-to-workspace keys. Focus follows the mouse by default
(hovering over a window activates it, Hyprland-style; it never steals focus
mid-drag or from open menus) — set `focus_follows_mouse = false` for
click-to-focus. Note: rebinding a key from the `?`
panel rewrites `config.toml` (settings are preserved, comments are not).

## Workspace names & app rules

The bar's `≡` button opens a settings panel: click a workspace, type a name
("aveva e3d ProjectA"), Enter — the active workspace's name appears in its bar
cell and persists in the config. The panel also lists pinned apps
(click one to remove it).

App rules are strictly opt-in: pin an app with `Alt+Shift+P` while it's
focused on the workspace you want, and only its *newly opened* windows are
sent there (hidden if that workspace isn't visible). Existing windows are
never moved, restores from minimize are never redirected, and with no pins
the feature is entirely inert. Apps are identified by their AppUserModelID
when they set one (each browser web app — Brave/Chrome "open as app" — has
its own), falling back to the exe name, so pinning the YouTube app doesn't
drag other Brave windows along with it.

## Application launcher

`Alt+Space` (or the `»` bar button) opens a rofi-style launcher over the apps
declared as `[[launcher]]` entries in the config — a display name plus any
command: an .exe, a .bat (e.g. AVEVA `evars.bat` per project, local or
network path), a .lnk, a document or a URL, with optional args and working
directory and %ENV_VAR% expansion. Type to fuzzy-filter, ↑/↓ to pick, Enter
to launch. If nothing matches, Enter runs the typed text directly
(`notepad`, a path, a URL).

Entries are managed from the GUI too: the "+ add app (browse…)" row opens
the Windows file picker (network paths included), you type a display name,
Enter saves it to the config. The ✕ on each row removes an entry.

Each app can have a global launch shortcut, Hyprland-style (`Win+Enter` →
WezTerm): click the row's key chip ("set key"), press the chord — it is
conflict-checked, saved (`key = "win+enter"`), and active immediately.
While capturing, Backspace clears the shortcut and Esc cancels.

## Display changes

Docking, undocking, resolution changes and taskbar moves are handled
automatically (debounced `WM_DISPLAYCHANGE` / work-area broadcasts):
surviving monitors keep their workspaces, windows from a removed monitor
merge into the first monitor's same-numbered workspaces, and the bars are
rebuilt on the new layout. `Alt+Shift+R` remains as a manual refresh.

## Crash safety

Workspaces work by hiding windows, so a crash could strand them invisible.
`wtm` journals every hidden window to `%LOCALAPPDATA%\wtm-hidden-windows.txt`
and un-hides survivors on next start. Ctrl+C and closing the console both
trigger a clean shutdown that restores everything.

## Architecture

```
src/
  main.rs     message loop, WinEvent hooks, dynamic hotkey registration
  wm.rs       WindowManager: monitors → workspaces → windows; events & commands
  bar.rs      AppBar status bar per monitor + interactive keybindings panel
  border.rs   thick focus frame (topmost click-through hollow window)
  keys.rs     action table, chord parsing ("alt+shift+left"), conflict checks
  animate.rs  eased window-move animation engine (thread timer, ~60fps)
  layout.rs   dwindle layout math (pure, unit-tested)
  window.rs   HWND wrapper: manageability rules, move/hide/focus/border
  monitor.rs  monitor enumeration (work areas, primary first)
  config.rs   TOML config with defaults
```

Everything runs on one thread: out-of-context WinEvent hooks and `WM_HOTKEY`
are both delivered through the same message loop, so there are no locks.

## Roadmap ideas

- Move window across monitors with the swap keys when no swap target exists
- More per-app rules (always-float, opacity)

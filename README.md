# hiren

Wayland-native application launcher for Linux — now with a fully declarative UI runtime.

## Features

- **Declarative UI** — visual interface defined in TOML, no Rust changes needed to restyle
- **Unified search** — apps (`.desktop`), `$PATH` binaries, window switching, inline calculator
- **Fuzzy matching** — skim matcher for instant results
- **Frequency tracking** — most-used apps rise to the top
- **Native layer-shell** — `zwlr_layer_shell_v1` overlay with exclusive keyboard on wlroots compositors; winit toplevel fallback (X11, others, `--no-layer-shell`)
- **Free positioning** — absolute, circular, layered, game-like layouts all via same runtime
- **Independent selector** — selection indicator is its own visual object, not a row state
- **Expressive animations** — real damped springs, easings, enter animations with per-item stagger, rotation/scale transforms
- **Fast** — list virtualization, expression/text caches, vsync-paced rendering, zero CPU when idle
- **Themes** — 5 built-in radically different themes; user themes in `~/.config/hiren/themes/`

## Architecture

```
Launcher (Rust)  →  ObservableState  →  UI Runtime (TOML scene graph → layout → tiny-skia)
      IPC/modes           query/results/selected       bindings · repeater · animator
```

See [docs/CLIENT_ARCHITECTURE.md](docs/CLIENT_ARCHITECTURE.md) for full design, technology evaluation, and theme authoring guide.

## Requirements

| Dependency | Purpose |
|---|---|
| Rust 1.85+ | Compiler |
| `wl-copy` / `xclip` | Clipboard (calc mode) — optional |
| `wmctrl` | X11 window switching — optional |
| `swaymsg` / `hyprctl` | Sway/Hyprland window switching — optional |

No GTK4 required. Rendering is CPU via `softbuffer` + `tiny-skia` + `cosmic-text`.

## Build

```bash
git clone https://github.com/s3yf1337/hiren.git
cd hiren
cargo build --release -p hiren-daemon
cargo build --release -p hiren-client --features layer-shell   # Wayland-native backend
# or without the layer-shell feature for a pure winit build (X11-friendly)
```

Binaries:

- `target/release/hiren-daemon` — background daemon (scans .desktop files, serves search over IPC)
- `target/release/hiren-client` — declarative UI launcher (layer-shell on Wayland, winit elsewhere)

Validate built-in themes without opening a window:

```bash
cargo run -p hiren-client --features layer-shell -- --validate-themes
# ✓ default, atlus, macos, layered, circular
```

## Install

```bash
sudo make install
# or
make install PREFIX=/usr
systemctl --user enable --now hiren-daemon
```

## Usage

```bash
./target/release/hiren-daemon &
./target/release/hiren-client              # default theme
./target/release/hiren-client --theme atlus
./target/release/hiren-client --list-themes
./target/release/hiren-client --screenshot layered --out /tmp/preview.png
```

### Sway config example

```
exec_always hiren-daemon
bindsym $mod+space exec hiren-client
bindsym $mod+Shift+space exec hiren-client --theme circular
```

### Hyprland config example

```
exec-once = hiren-daemon
bind = $mainMod, space, exec, hiren-client
```

## Configuration

`~/.config/hiren/config.toml` (behavior, not visuals):

```toml
[ui]
width = 640
height = 420
auto_close_timeout_secs = 8
freq_weight = 0.8
keyboard_mode = "exclusive"  # or "on_demand"

[mode]
drun = true
run = true
calc = true
window = false

[window]
list_command = "swaymsg -t get_tree"
activate_command = 'swaymsg "[con_id={id}] focus"'

[terminal]
command = "foot"
exec_flag = "-e"

[[bindings]]
key = "Return"
[[bindings]]
key = "Ctrl+Return"
  prefix = "proxychains"

[theme]
name = "default"  # or atlus, macos, layered, circular, or your own
```

## Themes

Themes are **TOML scene graphs**, not code:

```toml
[[nodes]]
id = "selector"
type = "Rectangle"
x = "32"
y = "launcher.selected_index * 56 + 92"
width = "window.width - 64"
height = "48"
props = { background = "rgba(137,180,250,0.18)", radius = "10" }
animate = [{ property = "y", duration_ms = 220, easing = "ease_out_cubic" }]
```

- `hiren-client/themes/default/theme.toml` — clean vertical launcher (uses runtime like all others)
- `hiren-client/themes/atlus/theme.toml` — game-like, bold, skewed, independent selector
- `hiren-client/themes/macos/theme.toml` — calm floating card, spring motion
- `hiren-client/themes/layered/theme.toml` — layered panels, decorative orbs reacting to selection
- `hiren-client/themes/circular/theme.toml` — radial, search centered, results in circle

Create your own: copy `hiren-client/themes/default` to `~/.config/hiren/themes/my/` and edit `theme.toml`. No rebuild needed.

See `docs/CLIENT_ARCHITECTURE.md` for full theme authoring, bindings, components, animations, and Wayland details.

## How it works

```
┌─────────────┐     UNIX socket      ┌──────────────┐
│ hiren-daemon │ ◄──────────────────► │ hiren-client │
│              │   /tmp/hiren.socket  │              │
│ .desktop     │                      │ layer-shell  │
│ parser       │                      │ or winit     │
│ inotify      │                      │ tiny-skia    │
│ watcher      │                      │ UI runtime   │
└─────────────┘                      └──────────────┘
```

Launcher exposes `launcher.query`, `launcher.results`, `launcher.selected_index`, etc.; the TOML theme binds visual properties to them.

## License

MIT

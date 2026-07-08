# hiren

Wayland-native application launcher for Linux, written in Rust with GTK4 + layer-shell.

## Features

- **Unified search** — apps (`.desktop`), `$PATH` binaries, window switching, and inline calculator all in one box
- **Fuzzy matching** — [skim](https://github.com/lotabout/skim) matcher for instant results
- **Frequency tracking** — most-used apps rise to the top
- **Category-aware** — search by category keywords (e.g. "music" finds Spotify)
- **Layer-shell overlay** — pops up as an overlay on any Wayland compositor
- **Auto WM detection** — Sway, Hyprland, and X11 (wmctrl) supported out of the box

## Requirements

| Dependency | Purpose |
|---|---|
| Rust 1.85+ | Compiler |
| GTK4 + layer-shell | UI |
| `wl-copy` / `xclip` | Clipboard (calc mode) |
| `wmctrl` (optional) | X11 window switching |
| `swaymsg` (optional) | Sway window switching |
| `hyprctl` (optional) | Hyprland window switching |

### Install build deps (Arch)

```bash
pacman -S gtk4 gtk4-layer-shell
```

### Install build deps (Ubuntu/Debian)

```bash
apt install libgtk-4-dev libgtk4-layer-shell-dev
```

## Build

```bash
git clone https://github.com/seyf1337/hiren.git
cd hiren
cargo build --release
```

Binaries:

- `target/release/hiren-daemon` — background daemon (scans .desktop files, serves search over IPC)
- `target/release/hiren-client` — GTK4 overlay launcher

## Install

```bash
# Build and install to /usr/local/bin
sudo make install

# Or specify a custom prefix
make install PREFIX=/usr

# Enable daemon auto-start (systemd user service)
systemctl --user enable --now hiren-daemon

# Uninstall
sudo make uninstall
```

## Usage

Start the daemon first, then bind the client to a hotkey:

```bash
# Start daemon (auto-starts in sway/hyprland config, or as systemd user service)
./target/release/hiren-daemon &

# Launch the overlay
./target/release/hiren-client
```

### Sway config example

```
exec_always hiren-daemon
bindsym $mod+space exec hiren-client
```

### Hyprland config example

```
exec-once = hiren-daemon
bind = $mainMod, space, exec, hiren-client
```

## Configuration

Create `~/.config/hiren/config.toml`:

```toml
[ui]
width = 620
height = 360
auto_close_timeout_secs = 8
text_align = "center"   # "left" or "center"

[mode]
drun = true
run = true
calc = true
window = false

[window]
# Custom window commands (overrides auto-detection)
list_command = "swaymsg -t get_tree"
activate_command = 'swaymsg "[con_id={id}] focus"'

[terminal]
command = "foot"
exec_flag = "-e"

[[bindings]]
key = "Return"
# prefix = "foot -e"   # optional: launch in terminal

[[bindings]]
key = "Ctrl+Return"
```

## How it works

```
┌─────────────┐     UNIX socket      ┌──────────────┐
│ hiren-daemon │ ◄──────────────────► │ hiren-client │
│              │   /tmp/hiren.socket  │              │
│ .desktop     │                      │ GTK4 overlay │
│ parser       │                      │ fuzzy search │
│ inotify      │                      │ keyboard     │
│ watcher      │                      │ launch       │
└─────────────┘                      └──────────────┘
```

## License

MIT

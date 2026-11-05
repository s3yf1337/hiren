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
- **Themes** — five radically different built-in themes; user themes in `~/.config/hiren/themes/`

## Architecture

```
Launcher (Rust)  →  ObservableState  →  UI Runtime (TOML scene graph → layout → tiny-skia)
      IPC/modes           query/results/selected       bindings · repeater · animator
```

See [docs/CLIENT_ARCHITECTURE.md](docs/CLIENT_ARCHITECTURE.md) for full design, technology evaluation, and theme authoring guide.

## Requirements

| Dependency | Purpose |
|---|---|
| Rust 1.85+ | Compiler (`rust-version` in the workspace manifest) |
| `wl-copy` / `xclip` | Clipboard (calc mode) — optional |
| `wmctrl` | X11 window switching — optional |
| `swaymsg` / `hyprctl` | Sway/Hyprland window switching — optional |
| a terminal (`foot`, `kitty`, …) | `Ctrl+Return` launches and `Terminal=true` apps — optional |

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

- `target/release/hiren-daemon` — background daemon (scans `.desktop` files, serves search over IPC)
- `target/release/hiren-client` — declarative UI launcher (layer-shell on Wayland, winit elsewhere)

Validate every discovered theme (bundled, user and installed) without opening a window:

```bash
cargo run -p hiren-client --features layer-shell -- --validate-themes
```

The command loads, resolves and renders each theme headlessly, prints per-theme warnings, and exits non-zero if a theme fails.

`make test` runs `cargo test --workspace`; `make check` also runs `--validate-themes`
and a `--screenshot` smoke test headlessly (no compositor or display required).

## Install

```bash
sudo make install              # PREFIX=/usr/local by default
# or
sudo make install PREFIX=/usr

systemctl --user daemon-reload
systemctl --user enable --now hiren-daemon
```

`make install` installs:

| Path | Contents |
|---|---|
| `$(PREFIX)/bin/hiren-daemon`, `hiren-client` | binaries |
| `$(PREFIX)/share/hiren/themes/<name>/` | complete theme directories (theme.toml, `fonts/`, `assets/`, …) |
| `~/.config/systemd/user/hiren-daemon.service` | user unit (installed into the invoking user's home under `sudo`; with `DESTDIR` it is staged under the staging root) |

`DESTDIR` only relocates file paths. It is never embedded in file contents, so a staged unit still has `ExecStart=$(PREFIX)/bin/hiren-daemon`.

`make uninstall` removes the binaries, the unit and the bundled theme directories. User themes under `~/.config/hiren/themes/` are never touched; the target prints the `systemctl --user disable --now` / `daemon-reload` commands to run.

No GTK4 or other heavy runtime is required.

## Usage

```bash
./target/release/hiren-daemon &
./target/release/hiren-client              # theme from config
./target/release/hiren-client --theme atlus
./target/release/hiren-client --list-themes
./target/release/hiren-client --screenshot layered --out /tmp/preview.png
```

### Command line

| Option | Meaning |
|---|---|
| `--theme <name>` | select a theme (built-in or user) for this run |
| `--width <px> --height <px>` | override the window size; both values are required together (a single one is ignored with a warning) and are clamped to `1..=8192` |
| `--list-themes` | print every discoverable theme name |
| `--validate-themes` | load + resolve + render all discovered themes (bundled, user, installed); report warnings/failures |
| `--screenshot [THEME] --out FILE [--query Q] [--settle-ms N]` | headless render to PNG (`--screenshot=THEME` is equivalent). `THEME` is optional (defaults to the config theme); if the next argument starts with `--` it is left alone, so `--screenshot --out x.png` works. `--out` defaults to `/tmp/hiren-<theme>.png`; `--settle-ms` defaults to 900. A missing theme is an error and writes no PNG |
| `--no-layer-shell` | force the winit toplevel backend |
| `--reload` | poll `theme.toml` for changes (development) |
| `--help` | print the built-in help |

`HIREN_DEBUG_NODES=1` (with `--screenshot`) dumps resolved node geometry. Set `RUST_LOG=hiren_client=debug` for frame timings.

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
width = 640                    # clamped to 1..=8192
height = 420                   # clamped to 1..=8192
auto_close_timeout_secs = 8
freq_weight = 0.8              # clamped to 0..=10
keyboard_mode = "exclusive"    # or "on_demand"

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
# The default Ctrl+Return binding derives its prefix from these two values.
# Entries with Terminal=true are launched through the same command.

[[bindings]]
key = "Return"
[[bindings]]
key = "Ctrl+Return"
# prefix = "proxychains"       # optional; overrides the terminal prefix

[theme]
name = "default"               # or atlus, circular, layered, macos, or your own
```

A malformed config no longer discards all settings: each section is parsed
independently, broken sections fall back to defaults with a warning, unknown
keys are warned about, and out-of-range values are clamped. Full reference:
[docs/CONFIG.md](docs/CONFIG.md).

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

Bundled themes:

- `hiren-client/themes/default/theme.toml` — clean vertical launcher (uses the runtime like all others)
- `hiren-client/themes/atlus/theme.toml` — Metaphor-inspired archive menu: cream manuscript plane + navy void, independent selector, search as display type, OFL fonts in `atlus/fonts/`
- `hiren-client/themes/circular/theme.toml` — radial: search centered, results on a circle
- `hiren-client/themes/layered/theme.toml` — layered panels, decorative orbs reacting to selection
- `hiren-client/themes/macos/theme.toml` — calm floating card, spring motion

Theme discovery order:

1. `~/.config/hiren/themes/<name>/` (user themes take precedence)
2. `themes/` next to the client binary
3. `$(PREFIX)/share/hiren/themes/<name>/`
4. `$XDG_DATA_HOME/hiren/themes/<name>/` (usually `~/.local/share`)
5. `$XDG_DATA_DIRS/hiren/themes/<name>/` (`/usr/local/share`, Flatpak exports, …)

A missing requested theme is reported loudly by name (with the search paths);
a normal launch falls back to the built-in theme with a warning, while
`--screenshot <missing>` fails with an error instead of writing a misleading
PNG.

Create your own: copy `hiren-client/themes/default` to
`~/.config/hiren/themes/my/` and edit `theme.toml`. No rebuild needed. See
[docs/CLIENT_ARCHITECTURE.md](docs/CLIENT_ARCHITECTURE.md) for the schema,
bindings, components and animations.

## How it works

```
┌─────────────┐   UNIX socket (0600)   ┌──────────────┐
│ hiren-daemon │ ◄────────────────────► │ hiren-client │
│              │  $XDG_RUNTIME_DIR/     │              │
│ .desktop     │      hiren.socket      │ layer-shell  │
│ parser       │                        │ or winit     │
│ inotify      │                        │ tiny-skia    │
│ watcher      │                        │ UI runtime   │
└─────────────┘                        └──────────────┘
```

The daemon listens on `$XDG_RUNTIME_DIR/hiren.socket` (mode `0600`, peer uid
checked, one instance per user). `HIREN_SOCKET_PATH` overrides the location for
tests and advanced setups; both processes must agree. If `XDG_RUNTIME_DIR` is
unset, the daemon falls back to `$TMPDIR/hiren-<uid>.socket` or
`/tmp/hiren-<uid>.socket`. IPC frames are capped at 256 KiB, queries at 1024
characters, the daemon accepts at most 32 concurrent connections with a 5 s
read/write timeout, and protocol errors (including a version mismatch) are
surfaced in the UI (`launcher.error`) instead of being dropped silently.

Launcher exposes `launcher.query`, `launcher.results`, `launcher.selected_index`, `launcher.error`, etc.; the TOML theme binds visual properties to them.

## License

MIT

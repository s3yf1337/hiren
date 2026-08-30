# Hiren Client — Declarative UI Runtime Architecture

## Overview

`hiren-client` separates **launcher logic** from **visual representation**. The
interface is a TOML scene graph resolved against launcher state; the runtime
contains no hardcoded search bar, result list, or selection widget.

```
┌──────────────────────────────────────────┐
│                Hiren UI                  │
│  TOML scene graph · components           │
│  bindings · animations · actions         │
├──────────────────────────────────────────┤
│             Hiren UI Runtime             │
│  resolve (bindings → nodes · per frame)  │
│  animation (spring/easing · vsync)       │
│  render (tiny-skia + cosmic-text)        │
│  input routing (hit-test → actions)      │
├──────────────────────────────────────────┤
│  Window backends (interchangeable)       │
│  window.rs: winit toplevel (X11/Wayland) │
│  wayland.rs: native wlr-layer-shell      │
├──────────────────────────────────────────┤
│        frontend.rs — AppCore             │
│  keys/clicks → launcher actions          │
├──────────────────────────────────────────┤
│               Hiren Client               │
│  IPC · launcher state · modes · freq     │
├──────────────────────────────────────────┤
│               Hiren Daemon               │
└──────────────────────────────────────────┘
```

Replacing the visual structure requires **no Rust change**.

---

## Technology Choices

| Piece | Crate | Why |
|---|---|---|
| Window (portable) | winit 0.30 + softbuffer 0.4 | X11 + Wayland toplevel, transparent undecorated, CPU surface |
| Window (Wayland-native) | wayland-client 0.31 + wayland-protocols-wlr 0.3 | `zwlr_layer_shell_v1`: overlay layer, exclusive keyboard, compositor-centered |
| Rendering | tiny-skia 0.11 | rects, rounded rects, gradients, shadows, transforms, PNG |
| Text | cosmic-text 0.14 (+fontdb, swash) | shaping, wrapping, measurement, glyph rasterization |
| Theme | TOML + serde | declarative, no custom language |
| Bindings | meval 0.2 (+ a small evaluator in `binding.rs`) | arithmetic/functions; ternaries and comparisons handled by the runtime |
| Keyboard (Wayland) | xkbcommon-dl 0.4 (dlopen) | keymap handling without a hard link-time dependency |

**Rationale:** the DSL stays *small and focused on UI* (nodes, components,
properties, bindings, repeaters, animations, actions) without becoming a
general-purpose language. Rendering reuses proven crates instead of inventing a
shader engine; a `wgpu` backend can replace the CPU rasterizer later without
touching themes.

---

## Boundary: Launcher vs UI

**Launcher owns** (`frontend.rs` `AppCore`, `launcher.rs`, `modes/`):
- IPC to the daemon (`/tmp/hiren.socket`), modes: `drun`, `run`, `window`, `calc`
- `LauncherState { query, results, selected_index, … }`
- Frequency history and hybrid freq-weighted ranking
- Launching: terminal prefix (`Ctrl+Enter`), clipboard for calc results, detached exec
- Window lifecycle: auto-close timeout, theme hot-reload polling

**UI owns** (`ui_runtime/`):
- How state is *represented*: scene graph, layout, animation, rendering
- Input routing: hit-test topmost actionable node → `NodeAction`

**Bridge:** `ObservableState` (interior mutability + subscribers). The runtime
snapshots state each frame; the launcher pushes updates. No UI code knows IPC,
no launcher code knows rectangles.

State exposed to bindings (read-only):

```
launcher.query                     String
launcher.results                   [AppEntry { id, name, exec, description, keywords, mode, score, icon }]
launcher.results_count             usize
launcher.selected_index            usize
launcher.selected_result.name / .exec / .id / .description / .mode / .icon
launcher.loading                   bool
launcher.launching                 bool (true after activate; themes bind this for exit motion)
window.width / window.height       u32 (logical)
time                               f32 (seconds since start)
hit                                f32 (1 → 0 after selection change / open)
hit_type                           f32 (1 → 0 after query change)
since_select / since_type          f32 (seconds since that event)
pi, tau                            constants

Repeater locals: index, count, is_selected, selected_index,
                 item_name, item_exec, item_id, item_description, item_keywords,
                 item_mode, item_icon (resolved icon path, empty if none)
```

Helper functions available inside expressions:
`min(a,b) max(a,b) abs(x) floor(x) ceil(x) round(x) sqrt(x) sin cos tan
clamp(v,lo,hi)`, `mod(a,n)` (Euclidean remainder; `mod(-1, 5)` is `4`),
`hash(n)` (deterministic `0..1`), `shake(amp, seed)` (select-impact
offset), `type_shake(amp, seed)` (query-impact offset),
`text_width(text, font_size)` (measured — usable inside
arithmetic), `initial(text)` (first grapheme, for icon chips).

---

## Theme Format

A theme is a directory with `theme.toml` (plus optional `assets/`):

```toml
[meta]
name = "default"
description = "Polished vertical launcher"

[window]
width = 640            # exact size (logical px)
# width/height may be omitted → falls back to config width/height
transparent = true
time_hz = 60           # optional; vsync for time-driven idle (default ~20 fps)
layer = "overlay"      # layer-shell: background | bottom | top | overlay
anchor = "center"      # layer-shell: center | top | bottom

[[nodes]]
id = "search_bg"
type = "Rectangle"     # Rectangle | Text | Image | Container | Repeater
x = "32"
y = "32"
width = "window.width - 64"
height = "48"
props = { background = "rgba(255,255,255,0.08)", radius = "12" }

[[nodes]]
id = "caret"
type = "Rectangle"
x = "69 + text_width(launcher.query, 16)"   # measured caret position
y = "42"
width = "2"
height = "28"
opacity = "sin(time * 5) > 0 ? 1 : 0.15"    # blink via the time binding

[[nodes]]
id = "results"
type = "Repeater"
x = "32"
y = "96 - clamp(launcher.selected_index - 4, 0, launcher.results_count) * 52"
width = "window.width - 64"
height = "floor((window.height - 32 - 96) / 52) * 52"   # visible list viewport
model = "launcher.results"       # or a static count via props.count
delegate = "result_row"
props = { gap = "8", item_height = "44", layout = "vertical", clip_y = "96" }

[[nodes]]
id = "selector"
type = "Rectangle"
x = "30"
y = "94 + (launcher.selected_index - clamp(launcher.selected_index - 4, 0, launcher.results_count)) * 52"
width = "window.width - 60"
height = "48"
props = { background = "rgba(137,180,250,0.13)", radius = "10", border = "1px rgba(137,180,250,0.22)" }
animate = [{ property = "y", duration_ms = 420, easing = "spring" }]

[components.result_row]
  [[components.result_row.nodes]]
  id = "row_bg"
  type = "Rectangle"
  x = "0" y = "0" width = "576" height = "44"
  props = { background = "is_selected ? rgba(137,180,250,0.10) : transparent", radius = "10" }
  on_click = "activate(index)"
  [[components.result_row.animate]]
  property = "opacity"
  from = 0                  # enter animation: fade in…
  duration_ms = 240
  delay = "index * 30"      # …staggered per row
  easing = "ease_out_cubic"
```

### Node fields

| Field | Type | Notes |
|---|---|---|
| `id`, `type` | string | `Rectangle`, `Text`, `Image`, `Container`, `Repeater` |
| `x`, `y`, `width`, `height` | binding (f32) | free positioning is first-class |
| `visible` | binding (bool) | invisible subtrees are skipped entirely |
| `opacity` | binding (f32) | 0..1, multiplies down the tree |
| `rotation` | binding (deg) | around the node center |
| `scale` | binding (f32) | uniform, around the node center |
| `z` | int | stable z-order sort |
| `text` | binding (string) | `Text` nodes |
| `props` | map of bindings | free-form: `background`, `color`, `font_size`, `font_weight`, `align`, `radius`, `border`, `shadow`, `src`, … |
| `on_click` | action string | see [Actions](#actions) |
| `animate` | array | see [Animations](#animations) |
| `model` / `delegate` | string | `Repeater` |
| `children` | array | nested nodes (containers pass offsets down) |

### Binding expressions

A property value is either a **literal** (`x = 32`, `text = "Firefox"`) or an
**expression string** (`x = "window.width - 64"`). Expressions support:

- arithmetic `+ - * /`, parentheses, `min/max/abs/floor/ceil/round/sqrt/sin/cos/clamp/mod`
- `hash(n)` → `0..1`; `shake(amp, seed)` / `type_shake(amp, seed)` — stepped
  impact offsets gated on `hit` / `hit_type` (full slam, then 42 Hz chaos)
- comparisons `== != < <= > >=`, boolean `&& || !`, ternary `cond ? a : b` (lazy —
  a ternary must be the whole expression; inside larger arithmetic, use `min`/`max`)
- state paths (`launcher.*`, `window.*`, `time`, `hit`, `hit_type`, `since_select`,
  `since_type`), repeater locals
- `text_width(text, size[, family])` — measured width, inlines into arithmetic.
  A comma-separated `family` (`'Anton, Archivo Black, Titan One'`) measures as
  ransom type so plates sized to mixed-font names stay tight.

### Geometry & type primitives

- `skew = "-18"` — horizontal shear (degrees) about the node center; combined with
  `rotation` it turns rectangles into the slanted parallelograms P5 builds its
  lists from. Hit-testing, bounds and the scissor fast path all honor it.
- `type = "Polygon"` + `points = "0,0; 120,8; 120,52; 0,60"` — arbitrary filled
  shape (stars, torn jags, slashes). `;`-separated `x,y` pairs in node-local
  coordinates, each coordinate a binding. Supports `background`/`fill`, `border`,
  `shadow` (hard offset for polygons), transforms and `on_click` (point-in-polygon).
  Recut (hard silhouette change, not vertex morph): stack several `Polygon` nodes
  with different vertex counts and toggle `visible` with
  `mod(launcher.selected_index, n) == i`. Invisible nodes are skipped entirely.
  Event-driven impact (P5): `x = "20 + shake(14, 1)"`, `visible = "hit > 0.35"`
  for slashes that only exist during the cut. Do not loop recut on `time`.
- Text styling: `font_family = "Anton"` (system font **or** a TTF/OTF dropped in
  `themes/<name>/fonts/` — loaded on start and hot-reload; falls back to sans
  when missing), `outline = "3px #000000"` (comic outline, ring blits),
  `text_shadow = "3px 3px #000000"` (hard sticker offset under the outline),
  `wrap = "none"` (single-line; default word-wraps to the node box — a search
  query with a space otherwise drops the rest of the line),
  `text_case = "upper"` (case-fold at layout time), `ransom = "true"` with
  `ransom_fonts = "Anton, Archivo Black, Titan One"` (per-letter mix of family /
  weight / size / rotation / case — the P5 cut-paper look; `text_width` with a
  comma-separated family list uses the same mix).
- `[window] time_hz = 60` — `time` bindings without a running spring normally
  throttle to ~20 fps (caret blink). Sharp caret + impact frames (atlus) set 60.
- String helpers in expressions: `upper(x)` / `lower(x)` alongside `initial(x)`;
  repeater locals include `item_mode` (`drun`/`run`/`calc`/`window`) for
  mode-tag columns.
- Repeater prop `clip_pad = "160"` — widens the fixed scissor band horizontally so
  delegate chrome (tag chips hanging off rows, plate stacks) is not clipped.
- Overlay themes: omit any full-window background node and `window.background` —
  with `transparent = true` the compositor shows the desktop between elements,
  so the surface reads as a shaped overlay instead of a window; give text its
  own black `outline` to stay readable on any wallpaper.
- `initial(text)` — first grapheme (icon chips)
- color literals in props: `#rgb`, `#rrggbb`, `#rrggbbaa`, `rgb()/rgba()`, CSS names, `transparent`
- `background = "linear-gradient(160deg, #1e1e2e, #181825)"` (CSS-style angle)

### Components & repeaters

`[components.<id>]` declares a reusable subtree; a `Repeater` expands its
`delegate` once per item with isolated locals. `Repeater` layout modes:

- `vertical` — flowing list (`item_height` + `gap`)
- `circular` — items placed on a ring: `angle = index/count * tau - pi/2`, `radius`
- `row` — horizontal (delegates position themselves; scissored)
- `free` — no automatic offset, no scissor (decorative scatter; delegates place themselves)

**Virtualization** is built in: instances entirely outside the repeater's
visible band are never materialized, and delegate output is scissored to the
repeater's viewport (`height` + `props.clip_y`), so scrolled lists slide *under*
headers instead of painting over them. Ring/row repeaters over results default
to a 9-item sliding window around the selection (`props.window` overrides).

---

## Selection Is Not Widget State

Selection is exposed as `launcher.selected_index` / `launcher.selected_result`.
The visual highlight is whatever the theme binds to it — usually an independent
`Rectangle` (the *selector*) that can be larger than a row, layered behind
(`z = -1`), spring-animated, or replaced by a ring in a radial theme. Other
objects react too: `preview_name.text = "launcher.selected_result.name"`,
decorative orbs bound to `launcher.selected_index`, etc. There is no built-in
"selected row background".

---

## Actions

`on_click = "<action>"` wires pointer input (parsed once per node, hit-tested
topmost-first with rotation/scale-aware inverse transforms):

```
activate(index)     launch result at index (Enter without index)
select              select this node
select(i)           select index i
move_selection(±1)  relative move
set_query("...")    set the query text
close               close the launcher
```

Keyboard mapping (both backends → `UiKey`/`UiMods` → `AppCore::handle_key`):
characters, `Backspace`, `Escape` (close), `Enter` (activate),
`Ctrl+Enter` (terminal prefix from config bindings), `↑/↓` move,
`Home/End`, `PageUp/PageDown`, `Tab` (completions). Hover over a row selects it.

---

## Animations

Per-node `animate` array; component-level `animate` merges onto every delegate
instance. Properties: `x`, `y`, `width`, `height`, `opacity`, `rotation`, `scale`, `skew`.

```toml
animate = [
  { property = "y", duration_ms = 420, easing = "spring" },
  { property = "opacity", from = 0, duration_ms = 240, delay = "index * 30", easing = "ease_out_cubic" },
  { property = "x", from = "window.width + 60", duration_ms = 500, easing = "ease_out_quart" },
  { property = "scale", duration_ms = 420, easing = "spring", spring = { stiffness = 260, damping = 18, mass = 1 } },
  { property = "scale", from = 1.18, duration_ms = 90, trigger = "select", easing = "ease_out_quad" },
]
```

- `from` — enter-animation start value (number or expression), evaluated at layout
- `delay` — fixed ms or a per-instance expression (`"index * 30"` stagger)
- `trigger` — `"select"` or `"type"` replays `from` → target on that event
  (default: first appearance only)
- `easing` — `linear`, `ease_in/out/in_out_quad`, `…_cubic`, `ease_out_quart`,
  `ease_out_expo`, `ease_out_back`, `ease_out_elastic`, `spring`
- `spring = { stiffness, damping, mass }` — real damped-oscillator integration
  (240 Hz fixed step, stateless), e.g. `170/22/1` default: slight overshoot

**Impact (P5-style, not idle wobble).** `hit` is 1 on open and whenever
`launcher.selected_index` changes, holds ~2 frames, then decays (~200 ms).
`hit_type` does the same for query edits. `shake(amp, seed)` turns `hit` into a
stepped bipolar offset (full-amplitude slam, then 42 Hz quantized chaos);
layers with different `seed`s mis-register. Bind `visible = "hit > 0.35"` for
slashes that only exist during the cut. The window loop ticks while `hit` is
live if the theme binds any of these; otherwise selection does not keep a
theme in motion.

Implementation: `AnimationState` tracks one motion per `node-id:property`;
retargeting starts a transition from the current interpolated value. Frame
pacing: the layer-shell loop presents on the compositor's `wl_surface.frame`
vsync callback (no busy rendering); time-driven themes with nothing else in
motion throttle to ~20 fps unless `[window] time_hz` is set (atlus uses 60 for
caret + impact frames); idle cost is zero when the theme does not bind `time`
and no impulse is decaying.

---

## Wayland & Window Architecture

Two interchangeable backends share `AppCore`; themes don't know which is used.

### Native layer-shell (`wayland.rs`, feature `layer-shell`)

Used automatically when `WAYLAND_DISPLAY` is set and `--no-layer-shell` is not
passed; falls back to winit if the compositor lacks `zwlr_layer_shell_v1`.

- `zwlr_layer_shell_v1` surface: layer from `[window].layer` (default overlay),
  anchored center (or top/bottom strips), `exclusive_zone = -1` for centered
  launchers (float over panels), keyboard interactivity from config
  (`exclusive` default, `on_demand` optional)
- Rendering: two `wl_shm` buffers (anonymous unlinked temp files,
  `ARGB8888` premultiplied — tiny-skia output is premultiplied too, so only
  R/B bytes swap). Double buffering with `wl_buffer.release` tracking.
- Hidpi: integer `wl_surface.set_buffer_scale` (compositor v6+); the frame
  rasterizes at physical size so text stays sharp.
- Input: `xkb` keymap from `wl_keyboard.keymap` (raw FFI via `xkbcommon-dl`),
  pointer motion → hover-select, left click → actions.
- Loop: `poll(2)` on the Wayland fd with a frame-sized timeout while animating;
  present gated on the `wl_surface.frame` callback (vsync); `blocking` when idle.

### winit toplevel (`window.rs`)

Fallback for X11, other platforms, and `--no-layer-shell`. Undecorated,
transparent, always-on-top window; softbuffer presents the same rendered
pixmap. On Wayland the compositor places the window (usually centered).

### Compositor notes

- **wlroots (Sway, Hyprland, river):** full layer-shell support (tested on
  Hyprland 0.56: overlay layer, exclusive keyboard, per-output centering).
- **KWin / Mutter:** layer-shell support varies; the winit fallback covers them.
- **X11:** winit backend (no layer concept; the window floats via WM hints).

---

## Diagnostics & Tooling

```
hiren-client --theme NAME            theme by name (built-in or ~/.config/hiren/themes/)
hiren-client --list-themes           list available themes
hiren-client --validate-themes       load + resolve every theme, print warnings
hiren-client --screenshot [NAME] --out FILE [--query Q] [--settle-ms N]
                                     headless render (demo state) → PNG
hiren-client --width N --height N    override window size
hiren-client --reload                enable theme hot-reload polling
hiren-client --no-layer-shell        force the winit backend
HIREN_DEBUG_NODES=1 (with --screenshot)  dump resolved node geometry
RUST_LOG=hiren_client=debug          per-frame timing logs
```

Theme binding warnings (unknown properties, failed expressions) are collected
during resolve and printed by the running client and `--validate-themes`.

---

## Performance Notes

Measured on the reference theme (640×420, ~180 live results, release build):

- List virtualization keeps the resolved node count at ~56 regardless of
  result count (was ~1100 before virtualization).
- Expression parse cache + text pixmap cache + measurement cache keep a full
  frame at ~8–10 ms (resolve ~1.2 ms, render ~7 ms, present ~0.5 ms).
- While idle (no `time` bindings, no animation) the client wakes only on input.

---

## Known Limitations & Future Work

- **No SVG** — `Image` nodes support PNG (`props.src`, relative to the theme dir).
- **Blur** is a compositor feature; `window.transparent` + translucent
  backgrounds is all the runtime guarantees. Hyprland blur applies to layer surfaces.
- **Fractional scaling** falls back to integer scale (no `wp_viewporter` yet).
- **Text hit-testing** uses node rects (rotation-aware), not per-glyph boxes.
- **One surface per launcher** — multi-surface overlay (shared `ObservableState`)
  is straightforward to add but not enabled.
- **Expression evaluation** is per-frame per-node; caches make it cheap, but a
  dirty-tracking resolver could cut idle cost further for `time`-heavy themes.

---

## File Organization

```
hiren-client/
├── Cargo.toml
├── src/
│   ├── main.rs               # args, backend selection, screenshot/validate modes
│   ├── config.rs             # LauncherConfig + theme discovery
│   ├── frontend.rs           # AppCore: keys/clicks → actions → state/launch
│   ├── freq.rs               # history.json freq boost
│   ├── ipc.rs                # UNIX socket to daemon
│   ├── launcher.rs           # LauncherState + ObservableState
│   ├── modes/                # drun/run/window/calc
│   ├── window.rs             # winit + softbuffer backend
│   ├── wayland.rs            # native layer-shell backend (feature "layer-shell")
│   └── ui_runtime/
│       ├── mod.rs            # UiRuntime: resolve → animate → render
│       ├── theme.rs          # TOML schema + validation
│       ├── binding.rs        # expression evaluator + caches
│       ├── layout.rs         # resolve: nodes, repeaters, components, virtualization
│       ├── animation.rs      # springs/easings, AnimationState
│       ├── node.rs           # ResolvedNode + hit-test
│       ├── render.rs         # tiny-skia renderer (shadows, gradients, scissor)
│       ├── text.rs           # cosmic-text engine + caches
│       ├── color.rs          # color parsing
│       └── state_bridge.rs   # SearchBridge (modes → ObservableState)
└── themes/
    ├── default/  atlus/  macos/  layered/  circular/   # theme.toml (+ atlus/fonts)
```

User themes: `~/.config/hiren/themes/<name>/theme.toml`, selected via
`~/.config/hiren/config.toml` (`theme = "my"`) or `--theme my`.

---

## How to Create a Theme

1. Copy a theme: `cp -r themes/default ~/.config/hiren/themes/my`
2. Edit `~/.config/hiren/themes/my/theme.toml` — window size, colors, layout,
   selector binding, animations. Everything is hot-editable; `--reload` re-reads
   the file every frame while running.
3. Check: `hiren-client --validate-themes`, preview: `hiren-client --screenshot my --out /tmp/my.png`
4. Run: `hiren-client --theme my`. No Rust rebuild needed.

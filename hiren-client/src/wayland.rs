//! Wayland backend: native `zwlr_layer_shell_v1` layer surface.
//!
//! This is the launcher-native Wayland frontend. A layer surface is the
//! architecturally correct surface type for a launcher: compositor-positioned
//! (centered, or anchored top/bottom by the theme), unaffected by other
//! layers' reserved space, and given keyboard focus without a focus race.
//! Rendering feeds premultiplied ARGB8888 `wl_shm` buffers from tiny-skia;
//! keyboard mapping goes through `libxkbcommon` (dlopen via `xkbcommon-dl`).
//!
//! Backend selection happens in `main`: this backend is used automatically
//! when compiled with the `layer-shell` feature and `WAYLAND_DISPLAY` is set;
//! the winit toplevel backend (`window.rs`) remains the fallback (X11,
//! non-wlroots compositors, `--no-layer-shell`).
//!
//! Compositor notes (see docs/CLIENT_ARCHITECTURE.md):
//!   - wlroots compositors (Sway, Hyprland, river): full support.
//!   - KWin/Mutter: layer-shell support varies; they may treat the surface
//!     as a normal toplevel or ignore the keyboard grab. Fallback covers this.
//!   - X11: not handled here (winit backend instead).
//!
//! Frame pacing: the main loop is `blocking_dispatch` — an idle launcher
//! costs zero CPU. While the core reports it needs frames (animations or a
//! `time`-driven theme), a small ticker thread sends `wl_display.sync` every
//! ~16 ms, each callback waking the queue for one render.

#![cfg(feature = "layer-shell")]

use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::time::Instant;

use anyhow::{anyhow, Result};
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry,
    wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use xkbcommon_dl::{
    self as xkb, keysyms, xkbcommon_handle, xkb_context_flags, xkb_keymap_format,
    xkb_state_component, XkbCommon,
};

use crate::frontend::{AppCore, Outcome, UiKey, UiMods};

const FRAME_MS: i32 = 16;
const IDLE_TICK_MS: i32 = 250;
const BTN_LEFT: u32 = 0x110;

/// Raw libxkbcommon function table (dlopen'd once).
fn xh() -> &'static XkbCommon {
    xkbcommon_handle()
}

pub fn run(core: AppCore) -> Result<()> {
    let conn = Connection::connect_to_env().map_err(|e| anyhow!("Wayland connect: {e}"))?;
    let mut queue = conn.new_event_queue::<AppState>();
    let qh = queue.handle();

    let mut state = AppState {
        core,
        registry: None,
        globals: Vec::new(),
        compositor: None,
        compositor_version: 0,
        shm: None,
        layer_shell: None,
        layer: 3,
        anchor: String::from("center"),
        keyboard_mode: 1,
        scale: 1.0,
        surface: None,
        layer_surface: None,
        configured: None,
        buffers: None,
        next_buffer: 0,
        pointer: PointerState::default(),
        xkb: Xkb::new(),
        dirty: true,
        frame_arrived: true, // draw the first frame immediately
        done: false,
        last_frame: None,
        last_draw: None,
    };

    // Discover globals, bind what we need, learn output scale + keymap.
    conn.display().get_registry(&qh, ());
    queue.roundtrip(&mut state)?;
    state.read_theme_window();
    state.bind_globals(&qh)?;
    queue.roundtrip(&mut state)?;

    // Create the layer surface and wait for the first configure.
    state.create_surface(&qh)?;
    queue.roundtrip(&mut state)?;
    if state.configured.is_none() {
        anyhow::bail!("compositor did not configure the layer surface");
    }

    // Main loop: poll(2) on the Wayland socket with a frame-sized timeout.
    // While the UI is animating (or the theme uses `time`) we wake every
    // FRAME_MS; when idle we block forever at zero CPU until an event arrives.
    // `process_frame` draws when the core asks for frames or input made the
    // scene dirty, and reports the measured frame time in debug logs.
    state.process_frame(&qh);
    let _ = conn.flush();
    while !state.done {
        let timeout = state.frame_timeout_ms();
        wait_readable(&conn, &queue, timeout)?;
        queue.dispatch_pending(&mut state)?;
        state.process_frame(&qh);
        // Push commits/acks to the compositor before sleeping — dispatch does
        // NOT flush, and without this the compositor never sees our frames
        // (and never sends wl_buffer.release, deadlocking double buffering).
        if let Err(e) = conn.flush() {
            // WouldBlock just means "partially written, retry next iteration".
            use wayland_client::backend::WaylandError;
            match e {
                WaylandError::Io(io) if io.kind() == std::io::ErrorKind::WouldBlock => {}
                other => return Err(anyhow!("wayland flush: {other}")),
            }
        }
    }
    Ok(())
}

/// Block until the Wayland socket has data (or the timeout expires), then read
/// the pending messages into their queues without dispatching them.
fn wait_readable(conn: &Connection, queue: &EventQueue<AppState>, timeout_ms: i32) -> Result<()> {
    let fd = queue.as_fd().as_raw_fd();
    let mut fds = [libc::pollfd { fd, events: libc::POLLIN, revents: 0 }];
    let n = unsafe { libc::poll(fds.as_mut_ptr(), 1, timeout_ms) };
    if n < 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
            return Ok(()); // EINTR: dispatch whatever we have next iteration
        }
        return Err(err.into());
    }
    if n > 0 && (fds[0].revents & libc::POLLIN) != 0 {
        if let Some(guard) = conn.prepare_read() {
            guard.read().map_err(|e| anyhow!("wayland read: {e}"))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct AppState {
    core: AppCore,
    registry: Option<wl_registry::WlRegistry>,
    /// Globals seen by the registry: (name, interface, version).
    globals: Vec<(u32, String, u32)>,
    compositor: Option<wl_compositor::WlCompositor>,
    compositor_version: u32,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    /// zwlr layer number (0 background .. 3 overlay), from the theme.
    layer: u32,
    /// Theme anchor: "center" | "top" | "bottom".
    anchor: String,
    /// zwlr keyboard interactivity: 1 exclusive, 2 on-demand.
    keyboard_mode: u32,
    /// Output scale (integer; fractional scaling is not attempted without
    /// wp_viewporter — fallback stays sharp enough at integer scales).
    scale: f32,
    surface: Option<wl_surface::WlSurface>,
    layer_surface: Option<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1>,
    /// Logical size last configured by the compositor (0 = client-decided).
    configured: Option<(u32, u32)>,
    buffers: Option<Buffers>,
    next_buffer: usize,
    pointer: PointerState,
    xkb: Xkb,
    /// A repaint is pending even though no animation is running.
    dirty: bool,
    /// A `wl_surface.frame` callback fired: the compositor says it's a good
    /// time to present (vsync pacing — the loop never renders faster).
    frame_arrived: bool,
    done: bool,
    /// When the previous frame was drawn (frame-time diagnostics).
    last_frame: Option<Instant>,
    /// When the previous frame was PRESENTED (throttle gate for time-driven
    /// themes with nothing else in motion).
    last_draw: Option<Instant>,
}

#[derive(Default)]
struct PointerState {
    x: f64,
    y: f64,
    inside: bool,
}

impl AppState {
    /// Window placement from the theme (`[window]`) and config.
    fn read_theme_window(&mut self) {
        let win = &self.core.runtime.theme().window;
        self.layer = match win.layer.as_str() {
            "background" => 0,
            "bottom" => 1,
            "top" => 2,
            _ => 3, // overlay
        };
        self.anchor = win.anchor.clone();
        self.keyboard_mode = match self.core.config.keyboard_mode {
            crate::config::KeyboardModeConfig::OnDemand => 2,
            crate::config::KeyboardModeConfig::Exclusive => 1,
        };
    }

    fn bind_globals(&mut self, qh: &QueueHandle<AppState>) -> Result<()> {
        let globals = std::mem::take(&mut self.globals);
        let registry = self.registry.clone().ok_or_else(|| anyhow!("no wl_registry"))?;
        for (name, interface, version) in globals {
            match interface.as_str() {
                "wl_compositor" => {
                    self.compositor_version = version.min(6);
                    self.compositor = Some(registry.bind(name, version.min(6), qh, ()));
                }
                "wl_shm" => {
                    self.shm = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "zwlr_layer_shell_v1" => {
                    self.layer_shell = Some(registry.bind(name, version.min(4), qh, ()));
                }
                "wl_seat" => {
                    // Keep seat version low enough that get_keyboard/get_pointer stay legal.
                    registry.bind::<wl_seat::WlSeat, (), AppState>(name, version.min(9), qh, ());
                }
                "wl_output" => {
                    registry.bind::<wl_output::WlOutput, u32, AppState>(name, version.min(4), qh, name);
                }
                _ => {}
            }
        }
        if self.compositor.is_none() {
            anyhow::bail!("compositor lacks wl_compositor");
        }
        if self.shm.is_none() {
            anyhow::bail!("compositor lacks wl_shm");
        }
        if self.layer_shell.is_none() {
            anyhow::bail!(
                "compositor does not support zwlr_layer_shell_v1 (is it a wlroots compositor?)"
            );
        }
        Ok(())
    }

    fn create_surface(&mut self, qh: &QueueHandle<AppState>) -> Result<()> {
        let compositor = self.compositor.as_ref().ok_or_else(|| anyhow!("no compositor"))?;
        let shell = self.layer_shell.as_ref().ok_or_else(|| anyhow!("no layer shell"))?;
        let surface = compositor.create_surface(qh, ());

        let layer = match self.layer {
            0 => zwlr_layer_shell_v1::Layer::Background,
            1 => zwlr_layer_shell_v1::Layer::Bottom,
            2 => zwlr_layer_shell_v1::Layer::Top,
            _ => zwlr_layer_shell_v1::Layer::Overlay,
        };
        let ls = shell.get_layer_surface(&surface, None, layer, String::from("hiren"), qh, ());

        // Centered launchers anchor to nothing (the compositor centers them);
        // top/bottom strips anchor to an edge and fill its width.
        let anchored = match self.anchor.as_str() {
            "top" => Some(zwlr_layer_surface_v1::Anchor::Top),
            "bottom" => Some(zwlr_layer_surface_v1::Anchor::Bottom),
            _ => None,
        };
        match anchored {
            Some(edge) => {
                ls.set_anchor(edge | zwlr_layer_surface_v1::Anchor::Left | zwlr_layer_surface_v1::Anchor::Right);
                // Respect other layers' exclusive zones (slide below/above panels).
                ls.set_exclusive_zone(0);
            }
            None => {
                ls.set_anchor(zwlr_layer_surface_v1::Anchor::empty());
                // Float over everything: ignore panels' reserved space.
                ls.set_exclusive_zone(-1);
            }
        }
        let (w, h) = self.core.size();
        ls.set_size(w, h);
        ls.set_keyboard_interactivity(match self.keyboard_mode {
            2 => zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand,
            _ => zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive,
        });
        // Integer buffer scale for hidpi outputs (wl_surface.set_buffer_scale
        // exists since wl_surface v6, shipped by wl_compositor v6).
        if self.compositor_version >= 6 && self.scale > 1.0 {
            surface.set_buffer_scale(self.scale.round() as i32);
        }
        surface.commit();

        self.surface = Some(surface);
        self.layer_surface = Some(ls);
        Ok(())
    }

    /// Effective integer buffer scale (1.0 when the surface can't scale).
    fn effective_scale(&self) -> f32 {
        if self.compositor_version >= 6 {
            self.scale.max(1.0)
        } else {
            1.0
        }
    }

    /// Logical surface size: compositor-configured when it chose one, else the
    /// theme size.
    fn logical_size(&self) -> (u32, u32) {
        match self.configured {
            Some((cw, ch)) if cw > 0 && ch > 0 => (cw, ch),
            _ => self.core.size(),
        }
    }

    /// (Re)create the wl_shm buffer pool when missing or when the pixel size
    /// changed.
    fn ensure_buffers(&mut self, qh: &QueueHandle<AppState>) {
        let (w, h) = self.logical_size();
        let scale = self.effective_scale();
        let pw = (w as f32 * scale).round().max(1.0) as i32;
        let ph = (h as f32 * scale).round().max(1.0) as i32;
        if let Some(b) = &self.buffers {
            if b.pw == pw && b.ph == ph {
                return;
            }
        }
        let Some(shm) = self.shm.clone() else { return };
        let stride = pw * 4;
        let size = stride * ph;

        // One anonymous temp file + pool + buffer per slot; each file is
        // unlinked immediately so nothing persists on disk.
        let make = |tag: u32| -> Result<ShmBuffer> {
            let mut path = std::env::temp_dir();
            path.push(format!("hiren-shm-{}-{}", std::process::id(), tag));
            let file = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&path)?;
            let _ = std::fs::remove_file(&path);
            file.set_len(size as u64)?;
            let pool = shm.create_pool(file.as_fd(), size, qh, ());
            let buffer =
                pool.create_buffer(0, pw, ph, stride, wl_shm::Format::Argb8888, qh, ());
            Ok(ShmBuffer { file, pool, buffer, busy: false })
        };

        match (make(0), make(1)) {
            (Ok(a), Ok(b)) => {
                self.buffers = Some(Buffers { bufs: [a, b], stride, pw, ph });
            }
            (Err(e), _) | (_, Err(e)) => {
                log::warn!("hiren: shm buffer setup failed: {e}");
            }
        }
    }

    fn draw(&mut self, qh: &QueueHandle<AppState>) {
        self.ensure_buffers(qh);
        let scale = self.effective_scale();
        let idx = self.next_buffer;
        {
            let Some(buffers) = self.buffers.as_ref() else {
                eprintln!("[hiren-wl] draw: no buffers");
                return;
            };
            if buffers.bufs[idx].busy {
                // Compositor still reads this buffer; try again next poll tick.
                return;
            }
        }

        // Resolve + render at physical pixel size (nodes carry the scale in
        // their transforms, so hidpi outputs get sharp text).
        let (pw, ph) = {
            let buffers = self.buffers.as_ref().unwrap();
            (buffers.pw.max(1) as u32, buffers.ph.max(1) as u32)
        };
        let t0 = Instant::now();
        let nodes = self.core.render_frame(scale).to_vec();
        let t1 = Instant::now();
        let pixmap = self.core.runtime.render_nodes(&nodes, (pw, ph), scale);
        let t2 = Instant::now();

        // tiny-skia produces premultiplied RGBA bytes; wl_shm Argb8888 on
        // little-endian is bytes B,G,R,A — also premultiplied. Swap R<->B as
        // u32 words (4x fewer iterations than byte shuffling).
        let px = pixmap.data();
        let mut out = Vec::with_capacity(px.len());
        let mut word = [0u8; 4];
        for chunk in px.chunks_exact(4) {
            word.copy_from_slice(chunk);
            let v = u32::from_le_bytes(word);
            let swizzled =
                (v & 0xFF00_FF00) | ((v & 0x0000_00FF) << 16) | ((v >> 16) & 0x0000_00FF);
            out.extend_from_slice(&swizzled.to_le_bytes());
        }

        let Some(surface) = self.surface.clone() else { return };
        {
            let buffers = self.buffers.as_mut().unwrap();
            let buf = &mut buffers.bufs[idx];
            if buf.file.seek(SeekFrom::Start(0)).and_then(|_| buf.file.write_all(&out)).is_err() {
                log::warn!("hiren: shm buffer write failed");
                return;
            }
            let _ = buf.file.flush();
            buf.busy = true;
            // Arm the vsync callback BEFORE the presenting commit: the loop
            // then draws at most once per compositor frame.
            surface.frame(qh, ());
            surface.attach(Some(&buf.buffer), 0, 0);
        }
        surface.damage(0, 0, i32::MAX, i32::MAX);
        surface.commit();
        self.next_buffer = 1 - idx;
        self.frame_arrived = false;
        self.last_draw = Some(Instant::now());
        if log::log_enabled!(log::Level::Debug) {
            let now = Instant::now();
            let dt = self.last_frame.map(|t| now.duration_since(t).as_millis()).unwrap_or(0);
            log::debug!(
                "frame {}x{} scale={scale} nodes={} resolve={}us render={}us convert={}us {}ms since previous",
                pw,
                ph,
                nodes.len(),
                (t1 - t0).as_micros(),
                (t2 - t1).as_micros(),
                now.duration_since(t2).as_micros(),
                dt
            );
            self.last_frame = Some(now);
        }
    }

    /// Poll timeout for the next loop iteration. Real pacing comes from the
    /// `wl_surface.frame` vsync callback; these timeouts are only safety
    /// nets (e.g. when the surface is occluded and callbacks pause).
    fn frame_timeout_ms(&self) -> i32 {
        // Continuous animations get vsync-ish pacing; a `time`-driven theme
        // with nothing else in motion (caret blink, ambient drift) is throttled
        // to ~20 fps — plenty for slow effects, a fifth of the CPU.
        let animating = self.core.runtime.animating();
        let nf = self.core.needs_frame();
        if std::env::var_os("HIREN_LOOP_DEBUG").is_some() {
            eprintln!(
                "[loop] animating={} uses_time={} dirty={} frame_arrived={}",
                animating,
                self.core.last_uses_time_debug(),
                self.dirty,
                self.frame_arrived,
            );
        }
        if animating {
            FRAME_MS
        } else if self.core.last_uses_time_debug() {
            50
        } else if nf || self.dirty {
            0
        } else if self.core.auto_close_pending() {
            IDLE_TICK_MS
        } else {
            -1
        }
    }

    /// After each poll/dispatch cycle: redraw when the UI needs it. Continuous
    /// animation frames wait for the vsync callback; input-driven repaints
    /// (dirty) go out immediately for minimal latency. A `time`-driven theme
    /// with no animation in motion is rate-limited to ~20 fps — the effects it
    /// drives (caret blink, ambient drift) are far slower than that.
    fn process_frame(&mut self, qh: &QueueHandle<AppState>) {
        if self.done {
            return;
        }
        if self.core.auto_close_expired() {
            log::info!("hiren: auto-close timeout");
            self.done = true;
            return;
        }
        let animating = self.core.runtime.animating();
        let throttle_ms = if animating || self.dirty {
            0
        } else if self.core.last_uses_time_debug() {
            45
        } else {
            0
        };
        let due = self
            .last_draw
            .map(|t| t.elapsed().as_millis() as i32 >= throttle_ms)
            .unwrap_or(true);
        if self.dirty || (self.frame_arrived && due && self.core.needs_frame()) {
            self.dirty = false;
            self.draw(qh);
        }
    }
}

struct ShmBuffer {
    file: std::fs::File,
    /// Kept alive: the buffer references its pool's memory.
    #[allow(dead_code)]
    pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
    busy: bool,
}

struct Buffers {
    bufs: [ShmBuffer; 2],
    /// Row stride in bytes (kept for buffer (re)creation diagnostics).
    #[allow(dead_code)]
    stride: i32,
    pw: i32,
    ph: i32,
}

// ---------------------------------------------------------------------------
// xkb (raw FFI through xkbcommon-dl)
// ---------------------------------------------------------------------------

struct Xkb {
    ctx: *mut xkb::xkb_context,
    keymap: *mut xkb::xkb_keymap,
    state: *mut xkb::xkb_state,
    mod_ctrl: u32,
    mod_shift: u32,
    mod_alt: u32,
    mod_super: u32,
}

impl Xkb {
    fn new() -> Self {
        Self {
            ctx: std::ptr::null_mut(),
            keymap: std::ptr::null_mut(),
            state: std::ptr::null_mut(),
            mod_ctrl: u32::MAX,
            mod_shift: u32::MAX,
            mod_alt: u32::MAX,
            mod_super: u32::MAX,
        }
    }

    fn load_keymap(&mut self, data: &[u8]) {
        unsafe {
            let xh = xh();
            if self.ctx.is_null() {
                self.ctx = (xh.xkb_context_new)(xkb_context_flags::XKB_CONTEXT_NO_FLAGS);
            }
            if self.ctx.is_null() {
                log::warn!("hiren: xkb_context_new failed");
                return;
            }
            if !self.keymap.is_null() {
                (xh.xkb_keymap_unref)(self.keymap);
            }
            self.keymap = (xh.xkb_keymap_new_from_buffer)(
                self.ctx,
                data.as_ptr() as *const std::os::raw::c_char,
                data.len(),
                xkb_keymap_format::XKB_KEYMAP_FORMAT_TEXT_V1,
                xkb::xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS,
            );
            if self.keymap.is_null() {
                log::warn!("hiren: xkb keymap compile failed");
                self.state = std::ptr::null_mut();
                return;
            }
            log::debug!(
                "xkb: keymap loaded ({} bytes), min_keycode={} max_keycode={}, num_layouts={}",
                data.len(),
                (xh.xkb_keymap_min_keycode)(self.keymap),
                (xh.xkb_keymap_max_keycode)(self.keymap),
                (xh.xkb_keymap_num_layouts)(self.keymap),
            );
            if !self.state.is_null() {
                (xh.xkb_state_unref)(self.state);
            }
            self.state = (xh.xkb_state_new)(self.keymap);
            let cstr = |s: &[u8]| s.as_ptr() as *const std::os::raw::c_char;
            self.mod_ctrl = (xh.xkb_keymap_mod_get_index)(self.keymap, cstr(b"Control\0"));
            self.mod_shift = (xh.xkb_keymap_mod_get_index)(self.keymap, cstr(b"Shift\0"));
            self.mod_alt = (xh.xkb_keymap_mod_get_index)(self.keymap, cstr(b"Mod1\0"));
            self.mod_super = (xh.xkb_keymap_mod_get_index)(self.keymap, cstr(b"Mod4\0"));
        }
    }

    /// Apply compositor modifier state. `group` is the active layout index and
    /// belongs in the *locked layout* slot (same convention as wlroots and
    /// smithay-client-toolkit).
    fn update_mask(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        if self.state.is_null() {
            return;
        }
        unsafe {
            (xh().xkb_state_update_mask)(self.state, depressed, latched, locked, 0, 0, group);
        }
    }

    fn mods(&self) -> UiMods {
        if self.state.is_null() {
            return UiMods::default();
        }
        let active = |idx: u32| -> bool {
            if idx == u32::MAX {
                return false;
            }
            unsafe {
                (xh().xkb_state_mod_index_is_active)(
                    self.state,
                    idx,
                    xkb_state_component::XKB_STATE_MODS_EFFECTIVE,
                ) == 1
            }
        };
        UiMods {
            ctrl: active(self.mod_ctrl),
            alt: active(self.mod_alt),
            super_key: active(self.mod_super),
            shift: active(self.mod_shift),
        }
    }

    /// Map a key event (evdev keycode) to a UiKey.
    fn interpret(&self, keycode: u32) -> Option<UiKey> {
        if self.state.is_null() {
            return None;
        }
        let kc = keycode + 8; // evdev → xkb keycode offset
        let ks = unsafe { (xh().xkb_state_key_get_one_sym)(self.state, kc) };
        let named = match ks {
            x if x == keysyms::Escape => Some(UiKey::Escape),
            x if x == keysyms::Return || x == keysyms::KP_Enter => Some(UiKey::Enter),
            x if x == keysyms::BackSpace => Some(UiKey::Backspace),
            x if x == keysyms::Down || x == keysyms::KP_Down => Some(UiKey::ArrowDown),
            x if x == keysyms::Up || x == keysyms::KP_Up => Some(UiKey::ArrowUp),
            x if x == keysyms::Home => Some(UiKey::Home),
            x if x == keysyms::End => Some(UiKey::End),
            x if x == keysyms::Prior => Some(UiKey::PageUp),
            x if x == keysyms::Next => Some(UiKey::PageDown),
            x if x == keysyms::ISO_Left_Tab => Some(UiKey::BackTab),
            x if x == keysyms::Tab => Some(UiKey::Tab),
            _ => None,
        };
        if named.is_some() {
            return named;
        }
        // Text input from the key event.
        let mut buf = [0u8; 32];
        let n = unsafe {
            (xh().xkb_state_key_get_utf8)(
                self.state,
                kc,
                buf.as_mut_ptr() as *mut std::os::raw::c_char,
                buf.len(),
            )
        };
        if n > 0 {
            let end = (n as usize).min(buf.len());
            let s = std::str::from_utf8(&buf[..end]).unwrap_or("");
            let s: String = s.chars().filter(|c| !c.is_control()).collect();
            if !s.is_empty() {
                return Some(UiKey::Char(s));
            }
        }
        None
    }
}

impl Drop for Xkb {
    fn drop(&mut self) {
        unsafe {
            let xh = xh();
            if !self.state.is_null() {
                (xh.xkb_state_unref)(self.state);
            }
            if !self.keymap.is_null() {
                (xh.xkb_keymap_unref)(self.keymap);
            }
            if !self.ctx.is_null() {
                (xh.xkb_context_unref)(self.ctx);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Wayland Dispatch impls
// ---------------------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, ()> for AppState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if state.registry.is_none() {
            state.registry = Some(registry.clone());
        }
        if let wl_registry::Event::Global { name, interface, version } = event {
            state.globals.push((name, interface, version));
        }
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &wl_compositor::WlCompositor,
        _: wl_compositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm::WlShm, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &wl_shm::WlShm,
        _: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// WlShmPool has no events; the (empty, non_exhaustive) Event type just
// carries the interface.
impl Dispatch<wl_shm_pool::WlShmPool, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        _: wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for AppState {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(caps) } = event {
            if caps.contains(wl_seat::Capability::Keyboard) {
                seat.get_keyboard(qh, ());
            }
            if caps.contains(wl_seat::Capability::Pointer) {
                seat.get_pointer(qh, ());
            }
        }
        let _ = state;
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for AppState {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap { format: WEnum::Value(format), fd, size } => {
                log::debug!("keyboard: keymap event, format={format:?} size={size}");
                // The fd is delivered as an owned handle; read the keymap text
                // and drop it (closing the fd).
                let mut file = std::fs::File::from(fd);
                if format == wl_keyboard::KeymapFormat::XkbV1 && size > 0 {
                    // The received fd shares its file offset with the
                    // compositor's end, which sits *after* the written keymap
                    // — a raw read would see EOF (0 bytes). Rewind to 0 first,
                    // then read to EOF: the advertised `size` may not match
                    // the fd's readable content exactly.
                    let mut buf = Vec::with_capacity(size as usize);
                    match file
                        .seek(SeekFrom::Start(0))
                        .and_then(|_| file.read_to_end(&mut buf))
                    {
                        Ok(_) => {
                            log::debug!("keyboard: keymap read {} bytes (advertised {size})", buf.len());
                            state.xkb.load_keymap(&buf);
                        }
                        Err(e) => log::warn!("hiren: failed to read keymap fd (size={size}): {e}"),
                    }
                }
            }
            wl_keyboard::Event::Modifiers { mods_depressed, mods_latched, mods_locked, group, .. } => {
                log::debug!("keyboard: modifiers dep={mods_depressed} lat={mods_latched} lock={mods_locked} group={group}");
                state.xkb.update_mask(mods_depressed, mods_latched, mods_locked, group);
            }
            wl_keyboard::Event::Key { key, state: WEnum::Value(kstate), .. } => {
                log::debug!("keyboard: key event code={key} state={kstate:?}");
                if kstate == wl_keyboard::KeyState::Pressed {
                    match state.xkb.interpret(key) {
                        Some(key) => {
                            log::debug!("keyboard: interpreted as {key:?}");
                            let mods = state.xkb.mods();
                            let outcome = state.core.handle_key(key, mods);
                            state.dirty = true;
                            if outcome == Outcome::Exit {
                                state.done = true;
                            }
                        }
                        None => log::debug!("keyboard: key code={key} produced NO keysym"),
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for AppState {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter { surface_x, surface_y, .. } => {
                state.pointer.inside = true;
                state.pointer.x = surface_x;
                state.pointer.y = surface_y;
                state.core.handle_hover(surface_x, surface_y);
            }
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                state.pointer.x = surface_x;
                state.pointer.y = surface_y;
                state.core.handle_hover(surface_x, surface_y);
            }
            wl_pointer::Event::Button { button, state: WEnum::Value(bstate), .. } => {
                if bstate == wl_pointer::ButtonState::Pressed && button == BTN_LEFT {
                    let (x, y) = (state.pointer.x, state.pointer.y);
                    let mods = state.xkb.mods();
                    let outcome = state.core.handle_click(x, y, mods);
                    state.dirty = true;
                    if outcome == Outcome::Exit {
                        state.done = true;
                    }
                }
            }
            wl_pointer::Event::Leave { .. } => {
                state.pointer.inside = false;
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for AppState {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The surface frame callback: compositor says "present now" (vsync).
        state.frame_arrived = true;
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for AppState {
    fn event(
        state: &mut Self,
        buffer: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release {} = event {
            if let Some(buffers) = state.buffers.as_mut() {
                for b in buffers.bufs.iter_mut() {
                    if b.buffer.id() == buffer.id() {
                        b.busy = false;
                    }
                }
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for AppState {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Scale { factor } = event {
            if factor as f32 > state.scale {
                state.scale = factor as f32;
            }
        }
    }
}

impl Dispatch<zwlr_layer_shell_v1::ZwlrLayerShellV1, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &zwlr_layer_shell_v1::ZwlrLayerShellV1,
        _: zwlr_layer_shell_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for AppState {
    fn event(
        state: &mut Self,
        surface: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                surface.ack_configure(serial);
                if state.configured != Some((width, height)) {
                    state.configured = Some((width, height));
                    // Adopt the configured logical size for layout.
                    if width > 0 && height > 0 {
                        state.core.set_size(width, height);
                    }
                    state.dirty = true;
                }
            }
            zwlr_layer_surface_v1::Event::Closed {} => {
                state.done = true;
            }
            _ => {}
        }
    }
}

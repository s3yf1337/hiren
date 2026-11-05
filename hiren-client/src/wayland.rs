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

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry,
    wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use xkbcommon_dl::{
    self as xkb, keysyms, xkbcommon_option, xkb_context_flags, xkb_keymap_format,
    xkb_state_component, XkbCommon,
};

use crate::frontend::{AppCore, Outcome, UiKey, UiMods};

const FRAME_MS: i32 = 16;
const IDLE_TICK_MS: i32 = 250;
const BTN_LEFT: u32 = 0x110;

/// Raw libxkbcommon function table (dlopen'd once).
///
/// `None` when the library is not installed: the caller then falls back to the
/// winit backend instead of panicking (previous behavior was `expect()` inside
/// `xkbcommon_handle`).
fn xh() -> Option<&'static XkbCommon> {
    xkbcommon_option()
}

pub fn run(core: AppCore) -> Result<()> {
    if xh().is_none() {
        anyhow::bail!(
            "libxkbcommon.so is not available; cannot map keys on the layer-shell backend — \
             use the winit backend (--no-layer-shell)"
        );
    }
    let conn = Connection::connect_to_env().map_err(|e| anyhow!("Wayland connect: {e}"))?;
    let mut queue = conn.new_event_queue::<AppState>();
    let qh = queue.handle();
    let mut state = AppState::new(core);

    let result = run_loop(&conn, &mut queue, &mut state, &qh);

    // wayland-client 0.31 does not destroy proxies on Drop: release the
    // protocol objects explicitly (layer surface, wl_surface, buffers,
    // keyboard/pointer/seat/outputs) and flush those requests before the
    // connection closes. `Drop for AppState` also runs as a safety net.
    drop(state);
    let _ = conn.flush();
    result
}

fn run_loop(
    conn: &Connection,
    queue: &mut EventQueue<AppState>,
    state: &mut AppState,
    qh: &QueueHandle<AppState>,
) -> Result<()> {
    // Discover globals, bind what we need, learn output scale + keymap.
    conn.display().get_registry(qh, ());
    queue.roundtrip(state)?;
    state.read_theme_window();
    state.bind_globals(qh)?;
    queue.roundtrip(state)?;

    // Create the layer surface and wait for the first configure.
    state.create_surface(qh)?;
    queue.roundtrip(state)?;
    if state.configured.is_none() {
        anyhow::bail!("compositor did not configure the layer surface");
    }

    // Main loop: poll(2) on the Wayland socket with a frame-sized timeout.
    // While the UI is animating (or the theme uses `time`) we wake every
    // FRAME_MS; when idle we block forever at zero CPU until an event arrives.
    state.process_frame(qh);
    flush_connection(conn)?;
    while !state.done {
        let timeout = state.poll_timeout_ms();
        match wait_readable(conn, queue, timeout)? {
            WaitStatus::Hangup => {
                // Compositor gone / socket closed: do not busy-spin on POLLHUP.
                return Err(anyhow!("compositor closed the Wayland connection"));
            }
            WaitStatus::Readable | WaitStatus::Timeout => {}
        }
        queue.dispatch_pending(state)?;
        if let Some(fatal) = state.fatal.take() {
            return Err(anyhow!(fatal));
        }
        if state.done {
            break;
        }
        // Synthesize keyboard auto-repeat before drawing: Wayland compositors
        // do not repeat keys client-side (only xkb RepeatInfo is advertised).
        if let Some(keycode) = state.repeat.poll(Instant::now()) {
            state.handle_pressed_key(keycode);
        }
        state.process_frame(qh);
        // Push commits/acks to the compositor before sleeping — dispatch does
        // NOT flush, and without this the compositor never sees our frames
        // (and never sends wl_buffer.release, deadlocking double buffering).
        flush_connection(conn)?;
    }
    Ok(())
}

fn flush_connection(conn: &Connection) -> Result<()> {
    use wayland_client::backend::WaylandError;
    if let Err(e) = conn.flush() {
        // WouldBlock just means "partially written, retry next iteration".
        match e {
            WaylandError::Io(io) if io.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
            other => Err(anyhow!("wayland flush: {other}")),
        }
    } else {
        Ok(())
    }
}

/// Result of one `poll(2)` wait on the Wayland socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitStatus {
    Readable,
    Timeout,
    /// POLLHUP/POLLERR/POLLNVAL: compositor closed the connection.
    Hangup,
}

/// Block until the Wayland socket has data (or the timeout expires), then read
/// the pending messages into their queues without dispatching them.
fn wait_readable(
    conn: &Connection,
    queue: &EventQueue<AppState>,
    timeout_ms: i32,
) -> Result<WaitStatus> {
    let fd = queue.as_fd().as_raw_fd();
    let mut fds = [libc::pollfd { fd, events: libc::POLLIN, revents: 0 }];
    let n = unsafe { libc::poll(fds.as_mut_ptr(), 1, timeout_ms) };
    if n < 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
            return Ok(WaitStatus::Timeout); // EINTR: dispatch whatever we have next iteration
        }
        return Err(err.into());
    }
    if n == 0 {
        return Ok(WaitStatus::Timeout);
    }
    let revents = fds[0].revents;
    if revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        return Ok(WaitStatus::Hangup);
    }
    if revents & libc::POLLIN != 0 {
        if let Some(guard) = conn.prepare_read() {
            guard.read().map_err(|e| anyhow!("wayland read: {e}"))?;
        }
    }
    Ok(WaitStatus::Readable)
}

// ---------------------------------------------------------------------------
// Keyboard auto-repeat (pure schedule, unit-tested)
// ---------------------------------------------------------------------------

/// Клиентский key-repeat: Wayland-композитор присылает `wl_keyboard.repeat_info`
/// (rate в keys/sec и delay в мс), но сами повторные `Key`-события НЕ шлёт —
/// повтор обязан синтезировать клиент (иначе удержание Backspace даёт 1 символ).
#[derive(Debug, Default)]
struct KeyRepeat {
    enabled: bool,
    interval: Duration,
    delay: Duration,
    keycode: Option<u32>,
    next: Option<Instant>,
}

impl KeyRepeat {
    /// Применить `wl_keyboard.repeat_info`. `rate <= 0` отключает повтор.
    fn set_info(&mut self, rate: i32, delay_ms: i32) {
        self.enabled = rate > 0;
        self.interval = if rate > 0 {
            Duration::from_millis((1000 / rate.max(1) as u64).max(1))
        } else {
            Duration::ZERO
        };
        self.delay = Duration::from_millis(delay_ms.max(0) as u64);
        if !self.enabled {
            self.clear();
        }
    }

    fn press(&mut self, keycode: u32, now: Instant) {
        if !self.enabled {
            self.keycode = None;
            self.next = None;
            return;
        }
        self.keycode = Some(keycode);
        self.next = Some(now + self.delay);
    }

    fn release(&mut self, keycode: u32) {
        if self.keycode == Some(keycode) {
            self.keycode = None;
            self.next = None;
        }
    }

    fn clear(&mut self) {
        self.keycode = None;
        self.next = None;
    }

    /// Вернуть keycode, если повтор назрел в `now`, и запланировать следующий.
    /// Пропущенные повторы не догоняются пачкой (после лага — один повтор).
    fn poll(&mut self, now: Instant) -> Option<u32> {
        let keycode = self.keycode?;
        let mut next = self.next?;
        if now < next {
            return None;
        }
        next += self.interval;
        while next <= now {
            next += self.interval;
        }
        self.next = Some(next);
        Some(keycode)
    }

    /// Сколько осталось до следующего повтора (для poll-timeout).
    fn timeout(&self, now: Instant) -> Option<Duration> {
        let _held = self.keycode?;
        Some(self.next?.saturating_duration_since(now))
    }
}

/// Запасная раскладка, если xkbcommon не смог скомпилировать keymap: базовые
/// клавиши (в первую очередь Escape, чтобы лаунчер можно было закрыть) берутся
/// из evdev-кодов и не зависят от раскладки.
fn raw_evdev_key(keycode: u32) -> Option<UiKey> {
    match keycode {
        1 => Some(UiKey::Escape),
        14 => Some(UiKey::Backspace),
        15 => Some(UiKey::Tab),
        28 | 96 => Some(UiKey::Enter),
        102 => Some(UiKey::Home),
        103 => Some(UiKey::ArrowUp),
        104 => Some(UiKey::PageUp),
        107 => Some(UiKey::End),
        108 => Some(UiKey::ArrowDown),
        109 => Some(UiKey::PageDown),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Per-output scale tracking (pure logic, unit-tested)
// ---------------------------------------------------------------------------

/// Scale of the outputs the surface is currently on. Wayland does not have a
/// single global scale: a surface can span/enter multiple outputs (hotplug,
/// mixed-DPI). The buffer scale is the max of the scales of the entered
/// outputs; it must be able to go back down when the surface leaves a hidpi
/// output (the old code only ever grew the global max).
#[derive(Debug, Default)]
struct ScaleTracker {
    /// registry name of a wl_output -> integer scale
    scales: HashMap<u32, i32>,
    /// outputs the surface has entered
    on_surface: HashSet<u32>,
}

impl ScaleTracker {
    fn set_output_scale(&mut self, name: u32, scale: i32) {
        self.scales.insert(name, scale.max(1));
    }

    fn surface_enter(&mut self, name: u32) {
        self.on_surface.insert(name);
    }

    fn surface_leave(&mut self, name: u32) {
        self.on_surface.remove(&name);
    }

    fn remove_output(&mut self, name: u32) {
        self.scales.remove(&name);
        self.on_surface.remove(&name);
    }

    fn effective_scale(&self) -> i32 {
        let on_surface: Vec<i32> = self
            .on_surface
            .iter()
            .filter_map(|name| self.scales.get(name).copied())
            .collect();
        if on_surface.is_empty() {
            // До первого enter (или пока compositor молчит) берём максимум
            // известных шкал: буферы не будут мыльными, а после enter
            // пересчитаем точно (в т.ч. вниз).
            self.scales.values().copied().max().unwrap_or(1).max(1)
        } else {
            on_surface.into_iter().max().unwrap_or(1).max(1)
        }
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Global {
    name: u32,
    interface: String,
    version: u32,
}

#[derive(Debug)]
struct OutputBinding {
    /// wl_registry global name (also the user-data of the proxy).
    name: u32,
    proxy: wl_output::WlOutput,
}

struct AppState {
    core: AppCore,
    registry: Option<wl_registry::WlRegistry>,
    /// Все объявленные globals (нужны для GlobalRemove, поэтому не очищаем).
    globals: Vec<Global>,
    compositor: Option<wl_compositor::WlCompositor>,
    compositor_version: u32,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    layer_shell_version: u32,
    seat: Option<wl_seat::WlSeat>,
    seat_name: Option<u32>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer_proxy: Option<wl_pointer::WlPointer>,
    outputs: Vec<OutputBinding>,
    scale_tracker: ScaleTracker,
    /// `wl_surface.preferred_buffer_scale` (surface v6+), if the compositor sent it.
    preferred_scale: Option<i32>,
    /// Buffer scale currently applied to the surface, to avoid redundant requests.
    applied_buffer_scale: Option<i32>,
    /// zwlr layer number (0 background .. 3 overlay), from the theme.
    layer: u32,
    /// Theme anchor: "center" | "top" | "bottom".
    anchor: String,
    /// zwlr keyboard interactivity *requested*: 1 exclusive, 2 on-demand.
    keyboard_mode: u32,
    surface: Option<wl_surface::WlSurface>,
    layer_surface: Option<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1>,
    /// Logical size last configured by the compositor (0 = client-decided).
    configured: Option<(u32, u32)>,
    buffers: Option<Buffers>,
    /// Buffers that were still busy at resize; destroyed when released.
    retired_buffers: Vec<ShmBuffer>,
    next_buffer: usize,
    pointer: PointerState,
    xkb: Xkb,
    repeat: KeyRepeat,
    /// A repaint is pending even though no animation is running.
    dirty: bool,
    /// The last draw found both buffers busy; pace the retry instead of spinning.
    draw_blocked: bool,
    /// A `wl_surface.frame` callback fired: the compositor says it's a good
    /// time to present (vsync pacing — the loop never renders faster).
    frame_arrived: bool,
    done: bool,
    /// Fatal protocol error detected inside a dispatch handler.
    fatal: Option<String>,
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
    fn new(core: AppCore) -> Self {
        Self {
            core,
            registry: None,
            globals: Vec::new(),
            compositor: None,
            compositor_version: 0,
            shm: None,
            layer_shell: None,
            layer_shell_version: 0,
            seat: None,
            seat_name: None,
            keyboard: None,
            pointer_proxy: None,
            outputs: Vec::new(),
            scale_tracker: ScaleTracker::default(),
            preferred_scale: None,
            applied_buffer_scale: None,
            layer: 3,
            anchor: String::from("center"),
            keyboard_mode: 1,
            surface: None,
            layer_surface: None,
            configured: None,
            buffers: None,
            retired_buffers: Vec::new(),
            next_buffer: 0,
            pointer: PointerState::default(),
            xkb: Xkb::new(),
            repeat: KeyRepeat::default(),
            dirty: true,
            draw_blocked: false,
            frame_arrived: true, // draw the first frame immediately
            done: false,
            fatal: None,
            last_frame: None,
            last_draw: None,
        }
    }

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
        let globals = self.globals.clone();
        let registry = self.registry.clone().ok_or_else(|| anyhow!("no wl_registry"))?;
        for global in globals {
            match global.interface.as_str() {
                "wl_compositor" => {
                    let version = global.version.min(6);
                    self.compositor_version = version;
                    self.compositor = Some(registry.bind(global.name, version, qh, ()));
                }
                "wl_shm" => {
                    self.shm = Some(registry.bind(global.name, global.version.min(1), qh, ()));
                }
                "zwlr_layer_shell_v1" => {
                    let version = global.version.min(4);
                    self.layer_shell_version = version;
                    self.layer_shell = Some(registry.bind(global.name, version, qh, ()));
                }
                "wl_seat" => {
                    // Keep seat version low enough that get_keyboard/get_pointer stay legal.
                    let version = global.version.min(9);
                    self.seat = Some(registry.bind(global.name, version, qh, ()));
                    self.seat_name = Some(global.name);
                }
                "wl_output" => {
                    let version = global.version.min(4);
                    let proxy = registry.bind(global.name, version, qh, global.name);
                    self.outputs.push(OutputBinding { name: global.name, proxy });
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
        if self.seat.is_none() {
            log::warn!("hiren: compositor exposes no wl_seat — keyboard/pointer input unavailable");
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
                ls.set_anchor(
                    edge | zwlr_layer_surface_v1::Anchor::Left
                        | zwlr_layer_surface_v1::Anchor::Right,
                );
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
        ls.set_keyboard_interactivity(self.keyboard_interactivity());
        // Integer buffer scale for hidpi outputs (wl_surface.set_buffer_scale
        // exists since wl_surface v3; fractional scaling needs wp_viewporter).
        self.apply_buffer_scale(&surface);
        surface.commit();

        self.surface = Some(surface);
        self.layer_surface = Some(ls);
        Ok(())
    }

    /// Requested keyboard mode, gated by protocol version: `on_demand` exists
    /// only in zwlr_layer_shell_v1 v4+.
    fn keyboard_interactivity(&self) -> zwlr_layer_surface_v1::KeyboardInteractivity {
        if self.keyboard_mode == 2 {
            if self.layer_shell_version >= 4 {
                zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand
            } else {
                log::warn!(
                    "hiren: keyboard_mode=on_demand requires zwlr_layer_shell_v1 v4 \
                     (compositor has v{}); using exclusive",
                    self.layer_shell_version
                );
                zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive
            }
        } else {
            zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive
        }
    }

    /// Отпустить эксклюзивный захват клавиатуры, чтобы запущенное приложение
    /// получило клавиатуру сразу, а не после 420 мс exit-анимации.
    fn release_keyboard_grab(&mut self) {
        if let (Some(ls), Some(surface)) = (self.layer_surface.as_ref(), self.surface.as_ref()) {
            ls.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
            surface.commit();
            log::debug!("hiren: released keyboard grab for the launched application");
        }
    }

    /// Effective integer buffer scale: per-output scale (v3+ surfaces) combined
    /// with the compositor's preferred_buffer_scale hint.
    fn effective_scale(&self) -> i32 {
        let surface_scale_supported = self
            .surface
            .as_ref()
            .map(|surface| surface.version() >= 3)
            .unwrap_or(self.compositor_version >= 3);
        if !surface_scale_supported {
            return 1;
        }
        let tracked = self.scale_tracker.effective_scale();
        let preferred = self.preferred_scale.unwrap_or(1).max(1);
        tracked.max(preferred).max(1)
    }

    fn apply_buffer_scale(&mut self, surface: &wl_surface::WlSurface) {
        // set_buffer_scale exists since wl_surface v3; sending it on an older
        // proxy would be a protocol error.
        if surface.version() < 3 {
            return;
        }
        let scale = self.effective_scale();
        if self.applied_buffer_scale != Some(scale) {
            surface.set_buffer_scale(scale);
            self.applied_buffer_scale = Some(scale);
            log::debug!("hiren: wl_surface buffer scale = {scale}");
        }
    }

    /// Some output changed scale / surface entered or left one: reallocate
    /// buffers at the new physical size and redraw. Scale may go DOWN now.
    fn on_scale_maybe_changed(&mut self) {
        if self.effective_scale() != self.applied_buffer_scale.unwrap_or(1) {
            self.dirty = true;
        }
    }

    fn output_name(&self, output: &wl_output::WlOutput) -> Option<u32> {
        self.outputs.iter().find(|binding| binding.proxy.id() == output.id()).map(|binding| binding.name)
    }

    /// Logical surface size: compositor-configured when it chose one, else the
    /// theme size.
    fn logical_size(&self) -> (u32, u32) {
        match self.configured {
            Some((cw, ch)) if cw > 0 && ch > 0 => (cw, ch),
            _ => self.core.size(),
        }
    }

    /// Physical buffer size for the current logical size and scale.
    fn buffer_size(&self) -> (i32, i32) {
        buffer_size_for(self.logical_size(), self.effective_scale())
    }

    /// (Re)create the wl_shm buffer pool when missing or when the pixel size
    /// changed. Old buffers are destroyed — or retired until the compositor
    /// releases them, because destroying a buffer that is still being read is
    /// a use-after-free on the compositor side.
    fn ensure_buffers(&mut self, qh: &QueueHandle<AppState>) {
        let (pw, ph) = self.buffer_size();
        if let Some(buffers) = &self.buffers {
            if buffers.pw == pw && buffers.ph == ph && self.retired_buffers.is_empty() {
                return;
            }
        }
        if let Some(old) = self.buffers.take() {
            for buffer in old.bufs {
                if buffer.busy {
                    self.retired_buffers.push(buffer);
                } else {
                    buffer.destroy();
                }
            }
        }
        let Some(shm) = self.shm.clone() else { return };
        let stride = pw * 4;
        let size = stride * ph;
        match (
            create_shm_buffer(&shm, qh, pw, ph, stride, size),
            create_shm_buffer(&shm, qh, pw, ph, stride, size),
        ) {
            (Ok(a), Ok(b)) => {
                self.buffers = Some(Buffers { bufs: [a, b], stride, pw, ph });
            }
            (Err(e), _) | (_, Err(e)) => {
                log::warn!("hiren: shm buffer setup failed: {e}");
            }
        }
    }

    /// Draw one frame. Returns `true` when a frame was actually committed;
    /// `false` means "retry later" (no surface/buffers yet, or both buffers
    /// still busy). Callers must only clear `dirty` on success — the old code
    /// cleared it before drawing, silently dropping the pending repaint.
    fn draw(&mut self, qh: &QueueHandle<AppState>) -> bool {
        self.ensure_buffers(qh);
        let (pw, ph, idx) = {
            let Some(buffers) = self.buffers.as_ref() else {
                self.draw_blocked = true;
                return false;
            };
            let idx = if !buffers.bufs[self.next_buffer].busy {
                self.next_buffer
            } else if !buffers.bufs[1 - self.next_buffer].busy {
                1 - self.next_buffer
            } else {
                // Compositor still reads both buffers; try again next poll tick.
                self.draw_blocked = true;
                return false;
            };
            (buffers.pw.max(1) as u32, buffers.ph.max(1) as u32, idx)
        };
        let scale = self.effective_scale() as f32;

        // Resolve + render at physical pixel size (nodes carry the scale in
        // their transforms, so hidpi outputs get sharp text).
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

        let Some(surface) = self.surface.clone() else { return false };
        // Re-apply the (possibly changed) buffer scale before attaching.
        self.apply_buffer_scale(&surface);
        {
            let buffers = self.buffers.as_mut().unwrap();
            let buf = &mut buffers.bufs[idx];
            if buf
                .file
                .seek(SeekFrom::Start(0))
                .and_then(|_| buf.file.write_all(&out))
                .is_err()
            {
                log::warn!("hiren: shm buffer write failed");
                return false;
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
        self.draw_blocked = false;
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
        true
    }

    /// Poll timeout for the next loop iteration. Real pacing comes from the
    /// `wl_surface.frame` vsync callback; these timeouts are only safety
    /// nets (e.g. when the surface is occluded and callbacks pause).
    fn frame_timeout_ms(&self) -> i32 {
        if self.done {
            return 0;
        }
        // Exit delay (420 ms launch animation): wake on the deadline / next
        // frame, never poll(0) in a loop (that was 100% CPU for the whole delay).
        if let Some(remaining) = self.core.exit_delay_remaining_ms() {
            let remaining = remaining.max(0);
            return if self.core.runtime.animating() {
                FRAME_MS.min(remaining)
            } else {
                remaining
            };
        }
        if self.core.runtime.animating() {
            return FRAME_MS;
        }
        if self.core.last_uses_time_debug() || self.core.impulse_ticking() {
            return self.core.time_frame_ms();
        }
        if self.dirty {
            // Both buffers were busy: wait for a release/frame event instead of
            // retrying at 100% CPU; the 16 ms cap is only a safety net.
            return if self.draw_blocked { FRAME_MS } else { 0 };
        }
        if self.core.auto_close_pending() {
            IDLE_TICK_MS
        } else {
            -1
        }
    }

    /// Poll timeout including the next key-repeat deadline.
    fn poll_timeout_ms(&self) -> i32 {
        let mut timeout = self.frame_timeout_ms();
        if let Some(delay) = self.repeat.timeout(Instant::now()) {
            let repeat_ms = delay.as_millis().min(i32::MAX as u128) as i32;
            timeout = if timeout < 0 { repeat_ms } else { timeout.min(repeat_ms) };
        }
        timeout
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
        // Launch failures from reaper threads: show them in the open window.
        if self.core.poll_launch_errors() {
            self.dirty = true;
        }
        if self.core.auto_close_expired() {
            log::info!("hiren: auto-close timeout");
            self.done = true;
            return;
        }
        let animating = self.core.runtime.animating();
        let throttle_ms = if animating || self.dirty {
            0
        } else if self.core.last_uses_time_debug() || self.core.impulse_ticking() {
            self.core.time_frame_ms()
        } else {
            0
        };
        let due = self
            .last_draw
            .map(|t| t.elapsed().as_millis() as i32 >= throttle_ms)
            .unwrap_or(true);
        if self.dirty || (self.frame_arrived && due && self.core.needs_frame()) {
            // Only clear `dirty` when the frame was actually committed.
            if self.draw(qh) {
                self.dirty = false;
            }
        }
    }

    /// Handle one logical key press (real event or synthesized repeat).
    /// Returns `true` if the key mapped to a logical key (repeat candidate).
    fn handle_pressed_key(&mut self, keycode: u32) -> bool {
        let Some(key) = self.xkb.interpret(keycode) else {
            log::debug!("keyboard: key code={keycode} produced NO logical key");
            return false;
        };
        let mods = self.xkb.mods();
        let outcome = self.core.handle_key(key, mods);
        self.dirty = true;
        if self.core.take_grab_release() {
            self.release_keyboard_grab();
        }
        if outcome == Outcome::Exit {
            self.done = true;
        }
        true
    }

    fn release_keyboard(&mut self) {
        self.repeat.clear();
        if let Some(keyboard) = self.keyboard.take() {
            if keyboard.version() >= 3 {
                keyboard.release();
            }
        }
    }

    fn release_pointer(&mut self) {
        if let Some(pointer) = self.pointer_proxy.take() {
            if pointer.version() >= 3 {
                pointer.release();
            }
        }
    }

    /// wl_registry.global_remove: destroy/release the matching proxies. The old
    /// code ignored GlobalRemove entirely, so outputs/seat/keyboard resources
    /// leaked and hotplugged outputs kept stale scale values.
    fn handle_global_remove(&mut self, name: u32) {
        let interface = self
            .globals
            .iter()
            .find(|global| global.name == name)
            .map(|global| global.interface.clone());
        self.globals.retain(|global| global.name != name);
        match interface.as_deref() {
            Some("wl_output") => {
                if let Some(index) = self.outputs.iter().position(|output| output.name == name) {
                    let output = self.outputs.remove(index);
                    if output.proxy.version() >= 3 {
                        output.proxy.release();
                    }
                }
                self.scale_tracker.remove_output(name);
                self.dirty = true;
            }
            Some("wl_seat") if self.seat_name == Some(name) => {
                self.release_keyboard();
                self.release_pointer();
                if let Some(seat) = self.seat.take() {
                    if seat.version() >= 5 {
                        seat.release();
                    }
                }
                self.seat_name = None;
                log::warn!("hiren: wl_seat was removed by the compositor — keyboard input is gone");
            }
            Some("wl_compositor") | Some("wl_shm") | Some("zwlr_layer_shell_v1") => {
                let interface = interface.unwrap_or_else(|| "unknown".into());
                self.fatal = Some(format!("compositor removed the required global `{interface}`"));
                self.done = true;
            }
            _ => {}
        }
    }
}

/// Remove every protocol object explicitly (wayland-client does not do it on
/// Drop). Called from `Drop` and therefore also on error paths.
impl Drop for AppState {
    fn drop(&mut self) {
        if let Some(buffers) = self.buffers.take() {
            for buffer in buffers.bufs {
                buffer.destroy();
            }
        }
        for buffer in self.retired_buffers.drain(..) {
            buffer.destroy();
        }
        if let Some(keyboard) = self.keyboard.take() {
            if keyboard.version() >= 3 {
                keyboard.release();
            }
        }
        if let Some(pointer) = self.pointer_proxy.take() {
            if pointer.version() >= 3 {
                pointer.release();
            }
        }
        // The layer surface must be destroyed before its wl_surface.
        if let Some(layer_surface) = self.layer_surface.take() {
            layer_surface.destroy();
        }
        if let Some(surface) = self.surface.take() {
            surface.destroy();
        }
        if let Some(seat) = self.seat.take() {
            if seat.version() >= 5 {
                seat.release();
            }
        }
        for output in self.outputs.drain(..) {
            if output.proxy.version() >= 3 {
                output.proxy.release();
            }
        }
    }
}

/// Physical pixel size of a buffer for a logical surface size and integer scale.
fn buffer_size_for(logical: (u32, u32), scale: i32) -> (i32, i32) {
    let scale = scale.max(1) as f32;
    (
        (logical.0 as f32 * scale).round().max(1.0) as i32,
        (logical.1 as f32 * scale).round().max(1.0) as i32,
    )
}

struct ShmBuffer {
    file: std::fs::File,
    buffer: wl_buffer::WlBuffer,
    busy: bool,
}

impl ShmBuffer {
    fn destroy(self) {
        self.buffer.destroy();
        // file is dropped (fd closed) after the buffer request; the compositor's
        // mapping of the shm fd stays valid.
    }
}

/// Create one anonymous shm file + pool + ARGB8888 buffer. The pool is
/// destroyed immediately after the buffers are created (they keep its memory
/// alive); otherwise every resize leaked a pool in the compositor.
fn create_shm_buffer(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<AppState>,
    pw: i32,
    ph: i32,
    stride: i32,
    size: i32,
) -> Result<ShmBuffer> {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "hiren-shm-{}-{}",
        std::process::id(),
        // Avoid clashing with other buffers/processes; the file is unlinked below.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(&path)?;
    let _ = std::fs::remove_file(&path);
    file.set_len(size as u64)?;
    let pool = shm.create_pool(file.as_fd(), size, qh, ());
    let buffer = pool.create_buffer(0, pw, ph, stride, wl_shm::Format::Argb8888, qh, ());
    // Buffers keep the pool's memory alive; destroy the pool object right away.
    pool.destroy();
    Ok(ShmBuffer { file, buffer, busy: false })
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
        let Some(xh) = xh() else {
            log::warn!("hiren: libxkbcommon vanished; using raw evdev fallback keys");
            return;
        };
        unsafe {
            if self.ctx.is_null() {
                self.ctx = (xh.xkb_context_new)(xkb_context_flags::XKB_CONTEXT_NO_FLAGS);
            }
            if self.ctx.is_null() {
                log::warn!("hiren: xkb_context_new failed; using raw evdev fallback keys");
                return;
            }
            if !self.keymap.is_null() {
                (xh.xkb_keymap_unref)(self.keymap);
                self.keymap = std::ptr::null_mut();
            }
            if !self.state.is_null() {
                (xh.xkb_state_unref)(self.state);
                self.state = std::ptr::null_mut();
            }
            self.keymap = (xh.xkb_keymap_new_from_buffer)(
                self.ctx,
                data.as_ptr() as *const std::os::raw::c_char,
                data.len(),
                xkb_keymap_format::XKB_KEYMAP_FORMAT_TEXT_V1,
                xkb::xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS,
            );
            if self.keymap.is_null() {
                // Do not leave the keyboard dead including Escape: `interpret`
                // falls back to raw evdev codes for the essential keys.
                log::warn!("hiren: xkb keymap compile failed; using raw evdev fallback keys");
                return;
            }
            log::debug!(
                "xkb: keymap loaded ({} bytes), min_keycode={} max_keycode={}, num_layouts={}",
                data.len(),
                (xh.xkb_keymap_min_keycode)(self.keymap),
                (xh.xkb_keymap_max_keycode)(self.keymap),
                (xh.xkb_keymap_num_layouts)(self.keymap),
            );
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
        let Some(xh) = xh() else { return };
        unsafe {
            (xh.xkb_state_update_mask)(self.state, depressed, latched, locked, 0, 0, group);
        }
    }

    fn mods(&self) -> UiMods {
        if self.state.is_null() {
            return UiMods::default();
        }
        let Some(xh) = xh() else { return UiMods::default() };
        let active = |idx: u32| -> bool {
            if idx == u32::MAX {
                return false;
            }
            unsafe {
                (xh.xkb_state_mod_index_is_active)(
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

    /// Map a key event (evdev keycode) to a UiKey. With a broken/missing keymap
    /// the essential keys still resolve through raw evdev codes.
    fn interpret(&self, keycode: u32) -> Option<UiKey> {
        if !self.state.is_null() {
            let Some(xh) = xh() else { return raw_evdev_key(keycode) };
            let kc = keycode + 8; // evdev → xkb keycode offset
            let ks = unsafe { (xh.xkb_state_key_get_one_sym)(self.state, kc) };
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
                (xh.xkb_state_key_get_utf8)(
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
        }
        // Essential keys never depend on a successfully compiled keymap.
        raw_evdev_key(keycode)
    }
}

impl Drop for Xkb {
    fn drop(&mut self) {
        if self.state.is_null() && self.keymap.is_null() && self.ctx.is_null() {
            return;
        }
        let Some(xh) = xh() else { return };
        unsafe {
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
        match event {
            wl_registry::Event::Global { name, interface, version } => {
                state.globals.push(Global { name, interface, version });
            }
            wl_registry::Event::GlobalRemove { name } => {
                state.handle_global_remove(name);
            }
            _ => {}
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
                if state.keyboard.is_none() {
                    state.keyboard = Some(seat.get_keyboard(qh, ()));
                }
            } else {
                state.release_keyboard();
            }
            if caps.contains(wl_seat::Capability::Pointer) {
                if state.pointer_proxy.is_none() {
                    state.pointer_proxy = Some(seat.get_pointer(qh, ()));
                }
            } else {
                state.release_pointer();
            }
        }
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
                            log::debug!(
                                "keyboard: keymap read {} bytes (advertised {size})",
                                buf.len()
                            );
                            state.xkb.load_keymap(&buf);
                        }
                        Err(e) => {
                            log::warn!("hiren: failed to read keymap fd (size={size}): {e}")
                        }
                    }
                }
            }
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                log::debug!("keyboard: repeat_info rate={rate}/s delay={delay}ms");
                state.repeat.set_info(rate, delay);
            }
            wl_keyboard::Event::Modifiers { mods_depressed, mods_latched, mods_locked, group, .. } => {
                log::debug!("keyboard: modifiers dep={mods_depressed} lat={mods_latched} lock={mods_locked} group={group}");
                state.xkb.update_mask(mods_depressed, mods_latched, mods_locked, group);
            }
            wl_keyboard::Event::Key { key, state: WEnum::Value(kstate), .. } => {
                log::debug!("keyboard: key event code={key} state={kstate:?}");
                match kstate {
                    wl_keyboard::KeyState::Pressed => {
                        let now = Instant::now();
                        // Track repeat only for keys that map to a logical key:
                        // modifier presses should not schedule text repeats.
                        if state.handle_pressed_key(key) {
                            state.repeat.press(key, now);
                        }
                    }
                    wl_keyboard::KeyState::Released => state.repeat.release(key),
                    _ => {}
                }
            }
            wl_keyboard::Event::Leave { .. } => {
                // Focus lost: stop synthesizing repeats of a key we no longer own.
                state.repeat.clear();
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
                if state.core.handle_hover(surface_x, surface_y) {
                    state.dirty = true;
                }
            }
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                state.pointer.x = surface_x;
                state.pointer.y = surface_y;
                if state.core.handle_hover(surface_x, surface_y) {
                    state.dirty = true;
                }
            }
            wl_pointer::Event::Button { button, state: WEnum::Value(bstate), .. } => {
                if bstate == wl_pointer::ButtonState::Pressed && button == BTN_LEFT {
                    let (x, y) = (state.pointer.x, state.pointer.y);
                    let mods = state.xkb.mods();
                    let outcome = state.core.handle_click(x, y, mods);
                    state.dirty = true;
                    if state.core.take_grab_release() {
                        state.release_keyboard_grab();
                    }
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
        state: &mut Self,
        _: &wl_surface::WlSurface,
        event: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_surface::Event::Enter { output } => {
                if let Some(name) = state.output_name(&output) {
                    state.scale_tracker.surface_enter(name);
                    state.on_scale_maybe_changed();
                }
            }
            wl_surface::Event::Leave { output } => {
                if let Some(name) = state.output_name(&output) {
                    state.scale_tracker.surface_leave(name);
                    state.on_scale_maybe_changed();
                }
            }
            wl_surface::Event::PreferredBufferScale { factor } => {
                if factor > 0 {
                    state.preferred_scale = Some(factor);
                    state.on_scale_maybe_changed();
                }
            }
            _ => {}
        }
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
        if let wl_buffer::Event::Release = event {
            if let Some(buffers) = state.buffers.as_mut() {
                for candidate in buffers.bufs.iter_mut() {
                    if candidate.buffer.id() == buffer.id() {
                        candidate.busy = false;
                    }
                }
            }
            // A buffer retired during a resize can be destroyed only now.
            if let Some(index) =
                state.retired_buffers.iter().position(|retired| retired.buffer.id() == buffer.id())
            {
                let retired = state.retired_buffers.remove(index);
                retired.destroy();
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for AppState {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Scale { factor } = event {
            state.scale_tracker.set_output_scale(*name, factor);
            state.on_scale_maybe_changed();
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
            zwlr_layer_surface_v1::Event::Closed => {
                state.done = true;
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (pure logic only — no compositor is available in the headless env)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // Keyboard auto-repeat schedule
    // -----------------------------------------------------------------

    #[test]
    fn repeat_waits_for_delay_then_repeats_at_rate() {
        let mut repeat = KeyRepeat::default();
        repeat.set_info(25, 400); // 25/s -> 40ms interval, 400ms delay
        let t0 = Instant::now();
        repeat.press(14, t0);

        assert_eq!(repeat.timeout(t0), Some(Duration::from_millis(400)));
        assert_eq!(repeat.poll(t0 + Duration::from_millis(399)), None);
        assert_eq!(repeat.poll(t0 + Duration::from_millis(400)), Some(14));
        assert_eq!(repeat.poll(t0 + Duration::from_millis(439)), None);
        assert_eq!(repeat.poll(t0 + Duration::from_millis(440)), Some(14));
        assert_eq!(repeat.poll(t0 + Duration::from_millis(480)), Some(14));
    }

    #[test]
    fn repeat_catches_up_without_bursting() {
        let mut repeat = KeyRepeat::default();
        repeat.set_info(25, 400);
        let t0 = Instant::now();
        repeat.press(14, t0);
        // Пришли сильно позже (лаг): синтезируем один повтор, не пачку.
        assert_eq!(repeat.poll(t0 + Duration::from_millis(2_000)), Some(14));
        assert_eq!(repeat.poll(t0 + Duration::from_millis(2_000)), None);
        // Следующий — уже через интервал от «сейчас», не догоняя прошлое.
        assert_eq!(repeat.poll(t0 + Duration::from_millis(2_040)), Some(14));
    }

    #[test]
    fn repeat_release_stops_it() {
        let mut repeat = KeyRepeat::default();
        repeat.set_info(25, 400);
        let t0 = Instant::now();
        repeat.press(14, t0);
        repeat.release(14);
        assert_eq!(repeat.poll(t0 + Duration::from_secs(5)), None);
        assert_eq!(repeat.timeout(t0), None);
    }

    #[test]
    fn repeat_release_of_other_key_keeps_current() {
        let mut repeat = KeyRepeat::default();
        repeat.set_info(25, 400);
        let t0 = Instant::now();
        repeat.press(14, t0);
        repeat.release(15);
        assert_eq!(repeat.poll(t0 + Duration::from_millis(400)), Some(14));
    }

    #[test]
    fn repeat_disabled_when_rate_is_zero_or_negative() {
        for rate in [0, -5] {
            let mut repeat = KeyRepeat::default();
            repeat.set_info(rate, 400);
            let t0 = Instant::now();
            repeat.press(14, t0);
            assert_eq!(repeat.poll(t0 + Duration::from_secs(5)), None);
            assert_eq!(repeat.timeout(t0), None);
        }
    }

    #[test]
    fn repeat_with_zero_delay_fires_same_tick() {
        let mut repeat = KeyRepeat::default();
        repeat.set_info(10, 0);
        let t0 = Instant::now();
        repeat.press(14, t0);
        assert_eq!(repeat.poll(t0), Some(14));
        assert_eq!(repeat.poll(t0), None);
        assert_eq!(repeat.poll(t0 + Duration::from_millis(100)), Some(14));
    }

    #[test]
    fn repeat_info_change_before_press_applies() {
        let mut repeat = KeyRepeat::default();
        repeat.set_info(25, 400);
        repeat.release(14); // сброс предыдущего удержания
        repeat.set_info(10, 100); // 100ms interval, 100ms delay
        let t0 = Instant::now();
        repeat.press(14, t0);
        assert_eq!(repeat.poll(t0 + Duration::from_millis(99)), None);
        assert_eq!(repeat.poll(t0 + Duration::from_millis(100)), Some(14));
    }

    // -----------------------------------------------------------------
    // Raw evdev fallback (broken keymap must not kill Escape)
    // -----------------------------------------------------------------

    #[test]
    fn raw_evdev_fallback_covers_essential_keys() {
        assert_eq!(raw_evdev_key(1), Some(UiKey::Escape));
        assert_eq!(raw_evdev_key(28), Some(UiKey::Enter));
        assert_eq!(raw_evdev_key(14), Some(UiKey::Backspace));
        assert_eq!(raw_evdev_key(103), Some(UiKey::ArrowUp));
        assert_eq!(raw_evdev_key(108), Some(UiKey::ArrowDown));
        assert_eq!(raw_evdev_key(15), Some(UiKey::Tab));
        assert_eq!(raw_evdev_key(200), None);
    }

    // -----------------------------------------------------------------
    // Per-output scale
    // -----------------------------------------------------------------

    #[test]
    fn scale_defaults_to_one_without_outputs() {
        assert_eq!(ScaleTracker::default().effective_scale(), 1);
    }

    #[test]
    fn scale_is_max_of_entered_outputs_and_can_go_down() {
        let mut tracker = ScaleTracker::default();
        tracker.set_output_scale(1, 1);
        tracker.set_output_scale(2, 2);
        // Пока surface не вошёл ни в один output — максимум известных шкал.
        assert_eq!(tracker.effective_scale(), 2);

        tracker.surface_enter(1);
        assert_eq!(tracker.effective_scale(), 1);

        tracker.surface_enter(2);
        assert_eq!(tracker.effective_scale(), 2);

        // Ушли с hidpi-монитора: шкала должна уменьшиться (старый код не умел).
        tracker.surface_leave(2);
        assert_eq!(tracker.effective_scale(), 1);
    }

    #[test]
    fn scale_change_of_entered_output_is_picked_up() {
        let mut tracker = ScaleTracker::default();
        tracker.set_output_scale(1, 1);
        tracker.surface_enter(1);
        assert_eq!(tracker.effective_scale(), 1);
        tracker.set_output_scale(1, 3);
        assert_eq!(tracker.effective_scale(), 3);
        tracker.set_output_scale(1, 0); // мусорное значение клампится к 1
        assert_eq!(tracker.effective_scale(), 1);
    }

    #[test]
    fn removed_output_no_longer_affects_scale() {
        let mut tracker = ScaleTracker::default();
        tracker.set_output_scale(1, 2);
        tracker.set_output_scale(2, 1);
        tracker.surface_enter(1);
        tracker.remove_output(1);
        // output 1 удалён: остался только 2 со scale 1.
        assert_eq!(tracker.effective_scale(), 1);
    }

    // -----------------------------------------------------------------
    // Buffer geometry
    // -----------------------------------------------------------------

    #[test]
    fn buffer_size_multiplies_logical_by_scale() {
        assert_eq!(buffer_size_for((100, 50), 1), (100, 50));
        assert_eq!(buffer_size_for((100, 50), 2), (200, 100));
        assert_eq!(buffer_size_for((100, 50), 3), (300, 150));
        // Degenerate scale never produces a zero-sized buffer.
        assert_eq!(buffer_size_for((100, 50), 0), (100, 50));
    }
}

//! Window backend: winit (xdg-toplevel on Wayland, also X11/dev fallback).
//!
//! This is one of two interchangeable frontends. It translates winit events
//! into `UiKey`/`UiMods`/clicks and lets `frontend::AppCore` do everything else.
//! The layer-shell backend (`wayland.rs`) shares the same core.
//!
//! DPI: `AppCore` always works with *logical* coordinates/sizes. winit hands us
//! physical pixels (`Resized`, `CursorMoved`), so the backend converts with the
//! current scale factor; rendering targets the physical pixel size and scales
//! nodes by `scale`.

use crate::frontend::{AppCore, Outcome, UiKey, UiMods};
use anyhow::Result;
use softbuffer::{Context, Surface};
use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::{ElementState, Ime, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

pub fn run(core: AppCore) -> Result<()> {
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let size = core.size();
    let mut app = WinitApp {
        core: Some(core),
        window: None,
        context: None,
        surface: None,
        logical_size: size,
        scale: 1.0,
        cursor_logical: None,
        modifiers: ModifiersState::empty(),
        next_frame: None,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

/// Convert a physical pixel size to logical size (nearest pixel, min 1).
fn physical_to_logical(size: PhysicalSize<u32>, scale: f32) -> (u32, u32) {
    let scale = if scale > 0.0 { scale as f64 } else { 1.0 };
    (
        (size.width as f64 / scale).round().max(1.0) as u32,
        (size.height as f64 / scale).round().max(1.0) as u32,
    )
}

/// Convert a logical size to the physical pixel size used for the pixmap.
fn logical_to_physical(size: (u32, u32), scale: f32) -> (u32, u32) {
    let scale = if scale > 0.0 { scale as f64 } else { 1.0 };
    (
        (size.0 as f64 * scale).round().max(1.0) as u32,
        (size.1 as f64 * scale).round().max(1.0) as u32,
    )
}

/// Convert physical cursor coordinates to logical (hit-testing) coordinates.
fn cursor_to_logical(x: f64, y: f64, scale: f32) -> (f64, f64) {
    let scale = if scale > 0.0 { scale as f64 } else { 1.0 };
    (x / scale, y / scale)
}

struct WinitApp {
    core: Option<AppCore>,
    window: Option<Rc<Window>>,
    context: Option<Context<Rc<Window>>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
    /// Logical window size — the only size `AppCore`/layout sees.
    logical_size: (u32, u32),
    scale: f32,
    /// Cursor position in logical coordinates (for hit-testing).
    cursor_logical: Option<(f64, f64)>,
    modifiers: ModifiersState,
    /// Earliest moment the next animation frame may be drawn. `about_to_wait`
    /// fires on every loop pass — without this gate `request_redraw` would
    /// preempt the ControlFlow wait and busy-spin the loop at 100% CPU.
    next_frame: Option<Instant>,
}

impl WinitApp {
    fn core(&mut self) -> &mut AppCore {
        self.core.as_mut().expect("core present while running")
    }

    fn mods(&self) -> UiMods {
        UiMods {
            ctrl: self.modifiers.control_key(),
            alt: self.modifiers.alt_key(),
            super_key: self.modifiers.super_key(),
            shift: self.modifiers.shift_key(),
        }
    }

    fn physical_size(&self) -> (u32, u32) {
        logical_to_physical(self.logical_size, self.scale)
    }

    fn render(&mut self) {
        let Some(window) = self.window.clone() else { return };
        let scale = self.scale;
        let nodes = self.core().render_frame(scale).to_vec();

        if self.context.is_none() {
            if let Ok(ctx) = Context::new(window.clone()) {
                self.context = Some(ctx);
            }
        }
        if self.surface.is_none() {
            if let Some(ctx) = &self.context {
                if let Ok(surf) = Surface::new(ctx, window) {
                    self.surface = Some(surf);
                }
            }
        }
        // Layout is logical; the pixmap/surface are physical pixels.
        let (pw, ph) = self.physical_size();
        let pixmap = self.core().runtime.render_nodes(&nodes, (pw, ph), scale);
        let Some(surface) = &mut self.surface else { return };
        let _ = surface.resize(
            NonZeroU32::new(pw.max(1)).unwrap(),
            NonZeroU32::new(ph.max(1)).unwrap(),
        );
        if let Ok(mut buffer) = surface.buffer_mut() {
            let pixels = pixmap.pixels();
            for (i, px) in buffer.iter_mut().enumerate() {
                let c = match pixels.get(i) {
                    Some(c) => *c,
                    None => continue,
                };
                // tiny-skia stores premultiplied RGBA; Wayland's ARGB8888 is
                // premultiplied too — pass channels through unchanged.
                *px = ((c.alpha() as u32) << 24) | ((c.red() as u32) << 16) | ((c.green() as u32) << 8) | c.blue() as u32;
            }
            let _ = buffer.present();
        }
    }

    /// Schedule the next frame: vsync-ish cadence while animations run,
    /// ~22 fps for time-driven themes with nothing else in motion (caret
    /// blink), slow wake-ups only while an auto-close deadline is pending,
    /// else fully idle.
    fn schedule(&mut self, event_loop: &ActiveEventLoop) {
        let core = self.core.as_ref().unwrap();
        self.next_frame = if core.runtime.animating() {
            Some(Instant::now() + Duration::from_millis(16))
        } else if core.needs_frame() {
            Some(Instant::now() + Duration::from_millis(core.time_frame_ms() as u64))
        } else if core.auto_close_pending() {
            Some(Instant::now() + Duration::from_millis(250))
        } else {
            None
        };
        event_loop.set_control_flow(match self.next_frame {
            Some(t) => ControlFlow::WaitUntil(t),
            None => ControlFlow::Wait,
        });
    }

    fn maybe_exit(&mut self, outcome: Outcome, event_loop: &ActiveEventLoop) {
        if outcome == Outcome::Exit {
            event_loop.exit();
        }
    }

    fn request_redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Adopt the physical size reported by the compositor as logical layout size.
    fn adopt_physical_size(&mut self, physical: PhysicalSize<u32>) {
        if physical.width == 0 || physical.height == 0 {
            return;
        }
        let logical = physical_to_logical(physical, self.scale);
        if logical != self.logical_size {
            self.logical_size = logical;
            self.core().set_size(logical.0, logical.1);
            self.request_redraw();
        }
    }
}

impl ApplicationHandler for WinitApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            let transparent = self.core.as_ref().unwrap().config_theme_transparent();
            let attrs = Window::default_attributes()
                .with_title("hiren")
                .with_inner_size(LogicalSize::new(self.logical_size.0, self.logical_size.1))
                .with_decorations(false)
                .with_transparent(transparent)
                .with_window_level(winit::window::WindowLevel::AlwaysOnTop)
                .with_resizable(false);
            match event_loop.create_window(attrs) {
                Ok(w) => {
                    let w = Rc::new(w);
                    // IME must be enabled explicitly for composed input
                    // (CJK/emoji) to arrive as `Ime::Commit` events.
                    w.set_ime_allowed(true);
                    self.scale = w.scale_factor() as f32;
                    if self.scale <= 0.0 {
                        self.scale = 1.0;
                    }
                    w.request_redraw();
                    self.window = Some(w);
                }
                Err(e) => {
                    eprintln!("[hiren-client] Failed to create window: {e:?}");
                    event_loop.exit();
                }
            }
        }
    }

    /// Timer wake-ups (ControlFlow::WaitUntil) land here — request a redraw so
    /// animations keep playing while the loop is otherwise idle.
    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Ошибки запуска из reaper-потоков показываем в открытом окне.
        let error_changed = self
            .core
            .as_mut()
            .map(|core| core.poll_launch_errors())
            .unwrap_or(false);
        if error_changed {
            self.request_redraw();
        }
        if let Some(w) = &self.window {
            let due = self.next_frame.map(|t| Instant::now() >= t).unwrap_or(false);
            if due {
                w.request_redraw();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => {
                if self.core.as_ref().unwrap().auto_close_expired() {
                    log::info!("auto-close timeout");
                    event_loop.exit();
                    return;
                }
                self.render();
                self.schedule(event_loop);
            }
            WindowEvent::KeyboardInput { event: KeyEvent { logical_key, state: ElementState::Pressed, .. }, .. } => {
                let mods = self.mods();
                let Some(key) = map_key(&logical_key, mods.shift) else { return };
                let outcome = self.core().handle_key(key, mods);
                self.maybe_exit(outcome, event_loop);
                self.request_redraw();
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::Ime(Ime::Commit(text)) => {
                if !text.is_empty() {
                    let mods = self.mods();
                    let outcome = self.core().handle_key(UiKey::Char(text.clone()), mods);
                    self.maybe_exit(outcome, event_loop);
                    self.request_redraw();
                }
            }
            WindowEvent::Ime(Ime::Preedit(_, _)) => {}
            WindowEvent::MouseInput { state: ElementState::Pressed, button: winit::event::MouseButton::Left, .. } => {
                if let Some((x, y)) = self.cursor_logical {
                    let mods = self.mods();
                    let outcome = self.core().handle_click(x, y, mods);
                    self.maybe_exit(outcome, event_loop);
                    self.request_redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                // winit даёт физические координаты; hit-test/layout — логические.
                let logical = cursor_to_logical(position.x, position.y, self.scale);
                self.cursor_logical = Some(logical);
                let changed = self.core().handle_hover(logical.0, logical.1);
                if changed {
                    // Перерисовываемся только когда выбор реально изменился.
                    self.request_redraw();
                }
            }
            WindowEvent::Resized(physical) => {
                // Композитор сообщает размер в физических пикселях; layout —
                // логический, поэтому конвертируем через scale (иначе двойное
                // масштабирование и четверть видимого окна на hidpi).
                self.adopt_physical_size(physical);
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if scale_factor > 0.0 {
                    self.scale = scale_factor as f32;
                }
                // Сохраняем логический размер окна при смене масштаба.
                if let Some(w) = &self.window {
                    let _ = w.request_inner_size(LogicalSize::new(
                        self.logical_size.0 as f64,
                        self.logical_size.1 as f64,
                    ));
                }
                self.request_redraw();
            }
            _ => {}
        }
    }
}

fn map_key(key: &Key, shift: bool) -> Option<UiKey> {
    match key {
        Key::Named(NamedKey::Escape) => Some(UiKey::Escape),
        Key::Named(NamedKey::Enter) => Some(UiKey::Enter),
        Key::Named(NamedKey::Backspace) => Some(UiKey::Backspace),
        Key::Named(NamedKey::ArrowDown) => Some(UiKey::ArrowDown),
        Key::Named(NamedKey::ArrowUp) => Some(UiKey::ArrowUp),
        Key::Named(NamedKey::Home) => Some(UiKey::Home),
        Key::Named(NamedKey::End) => Some(UiKey::End),
        Key::Named(NamedKey::PageDown) => Some(UiKey::PageDown),
        Key::Named(NamedKey::PageUp) => Some(UiKey::PageUp),
        Key::Named(NamedKey::Tab) => Some(if shift { UiKey::BackTab } else { UiKey::Tab }),
        Key::Character(s) => {
            if s.is_empty() {
                None
            } else if s == "\t" {
                Some(if shift { UiKey::BackTab } else { UiKey::Tab })
            } else {
                Some(UiKey::Char(s.to_string()))
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // DPI conversion
    // -----------------------------------------------------------------

    #[test]
    fn physical_to_logical_at_scale_one_is_identity() {
        assert_eq!(
            physical_to_logical(PhysicalSize::new(640, 420), 1.0),
            (640, 420)
        );
    }

    #[test]
    fn physical_to_logical_divides_by_scale() {
        assert_eq!(
            physical_to_logical(PhysicalSize::new(1280, 840), 2.0),
            (640, 420)
        );
        assert_eq!(
            physical_to_logical(PhysicalSize::new(960, 630), 1.5),
            (640, 420)
        );
        // Некруглые физические размеры округляются к ближайшему логическому.
        assert_eq!(
            physical_to_logical(PhysicalSize::new(1001, 667), 1.5),
            (667, 445)
        );
    }

    #[test]
    fn logical_to_physical_multiplies_by_scale() {
        assert_eq!(logical_to_physical((640, 420), 1.0), (640, 420));
        assert_eq!(logical_to_physical((640, 420), 2.0), (1280, 840));
        assert_eq!(logical_to_physical((640, 420), 1.5), (960, 630));
    }

    #[test]
    fn dpi_roundtrip_is_stable() {
        // Resized(physical) → logical → physical не должен «дрожать».
        for scale in [1.0f32, 1.25, 1.5, 2.0] {
            let physical = logical_to_physical((640, 420), scale);
            let logical = physical_to_logical(PhysicalSize::new(physical.0, physical.1), scale);
            assert_eq!(logical, (640, 420), "scale {scale}");
        }
    }

    #[test]
    fn degenerate_scale_is_treated_as_one() {
        assert_eq!(physical_to_logical(PhysicalSize::new(640, 420), 0.0), (640, 420));
        assert_eq!(logical_to_physical((640, 420), -1.0), (640, 420));
        assert_eq!(cursor_to_logical(10.0, 20.0, 0.0), (10.0, 20.0));
    }

    #[test]
    fn cursor_physical_maps_to_logical() {
        assert_eq!(cursor_to_logical(1280.0, 840.0, 2.0), (640.0, 420.0));
        assert_eq!(cursor_to_logical(960.0, 630.0, 1.5), (640.0, 420.0));
    }

    // -----------------------------------------------------------------
    // Key mapping
    // -----------------------------------------------------------------

    #[test]
    fn shift_tab_maps_to_backtab() {
        let tab = Key::Named(NamedKey::Tab);
        assert_eq!(map_key(&tab, false), Some(UiKey::Tab));
        assert_eq!(map_key(&tab, true), Some(UiKey::BackTab));
    }

    #[test]
    fn common_named_keys_map() {
        assert_eq!(map_key(&Key::Named(NamedKey::Escape), false), Some(UiKey::Escape));
        assert_eq!(map_key(&Key::Named(NamedKey::Enter), false), Some(UiKey::Enter));
        assert_eq!(map_key(&Key::Named(NamedKey::Backspace), false), Some(UiKey::Backspace));
        assert_eq!(map_key(&Key::Named(NamedKey::ArrowUp), false), Some(UiKey::ArrowUp));
        assert_eq!(map_key(&Key::Named(NamedKey::ArrowDown), false), Some(UiKey::ArrowDown));
    }

    #[test]
    fn character_keys_map_to_text_ignoring_shift() {
        assert_eq!(
            map_key(&Key::Character("a".into()), false),
            Some(UiKey::Char("a".into()))
        );
        assert_eq!(
            map_key(&Key::Character("A".into()), true),
            Some(UiKey::Char("A".into()))
        );
        assert_eq!(map_key(&Key::Character("".into()), false), None);
    }
}

//! Window backend: winit (xdg-toplevel on Wayland, also X11/dev fallback).
//!
//! This is one of two interchangeable frontends. It translates winit events
//! into `UiKey`/`UiMods`/clicks and lets `frontend::AppCore` do everything else.
//! The layer-shell backend (`wayland.rs`) shares the same core.

use crate::frontend::{AppCore, Outcome, UiKey, UiMods};
use anyhow::Result;
use softbuffer::{Context, Surface};
use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
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
        size,
        scale: 1.0,
        cursor_pos: None,
        modifiers: ModifiersState::empty(),
        next_frame: None,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

struct WinitApp {
    core: Option<AppCore>,
    window: Option<Rc<Window>>,
    context: Option<Context<Rc<Window>>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
    size: (u32, u32),
    scale: f32,
    cursor_pos: Option<(f64, f64)>,
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
        let (w, h) = self.size;
        let pixmap = self.core().runtime.render_nodes(&nodes, (w, h), scale);
        let Some(surface) = &mut self.surface else { return };
        let _ = surface.resize(NonZeroU32::new(w.max(1)).unwrap(), NonZeroU32::new(h.max(1)).unwrap());
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
        } else if core.last_uses_time_debug() || core.needs_frame() {
            Some(Instant::now() + Duration::from_millis(45))
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
}

impl ApplicationHandler for WinitApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            let transparent = self.core.as_ref().unwrap().config_theme_transparent();
            let attrs = Window::default_attributes()
                .with_title("hiren")
                .with_inner_size(LogicalSize::new(self.size.0, self.size.1))
                .with_decorations(false)
                .with_transparent(transparent)
                .with_window_level(winit::window::WindowLevel::AlwaysOnTop)
                .with_resizable(false);
            match event_loop.create_window(attrs) {
                Ok(w) => {
                    let w = Rc::new(w);
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
        if let Some(w) = &self.window {
            let due = self.next_frame.map(|t| Instant::now() >= t).unwrap_or(false);
            if due {
                w.request_redraw();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {        match event {
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
            WindowEvent::KeyboardInput { event: KeyEvent { logical_key, state: ElementState::Pressed, text, .. }, .. } => {
                let mods = self.mods();
                let Some(key) = map_key(&logical_key, text.as_deref()) else { return };
                let outcome = self.core().handle_key(key, mods);
                self.maybe_exit(outcome, event_loop);
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::Ime(Ime::Commit(text)) => {
                if !text.is_empty() {
                    let mods = self.mods();
                    let outcome = self.core().handle_key(UiKey::Char(text.clone()), mods);
                    self.maybe_exit(outcome, event_loop);
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            WindowEvent::Ime(Ime::Preedit(_, _)) => {}
            WindowEvent::MouseInput { state: ElementState::Pressed, button: winit::event::MouseButton::Left, .. } => {
                if let Some((x, y)) = self.cursor_pos {
                    let mods = self.mods();
                    let outcome = self.core().handle_click(x, y, mods);
                    self.maybe_exit(outcome, event_loop);
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = Some((position.x, position.y));
                self.core().handle_hover(position.x, position.y);
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::Resized(size) => {
                // The compositor may resize (e.g. scale changes); accept it.
                if size.width > 0 && size.height > 0 && (size.width != self.size.0 || size.height != self.size.1) {
                    self.size = (size.width, size.height);
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if scale_factor > 0.0 {
                    self.scale = scale_factor as f32;
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            _ => {}
        }
    }
}

fn map_key(key: &Key, text: Option<&str>) -> Option<UiKey> {
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
        Key::Named(NamedKey::Tab) => Some(UiKey::Tab),
        Key::Character(s) => {
            if s.is_empty() {
                None
            } else if s == "\t" {
                Some(UiKey::Tab)
            } else {
                let _ = text;
                Some(UiKey::Char(s.to_string()))
            }
        }
        _ => None,
    }
}

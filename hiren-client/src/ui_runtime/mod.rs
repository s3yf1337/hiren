//! Hiren UI Runtime — declarative, state-driven UI engine.
//!
//! Data flow, once per frame:
//!
//! ```text
//! ObservableState ──► layout::resolve ──► ResolvedNodes ──► animations ──► Renderer
//!      (launcher)       (bindings)          (concrete)       (spring/ease)   (tiny-skia)
//! ```
//!
//! The runtime is deliberately small and built on existing primitives:
//!   - winit / wayland-client : window + input (two interchangeable frontends)
//!   - softbuffer             : CPU pixel buffer → surface
//!   - tiny-skia              : vector rendering (rects, gradients, shadows, paths)
//!   - cosmic-text            : text shaping, measurement, rasterization
//!   - toml/serde + meval     : theme definition (no custom language)
//!
//! Themes are TOML files describing a scene graph whose every property is a
//! binding against launcher state — the runtime contains no hardcoded UI.

pub mod animation;
pub mod binding;
pub mod color;
pub mod layout;
pub mod node;
pub mod render;
pub mod state_bridge;
pub mod text;
pub mod theme;

use crate::launcher::{LauncherState, ObservableState};
use render::Renderer;
use std::cell::RefCell;
use std::rc::Rc;
use theme::Theme;

/// The UI runtime owns the theme, text engine, animation clock and diagnostics.
pub struct UiRuntime {
    theme: Theme,
    state: Rc<ObservableState>,
    start: std::time::Instant,
    anim: RefCell<animation::AnimationState>,
    diag: binding::SharedDiag,
    /// Renderer (owns the text engine; bindings measure through it too).
    renderer: RefCell<Renderer>,
}

impl UiRuntime {
    pub fn new(theme: Theme, state: Rc<ObservableState>) -> Self {
        Self {
            theme,
            state,
            start: std::time::Instant::now(),
            anim: RefCell::new(animation::AnimationState::default()),
            diag: Rc::new(RefCell::new(binding::Diag::default())),
            renderer: RefCell::new(Renderer::new()),
        }
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }
    pub fn state_snapshot(&self) -> LauncherState {
        self.state.get()
    }
    pub fn elapsed(&self) -> f32 {
        self.start.elapsed().as_secs_f32()
    }

    /// Binding-evaluation warnings collected during resolve (theme diagnostics).
    pub fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut self.diag.borrow_mut().warnings)
    }

    /// Hot-reload the theme if its file changed and reloading is enabled.
    pub fn try_reload(&mut self, enabled: bool) -> bool {
        if !enabled {
            return false;
        }
        if let Some(path) = self.theme.source_path.clone() {
            if let Ok(mtime) = std::fs::metadata(&path).and_then(|m| m.modified()) {
                if mtime > self.theme.loaded_at {
                    match Theme::load_from_file(&path) {
                        Ok(new_theme) => {
                            self.theme = new_theme;
                            log::info!("Theme reloaded: {}", path.display());
                            return true;
                        }
                        Err(e) => log::warn!("Theme reload failed (keeping old): {e:#}"),
                    }
                }
            }
        }
        false
    }

    /// Resolve the current frame: bindings → concrete nodes → animations.
    pub fn resolve(&mut self, window_size: (u32, u32)) -> layout::ResolveOutput {
        let snap = self.state.get();
        let elapsed = self.elapsed();
        let theme = &self.theme;
        let text = self.renderer.borrow().text.clone();
        let measurer = move |t: &str, size: f32| text.measure_default_weight(t, size);
        let mut out = layout::resolve(
            theme,
            &snap,
            window_size,
            elapsed,
            Some(&measurer),
            Some(self.diag.clone()),
        );

        // Resolve image src paths relative to the theme directory.
        let base = self.theme.base_dir.clone();
        for n in out.nodes.iter_mut() {
            if let Some(src) = n.props.get_mut("src") {
                let p = std::path::Path::new(src.trim().trim_matches('"').trim_matches('\''));
                if p.is_relative() {
                    *src = base.join(p).to_string_lossy().into_owned();
                }
            }
        }

        // Apply declared animations to x/y/width/height/opacity/rotation/scale.
        let mut anim = self.anim.borrow_mut();
        for n in out.nodes.iter_mut() {
            for a in &n.animate {
                let key = format!("{}:{}", n.id, a.property);
                let target = match a.property.as_str() {
                    "x" => n.x,
                    "y" => n.y,
                    "width" => n.width,
                    "height" => n.height,
                    "opacity" => n.opacity,
                    "rotation" => n.rotation,
                    "scale" => n.scale,
                    other => {
                        let (id, prop) = (n.id.clone(), other.to_string());
                        self.diag.borrow_mut().warn(&id, "animate", &format!("unknown property `{prop}`"));
                        continue;
                    }
                };
                let v = anim.animate(&key, target, a.duration_ms, a.delay_ms, &a.easing, a.spring.clone(), a.from_value);
                match a.property.as_str() {
                    "x" => n.x = v,
                    "y" => n.y = v,
                    "width" => n.width = v,
                    "height" => n.height = v,
                    "opacity" => n.opacity = v.clamp(0.0, 1.0),
                    "rotation" => n.rotation = v,
                    "scale" => n.scale = v,
                    _ => {}
                }
            }
        }
        anim.tick();
        out
    }

    /// True while any declared transition/spring is still in motion.
    pub fn animating(&self) -> bool {
        self.anim.borrow().is_active()
    }

    /// Resolve one frame and bookkeep warnings/animation state. Rendering is
    /// the caller's job (`render_nodes`) — the resolved nodes are returned so
    /// each backend rasterizes exactly once at its own pixel size.
    pub fn render_frame(&mut self, window_size: (u32, u32), _scale: f32) -> layout::ResolveOutput {
        self.resolve(window_size)
    }

    /// Render already-resolved nodes (used by the screenshot mode).
    pub fn render_nodes(&mut self, nodes: &[node::ResolvedNode], window_size: (u32, u32), scale: f32) -> tiny_skia::Pixmap {
        self.renderer.borrow_mut().render(nodes, window_size.0, window_size.1, scale)
    }
}

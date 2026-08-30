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
pub mod icon;
pub mod layout;
pub mod node;
pub mod render;
pub mod state_bridge;
pub mod text;
pub mod theme;

use crate::launcher::{LauncherState, ObservableState};
use binding::{hit_envelope, Impulse};
use render::Renderer;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};
use theme::Theme;

/// Selection / query impact clock. Spikes `hit` on change, then decays.
struct ImpulseClock {
    started: bool,
    last_sel: usize,
    last_query: String,
    select_at: Instant,
    type_at: Instant,
    select_gen: u32,
    type_gen: u32,
}

impl ImpulseClock {
    fn new() -> Self {
        let ago = Instant::now()
            .checked_sub(Duration::from_secs(10))
            .unwrap_or_else(Instant::now);
        Self {
            started: false,
            last_sel: 0,
            last_query: String::new(),
            select_at: ago,
            type_at: ago,
            select_gen: 0,
            type_gen: 0,
        }
    }

    fn sync(&mut self, snap: &LauncherState) -> Impulse {
        let now = Instant::now();
        if !self.started {
            self.started = true;
            self.last_sel = snap.selected_index;
            self.last_query = snap.query.clone();
            self.select_at = now;
            self.select_gen = 1;
            // Opening slam is a select hit, not a type hit.
        } else {
            if snap.selected_index != self.last_sel {
                self.last_sel = snap.selected_index;
                self.select_at = now;
                self.select_gen = self.select_gen.wrapping_add(1);
            }
            if snap.query != self.last_query {
                self.last_query = snap.query.clone();
                self.type_at = now;
                self.type_gen = self.type_gen.wrapping_add(1);
            }
        }
        Impulse {
            hit: hit_envelope(now.duration_since(self.select_at).as_secs_f32()),
            hit_type: hit_envelope(now.duration_since(self.type_at).as_secs_f32()),
            since_select: now.duration_since(self.select_at).as_secs_f32(),
            since_type: now.duration_since(self.type_at).as_secs_f32(),
            select_gen: self.select_gen,
            type_gen: self.type_gen,
        }
    }

    fn active(&self) -> bool {
        hit_envelope(self.select_at.elapsed().as_secs_f32()) > 0.02
            || hit_envelope(self.type_at.elapsed().as_secs_f32()) > 0.02
    }
}

/// The UI runtime owns the theme, text engine, animation clock and diagnostics.
pub struct UiRuntime {
    theme: Theme,
    state: Rc<ObservableState>,
    start: Instant,
    anim: RefCell<animation::AnimationState>,
    diag: binding::SharedDiag,
    /// Renderer (owns the text engine; bindings measure through it too).
    renderer: RefCell<Renderer>,
    impulse: ImpulseClock,
}

impl UiRuntime {
    pub fn new(theme: Theme, state: Rc<ObservableState>) -> Self {
        let renderer = Renderer::new();
        renderer.text.load_fonts_from_dir(&theme.base_dir);
        Self {
            theme,
            state,
            start: std::time::Instant::now(),
            anim: RefCell::new(animation::AnimationState::default()),
            diag: Rc::new(RefCell::new(binding::Diag::default())),
            renderer: RefCell::new(renderer),
            impulse: ImpulseClock::new(),
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
                            self.renderer.borrow().text.load_fonts_from_dir(&self.theme.base_dir);
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
        let impulse = self.impulse.sync(&snap);
        let theme = &self.theme;
        let text = self.renderer.borrow().text.clone();
        let measurer = move |t: &str, size: f32, family: &str| text.measure(t, size, cosmic_text::Weight::NORMAL, family);
        let mut out = layout::resolve_with(
            theme,
            &snap,
            window_size,
            elapsed,
            impulse,
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

        // Apply declared animations to x/y/width/height/opacity/rotation/scale/skew.
        let mut anim = self.anim.borrow_mut();
        for n in out.nodes.iter_mut() {
            for a in &n.animate {
                let gen = match a.trigger.as_deref() {
                    Some("select") => format!("#s{}", impulse.select_gen),
                    Some("type") => format!("#t{}", impulse.type_gen),
                    Some(other) => {
                        let (id, t) = (n.id.clone(), other.to_string());
                        self.diag.borrow_mut().warn(&id, "animate", &format!("unknown trigger `{t}`"));
                        String::new()
                    }
                    None => String::new(),
                };
                let key = format!("{}:{}{}", n.id, a.property, gen);
                let target = match a.property.as_str() {
                    "x" => n.x,
                    "y" => n.y,
                    "width" => n.width,
                    "height" => n.height,
                    "opacity" => n.opacity,
                    "rotation" => n.rotation,
                    "scale" => n.scale,
                    "skew" => n.skew,
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
                    "skew" => n.skew = v,
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

    /// True while a select/type impact envelope is still decaying.
    pub fn impulse_active(&self) -> bool {
        self.impulse.active()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launcher::{LauncherState, ObservableState};
    use hiren_shared::AppEntry;
    use std::time::Duration;

    #[test]
    fn opening_slam_decays_and_retriggers_on_select() {
        let state = ObservableState::new(LauncherState::new());
        state.update(|s| {
            s.set_results(
                (0..3)
                    .map(|i| AppEntry::run(format!("id{i}"), format!("App{i}"), format!("app{i}")))
                    .collect(),
            );
            s.selected_index = 0;
        });
        let mut rt = UiRuntime::new(Theme::fallback(), state.clone());
        assert!(!rt.impulse_active(), "clock starts cold");
        let _ = rt.resolve((640, 400));
        assert!(rt.impulse_active(), "first frame is an opening slam");
        std::thread::sleep(Duration::from_millis(400));
        let _ = rt.resolve((640, 400));
        assert!(!rt.impulse_active(), "envelope decayed");
        state.update(|s| s.selected_index = 1);
        let _ = rt.resolve((640, 400));
        assert!(rt.impulse_active(), "moving selection retriggers hit");
    }

    #[test]
    fn animate_trigger_select_replays_from() {
        let raw = r#"
            [meta]
            name = "t"
            [[nodes]]
            id = "card"
            type = "Rectangle"
            x = "10"
            y = "10"
            width = "40"
            height = "40"
            scale = "1"
            animate = [{ property = "scale", from = 1.4, duration_ms = 200, trigger = "select", easing = "linear" }]
        "#;
        let theme: Theme = toml::from_str(raw).expect("theme");
        let state = ObservableState::new(LauncherState::new());
        state.update(|s| {
            s.set_results(vec![
                AppEntry::run("a".into(), "A".into(), "a".into()),
                AppEntry::run("b".into(), "B".into(), "b".into()),
            ]);
        });
        let mut rt = UiRuntime::new(theme, state.clone());
        let a = rt.resolve((200, 200));
        let s0 = a.nodes.iter().find(|n| n.id == "card").unwrap().scale;
        assert!(s0 > 1.2, "opening trigger plays from=1.4, got {s0}");
        std::thread::sleep(Duration::from_millis(250));
        let b = rt.resolve((200, 200));
        let s1 = b.nodes.iter().find(|n| n.id == "card").unwrap().scale;
        assert!((s1 - 1.0).abs() < 0.05, "settled at 1, got {s1}");
        state.update(|s| s.selected_index = 1);
        let c = rt.resolve((200, 200));
        let s2 = c.nodes.iter().find(|n| n.id == "card").unwrap().scale;
        assert!(s2 > 1.2, "select trigger replays from=1.4, got {s2}");
    }
}

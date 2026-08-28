//! Frontend core — launcher behavior shared by all window backends.
//!
//! `AppCore` owns the bridge between the UI runtime and launcher logic. Window
//! backends (winit toplevel, wlr layer-shell) translate their events into
//! `UiKey`/`UiMods`/click coordinates and call into this; nothing backend-
//! specific leaks into launcher behavior, and nothing here knows Wayland.

use crate::config::LauncherConfig;
use crate::launcher::ObservableState;
use crate::ui_runtime::node::{NodeAction, ResolvedNode};
use crate::ui_runtime::state_bridge::SearchBridge;
use crate::ui_runtime::{theme::Theme, UiRuntime};
use hiren_shared::{AppEntry, AppMode};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Backend-agnostic key event.
#[derive(Debug, Clone, PartialEq)]
pub enum UiKey {
    Char(String),
    Backspace,
    Escape,
    Enter,
    ArrowUp,
    ArrowDown,
    Home,
    End,
    PageUp,
    PageDown,
    Tab,
    BackTab,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct UiMods {
    pub ctrl: bool,
    pub alt: bool,
    pub super_key: bool,
    pub shift: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    None,
    /// Launcher finished (close requested or a launch is in progress).
    Exit,
}

pub struct AppCore {
    pub runtime: UiRuntime,
    pub state: Rc<ObservableState>,
    pub bridge: SearchBridge,
    pub config: LauncherConfig,
    /// Whether theme hot-reload polling is enabled (dev convenience).
    pub reload: bool,
    input_buffer: String,
    auto_close: Option<Instant>,
    last_nodes: Vec<ResolvedNode>,
    last_uses_time: bool,
    size: (u32, u32),
}

impl AppCore {
    pub fn new(
        theme: Theme,
        state: Rc<ObservableState>,
        bridge: SearchBridge,
        config: LauncherConfig,
        size_override: Option<(u32, u32)>,
        reload: bool,
    ) -> Self {
        let size = size_override.unwrap_or_else(|| {
            theme
                .window
                .effective_size((config.window_width.max(1) as u32, config.window_height.max(1) as u32))
        });
        let runtime = UiRuntime::new(theme, state.clone());
        // Initial population (empty query → recent apps / calc / etc.)
        bridge.search("", &state);
        let auto_close = if config.auto_close_timeout_secs > 0 {
            Some(Instant::now() + Duration::from_secs(config.auto_close_timeout_secs))
        } else {
            None
        };
        Self {
            runtime,
            state,
            bridge,
            config,
            reload,
            input_buffer: String::new(),
            auto_close,
            last_nodes: Vec::new(),
            last_uses_time: false,
            size,
        }
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Adopt a new logical window size (layer-shell configure, reload).
    pub fn set_size(&mut self, w: u32, h: u32) {
        if w > 0 && h > 0 && (w, h) != self.size {
            self.size = (w, h);
        }
    }

    /// True while an auto-close deadline is pending (idle wake-ups needed).
    pub fn auto_close_pending(&self) -> bool {
        self.auto_close.is_some()
    }

    /// Debug access for the loop instrumentation (HIREN_LOOP_DEBUG=1).
    pub fn last_uses_time_debug(&self) -> bool {
        self.last_uses_time
    }

    /// Window transparency preference from the theme.
    pub fn config_theme_transparent(&self) -> bool {
        self.runtime.theme().window.transparent
    }

    pub fn auto_close_expired(&self) -> bool {
        self.auto_close.map(|d| Instant::now() >= d).unwrap_or(false)
    }

    /// Keep rendering while animations are running or the theme uses `time`.
    pub fn needs_frame(&self) -> bool {
        self.runtime.animating() || self.last_uses_time
    }

    /// Resolve + render the current frame; returns resolved nodes.
    pub fn render_frame(&mut self, scale: f32) -> &[ResolvedNode] {
        let out = self.runtime.render_frame(self.size, scale);
        if self.reload {
            if self.runtime.try_reload(true) {
                // Re-resolve with the fresh theme on the next frame.
                self.size = self.runtime.theme().window.effective_size((self.config.window_width.max(1) as u32, self.config.window_height.max(1) as u32));
            }
        }
        for w in self.runtime.take_warnings() {
            log::warn!("theme binding: {w}");
        }
        self.last_nodes = out.nodes;
        self.last_uses_time = out.uses_time;
        &self.last_nodes
    }

    // -----------------------------------------------------------------
    // Query / selection
    // -----------------------------------------------------------------

    fn reset_idle_timer(&mut self) {
        if self.config.auto_close_timeout_secs > 0 {
            self.auto_close = Some(Instant::now() + Duration::from_secs(self.config.auto_close_timeout_secs));
        }
    }

    fn set_query(&mut self, q: String) {
        self.reset_idle_timer();
        self.input_buffer = q.clone();
        self.bridge.search(&q, &self.state);
    }

    fn move_selection(&mut self, delta: i32) {
        self.reset_idle_timer();
        self.state.update(|s| s.select_next(delta));
    }

    fn select(&mut self, idx: usize) {
        self.reset_idle_timer();
        self.state.update(|s| s.select(idx));
    }

    // -----------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------

    pub fn handle_key(&mut self, key: UiKey, mods: UiMods) -> Outcome {
        self.reset_idle_timer();
        match key {
            UiKey::Escape => return Outcome::Exit,
            UiKey::Enter => {
                let prefix = self.enter_prefix(mods.ctrl);
                return self.activate(None, prefix);
            }
            UiKey::ArrowDown => self.move_selection(1),
            UiKey::ArrowUp => self.move_selection(-1),
            UiKey::Tab => self.move_selection(1),
            UiKey::BackTab => self.move_selection(-1),
            UiKey::Home => {
                let s = self.state.get();
                if !s.results.is_empty() {
                    self.select(0);
                }
            }
            UiKey::End => {
                let s = self.state.get();
                if !s.results.is_empty() {
                    self.select(s.results.len() - 1);
                }
            }
            UiKey::PageDown => self.move_selection(6),
            UiKey::PageUp => self.move_selection(-6),
            UiKey::Backspace => {
                if mods.ctrl {
                    self.set_query(String::new());
                } else {
                    let mut q = self.input_buffer.clone();
                    q.pop();
                    self.set_query(q);
                }
            }
            UiKey::Char(s) => {
                if mods.ctrl || mods.alt || mods.super_key {
                    return Outcome::None;
                }
                let mut q = self.input_buffer.clone();
                q.push_str(&s);
                self.set_query(q);
            }
        }
        Outcome::None
    }

    fn enter_prefix(&self, ctrl: bool) -> Option<String> {
        let flag = if ctrl { crate::config::Modifiers::CTRL } else { 0 };
        self.config
            .bindings
            .iter()
            .find(|b| b.key_name == "return" && (b.modifiers.0 & flag) == flag && (b.modifiers.0 & !flag) == 0)
            .and_then(|b| b.prefix.clone())
    }

    /// Click at logical (x, y): run the action of the topmost hit node.
    pub fn handle_click(&mut self, x: f64, y: f64, mods: UiMods) -> Outcome {
        let Some(node) = self.topmost_actionable(x, y) else {
            return Outcome::None;
        };
        let action = node.action.clone().expect("checked");
        self.reset_idle_timer();
        match action {
            NodeAction::Activate(idx) => {
                let prefix = if mods.ctrl { self.enter_prefix(true) } else { self.enter_prefix(false) };
                self.activate(idx, prefix)
            }
            NodeAction::Select(idx) => match idx {
                Some(i) => {
                    self.select(i);
                    Outcome::None
                }
                None => Outcome::None,
            },
            NodeAction::Move(d) => {
                self.move_selection(d);
                Outcome::None
            }
            NodeAction::SetQuery(q) => {
                self.set_query(q);
                Outcome::None
            }
            NodeAction::Close => Outcome::Exit,
        }
    }

    /// Hover over a result node changes selection to it.
    pub fn handle_hover(&mut self, x: f64, y: f64) {
        if let Some(node) = self.topmost_actionable(x, y) {
            if let Some(i) = node.action.as_ref().and_then(|a| a.targeted_index()) {
                let current = self.state.with(|s| s.selected_index);
                if i != current {
                    self.select(i);
                }
            }
        }
    }

    fn topmost_actionable(&self, x: f64, y: f64) -> Option<&ResolvedNode> {
        self.last_nodes
            .iter()
            .rev()
            .find(|n| n.visible && n.opacity > 0.1 && n.action.is_some() && n.hit(x, y))
    }

    // -----------------------------------------------------------------
    // Activation / launch (launcher-owned functionality)
    // -----------------------------------------------------------------

    fn activate(&mut self, idx: Option<usize>, prefix: Option<String>) -> Outcome {
        let entry = {
            let s = self.state.get();
            let i = idx.unwrap_or(s.selected_index);
            s.results.get(i).cloned()
        };
        let Some(entry) = entry else { return Outcome::None }; // nothing to launch — stay open
        if let Some(i) = idx {
            self.state.update(|s| s.select(i));
        }
        self.launch(&entry, prefix);
        Outcome::Exit
    }

    fn launch(&mut self, entry: &AppEntry, prefix: Option<String>) {
        if entry.mode == AppMode::Calc {
            let result = entry.exec.clone();
            let cmd = if crate::config::cmd_exists("wl-copy") {
                format!("printf '%s' '{}' | wl-copy", result.replace('\'', "'\\''"))
            } else if crate::config::cmd_exists("xclip") {
                format!("printf '%s' '{}' | xclip -selection clipboard", result.replace('\'', "'\\''"))
            } else {
                return;
            };
            eprintln!("[hiren-client] Calc result copied: {}", result);
            crate::modes::exec_detached(&cmd);
            return;
        }
        if entry.mode == AppMode::Window {
            if let Some(w) = self.bridge.sources.get(&AppMode::Window) {
                w.execute(entry, &self.config);
            }
            return;
        }
        let exec = match prefix {
            Some(p) => format!("{p} {}", entry.exec),
            None => entry.exec.clone(),
        };
        self.bridge.record_launch(entry);
        eprintln!("[hiren-client] Launching: {exec}");
        crate::modes::exec_detached(&exec);
    }
}

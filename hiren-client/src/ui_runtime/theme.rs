//! Theme definition — TOML scene graph.
//!
//! A theme is a collection of nodes (visual objects) plus global settings.
//! Nodes can be:
//!   - Container   (layout, stacking, clipping)
//!   - Rectangle   (fill, radius, border, shadow)
//!   - Polygon     (fill, border; `points` in node-local coords)
//!   - Text        (content, family, size, color, align, outline, hard shadow)
//!   - Image/Icon  (icon name, path, size)
//!   - TextInput   (bound to launcher.query)
//!   - Repeater    (model = launcher.results, delegate = component id)
//!   - Selector    (independent visual object that follows selection via bindings)
//!
//! Bindings are strings evaluated at runtime via `binding::eval`.
//! Animations are declared per-property with duration/easing.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

fn now_system() -> SystemTime { SystemTime::now() }
fn default_base() -> PathBuf { PathBuf::from(".") }

#[derive(Debug, Clone, Deserialize)]
pub struct Theme {
    #[serde(default)] pub meta: ThemeMeta,
    #[serde(default)] pub window: WindowTheme,
    #[serde(default)] pub animations: AnimationsConfig,
    #[serde(default)] pub nodes: Vec<NodeDef>,
    #[serde(default)] pub components: HashMap<String, ComponentDef>,
    /// Not serialized: where this theme was loaded from
    #[serde(skip)] pub source_path: Option<PathBuf>,
    #[serde(skip, default = "now_system")] pub loaded_at: SystemTime,
    #[serde(skip, default = "default_base")] pub base_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ThemeMeta {
    #[serde(default)] pub name: String,
    #[serde(default)] pub description: String,
    #[serde(default)] pub author: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WindowTheme {
    #[serde(default = "default_window_width")] pub width: Option<u32>,
    #[serde(default = "default_window_height")] pub height: Option<u32>,
    #[serde(default)] pub transparent: bool,
    #[serde(default)] pub blur: bool,
    #[serde(default)] pub background: Option<String>, // e.g. "rgba(30,30,46,0.92)"
    /// wlr-layer-shell layer when running on the layer-shell backend.
    #[serde(default = "default_layer")] pub layer: String, // overlay | top | bottom | background
    /// Anchoring: "center" (all edges anchored, fixed size → centered) or "top"/"bottom".
    #[serde(default = "default_anchor")] pub anchor: String,
    /// Target Hz for `time`-driven themes when no spring is in motion.
    /// `None` keeps the ~20 fps idle throttle (caret blink). Set `60` for
    /// sharp caret + impact frames (atlus). Springs already run at vsync.
    #[serde(default)] pub time_hz: Option<u32>,
}
impl Default for WindowTheme {
    fn default() -> Self {
        Self { width: None, height: None, transparent: true, blur: false, background: None, layer: default_layer(), anchor: default_anchor(), time_hz: None }
    }
}
impl WindowTheme {
    /// Effective size: theme value, else fallback (from config or default).
    pub fn effective_size(&self, fallback: (u32, u32)) -> (u32, u32) {
        (self.width.unwrap_or(fallback.0).max(1), self.height.unwrap_or(fallback.1).max(1))
    }
}
fn default_window_width() -> Option<u32> { None }
fn default_window_height() -> Option<u32> { None }
fn default_layer() -> String { "overlay".into() }
fn default_anchor() -> String { "center".into() }

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AnimationsConfig {
    #[serde(default)] pub default_duration_ms: Option<u32>,
    #[serde(default)] pub default_easing: Option<String>,
}

// ---------------------------------------------------------------------------
// Nodes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct NodeDef {
    pub id: String,
    #[serde(rename = "type")] pub kind: String, // Container, Rectangle, Text, TextInput, Repeater, Image, Selector...
    #[serde(default)] pub x: Option<String>,      // binding expr or literal "20"
    #[serde(default)] pub y: Option<String>,
    #[serde(default)] pub width: Option<String>,
    #[serde(default)] pub height: Option<String>,
    #[serde(default)] pub visible: Option<String>, // binding -> bool
    #[serde(default)] pub opacity: Option<String>,
    #[serde(default)] pub props: HashMap<String, String>, // arbitrary typed props as binding strings

    // children for Containers (inline)
    #[serde(default)] pub children: Vec<NodeDef>,

    // repeater specifics
    #[serde(default)] pub model: Option<String>,    // e.g. "launcher.results"
    #[serde(default)] pub delegate: Option<String>, // component id

    // text specifics via props as well, but allow top-level for convenience
    #[serde(default)] pub text: Option<String>,
    #[serde(default)] pub placeholder: Option<String>,
    /// Case-fold resolved text: "upper" | "lower" (P5 sets menus in caps).
    #[serde(default)] pub text_case: Option<String>,

    // transform (bindings, applied around the node center)
    #[serde(default)] pub rotation: Option<String>, // degrees
    #[serde(default)] pub skew: Option<String>,     // horizontal shear, degrees (parallelogram lean)
    #[serde(default)] pub scale: Option<String>,    // uniform factor
    /// Polygon vertices in node-local coordinates, `;`-separated pairs:
    /// `points = "0,0; 120,6; 120,46; 0,52"` (each coordinate is a binding).
    #[serde(default)] pub points: Option<String>,

    // animation overrides
    #[serde(default)] pub animate: Vec<AnimateDef>,

    // z-order
    #[serde(default)] pub z: Option<i32>,

    // interaction — action executed when this node is clicked.
    // Grammar: "activate" | "select" | "move_selection(±1)" | "set_query('...')" | "close".
    // Inside repeaters the arg may use locals: "activate(index)".
    #[serde(default)] pub on_click: Option<String>,
}

/// Delay: fixed ms or a binding expression (stagger), e.g. `delay = "index * 30"`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum DelaySpec {
    Fixed(u32),
    Expr(String),
}

/// From-value: fixed number or a binding expression, e.g. `from = "window.width"`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum FromSpec {
    Fixed(f32),
    Expr(String),
}

    #[derive(Debug, Clone, Deserialize)]
pub struct AnimateDef {
    pub property: String, // x, y, width, height, opacity, rotation, scale, skew
    #[serde(default = "default_duration")] pub duration_ms: u32,
    #[serde(default)] pub delay_ms: u32,
    /// Per-instance delay (stagger): fixed ms or binding evaluated per repeater instance.
    #[serde(default)] pub delay: Option<DelaySpec>,
    /// Value to animate from on first appearance (enter animation).
    /// Evaluated at layout time and stored in `from_value`.
    #[serde(default)] pub from: Option<FromSpec>,
    #[serde(skip)] pub from_value: Option<f32>,
    #[serde(default = "default_easing")] pub easing: String,
    #[serde(default)] pub spring: Option<SpringDef>,
    /// Replay `from` → target when this event fires: `"select"` | `"type"`.
    /// Default (unset) is first appearance only.
    #[serde(default)] pub trigger: Option<String>,
}
fn default_duration() -> u32 { 200 }
fn default_easing() -> String { "ease_out_cubic".into() }
#[derive(Debug, Clone, Deserialize)]
pub struct SpringDef { pub damping: f32, pub stiffness: f32, pub mass: f32 }

#[derive(Debug, Clone, Deserialize)]
pub struct ComponentDef {
    pub nodes: Vec<NodeDef>,
    #[serde(default)] pub width: Option<String>,
    #[serde(default)] pub height: Option<String>,
    /// Animations applied to every node of the component (merged with node-level ones).
    /// Useful for entrance staggers: `delay = "index * 30"`.
    #[serde(default)] pub animate: Vec<AnimateDef>,
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

impl Theme {
    pub fn load_from_file(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).with_context(|| format!("read theme {}", path.display()))?;
        let mut theme: Theme = toml::from_str(&raw).with_context(|| format!("parse theme {}", path.display()))?;
        theme.source_path = Some(path.to_path_buf());
        theme.loaded_at = SystemTime::now();
        theme.base_dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        theme.validate()?;
        Ok(theme)
    }

    pub fn load_from_dir(dir: &Path) -> Result<Self> {
        // try theme.toml, then main.toml
        let candidates = [dir.join("theme.toml"), dir.join("main.toml"), dir.join("theme/main.toml")];
        for p in &candidates {
            if p.exists() { return Self::load_from_file(p); }
        }
        // if dir itself is a file (legacy single file theme)
        if dir.is_file() { return Self::load_from_file(dir); }
        anyhow::bail!("No theme file found in {}", dir.display());
    }

    /// Validate minimal correctness, provide diagnostics.
    pub fn validate(&self) -> Result<()> {
        let mut ids = std::collections::HashSet::new();
        for n in &self.nodes {
            if !ids.insert(n.id.clone()) {
                anyhow::bail!("Duplicate node id: {}", n.id);
            }
        }
        // check repeater delegates exist
        for n in &self.nodes {
            if n.kind == "Repeater" {
                if let Some(d) = &n.delegate {
                    if !self.components.contains_key(d) {
                        anyhow::bail!("Repeater '{}' references unknown component '{}'", n.id, d);
                    }
                }
            }
        }
        Ok(())
    }

    /// Built-in fallback theme (minimal vertical launcher) — guarantees window even if theme broken.
    pub fn fallback() -> Self {
        // Hardcoded minimal fallback without file dependency
        Theme {
            meta: ThemeMeta { name: "fallback".into(), description: "fallback built-in".into(), author: "hiren".into() },
            window: WindowTheme::default(),
            animations: AnimationsConfig::default(),
            nodes: vec![
                NodeDef {
                    id: "search".into(),
                    kind: "Text".into(),
                    x: Some("20".into()),
                    y: Some("20".into()),
                    width: Some("window.width - 40".into()),
                    height: Some("40".into()),
                    visible: None,
                    opacity: None,
                    props: {
                        let mut m = HashMap::new();
                        m.insert("color".into(), "#ffffff".into());
                        m.insert("font_size".into(), "16".into());
                        m
                    },
                    children: vec![],
                    model: None,
                    delegate: None,
                    text: Some("launcher.query".into()),
                    placeholder: None,
                    text_case: None,
                    rotation: None,
                    skew: None,
                    scale: None,
                    points: None,
                    animate: vec![],
                    z: None,
                    on_click: None,
                }
            ],
            components: HashMap::new(),
            source_path: None,
            loaded_at: SystemTime::now(),
            base_dir: PathBuf::from("."),
        }
    }
}

impl Default for Theme {
    fn default() -> Self { Self::fallback() }
}

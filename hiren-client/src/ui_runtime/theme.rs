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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct ThemeMeta {
    #[serde(default)] pub name: String,
    #[serde(default)] pub description: String,
    #[serde(default)] pub author: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
        // Also clamp at use time: `Theme::fallback()` and callers that mutate
        // the struct do not go through `validate()`.
        (
            self.width.unwrap_or(fallback.0).clamp(1, MAX_WINDOW_SIZE),
            self.height.unwrap_or(fallback.1).clamp(1, MAX_WINDOW_SIZE),
        )
    }
}
/// Largest accepted `[window] width/height`; bigger values mean hundreds of
/// MB of frame buffer per frame (and used to reach `Pixmap::new(..).expect`).
pub const MAX_WINDOW_SIZE: u32 = 16384;
/// Largest accepted `[window] time_hz` tick rate.
pub const MAX_TIME_HZ: u32 = 1000;

fn default_window_width() -> Option<u32> { None }
fn default_window_height() -> Option<u32> { None }
fn default_layer() -> String { "overlay".into() }
fn default_anchor() -> String { "center".into() }

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AnimationsConfig {
    #[serde(default)] pub default_duration_ms: Option<u32>,
    #[serde(default)] pub default_easing: Option<String>,
}

// ---------------------------------------------------------------------------
// Nodes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct SpringDef { pub damping: f32, pub stiffness: f32, pub mass: f32 }

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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

/// Node kinds the runtime understands (`TextInput`, `Icon` and `Selector` are
/// aliases resolved by layout). Anything else silently rendered as a plain
/// rectangle, which is why validate() warns.
const ALLOWED_NODE_TYPES: &[&str] = &[
    "Container",
    "Rectangle",
    "Polygon",
    "Text",
    "TextInput",
    "Repeater",
    "Image",
    "Icon",
    "Selector",
];

/// Props read by layout/render/text; anything else is a no-op (likely a typo).
const KNOWN_PROPS: &[&str] = &[
    "align",
    "background",
    "border",
    "clip_pad",
    "clip_y",
    "color",
    "count",
    "fill",
    "font_family",
    "font_size",
    "font_weight",
    "gap",
    "item_height",
    "layout",
    "outline",
    "radius",
    "ransom",
    "ransom_fonts",
    "ransom_paper",
    "shadow",
    "src",
    "text_case",
    "text_shadow",
    "window",
    "wrap",
];

/// Walk a node tree depth-first (top-level nodes, component root nodes and
/// their inline children).
fn walk_nodes<'a>(node: &'a NodeDef, f: &mut impl FnMut(&'a NodeDef)) {
    f(node);
    for child in &node.children {
        walk_nodes(child, f);
    }
}

/// Check ids are unique within one node scope (a top-level tree or a single
/// component's node list). The same id in two different components is fine:
/// repeater instances carry an `-<index>` suffix at resolve time.
fn walk_scope_ids(
    nodes: &[NodeDef],
    ids: &mut std::collections::HashSet<String>,
) -> Result<()> {
    let mut duplicate: Option<String> = None;
    for n in nodes {
        walk_nodes(n, &mut |node| {
            if duplicate.is_none() && !ids.insert(node.id.clone()) {
                duplicate = Some(node.id.clone());
            }
        });
    }
    if let Some(id) = duplicate {
        anyhow::bail!("Duplicate node id: {id}");
    }
    Ok(())
}

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

    /// Validate the theme: fail on data that would panic/hang the renderer or
    /// silently do nothing, and warn about likely typos.
    pub fn validate(&self) -> Result<()> {
        self.validate_window()?;
        self.validate_components()?;
        self.validate_font_sizes()?;

        // Unknown node types and duplicate ids are hard errors: the node
        // would silently render as a plain rectangle / collide in diagnostics.
        // Unknown prop keys are warned about (forward-compatible with themes
        // written for a newer client) but allowed.
        self.validate_node_types()?;
        let mut ids = std::collections::HashSet::new();
        walk_scope_ids(&self.nodes, &mut ids)?;
        for (name, comp) in &self.components {
            let mut comp_ids = std::collections::HashSet::new();
            walk_scope_ids(&comp.nodes, &mut comp_ids)
                .map_err(|e| anyhow::anyhow!("component '{name}': {e}"))?;
            for n in &comp.nodes {
                walk_nodes(n, &mut |node| {
                    if node.kind == "Repeater" {
                        if let Some(d) = &node.delegate {
                            if !self.components.contains_key(d) {
                                log::warn!(
                                    "component '{name}' repeater '{}' references unknown component '{d}'",
                                    node.id
                                );
                            }
                        }
                    }
                });
            }
        }
        // Repeater delegates on top-level nodes must exist (hard error: the
        // repeater would render nothing at all).
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

    fn validate_node_types(&self) -> Result<()> {
        let mut bad: Option<String> = None;
        let mut check = |node: &NodeDef| {
            if bad.is_none() && !ALLOWED_NODE_TYPES.contains(&node.kind.as_str()) {
                bad = Some(format!(
                    "Node '{}': unknown type '{}' (known: {})",
                    node.id,
                    node.kind,
                    ALLOWED_NODE_TYPES.join(", ")
                ));
            }
            for key in node.props.keys() {
                if !KNOWN_PROPS.contains(&key.as_str()) {
                    log::warn!("node '{}': unknown prop '{}' (typo?)", node.id, key);
                }
            }
        };
        for n in &self.nodes {
            walk_nodes(n, &mut check);
        }
        for comp in self.components.values() {
            for n in &comp.nodes {
                walk_nodes(n, &mut check);
            }
        }
        if let Some(err) = bad {
            anyhow::bail!("{err}");
        }
        Ok(())
    }

    /// Reject absurd literal font sizes early: `font_size="0"` panics
    /// cosmic-text and negative sizes wedge shaping. Runtime entry points
    /// clamp dynamic values, but a bad literal should surface in
    /// --validate-themes instead of silently rendering at another size.
    fn validate_font_sizes(&self) -> Result<()> {
        let mut font_err: Option<String> = None;
        let mut check = |node: &NodeDef| {
            if font_err.is_some() {
                return;
            }
            if let Some(raw) = node.props.get("font_size") {
                if let Ok(size) = raw.trim().parse::<f32>() {
                    if !size.is_finite() || !(0.5..=4096.0).contains(&size) {
                        font_err = Some(format!(
                            "Node '{}': font_size {} is out of range (0.5..=4096)",
                            node.id, size
                        ));
                    }
                }
            }
        };
        for n in &self.nodes {
            walk_nodes(n, &mut check);
        }
        for comp in self.components.values() {
            for n in &comp.nodes {
                walk_nodes(n, &mut check);
            }
        }
        if let Some(err) = font_err {
            anyhow::bail!("{err}");
        }
        Ok(())
    }

    fn validate_window(&self) -> Result<()> {
        let w = &self.window;
        for (key, value) in [("width", w.width), ("height", w.height)] {
            if let Some(size) = value {
                if !(1..=MAX_WINDOW_SIZE).contains(&size) {
                    anyhow::bail!(
                        "[window] {key} = {size} is out of range (1..={MAX_WINDOW_SIZE})"
                    );
                }
            }
        }
        if let Some(hz) = w.time_hz {
            if !(1..=MAX_TIME_HZ).contains(&hz) {
                anyhow::bail!("[window] time_hz = {hz} is out of range (1..={MAX_TIME_HZ})");
            }
        }
        if w.layer != "overlay" && w.layer != "top" && w.layer != "bottom" && w.layer != "background" {
            log::warn!("[window] layer '{}' is not one of overlay|top|bottom|background", w.layer);
        }
        if w.anchor != "center" && w.anchor != "top" && w.anchor != "bottom" {
            log::warn!("[window] anchor '{}' is not one of center|top|bottom", w.anchor);
        }
        Ok(())
    }

    fn validate_components(&self) -> Result<()> {
        // Springs: mass must be positive, damping/stiffness non-negative and
        // finite; a zero/negative mass divides by zero in the integrator.
        let mut spring_err: Option<String> = None;
        let mut check_springs = |scope: &str, anims: &[AnimateDef]| {
            if spring_err.is_some() {
                return;
            }
            for a in anims {
                if let Some(sd) = &a.spring {
                    if !sd.mass.is_finite() || sd.mass <= 0.0 {
                        spring_err = Some(format!("{scope}: spring mass {} must be > 0", sd.mass));
                    } else if !sd.damping.is_finite() || sd.damping < 0.0 {
                        spring_err = Some(format!("{scope}: spring damping {} must be >= 0", sd.damping));
                    } else if !sd.stiffness.is_finite() || sd.stiffness < 0.0 {
                        spring_err = Some(format!("{scope}: spring stiffness {} must be >= 0", sd.stiffness));
                    }
                }
            }
        };
        for n in &self.nodes {
            walk_nodes(n, &mut |node| check_springs(&format!("node '{}'", node.id), &node.animate));
        }
        for (name, comp) in &self.components {
            check_springs(&format!("component '{name}'"), &comp.animate);
            for n in &comp.nodes {
                walk_nodes(n, &mut |node| {
                    check_springs(&format!("component '{name}' node '{}'", node.id), &node.animate)
                });
            }
        }
        if let Some(err) = spring_err {
            anyhow::bail!("{err}");
        }
        // Component cycle detection: A -> B -> A would expand forever (a
        // self-referencing repeater delegate is the minimal case).
        let mut edges: std::collections::HashMap<&str, std::collections::HashSet<&str>> =
            std::collections::HashMap::new();
        for (name, comp) in &self.components {
            let mut out: std::collections::HashSet<&str> = std::collections::HashSet::new();
            for n in &comp.nodes {
                walk_nodes(n, &mut |node| {
                    if node.kind == "Repeater" {
                        if let Some(d) = node.delegate.as_deref() {
                            if self.components.contains_key(d) {
                                out.insert(d);
                            }
                        }
                    }
                });
            }
            edges.insert(name.as_str(), out);
        }
        let mut visiting: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut done: std::collections::HashSet<&str> = std::collections::HashSet::new();
        fn visit<'a>(
            name: &'a str,
            edges: &std::collections::HashMap<&'a str, std::collections::HashSet<&'a str>>,
            visiting: &mut std::collections::HashSet<&'a str>,
            done: &mut std::collections::HashSet<&'a str>,
            path: &mut Vec<&'a str>,
        ) -> Result<()> {
            if done.contains(name) {
                return Ok(());
            }
            if !visiting.insert(name) {
                anyhow::bail!(
                    "self-referential components: {} -> {name}",
                    path.join(" -> ")
                );
            }
            path.push(name);
            if let Some(next) = edges.get(name) {
                for n in next {
                    visit(n, edges, visiting, done, path)?;
                }
            }
            path.pop();
            visiting.remove(name);
            done.insert(name);
            Ok(())
        }
        let component_names: Vec<&str> = self.components.keys().map(|s| s.as_str()).collect();
        for name in component_names {
            let mut path = Vec::new();
            visit(name, &edges, &mut visiting, &mut done, &mut path)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn theme_with(raw: &str) -> Theme {
        toml::from_str(raw).expect("theme parses")
    }

    #[test]
    fn bundled_themes_validate() {
        let dirs = ["default", "atlus", "circular", "layered", "macos"];
        for name in dirs {
            let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("themes")
                .join(name)
                .join("theme.toml");
            let theme = Theme::load_from_file(&path)
                .unwrap_or_else(|e| panic!("bundled theme {name} must load: {e:#}"));
            assert_eq!(theme.meta.name.is_empty(), false, "{name}: meta.name");
        }
    }

    #[test]
    fn zero_font_size_literal_is_rejected() {
        let t = theme_with(r#"
            [[nodes]]
            id = "t"
            type = "Text"
            props = { font_size = "0" }
        "#);
        let err = t.validate().expect_err("font_size=0 must be rejected");
        assert!(err.to_string().contains("font_size"), "{err}");
    }

    #[test]
    fn negative_and_huge_font_size_literals_are_rejected() {
        for bad in ["-8", "99999"] {
            let t = theme_with(&format!(r#"
                [[nodes]]
                id = "t"
                type = "Text"
                props = {{ font_size = "{bad}" }}
            "#));
            assert!(t.validate().is_err(), "font_size {bad} must be rejected");
        }
    }

    #[test]
    fn binding_font_size_is_left_to_runtime_clamp() {
        let t = theme_with(r#"
            [[nodes]]
            id = "t"
            type = "Text"
            props = { font_size = "launcher.loading ? 0 : 15" }
        "#);
        assert!(t.validate().is_ok(), "dynamic sizes are clamped at runtime, not statically rejected");
    }

    #[test]
    fn unknown_fields_are_rejected() {
        // Top-level typo ([[node]] instead of [[nodes]]) used to parse as an
        // empty theme; deny_unknown_fields makes it a load error.
        assert!(toml::from_str::<Theme>(r#"
            [[node]]
            id = "x"
            type = "Rectangle"
        "#).is_err());
        // Node field typo.
        assert!(toml::from_str::<Theme>(r#"
            [[nodes]]
            id = "x"
            type = "Rectangle"
            heigth = "40"
        "#).is_err());
        // Window field typo.
        assert!(toml::from_str::<Theme>(r#"
            [window]
            widht = 640
        "#).is_err());
        // Animate/spring typos.
        assert!(toml::from_str::<Theme>(r#"
            [[nodes]]
            id = "x"
            type = "Rectangle"
            animate = [{ property = "x", spring = { stiffness = 1, damping = 1, mas = 1 } }]
        "#).is_err());
    }

    #[test]
    fn unknown_prop_is_warned_but_allowed() {
        // props is an open map: typos warn (log) but do not break a theme that
        // may be written for a newer client.
        let t = theme_with(r#"
            [[nodes]]
            id = "x"
            type = "Text"
            props = { font_sise = "28" }
        "#);
        assert!(t.validate().is_ok());
    }

    #[test]
    fn unknown_node_type_is_rejected() {
        let t = theme_with(r#"
            [[nodes]]
            id = "x"
            type = "Rectanlge"
        "#);
        let err = t.validate().expect_err("unknown type must fail");
        assert!(err.to_string().contains("unknown type"), "{err}");
        // ... also inside a component.
        let t = theme_with(r#"
            [components.c]
            nodes = [{ id = "x", type = "Nope" }]
        "#);
        assert!(t.validate().is_err());
    }

    #[test]
    fn window_bounds_and_time_hz_are_validated() {
        for (raw, what) in [
            ("[window]\nwidth = 0", "width 0"),
            ("[window]\nheight = 20000", "height 20000"),
            ("[window]\ntime_hz = 0", "time_hz 0"),
            ("[window]\ntime_hz = 100000", "time_hz 100000"),
        ] {
            let t = theme_with(raw);
            assert!(t.validate().is_err(), "{what} must be rejected");
        }
        // effective_size clamps even for structs that bypass validate().
        let mut t = Theme::fallback();
        t.window.width = Some(500_000);
        t.window.height = Some(0);
        assert_eq!(t.window.effective_size((640, 420)), (MAX_WINDOW_SIZE, 1));
    }

    #[test]
    fn spring_values_are_validated() {
        for spring in [
            "{ stiffness = 100, damping = 10, mass = 0 }",
            "{ stiffness = 100, damping = -1, mass = 1 }",
            "{ stiffness = -5, damping = 10, mass = 1 }",
            "{ stiffness = 100, damping = 10, mass = 0.0 }",
        ] {
            let t = theme_with(&format!(r#"
                [[nodes]]
                id = "x"
                type = "Rectangle"
                animate = [{{ property = "x", spring = {spring} }}]
            "#));
            assert!(t.validate().is_err(), "spring {spring} must be rejected");
        }
        // Valid spring passes.
        let t = theme_with(r#"
            [[nodes]]
            id = "x"
            type = "Rectangle"
            animate = [{ property = "x", spring = { stiffness = 240, damping = 18, mass = 1 } }]
        "#);
        assert!(t.validate().is_ok());
    }

    #[test]
    fn duplicate_ids_are_rejected_in_nested_scopes() {
        let t = theme_with(r#"
            [[nodes]]
            id = "card"
            type = "Container"
            children = [
                { id = "label", type = "Text" },
                { id = "label", type = "Text" },
            ]
        "#);
        assert!(t.validate().is_err(), "duplicate nested child ids");
        let t = theme_with(r#"
            [components.a]
            nodes = [
                { id = "dup", type = "Text" },
                { id = "dup", type = "Text" },
            ]
        "#);
        assert!(t.validate().is_err(), "duplicate component node ids");
        // Same id in two different components is fine (instances get suffixes).
        let t = theme_with(r#"
            [components.a]
            nodes = [{ id = "label", type = "Text" }]
            [components.b]
            nodes = [{ id = "label", type = "Text" }]
        "#);
        assert!(t.validate().is_ok());
    }

    #[test]
    fn self_referential_and_cyclic_components_are_rejected() {
        let self_ref = theme_with(r#"
            [components.loop]
            nodes = [{ id = "r", type = "Repeater", model = "launcher.results", delegate = "loop" }]
        "#);
        let err = self_ref.validate().expect_err("self reference must fail");
        assert!(err.to_string().contains("self-referential"), "{err}");
        let cycle = theme_with(r#"
            [components.a]
            nodes = [{ id = "ra", type = "Repeater", delegate = "b" }]
            [components.b]
            nodes = [{ id = "rb", type = "Repeater", delegate = "a" }]
        "#);
        assert!(cycle.validate().is_err(), "A->B->A cycle must fail");
        let chain = theme_with(r#"
            [components.a]
            nodes = [{ id = "ra", type = "Repeater", delegate = "b" }]
            [components.b]
            nodes = [{ id = "rb", type = "Rectangle" }]
        "#);
        assert!(chain.validate().is_ok(), "a non-cyclic chain is fine");
    }

    #[test]
    fn bundled_themes_still_load_with_strict_schema() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("themes");
        for theme in ["default", "atlus", "circular", "layered", "macos"] {
            let path = root.join(theme).join("theme.toml");
            assert!(path.is_file(), "missing bundled theme {theme}");
            Theme::load_from_file(&path)
                .unwrap_or_else(|e| panic!("bundled theme {theme} must pass strict validation: {e:#}"));
        }
    }
}

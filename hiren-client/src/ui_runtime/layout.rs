//! Layout resolution — turns Theme + LauncherState into flat `ResolvedNode`s.
//!
//! Everything here is generic scene-graph mechanics; no launcher-specific
//! structure is assumed. The resolver knows how to:
//!   * evaluate bindings (x/y/w/h/visible/opacity/rotation/scale/props/text)
//!   * expand `Repeater` nodes over `launcher.results` or a static `count`
//!     (vertical / row / circular arrangement — positioning is data, not code)
//!   * nest `Container` children (free / column / row flow)
//!   * parse `on_click` actions with per-instance locals
//!   * bake per-instance animation delays (stagger) into nodes

use super::binding::{eval_bool, eval_f32, eval_str, EvalContext, SharedDiag, Measurer, Impulse};
use super::node::{NodeAction, ResolvedNode};
use super::theme::{Theme, NodeDef, AnimateDef};
use crate::launcher::LauncherState;
use std::collections::HashMap;

pub struct ResolveOutput {
    pub nodes: Vec<ResolvedNode>,
    /// True when any geometry binding references `time` — the caller should
    /// keep producing frames for continuous animation.
    pub uses_time: bool,
    /// True when the theme binds `hit` / `shake` / `since_select` (or type
    /// variants). Combined with a live impulse, the window loop keeps ticking.
    pub uses_impulse: bool,
}

pub fn resolve(
    theme: &Theme,
    state: &LauncherState,
    window_size: (u32, u32),
    time: f32,
    measure: Option<Measurer>,
    diag: Option<SharedDiag>,
) -> ResolveOutput {
    resolve_with(theme, state, window_size, time, Impulse::default(), measure, diag)
}

pub fn resolve_with(
    theme: &Theme,
    state: &LauncherState,
    window_size: (u32, u32),
    time: f32,
    impulse: Impulse,
    measure: Option<Measurer>,
    diag: Option<SharedDiag>,
) -> ResolveOutput {
    let uses_time = theme_uses_token(theme, &["time"]);
    let uses_impulse = theme_uses_token(
        theme,
        &["hit", "hit_type", "since_select", "since_type", "shake", "type_shake"],
    );
    let mut ctx = EvalContext::new(state, window_size, time);
    ctx.impulse = impulse;
    ctx.measure = measure;
    ctx.diag = diag;
    let mut out = Vec::new();
    for node in &theme.nodes {
        resolve_node(node, theme, &mut ctx, 0.0, 0.0, None, &mut out);
    }
    // Stable sort by z keeps document order within a layer.
    out.sort_by_key(|n| n.z);
    ResolveOutput { nodes: out, uses_time, uses_impulse }
}

/// Cheap scan: does any geometry/visual binding mention one of `tokens`?
pub(crate) fn theme_uses_token(theme: &Theme, tokens: &[&str]) -> bool {
    fn expr_uses(e: &str, tokens: &[&str]) -> bool {
        e.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .any(|t| tokens.contains(&t))
    }
    fn node_uses(n: &NodeDef, tokens: &[&str]) -> bool {
        let prop_hit = n.props.values().any(|v| expr_uses(v, tokens));
        let points_hit = n.points.as_deref().map(|e| expr_uses(e, tokens)).unwrap_or(false);
        let child_hit = n.children.iter().any(|c| node_uses(c, tokens));
        let anim_hit = n.animate.iter().any(|a| {
            matches!(&a.delay, Some(super::theme::DelaySpec::Expr(e)) if expr_uses(e, tokens))
                || matches!(&a.from, Some(super::theme::FromSpec::Expr(e)) if expr_uses(e, tokens))
        });
        [&n.x, &n.y, &n.width, &n.height, &n.visible, &n.opacity, &n.rotation, &n.skew, &n.scale]
            .iter()
            .any(|f| f.as_deref().map(|e| expr_uses(e, tokens)).unwrap_or(false))
            || prop_hit || points_hit || child_hit || anim_hit
    }
    theme.nodes.iter().any(|n| node_uses(n, tokens))
        || theme.components.values().flat_map(|c| &c.nodes).any(|n| node_uses(n, tokens))
        || theme.components.values().any(|c| {
            c.animate.iter().any(|a| {
                matches!(&a.delay, Some(super::theme::DelaySpec::Expr(e)) if expr_uses(e, tokens))
                    || matches!(&a.from, Some(super::theme::FromSpec::Expr(e)) if expr_uses(e, tokens))
            })
        })
}

/// Resolve one node (and its children / expansions) into `out`.
///
/// * `base_x/base_y` — offset added to evaluated coordinates (repeater/container origin).
/// * `id_suffix` — instance suffix for delegate expansion (`"-3"`).
fn resolve_node(
    def: &NodeDef,
    theme: &Theme,
    ctx: &mut EvalContext,
    base_x: f32,
    base_y: f32,
    id_suffix: Option<&str>,
    out: &mut Vec<ResolvedNode>,
) {
    ctx.node = def.id.clone();

    let visible = match &def.visible {
        Some(v) => eval_bool(v, ctx, true),
        None => true,
    };
    if !visible {
        return;
    }

    // ---- Repeater: expand over results or a static count --------------------
    if def.kind == "Repeater" {
        resolve_repeater(def, theme, ctx, base_x, base_y, out);
        return;
    }

    let x = base_x + eval_f32(def.x.as_deref().unwrap_or("0"), ctx, 0.0);
    let y = base_y + eval_f32(def.y.as_deref().unwrap_or("0"), ctx, 0.0);
    let w = eval_f32(def.width.as_deref().unwrap_or("100"), ctx, 100.0);
    let h = eval_f32(def.height.as_deref().unwrap_or("40"), ctx, 40.0);
    let opacity = eval_f32(def.opacity.as_deref().unwrap_or("1"), ctx, 1.0).clamp(0.0, 1.0);
    let rotation = def.rotation.as_deref().map(|e| eval_f32(e, ctx, 0.0)).unwrap_or(0.0);
    let skew = def.skew.as_deref().map(|e| eval_f32(e, ctx, 0.0)).unwrap_or(0.0);
    let scale = def.scale.as_deref().map(|e| eval_f32(e, ctx, 1.0)).unwrap_or(1.0);
    let points = def.points.as_deref().map(|e| parse_points(e, ctx)).unwrap_or_default();

    let mut props = HashMap::new();
    for (k, v) in &def.props {
        props.insert(k.clone(), eval_str(v, ctx));
    }
    if let Some(tc) = &def.text_case {
        props.entry("text_case".to_string()).or_insert_with(|| tc.clone());
    }

    let background = props
        .get("background")
        .or_else(|| props.get("fill"))
        .and_then(|c| super::render::solid_color(c));
    let color = props.get("color").and_then(|c| super::render::solid_color(c));
    let radius = props
        .get("radius")
        .and_then(|s| s.trim().parse::<f32>().ok())
        .unwrap_or(0.0);

    let text = def.text.as_ref().map(|e| apply_text_case(&eval_str(e, ctx), &props));
    let action = def.on_click.as_deref().and_then(|a| parse_action(a, ctx));

    let node = ResolvedNode {
        id: with_suffix(&def.id, id_suffix),
        kind: def.kind.clone(),
        x, y, width: w, height: h,
        opacity,
        z: def.z.unwrap_or(0),
        visible: true,
        props,
        text,
        color,
        background,
        radius,
        rotation,
        skew,
        scale,
        points,
        animate: bake_anims(&def.animate, ctx),
        action,
        index: None,
        clip: None,
    };
    out.push(node);

    // ---- Container children -------------------------------------------------
    if !def.children.is_empty() {
        let layout = def.props.get("layout").map(|s| s.as_str()).unwrap_or("free");
        let gap = def.props.get("gap").map(|s| eval_f32(s, ctx, 8.0)).unwrap_or(8.0);
        let mut cursor_x = 0.0;
        let mut cursor_y = 0.0;
        for child in &def.children {
            let (cx, cy) = match layout {
                "column" => (0.0 + cursor_x, cursor_y),
                "row" => (cursor_x, 0.0 + cursor_y),
                _ => (0.0, 0.0),
            };
            resolve_node(child, theme, ctx, x + cx, y + cy, id_suffix, out);
            let chw = eval_f32(child.width.as_deref().unwrap_or("100"), ctx, 100.0);
            let chh = eval_f32(child.height.as_deref().unwrap_or("40"), ctx, 40.0);
            match layout {
                "column" => cursor_y += chh + gap,
                "row" => cursor_x += chw + gap,
                _ => {}
            }
        }
    }
}

/// `text_case = "upper" | "lower"` — case-fold the resolved text at layout
/// time (P5 sets its menus in caps; condensed grotesques read in uppercase).
fn apply_text_case(text: &str, props: &HashMap<String, String>) -> String {
    match props.get("text_case").map(|s| s.as_str()) {
        Some("upper") => text.to_uppercase(),
        Some("lower") => text.to_lowercase(),
        _ => text.to_string(),
    }
}

fn with_suffix(id: &str, suffix: Option<&str>) -> String {
    match suffix {
        Some(s) => format!("{id}{s}"),
        None => id.to_string(),
    }
}

/// Parse a polygon `points` string: `;`-separated `x,y` pairs, each coordinate
/// a binding expression (in node-local coordinates).
/// Example: `points = "0,0; window.width,8; window.width,52; 0,60"`.
fn parse_points(raw: &str, ctx: &mut EvalContext) -> Vec<(f32, f32)> {
    raw.split(';')
        .filter_map(|pair| {
            let pair = pair.trim();
            if pair.is_empty() {
                return None;
            }
            let (xs, ys) = split_xy(pair)?;
            let x = eval_f32(xs.trim(), ctx, f32::NAN);
            let y = eval_f32(ys.trim(), ctx, f32::NAN);
            if x.is_finite() && y.is_finite() {
                Some((x, y))
            } else {
                None
            }
        })
        .collect()
}

/// Split `x,y` on the first comma that is not inside quotes or parentheses,
/// so `text_width(a, 32, 'Anton, Oswald') + 8, 12` is one pair.
fn split_xy(pair: &str) -> Option<(&str, &str)> {
    let bytes = pair.as_bytes();
    let mut depth = 0i32;
    let mut quote: Option<u8> = None;
    for (i, &c) in bytes.iter().enumerate() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            b'\'' | b'"' => quote = Some(c),
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => return Some((pair[..i].trim(), pair[i + 1..].trim())),
            _ => {}
        }
    }
    None
}

/// Bake per-instance animation values: resolve `delay` expressions into
/// `delay_ms` and `from` expressions into `from_value`.
fn bake_anims(anims: &[AnimateDef], ctx: &mut EvalContext) -> Vec<AnimateDef> {
    anims
        .iter()
        .map(|a| {
            let mut a = a.clone();
            match a.delay.take() {
                Some(super::theme::DelaySpec::Fixed(v)) => a.delay_ms += v,
                Some(super::theme::DelaySpec::Expr(e)) => a.delay_ms += eval_f32(&e, ctx, 0.0).max(0.0) as u32,
                None => {}
            }
            if let Some(f) = &a.from {
                a.from_value = Some(match f {
                    super::theme::FromSpec::Fixed(v) => *v,
                    super::theme::FromSpec::Expr(e) => eval_f32(e, ctx, 0.0),
                });
            }
            a
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Repeaters
// ---------------------------------------------------------------------------

fn resolve_repeater(
    def: &NodeDef,
    theme: &Theme,
    ctx: &mut EvalContext,
    base_x: f32,
    base_y: f32,
    out: &mut Vec<ResolvedNode>,
) {
    let model = def.model.as_deref().unwrap_or("launcher.results");
    let delegate_id = def.delegate.as_deref().unwrap_or("");
    let Some(comp) = theme.components.get(delegate_id) else {
        if let Some(d) = &ctx.diag {
            d.borrow_mut().warn(&def.id, "repeater", &format!("unknown delegate component `{delegate_id}`"));
        }
        return;
    };

    // Items: launcher results, or a static count for decorative repeats
    // (`count` prop takes precedence).
    let static_count = def.props.get("count").map(|c| eval_f32(c, ctx, 0.0).max(0.0) as usize);
    let use_results = static_count.is_none() && model == "launcher.results";
    let count = match static_count {
        Some(n) => n,
        None if use_results => ctx.launcher.results.len(),
        None => 0,
    };
    if count == 0 {
        return;
    }

    // Repeater origin (its own x/y are evaluated like any node).
    let origin_x = base_x + eval_f32(def.x.as_deref().unwrap_or("0"), ctx, 0.0);
    let origin_y = base_y + eval_f32(def.y.as_deref().unwrap_or("0"), ctx, 0.0);

    let layout = def.props.get("layout").map(|s| s.as_str()).unwrap_or("vertical");
    let gap = def.props.get("gap").map(|e| eval_f32(e, ctx, 8.0)).unwrap_or(8.0);
    let item_h = def.props.get("item_height").map(|e| eval_f32(e, ctx, 44.0)).unwrap_or(44.0);
    let radius = def.props.get("radius").map(|e| eval_f32(e, ctx, 120.0)).unwrap_or(120.0);
    // Optional sliding window around the selection (ring/row layouts with many
    // items): only instances within ±window/2 of `selected_index` are built.
    // Results-driven ring/row repeaters default to a 9-item window — a ring of
    // 200 results is unusable; decorative (count-prop) repeats are untouched.
    let sel_window = def
        .props
        .get("window")
        .map(|e| eval_f32(e, ctx, 0.0))
        .unwrap_or(if use_results && layout != "vertical" && layout != "free" { 9.0 } else { 0.0 }) as usize;
    // Visible list height (viewport for virtualization). Defaults to "rest of
    // the window" so themes only opt in when they care about the exact clip.
    let view_h = def
        .height
        .as_deref()
        .map(|e| eval_f32(e, ctx, 0.0))
        .unwrap_or((ctx.window_size.1 as f32 - origin_y).max(0.0));
    let view_w = def
        .width
        .as_deref()
        .map(|e| eval_f32(e, ctx, 0.0))
        .unwrap_or((ctx.window_size.0 as f32 - origin_x).max(0.0));
    // The scissor band is FIXED in window space (that's the whole point —
    // content slides under it), so scrolling themes pin it via `clip_y`
    // instead of inheriting the repeater's animated y. `clip_pad` widens the
    // band horizontally so delegate chrome (tag chips, plate stacks) may
    // stick out of the repeater box without being scissored away.
    let clip_y = def.props.get("clip_y").map(|e| eval_f32(e, ctx, origin_y)).unwrap_or(origin_y);
    let clip_pad = def.props.get("clip_pad").map(|e| eval_f32(e, ctx, 0.0)).unwrap_or(0.0);

    // Virtualization: instances entirely outside the viewport band are never
    // built (see the pre-cull below). A pure runtime optimization — item
    // positions stay exactly as the bindings compute them.
    let in_selection_window = |idx: usize| -> bool {
        sel_window == 0
            || {
                let half = (sel_window / 2) as isize;
                let sel = ctx.launcher.selected_index as isize;
                (idx as isize - sel).abs() <= half.max(1)
            }
    };

    for idx in 0..count {
        // Cheap pre-cull before any binding evaluation for this instance: an
        // item entirely outside the repeater's viewport is never built. The
        // origin already includes scroll offsets, so spring overshoot is
        // handled naturally.
        match layout {
            "vertical" => {
                let top = origin_y + idx as f32 * (item_h + gap);
                // Cull against the same fixed window-space band the renderer
                // scissors to — NOT the (scrolled) repeater origin.
                if top >= clip_y + view_h + 0.5 || top + item_h <= clip_y - 0.5 {
                    continue;
                }
            }
            "circular" | "row" | "free" => {
                if !in_selection_window(idx) {
                    continue;
                }
            }
            _ => {}
        }
        let item = if use_results { Some(&ctx.launcher.results[idx]) } else { None };
        let is_selected = use_results && idx == ctx.launcher.selected_index;

        let mut locals = HashMap::new();
        locals.insert("index".into(), idx.to_string());
        locals.insert("count".into(), count.to_string());
        locals.insert("is_selected".into(), is_selected.to_string());
        locals.insert("selected_index".into(), ctx.launcher.selected_index.to_string());
        if let Some(it) = item {
            locals.insert("item_name".into(), it.name.clone());
            locals.insert("item_exec".into(), it.exec.clone());
            locals.insert("item_id".into(), it.id.clone());
            locals.insert("item_description".into(), it.description.clone().unwrap_or_default());
            locals.insert("item_keywords".into(), it.keywords.clone());
            locals.insert("item_mode".into(), format!("{:?}", it.mode).to_lowercase());
            locals.insert("item_icon".into(), super::icon::resolve(&it.icon));
        }
        let mut item_ctx = ctx.clone();
        item_ctx.locals = locals;
        item_ctx.node = format!("{}[{}]", def.id, idx);

        // Instance offset per arrangement.
        let (off_x, off_y) = match layout {
            "circular" => {
                let angle = (idx as f32 / count as f32) * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
                (angle.cos() * radius, angle.sin() * radius)
            }
            "row" | "free" => (0.0, 0.0), // delegates place themselves
            _ => (0.0, idx as f32 * (item_h + gap)), // vertical
        };

        let suffix = format!("-{idx}");
        // Delegate instances are scissored to the repeater's box so a scrolled
        // list slides under the rest of the UI instead of over it. Rings place
        // items around the center — no clip there.
        let clip = match layout {
            "vertical" => Some((origin_x - clip_pad, clip_y - 0.5, view_w + clip_pad * 2.0, view_h + 1.0)),
            "row" => Some((origin_x, clip_y - 0.5, view_w, item_h + 1.0)),
            _ => None, // circular + free: no scissor
        };
        for delegate_node in &comp.nodes {
            resolve_delegate_node_with_component(delegate_node, &mut item_ctx, origin_x + off_x, origin_y + off_y, &suffix, idx, &comp.animate, clip, out);
        }
    }
}

fn resolve_delegate_node_with_component(
    def: &NodeDef,
    ctx: &mut EvalContext,
    base_x: f32,
    base_y: f32,
    id_suffix: &str,
    index: usize,
    comp_anims: &[AnimateDef],
    clip: Option<(f32, f32, f32, f32)>,
    out: &mut Vec<ResolvedNode>,
) {
    ctx.node = format!("{}{}", def.id, id_suffix);

    let visible = match &def.visible {
        Some(v) => eval_bool(v, ctx, true),
        None => true,
    };
    if !visible {
        return;
    }

    let x = base_x + eval_f32(def.x.as_deref().unwrap_or("0"), ctx, 0.0);
    let y = base_y + eval_f32(def.y.as_deref().unwrap_or("0"), ctx, 0.0);
    let w = eval_f32(def.width.as_deref().unwrap_or("100"), ctx, 100.0);
    let h = eval_f32(def.height.as_deref().unwrap_or("40"), ctx, 40.0);
    let opacity = eval_f32(def.opacity.as_deref().unwrap_or("1"), ctx, 1.0).clamp(0.0, 1.0);
    let rotation = def.rotation.as_deref().map(|e| eval_f32(e, ctx, 0.0)).unwrap_or(0.0);
    let skew = def.skew.as_deref().map(|e| eval_f32(e, ctx, 0.0)).unwrap_or(0.0);
    let scale = def.scale.as_deref().map(|e| eval_f32(e, ctx, 1.0)).unwrap_or(1.0);
    let points = def.points.as_deref().map(|e| parse_points(e, ctx)).unwrap_or_default();

    let mut props = HashMap::new();
    for (k, v) in &def.props {
        props.insert(k.clone(), eval_str(v, ctx));
    }
    if let Some(tc) = &def.text_case {
        props.entry("text_case".to_string()).or_insert_with(|| tc.clone());
    }
    let background = props
        .get("background")
        .or_else(|| props.get("fill"))
        .and_then(|c| super::render::solid_color(c));
    let color = props.get("color").and_then(|c| super::render::solid_color(c));
    let radius = props.get("radius").and_then(|s| s.trim().parse::<f32>().ok()).unwrap_or(0.0);
    let text = def.text.as_ref().map(|e| apply_text_case(&eval_str(e, ctx), &props));
    let action = def.on_click.as_deref().and_then(|a| parse_action(a, ctx));

    let mut merged: Vec<AnimateDef> = def.animate.iter().chain(comp_anims.iter()).cloned().collect();
    let baked = bake_anims(&merged, ctx);
    merged.clear();

    out.push(ResolvedNode {
        id: format!("{}{}", def.id, id_suffix),
        kind: def.kind.clone(),
        x, y, width: w, height: h,
        opacity,
        z: def.z.unwrap_or(0),
        visible: true,
        props,
        text,
        color,
        background,
        radius,
        rotation,
        skew,
        scale,
        points,
        animate: baked,
        action,
        index: Some(index),
        clip,
    });

    // Delegate children (nested), same suffix, offsets relative to parent.
    if !def.children.is_empty() {
        for child in &def.children {
            resolve_delegate_node_with_component(child, ctx, x, y, id_suffix, index, comp_anims, clip, out);
        }
    }
}

/// Alias kept for direct delegate resolution without component animations.
#[allow(dead_code)]
fn resolve_delegate_node(def: &NodeDef, ctx: &mut EvalContext, base_x: f32, base_y: f32, id_suffix: &str, index: usize, out: &mut Vec<ResolvedNode>) {
    resolve_delegate_node_with_component(def, ctx, base_x, base_y, id_suffix, index, &[], None, out)
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

/// Parse an `on_click` action: `name` or `name(arg)` with arg evaluated in
/// the current scope (so `"activate(index)"` resolves per repeater instance).
fn parse_action(raw: &str, ctx: &mut EvalContext) -> Option<NodeAction> {
    let raw = raw.trim();
    let (name, arg) = match raw.find('(') {
        Some(i) if raw.ends_with(')') => (raw[..i].trim(), Some(raw[i + 1..raw.len() - 1].trim())),
        _ => (raw, None),
    };
    // Evaluate args once (arg may reference repeater locals like `index`).
    let idx = arg
        .map(|e| eval_f32(e, ctx, -1.0))
        .filter(|v| *v >= 0.0)
        .map(|v| v as usize);
    let delta = arg.map(|e| eval_f32(e, ctx, 0.0)).unwrap_or(0.0);
    let text = arg.map(|e| eval_str(e, ctx)).unwrap_or_default();
    match name {
        "activate" | "launch" => Some(NodeAction::Activate(idx)),
        "select" => Some(NodeAction::Select(idx)),
        "move_selection" | "move" => Some(NodeAction::Move(delta as i32)),
        "set_query" | "query" => Some(NodeAction::SetQuery(text)),
        "close" | "exit" | "quit" => Some(NodeAction::Close),
        other => {
            if let Some(d) = &ctx.diag {
                d.borrow_mut().warn(&ctx.node.clone(), "on_click", &format!("unknown action `{other}`"));
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_runtime::theme::Theme;
    use hiren_shared::AppEntry;

    fn state(n: usize, sel: usize) -> LauncherState {
        let mut s = LauncherState::new();
        s.selected_index = sel;
        let v: Vec<AppEntry> = (0..n)
            .map(|i| AppEntry::run(format!("id{i}"), format!("App{i}"), format!("app{i}")))
            .collect();
        s.set_results(v);
        s
    }

    fn theme_from(toml: &str) -> Theme {
        let t: Theme = toml::from_str(toml).unwrap();
        t.validate().unwrap();
        t
    }

    #[test]
    fn free_positioning_and_bindings() {
        let t = theme_from(
            r#"
            [[nodes]]
            id = "a"
            type = "Rectangle"
            x = "window.width / 2 - 50"
            y = "launcher.selected_index * 40 + 10"
            width = "100"
            height = "40"
            props = { background = "rgba(1,2,3,0.5)" }
            "#,
        );
        let out = resolve(&t, &state(3, 2), (400, 300), 0.0, None, None);
        assert_eq!(out.nodes.len(), 1);
        let n = &out.nodes[0];
        assert_eq!((n.x, n.y), (150.0, 90.0));
        assert_eq!(n.background, Some((1, 2, 3, 128)));
    }

    #[test]
    fn repeater_vertical_with_locals_and_actions() {
        let t = theme_from(
            r#"
            [[nodes]]
            id = "list"
            type = "Repeater"
            x = "10"
            y = "20"
            model = "launcher.results"
            delegate = "row"
            props = { item_height = "40", gap = "6" }

            [components.row]
            [[components.row.nodes]]
            id = "rowbg"
            type = "Rectangle"
            x = "0"
            y = "0"
            width = "200"
            height = "40"
            props = { background = "is_selected ? #ff0000 : transparent" }
            on_click = "activate(index)"

            [[components.row.nodes]]
            id = "rowtxt"
            type = "Text"
            x = "0"
            y = "0"
            width = "200"
            height = "40"
            text = "item_name"
            "#,
        );
        let out = resolve(&t, &state(3, 1), (400, 300), 0.0, None, None);
        assert_eq!(out.nodes.len(), 6);
        let row0 = out.nodes.iter().find(|n| n.id == "rowbg-0").unwrap();
        let row1 = out.nodes.iter().find(|n| n.id == "rowbg-1").unwrap();
        assert_eq!((row0.x, row0.y), (10.0, 20.0));
        assert_eq!(row1.y, 66.0);
        assert_eq!(row0.background, Some((0, 0, 0, 0)));
        assert_eq!(row1.background, Some((255, 0, 0, 255)));
        assert_eq!(row1.action, Some(NodeAction::Activate(Some(1))));
        let txt = out.nodes.iter().find(|n| n.id == "rowtxt-2").unwrap();
        assert_eq!(txt.text.as_deref(), Some("App2"));
    }

    #[test]
    fn repeater_circular_arrangement() {
        let t = theme_from(
            r#"
            [[nodes]]
            id = "orbit"
            type = "Repeater"
            x = "200"
            y = "200"
            model = "launcher.results"
            delegate = "it"
            props = { layout = "circular", radius = "100", item_height = "40", gap = "0" }

            [components.it]
            [[components.it.nodes]]
            id = "dot"
            type = "Rectangle"
            x = "-20"
            y = "-20"
            width = "40"
            height = "40"
            "#,
        );
        let out = resolve(&t, &state(4, 0), (400, 400), 0.0, None, None);
        let d0 = out.nodes.iter().find(|n| n.id == "dot-0").unwrap();
        // first item is at -90° (top): center + (0, -100) + delegate offset (-20,-20)
        assert!((d0.x - (200.0 - 20.0)).abs() < 0.5, "d0.x={}", d0.x);
        assert!((d0.y - (200.0 - 100.0 - 20.0)).abs() < 0.5, "d0.y={}", d0.y);
    }

    #[test]
    fn static_count_repeater_for_decorations() {
        let t = theme_from(
            r#"
            [[nodes]]
            id = "ticks"
            type = "Repeater"
            x = "0"
            y = "0"
            delegate = "tick"
            props = { count = "5" }

            [components.tick]
            [[components.tick.nodes]]
            id = "tickline"
            type = "Rectangle"
            x = "index * 20"
            y = "0"
            width = "10"
            height = "2"
            "#,
        );
        let out = resolve(&t, &state(0, 0), (400, 300), 0.0, None, None);
        assert_eq!(out.nodes.len(), 5);
        assert_eq!(out.nodes[3].x, 60.0);
    }

    #[test]
    fn container_column_flow() {
        let t = theme_from(
            r#"
            [[nodes]]
            id = "panel"
            type = "Container"
            x = "10"
            y = "10"
            width = "200"
            height = "200"
            props = { layout = "column", gap = "4" }

            [[nodes.children]]
            id = "c1"
            type = "Rectangle"
            x = "0"
            y = "0"
            width = "100"
            height = "30"

            [[nodes.children]]
            id = "c2"
            type = "Rectangle"
            x = "5"
            y = "0"
            width = "100"
            height = "30"
            "#,
        );
        let out = resolve(&t, &state(0, 0), (400, 300), 0.0, None, None);
        let c1 = out.nodes.iter().find(|n| n.id == "c1").unwrap();
        let c2 = out.nodes.iter().find(|n| n.id == "c2").unwrap();
        assert_eq!((c1.x, c1.y), (10.0, 10.0));
        assert_eq!((c2.x, c2.y), (15.0, 44.0)); // 10 + 30 + 4
    }

    #[test]
    fn rotation_scale_and_stagger() {
        let t = theme_from(
            r#"
            [[nodes]]
            id = "card"
            type = "Rectangle"
            x = "10"
            y = "10"
            width = "100"
            height = "50"
            rotation = "-6"
            scale = "1.05"

            [[nodes]]
            id = "rows"
            type = "Repeater"
            x = "0"
            y = "0"
            model = "launcher.results"
            delegate = "r"

            [components.r]
            [[components.r.nodes]]
            id = "rr"
            type = "Rectangle"
            x = "0"
            y = "0"
            width = "50"
            height = "20"
            animate = [{ property = "opacity", from = 0, duration_ms = 200, delay = "index * 30" }]
            "#,
        );
        let out = resolve(&t, &state(3, 0), (400, 300), 0.0, None, None);
        let card = out.nodes.iter().find(|n| n.id == "card").unwrap();
        assert_eq!(card.rotation, -6.0);
        assert_eq!(card.scale, 1.05);
        let rr2 = out.nodes.iter().find(|n| n.id == "rr-2").unwrap();
        assert_eq!(rr2.animate[0].delay_ms, 60); // index * 30 baked
    }

    #[test]
    fn uses_time_detection() {
        let t = theme_from(
            r#"
            [[nodes]]
            id = "pulse"
            type = "Rectangle"
            x = "sin(time) * 10"
            y = "0"
            width = "10"
            height = "10"
            "#,
        );
        let out = resolve(&t, &state(0, 0), (400, 300), 0.0, None, None);
        assert!(out.uses_time);
        assert!(!out.uses_impulse);
    }

    #[test]
    fn uses_impulse_detection() {
        let t = theme_from(
            r#"
            [[nodes]]
            id = "slam"
            type = "Rectangle"
            x = "20 + shake(12, 1)"
            y = "0"
            width = "10"
            height = "10"
            "#,
        );
        let out = resolve(&t, &state(0, 0), (400, 300), 0.0, None, None);
        assert!(out.uses_impulse);
        assert!(!out.uses_time);
        assert_eq!(out.nodes[0].x, 20.0, "hit defaults to 0 so shake is a no-op");
    }
}

#[cfg(test)]
mod atlus_theme_tests {
    use super::*;
    use crate::launcher::{LauncherState, ObservableState};
    use crate::ui_runtime::theme::Theme;

    fn load() -> (Theme, impl Fn(&str, f32, &str) -> f32) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("themes/atlus/theme.toml");
        let t = Theme::load_from_file(&path).expect("load atlus");
        let engine = crate::ui_runtime::text::TextEngine::new();
        engine.load_fonts_from_dir(path.parent().unwrap());
        let meas = move |t: &str, size: f32, family: &str| engine.measure(t, size, cosmic_text::Weight::NORMAL, family);
        (t, meas)
    }

    fn demo(n: usize, sel: usize) -> LauncherState {
        let mut s = LauncherState::new();
        s.set_results(
            (0..n)
                .map(|i| hiren_shared::AppEntry::run(format!("id{i}"), format!("App{i}"), format!("app{i}")))
                .collect(),
        );
        s.selected_index = sel;
        s
    }

    #[test]
    fn atlus_is_an_editorial_overlay_not_a_card() {
        let (t, meas) = load();
        assert_eq!(t.window.time_hz, Some(60));
        assert!(t.window.transparent);
        assert!(theme_uses_token(&t, &["launching"]), "exit motion binds launching");
        let out = resolve(&t, &demo(5, 0), (1080, 640), 0.5, Some(&meas), None);
        assert!(out.uses_impulse, "selection impact");
        assert!(out.uses_time, "caret binds time");
        assert!(out.nodes.iter().any(|n| n.id == "cream_page" && n.points.len() >= 6));
        assert!(out.nodes.iter().any(|n| n.id == "navy_field" && n.points.len() >= 6));
        assert!(out.nodes.iter().any(|n| n.id == "selector" && n.points.len() >= 4));
        let cream = out.nodes.iter().find(|n| n.id == "cream_page").unwrap();
        let navy = out.nodes.iter().find(|n| n.id == "navy_field").unwrap();
        let cream_left = cream.x + cream.points.iter().map(|(x, _)| *x).fold(f32::MAX, f32::min);
        let cream_right = cream.x + cream.points.iter().map(|(x, _)| *x).fold(f32::MIN, f32::max);
        let navy_left = navy.x + navy.points.iter().map(|(x, _)| *x).fold(f32::MAX, f32::min);
        let navy_right = navy.x + navy.points.iter().map(|(x, _)| *x).fold(f32::MIN, f32::max);
        assert!(cream_right > navy_left + 80.0, "plates overlap (cream_r={cream_right} navy_l={navy_left})");
        assert!(cream_left > 12.0, "cream inset from the left, x0={cream_left}");
        assert!(navy_right < 1070.0, "navy recedes from the right, x1={navy_right}");
        let plate = out.nodes.iter().find(|n| n.id == "archive_plate").unwrap();
        let plate_right = plate.x + plate.points.iter().map(|(x, _)| *x).fold(0.0f32, f32::max);
        assert!(plate_right < 1072.0, "dossier fully in frame, right={plate_right}");
        assert!(plate_right < navy_right + 8.0, "dossier sits on the navy plate");
        let full_round = out.nodes.iter().any(|n| {
            n.kind == "Rectangle"
                && n.x.abs() < 1.0
                && n.y.abs() < 1.0
                && (n.width - 1080.0).abs() < 2.0
                && (n.height - 640.0).abs() < 2.0
                && n.radius > 8.0
        });
        assert!(!full_round, "must not be a centered rounded window card");
        let title = out.nodes.iter().find(|n| n.id == "title").expect("title");
        assert!(title.text.as_deref().unwrap_or("").contains("HIR"));
        let inquire = out.nodes.iter().find(|n| n.id == "inquire").expect("inquire");
        assert_eq!(inquire.text.as_deref(), Some("INQUIRE"));
        assert!(inquire.radius < 1.0, "search is a labeled rule, not a rounded input");
        assert!(out.nodes.iter().all(|n| n.id != "search_ph"), "no meme caption in the inquire field");
    }

    #[test]
    fn atlus_selector_is_independent_and_selection_moves_the_composition() {
        let (t, meas) = load();
        let a = resolve(&t, &demo(6, 0), (1080, 640), 1.0, Some(&meas), None);
        let b = resolve(&t, &demo(6, 2), (1080, 640), 1.0, Some(&meas), None);
        let sel_a = a.nodes.iter().find(|n| n.id == "selector").unwrap();
        let sel_b = b.nodes.iter().find(|n| n.id == "selector").unwrap();
        assert!(sel_a.height > 70.0, "selector is larger than a row");
        assert!(sel_b.y - sel_a.y > 80.0, "selector travels with selection ({} -> {})", sel_a.y, sel_b.y);
        let name0 = a.nodes.iter().find(|n| n.id == "row_name-0").unwrap();
        let name1 = a.nodes.iter().find(|n| n.id == "row_name-1").unwrap();
        assert!(name0.x > name1.x, "selected row steps forward of its neighbours ({} vs {})", name0.x, name1.x);
        let dossier_a = a.nodes.iter().find(|n| n.id == "archive_name").unwrap();
        let dossier_b = b.nodes.iter().find(|n| n.id == "archive_name").unwrap();
        assert_eq!(dossier_a.text.as_deref(), Some("App0"));
        assert_eq!(dossier_b.text.as_deref(), Some("App2"));
        let num_b = b.nodes.iter().find(|n| n.id == "bg_num").unwrap();
        assert_eq!(num_b.text.as_deref(), Some("3"));
        let bg_a = a.nodes.iter().find(|n| n.id == "bg_archive").unwrap();
        let bg_b = b.nodes.iter().find(|n| n.id == "bg_archive").unwrap();
        assert!((bg_b.x - bg_a.x).abs() > 1.0, "background type reacts to selection");
    }

    #[test]
    fn atlus_empty_hides_dossier_and_shows_stamp() {
        let (t, meas) = load();
        let mut s = LauncherState::new();
        s.query = "zzz".into();
        let out = resolve(&t, &s, (1080, 640), 0.5, Some(&meas), None);
        assert!(out.nodes.iter().any(|n| n.id == "empty_stamp"));
        assert!(out.nodes.iter().all(|n| n.id != "archive_name"));
        assert!(out.nodes.iter().all(|n| n.id != "selector"));
    }

    #[test]
    fn atlus_select_impact_offsets_planes() {
        let (t, meas) = load();
        let s = demo(5, 0);
        let rest = resolve(&t, &s, (1080, 640), 0.0, Some(&meas), None);
        let mut slam = crate::ui_runtime::binding::Impulse::default();
        slam.hit = 1.0;
        slam.since_select = 0.0;
        let hit = resolve_with(&t, &s, (1080, 640), 0.0, slam, Some(&meas), None);
        let cream_r = rest.nodes.iter().find(|n| n.id == "cream_page").unwrap();
        let cream_h = hit.nodes.iter().find(|n| n.id == "cream_page").unwrap();
        assert!((cream_h.x - cream_r.x).abs() > 2.0, "select slams the manuscript page");
        assert!(hit.nodes.iter().all(|n| n.id != "hit_slash"), "no full-window slash on select");
        assert!(rest.nodes.iter().all(|n| n.id != "hit_slash"));
    }

    #[test]
    fn atlus_runtime_settles_and_writes_png() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("themes/atlus/theme.toml");
        let theme = Theme::load_from_file(&path).expect("load atlus");
        let state = ObservableState::new(LauncherState::new());
        state.update(|s| {
            s.query = "f".into();
            s.set_results(vec![
                hiren_shared::AppEntry::drun(
                    "firefox".into(),
                    "Firefox".into(),
                    "firefox".into(),
                    Some("Browse the Web".into()),
                    "web".into(),
                ),
                hiren_shared::AppEntry::run("foot".into(), "Foot terminal".into(), "foot".into()),
                hiren_shared::AppEntry::run("code".into(), "Visual Studio Code".into(), "code".into()),
            ]);
            s.selected_index = 0;
        });
        let mut rt = crate::ui_runtime::UiRuntime::new(theme, state.clone());
        let size = (1080u32, 640u32);
        let _ = rt.resolve(size);
        std::thread::sleep(std::time::Duration::from_millis(450));
        let rest = rt.resolve(size);
        let cream = rest.nodes.iter().find(|n| n.id == "cream_page").expect("cream");
        assert!(cream.opacity > 0.9, "manuscript settled opaque, o={}", cream.opacity);
        let row = rest.nodes.iter().find(|n| n.id == "row_name-0").expect("row");
        assert!(row.opacity > 0.5, "results must remain readable, o={}", row.opacity);
        assert!(row.x > 80.0, "selected name sits on the cascade, x={}", row.x);
        let plate = rest.nodes.iter().find(|n| n.id == "archive_plate").expect("plate");
        assert!(plate.x < 900.0, "dossier plate must settle on-screen, x={}", plate.x);
        let plate_right = plate.x + plate.points.iter().map(|(x, _)| *x).fold(0.0f32, f32::max);
        assert!(plate_right < 1072.0, "dossier fully in frame after settle, right={plate_right}");
        let query = rest.nodes.iter().find(|n| n.id == "search_text").expect("query");
        assert_eq!(query.text.as_deref(), Some("f"));
        let png = rt.render_nodes(&rest.nodes, size, 1.0);
        std::fs::write("/tmp/hiren-atlus-rest.png", png.encode_png().expect("png")).ok();
        state.update(|s| s.selected_index = 1);
        let slam = rt.resolve(size);
        assert!(slam.nodes.iter().any(|n| n.id == "selector"));
        let dossier = slam.nodes.iter().find(|n| n.id == "archive_name").unwrap();
        assert_eq!(dossier.text.as_deref(), Some("Foot terminal"));
        let slam_png = rt.render_nodes(&slam.nodes, size, 1.0);
        std::fs::write("/tmp/hiren-atlus-slam.png", slam_png.encode_png().expect("png")).ok();
    }
}

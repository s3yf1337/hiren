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

use super::binding::{eval_bool, eval_f32, eval_str, EvalContext, SharedDiag, Measurer};
use super::node::{NodeAction, ResolvedNode};
use super::theme::{Theme, NodeDef, AnimateDef};
use crate::launcher::LauncherState;
use std::collections::HashMap;

pub struct ResolveOutput {
    pub nodes: Vec<ResolvedNode>,
    /// True when any geometry binding references `time` — the caller should
    /// keep producing frames for continuous animation.
    pub uses_time: bool,
}

pub fn resolve(
    theme: &Theme,
    state: &LauncherState,
    window_size: (u32, u32),
    time: f32,
    measure: Option<Measurer>,
    diag: Option<SharedDiag>,
) -> ResolveOutput {
    let uses_time = theme_uses_time(theme);
    let mut ctx = EvalContext::new(state, window_size, time);
    ctx.measure = measure;
    ctx.diag = diag;
    let mut out = Vec::new();
    for node in &theme.nodes {
        resolve_node(node, theme, &mut ctx, 0.0, 0.0, None, &mut out);
    }
    // Stable sort by z keeps document order within a layer.
    out.sort_by_key(|n| n.z);
    ResolveOutput { nodes: out, uses_time }
}

/// Cheap scan: does any geometry/visual binding mention `time`?
fn theme_uses_time(theme: &Theme) -> bool {
    fn expr_uses_time(e: &str) -> bool {
        e.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .any(|t| t == "time")
    }
    fn node_uses_time(n: &NodeDef) -> bool {
        let prop_hit = n.props.values().any(|v| expr_uses_time(v));
        let points_hit = n.points.as_deref().map(expr_uses_time).unwrap_or(false);
        let child_hit = n.children.iter().any(node_uses_time);
        let anim_hit = n.animate.iter().any(|a| {
            matches!(&a.delay, Some(super::theme::DelaySpec::Expr(e)) if expr_uses_time(e))
                || matches!(&a.from, Some(super::theme::FromSpec::Expr(e)) if expr_uses_time(e))
        });
        [&n.x, &n.y, &n.width, &n.height, &n.visible, &n.opacity, &n.rotation, &n.skew, &n.scale]
            .iter()
            .any(|f| f.as_deref().map(expr_uses_time).unwrap_or(false))
            || prop_hit || points_hit || child_hit || anim_hit
    }
    theme.nodes.iter().any(node_uses_time)
        || theme.components.values().flat_map(|c| &c.nodes).any(node_uses_time)
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

    let background = props
        .get("background")
        .or_else(|| props.get("fill"))
        .and_then(|c| super::render::solid_color(c));
    let color = props.get("color").and_then(|c| super::render::solid_color(c));
    let radius = props
        .get("radius")
        .and_then(|s| s.trim().parse::<f32>().ok())
        .unwrap_or(0.0);

    let text = def.text.as_ref().map(|e| eval_str(e, ctx));
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
            let (xs, ys) = pair.split_once(',')?;
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
        .unwrap_or(if use_results && layout != "vertical" { 9.0 } else { 0.0 }) as usize;
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
            "circular" | "row" => {
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
            "row" => (0.0, 0.0), // row spacing resolved by delegates; kept free
            _ => (0.0, idx as f32 * (item_h + gap)), // vertical
        };

        let suffix = format!("-{idx}");
        // Delegate instances are scissored to the repeater's box so a scrolled
        // list slides under the rest of the UI instead of over it. Rings place
        // items around the center — no clip there.
        let clip = match layout {
            "vertical" => Some((origin_x - clip_pad, clip_y - 0.5, view_w + clip_pad * 2.0, view_h + 1.0)),
            "row" => Some((origin_x, clip_y - 0.5, view_w, item_h + 1.0)),
            _ => None,
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
    let background = props
        .get("background")
        .or_else(|| props.get("fill"))
        .and_then(|c| super::render::solid_color(c));
    let color = props.get("color").and_then(|c| super::render::solid_color(c));
    let radius = props.get("radius").and_then(|s| s.trim().parse::<f32>().ok()).unwrap_or(0.0);
    let text = def.text.as_ref().map(|e| eval_str(e, ctx));
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
        let mut t: Theme = toml::from_str(toml).unwrap();
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
    }
}

#[cfg(test)]
mod font_tests {
    use super::*;

    #[test]
    fn atlus_font_family_resolves() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("themes/atlus/theme.toml");
        let t = Theme::load_from_file(&path).expect("load atlus");
        let mut s = LauncherState::new();
        let entries: Vec<hiren_shared::AppEntry> = (0..5)
            .map(|i| hiren_shared::AppEntry::run(format!("id{i}"), format!("App{i}"), format!("app{i}")))
            .collect();
        s.set_results(entries);
        let out = resolve(&t, &s, (860, 560), 0.0, None, None);
        let wm = out.nodes.iter().find(|n| n.id == "wm_h").expect("wm_h");
        assert_eq!(wm.props.get("font_family").map(|s| s.as_str()), Some("Antonio"));
        let bar_sel = out.nodes.iter().find(|n| n.id == "row_bar-0").expect("row0");
        assert_eq!(bar_sel.props.get("background").map(|s| s.as_str()), Some("#E60012"), "row 0 is selected");
        let bar = out.nodes.iter().find(|n| n.id == "row_bar-1").expect("row1");
        assert_eq!(bar.props.get("background").map(|s| s.as_str()), Some("#FFFFFF"));
        assert_eq!(bar.skew, -18.0);
        let star = out.nodes.iter().find(|n| n.id == "mass_star").expect("star");
        assert_eq!(star.points.len(), 8, "polygon points parsed");
    }
}

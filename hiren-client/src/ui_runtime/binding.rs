//! Binding evaluation — the one expression engine every node property goes through.
//!
//! Bindings are strings kept deliberately small and UI-focused. Supported forms:
//!
//!   literals            "20"   "hello"   'a string'   "#89b4fa"   "rgba(137,180,250,0.2)"
//!   property paths      launcher.query   launcher.selected_index   window.width
//!   repeater locals     index  is_selected  item_name  item_exec  count  ...
//!   indexed results     launcher.results[0].name
//!   arithmetic          launcher.selected_index * 56 + 12
//!   comparisons         launcher.results_count > 0   item_name == 'Firefox'
//!   logic               a && b   a || b   !a
//!   conditionals        cond ? value_if_true : value_if_false   (lazy branches)
//!   functions           min(a,b) max(a,b) clamp(v,lo,hi) mod(a,n) abs floor ceil round sin cos sqrt pow
//!                       hash(n)  shake(amp, seed)  type_shake(amp, seed)
//!   impulse             hit  hit_type  since_select  since_type
//!   measurement         text_width(expr, font_size)  — measured text width in px
//!
//! Anything that is not recognized as one of the above passes through unchanged,
//! so color literals and plain text keep working without escaping.

use super::color::{parse_color_str, Color};
use crate::launcher::LauncherState;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// Optional diagnostic sink: records expressions that failed to evaluate,
/// so `--validate-themes` can report theme bugs instead of silently defaulting.
#[derive(Default)]
pub struct Diag {
    pub warnings: Vec<String>,
}

impl Diag {
    pub fn warn(&mut self, node: &str, what: &str, err: &str) {
        let w = format!("node `{node}`: {what}: {err}");
        if !self.warnings.contains(&w) {
            self.warnings.push(w);
        }
    }
}

pub type SharedDiag = Rc<RefCell<Diag>>;

/// Event-driven impact envelope (Persona 5: slam on input, not idle wobble).
///
/// `hit` spikes to 1 when the selection changes (and on first open), holds a
/// couple of frames, then decays. `hit_type` does the same for query edits.
/// `shake(amp, seed)` turns that envelope into stepped, seeded camera offsets.
#[derive(Debug, Clone, Copy, Default)]
pub struct Impulse {
    pub hit: f32,
    pub hit_type: f32,
    pub since_select: f32,
    pub since_type: f32,
    pub select_gen: u32,
    pub type_gen: u32,
}

impl Impulse {
    pub fn active(&self) -> bool {
        self.hit > 0.02 || self.hit_type > 0.02
    }
}

/// Hold ~2 frames at 60 Hz, then exponential decay (~200 ms to near-zero).
pub fn hit_envelope(elapsed: f32) -> f32 {
    const HOLD: f32 = 0.036;
    const TAU: f32 = 0.065;
    if elapsed <= HOLD {
        1.0
    } else {
        (-(elapsed - HOLD) / TAU).exp()
    }
}

/// Deterministic `0..1` from a numeric seed (stepped P5 shake, not `sin`).
pub fn hash01(n: f64) -> f64 {
    let mut x = n.floor() as i64 as u32;
    x = x.wrapping_add(0x9e3779b9);
    x = x.wrapping_mul(0x45d9f3b);
    x ^= x >> 16;
    x = x.wrapping_mul(0x45d9f3b);
    x ^= x >> 16;
    (x as f64) / (u32::MAX as f64 + 1.0)
}

/// Stepped bipolar offset in `[-amp, amp]`, gated by `hit`.
/// First ~36 ms is a full-amplitude slam; after that, 42 Hz quantized chaos.
pub fn shake(amp: f64, seed: f64, hit: f64, since: f64) -> f64 {
    if hit < 0.008 || amp.abs() < 1e-9 {
        return 0.0;
    }
    const HOLD: f64 = 0.036;
    if since <= HOLD {
        let dir = if hash01(seed.floor()) >= 0.5 { 1.0 } else { -1.0 };
        return dir * amp * hit;
    }
    let frame = (since * 42.0).floor() + seed.floor() * 17.0;
    let bipolar = hash01(frame) * 2.0 - 1.0;
    let stepped = (bipolar * 4.0).round() / 4.0;
    let stepped = if stepped.abs() < 0.01 { 0.25 } else { stepped };
    stepped * amp * hit
}

/// Text measurement hook (implemented by the text engine; optional).
/// Arguments: (text, font_size, family) — empty family = default sans-serif.
pub type Measurer<'a> = &'a dyn Fn(&str, f32, &str) -> f32;

/// Context available during binding evaluation.
#[derive(Clone)]
pub struct EvalContext<'a> {
    pub launcher: &'a LauncherState,
    /// Local scope (repeater instance: index, is_selected, item_name, ...).
    pub locals: HashMap<String, String>,
    /// Window size in logical pixels.
    pub window_size: (u32, u32),
    /// Animation clock (seconds since runtime start).
    pub time: f32,
    /// Selection / typing impact (see `Impulse`).
    pub impulse: Impulse,
    pub measure: Option<Measurer<'a>>,
    pub diag: Option<SharedDiag>,
    /// Node id for diagnostics.
    pub node: String,
}

impl<'a> EvalContext<'a> {
    pub fn new(launcher: &'a LauncherState, window_size: (u32, u32), time: f32) -> Self {
        Self {
            launcher,
            locals: HashMap::new(),
            window_size,
            time,
            impulse: Impulse::default(),
            measure: None,
            diag: None,
            node: String::new(),
        }
    }

    pub fn with_locals(mut self, locals: HashMap<String, String>) -> Self {
        self.locals = locals;
        self
    }

    fn record_diag(&mut self, what: &str, err: &str) {
        if let Some(d) = &self.diag {
            d.borrow_mut().warn(&self.node.clone(), what, err);
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

pub fn eval_str(expr: &str, ctx: &mut EvalContext) -> String {
    eval_inner(expr.trim(), ctx)
}

pub fn eval_f32(expr: &str, ctx: &mut EvalContext, default: f32) -> f32 {
    let s = eval_str(expr, ctx);
    match s.trim().parse::<f32>() {
        Ok(v) => v,
        Err(_) => {
            // Numeric expressions that meval already reduced come back as numbers;
            // reaching here with leftover text usually means a theme typo.
            if !s.trim().is_empty() && looks_numeric(expr) {
                ctx.record_diag("binding", &format!("`{expr}` → `{s}` is not a number"));
            }
            default
        }
    }
}

pub fn eval_bool(expr: &str, ctx: &mut EvalContext, default: bool) -> bool {
    match eval_str(expr, ctx).trim() {
        "true" | "1" => true,
        "false" | "0" | "" => false,
        other => match other.parse::<f32>() {
            Ok(v) => v != 0.0,
            Err(_) => default,
        },
    }
}

pub fn eval_color(expr: &str, ctx: &mut EvalContext, default: Color) -> Color {
    let s = eval_str(expr, ctx);
    parse_color_str(&s).unwrap_or(default)
}

fn looks_numeric(expr: &str) -> bool {
    // Heuristic: only complain when the author clearly expected a number.
    expr.chars().all(|c| c.is_ascii_digit() || "+-*/(). ".contains(c) || c.is_ascii_alphabetic())
        && expr.chars().any(|c| c.is_ascii_digit() || "+-*/".contains(c))
}

// ---------------------------------------------------------------------------
// Core recursive evaluation
// ---------------------------------------------------------------------------

fn eval_inner(expr: &str, ctx: &mut EvalContext) -> String {
    let expr = expr.trim();
    if expr.is_empty() {
        return String::new();
    }

    // Quoted string literal (also strips surrounding whitespace).
    if let Some(inner) = strip_quotes(expr) {
        return inner.to_string();
    }

    // Lazy ternary: cond ? a : b
    if let Some((cond, a, b)) = split_ternary(expr) {
        let chosen = if eval_bool(cond, ctx, false) { a } else { b };
        return eval_inner(chosen, ctx);
    }

    // Boolean logic and comparisons.
    if let Some(v) = eval_logic(expr, ctx) {
        return v.to_string();
    }

    // text_width(expr, size[, family])
    if expr.starts_with("text_width(") && expr.ends_with(')') {
        if let Some(args) = split_top_args(&expr["text_width(".len()..expr.len() - 1]) {
            if args.len() >= 2 {
                let text = eval_inner(args[0].trim(), ctx);
                let size = eval_f32(args[1].trim(), ctx, 15.0);
                let family = if args.len() >= 3 { eval_inner(args[2].trim(), ctx) } else { String::new() };
                if let Some(m) = ctx.measure {
                    let w = m(&text, size, &family);
                    return format!("{}", w.round());
                }
                return "0".into();
            }
        }
    }

    // upper(expr) / lower(expr) — case-fold a string value (P5-style caps UI;
    // lets caret measurement match uppercased display text)
    for (fun, fold) in [("upper(", 0u8), ("lower(", 1)] {
        if expr.starts_with(fun) && expr.ends_with(')') {
            if let Some(args) = split_top_args(&expr[fun.len()..expr.len() - 1]) {
                let text = eval_inner(args[0].trim(), ctx);
                return if fold == 0 { text.to_uppercase() } else { text.to_lowercase() };
            }
        }
    }

    // initial(expr) — first grapheme (icon-chip fallback when item_icon is empty)
    if expr.starts_with("initial(") && expr.ends_with(')') {
        if let Some(args) = split_top_args(&expr["initial(".len()..expr.len() - 1]) {
            let text = eval_inner(args[0].trim(), ctx);
            return text.chars().next().map(|c| c.to_string()).unwrap_or_default();
        }
    }

    // Exact property path / local (including indexed results).
    if let Some(v) = property_value(expr, ctx) {
        return v;
    }

    // Arithmetic / math functions via meval after path substitution.
    match eval_numeric(expr, ctx) {
        Some(v) => {
            if (v - v.round()).abs() < 1e-9 {
                format!("{}", v.round() as i64)
            } else {
                format!("{}", (v * 1e4).round() / 1e4)
            }
        }
        None => expr.to_string(), // passthrough: plain text, color literal, etc.
    }
}

fn eval_logic(expr: &str, ctx: &mut EvalContext) -> Option<bool> {
    for op in ["||", "&&"] {
        if let Some((a, b)) = split_top(expr, op) {
            let (va, vb) = (eval_bool(a.trim(), ctx, false), eval_bool(b.trim(), ctx, false));
            return Some(if op == "||" { va || vb } else { va && vb });
        }
    }
    // Negation of a top-level expression.
    if let Some(rest) = expr.strip_prefix('!') {
        if !rest.trim_start().starts_with('=') {
            return Some(!eval_bool(rest.trim(), ctx, false));
        }
    }
    for op in ["==", "!=", ">=", "<=", ">", "<"] {
        if let Some((a, b)) = split_top(expr, op) {
            let av = eval_inner(a.trim(), ctx);
            let bv = eval_inner(b.trim(), ctx);
            let (an, bn) = (av.trim().parse::<f64>().ok(), bv.trim().parse::<f64>().ok());
            let res = match (an, bn) {
                (Some(x), Some(y)) => match op {
                    "==" => (x - y).abs() < 1e-9,
                    "!=" => (x - y).abs() >= 1e-9,
                    ">=" => x >= y,
                    "<=" => x <= y,
                    ">" => x > y,
                    "<" => x < y,
                    _ => unreachable!(),
                },
                _ => {
                    let (as_, bs) = (unquote(&av), unquote(&bv));
                    match op {
                        "==" => as_ == bs,
                        "!=" => as_ != bs,
                        ">=" => as_ >= bs,
                        "<=" => as_ <= bs,
                        ">" => as_ > bs,
                        "<" => as_ < bs,
                        _ => unreachable!(),
                    }
                }
            };
            return Some(res);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Property paths
// ---------------------------------------------------------------------------

fn property_value(expr: &str, ctx: &EvalContext) -> Option<String> {
    // locals first (they shadow launcher paths inside repeaters)
    if let Some(v) = ctx.locals.get(expr) {
        return Some(v.clone());
    }
    match expr {
        "launcher.query" => return Some(ctx.launcher.query.clone()),
        "launcher.selected_index" => return Some(ctx.launcher.selected_index.to_string()),
        "launcher.results_count" => return Some(ctx.launcher.results.len().to_string()),
        "launcher.loading" => return Some(ctx.launcher.loading.to_string()),
        "launcher.launching" => return Some(ctx.launcher.launching.to_string()),
        "launcher.error" => return Some(ctx.launcher.error.clone().unwrap_or_default()),
        "window.width" => return Some(ctx.window_size.0.to_string()),
        "window.height" => return Some(ctx.window_size.1.to_string()),
        "time" => return Some(format!("{}", ctx.time)),
        "hit" => return Some(format!("{}", ctx.impulse.hit)),
        "hit_type" => return Some(format!("{}", ctx.impulse.hit_type)),
        "since_select" => return Some(format!("{}", ctx.impulse.since_select)),
        "since_type" => return Some(format!("{}", ctx.impulse.since_type)),
        "true" => return Some("true".into()),
        "false" => return Some("false".into()),
        _ => {}
    }
    if let Some(rest) = expr.strip_prefix("launcher.selected_result.") {
        if let Some(r) = ctx.launcher.selected_result() {
            return Some(match rest {
                "name" => r.name.clone(),
                "exec" => r.exec.clone(),
                "id" => r.id.clone(),
                "description" => r.description.clone().unwrap_or_default(),
                "keywords" => r.keywords.clone(),
                "mode" => format!("{:?}", r.mode).to_lowercase(),
                "score" => r.score.to_string(),
                "icon" => super::icon::resolve(&r.icon),
                _ => return None,
            });
        }
        return Some(String::new());
    }
    if let Some(rest) = expr.strip_prefix("launcher.results[") {
        let bracket = rest.find(']')?;
        let idx: usize = rest[..bracket].trim().parse().ok()?;
        let suffix = &rest[bracket + 1..];
        let entry = ctx.launcher.results.get(idx)?;
        return Some(match suffix {
            "" => entry.name.clone(),
            ".name" => entry.name.clone(),
            ".exec" => entry.exec.clone(),
            ".id" => entry.id.clone(),
            ".description" => entry.description.clone().unwrap_or_default(),
            ".keywords" => entry.keywords.clone(),
            ".score" => entry.score.to_string(),
            ".icon" => super::icon::resolve(&entry.icon),
            _ => return None,
        });
    }
    None
}

/// Replace dotted property paths, numeric locals and `text_width(...)` calls
/// with their values so the expression can be handed to meval. Scans identifier
/// tokens (letters, digits, `_`, and `.` followed by a letter) so literals like
/// `1.5` are left intact.
fn substitute_paths(expr: &str, ctx: &mut EvalContext) -> String {
    let mut out = String::with_capacity(expr.len());
    let b: Vec<char> = expr.chars().collect();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == '\'' || c == '"' {
            // copy quoted region verbatim
            let q = c;
            out.push(c);
            i += 1;
            while i < b.len() {
                out.push(b[i]);
                if b[i] == q {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if expr[i..].starts_with("text_width(") && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == '_')) {
            // measured text width → numeric literal usable in arithmetic
            if let Some((args, end)) = scan_paren_args(&expr[i + "text_width".len()..]) {
                let arg_list = split_top_args(args);
                let mut value = 0.0f64;
                if let Some(list) = arg_list {
                    if list.len() >= 2 {
                        let text = eval_inner(list[0].trim(), ctx);
                        let size = eval_f32(list[1].trim(), ctx, 15.0);
                        let family = if list.len() >= 3 { eval_inner(list[2].trim(), ctx) } else { String::new() };
                        if let Some(m) = ctx.measure {
                            value = m(&text, size, &family) as f64;
                        }
                    }
                }
                out.push_str(&format!("{value}"));
                // `end` is the index of `)` in the slice that starts at `(`.
                i += "text_width".len() + end + 1;
                continue;
            }
        }
        if c.is_ascii_alphabetic() || c == '_' {
            // identifier token, may contain dots followed by letters (dotted path)
            let start = i;
            i += 1;
            while i < b.len() {
                if b[i].is_ascii_alphanumeric() || b[i] == '_' {
                    i += 1;
                } else if b[i] == '.' && i + 1 < b.len() && (b[i + 1].is_ascii_alphabetic() || b[i + 1] == '_') {
                    i += 1;
                } else {
                    break;
                }
            }
            let token: String = b[start..i].iter().collect();
            match path_number(&token, ctx) {
                Some(v) => out.push_str(&format!("{v}")),
                None => out.push_str(&token),
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// Given a string starting at `(`, return the balanced paren contents and the
/// index of the matching `)` (relative to the opening paren).
fn scan_paren_args(s: &str) -> Option<(&str, usize)> {
    let b = s.as_bytes();
    if b.first() != Some(&b'(') {
        return None;
    }
    let mut depth = 0i32;
    for (i, ch) in b.iter().enumerate() {
        match ch {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((&s[1..i], i));
                }
            }
            _ => {}
        }
    }
    None
}

fn path_number(token: &str, ctx: &EvalContext) -> Option<f64> {
    // Numeric locals (index, count, ...) participate in arithmetic.
    if let Some(v) = ctx.locals.get(token) {
        if let Ok(f) = v.parse::<f64>() {
            return Some(f);
        }
        if v == "true" {
            return Some(1.0);
        }
        if v == "false" {
            return Some(0.0);
        }
        return None; // string-valued local inside arithmetic: leave as-is
    }
    match token {
        "launcher.selected_index" => Some(ctx.launcher.selected_index as f64),
        "launcher.results_count" => Some(ctx.launcher.results.len() as f64),
        "launcher.launching" => Some(if ctx.launcher.launching { 1.0 } else { 0.0 }),
        "launcher.loading" => Some(if ctx.launcher.loading { 1.0 } else { 0.0 }),
        "window.width" => Some(ctx.window_size.0 as f64),
        "window.height" => Some(ctx.window_size.1 as f64),
        "time" => Some(ctx.time as f64),
        "hit" => Some(ctx.impulse.hit as f64),
        "hit_type" => Some(ctx.impulse.hit_type as f64),
        "since_select" => Some(ctx.impulse.since_select as f64),
        "since_type" => Some(ctx.impulse.since_type as f64),
        "pi" => Some(std::f64::consts::PI),
        "tau" => Some(std::f64::consts::TAU),
        "is_selected" => None,
        _ => None,
    }
}

/// Parsed-expression cache: theme expressions are stable strings, but the
/// substituted text changes with state. Parsing dominates frame time without
/// this (thousands of `meval` parses per frame at ~2µs each). Cleared when it
/// grows past the cap so query-dependent expressions don't leak memory.
fn cached_expr(s: &str) -> Option<meval::Expr> {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::str::FromStr;
    thread_local! {
        static CACHE: RefCell<HashMap<String, Option<meval::Expr>>> = RefCell::new(HashMap::new());
    }
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() > 4096 {
            c.clear();
        }
        if let Some(e) = c.get(s) {
            return e.clone();
        }
        let e = meval::Expr::from_str(s).ok();
        c.insert(s.to_string(), e.clone());
        e
    })
}

fn looks_like_math(expr: &str) -> bool {
    expr.chars().any(|c| c.is_ascii_digit())
        || expr.contains("launcher.")
        || expr.contains("window.")
        || expr.contains("text_width")
        || expr.contains("shake")
        || expr.contains("hash")
        || expr.contains("since_select")
        || expr.contains("since_type")
        || expr.contains("index")
        || expr.contains("count")
        || expr.contains("time")
        || expr.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).any(|t| t == "hit" || t == "hit_type")
}

fn eval_numeric(expr: &str, ctx: &mut EvalContext) -> Option<f64> {
    // Fast reject: no digits and no known numeric path → not arithmetic.
    if !looks_like_math(expr) {
        return None;
    }
    let substituted = substitute_paths(expr, ctx);
    let hit = ctx.impulse.hit as f64;
    let hit_type = ctx.impulse.hit_type as f64;
    let since_select = ctx.impulse.since_select as f64;
    let since_type = ctx.impulse.since_type as f64;
    let mut mctx = meval::Context::new();
    mctx.var("pi", std::f64::consts::PI)
        .var("tau", std::f64::consts::TAU)
        .var("time", ctx.time as f64)
        .var("hit", hit)
        .var("hit_type", hit_type)
        .var("since_select", since_select)
        .var("since_type", since_type)
        .func("hash", hash01)
        .func2("shake", move |amp, seed| shake(amp, seed, hit, since_select))
        .func2("type_shake", move |amp, seed| shake(amp, seed, hit_type, since_type))
        .func2("min", f64::min)
        .func2("max", f64::max)
        .func3("clamp", |v: f64, lo: f64, hi: f64| v.max(lo).min(hi))
        .func2("mod", |a: f64, n: f64| {
            if !n.is_finite() || n.abs() < 1e-12 {
                0.0
            } else {
                a - (a / n).floor() * n
            }
        });
    // expose remaining numeric locals (index, count, selected_index, ...)
    for (k, v) in &ctx.locals {
        if k.contains('.') {
            continue;
        }
        if let Ok(f) = v.parse::<f64>() {
            mctx.var(k, f);
        } else if v == "true" {
            mctx.var(k, 1.0);
        } else if v == "false" {
            mctx.var(k, 0.0);
        }
    }
    match cached_expr(&substituted).and_then(|e| e.eval_with_context(&mctx).ok()) {
        Some(v) if v.is_finite() => Some(v),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Top-level token scanning helpers
// ---------------------------------------------------------------------------

fn strip_quotes(s: &str) -> Option<&str> {
    let b = s.as_bytes();
    if b.len() >= 2 && (b[0] == b'\'' || b[0] == b'"') && b[b.len() - 1] == b[0] {
        Some(&s[1..s.len() - 1])
    } else {
        None
    }
}

fn unquote(s: &str) -> &str {
    strip_quotes(s.trim()).unwrap_or(s.trim())
}

/// Find the first top-level occurrence of `needle` (not inside quotes, parens
/// or brackets) and split around it.
fn split_top<'a>(expr: &'a str, needle: &str) -> Option<(&'a str, &'a str)> {
    let bytes = expr.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < bytes.len() {
        // Skip UTF-8 continuation bytes so slicing stays on char boundaries.
        if bytes[i] & 0xC0 == 0x80 {
            i += 1;
            continue;
        }
        match bytes[i] {
            b'\'' | b'"' => {
                let q = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != q {
                    i += 1;
                }
            }
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            _ if depth == 0 => {
                if expr[i..].starts_with(needle) {
                    return Some((&expr[..i], &expr[i + needle.len()..]));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Split `cond ? a : b` at top level, handling nested ternaries.
fn split_ternary(expr: &str) -> Option<(&str, &str, &str)> {
    let q = split_top(expr, "?")?;
    let cond = q.0.trim();
    let rest = q.1;
    // find matching ':' — a nested '?' before it raises the pending count
    let bytes = rest.as_bytes();
    let mut depth = 0i32;
    let mut pending = 0i32;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] & 0xC0 == 0x80 {
            i += 1;
            continue;
        }
        match bytes[i] {
            b'\'' | b'"' => {
                let qt = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != qt {
                    i += 1;
                }
            }
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            _ if depth == 0 => {
                if bytes[i] == b'?' {
                    pending += 1;
                } else if bytes[i] == b':' {
                    if pending == 0 {
                        return Some((cond, rest[..i].trim(), rest[i + 1..].trim()));
                    }
                    pending -= 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Split a top-level comma-separated argument list.
fn split_top_args(s: &str) -> Option<Vec<&str>> {
    let mut args = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] & 0xC0 == 0x80 {
            i += 1;
            continue;
        }
        match b[i] {
            b'\'' | b'"' => {
                let q = b[i];
                i += 1;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
            }
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b',' if depth == 0 => {
                args.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if b.len() > 0 {
        args.push(&s[start..]);
    }
    if args.is_empty() {
        None
    } else {
        Some(args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launcher::LauncherState;
    use hiren_shared::AppEntry;

    fn ctx<'a>(s: &'a LauncherState) -> EvalContext<'a> {
        EvalContext::new(s, (640, 480), 0.0)
    }

    fn s_with_results(n: usize, sel: usize) -> LauncherState {
        let mut s = LauncherState::new();
        s.query = "fi".into();
        s.selected_index = sel;
        let v: Vec<AppEntry> = (0..n)
            .map(|i| AppEntry::run(format!("id{i}"), format!("App{i}"), format!("app{i}")))
            .collect();
        s.set_results(v);
        s
    }

    #[test]
    fn literals() {
        let s = LauncherState::new();
        let mut c = ctx(&s);
        assert_eq!(eval_str("hello", &mut c), "hello");
        assert_eq!(eval_str("'a,b'", &mut c), "a,b");
        assert_eq!(eval_str("20", &mut c), "20");
        assert_eq!(eval_str("#89b4fa", &mut c), "#89b4fa");
    }

    #[test]
    fn paths_and_arithmetic() {
        let s = s_with_results(7, 2);
        let mut c = ctx(&s);
        assert_eq!(eval_str("launcher.selected_index * 56 + 12", &mut c), "124");
        assert_eq!(eval_str("launcher.results_count", &mut c), "7");
        assert_eq!(eval_str("window.width / 2 - 140", &mut c), "180");
        assert_eq!(eval_str("launcher.results[1].name", &mut c), "App1");
        assert_eq!(eval_str("launcher.selected_result.exec", &mut c), "app2");
    }

    #[test]
    fn ternary_and_logic() {
        let s = s_with_results(3, 1);
        let mut c = ctx(&s);
        c.locals.insert("is_selected".into(), "true".into());
        assert_eq!(eval_str("launcher.results_count > 0 ? 10 : 20", &mut c), "10");
        assert_eq!(
            eval_str("is_selected ? rgba(255,0,0,0.5) : transparent", &mut c),
            "rgba(255,0,0,0.5)"
        );
        c.locals.insert("is_selected".into(), "false".into());
        assert_eq!(
            eval_str("is_selected ? rgba(255,0,0,0.5) : transparent", &mut c),
            "transparent"
        );
        assert_eq!(eval_bool("launcher.results_count > 0", &mut c, false), true);
        assert_eq!(eval_bool("launcher.results_count > 10 || launcher.query == 'fi'", &mut c, false), true);
        assert_eq!(eval_bool("!launcher.loading", &mut c, false), true);
        assert_eq!(eval_bool("launcher.query == ''", &mut c, false), false);
    }

    #[test]
    fn locals_and_functions() {
        let s = s_with_results(8, 0);
        let mut c = ctx(&s);
        c.locals.insert("index".into(), "3".into());
        c.locals.insert("is_selected".into(), "false".into());
        c.locals.insert("count".into(), "8".into());
        assert_eq!(eval_str("index * 360 / count", &mut c), "135");
        assert_eq!(eval_str("cos(index * 0.5) * 170", &mut c), "12.0253"); // cos(1.5)*170
        assert_eq!(eval_str("clamp(index * 100, 0, 250)", &mut c), "250");
        assert_eq!(eval_str("min(4, 9) + max(1, 2)", &mut c), "6");
        assert_eq!(eval_str("mod(7, 5)", &mut c), "2");
        assert_eq!(eval_str("mod(-1, 5)", &mut c), "4");
        assert_eq!(eval_str("mod(floor(1.5) + 2, 5)", &mut c), "3");
    }

    #[test]
    fn passthrough_strings() {
        let s = LauncherState::new();
        let mut c = ctx(&s);
        assert_eq!(eval_str("linear-gradient(180deg, #111 0%, #222 100%)", &mut c), "linear-gradient(180deg, #111 0%, #222 100%)");
        assert_eq!(eval_str("H I R E N", &mut c), "H I R E N");
    }

    #[test]
    fn initial_and_text_width() {
        let s = s_with_results(2, 0);
        let mut c = ctx(&s);
        assert_eq!(eval_str("initial(launcher.query)", &mut c), "f");
        assert_eq!(eval_str("initial('')", &mut c), "");
        c.locals.insert("item_name".into(), "Alacritty".into());
        assert_eq!(eval_str("initial(item_name)", &mut c), "A");
    }

    #[test]
    fn time_binding() {
        let s = LauncherState::new();
        let mut c = EvalContext::new(&s, (640, 480), 1.5);
        assert_eq!(eval_str("sin(time * 2) * 10", &mut c), "1.4112"); // sin(3)*10
    }

    #[test]
    fn hash_and_shake_are_stepped_not_sine() {
        let h = hash01(1.0);
        assert!(h >= 0.0 && h < 1.0);
        assert!((hash01(1.0) - h).abs() < 1e-12, "deterministic");
        assert!((hash01(2.0) - h).abs() > 0.01, "changes with seed");
        assert_eq!(shake(10.0, 1.0, 0.0, 0.0), 0.0);
        let slam = shake(10.0, 1.0, 1.0, 0.0);
        assert!((slam.abs() - 10.0).abs() < 1e-9, "opening slam is full amplitude, got {slam}");
        let later = shake(10.0, 1.0, 0.6, 0.08);
        assert!(later.abs() > 0.5, "chaos window still offsets, got {later}");
        assert!((hit_envelope(0.0) - 1.0).abs() < 1e-6);
        assert!(hit_envelope(0.4) < 0.02);
    }

    #[test]
    fn hit_bindings_and_shake_fn() {
        let s = LauncherState::new();
        let mut c = EvalContext::new(&s, (640, 480), 0.0);
        c.impulse.hit = 1.0;
        c.impulse.since_select = 0.0;
        c.impulse.hit_type = 1.0;
        c.impulse.since_type = 0.0;
        let x = eval_f32("20 + shake(12, 1)", &mut c, 0.0);
        assert!((x - 20.0).abs() > 5.0, "shake at hit=1 moves the node, got {x}");
        assert_eq!(eval_f32("hit", &mut c, 0.0), 1.0);
        let typed = eval_f32("type_shake(8, 2)", &mut c, 0.0);
        assert!(typed.abs() > 3.0, "type_shake at hit_type=1, got {typed}");
        assert!(eval_f32("hash(3)", &mut c, -1.0) >= 0.0);
        c.impulse.hit = 0.0;
        assert_eq!(eval_f32("20 + shake(12, 1)", &mut c, 0.0), 20.0);
    }

    #[test]
    fn min_plus_shake_and_text_width() {
        let mut s = LauncherState::new();
        s.query = "f".into();
        let mut c = EvalContext::new(&s, (1100, 680), 0.0);
        let meas = |_: &str, _: f32, _: &str| 40.0;
        c.measure = Some(&meas);
        assert_eq!(
            eval_f32(
                "min(68 + text_width(upper(launcher.query), 42, 'Anton, Archivo Black, Titan One'), 532)",
                &mut c,
                -1.0,
            ),
            108.0
        );
        assert_eq!(
            eval_f32(
                "min(68 + text_width(upper(launcher.query), 42, 'Anton, Archivo Black, Titan One'), 532) + shake(14, 1) + type_shake(8, 1)",
                &mut c,
                -1.0,
            ),
            108.0
        );
    }
}

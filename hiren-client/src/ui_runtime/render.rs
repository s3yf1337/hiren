//! Renderer — tiny-skia + cosmic-text onto a premultiplied RGBA pixmap.
//!
//! Draw order = resolved node order (z-sorted). Every node is drawn through an
//! optional transform (rotation/scale around its center). Text is rasterized
//! into its own pixmap first, then composited — so transformed text works.

use std::collections::HashMap;
use std::path::PathBuf;

use tiny_skia::{
    Color as SkColor, GradientStop, LinearGradient, Paint, PathBuilder, Pixmap, PixmapPaint,
    Rect, SpreadMode, Transform,
};

use super::color::{parse_color_str, Color};
use super::node::ResolvedNode;
use super::text::{TextEngine, TextStyle};

pub struct Renderer {
    pub text: TextEngine,
    images: HashMap<PathBuf, Option<Pixmap>>,
    /// Scratch surface for scissored nodes (repeater viewports).
    scratch: Option<Pixmap>,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderer {
    pub fn new() -> Self {
        Self { text: TextEngine::new(), images: HashMap::new(), scratch: None }
    }

    /// Render at `width x height` physical pixels; nodes are authored in
    /// logical pixels and scaled by `scale` (hidpi).
    pub fn render(&mut self, nodes: &[ResolvedNode], width: u32, height: u32, scale: f32) -> Pixmap {
        let mut pixmap = Pixmap::new(width.max(1), height.max(1)).expect("pixmap");
        pixmap.fill(SkColor::TRANSPARENT);
        let scale = if scale.is_finite() && scale >= 0.25 { scale } else { 1.0 };
        for node in nodes {
            if !node.visible || node.opacity <= 0.004 {
                continue;
            }
            if let Some((cx, cy, cw, ch)) = node.clip {
                // Fast path: the transformed node lies fully inside the clip
                // rect, so the scissor is a no-op — draw directly instead of
                // paying a full-window scratch clear + composite per node.
                // (Shadows spill past the bounds and need the scratch.)
                let (nx0, ny0, nx1, ny1) = Self::transformed_bounds(node);
                if !node.props.contains_key("shadow")
                    && nx0 >= cx
                    && ny0 >= cy
                    && nx1 <= cx + cw
                    && ny1 <= cy + ch
                {
                    self.draw_node(node, &mut pixmap, scale);
                    continue;
                }
                // Scissor: draw into a scratch surface, then composite only
                // the part of the node that falls inside the clip rect.
                let pad = 80.0 * scale; // rotation/scale/shadow spill
                let bx = ((node.x.min(node.x + node.width) - pad).floor() as i32).max(0);
                let by = ((node.y.min(node.y + node.height) - pad).floor() as i32).max(0);
                let br = ((node.x.max(node.x + node.width) + pad).ceil() as i32).min(width as i32);
                let bb = ((node.y.max(node.y + node.height) + pad).ceil() as i32).min(height as i32);
                if br <= bx || bb <= by {
                    continue;
                }
                // Node ∩ clip rect, in physical pixels.
                let ix0 = bx.max((cx * scale).floor() as i32).max(0);
                let iy0 = by.max((cy * scale).floor() as i32).max(0);
                let ix1 = br.min(((cx + cw) * scale).ceil() as i32).min(width as i32);
                let iy1 = bb.min(((cy + ch) * scale).ceil() as i32).min(height as i32);
                if ix1 <= ix0 || iy1 <= iy0 {
                    continue; // entirely clipped away
                }
                let mut scratch = self.scratch.take().unwrap_or_else(|| {
                    Pixmap::new(width.max(1), height.max(1)).expect("scratch pixmap")
                });
                if scratch.width() < ix1 as u32 || scratch.height() < iy1 as u32 {
                    scratch = Pixmap::new(width.max(1), height.max(1)).expect("scratch pixmap");
                }
                scratch.fill(SkColor::TRANSPARENT);
                self.draw_node(node, &mut scratch, scale);
                if let Some(region) =
                    tiny_skia::IntRect::from_xywh(ix0, iy0, (ix1 - ix0) as u32, (iy1 - iy0) as u32)
                {
                    if let Some(sub) = scratch.clone_rect(region) {
                        pixmap.draw_pixmap(
                            ix0,
                            iy0,
                            sub.as_ref(),
                            &PixmapPaint::default(),
                            Transform::identity(),
                            None,
                        );
                    }
                }
                self.scratch = Some(scratch);
            } else {
                self.draw_node(node, &mut pixmap, scale);
            }
        }
        pixmap
    }

    /// Axis-aligned bounds of a node after its skew/rotation/scale transform
    /// (all act about the node center). Polygon nodes also include their
    /// transformed vertices so nothing sticks out of the fast-path bounds.
    fn transformed_bounds(node: &ResolvedNode) -> (f32, f32, f32, f32) {
        let t = Self::node_transform(node);
        let rect = node.rect();
        let corners = [
            (rect.x(), rect.y()),
            (rect.x() + rect.width(), rect.y()),
            (rect.x() + rect.width(), rect.y() + rect.height()),
            (rect.x(), rect.y() + rect.height()),
        ];
        let mut xs: Vec<f32> = Vec::with_capacity(4 + node.points.len());
        let mut ys: Vec<f32> = Vec::with_capacity(4 + node.points.len());
        for (px, py) in corners {
            let mut p = tiny_skia::Point::from_xy(px, py);
            t.map_point(&mut p);
            xs.push(p.x);
            ys.push(p.y);
        }
        for (lx, ly) in &node.points {
            let mut p = tiny_skia::Point::from_xy(node.x + lx, node.y + ly);
            t.map_point(&mut p);
            xs.push(p.x);
            ys.push(p.y);
        }
        let (x0, x1) = (xs.iter().cloned().fold(f32::INFINITY, f32::min), xs.iter().cloned().fold(f32::NEG_INFINITY, f32::max));
        let (y0, y1) = (ys.iter().cloned().fold(f32::INFINITY, f32::min), ys.iter().cloned().fold(f32::NEG_INFINITY, f32::max));
        (x0, y0, x1, y1)
    }

    fn node_transform_with_scale(node: &ResolvedNode, scale: f32) -> Transform {
        // physical = scale * (node transform about logical center)
        if scale == 1.0 {
            return Self::node_transform(node);
        }
        Transform::from_scale(scale, scale).pre_concat(Self::node_transform(node))
    }

    #[allow(clippy::only_used_in_recursion)]
    fn node_transform(node: &ResolvedNode) -> Transform {
        if node.rotation.abs() < 1e-4 && (node.scale - 1.0).abs() < 1e-4 && node.skew.abs() < 1e-4 {
            return Transform::identity();
        }
        let cx = node.x + node.width / 2.0;
        let cy = node.y + node.height / 2.0;
        // p' = T(c) · R · S · K · T(-c) · p  (skew/scale/rotate about the node
        // center; K is a horizontal shear, skew in degrees).
        let k = node.skew.to_radians().tan();
        Transform::identity()
            .pre_concat(Transform::from_translate(cx, cy))
            .pre_concat(Transform::from_rotate(node.rotation))
            .pre_concat(Transform::from_scale(node.scale, node.scale))
            .pre_concat(Transform::from_skew(k, 0.0))
            .pre_concat(Transform::from_translate(-cx, -cy))
    }

    fn draw_node(&mut self, node: &ResolvedNode, pixmap: &mut Pixmap, scale: f32) {
        let t = Self::node_transform_with_scale(node, scale);
        let opacity = node.opacity.clamp(0.0, 1.0);

        // Shape: polygon path when `points` resolve to ≥3 vertices, else the
        // (rounded) rect. Fill, shadow and border all use the same shape.
        let poly = polygon_path(node);
        let shape = |expand: f32| -> tiny_skia::Path {
            match &poly {
                Some(_) if expand.abs() < 0.01 => poly.clone().unwrap(),
                _ => rounded_rect_path(
                    Rect::from_xywh(
                        node.x - expand,
                        node.y - expand,
                        (node.width + expand * 2.0).max(1.0),
                        (node.height + expand * 2.0).max(1.0),
                    )
                    .unwrap_or_else(|| Rect::from_xywh(0.0, 0.0, 1.0, 1.0).unwrap()),
                    node.radius + expand,
                ),
            }
        };

        // Shadow: "dx dy blur color" — hard offset for polygons (comic style),
        // three feathered layers for rects.
        if let Some(shadow) = node.props.get("shadow") {
            if let Some((sx, sy, blur, sr, sg, sb, sa)) = parse_shadow(shadow) {
                let shadow_alpha = (sa as f32 * opacity) as u8;
                if shadow_alpha > 2 {
                    if poly.is_some() {
                        // Hard offset duplicate of the polygon (no blur).
                        let mut paint = Paint::default();
                        paint.set_color_rgba8(sr, sg, sb, shadow_alpha);
                        paint.anti_alias = true;
                        let path = polygon_path_offset(node, sx, sy).unwrap_or_else(|| shape(0.0));
                        pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, t, None);
                    } else {
                        for i in 0..3 {
                            let f = i as f32 / 2.0; // 0, 0.5, 1
                            let expand = blur * f * 0.5;
                            let layer_alpha = (shadow_alpha as f32 * (0.35 - f * 0.22).max(0.06)) as u8;
                            let mut paint = Paint::default();
                            paint.set_color_rgba8(sr, sg, sb, layer_alpha);
                            paint.anti_alias = true;
                            let rect = Rect::from_xywh(
                                node.x + sx - expand,
                                node.y + sy - expand,
                                (node.width + expand * 2.0).max(1.0),
                                (node.height + expand * 2.0).max(1.0),
                            );
                            let Some(rect) = rect else { continue };
                            let path = rounded_rect_path(rect, (node.radius + expand).max(0.0));
                            pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, t, None);
                        }
                    }
                }
            }
        }

        // Background: solid color or linear-gradient. Plain axis-aligned
        // rectangles skip path machinery entirely (fill_rect fast blit) —
        // they dominate node counts, and AA path fills cost ~2x here.
        let bg_raw = node.props.get("background").or_else(|| node.props.get("fill"));
        let solid_bg = bg_raw.and_then(|c| parse_color_str(c.trim()));
        let plain_rect = poly.is_none() && node.radius <= 0.5;
        if let Some(bg_raw) = bg_raw {
            let bg = bg_raw.trim();
            if bg.starts_with("linear-gradient") {
                if let Some(shader) = parse_linear_gradient(bg, node, opacity) {
                    let mut paint = Paint::default();
                    paint.shader = shader;
                    paint.anti_alias = true;
                    pixmap.fill_path(&shape(0.0), &paint, tiny_skia::FillRule::Winding, t, None);
                }
            } else if let Some((r, g, b, a)) = solid_bg {
                if plain_rect {
                    fill_rect(pixmap, node, t, r, g, b, (a as f32 * opacity) as u8);
                } else {
                    fill_shape(pixmap, &shape(0.0), t, r, g, b, (a as f32 * opacity) as u8);
                }
            }
        } else if let Some((r, g, b, a)) = node.background {
            if plain_rect {
                fill_rect(pixmap, node, t, r, g, b, (a as f32 * opacity) as u8);
            } else {
                fill_shape(pixmap, &shape(0.0), t, r, g, b, (a as f32 * opacity) as u8);
            }
        }

        // Image (PNG file via `src` prop).
        if node.kind == "Image" || node.props.contains_key("src") {
            self.draw_image(node, t, opacity, pixmap, scale);
        }

        // Text — rasterized into its own box, then composited (transforms apply).
        if let Some(text) = &node.text {
            if !text.is_empty() {
                let color = node.color.unwrap_or((205, 214, 244, 255));
                let size = node.props.get("font_size").and_then(|s| s.trim().parse::<f32>().ok()).unwrap_or(15.0);
                let align = node.props.get("align").map(|s| s.as_str()).unwrap_or("left");
                let weight = TextEngine::weight_of(&node.props);
                let family = node.props.get("font_family").cloned().unwrap_or_default();
                // Comic outline: "4px #0C0C0D"; hard sticker shadow: "4px 5px #0C0C0D".
                let outline = node
                    .props
                    .get("outline")
                    .and_then(|s| parse_border(s))
                    .map(|(w, r, g, b, a)| (w, (r, g, b, a)));
                let hard_shadow = node
                    .props
                    .get("text_shadow")
                    .and_then(|s| parse_shadow(s))
                    .map(|(dx, dy, _, r, g, b, a)| (dx, dy, (r, g, b, a)));
                let style = TextStyle::new(&family, outline, hard_shadow);
                // rasterize text at physical resolution for crisp hidpi output
                if let Some((sub, margin)) = self.text.render(
                    text,
                    node.width * scale,
                    node.height * scale,
                    size * scale,
                    weight,
                    align,
                    color,
                    &style,
                ) {
                    let paint = PixmapPaint {
                        opacity,
                        blend_mode: tiny_skia::BlendMode::SourceOver,
                        quality: if scale == 1.0 && node.rotation.abs() < 1e-4 && (node.scale - 1.0).abs() < 1e-4 && node.skew.abs() < 1e-4 {
                            tiny_skia::FilterQuality::Nearest
                        } else {
                            tiny_skia::FilterQuality::Bilinear
                        },
                    };
                    let tt = t.pre_translate(node.x - margin / scale, node.y - margin / scale);
                    pixmap.draw_pixmap(0, 0, sub.as_ref(), &paint, tt, None);
                }
            }
        }

        // Border: "1px rgba(...)" / "2px #fff".
        if let Some(border) = node.props.get("border") {
            if let Some((bw, br, bgc, bb, ba)) = parse_border(border) {
                let alpha = (ba as f32 * opacity) as u8;
                if alpha > 2 && bw > 0.1 {
                    let mut paint = Paint::default();
                    paint.set_color_rgba8(br, bgc, bb, alpha);
                    paint.anti_alias = true;
                    let stroke = tiny_skia::Stroke {
                        width: bw,
                        line_join: tiny_skia::LineJoin::Miter,
                        ..tiny_skia::Stroke::default()
                    };
                    pixmap.stroke_path(&shape(0.0), &paint, &stroke, t, None);
                }
            }
        }
    }

    fn draw_image(&mut self, node: &ResolvedNode, t: Transform, opacity: f32, pixmap: &mut Pixmap, scale: f32) {
        let Some(src) = node.props.get("src") else { return };
        let path = PathBuf::from(src.trim().trim_matches('"').trim_matches('\''));
        if !self.images.contains_key(&path) {
            let loaded = Pixmap::load_png(&path).ok();
            if loaded.is_none() {
                log::warn!("Image node `{}`: cannot load {}", node.id, path.display());
            }
            self.images.insert(path.clone(), loaded);
        }
        let Some(Some(img)) = self.images.get(&path) else { return };
        let (iw, ih) = (img.width() as f32, img.height() as f32);
        if iw < 1.0 || ih < 1.0 || node.width < 1.0 || node.height < 1.0 {
            return;
        }
        let sx = node.width * scale / iw;
        let sy = node.height * scale / ih;
        let paint = PixmapPaint {
            opacity,
            blend_mode: tiny_skia::BlendMode::SourceOver,
            quality: tiny_skia::FilterQuality::Bilinear,
        };
        let tt = t.pre_translate(node.x, node.y).pre_scale(sx, sy);
        pixmap.draw_pixmap(0, 0, img.as_ref(), &paint, tt, None);
    }
}

/// Polygon path from node-local vertices (offset by the node position).
fn polygon_path(node: &ResolvedNode) -> Option<tiny_skia::Path> {
    polygon_path_offset(node, 0.0, 0.0)
}

/// Polygon path offset by (dx, dy) in logical space.
fn polygon_path_offset(node: &ResolvedNode, dx: f32, dy: f32) -> Option<tiny_skia::Path> {
    if node.points.len() < 3 {
        return None;
    }
    let mut pb = PathBuilder::new();
    let (x0, y0) = node.points[0];
    pb.move_to(node.x + x0 + dx, node.y + y0 + dy);
    for (px, py) in &node.points[1..] {
        pb.line_to(node.x + px + dx, node.y + py + dy);
    }
    pb.close();
    pb.finish()
}

fn fill_rect(pixmap: &mut Pixmap, node: &ResolvedNode, t: Transform, r: u8, g: u8, b: u8, a: u8) {
    if a == 0 {
        return;
    }
    let mut paint = Paint::default();
    paint.set_color_rgba8(r, g, b, a);
    paint.anti_alias = true;
    let rect = node.rect();
    if node.radius > 0.5 {
        let path = rounded_rect_path(rect, node.radius);
        pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, t, None);
    } else {
        pixmap.fill_rect(rect, &paint, t, None);
    }
}

fn fill_shape(pixmap: &mut Pixmap, path: &tiny_skia::Path, t: Transform, r: u8, g: u8, b: u8, a: u8) {
    if a == 0 {
        return;
    }
    let mut paint = Paint::default();
    paint.set_color_rgba8(r, g, b, a);
    paint.anti_alias = true;
    pixmap.fill_path(path, &paint, tiny_skia::FillRule::Winding, t, None);
}

fn rounded_rect_path(rect: Rect, radius: f32) -> tiny_skia::Path {
    let r = radius.min(rect.width() / 2.0).min(rect.height() / 2.0);
    if r <= 0.0 {
        return PathBuilder::from_rect(rect);
    }
    let mut pb = PathBuilder::new();
    let (x, y, w, h) = (rect.x(), rect.y(), rect.width(), rect.height());
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish().unwrap()
}

fn parse_shadow(s: &str) -> Option<(f32, f32, f32, u8, u8, u8, u8)> {
    // formats: "0 12 32 rgba(0,0,0,0.35)" or "0 8 24 #00000040"
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 4 {
        return None;
    }
    let sx = parts[0].parse::<f32>().ok()?;
    let sy = parts[1].parse::<f32>().ok()?;
    let blur = parts[2].parse::<f32>().ok()?;
    let (r, g, b, a) = parse_color_str(&parts[3..].join(" "))?;
    Some((sx, sy, blur, r, g, b, a))
}

fn parse_border(s: &str) -> Option<(f32, u8, u8, u8, u8)> {
    // "1px rgba(255,255,255,0.08)" → width + color; bare color → width 1
    let mut width = 1.0;
    let mut color_part = s.trim();
    if let Some(idx) = s.find("px") {
        let w_str = s[..idx].trim().split_whitespace().last().unwrap_or("1");
        width = w_str.parse::<f32>().unwrap_or(1.0);
        color_part = s[idx + 2..].trim();
        if color_part.is_empty() {
            color_part = "rgba(255,255,255,0.08)";
        }
    } else if !(s.starts_with("rgba") || s.starts_with("rgb") || s.starts_with('#')) {
        return None;
    }
    let (r, g, b, a) = parse_color_str(color_part)?;
    Some((width, r, g, b, a))
}

/// Parse `linear-gradient(<angle>deg, color pos%, color pos%, ...)` with CSS
/// angle semantics (0° = to top, 90° = to right, 180° = to bottom).
fn parse_linear_gradient(s: &str, node: &ResolvedNode, opacity: f32) -> Option<tiny_skia::Shader<'static>> {
    let inner = s.split('(').nth(1)?.split(')').next()?;
    let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();
    if parts.len() < 2 {
        return None;
    }
    let mut angle: f32 = 180.0;
    let mut colors: Vec<(SkColor, f32)> = Vec::new();
    let mut explicit_positions = true;
    for (i, p) in parts.iter().enumerate() {
        let p = p.trim();
        if let Some(deg) = p.strip_suffix("deg") {
            angle = deg.trim().parse::<f32>().unwrap_or(180.0);
            continue;
        }
        // "color pos%" or "color"
        let (col_str, pos) = match p.rsplit_once(char::is_whitespace) {
            Some((c, pct)) if pct.ends_with('%') => {
                (c.trim(), pct[..pct.len() - 1].trim().parse::<f32>().unwrap_or(0.0) / 100.0)
            }
            _ => {
                explicit_positions = false;
                (p, i as f32 / (parts.len() - 1).max(1) as f32)
            }
        };
        if let Some((r, g, b, a)) = parse_color_str(col_str) {
            colors.push((SkColor::from_rgba8(r, g, b, (a as f32 * opacity) as u8), pos));
        }
    }
    if colors.len() < 2 {
        return None;
    }
    if !explicit_positions {
        // distribute evenly if author omitted percentages
        let n = colors.len();
        for (i, c) in colors.iter_mut().enumerate() {
            c.1 = i as f32 / (n - 1).max(1) as f32;
        }
    }
    colors.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

    let stops: Vec<GradientStop> = colors.into_iter().map(|(c, p)| GradientStop::new(p.clamp(0.0, 1.0), c)).collect();

    // CSS gradient line through the node center.
    let rad = angle.to_radians();
    let (dx, dy) = (rad.sin(), -rad.cos());
    let half = (node.width * dx.abs() + node.height * dy.abs()) / 2.0;
    let cx = node.x + node.width / 2.0;
    let cy = node.y + node.height / 2.0;
    let start = tiny_skia::Point::from_xy(cx - dx * half, cy - dy * half);
    let end = tiny_skia::Point::from_xy(cx + dx * half, cy + dy * half);
    LinearGradient::new(start, end, stops, SpreadMode::Pad, Transform::identity())
}

/// Extract a solid `Color` from a resolved prop, or `None` for gradients.
pub fn solid_color(s: &str) -> Option<Color> {
    let t = s.trim();
    if t.starts_with("linear-gradient") || t.starts_with("radial-gradient") {
        return None;
    }
    parse_color_str(t)
}

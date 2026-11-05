//! Renderer — tiny-skia + cosmic-text onto a premultiplied RGBA pixmap.
//!
//! Draw order = resolved node order (z-sorted). Every node is drawn through an
//! optional transform (rotation/scale around its center). Text is rasterized
//! into its own pixmap first, then composited — so transformed text works.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tiny_skia::{
    Color as SkColor, GradientStop, LinearGradient, Paint, PathBuilder, Pixmap, PixmapPaint,
    Rect, SpreadMode, Transform,
};

use super::color::{parse_color_str, Color};
use super::node::ResolvedNode;
use super::text::{TextEngine, TextStyle};

pub struct Renderer {
    pub text: TextEngine,
    /// Rasterized images keyed by `(path, raster width, raster height)`.
    /// PNGs use `(0, 0)` (native size); SVGs are rasterized at the node's
    /// physical pixel size and keyed by it, so a large SVG is not upscaled
    /// from a fixed 128 px bitmap. Cleared on theme hot reload.
    images: HashMap<(PathBuf, u32, u32), Option<Pixmap>>,
    /// Scratch surface for scissored nodes (repeater viewports).
    scratch: Option<Pixmap>,
}

/// Image cache entry cap: SVG entries are per-size, so a resizing node would
/// otherwise grow the map. Dropping all is fine — rasters re-load lazily.
const IMAGE_CACHE_MAX_ENTRIES: usize = 128;

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderer {
    pub fn new() -> Self {
        Self { text: TextEngine::new(), images: HashMap::new(), scratch: None }
    }

    /// Drop every cached raster (text pixmaps/measurements, loaded images and
    /// the scissor scratch). Called by the runtime on theme hot reload: fonts
    /// may have been added and image files may have been edited.
    pub fn clear_caches(&mut self) {
        self.text.clear_caches();
        self.images.clear();
        self.scratch = None;
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
                //
                // The whole path below is in PHYSICAL pixels: nodes and clip
                // rects are authored in logical units, but the scratch raster
                // and draw_node_at's offset are physical. Mixing the two made
                // scissored nodes vanish on HiDPI (circular theme items at
                // scale 2) because the node was drawn past the scratch edge.
                let pad = 80.0 * scale; // rotation/scale/shadow spill
                let bx = (((node.x.min(node.x + node.width)) * scale - pad).floor() as i32).max(0);
                let by = (((node.y.min(node.y + node.height)) * scale - pad).floor() as i32).max(0);
                let br = (((node.x.max(node.x + node.width)) * scale + pad).ceil() as i32).min(width as i32);
                let bb = (((node.y.max(node.y + node.height)) * scale + pad).ceil() as i32).min(height as i32);
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
                // The scratch is only as large as the node's padded bbox, and
                // the node is drawn into it at an offset — clearing and cloning
                // stay proportional to the node, never to the window (a frame
                // with ~20 clipped nodes used to memset ~40MB at 22fps).
                let (rw, rh) = ((br - bx).max(1) as u32, (bb - by).max(1) as u32);
                let mut scratch = self.scratch.take().unwrap_or_else(|| {
                    Pixmap::new(rw, rh).expect("scratch pixmap")
                });
                if scratch.width() < rw || scratch.height() < rh {
                    scratch = Pixmap::new(rw, rh).expect("scratch pixmap");
                }
                scratch.fill(SkColor::TRANSPARENT);
                self.draw_node_at(node, &mut scratch, scale, -(bx as f32), -(by as f32));
                if let Some(region) = tiny_skia::IntRect::from_xywh(
                    ix0 - bx,
                    iy0 - by,
                    (ix1 - ix0) as u32,
                    (iy1 - iy0) as u32,
                ) {
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
        self.draw_node_at(node, pixmap, scale, 0.0, 0.0);
    }

    fn draw_node_at(&mut self, node: &ResolvedNode, pixmap: &mut Pixmap, scale: f32, ox: f32, oy: f32) {
        let t = Self::node_transform_with_scale(node, scale).post_concat(Transform::from_translate(ox, oy));
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
                // Comic outline: "4px #0C0C0D"; hard sticker shadow: "3px 3px #000000".
                // Authoring units are logical px; the raster is physical, so
                // scale widths/offsets here or effects halve on HiDPI.
                let outline = node
                    .props
                    .get("outline")
                    .and_then(|s| parse_border(s))
                    .map(|(w, r, g, b, a)| (w * scale, (r, g, b, a)));
                let hard_shadow = node
                    .props
                    .get("text_shadow")
                    .and_then(|s| parse_text_shadow(s))
                    .map(|(dx, dy, r, g, b, a)| (dx * scale, dy * scale, (r, g, b, a)));
                let mut style = TextStyle::new(&family, outline, hard_shadow);
                style.scale = scale;
                style.ransom = TextEngine::ransom_of(&node.props);
                style.ransom_fonts = TextEngine::ransom_fonts_of(&node.props);
                style.nowrap = TextEngine::nowrap_of(&node.props);
                style.ransom_paper = node
                    .props
                    .get("ransom_paper")
                    .and_then(|s| parse_color_str(s.trim()))
                    .filter(|c| c.3 > 0);
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
        if path.as_os_str().is_empty() {
            return;
        }
        if !node.width.is_finite() || !node.height.is_finite() || node.width < 1.0 || node.height < 1.0 {
            return;
        }
        // SVG: rasterize at the node's physical size (and key the cache by it)
        // so large vector art stays sharp instead of being upscaled from 128.
        let is_svg = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("svg"));
        let (rw, rh) = if is_svg {
            let w = (node.width * scale).ceil().clamp(8.0, 4096.0) as u32;
            let h = (node.height * scale).ceil().clamp(8.0, 4096.0) as u32;
            (w, h)
        } else {
            (0, 0)
        };
        let key = (path.clone(), rw, rh);
        if !self.images.contains_key(&key) {
            let loaded = if is_svg {
                rasterize_svg(&path, rw, rh)
            } else {
                load_image_file(&path).or_else(|| rasterize_svg(&path, rw.max(128), rh.max(128)))
            };
            if loaded.is_none() {
                log::warn!("Image node `{}`: cannot load {}", node.id, path.display());
            }
            if self.images.len() >= IMAGE_CACHE_MAX_ENTRIES {
                self.images.clear();
            }
            self.images.insert(key.clone(), loaded);
        }
        let Some(Some(img)) = self.images.get(&key) else { return };
        let (iw, ih) = (img.width() as f32, img.height() as f32);
        if iw < 1.0 || ih < 1.0 {
            return;
        }
        // `t` already contains the buffer scale (node_transform_with_scale),
        // so the source is stretched in LOGICAL node units. Scaling by
        // node.width*scale here drew every image at scale^2 (~197 px instead
        // of 100 px for a 50 px node at 2x).
        let sx = node.width / iw;
        let sy = node.height / ih;
        let paint = PixmapPaint {
            opacity,
            blend_mode: tiny_skia::BlendMode::SourceOver,
            quality: tiny_skia::FilterQuality::Bilinear,
        };
        let tt = t.pre_translate(node.x, node.y).pre_scale(sx, sy);
        pixmap.draw_pixmap(0, 0, img.as_ref(), &paint, tt, None);
    }
}

fn load_image_file(path: &PathBuf) -> Option<Pixmap> {
    Pixmap::load_png(path).ok()
}

/// Rasterize an SVG into a `w x h` pixmap (fit, centred, aspect preserved).
fn rasterize_svg(path: &Path, w: u32, h: u32) -> Option<Pixmap> {
    let data = std::fs::read(path).ok()?;
    let tree = resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default()).ok()?;
    let mut pixmap = Pixmap::new(w.max(1), h.max(1))?;
    let sz = tree.size();
    let (sw, sh) = (sz.width(), sz.height());
    if sw < 0.5 || sh < 0.5 {
        return None;
    }
    let s = (w as f32 / sw).min(h as f32 / sh);
    let tx = (w as f32 - sw * s) * 0.5;
    let ty = (h as f32 - sh * s) * 0.5;
    let transform = Transform::from_translate(tx, ty).pre_scale(s, s);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Some(pixmap)
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

/// Parse a text hard-shadow written in authoring/logical units:
/// `"3px 3px #000000"`, `"4px 5px rgba(12,12,13,0.5)"` or `"2 2 #fff"`.
/// A numeric blur token is tolerated (`"4px 5px 2px #000"`) for symmetry with
/// the node `shadow` format; text shadows are never blurred. The old code
/// reused `parse_shadow` (which requires 4 whitespace tokens and bare numbers)
/// so the documented `"3px 3px #000000"` silently never parsed.
fn parse_text_shadow(s: &str) -> Option<(f32, f32, u8, u8, u8, u8)> {
    let tokens: Vec<&str> = s.split_whitespace().collect();
    if tokens.len() < 3 {
        return None;
    }
    let dx = parse_px_token(tokens[0])?;
    let dy = parse_px_token(tokens[1])?;
    let mut color_tokens = &tokens[2..];
    if color_tokens.len() >= 2 && parse_px_token(color_tokens[0]).is_some() {
        color_tokens = &color_tokens[1..]; // optional blur, ignored
    }
    let (r, g, b, a) = parse_color_str(&color_tokens.join(" "))?;
    Some((dx, dy, r, g, b, a))
}

/// Parse a number with an optional `px` suffix (`"4"`, `"4px"`, `"-2.5px"`).
fn parse_px_token(token: &str) -> Option<f32> {
    let token = token.trim();
    let number = token.strip_suffix("px").unwrap_or(token).trim();
    number.parse::<f32>().ok().filter(|v| v.is_finite())
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

/// Extract the arguments of a CSS function, respecting nested parentheses
/// (`linear-gradient(180deg, rgba(0,0,0,0.4) 0%, #fff 100%)`).
fn gradient_args(s: &str) -> Option<&str> {
    let open = s.find('(')?;
    let rest = &s[open + 1..];
    let mut depth = 0usize;
    for (i, ch) in rest.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                if depth == 0 {
                    return Some(&rest[..i]);
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    None
}

/// Split on commas that are not inside parentheses (colour fallbacks).
fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// Parse the angle and colour stops of a CSS `linear-gradient` (colour alpha
/// multiplied by node opacity). Positions are allowed to be omitted; interior
/// runs are interpolated between the surrounding explicit stops.
fn parse_linear_gradient_stops(s: &str, opacity: f32) -> Option<(f32, Vec<(SkColor, f32)>)> {
    let inner = gradient_args(s)?;
    let parts = split_top_level_commas(inner);
    if parts.len() < 2 {
        return None;
    }
    let mut angle: f32 = 180.0;
    let mut raw: Vec<(SkColor, Option<f32>)> = Vec::new();
    for p in parts {
        let p = p.trim();
        if p.is_empty() {
            continue;
        }
        if let Some(deg) = p.strip_suffix("deg") {
            angle = deg.trim().parse::<f32>().unwrap_or(180.0);
            continue;
        }
        // "color pos%" / "color" (colour itself may contain a comma: rgba()).
        let (col_str, pos) = match p.rsplit_once(char::is_whitespace) {
            Some((c, pct)) if pct.trim().ends_with('%') => {
                let pct = pct.trim();
                (c.trim(), pct[..pct.len() - 1].trim().parse::<f32>().ok().map(|v| v / 100.0))
            }
            _ => (p, None),
        };
        if let Some((r, g, b, a)) = parse_color_str(col_str) {
            raw.push((SkColor::from_rgba8(r, g, b, (a as f32 * opacity) as u8), pos));
        }
    }
    if raw.len() < 2 {
        return None;
    }
    // CSS stop distribution: explicit positions win; a run of stops without a
    // position is spread evenly between the previous and next explicit stop.
    let mut positions: Vec<f32> = Vec::with_capacity(raw.len());
    let mut cursor = 0.0f32;
    for (i, (_, pos)) in raw.iter().enumerate() {
        cursor = match pos {
            Some(p) => (*p).clamp(cursor, 1.0),
            None => {
                let next = raw[i..]
                    .iter()
                    .find_map(|(_, p)| *p)
                    .unwrap_or(1.0)
                    .clamp(cursor, 1.0);
                let missing = raw[i..].iter().take_while(|(_, p)| p.is_none()).count();
                cursor + (next - cursor) / (missing as f32 + 1.0)
            }
        };
        positions.push(cursor);
    }
    let stops = raw
        .into_iter()
        .zip(positions)
        .map(|((colour, _), pos)| (colour, pos.clamp(0.0, 1.0)))
        .collect();
    Some((angle, stops))
}

/// Parse `linear-gradient(<angle>deg, color pos%, color pos%, ...)` with CSS
/// angle semantics (0° = to top, 90° = to right, 180° = to bottom).
fn parse_linear_gradient(s: &str, node: &ResolvedNode, opacity: f32) -> Option<tiny_skia::Shader<'static>> {
    let (angle, colors) = parse_linear_gradient_stops(s, opacity)?;
    let stops: Vec<GradientStop> = colors.into_iter().map(|(c, p)| GradientStop::new(p, c)).collect();

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn node(id: &str, kind: &str, x: f32, y: f32, w: f32, h: f32) -> ResolvedNode {
        ResolvedNode {
            id: id.into(),
            kind: kind.into(),
            x,
            y,
            width: w,
            height: h,
            opacity: 1.0,
            z: 0,
            visible: true,
            props: HashMap::new(),
            text: None,
            color: None,
            background: None,
            radius: 0.0,
            rotation: 0.0,
            skew: 0.0,
            scale: 1.0,
            points: vec![],
            animate: vec![],
            clip: None,
            action: None,
            index: None,
        }
    }

    fn bbox_of(pm: &Pixmap, pred: impl Fn(&[u8]) -> bool) -> Option<(u32, u32, u32, u32)> {
        let (w, h) = (pm.width(), pm.height());
        let data = pm.data();
        let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0u32, 0u32);
        let mut any = false;
        for y in 0..h {
            for x in 0..w {
                let px = &data[((y * w + x) * 4) as usize..][..4];
                if pred(px) {
                    any = true;
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x);
                    y1 = y1.max(y);
                }
            }
        }
        any.then_some((x0, y0, x1, y1))
    }

    fn rgba8_string(c: SkColor) -> (u8, u8, u8, u8) {
        (
            (c.red() * 255.0).round() as u8,
            (c.green() * 255.0).round() as u8,
            (c.blue() * 255.0).round() as u8,
            (c.alpha() * 255.0).round() as u8,
        )
    }

    #[test]
    fn documented_text_shadow_format_parses() {
        assert_eq!(parse_text_shadow("3px 3px #000000"), Some((3.0, 3.0, 0, 0, 0, 255)));
        assert_eq!(
            parse_text_shadow("4px 5px rgba(12,12,13,0.5)"),
            Some((4.0, 5.0, 12, 12, 13, 128))
        );
        assert_eq!(parse_text_shadow("2 2 #fff"), Some((2.0, 2.0, 255, 255, 255, 255)));
        // optional blur token (node `shadow` symmetry) is tolerated
        assert_eq!(parse_text_shadow("4px 5px 2px #0C0C0D"), Some((4.0, 5.0, 12, 12, 13, 255)));
        assert_eq!(parse_text_shadow("3px #000"), None);
        assert_eq!(parse_text_shadow("nonsense"), None);
    }

    #[test]
    fn gradient_with_rgba_stops_parses() {
        let (angle, stops) = parse_linear_gradient_stops(
            "linear-gradient(180deg, rgba(30,30,46,0.9) 0%, #1e1e2e 100%)",
            1.0,
        )
        .expect("gradient with rgba() stops must parse");
        assert_eq!(angle, 180.0);
        assert_eq!(stops.len(), 2);
        assert_eq!(rgba8_string(stops[0].0), (30, 30, 46, 230));
        assert_eq!(rgba8_string(stops[1].0), (30, 30, 46, 255));
        assert_eq!(stops[0].1, 0.0);
        assert_eq!(stops[1].1, 1.0);
    }

    #[test]
    fn gradient_positions_distribute_between_explicit_stops() {
        let (_, stops) = parse_linear_gradient_stops(
            "linear-gradient(90deg, #000 0%, #555, #aaa, #fff 100%)",
            1.0,
        )
        .expect("gradient");
        let pos: Vec<f32> = stops.iter().map(|(_, p)| *p).collect();
        assert_eq!(pos.len(), 4);
        assert!((pos[1] - 1.0 / 3.0).abs() < 0.01, "{pos:?}");
        assert!((pos[2] - 2.0 / 3.0).abs() < 0.01, "{pos:?}");
    }

    #[test]
    fn gradient_with_rgba_stops_renders_pixels() {
        let mut r = Renderer::new();
        let mut n = node("bg", "Rectangle", 0.0, 0.0, 100.0, 100.0);
        n.props.insert(
            "background".into(),
            "linear-gradient(180deg, rgba(30,30,46,0.9) 0%, #1e1e2e 100%)".into(),
        );
        let pm = r.render(&[n], 100, 100, 1.0);
        let painted = pm.data().chunks_exact(4).filter(|px| px[3] > 0).count();
        assert!(painted > 9000, "rgba()-stop gradient painted only {painted} px");
    }

    #[test]
    fn scissored_node_renders_at_scale_1_and_2() {
        let make = || {
            let mut n = node("row", "Rectangle", 100.0, 300.0, 100.0, 40.0);
            n.clip = Some((0.0, 0.0, 640.0, 320.0));
            n.props.insert("background".into(), "#ff0000".into());
            n
        };
        // 1x: node is cut at the clip's bottom edge (physical == logical).
        let mut r = Renderer::new();
        let pm = r.render(&[make()], 640, 320, 1.0);
        let (x0, y0, x1, y1) = bbox_of(&pm, |p| p[3] > 0).expect("paints at 1x");
        assert_eq!((x0, y0), (100, 300), "1x origin");
        assert_eq!((x1, y1), (199, 319), "1x clip");
        // 2x: node physical rect = (200,600)-(400,680), clipped at y=640.
        // The old logical/physical mix drew it outside the scratch (nothing).
        let mut r2 = Renderer::new();
        let pm2 = r2.render(&[make()], 1280, 640, 2.0);
        let (x0, y0, x1, y1) = bbox_of(&pm2, |p| p[3] > 0).expect("scissored node must paint at 2x");
        assert!((198..=202).contains(&x0), "2x x0={x0}");
        assert!((398..=402).contains(&x1), "2x x1={x1}");
        assert!((598..=602).contains(&y0), "2x y0={y0}");
        assert!((y1 as i32 - 639).abs() <= 1, "2x must clip at y=640, y1={y1}");
    }

    #[test]
    fn hidpi_image_drawn_at_logical_size_not_scale_squared() {
        let path = PathBuf::from("/hiren-test/icon.png");
        let mut src = Pixmap::new(10, 10).expect("src pixmap");
        src.fill(SkColor::from_rgba8(255, 0, 0, 255));
        let mut r = Renderer::new();
        r.images.insert((path.clone(), 0, 0), Some(src));
        let mut n = node("img", "Image", 10.0, 10.0, 50.0, 50.0);
        n.props.insert("src".into(), path.to_string_lossy().into_owned());
        let pm = r.render(&[n], 200, 200, 2.0);
        let (x0, y0, x1, y1) = bbox_of(&pm, |p| p[3] > 0).expect("image paints");
        let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
        assert!(
            (95..=102).contains(&w) && (95..=102).contains(&h),
            "50 px logical image must be ~100 px physical at 2x, got {w}x{h}"
        );
        assert!((18..=22).contains(&x0) && (18..=22).contains(&y0), "origin at {x0},{y0}");
    }

    #[test]
    fn clear_caches_drops_images_and_text_rasters() {
        let path = PathBuf::from("/hiren-test/icon.png");
        let mut r = Renderer::new();
        r.images.insert((path.clone(), 0, 0), Some(Pixmap::new(4, 4).expect("pm")));
        r.scratch = Some(Pixmap::new(8, 8).expect("pm"));
        r.clear_caches();
        assert!(r.images.is_empty());
        assert!(r.scratch.is_none());
    }
}

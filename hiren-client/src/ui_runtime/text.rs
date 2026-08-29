//! Text engine — shaping, measurement and rasterization via cosmic-text.
//!
//! Cheaply clonable (internals behind `Rc`): the renderer owns one instance,
//! the binding engine measures through a clone of the same engine, so caches
//! and the loaded font database are shared. Renders text into standalone
//! pixmaps so the renderer can composite them with transforms.
//!
//! Style extras beyond plain glyphs:
//!   * `family`      — system font family ("" = default sans-serif)
//!   * `outline`     — comic-style glyph outline (ring blits under the fill)
//!   * `hard_shadow` — offset duplicate under everything (P5 sticker type)
//! Outline/shadow expand the rasterized box by a margin so nothing clips;
//! `render` returns the margin so the caller can align the composite.

use cosmic_text::{Attrs, Buffer, Family, Metrics, Shaping, SwashCache, Weight, Wrap};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use tiny_skia::{Pixmap, PremultipliedColorU8};

use super::color::Color;

/// Extra glyph styling (all optional). Widths/offsets are in the same units as
/// the size argument passed to `render` (physical pixels there).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TextStyle {
    /// Font family name; empty = default sans-serif.
    pub family: String,
    /// Comic outline: (width, color).
    pub outline: Option<(f32, Color)>,
    /// Hard offset shadow: (dx, dy, color) — no blur, sticker style.
    pub hard_shadow: Option<(f32, f32, Color)>,
}

impl TextStyle {
    /// Extra pixels added around the text box so outline/shadow never clip.
    fn margin(&self) -> f32 {
        let mut m = 0.0f32;
        if let Some((w, _)) = self.outline {
            m = m.max(w);
        }
        if let Some((dx, dy, _)) = self.hard_shadow {
            m = m.max(dx.abs()).max(dy.abs());
        }
        m.ceil() + 1.0
    }

    /// Build a style from theme props (helper for the renderer).
    pub fn new(family: &str, outline: Option<(f32, Color)>, hard_shadow: Option<(f32, f32, Color)>) -> Self {
        Self { family: family.to_string(), outline, hard_shadow }
    }
}

struct Inner {
    font_system: RefCell<Option<cosmic_text::FontSystem>>,
    swash: RefCell<Option<SwashCache>>,
    measure_cache: RefCell<HashMap<(String, u32, u16, String), f32>>,
    /// Rasterized text pixmaps: shaping + glyph blitting dominates frame time,
    /// and most text is identical across frames. Key quantizes geometry.
    pixmap_cache: RefCell<HashMap<(String, u32, u32, u32, u16, u8, [u8; 4], String, TextStyleKey), Option<Pixmap>>>,
}

/// Cacheable subset of TextStyle (f32 widths quantized).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
struct TextStyleKey {
    outline: Option<(u32, [u8; 4])>,
    shadow: Option<(i32, i32, [u8; 4])>,
}

impl TextStyleKey {
    fn of(style: &TextStyle) -> Self {
        Self {
            outline: style.outline.map(|(w, c)| ((w * 2.0).round() as u32, [c.0, c.1, c.2, c.3])),
            shadow: style
                .hard_shadow
                .map(|(dx, dy, c)| ((dx * 2.0).round() as i32, (dy * 2.0).round() as i32, [c.0, c.1, c.2, c.3])),
        }
    }
}

/// Shared text engine.
#[derive(Clone)]
pub struct TextEngine {
    inner: Rc<Inner>,
}

impl Default for TextEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl TextEngine {
    pub fn new() -> Self {
        Self {
            inner: Rc::new(Inner {
                font_system: RefCell::new(None),
                swash: RefCell::new(None),
                measure_cache: RefCell::new(HashMap::new()),
                pixmap_cache: RefCell::new(HashMap::new()),
            }),
        }
    }

    /// Drop all cached rasterized text (theme hot-reload, memory pressure).
    pub fn clear_caches(&self) {
        self.inner.measure_cache.borrow_mut().clear();
        self.inner.pixmap_cache.borrow_mut().clear();
    }

    /// Fonts load lazily: constructing FontSystem scans system font dirs.
    pub fn fonts_loaded(&self) -> bool {
        self.inner.font_system.borrow().is_some()
    }

    fn weight_from(s: &str) -> Weight {
        match s {
            "100" | "thin" => Weight::THIN,
            "200" | "extralight" => Weight::EXTRA_LIGHT,
            "300" | "light" => Weight::LIGHT,
            "400" | "normal" | "regular" => Weight::NORMAL,
            "500" | "medium" => Weight::MEDIUM,
            "600" | "semibold" => Weight::SEMIBOLD,
            "700" | "bold" => Weight::BOLD,
            "800" | "extrabold" => Weight::EXTRA_BOLD,
            "900" | "black" => Weight::BLACK,
            _ => Weight::NORMAL,
        }
    }

    pub fn weight_of(props: &std::collections::HashMap<String, String>) -> Weight {
        Self::weight_from(props.get("font_weight").map(|s| s.as_str()).unwrap_or("400"))
    }

    fn make_buffer(fs: &mut cosmic_text::FontSystem, text: &str, size: f32, weight: Weight, family: &str) -> Buffer {
        let metrics = Metrics::new(size, size * 1.35);
        let mut buffer = Buffer::new(fs, metrics);
        let attrs = if family.is_empty() {
            Attrs::new().family(Family::SansSerif)
        } else {
            Attrs::new().family(Family::Name(family))
        };
        let attrs = attrs.weight(weight);
        buffer.set_text(fs, text, &attrs, Shaping::Advanced);
        buffer
    }

    /// Measured single-line width of `text` at `size` (cached).
    pub fn measure(&self, text: &str, size: f32, weight: Weight, family: &str) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let key = (text.to_string(), (size * 4.0).round() as u32, weight.0, family.to_string());
        if let Some(w) = self.inner.measure_cache.borrow().get(&key) {
            return *w;
        }
        let mut fs_guard = self.inner.font_system.borrow_mut();
        let fs = fs_guard.get_or_insert_with(cosmic_text::FontSystem::new);
        let mut buffer = Self::make_buffer(fs, text, size, weight, family);
        buffer.shape_until_scroll(fs, false);
        let w = buffer.layout_runs().next().map(|r| r.line_w).unwrap_or(0.0);
        self.inner.measure_cache.borrow_mut().insert(key, w);
        w
    }

    /// Convenience used by the binding engine's `text_width(expr, size)`.
    pub fn measure_default_weight(&self, text: &str, size: f32) -> f32 {
        self.measure(text, size, Weight::NORMAL, "")
    }

    /// Rasterize text into a pixmap of `(w + 2m) x (h + 2m)` where `m` is the
    /// style margin (returned). Handles word wrap, alignment, vertical center.
    pub fn render(
        &self,
        text: &str,
        w: f32,
        h: f32,
        size: f32,
        weight: Weight,
        align: &str,
        color: Color,
        style: &TextStyle,
    ) -> Option<(Pixmap, f32)> {
        if text.is_empty() || w < 2.0 || h < 2.0 {
            return None;
        }
        let margin = style.margin();
        // Quantized cache key: same text + box + style → same pixmap.
        let key = (
            text.to_string(),
            (w * 2.0).round() as u32,
            (h * 2.0).round() as u32,
            (size * 4.0).round() as u32,
            weight.0,
            align.bytes().next().unwrap_or(b'l'),
            [color.0, color.1, color.2, color.3],
            style.family.clone(),
            TextStyleKey::of(style),
        );
        if let Some(hit) = self.inner.pixmap_cache.borrow().get(&key) {
            return hit.clone().map(|pm| (pm, margin));
        }

        let rendered = self.render_uncached(text, w, h, size, weight, align, color, style, margin);
        {
            let mut cache = self.inner.pixmap_cache.borrow_mut();
            if cache.len() > 512 {
                cache.clear();
            }
            cache.insert(key, rendered.clone().map(|(pm, _)| pm));
        }
        rendered.map(|(pm, _)| (pm, margin))
    }

    /// Rasterize text into a pixmap of `(w + 2m) x (h + 2m)` (the box plus the
    /// style margin on every side). Handles word wrap, horizontal alignment
    /// and vertical centering. Pass order: hard shadow → outline rings → fill,
    /// each pass overwriting pixels (sticker look, no soft blending).
    #[allow(clippy::too_many_arguments)]
    fn render_uncached(
        &self,
        text: &str,
        w: f32,
        h: f32,
        size: f32,
        weight: Weight,
        align: &str,
        color: Color,
        style: &TextStyle,
        margin: f32,
    ) -> Option<(Pixmap, f32)> {
        if text.is_empty() || w < 2.0 || h < 2.0 {
            return None;
        }
        let mut fs_guard = self.inner.font_system.borrow_mut();
        let fs = fs_guard.get_or_insert_with(cosmic_text::FontSystem::new);
        let mut buffer = Self::make_buffer(fs, text, size, weight, &style.family);
        buffer.set_size(fs, Some(w), None);
        buffer.set_wrap(fs, Wrap::Word);
        buffer.shape_until_scroll(fs, false);

        let runs: Vec<_> = buffer.layout_runs().collect();
        let total_h: f32 = runs.iter().map(|r| r.line_height).sum();
        let offset_y = ((h - total_h) / 2.0).max(0.0);

        let mut swash_guard = self.inner.swash.borrow_mut();
        let swash = swash_guard.get_or_insert_with(SwashCache::new);

        let m = margin as i32;
        let mut pm = Pixmap::new((w.ceil() as u32) + (m.max(0) as u32) * 2, (h.ceil() as u32) + (m.max(0) as u32) * 2)?;
        let (pm_w, pm_h) = (pm.width() as i32, pm.height() as i32);
        let pixels = pm.pixels_mut();

        // Blit one glyph pass at a pixel offset with a flat color.
        let mut blit_pass =
            |pixels: &mut [PremultipliedColorU8], pass_color: Color, dx: i32, dy: i32, alpha_scale: u32| {
                let color_cosmic = cosmic_text::Color::rgba(pass_color.0, pass_color.1, pass_color.2, pass_color.3);
                for run in &runs {
                    let tx = match align {
                        "center" | "middle" => (w - run.line_w) / 2.0,
                        "right" => w - run.line_w,
                        _ => 0.0,
                    };
                    let y_base = (run.line_y + offset_y) as i32 + m + dy;
                    let x_base = m + dx;
                    for glyph in run.glyphs.iter() {
                        let physical = glyph.physical((tx, 0.0), 1.0);
                        swash.with_pixels(fs, physical.cache_key, color_cosmic, |px, py, pc| {
                            let x = physical.x + x_base + px;
                            let y = y_base + physical.y + py;
                            if x < 0 || y < 0 || x >= pm_w || y >= pm_h {
                                return;
                            }
                            let alpha = ((pc.a() as u32 * color.3 as u32 * alpha_scale) / (255 * 255)) as u8;
                            if alpha == 0 {
                                return;
                            }
                            // rasterizing straight onto a transparent pixmap: premultiply once
                            let prem = PremultipliedColorU8::from_rgba(
                                ((pass_color.0 as u32 * alpha as u32) / 255) as u8,
                                ((pass_color.1 as u32 * alpha as u32) / 255) as u8,
                                ((pass_color.2 as u32 * alpha as u32) / 255) as u8,
                                alpha,
                            );
                            if let Some(dst) = pixels.get_mut((y as u32 * pm_w as u32 + x as u32) as usize) {
                                *dst = prem.unwrap_or(*dst);
                            }
                        });
                    }
                }
            };

        // 1. Hard offset shadow (sticker duplicate).
        if let Some((sdx, sdy, scolor)) = style.hard_shadow {
            blit_pass(pixels, scolor, sdx.round() as i32, sdy.round() as i32, 255);
        }
        // 2. Outline: ring blits at integer radii in 16 directions.
        if let Some((ow, ocolor)) = style.outline {
            let rings = ow.round().max(1.0) as i32;
            const DIRS: usize = 16;
            for ring in 1..=rings {
                for d in 0..DIRS {
                    let a = (d as f32) * std::f32::consts::TAU / DIRS as f32;
                    let dx = (a.cos() * ring as f32).round() as i32;
                    let dy = (a.sin() * ring as f32).round() as i32;
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    blit_pass(pixels, ocolor, dx, dy, 255);
                }
            }
        }
        // 3. Fill.
        blit_pass(pixels, color, 0, 0, 255);
        Some((pm, margin))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_changes_width() {
        let e = TextEngine::new();
        let sans = e.measure("HIREN", 34.0, Weight::BOLD, "");
        let antonio = e.measure("HIREN", 34.0, Weight::BOLD, "Antonio");
        assert!(antonio > 0.0);
        assert!((antonio - sans).abs() > 1.0, "antonio={antonio} sans={sans}");
    }

    #[test]
    fn render_with_family_and_outline() {
        let e = TextEngine::new();
        let style = TextStyle::new("Antonio", Some((3.0, (0, 0, 0, 255))), Some((3.0, 3.0, (0, 0, 0, 255))));
        let (pm, margin) = e
            .render("HIREN", 150.0, 44.0, 34.0, Weight::BOLD, "left", (255, 255, 255, 255), &style)
            .expect("render");
        assert!(margin >= 4.0);
        assert!(pm.width() > 150);
        // non-empty raster
        assert!(pm.data().chunks_exact(4).any(|px| px[3] > 0));
    }
}

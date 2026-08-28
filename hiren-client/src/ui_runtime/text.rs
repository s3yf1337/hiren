//! Text engine — shaping, measurement and rasterization via cosmic-text.
//!
//! Cheaply clonable (internals behind `Rc`): the renderer owns one instance,
//! the binding engine measures through a clone of the same engine, so caches
//! and the loaded font database are shared. Renders text into standalone
//! pixmaps so the renderer can composite them with transforms.

use cosmic_text::{Attrs, Buffer, Family, Metrics, Shaping, SwashCache, Weight, Wrap};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use tiny_skia::{Pixmap, PremultipliedColorU8};

use super::color::Color;

struct Inner {
    font_system: RefCell<Option<cosmic_text::FontSystem>>,
    swash: RefCell<Option<SwashCache>>,
    measure_cache: RefCell<HashMap<(String, u32, u16), f32>>,
    /// Rasterized text pixmaps: shaping + glyph blitting dominates frame time,
    /// and most text is identical across frames. Key quantizes geometry.
    pixmap_cache: RefCell<HashMap<(String, u32, u32, u32, u16, u8, [u8; 4]), Option<Pixmap>>>,
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

    fn make_buffer(fs: &mut cosmic_text::FontSystem, text: &str, size: f32, weight: Weight) -> Buffer {
        let metrics = Metrics::new(size, size * 1.35);
        let mut buffer = Buffer::new(fs, metrics);
        let attrs = Attrs::new().family(Family::SansSerif).weight(weight);
        buffer.set_text(fs, text, &attrs, Shaping::Advanced);
        buffer
    }

    /// Measured single-line width of `text` at `size` (cached).
    pub fn measure(&self, text: &str, size: f32, weight: Weight) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let key = (text.to_string(), (size * 4.0).round() as u32, weight.0);
        if let Some(w) = self.inner.measure_cache.borrow().get(&key) {
            return *w;
        }
        let mut fs_guard = self.inner.font_system.borrow_mut();
        let fs = fs_guard.get_or_insert_with(cosmic_text::FontSystem::new);
        let mut buffer = Self::make_buffer(fs, text, size, weight);
        buffer.shape_until_scroll(fs, false);
        let w = buffer.layout_runs().next().map(|r| r.line_w).unwrap_or(0.0);
        self.inner.measure_cache.borrow_mut().insert(key, w);
        w
    }

    /// Convenience used by the binding engine's `text_width(expr, size)`.
    pub fn measure_default_weight(&self, text: &str, size: f32) -> f32 {
        self.measure(text, size, Weight::NORMAL)
    }

    /// Rasterize text into a pixmap of `w x h` (the box exactly as authored).
    /// Handles word wrap, horizontal alignment and vertical centering.
    pub fn render(
        &self,
        text: &str,
        w: f32,
        h: f32,
        size: f32,
        weight: Weight,
        align: &str,
        color: Color,
    ) -> Option<Pixmap> {
        if text.is_empty() || w < 2.0 || h < 2.0 {
            return None;
        }
        // Quantized cache key: same text + box + style → same pixmap.
        let key = (
            text.to_string(),
            (w * 2.0).round() as u32,
            (h * 2.0).round() as u32,
            (size * 4.0).round() as u32,
            weight.0,
            align.bytes().next().unwrap_or(b'l'),
            [color.0, color.1, color.2, color.3],
        );
        if let Some(hit) = self.inner.pixmap_cache.borrow().get(&key) {
            return hit.clone();
        }

        let rendered = self.render_uncached(text, w, h, size, weight, align, color);
        {
            let mut cache = self.inner.pixmap_cache.borrow_mut();
            if cache.len() > 512 {
                cache.clear();
            }
            cache.insert(key, rendered.clone());
        }
        rendered
    }

    /// Rasterize text into a pixmap of `w x h` (the box exactly as authored).
    /// Handles word wrap, horizontal alignment and vertical centering.
    fn render_uncached(
        &self,
        text: &str,
        w: f32,
        h: f32,
        size: f32,
        weight: Weight,
        align: &str,
        color: Color,
    ) -> Option<Pixmap> {
        if text.is_empty() || w < 2.0 || h < 2.0 {
            return None;
        }
        let mut fs_guard = self.inner.font_system.borrow_mut();
        let fs = fs_guard.get_or_insert_with(cosmic_text::FontSystem::new);
        let mut buffer = Self::make_buffer(fs, text, size, weight);
        buffer.set_size(fs, Some(w), None);
        buffer.set_wrap(fs, Wrap::Word);
        buffer.shape_until_scroll(fs, false);

        let runs: Vec<_> = buffer.layout_runs().collect();
        let total_h: f32 = runs.iter().map(|r| r.line_height).sum();
        let offset_y = ((h - total_h) / 2.0).max(0.0);

        let mut swash_guard = self.inner.swash.borrow_mut();
        let swash = swash_guard.get_or_insert_with(SwashCache::new);
        let color_cosmic = cosmic_text::Color::rgba(color.0, color.1, color.2, color.3);

        let mut pm = Pixmap::new(w.ceil() as u32, h.ceil() as u32)?;
        let (pm_w, pm_h) = (pm.width(), pm.height());
        let pixels = pm.pixels_mut();
        for run in &runs {
            let tx = match align {
                "center" | "middle" => (w - run.line_w) / 2.0,
                "right" => w - run.line_w,
                _ => 0.0,
            };
            let y_base = (run.line_y + offset_y) as i32;
            for glyph in run.glyphs.iter() {
                let physical = glyph.physical((tx, 0.0), 1.0);
                swash.with_pixels(fs, physical.cache_key, color_cosmic, |px, py, pc| {
                    let x = physical.x + px;
                    let y = y_base + physical.y + py;
                    if x < 0 || y < 0 || x >= pm_w as i32 || y >= pm_h as i32 {
                        return;
                    }
                    let alpha = ((pc.a() as u32 * color.3 as u32) / 255) as u8;
                    if alpha == 0 {
                        return;
                    }
                    // rasterizing straight onto a transparent pixmap: premultiply once
                    let prem = PremultipliedColorU8::from_rgba(
                        ((color.0 as u32 * alpha as u32) / 255) as u8,
                        ((color.1 as u32 * alpha as u32) / 255) as u8,
                        ((color.2 as u32 * alpha as u32) / 255) as u8,
                        alpha,
                    );
                    if let Some(dst) = pixels.get_mut((y as u32 * pm_w + x as u32) as usize) {
                        *dst = prem.unwrap_or(*dst);
                    }
                });
            }
        }
        Some(pm)
    }
}

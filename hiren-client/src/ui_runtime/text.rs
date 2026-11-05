//! Text engine — shaping, measurement and rasterization via cosmic-text.
//!
//! Cheaply clonable (internals behind `Rc`): the renderer owns one instance,
//! the binding engine measures through a clone of the same engine, so caches
//! and the loaded font database are shared. Renders text into standalone
//! pixmaps so the renderer can composite them with transforms.
//!
//! Style extras beyond plain glyphs:
//!   * `family`      — system or theme-bundled font family ("" = default sans-serif)
//!   * `outline`     — comic-style glyph outline (ring blits under the fill)
//!   * `hard_shadow` — offset duplicate under everything (P5 sticker type)
//!   * `nowrap`      — single-line (no word wrap); default wraps to the node box
//!   * `ransom`      — per-letter mix of family/weight/size/rotation/case
//!   * `ransom_paper`— jagged black (or any) backing behind the word, P5 cut-out
//! Outline/shadow expand the rasterized box by a margin so nothing clips;
//! `render` returns the margin so the caller can align the composite.
//!
//! Theme-bundled fonts: `load_fonts_from_dir` reads `fonts/*.{ttf,otf,ttc}`
//! next to theme.toml into the same FontSystem (hot-reload safe).

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache, Weight, Wrap};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, PixmapPaint, PremultipliedColorU8, Transform};

use super::color::Color;

/// Smallest font size the engine will shape. cosmic-text panics on a zero line
/// height and never returns from shaping negative sizes, so every entry point
/// clamps into this range before touching the shaper.
pub const MIN_FONT_SIZE: f32 = 0.5;
/// Largest accepted font size. Bigger values are authoring mistakes and would
/// allocate absurd pixmaps; they are clamped rather than honoured.
pub const MAX_FONT_SIZE: f32 = 4096.0;

/// Validate/clamp a requested font size. `None` means the value cannot be
/// rendered at all (NaN/inf); finite values are clamped into
/// `MIN_FONT_SIZE..=MAX_FONT_SIZE` so theme/binding data can never panic or
/// hang cosmic-text (`font_size = "0"` used to abort the whole launcher).
pub fn sanitize_font_size(size: f32) -> Option<f32> {
    if !size.is_finite() {
        return None;
    }
    Some(size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE))
}

/// Extra glyph styling (all optional). Widths/offsets are in the same units as
/// the size argument passed to `render` (physical pixels there).
#[derive(Debug, Clone, PartialEq)]
pub struct TextStyle {
    /// Font family name; empty = default sans-serif.
    pub family: String,
    /// Comic outline: (width, color).
    pub outline: Option<(f32, Color)>,
    /// Hard offset shadow: (dx, dy, color) — no blur, sticker style.
    pub hard_shadow: Option<(f32, f32, Color)>,
    /// Per-letter mixed type (Persona 5 ransom / cut-paper).
    pub ransom: bool,
    /// Families to cycle for ransom; empty = fall back to `family` / default set.
    pub ransom_fonts: Vec<String>,
    /// Opaque paper scrap behind each ransom letter. None = ink only.
    pub ransom_paper: Option<Color>,
    /// Single-line layout (`wrap = "none"`). Default is word wrap to the box.
    pub nowrap: bool,
    /// Physical/logical pixel ratio this text is rasterized for (HiDPI buffer
    /// scale). Sizes/offsets passed to `render` are already physical; this
    /// only scales the engine's own logical constants (ransom paper padding,
    /// serif margin fallback) so effects keep their authored visual weight.
    pub scale: f32,
}

impl Default for TextStyle {
    fn default() -> Self {
        Self {
            family: String::new(),
            outline: None,
            hard_shadow: None,
            ransom: false,
            ransom_fonts: Vec::new(),
            ransom_paper: None,
            nowrap: false,
            scale: 1.0,
        }
    }
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

    /// Buffer scale, clamped to something sane (0 would zero every constant).
    fn screen_scale(&self) -> f32 {
        if self.scale.is_finite() && self.scale >= 0.25 {
            self.scale.clamp(0.25, 8.0)
        } else {
            1.0
        }
    }

    /// Build a style from theme props (helper for the renderer).
    pub fn new(family: &str, outline: Option<(f32, Color)>, hard_shadow: Option<(f32, f32, Color)>) -> Self {
        Self {
            family: family.to_string(),
            outline,
            hard_shadow,
            ransom: false,
            ransom_fonts: Vec::new(),
            ransom_paper: None,
            nowrap: false,
            scale: 1.0,
        }
    }
}

struct Inner {
    font_system: RefCell<Option<FontSystem>>,
    swash: RefCell<Option<SwashCache>>,
    measure_cache: RefCell<MeasureCache>,
    /// Rasterized text pixmaps: shaping + glyph blitting dominates frame time,
    /// and most text is identical across frames.
    pixmap_cache: RefCell<PixmapCache>,
    /// Shaped ransom plans (per-letter mix is deterministic for a given
    /// text/size/font list, so re-planning every frame in `text_width` was
    /// pure waste).
    ransom_cache: RefCell<HashMap<RansomCacheKey, Rc<Vec<RansomGlyph>>>>,
    /// (requested family, requested weight) → (concrete family to use, weight
    /// that family actually ships). See `resolve_face`.
    face_cache: RefCell<HashMap<FaceCacheKey, FaceResolution>>,
    loaded_fonts: RefCell<HashSet<PathBuf>>,
}

/// Shaped ransom plans: (text, quantized size, joined font list).
type RansomCacheKey = (String, u32, String);
/// Resolved faces: (requested family, requested weight).
type FaceCacheKey = (String, u16);
/// (concrete family to shape with, weight that family ships).
type FaceResolution = (Option<String>, u16);

/// Key of the measurement cache: (text, quantized size, weight, family).
type MeasureKey = (String, u32, u16, String);

/// Bounded measurement cache. One entry per distinct control/query string;
/// a long-lived typing session would otherwise grow it without limit.
#[derive(Default)]
struct MeasureCache {
    map: HashMap<MeasureKey, f32>,
}

const MEASURE_CACHE_MAX_ENTRIES: usize = 4096;

impl MeasureCache {
    fn get(&self, key: &MeasureKey) -> Option<f32> {
        self.map.get(key).copied()
    }

    fn insert(&mut self, key: MeasureKey, value: f32) {
        if self.map.len() >= MEASURE_CACHE_MAX_ENTRIES {
            self.map.clear();
        }
        self.map.insert(key, value);
    }

    fn clear(&mut self) {
        self.map.clear();
    }
}

/// Key of the raster cache: (text, integer box w, integer box h, quantized
/// size, weight, alignment tag, colour, family, style bits). Box dimensions
/// are keyed as the integer pixel size the pixmap is actually allocated at,
/// so a fractional spring width can never hit a raster of a different width.
type PixmapKey = (String, u32, u32, u32, u16, u8, [u8; 4], String, TextStyleKey);

/// One cached raster. `None` records "this text cannot render" (e.g. empty
/// after shaping) so repeated frames do not re-shape it.
struct PixmapCacheEntry {
    value: Option<(Pixmap, f32)>,
    bytes: usize,
}

/// Byte-bounded FIFO raster cache. Entry count alone allowed ~240 MB of
/// display-size rasters at 2x; the budget below keeps the steady state small
/// (~24 MB) and evicts whole rasters instead of clearing everything at once.
#[derive(Default)]
struct PixmapCache {
    map: HashMap<PixmapKey, PixmapCacheEntry>,
    order: std::collections::VecDeque<PixmapKey>,
    bytes: usize,
}

const PIXMAP_CACHE_MAX_BYTES: usize = 24 * 1024 * 1024;
/// Single rasters above this are not worth caching (they are one-off display
/// frames); they are still rendered.
const PIXMAP_CACHE_MAX_ENTRY_BYTES: usize = 2 * 1024 * 1024;

impl PixmapCache {
    fn get(&self, key: &PixmapKey) -> Option<&Option<(Pixmap, f32)>> {
        self.map.get(key).map(|entry| &entry.value)
    }

    fn insert(&mut self, key: PixmapKey, value: Option<(Pixmap, f32)>) {
        let bytes = value.as_ref().map_or(0, |(pm, _)| pm.data().len());
        if let Some(old) = self.map.remove(&key) {
            self.bytes = self.bytes.saturating_sub(old.bytes);
        }
        if bytes > PIXMAP_CACHE_MAX_ENTRY_BYTES {
            return;
        }
        while self.bytes + bytes > PIXMAP_CACHE_MAX_BYTES {
            let Some(oldest) = self.order.pop_front() else {
                self.map.clear();
                self.bytes = 0;
                break;
            };
            if let Some(entry) = self.map.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(entry.bytes);
            }
        }
        self.map.insert(key.clone(), PixmapCacheEntry { value, bytes });
        self.order.push_back(key);
        self.bytes += bytes;
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
        self.bytes = 0;
    }
}

/// Cacheable subset of TextStyle (f32 widths quantized).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
struct TextStyleKey {
    outline: Option<(u32, [u8; 4])>,
    shadow: Option<(i32, i32, [u8; 4])>,
    ransom: bool,
    ransom_fonts: String,
    paper: Option<[u8; 4]>,
    nowrap: bool,
    scale: u16,
}

impl TextStyleKey {
    fn of(style: &TextStyle) -> Self {
        Self {
            outline: style.outline.map(|(w, c)| ((w * 2.0).round() as u32, [c.0, c.1, c.2, c.3])),
            shadow: style
                .hard_shadow
                .map(|(dx, dy, c)| ((dx * 2.0).round() as i32, (dy * 2.0).round() as i32, [c.0, c.1, c.2, c.3])),
            ransom: style.ransom,
            ransom_fonts: style.ransom_fonts.join("|"),
            paper: style.ransom_paper.map(|c| [c.0, c.1, c.2, c.3]),
            nowrap: style.nowrap,
            scale: (style.screen_scale() * 100.0).round() as u16,
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
                measure_cache: RefCell::new(MeasureCache::default()),
                pixmap_cache: RefCell::new(PixmapCache::default()),
                ransom_cache: RefCell::new(HashMap::new()),
                face_cache: RefCell::new(HashMap::new()),
                loaded_fonts: RefCell::new(HashSet::new()),
            }),
        }
    }

    /// Drop all cached rasterized text (theme hot-reload, memory pressure).
    pub fn clear_caches(&self) {
        self.inner.measure_cache.borrow_mut().clear();
        self.inner.pixmap_cache.borrow_mut().clear();
        self.inner.ransom_cache.borrow_mut().clear();
        self.inner.face_cache.borrow_mut().clear();
    }

    /// Load `dir/fonts/*.{ttf,otf,ttc}` into the shared FontSystem. Safe to
    /// call again (already-loaded paths are skipped). Always also injects the
    /// atlus display pack from `include_bytes`, so a stale install dir without
    /// a `fonts/` folder still gets Anton / Playfair / Abril Fatface / etc.
    pub fn load_fonts_from_dir(&self, dir: &Path) {
        let mut loaded = self.inner.loaded_fonts.borrow_mut();
        let mut fs_guard = self.inner.font_system.borrow_mut();
        let fs = fs_guard.get_or_insert_with(FontSystem::new);
        let mut n = 0usize;

        let bundled = PathBuf::from("<atlus-bundled>");
        if loaded.insert(bundled) {
            n += load_bundled_atlus_fonts(fs);
        }

        if let Ok(rd) = std::fs::read_dir(dir.join("fonts")) {
            for e in rd.flatten() {
                let p = e.path();
                let ext = p
                    .extension()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_ascii_lowercase());
                if !matches!(ext.as_deref(), Some("ttf" | "otf" | "ttc")) {
                    continue;
                }
                if !loaded.insert(p.clone()) {
                    continue;
                }
                match fs.db_mut().load_font_file(&p) {
                    Ok(_) => {
                        log::info!("theme font loaded: {}", p.display());
                        n += 1;
                    }
                    Err(err) => {
                        loaded.remove(&p);
                        log::warn!("theme font {}: {err}", p.display());
                    }
                }
            }
        }
        drop(fs_guard);
        drop(loaded);
        if n > 0 {
            self.clear_caches();
        }
    }

    /// Fonts load lazily: constructing FontSystem scans system font dirs.
    pub fn fonts_loaded(&self) -> bool {
        self.inner.font_system.borrow().is_some()
    }

    /// Weight token accepted by themes (`font_weight`, 4th `text_width` arg).
    pub fn weight_of_str(s: &str) -> Weight {
        Self::weight_from(s)
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

    pub fn ransom_of(props: &std::collections::HashMap<String, String>) -> bool {
        matches!(
            props.get("ransom").map(|s| s.trim()),
            Some("true" | "1" | "yes")
        )
    }

    pub fn ransom_fonts_of(props: &std::collections::HashMap<String, String>) -> Vec<String> {
        props
            .get("ransom_fonts")
            .map(|s| parse_font_list(s))
            .unwrap_or_default()
    }

    pub fn nowrap_of(props: &std::collections::HashMap<String, String>) -> bool {
        matches!(
            props.get("wrap").map(|s| s.trim()),
            Some("none" | "nowrap" | "clip")
        )
    }

    /// Resolve a requested `(family, weight)` to a face the loaded font
    /// database actually ships. cosmic-text's fallback only accepts an *exact*
    /// weight within a family; any other value silently abandons the family
    /// for an unrelated fallback (default theme w500 → Noto Sans, atlus
    /// Playfair w400 → Noto Sans). Snapping to the nearest available weight
    /// keeps the requested typeface; an empty family resolves the system
    /// sans-serif generic to its concrete family first.
    fn resolve_face_cached(&self, fs: &FontSystem, family: &str, weight: Weight) -> (Option<String>, Weight) {
        let key = (family.to_string(), weight.0);
        if let Some((name, w)) = self.inner.face_cache.borrow().get(&key) {
            return (name.clone(), Weight(*w));
        }
        let resolved = resolve_face(fs, family, weight);
        let mut cache = self.inner.face_cache.borrow_mut();
        if cache.len() >= 4096 {
            cache.clear();
        }
        cache.insert(key, (resolved.0.clone(), resolved.1.0));
        resolved
    }

    fn make_buffer(
        &self,
        fs: &mut FontSystem,
        text: &str,
        size: f32,
        weight: Weight,
        family: &str,
        nowrap: bool,
    ) -> Buffer {
        // Defence in depth: public entry points sanitize, but cosmic-text
        // panics on a zero line height, so never hand it an unsanitized size.
        let size = sanitize_font_size(size).unwrap_or(MIN_FONT_SIZE);
        // Wrapped UI copy uses 1.35 so two lines fit a theme box. Nowrap
        // display (stacked H1s) authors a box ≈ em-square; 1.35 then clips
        // the bottom ~20% of the glyph — didone hairlines vanish first.
        let line_height = if nowrap { size } else { size * 1.35 };
        let metrics = Metrics::new(size, line_height);
        let mut buffer = Buffer::new(fs, metrics);
        let (resolved_family, weight) = self.resolve_face_cached(fs, family, weight);
        let attrs = match &resolved_family {
            Some(name) => Attrs::new().family(Family::Name(name)),
            None if family.is_empty() => Attrs::new().family(Family::SansSerif),
            None => Attrs::new().family(Family::Name(family)),
        };
        let attrs = attrs.weight(weight);
        buffer.set_text(fs, text, &attrs, Shaping::Advanced);
        buffer
    }

    /// Measured single-line width of `text` at `size` (cached).
    /// A comma-separated `family` is treated as a ransom font list.
    pub fn measure(&self, text: &str, size: f32, weight: Weight, family: &str) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let Some(size) = sanitize_font_size(size) else {
            return 0.0;
        };
        if family.contains(',') {
            return self.measure_ransom(text, size, &parse_font_list(family));
        }
        let key = (text.to_string(), (size * 4.0).round() as u32, weight.0, family.to_string());
        if let Some(w) = self.inner.measure_cache.borrow().get(&key) {
            return w;
        }
        let mut fs_guard = self.inner.font_system.borrow_mut();
        let fs = fs_guard.get_or_insert_with(FontSystem::new);
        let mut buffer = self.make_buffer(fs, text, size, weight, family, false);
        buffer.set_wrap(fs, Wrap::None);
        buffer.set_size(fs, None, None);
        buffer.shape_until_scroll(fs, false);
        let w = buffer.layout_runs().next().map(|r| r.line_w).unwrap_or(0.0);
        drop(fs_guard);
        self.inner.measure_cache.borrow_mut().insert(key, w);
        w
    }

    fn measure_ransom(&self, text: &str, size: f32, fonts: &[String]) -> f32 {
        self.ransom_plan(text, size, fonts).iter().map(|g| g.advance).sum()
    }

    /// Cached ransom plan (deterministic per text/size/font list).
    fn ransom_plan(&self, text: &str, size: f32, fonts: &[String]) -> Rc<Vec<RansomGlyph>> {
        let key = (text.to_string(), (size * 4.0).round() as u32, fonts.join("|"));
        if let Some(hit) = self.inner.ransom_cache.borrow().get(&key) {
            return hit.clone();
        }
        let plan = Rc::new(self.plan_ransom(text, size, fonts));
        let mut cache = self.inner.ransom_cache.borrow_mut();
        if cache.len() >= 256 {
            cache.clear();
        }
        cache.insert(key, plan.clone());
        plan
    }

    /// Convenience used by the binding engine's `text_width(expr, size)`.
    pub fn measure_default_weight(&self, text: &str, size: f32) -> f32 {
        self.measure(text, size, Weight::NORMAL, "")
    }

    /// Rasterize text into a pixmap of `(w + 2m) x (h + 2m)` where `m` is the
    /// style margin (returned). Handles word wrap, alignment, vertical center.
    #[allow(clippy::too_many_arguments)]
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
        if text.is_empty() || !w.is_finite() || !h.is_finite() || w < 2.0 || h < 2.0 || w > 65536.0 || h > 65536.0 {
            return None;
        }
        let size = sanitize_font_size(size)?;
        // Base margin from the explicit style effects. Nowrap display type can
        // need more: render_uncached derives the rest from the real shaped
        // glyph extents and returns the exact integer padding it used.
        let margin = if style.ransom {
            // Rotated per-letter scraps spill past the box.
            style.margin().max(12.0 * style.screen_scale())
        } else {
            style.margin()
        };
        // Same text + integer box + style → same pixmap. The box is keyed by
        // the integer pixel dimensions the raster is allocated at (ceil), so
        // fractional spring widths cannot collide with a differently-sized
        // raster; cache hits and misses return the identical margin.
        let key = (
            text.to_string(),
            w.ceil() as u32,
            h.ceil() as u32,
            (size * 4.0).round() as u32,
            weight.0,
            align.bytes().next().unwrap_or(b'l'),
            [color.0, color.1, color.2, color.3],
            style.family.clone(),
            TextStyleKey::of(style),
        );
        if let Some(hit) = self.inner.pixmap_cache.borrow().get(&key) {
            return hit.clone();
        }

        let rendered = if style.ransom {
            self.render_ransom(text, w, h, size, align, color, style, margin)
        } else {
            self.render_uncached(text, w, h, size, weight, align, color, style, margin)
        };
        self.inner.pixmap_cache.borrow_mut().insert(key, rendered.clone());
        rendered
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
        let size = sanitize_font_size(size)?;
        let mut fs_guard = self.inner.font_system.borrow_mut();
        let fs = fs_guard.get_or_insert_with(FontSystem::new);
        let mut buffer = self.make_buffer(fs, text, size, weight, &style.family, style.nowrap);
        buffer.set_size(fs, Some(w), None);
        buffer.set_wrap(fs, if style.nowrap { Wrap::None } else { Wrap::Word });
        buffer.shape_until_scroll(fs, false);

        let runs: Vec<_> = buffer.layout_runs().collect();
        let total_h: f32 = runs.iter().map(|r| r.line_height).sum();
        let offset_y = if style.nowrap {
            (h - total_h) / 2.0
        } else {
            ((h - total_h) / 2.0).max(0.0)
        };

        let mut swash_guard = self.inner.swash.borrow_mut();
        let swash = swash_guard.get_or_insert_with(SwashCache::new);

        // Nowrap display type is typically authored in an em-square box, but
        // glyph ink (ascent+descent, didone overshoot and serifs) is taller
        // than one em. Derive the margin from the real raster extents so a
        // 148 px display serif is never clipped — a fixed 8 px pad lost ~5%
        // of the ink at that size and got proportionally worse as size grew.
        let mut margin = margin;
        if style.nowrap {
            let needed = match ink_extent_y(fs, swash, &runs) {
                Some((ink_top, ink_bottom)) => {
                    let top_overflow = -(offset_y + ink_top);
                    let bottom_overflow = (offset_y + ink_bottom) - h;
                    top_overflow.max(bottom_overflow).max(0.0)
                }
                // No ink extents available: fall back to an em-proportional
                // pad rather than the old absolute constant.
                None => size * 0.3,
            };
            // +1 px covers integer truncation in the blitter.
            margin = margin.max(needed.ceil() + 1.0).max(4.0 * style.screen_scale());
        }

        let m = margin.ceil() as i32;
        let margin = m as f32; // canonical: padding and returned margin agree
        let mut pm = Pixmap::new(
            (w.ceil() as u32) + (m.max(0) as u32) * 2,
            (h.ceil() as u32) + (m.max(0) as u32) * 2,
        )?;
        blit_layout(&mut pm, fs, swash, &runs, w, offset_y, m, align, color, style);
        Some((pm, margin))
    }

    #[allow(clippy::too_many_arguments)]
    fn render_ransom(
        &self,
        text: &str,
        w: f32,
        h: f32,
        size: f32,
        align: &str,
        color: Color,
        style: &TextStyle,
        margin: f32,
    ) -> Option<(Pixmap, f32)> {
        let size = sanitize_font_size(size)?;
        let scale = style.screen_scale();
        let fonts = ransom_font_list(style);
        let glyphs = self.ransom_plan(text, size, &fonts);
        if glyphs.is_empty() {
            return None;
        }
        let margin = margin.max(12.0 * scale);
        let total_w: f32 = glyphs.iter().map(|g| g.advance).sum();
        let start_x = match align {
            "center" | "middle" => ((w - total_w) / 2.0).max(0.0),
            "right" => (w - total_w).max(0.0),
            _ => 0.0,
        };

        let m = margin.ceil() as i32;
        let margin = m as f32; // canonical: padding and returned margin agree
        let mut dest = Pixmap::new(
            (w.ceil() as u32) + (m.max(0) as u32) * 2,
            (h.ceil() as u32) + (m.max(0) as u32) * 2,
        )?;

        let mut fs_guard = self.inner.font_system.borrow_mut();
        let fs = fs_guard.get_or_insert_with(FontSystem::new);
        let mut swash_guard = self.inner.swash.borrow_mut();
        let swash = swash_guard.get_or_insert_with(SwashCache::new);

        let mut cursor = start_x;
        // Shared cap-band for the word. Per-letter dy is a hash, never a
        // function of string length — rotating a length-sized box was what
        // made short names sit on a different row than long ones.
        let band_h = (size * 1.35).max(8.0);
        let dest_base_y = margin + ((h - band_h) / 2.0).max(0.0);
        let mut ink_style = style.clone();
        ink_style.outline = None;
        ink_style.hard_shadow = None;

        // One torn strip per word — P5 stickers, not a barcode of scraps.
        if let Some(back) = style.ransom_paper {
            fill_word_sticker(
                &mut dest,
                &glyphs,
                start_x,
                margin,
                dest_base_y,
                band_h,
                back,
            );
        }

        for g in glyphs.iter() {
            if g.ink_w < 1.0 {
                cursor += g.advance;
                continue;
            }
            // Pad past the ink: rotation + didone serifs were clipping off
            // half the glyph, which read as "missing pixels".
            let pad = 6.0 * scale;
            let box_w = (g.ink_w + pad * 2.0).max(12.0 * scale);
            let box_h = (g.size * 1.5 + pad).max(12.0 * scale);
            let Some(mut letter) = Pixmap::new(box_w.ceil() as u32, box_h.ceil() as u32) else {
                cursor += g.advance;
                continue;
            };
            let mut buffer = self.make_buffer(fs, &g.ch, g.size, g.weight, &g.family, true);
            buffer.set_wrap(fs, Wrap::None);
            buffer.shape_until_scroll(fs, false);
            let runs: Vec<_> = buffer.layout_runs().collect();
            let line_y = runs.first().map(|r| r.line_y).unwrap_or(g.size);
            let target_baseline = box_h * 0.78;
            let glyph_y = target_baseline - line_y;
            blit_layout(
                &mut letter,
                fs,
                swash,
                &runs,
                box_w,
                glyph_y,
                0,
                "center",
                color,
                &ink_style,
            );
            let dest_x = margin + cursor;
            let max_y = ((h + margin * 2.0) - box_h).max(0.0);
            // Small letters sit on the cap baseline, not stuck to the top of
            // the band (that read as random superscripts, not cut-outs).
            let down = if g.lower { (size - g.size).max(0.0) * 0.55 } else { 0.0 };
            let dest_y = (dest_base_y + g.dy + down).clamp(0.0, max_y);
            blit_rotated(&mut dest, &letter, dest_x, dest_y, g.rot);
            cursor += g.advance;
        }
        Some((dest, margin))
    }

    fn plan_ransom(&self, text: &str, size: f32, fonts: &[String]) -> Vec<RansomGlyph> {
        let fonts = if fonts.is_empty() {
            default_ransom_fonts()
        } else {
            fonts.to_vec()
        };
        let mut out = Vec::new();
        let mut streak = 0u32;
        let mut word_start = true;
        for (i, ch) in text.chars().filter(|c| !c.is_control()).enumerate() {
            let h = ransom_hash(text, i);
            if ch.is_whitespace() {
                streak = 0;
                word_start = true;
                out.push(RansomGlyph {
                    ch: " ".into(),
                    family: String::new(),
                    weight: Weight::NORMAL,
                    size,
                    rot: 0.0,
                    dy: 0.0,
                    ink_w: 0.0,
                    advance: (size * 0.22).max(3.0),
                    hash: h,
                    lower: false,
                });
                continue;
            }
            let lc = ch.to_ascii_lowercase();
            let lower_fonts: Vec<&String> = fonts.iter().filter(|f| family_has_lowercase(f)).collect();
            // Designed mix, not a lottery: caps = condensed black, lower = heavy
            // grotesque. Only a/e/o/u flip case so the word still reads.
            let lower_candidate = matches!(lc, 'a' | 'e' | 'o' | 'u');
            let want_lower = !word_start
                && ch.is_ascii_alphabetic()
                && streak < 2
                && !lower_fonts.is_empty()
                && lower_candidate
                && (h >> 16) % 4 != 0;
            let mut lower = want_lower;
            if lower {
                streak += 1;
            } else {
                streak = 0;
            }
            let mut family = if lower {
                pick_lower_family(&lower_fonts, h, lc)
            } else {
                pick_cap_family(&fonts, h, lc, size)
            };
            let mut weight = ransom_weight(&family);
            let mut s = ch.to_string();
            if ch.is_ascii_alphabetic() {
                s = if lower {
                    s.to_lowercase()
                } else {
                    s.to_uppercase()
                };
            }
            // Same optical band. P5 varies case/shape, not 5px vs 30px.
            let scale = if lower {
                0.88 + ((h >> 5) % 5) as f32 / 100.0 // 0.88..0.92
            } else if word_start {
                1.04 + ((h >> 5) % 4) as f32 / 100.0 // 1.04..1.07
            } else {
                0.98 + ((h >> 5) % 5) as f32 / 100.0 // 0.98..1.02
            };
            let mut gsize = (size * scale).max(size * 0.88);
            let rot = 0.0;
            let dy = (((h >> 8) % 3) as i32 - 1) as f32;
            let mut ink_w = self.measure(&s, gsize, weight, &family);
            let narrow = matches!(
                s.as_str(),
                "I" | "i" | "l" | "j" | "J" | "t" | "f" | "r" | "1" | "'" | "," | "."
            );
            if !narrow && ink_w < gsize * 0.18 {
                lower = false;
                streak = 0;
                family = pick_cap_family(&fonts, h, lc, size);
                weight = ransom_weight(&family);
                s = ch.to_ascii_uppercase().to_string();
                gsize = size;
                ink_w = self.measure(&s, gsize, weight, &family);
            }
            let advance = (ink_w * 0.96 + 1.2).max(gsize * 0.28);
            out.push(RansomGlyph {
                ch: s,
                family,
                weight,
                size: gsize,
                rot,
                dy,
                ink_w,
                advance,
                hash: h,
                lower,
            });
            word_start = false;
        }
        out
    }
}

struct RansomGlyph {
    ch: String,
    family: String,
    weight: Weight,
    size: f32,
    rot: f32,
    dy: f32,
    ink_w: f32,
    advance: f32,
    hash: u32,
    lower: bool,
}

/// See [`TextEngine::resolve_face_cached`]. Returns the concrete family to
/// shape with (`None` = keep the original request) and the weight that family
/// actually has. Without this, cosmic-text swaps the whole family whenever the
/// exact requested weight is missing.
fn resolve_face(fs: &FontSystem, family: &str, weight: Weight) -> (Option<String>, Weight) {
    let name: Option<String> = if family.is_empty() {
        let generic = fs.db().family_name(&Family::SansSerif).to_string();
        (!generic.is_empty()).then_some(generic)
    } else {
        Some(family.to_string())
    };
    let Some(name) = name else {
        return (None, weight);
    };
    let mut best: Option<(u16, u16)> = None; // (weight distance, available weight)
    let mut saw_family = false;
    for face in fs.db().faces() {
        if face.style != cosmic_text::fontdb::Style::Normal {
            continue;
        }
        if !face.families.iter().any(|(f, _)| f.eq_ignore_ascii_case(&name)) {
            continue;
        }
        saw_family = true;
        let available = face.weight.0;
        if available == weight.0 {
            return (Some(name), weight);
        }
        let diff = available.abs_diff(weight.0);
        // Prefer the lighter face on a tie (CSS: target 500 → 400 before 600).
        let better = match best {
            None => true,
            Some((best_diff, best_weight)) => diff < best_diff || (diff == best_diff && available < best_weight),
        };
        if better {
            best = Some((diff, available));
        }
    }
    if !saw_family {
        return (None, weight);
    }
    match best {
        Some((_, available)) => (Some(name), Weight(available)),
        None => (None, weight),
    }
}

fn parse_font_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(|p| p.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

fn default_ransom_fonts() -> Vec<String> {
    vec![
        "Anton".into(),
        "Archivo Black".into(),
        "Titan One".into(),
    ]
}

fn ransom_font_list(style: &TextStyle) -> Vec<String> {
    if !style.ransom_fonts.is_empty() {
        style.ransom_fonts.clone()
    } else if style.family.contains(',') {
        parse_font_list(&style.family)
    } else if !style.family.is_empty() {
        let mut v = default_ransom_fonts();
        if !v.iter().any(|f| f.eq_ignore_ascii_case(&style.family)) {
            v.insert(0, style.family.clone());
        }
        v
    } else {
        default_ransom_fonts()
    }
}

fn ransom_hash(text: &str, i: usize) -> u32 {
    let mut h = 2166136261u32;
    for b in text.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(16777619);
    }
    h ^= (i as u32).wrapping_mul(0x9E3779B9);
    h.wrapping_mul(16777619)
}

fn family_has_lowercase(family: &str) -> bool {
    let f = family.to_ascii_lowercase();
    f.contains("archivo")
}

fn family_is_hairline(family: &str) -> bool {
    let f = family.to_ascii_lowercase();
    f.contains("playfair") || f.contains("abril")
}

fn pick_from(pool: &[&String], h: u32, fallback: &[String]) -> String {
    if pool.is_empty() {
        fallback[h as usize % fallback.len()].clone()
    } else {
        pool[h as usize % pool.len()].to_string()
    }
}

fn pick_cap_family(fonts: &[String], h: u32, lc: char, _size: f32) -> String {
    // Two jobs: condensed black for most caps, round ultra-black for O/C/D.
    // Eight-face lottery was the "ugly ransom" look — P5 is a system.
    let anton: Vec<&String> = fonts.iter().filter(|f| f.to_ascii_lowercase().contains("anton")).collect();
    let wide: Vec<&String> = fonts
        .iter()
        .filter(|f| {
            let x = f.to_ascii_lowercase();
            x.contains("titan") || x.contains("archivo")
        })
        .collect();
    // Condensed stems (I, L) go wide/heavy so they don't disappear at 28px.
    if matches!(lc, 'i' | 'l' | 'j' | 'f' | 't' | '1') && !wide.is_empty() {
        return pick_from(&wide, h, fonts);
    }
    if matches!(lc, 'o' | 'c' | 'd' | 'g' | 'q' | '0') && !wide.is_empty() {
        return pick_from(&wide, h, fonts);
    }
    if !anton.is_empty() {
        return pick_from(&anton, h, fonts);
    }
    let heavy: Vec<&String> = fonts
        .iter()
        .filter(|f| !family_is_hairline(f) && !f.to_ascii_lowercase().contains("oswald"))
        .collect();
    pick_from(&heavy, h, fonts)
}

fn pick_lower_family(fonts: &[&String], h: u32, _lc: char) -> String {
    if fonts.is_empty() {
        return String::new();
    }
    if let Some(arch) = fonts.iter().find(|f| f.to_ascii_lowercase().contains("archivo")) {
        return (*arch).to_string();
    }
    fonts[h as usize % fonts.len()].to_string()
}

fn ransom_weight(family: &str) -> Weight {
    let fam = family.to_ascii_lowercase();
    if fam.contains("playfair") {
        Weight::BLACK
    } else if fam.contains("oswald") {
        Weight::BOLD
    } else {
        Weight::NORMAL
    }
}

fn load_bundled_atlus_fonts(fs: &mut FontSystem) -> usize {
    // Paths relative to this source file. These families are requested by
    // `themes/atlus/theme.toml`; embedding them means the live client does
    // not depend on a fonts/ directory next to whatever theme.toml was found.
    macro_rules! pack {
        ($($file:expr),+ $(,)?) => {{
            let mut n = 0usize;
            $(
                let data: &[u8] = include_bytes!(concat!("../../themes/atlus/fonts/", $file));
                fs.db_mut().load_font_data(data.to_vec());
                n += 1;
            )+
            n
        }};
    }
    pack!(
        "Anton-Regular.ttf",
        "ArchivoBlack-Regular.ttf",
        "Oswald-Bold.ttf",
        "PlayfairDisplay-Black.ttf",
        "PassionOne-Black.ttf",
        "AlfaSlabOne-Regular.ttf",
        "AbrilFatface-Regular.ttf",
        "TitanOne-Regular.ttf",
    )
}

fn blit_rotated(dst: &mut Pixmap, src: &Pixmap, dest_x: f32, dest_y: f32, rot_deg: f32) {
    let cx = dest_x + src.width() as f32 / 2.0;
    let cy = dest_y + src.height() as f32 / 2.0;
    let t = Transform::from_translate(cx, cy)
        .pre_concat(Transform::from_rotate(rot_deg))
        .pre_concat(Transform::from_translate(
            -(src.width() as f32) / 2.0,
            -(src.height() as f32) / 2.0,
        ));
    // Nearest: bilinear on white-on-red glyphs left a smeared halo that
    // fused neighbouring letters into one blot.
    let paint = PixmapPaint {
        opacity: 1.0,
        blend_mode: tiny_skia::BlendMode::SourceOver,
        quality: tiny_skia::FilterQuality::Bilinear,
    };
    dst.draw_pixmap(0, 0, src.as_ref(), &paint, t, None);
}

fn fill_word_sticker(
    pm: &mut Pixmap,
    glyphs: &[RansomGlyph],
    start_x: f32,
    margin: f32,
    dest_base_y: f32,
    band_h: f32,
    color: Color,
) {
    if glyphs.is_empty() {
        return;
    }
    let mut tops: Vec<(f32, f32)> = Vec::new();
    let mut bots: Vec<(f32, f32)> = Vec::new();
    let mut cx = start_x;
    let y0 = dest_base_y - 4.0;
    let y1 = dest_base_y + band_h + 3.0;
    let h0 = glyphs[0].hash;
    tops.push((margin + cx - 7.0, y0 + ((h0 >> 3) % 6) as f32 - 1.0));
    bots.push((margin + cx - 4.0, y1 + ((h0 >> 9) % 5) as f32 - 2.0));
    for g in glyphs {
        let span = if g.ink_w < 1.0 {
            g.advance
        } else {
            g.advance.max(g.ink_w)
        };
        let left = margin + cx - 1.0;
        let right = margin + cx + span + 1.0;
        let yt = y0 + ((g.hash >> 4) % 4) as f32 - 1.0;
        let yb = y1 + ((g.hash >> 10) % 3) as f32 - 1.0;
        tops.push((left, yt));
        tops.push((right, yt + ((g.hash >> 6) % 3) as f32 - 1.0));
        bots.push((left, yb));
        bots.push((right, yb + ((g.hash >> 12) % 3) as f32 - 1.0));
        cx += g.advance;
    }
    let hn = glyphs.last().unwrap().hash;
    tops.push((margin + cx + 6.0, y0 + ((hn >> 2) % 5) as f32));
    bots.push((margin + cx + 8.0, y1 - 2.0 + ((hn >> 7) % 6) as f32));
    fill_cut_ribbon(pm, &tops, &bots, color);
}

fn fill_cut_ribbon(pm: &mut Pixmap, tops: &[(f32, f32)], bots: &[(f32, f32)], color: Color) {
    if tops.len() < 2 || bots.len() < 2 {
        return;
    }
    let mut pb = PathBuilder::new();
    pb.move_to(tops[0].0, tops[0].1);
    for p in &tops[1..] {
        pb.line_to(p.0, p.1);
    }
    for p in bots.iter().rev() {
        pb.line_to(p.0, p.1);
    }
    pb.close();
    let Some(path) = pb.finish() else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color_rgba8(color.0, color.1, color.2, color.3);
    paint.anti_alias = false;
    pm.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
}

/// Vertical ink extent `(top, bottom)` of the shaped runs, relative to the
/// top of the text block, using the real swash raster bounds. Used to size the
/// nowrap margin from actual glyph ink instead of a fixed pixel pad.
fn ink_extent_y(
    fs: &mut FontSystem,
    swash: &mut SwashCache,
    runs: &[cosmic_text::LayoutRun<'_>],
) -> Option<(f32, f32)> {
    let mut top = f32::INFINITY;
    let mut bottom = f32::NEG_INFINITY;
    for run in runs {
        for glyph in run.glyphs.iter() {
            if glyph.glyph_id == 0 {
                continue; // missing glyph: no raster to measure
            }
            let physical = glyph.physical((0.0, 0.0), 1.0);
            let Some(image) = swash.get_image(fs, physical.cache_key) else {
                continue;
            };
            let glyph_top = run.line_y + physical.y as f32 - image.placement.top as f32;
            let glyph_bottom = glyph_top + image.placement.height as f32;
            top = top.min(glyph_top);
            bottom = bottom.max(glyph_bottom);
        }
    }
    if top.is_finite() && bottom.is_finite() && bottom > top {
        Some((top, bottom))
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
fn blit_layout(
    pm: &mut Pixmap,
    fs: &mut FontSystem,
    swash: &mut SwashCache,
    runs: &[cosmic_text::LayoutRun<'_>],
    w: f32,
    offset_y: f32,
    m: i32,
    align: &str,
    color: Color,
    style: &TextStyle,
) {
    let (pm_w, pm_h) = (pm.width() as i32, pm.height() as i32);
    let pixels = pm.pixels_mut();

    let mut blit_pass =
        |pixels: &mut [PremultipliedColorU8], pass_color: Color, dx: i32, dy: i32| {
            let color_cosmic = cosmic_text::Color::rgba(pass_color.0, pass_color.1, pass_color.2, pass_color.3);
            for run in runs {
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
                        // Premultiplied coverage mask: glyph coverage times
                        // the pass colour's alpha. Anti-aliased edges and
                        // translucent theme text (rgba(...,0.25) watermarks,
                        // dim placeholders) must keep their partial alpha —
                        // a binary alpha=255 mask destroys AA and makes every
                        // translucent colour opaque.
                        let alpha = ((pc.a() as u32 * pass_color.3 as u32) / 255) as u8;
                        if alpha == 0 {
                            return;
                        }
                        let Some(prem) = PremultipliedColorU8::from_rgba(
                            ((pass_color.0 as u32 * alpha as u32) / 255) as u8,
                            ((pass_color.1 as u32 * alpha as u32) / 255) as u8,
                            ((pass_color.2 as u32 * alpha as u32) / 255) as u8,
                            alpha,
                        ) else {
                            return;
                        };
                        if let Some(dst) = pixels.get_mut((y as u32 * pm_w as u32 + x as u32) as usize) {
                            *dst = prem;
                        }
                    });
                }
            }
        };

    if let Some((sdx, sdy, scolor)) = style.hard_shadow {
        blit_pass(pixels, scolor, sdx.round() as i32, sdy.round() as i32);
    }
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
                blit_pass(pixels, ocolor, dx, dy);
            }
        }
    }
    blit_pass(pixels, color, 0, 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atlus_fonts_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("themes/atlus")
    }

    #[test]
    fn family_changes_width() {
        let e = TextEngine::new();
        e.load_fonts_from_dir(&atlus_fonts_dir());
        let sans = e.measure("HIREN", 34.0, Weight::BOLD, "");
        let anton = e.measure("HIREN", 34.0, Weight::BOLD, "Anton");
        let titan = e.measure("HIREN", 34.0, Weight::NORMAL, "Titan One");
        let play = e.measure("HIREN", 34.0, Weight::BLACK, "Playfair Display");
        let abril = e.measure("HIREN", 34.0, Weight::NORMAL, "Abril Fatface");
        assert!(anton > 0.0 && titan > 0.0 && play > 0.0 && abril > 0.0);
        assert!((anton - sans).abs() > 1.0, "anton={anton} sans={sans}");
        assert!((titan - anton).abs() > 1.0, "display cuts must not collapse to one face");
    }

    #[test]
    fn measure_counts_spaces_and_nowrap_keeps_a_query_on_one_line() {
        let e = TextEngine::new();
        e.load_fonts_from_dir(&atlus_fonts_dir());
        let with = e.measure("seek an", 22.0, Weight::BOLD, "Oswald");
        let without = e.measure("seekan", 22.0, Weight::BOLD, "Oswald");
        assert!(with > without + 2.0, "space must advance: with={with} without={without}");

        let wrap = TextStyle::new("Oswald", None, None);
        let mut clip = wrap.clone();
        clip.nowrap = true;
        let box_w = 72.0;
        let box_h = 26.0;
        let size = 20.0;
        let color = (0, 0, 0, 255);
        let (pm_wrap, _) = e
            .render("hello world", box_w, box_h, size, Weight::BOLD, "left", color, &wrap)
            .expect("wrap");
        let (pm_none, _) = e
            .render("hello world", box_w, box_h, size, Weight::BOLD, "left", color, &clip)
            .expect("nowrap");
        let ink = |pm: &Pixmap| pm.data().chunks_exact(4).filter(|px| px[3] > 30).count();
        assert!(
            ink(&pm_none) > ink(&pm_wrap),
            "nowrap must keep the second word in the short box: nowrap={} wrap={}",
            ink(&pm_none),
            ink(&pm_wrap)
        );
    }

    #[test]
    fn render_with_family_and_outline() {
        let e = TextEngine::new();
        e.load_fonts_from_dir(&atlus_fonts_dir());
        let style = TextStyle::new("Anton", Some((3.0, (0, 0, 0, 255))), Some((3.0, 3.0, (0, 0, 0, 255))));
        let (pm, margin) = e
            .render("HIREN", 150.0, 44.0, 34.0, Weight::BOLD, "left", (255, 255, 255, 255), &style)
            .expect("render");
        assert!(margin >= 4.0);
        assert!(pm.width() > 150);
        assert!(pm.data().chunks_exact(4).any(|px| px[3] > 0));
    }

    #[test]
    fn ransom_width_differs_from_plain() {
        let e = TextEngine::new();
        e.load_fonts_from_dir(&atlus_fonts_dir());
        let list = "Anton, Archivo Black, Titan One";
        let plain = e.measure("CALCULATOR", 36.0, Weight::BOLD, "Anton");
        let ransom = e.measure("CALCULATOR", 36.0, Weight::BOLD, list);
        assert!(ransom > 0.0);
        assert!((ransom - plain).abs() > 1.0, "plain={plain} ransom={ransom}");
        let again = e.measure("CALCULATOR", 36.0, Weight::BOLD, list);
        assert!((again - ransom).abs() < 0.1);
    }

    #[test]
    fn ransom_ink_is_vertically_centered() {
        let e = TextEngine::new();
        e.load_fonts_from_dir(&atlus_fonts_dir());
        let mut style = TextStyle::new("Anton", None, None);
        style.ransom = true;
        style.ransom_fonts = default_ransom_fonts();
        style.ransom_paper = Some((0, 0, 0, 255));
        let h = 88.0;
        let (pm, _) = e
            .render("COMMAND", 420.0, h, 52.0, Weight::BOLD, "left", (0, 0, 0, 255), &style)
            .expect("ransom render");
        let w = pm.width();
        let data = pm.data();
        let mut first = None;
        let mut last = 0u32;
        for y in 0..pm.height() {
            let has = (0..w).any(|x| data[((y * w + x) * 4 + 3) as usize] > 30);
            if has {
                if first.is_none() {
                    first = Some(y);
                }
                last = y;
            }
        }
        let first = first.expect("ink");
        let top = first;
        let bot = pm.height() - 1 - last;
        assert!(
            (top as i32 - bot as i32).abs() < 18,
            "ink not centered in ransom pixmap: top_pad={top} bot_pad={bot} h={} last={last}",
            pm.height()
        );
    }

    fn first_ink_row(pm: &Pixmap) -> u32 {
        let w = pm.width();
        let data = pm.data();
        for y in 0..pm.height() {
            if (0..w).any(|x| data[((y * w + x) * 4 + 3) as usize] > 30) {
                return y;
            }
        }
        pm.height()
    }

    #[test]
    fn ransom_baseline_does_not_follow_string_length() {
        let e = TextEngine::new();
        e.load_fonts_from_dir(&atlus_fonts_dir());
        let mut style = TextStyle::new("Anton", None, None);
        style.ransom = true;
        style.ransom_fonts = default_ransom_fonts();
        style.ransom_paper = Some((0, 0, 0, 255));
        let h = 48.0;
        let short = e
            .render("HI", 480.0, h, 22.0, Weight::NORMAL, "left", (16, 16, 16, 255), &style)
            .expect("short")
            .0;
        let long = e
            .render(
                "WALLPAPER ENGINE",
                480.0,
                h,
                22.0,
                Weight::NORMAL,
                "left",
                (16, 16, 16, 255),
                &style,
            )
            .expect("long")
            .0;
        let a = first_ink_row(&short);
        let b = first_ink_row(&long);
        assert!(
            (a as i32 - b as i32).abs() <= 8,
            "short first ink row {a} vs long {b} — Y must not depend on length"
        );
    }

    #[test]
    fn ransom_no_micro_glyphs() {
        let e = TextEngine::new();
        e.load_fonts_from_dir(&atlus_fonts_dir());
        let fonts = default_ransom_fonts();
        let glyphs = e.plan_ransom("CONFIDANT FIREFOX WALLPAPER ENGINE", 24.0, &fonts);
        for g in &glyphs {
            if g.ink_w < 1.0 {
                continue;
            }
            let f = g.family.to_ascii_lowercase();
            assert!(
                !f.contains("abril") && !f.contains("playfair"),
                "hairline face {} on list-sized {:?}",
                g.family,
                g.ch
            );
            assert!(
                g.size >= 24.0 * 0.88 - 0.05,
                "micro letter {:?} at {}px (floor is {:.1})",
                g.ch,
                g.size,
                24.0 * 0.88
            );
            let narrow = matches!(
                g.ch.as_str(),
                "I" | "i" | "l" | "j" | "J" | "t" | "f" | "r" | "1" | "'" | "," | "."
            );
            if !narrow {
                assert!(
                    g.ink_w >= 8.0,
                    "speck glyph {:?} family={} ink_w={:.1} size={:.1}",
                    g.ch,
                    g.family,
                    g.ink_w,
                    g.size
                );
            }
            if g.lower {
                let f = g.family.to_ascii_lowercase();
                assert!(
                    family_has_lowercase(&g.family),
                    "lowercase {:?} on caps-only face {}",
                    g.ch,
                    g.family
                );
                assert!(
                    f.contains("archivo"),
                    "lowercase should use Archivo Black, got {}",
                    g.family
                );
            }
        }
    }

    #[test]
    fn ransom_mixes_case_like_p5() {
        let e = TextEngine::new();
        e.load_fonts_from_dir(&atlus_fonts_dir());
        let fonts = default_ransom_fonts();
        let stats = e.plan_ransom("STATS", 28.0, &fonts);
        assert_eq!(stats[0].ch, "S");
        assert!(!stats[0].lower, "first letter stays a large cap");
        let has_lower = stats.iter().any(|g| g.lower);
        let has_cap_after = stats.iter().skip(1).any(|g| !g.lower);
        assert!(has_lower, "STATS must mix minuscule like the CAMP reference");
        assert!(has_cap_after, "STATS must keep some interior caps");
        let families: std::collections::HashSet<_> = stats.iter().map(|g| g.family.as_str()).collect();
        assert!(families.len() >= 2, "one word must use more than one face, got {families:?}");

        let conf = e.plan_ransom("CONFIDANT", 28.0, &fonts);
        assert_eq!(conf[0].ch, "C");
        assert!(conf.iter().any(|g| g.lower && matches!(g.ch.as_str(), "a" | "o" | "e")));
        let lowers: Vec<_> = conf.iter().filter(|g| g.lower).map(|g| g.ch.as_str()).collect();
        assert!(
            lowers.iter().all(|c| family_has_lowercase(
                &conf.iter().find(|g| g.ch == *c && g.lower).unwrap().family
            )),
            "minuscule only on Archivo"
        );
    }

    #[test]
    fn ransom_rendered_letters_are_chunky() {
        let e = TextEngine::new();
        e.load_fonts_from_dir(&atlus_fonts_dir());
        let mut style = TextStyle::new("Anton", None, None);
        style.ransom = true;
        style.ransom_fonts = default_ransom_fonts();
        style.ransom_paper = Some((0, 0, 0, 255));
        let (pm, _) = e
            .render(
                "VISUAL STUDIO CODE",
                520.0,
                48.0,
                28.0,
                Weight::BOLD,
                "left",
                (255, 255, 255, 255),
                &style,
            )
            .expect("render");
        let w = pm.width();
        let h = pm.height();
        let data = pm.data();
        // White ink (not black paper): count columns that have a tall enough stem.
        let mut tall_cols = 0u32;
        for x in 0..w {
            let mut run = 0u32;
            let mut best = 0u32;
            for y in 0..h {
                let i = ((y * w + x) * 4) as usize;
                let (r, a) = (data[i], data[i + 3]);
                let white = a > 200 && r > 180;
                if white {
                    run += 1;
                    best = best.max(run);
                } else {
                    run = 0;
                }
            }
            if best >= 12 {
                tall_cols += 1;
            }
        }
        assert!(
            tall_cols >= 40,
            "too few readable stems ({tall_cols} cols ≥12px) — letters are specks or hollowed out"
        );
    }

    #[test]
    fn ransom_render_is_opaque() {
        let e = TextEngine::new();
        e.load_fonts_from_dir(&atlus_fonts_dir());
        let mut style = TextStyle::new("Anton", Some((3.0, (0, 0, 0, 255))), Some((3.0, 3.0, (0, 0, 0, 255))));
        style.ransom = true;
        style.ransom_fonts = default_ransom_fonts();
        let (pm, _) = e
            .render("SKILL", 220.0, 56.0, 36.0, Weight::BOLD, "left", (255, 255, 255, 255), &style)
            .expect("ransom render");
        assert!(pm.data().chunks_exact(4).any(|px| px[3] > 0));
    }

    #[test]
    fn load_fonts_from_missing_dir_still_gets_bundled_faces() {
        let e = TextEngine::new();
        e.load_fonts_from_dir(std::path::Path::new("/tmp/hiren-no-such-theme"));
        assert!(e.fonts_loaded());
        let sans = e.measure("HIREN", 34.0, Weight::NORMAL, "");
        let titan = e.measure("HIREN", 34.0, Weight::NORMAL, "Titan One");
        let abril = e.measure("HIREN", 34.0, Weight::NORMAL, "Abril Fatface");
        assert!(
            (titan - sans).abs() > 1.0,
            "bundled Titan One must resolve without a fonts/ dir: sans={sans} titan={titan}"
        );
        assert!((abril - sans).abs() > 1.0, "bundled Abril Fatface missing: sans={sans} abril={abril}");
    }

    #[test]
    fn zero_negative_and_non_finite_font_sizes_are_safe() {
        let e = TextEngine::new();
        // Regression: `font_size = "0"` used to panic inside cosmic-text
        // (`line height cannot be 0`) and negative sizes wedged shaping.
        let at_min = e.measure("x", MIN_FONT_SIZE, Weight::NORMAL, "");
        assert!(at_min > 0.0);
        assert_eq!(e.measure("x", 0.0, Weight::NORMAL, ""), at_min);
        assert_eq!(e.measure("x", -8.0, Weight::NORMAL, ""), at_min);
        assert_eq!(e.measure("x", f32::NAN, Weight::NORMAL, ""), 0.0);
        assert_eq!(e.measure("x", f32::INFINITY, Weight::NORMAL, ""), 0.0);
        let style = TextStyle::new("", None, None);
        // Finite out-of-range sizes clamp and rasterize; non-finite values
        // have no meaningful size and are rejected.
        for bad in [0.0f32, -8.0] {
            assert!(
                e.render("x", 100.0, 20.0, bad, Weight::NORMAL, "left", (255, 255, 255, 255), &style)
                    .is_some(),
                "font_size {bad} must clamp, not panic"
            );
        }
        for bad in [f32::NAN, f32::INFINITY] {
            assert!(
                e.render("x", 100.0, 20.0, bad, Weight::NORMAL, "left", (255, 255, 255, 255), &style)
                    .is_none(),
                "font_size {bad} must be rejected before shaping"
            );
        }
    }

    #[test]
    fn out_of_range_font_sizes_are_clamped() {
        let e = TextEngine::new();
        e.load_fonts_from_dir(&atlus_fonts_dir());
        let at_min = e.measure("HIREN", MIN_FONT_SIZE, Weight::NORMAL, "Anton");
        let clamped_min = e.measure("HIREN", 0.01, Weight::NORMAL, "Anton");
        assert!(at_min > 0.0);
        assert!((clamped_min - at_min).abs() < 0.01, "0.01 must clamp to {MIN_FONT_SIZE}: {clamped_min} vs {at_min}");
        let at_max = e.measure("HIREN", MAX_FONT_SIZE, Weight::NORMAL, "Anton");
        let clamped_max = e.measure("HIREN", 1.0e9, Weight::NORMAL, "Anton");
        assert!((clamped_max - at_max).abs() < 0.01, "1e9 must clamp to {MAX_FONT_SIZE}: {clamped_max} vs {at_max}");
    }

    #[test]
    fn sanitize_font_size_bounds() {
        assert_eq!(sanitize_font_size(16.0), Some(16.0));
        assert_eq!(sanitize_font_size(MIN_FONT_SIZE), Some(MIN_FONT_SIZE));
        assert_eq!(sanitize_font_size(MAX_FONT_SIZE), Some(MAX_FONT_SIZE));
        assert_eq!(sanitize_font_size(0.0), Some(MIN_FONT_SIZE));
        assert_eq!(sanitize_font_size(-3.0), Some(MIN_FONT_SIZE));
        assert_eq!(sanitize_font_size(1.0e9), Some(MAX_FONT_SIZE));
        assert_eq!(sanitize_font_size(f32::NAN), None);
        assert_eq!(sanitize_font_size(f32::NEG_INFINITY), None);
    }

    fn alpha_histogram(pm: &Pixmap) -> (usize, usize, u8) {
        let mut partial = 0usize;
        let mut opaque = 0usize;
        let mut max = 0u8;
        for px in pm.data().chunks_exact(4) {
            let a = px[3];
            max = max.max(a);
            if a > 0 && a < 255 {
                partial += 1;
            } else if a == 255 {
                opaque += 1;
            }
        }
        (partial, opaque, max)
    }

    fn ink_count(pm: &Pixmap) -> usize {
        pm.data().chunks_exact(4).filter(|px| px[3] > 30).count()
    }

    fn ink_bbox(pm: &Pixmap) -> Option<(u32, u32, u32, u32)> {
        let (w, h) = (pm.width(), pm.height());
        let data = pm.data();
        let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
        for y in 0..h {
            for x in 0..w {
                if data[((y * w + x) * 4 + 3) as usize] > 30 {
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x);
                    y1 = y1.max(y);
                }
            }
        }
        (x1 >= x0 && y1 >= y0).then_some((x0, y0, x1, y1))
    }

    #[test]
    fn text_keeps_antialiasing_and_colour_alpha() {
        let e = TextEngine::new();
        e.load_fonts_from_dir(&atlus_fonts_dir());
        let mut style = TextStyle::new("Anton", None, None);
        style.nowrap = true;
        // Opaque ink still has partial-coverage edge pixels.
        let (pm, _) = e
            .render("H", 160.0, 160.0, 96.0, Weight::NORMAL, "left", (255, 255, 255, 255), &style)
            .expect("render opaque");
        let (partial, opaque, max) = alpha_histogram(&pm);
        assert!(opaque > 100, "expected opaque ink, got {opaque} full pixels");
        assert!(partial > 100, "anti-aliased edges lost: only {partial} partial pixels");
        assert_eq!(max, 255);
        // Colour alpha must be honoured: rgba(255,255,255,0.25) is translucent.
        let (pm, _) = e
            .render("H", 160.0, 160.0, 96.0, Weight::NORMAL, "left", (255, 255, 255, 64), &style)
            .expect("render translucent");
        let (partial, _, max) = alpha_histogram(&pm);
        assert!(max <= 64, "text colour alpha ignored: max alpha {max} (was binary 0/255)");
        assert!(partial > 100, "translucent glyph must still be anti-aliased");
    }

    #[test]
    fn nowrap_large_display_ink_is_not_clipped() {
        let e = TextEngine::new();
        e.load_fonts_from_dir(&atlus_fonts_dir());
        let mut style = TextStyle::new("Playfair Display Black", None, None);
        style.nowrap = true;
        let size = 148.0;
        let color = (0, 0, 0, 255);
        // Display serif with a descender in the authored em-square box: the
        // ink overflows the 1-em line box above and below, so the old fixed
        // 8 px margin clipped it (margin must now derive from real ink).
        let (tall, _) = e
            .render("Typing", 1000.0, size * 2.0, size, Weight::NORMAL, "left", color, &style)
            .expect("tall box");
        let (em, margin) = e
            .render("Typing", 1000.0, size, size, Weight::NORMAL, "left", color, &style)
            .expect("em box");
        assert!(
            margin > 8.0,
            "margin must grow with display size/ink, got fixed-pad-like {margin}"
        );
        let (_, y0, _, y1) = ink_bbox(&em).expect("ink");
        assert!(
            y0 > 0 && y1 + 1 < em.height(),
            "nowrap ink clipped at the raster border: y {y0}..{y1} of {} (margin {margin})",
            em.height()
        );
        let (a, b) = (ink_count(&tall), ink_count(&em));
        let tolerance = (a / 100).max(4) as i64;
        assert!(
            (a as i64 - b as i64).abs() <= tolerance,
            "em-square nowrap box loses ink: tall={a} em={b} (margin {margin})"
        );
    }

    #[test]
    fn cached_render_returns_identical_margin_and_pixels() {
        let e = TextEngine::new();
        e.load_fonts_from_dir(&atlus_fonts_dir());
        let mut style = TextStyle::new("Playfair Display Black", Some((3.0, (0, 0, 0, 255))), None);
        style.nowrap = true;
        let draw = || {
            e.render("Wg", 320.0, 90.0, 64.0, Weight::NORMAL, "left", (255, 255, 255, 255), &style)
                .expect("render")
        };
        let (pm1, m1) = draw();
        let first = pm1.data().to_vec();
        let (pm2, m2) = draw();
        // Cache hit must not return a quantized margin or different pixels.
        assert_eq!(m1, m2, "hit/miss margin disagreement");
        assert_eq!(m1, m1.round(), "returned margin must match integer padding");
        assert_eq!(first, pm2.data(), "cache hit must reproduce the first raster");
    }

    #[test]
    fn pixmap_cache_is_byte_bounded() {
        let key = |i: u32| -> PixmapKey {
            (format!("k{i}"), 0, 0, 0, 0, 0, [0; 4], String::new(), TextStyleKey::default())
        };
        let mut cache = PixmapCache::default();
        for i in 0..200u32 {
            let pm = Pixmap::new(256, 256).expect("pixmap");
            cache.insert(key(i), Some((pm, 0.0)));
        }
        assert!(
            cache.bytes <= PIXMAP_CACHE_MAX_BYTES,
            "cache exceeds byte budget: {} > {PIXMAP_CACHE_MAX_BYTES}",
            cache.bytes
        );
        assert!(cache.map.len() < 200, "entries must be evicted once the budget is reached");
        // A single huge display raster is rendered but never cached.
        let mut big = PixmapCache::default();
        let pm = Pixmap::new(2048, 2048).expect("pixmap");
        big.insert(key(0), Some((pm, 0.0)));
        assert!(big.map.is_empty(), "oversized raster must not be cached");
        big.clear();
        assert_eq!(big.bytes, 0);
    }

    #[test]
    fn measure_cache_is_bounded() {
        let mut cache = MeasureCache::default();
        for i in 0..(MEASURE_CACHE_MAX_ENTRIES as u32 + 100) {
            cache.insert((format!("t{i}"), 0, 400, String::new()), i as f32);
        }
        assert!(
            cache.map.len() <= MEASURE_CACHE_MAX_ENTRIES,
            "measure cache unbounded: {} entries",
            cache.map.len()
        );
    }

    #[test]
    fn ransom_plans_are_memoized() {
        let e = TextEngine::new();
        e.load_fonts_from_dir(&atlus_fonts_dir());
        let fonts = default_ransom_fonts();
        let a = e.ransom_plan("CALCULATOR", 36.0, &fonts);
        let b = e.ransom_plan("CALCULATOR", 36.0, &fonts);
        assert!(Rc::ptr_eq(&a, &b), "ransom plan must be cached, not re-shaped per call");
    }

    #[test]
    fn font_weight_snaps_to_available_face_of_requested_family() {
        let e = TextEngine::new();
        e.load_fonts_from_dir(&atlus_fonts_dir());
        let _ = e.measure("x", 16.0, Weight::NORMAL, "Playfair Display Black");
        let mut guard = e.inner.font_system.borrow_mut();
        let fs = guard.as_mut().expect("font system");
        // Playfair Display Black ships 900 only; a w400 request must snap to it.
        let (family, weight) = resolve_face(fs, "Playfair Display Black", Weight::NORMAL);
        assert_eq!(family.as_deref(), Some("Playfair Display Black"));
        assert_eq!(weight.0, 900);
        // Anton ships 400 only; a bold request must snap down, not swap.
        let (family, weight) = resolve_face(fs, "Anton", Weight::BOLD);
        assert_eq!(family.as_deref(), Some("Anton"));
        assert_eq!(weight.0, 400);
        // An empty family first resolves the system sans-serif generic, then
        // snaps to a weight that concrete family really has.
        let generic = fs.db().family_name(&Family::SansSerif).to_string();
        if generic.is_empty() {
            return;
        }
        let (family, weight) = resolve_face(fs, "", Weight::MEDIUM);
        assert_eq!(family.as_deref(), Some(generic.as_str()));
        let exact_face = fs.db().faces().any(|f| {
            f.style == cosmic_text::fontdb::Style::Normal
                && f.weight.0 == weight.0
                && f.families.iter().any(|(n, _)| n.eq_ignore_ascii_case(&generic))
        });
        assert!(
            exact_face,
            "generic sans snapped to {generic} {:?} without an exact face",
            weight
        );
    }

    #[test]
    fn requested_weight_does_not_swap_the_family() {
        let e = TextEngine::new();
        e.load_fonts_from_dir(&atlus_fonts_dir());
        // Regression: Playfair Display Black at w400 silently shaped Noto Sans.
        let normal = e.measure("HAMBURG EF", 28.0, Weight::NORMAL, "Playfair Display Black");
        let black = e.measure("HAMBURG EF", 28.0, Weight::BLACK, "Playfair Display Black");
        assert!(
            (normal - black).abs() < 0.01,
            "weight request swapped the family instead of snapping: {normal} vs {black}"
        );
        let noto = e.measure("HAMBURG EF", 28.0, Weight::NORMAL, "Noto Sans");
        assert!(
            (normal - noto).abs() > 1.0,
            "Playfair text must not fall back to Noto Sans: playfair={normal} noto={noto}"
        );
        // Geometry derived from a weightless measurement (the old text_width)
        // now matches the rendered weight because both snap identically.
        let requested = e.measure("HAMBURG EF", 28.0, Weight::MEDIUM, "");
        let mut guard = e.inner.font_system.borrow_mut();
        let fs = guard.as_mut().expect("font system");
        let (family, weight) = resolve_face(fs, "", Weight::MEDIUM);
        drop(guard);
        let family = family.unwrap_or_default();
        let snapped = e.measure("HAMBURG EF", 28.0, Weight(weight.0), &family);
        assert!(
            (requested - snapped).abs() < 0.01,
            "measure must honour the snapped face: {requested} vs {snapped}"
        );
    }

    #[test]
    fn weight_tokens() {
        assert_eq!(TextEngine::weight_of_str("500"), Weight::MEDIUM);
        assert_eq!(TextEngine::weight_of_str("semibold"), Weight::SEMIBOLD);
        assert_eq!(TextEngine::weight_of_str("900"), Weight::BLACK);
        assert_eq!(TextEngine::weight_of_str(""), Weight::NORMAL);
        assert_eq!(TextEngine::weight_of_str("nonsense"), Weight::NORMAL);
    }
}

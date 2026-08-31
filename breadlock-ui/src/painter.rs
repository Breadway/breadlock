//! Software-rendering primitives shared by `breadlock`'s frame composition:
//! rounded-rect paths (radius tokens from [`bread_theme::tokens`]) and text
//! layout/rasterization via `cosmic-text`, blitted into a `tiny-skia`
//! `Pixmap`. Only linked into `breadlock` — `breadgreet` draws through GTK/CSS
//! instead and doesn't need a font-shaping stack.

pub use bread_theme::tokens;
pub use cosmic_text::Weight;
use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache};
use std::collections::HashMap;
use tiny_skia::{Path, PathBuilder, Pixmap, PremultipliedColorU8};

/// Builds a rounded-rectangle path. `radius` is clamped so it never exceeds
/// half the shorter side (a degenerate radius would otherwise self-intersect).
pub fn rounded_rect(x: f32, y: f32, w: f32, h: f32, radius: f32) -> Option<Path> {
    let r = radius.max(0.0).min(w / 2.0).min(h / 2.0);
    let mut pb = PathBuilder::new();
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
    pb.finish()
}

/// Owns the font database and glyph raster cache. Expensive to create
/// (`FontSystem::new()` scans installed fonts), so construct once and reuse
/// across every frame.
pub struct TextRenderer {
    font_system: FontSystem,
    swash_cache: SwashCache,
    /// Exact glyph-pixel span `(top, height)` per unique `(text, family,
    /// size, weight)` — see [`Self::measure_box`]. Keyed by size in
    /// centipixels so fractional sizes don't thrash the cache.
    boxes: HashMap<(String, String, u32, u16), (f32, f32)>,
    /// Whether `Family::Name(family)` resolved to an installed face. Missing
    /// families fall back to `Family::SansSerif` instead of panicking or
    /// drawing tofu; the result is cached so we don't scan fontdb every frame.
    family_ok: HashMap<String, bool>,
}

impl Default for TextRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl TextRenderer {
    pub fn new() -> Self {
        Self {
            font_system: FontSystem::new(),
            swash_cache: SwashCache::new(),
            boxes: HashMap::new(),
            family_ok: HashMap::new(),
        }
    }

    /// `Family::Name` if `family` is installed, otherwise the generic
    /// sans-serif. Never panics on a missing configured font.
    fn resolve_family<'a>(&mut self, family: &'a str) -> Family<'a> {
        if family.is_empty() || family.eq_ignore_ascii_case("sans-serif") {
            return Family::SansSerif;
        }
        let present = if let Some(&ok) = self.family_ok.get(family) {
            ok
        } else {
            let ok = self.font_system.db().faces().any(|face| {
                face.families
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case(family))
            });
            self.family_ok.insert(family.to_string(), ok);
            ok
        };
        if present {
            Family::Name(family)
        } else {
            Family::SansSerif
        }
    }

    fn shape_line(
        &mut self,
        text: &str,
        family: &str,
        size_px: f32,
        max_width: f32,
        weight: Weight,
    ) -> Buffer {
        // cosmic-text panics if `metrics.font_size` is zero; callers may pass a
        // scaled-to-zero size during the pill's appear overshoot at t=0.
        let size_px = size_px.max(0.01);
        let metrics = Metrics::new(size_px, size_px * 1.25);
        let mut buffer = Buffer::new(&mut self.font_system, metrics);
        buffer.set_size(&mut self.font_system, Some(max_width), Some(size_px * 2.0));
        let attrs = Attrs::new()
            .family(self.resolve_family(family))
            .weight(weight);
        buffer.set_text(&mut self.font_system, text, &attrs, Shaping::Advanced);
        buffer.shape_until_scroll(&mut self.font_system, false);
        buffer
    }

    /// Exact vertical span `(top, height)` of the glyph pixels a line drawn
    /// with [`Self::draw_line`] at `(0, 0)` would occupy: `top` is the
    /// distance from the draw origin down to the highest glyph pixel.
    /// `draw_line`'s `origin_y` anchors the *top* of the text (not the
    /// baseline), so centering a line of height `h` in a box spanning
    /// `[y0, y1]` needs `origin_y = y0 + (h - height) / 2 - top`.
    ///
    /// Measured exactly by rendering the line once into a tiny offscreen
    /// pixmap and scanning it, then cached — lock-screen text changes rarely
    /// (clock per minute, date per day, static hints once), so the one-off
    /// cost is negligible and the result is correct for any font.
    pub fn measure_box(&mut self, text: &str, family: &str, size_px: f32) -> (f32, f32) {
        self.measure_box_weighted(text, family, size_px, Weight::NORMAL)
    }

    /// Like [`Self::measure_box`] with an explicit font weight (the clock
    /// uses [`Weight::BOLD`] / 700).
    pub fn measure_box_weighted(
        &mut self,
        text: &str,
        family: &str,
        size_px: f32,
        weight: Weight,
    ) -> (f32, f32) {
        let key = (
            text.to_string(),
            family.to_string(),
            (size_px * 100.0) as u32,
            weight.0,
        );
        if let Some(b) = self.boxes.get(&key) {
            return *b;
        }
        let w = self
            .measure_line_weighted(text, family, size_px, weight)
            .ceil()
            .max(1.0) as u32;
        let h = (size_px * 1.5).ceil().max(1.0) as u32;
        let mut probe = match Pixmap::new(w, h) {
            Some(p) => p,
            None => return (0.0, size_px),
        };
        self.draw_line_weighted(
            &mut probe,
            text,
            family,
            size_px,
            tiny_skia::Color::WHITE,
            0.0,
            0.0,
            weight,
        );
        let (mut top, mut bottom) = (h as f32, 0.0f32);
        for y in 0..h {
            for x in 0..w {
                if probe.pixel(x, y).is_some_and(|p| p.alpha() > 0) {
                    top = top.min(y as f32);
                    bottom = bottom.max(y as f32);
                }
            }
        }
        let boxed = if bottom >= top {
            (top, bottom - top + 1.0)
        } else {
            (0.0, size_px)
        };
        self.boxes.insert(key, boxed);
        boxed
    }

    /// Width in pixels `text` would occupy if drawn via [`Self::draw_line`]
    /// with the same `family`/`size_px` — use to center text before drawing.
    pub fn measure_line(&mut self, text: &str, family: &str, size_px: f32) -> f32 {
        self.measure_line_weighted(text, family, size_px, Weight::NORMAL)
    }

    pub fn measure_line_weighted(
        &mut self,
        text: &str,
        family: &str,
        size_px: f32,
        weight: Weight,
    ) -> f32 {
        let buffer = self.shape_line(text, family, size_px, f32::INFINITY, weight);
        buffer
            .layout_runs()
            .map(|run| run.line_w)
            .fold(0.0, f32::max)
    }

    /// Shapes `text` as a single line in `family` at `size_px` and blits it
    /// into `pixmap` with its top-left baseline anchor at `(origin_x,
    /// origin_y)`. Pixels outside `pixmap`'s bounds are silently clipped.
    /// Origins stay float: subpixel X goes into cosmic-text's CacheKey bins
    /// so appear/unlock motion doesn't stair-step against the pill path.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_line(
        &mut self,
        pixmap: &mut Pixmap,
        text: &str,
        family: &str,
        size_px: f32,
        color: tiny_skia::Color,
        origin_x: f32,
        origin_y: f32,
    ) {
        self.draw_line_weighted(
            pixmap,
            text,
            family,
            size_px,
            color,
            origin_x,
            origin_y,
            Weight::NORMAL,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw_line_weighted(
        &mut self,
        pixmap: &mut Pixmap,
        text: &str,
        family: &str,
        size_px: f32,
        color: tiny_skia::Color,
        origin_x: f32,
        origin_y: f32,
        weight: Weight,
    ) {
        // Infinite width so this agrees with [`Self::measure_line`] (a finite
        // width would wrap, and centering from the unwrapped measure then
        // goes negative). Overflow is clipped at blit time.
        let buffer = self.shape_line(text, family, size_px, f32::INFINITY, weight);

        let c8 = color.to_color_u8();
        // cosmic-text's glyph-Mask rendering drops the base color's alpha
        // entirely — its swash `with_pixels` uses the glyph coverage as the
        // output alpha (see the "TODO: blend base alpha?" in its source), so
        // a translucent text color would render fully opaque. Fold the
        // requested alpha back in at blend time below; RGB stays straight.
        let base_alpha = c8.alpha();
        let text_color = cosmic_text::Color::rgba(c8.red(), c8.green(), c8.blue(), base_alpha);

        let (width, height) = (pixmap.width() as i32, pixmap.height() as i32);
        for run in buffer.layout_runs() {
            for glyph in run.glyphs.iter() {
                // Subpixel origin: X lands in CacheKey's subpixel bins; Y is
                // hinted (cosmic-text truncates the Y offset) and then the
                // run's line_y is rounded at blit so we don't trunc origin
                // independently of glyph placement.
                let physical = glyph.physical((origin_x, origin_y), 1.0);
                let glyph_color = glyph.color_opt.unwrap_or(text_color);
                self.swash_cache.with_pixels(
                    &mut self.font_system,
                    physical.cache_key,
                    glyph_color,
                    |x, y, color| {
                        let px = physical.x + x;
                        let py = run.line_y.round() as i32 + physical.y + y;
                        if px < 0 || py < 0 || px >= width || py >= height {
                            return;
                        }
                        let (r, g, b, a) = color.as_rgba_tuple();
                        if a == 0 {
                            return;
                        }
                        let a = (a as u32 * base_alpha as u32 / 255) as u8;
                        if a == 0 {
                            return;
                        }
                        blend_over(pixmap, px as u32, py as u32, r, g, b, a);
                    },
                );
            }
        }
    }
}

/// Alpha-blends a straight-alpha `(r, g, b, a)` source pixel over a
/// destination of *any* alpha. Two paths use this:
///
/// - **Full compose**: the background is painted fully opaque before any
///   text, so the destination alpha is always 255 and the result is opaque
///   (the exact formula below, kept byte-identical to the historic one).
/// - **GPU chrome** (`compose_chrome`): text is drawn into a *transparent*
///   pixmap that is later composited over the GPU background, so glyph
///   edges must keep real alpha — a forced-255 blend here would make every
///   glyph opaque and, composited over the background, visibly wrong.
///
/// Premultiplied source-over: `out = src_pm + dst_pm * (1 - src_a)`, which
/// preserves the `PremultipliedColorU8` invariant (`rgb <= a`).
fn blend_over(pixmap: &mut Pixmap, x: u32, y: u32, r: u8, g: u8, b: u8, a: u8) {
    let idx = (y * pixmap.width() + x) as usize;
    let pixels = pixmap.pixels_mut();
    let Some(dst) = pixels.get(idx).copied() else {
        return;
    };
    let sa = a as u32;
    if dst.alpha() == 255 {
        // Opaque destination: the classic exact blend. RGB mixes toward the
        // source, alpha stays 255 — identical to the pre-split behavior so
        // the single-pass software path doesn't move a single pixel.
        let mix = |s: u8, d: u8| -> u8 { ((s as u32 * sa + d as u32 * (255 - sa)) / 255) as u8 };
        if let Some(blended) = PremultipliedColorU8::from_rgba(
            mix(r, dst.red()),
            mix(g, dst.green()),
            mix(b, dst.blue()),
            255,
        ) {
            pixels[idx] = blended;
        }
        return;
    }
    // General (possibly transparent) destination: premultiplied source-over.
    // out_a = sa + da*(255-sa)/255; out_rgb = src_rgb*sa/255 + dst_rgb*(1-sa).
    let da = dst.alpha() as u32;
    let out_a = (sa + da * (255 - sa) / 255) as u8;
    let out_c =
        |c: u8, dc: u8| -> u8 { (c as u32 * sa / 255 + dc as u32 * (255 - sa) / 255) as u8 };
    if let Some(blended) = PremultipliedColorU8::from_rgba(
        out_c(r, dst.red()),
        out_c(g, dst.green()),
        out_c(b, dst.blue()),
        out_a,
    ) {
        pixels[idx] = blended;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounded_rect_produces_closed_path() {
        let path = rounded_rect(0.0, 0.0, 100.0, 40.0, tokens::RADIUS_SECONDARY as f32).unwrap();
        assert!(!path.is_empty());
    }

    #[test]
    fn rounded_rect_clamps_oversized_radius() {
        // radius larger than half the shorter side must not panic or produce garbage
        let path = rounded_rect(0.0, 0.0, 10.0, 10.0, 999.0);
        assert!(path.is_some());
    }

    #[test]
    fn text_renderer_draws_without_panicking_on_tiny_pixmap() {
        let mut pixmap = Pixmap::new(64, 16).unwrap();
        pixmap.fill(tiny_skia::Color::BLACK);
        let mut renderer = TextRenderer::new();
        renderer.draw_line(
            &mut pixmap,
            "12:34",
            "sans-serif",
            12.0,
            tiny_skia::Color::WHITE,
            2.0,
            2.0,
        );
        // No panic and the pixmap remains fully opaque is the property under test —
        // exact glyph coverage depends on whatever fonts are installed on the CI host.
        assert!(pixmap.pixels().iter().all(|p| p.alpha() == 255));
    }

    #[test]
    fn draw_line_respects_color_alpha() {
        // Regression: cosmic-text's glyph-Mask path drops the base color's
        // alpha (coverage becomes the only alpha), so translucent text used to
        // render fully opaque — which broke every text fade on the lock screen
        // (clock/date/hint/status never faded during appear/unlock).
        let mut renderer = TextRenderer::new();

        let mut full = Pixmap::new(200, 40).unwrap();
        full.fill(tiny_skia::Color::BLACK);
        renderer.draw_line(
            &mut full,
            "12:34",
            "sans-serif",
            24.0,
            tiny_skia::Color::WHITE,
            0.0,
            0.0,
        );
        let full_max = full.pixels().iter().map(|p| p.red()).max().unwrap();
        assert!(
            full_max > 200,
            "full-alpha text should render bright, got {full_max}"
        );

        let faint = tiny_skia::Color::from_rgba(1.0, 1.0, 1.0, 0.1).unwrap();
        let mut low = Pixmap::new(200, 40).unwrap();
        low.fill(tiny_skia::Color::BLACK);
        renderer.draw_line(&mut low, "12:34", "sans-serif", 24.0, faint, 0.0, 0.0);
        let low_max = low.pixels().iter().map(|p| p.red()).max().unwrap();
        assert!(
            low_max < 100,
            "10%-alpha text must not render near-white, got {low_max}"
        );
    }

    #[test]
    fn missing_font_family_falls_back_without_panic() {
        let mut pixmap = Pixmap::new(64, 16).unwrap();
        pixmap.fill(tiny_skia::Color::BLACK);
        let mut renderer = TextRenderer::new();
        renderer.draw_line(
            &mut pixmap,
            "12:34",
            "DefinitelyNotARealFontFamily_xyzzy",
            12.0,
            tiny_skia::Color::WHITE,
            2.0,
            2.0,
        );
        assert!(pixmap.pixels().iter().any(|p| p.alpha() > 0));
    }

    #[test]
    fn draw_line_onto_transparent_keeps_real_alpha() {
        // Regression: the GPU path (compose_chrome) draws text into a
        // transparent pixmap that is later composited over the GPU background.
        // The old blend forced output alpha to 255, so every glyph became
        // opaque and, once composited, rendered visibly wrong (dark, covering
        // the background instead of blending). Glyph cores must carry real
        // alpha here so the final source-over composite is correct.
        let mut renderer = TextRenderer::new();

        let mut t = Pixmap::new(200, 40).unwrap(); // starts transparent
        renderer.draw_line(
            &mut t,
            "12:34",
            "sans-serif",
            24.0,
            tiny_skia::Color::WHITE,
            0.0,
            0.0,
        );
        // Full-coverage glyph cores are legitimately opaque, but the AA
        // edges must carry real intermediate alphas — the old forced-255
        // blend made *every* drawn pixel (edges included) fully opaque.
        let has_edge = t.pixels().iter().any(|p| p.alpha() > 0 && p.alpha() < 255);
        assert!(
            has_edge,
            "glyph AA edges must keep intermediate alphas onto a transparent pixmap"
        );
        // And a 50%-alpha draw must not produce fully-opaque pixels.
        let mut t2 = Pixmap::new(200, 40).unwrap();
        let half = tiny_skia::Color::from_rgba(1.0, 1.0, 1.0, 0.5).unwrap();
        renderer.draw_line(&mut t2, "12:34", "sans-serif", 24.0, half, 0.0, 0.0);
        assert!(
            t2.pixels().iter().all(|p| p.alpha() <= 128 + 3),
            "50%-alpha text onto transparent must stay ~half alpha"
        );
    }
}

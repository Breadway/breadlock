//! Software-rendering primitives shared by `breadlock`'s frame composition:
//! rounded-rect paths (radius tokens from [`bread_theme::tokens`]) and text
//! layout/rasterization via `cosmic-text`, blitted into a `tiny-skia`
//! `Pixmap`. Only linked into `breadlock` — `breadgreet` draws through GTK/CSS
//! instead and doesn't need a font-shaping stack.

pub use bread_theme::tokens;
use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache};
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
        }
    }

    fn shape_line(&mut self, text: &str, family: &str, size_px: f32, max_width: f32) -> Buffer {
        let metrics = Metrics::new(size_px, size_px * 1.25);
        let mut buffer = Buffer::new(&mut self.font_system, metrics);
        buffer.set_size(&mut self.font_system, Some(max_width), Some(size_px * 2.0));
        let attrs = Attrs::new().family(Family::Name(family));
        buffer.set_text(&mut self.font_system, text, &attrs, Shaping::Advanced);
        buffer.shape_until_scroll(&mut self.font_system, false);
        buffer
    }

    /// Width in pixels `text` would occupy if drawn via [`Self::draw_line`]
    /// with the same `family`/`size_px` — use to center text before drawing.
    pub fn measure_line(&mut self, text: &str, family: &str, size_px: f32) -> f32 {
        let buffer = self.shape_line(text, family, size_px, f32::INFINITY);
        buffer
            .layout_runs()
            .map(|run| run.line_w)
            .fold(0.0, f32::max)
    }

    /// Shapes `text` as a single line in `family` at `size_px` and blits it
    /// into `pixmap` with its top-left baseline anchor at `(origin_x,
    /// origin_y)`. Pixels outside `pixmap`'s bounds are silently clipped.
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
        let buffer = self.shape_line(text, family, size_px, pixmap.width() as f32);

        let c8 = color.to_color_u8();
        let text_color = cosmic_text::Color::rgba(c8.red(), c8.green(), c8.blue(), c8.alpha());

        let (width, height) = (pixmap.width() as i32, pixmap.height() as i32);
        let ox = origin_x as i32;
        let oy = origin_y as i32;
        buffer.draw(
            &mut self.font_system,
            &mut self.swash_cache,
            text_color,
            |x, y, _w, _h, glyph_color| {
                let (px, py) = (ox + x, oy + y);
                if px < 0 || py < 0 || px >= width || py >= height {
                    return;
                }
                let (r, g, b, a) = glyph_color.as_rgba_tuple();
                if a == 0 {
                    return;
                }
                blend_over_opaque(pixmap, px as u32, py as u32, r, g, b, a);
            },
        );
    }
}

/// Alpha-blends a straight-alpha `(r, g, b, a)` source pixel over an
/// **opaque** destination pixel (always true here — the lock screen
/// background is painted fully opaque before any text or UI chrome).
/// Because the destination alpha is always 255, the blended result is also
/// opaque, so the `PremultipliedColorU8` invariant (`rgb <= a`) always holds.
fn blend_over_opaque(pixmap: &mut Pixmap, x: u32, y: u32, r: u8, g: u8, b: u8, a: u8) {
    let idx = (y * pixmap.width() + x) as usize;
    let pixels = pixmap.pixels_mut();
    let Some(dst) = pixels.get(idx).copied() else {
        return;
    };
    let a32 = a as u32;
    let mix = |s: u8, d: u8| -> u8 { ((s as u32 * a32 + d as u32 * (255 - a32)) / 255) as u8 };
    let blended = PremultipliedColorU8::from_rgba(
        mix(r, dst.red()),
        mix(g, dst.green()),
        mix(b, dst.blue()),
        255,
    );
    if let Some(blended) = blended {
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
}

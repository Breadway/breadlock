//! Lock-screen background: a solid palette color, or a static image scaled
//! to cover the surface. Live blur-of-desktop (hyprlock-style) is a v2
//! follow-up (see README) — `blur = true` is accepted but only logs a
//! warning in v1. `ken_burns = true` adds a slow, continuous pan+zoom to
//! image backgrounds (opt-in: it keeps the background redrawing at a low
//! frame rate while locked).
//!
//! The renderer is fully software (tiny-skia), so every frame redraws the
//! whole surface. Rescaling the *source* wallpaper on every frame is
//! prohibitively expensive for large images (a 4K source at output size took
//! ~50 ms/frame — choppy at any cadence), so the source is pre-scaled once
//! per output size into a cache and each frame is a translate-only blit.

use breadlock_ui::config::{Background as BackgroundConfig, BackgroundMode};
use std::cell::RefCell;
use std::f32::consts::TAU;
use tiny_skia::{Pixmap, PixmapPaint, Transform};

/// One full Ken Burns pan+zoom cycle, in seconds. Deliberately slow so the
/// motion reads as a gentle drift rather than a slideshow.
const KENBURNS_PERIOD_S: f32 = 90.0;
/// Extra zoom beyond plain cover-fit — gives the pan room to travel without
/// ever exposing the image edges.
const KENBURNS_ZOOM: f32 = 1.06;

pub enum Background {
    Color(tiny_skia::Color),
    Image(ImageBg),
}

/// A wallpaper with a lazily-built, output-sized copy. The first `paint` for
/// a given output size does one downscale; every frame after that blits the
/// cached copy with at most a translation (the Ken Burns pan).
/// Cap on cached scaled copies — enough for a typical multi-monitor setup
/// without unbounded growth if the compositor sends many sizes.
const SCALED_CACHE_SLOTS: usize = 4;

pub struct ImageBg {
    /// Original wallpaper. Kept so a different output size (hotplug) simply
    /// rebuilds the cache rather than needing the source reloaded.
    source: Pixmap,
    ken_burns: bool,
    /// Last scaled copies **per target size**. A single slot thrashed every
    /// frame under `redraw_all` with two monitors of different sizes.
    cache: RefCell<Vec<ScaledBg>>,
}

struct ScaledBg {
    /// `source` pre-scaled to cover-fit (× Ken Burns zoom when enabled) and
    /// sized to the output — same size or larger, so drawing it needs no
    /// per-frame scaling.
    pixmap: Pixmap,
    /// How many pixels the scaled image overhangs each axis — the pan room.
    pan_x: f32,
    pan_y: f32,
    target_w: u32,
    target_h: u32,
}

/// Copies `src` into `target` shifted by `(dx, dy)` (target pixels). `src` is
/// at least as large as `target` in both axes (guaranteed by the cover-fit
/// cache build), and `dx, dy` are pan offsets in `[-pan, 0]`, so the visible
/// region is `src[-dx..-dx+tw, -dy..-dy+th]`.
///
/// With `bilinear` the fractional part of the offset is sub-pixel filtered,
/// so a slow pan glides instead of stepping one whole pixel at a time (which
/// reads as judder); when the offset is (near-)integer, or `bilinear` is off
/// (the 60 fps animation frames, where the pan moves < 0.2 px anyway), the
/// whole thing collapses to row memcpys. The bilinear path is an integer
/// fixed-point (16.16) loop with the edge clamping hoisted out of the hot
/// columns/rows — far cheaper than
/// [`tiny_skia::Pixmap::draw_pixmap`], which rasterizes every pixel through
/// its general pattern pipeline.
fn blit_translate(target: &mut Pixmap, src: &Pixmap, dx: f32, dy: f32, bilinear: bool) {
    let tw = target.width() as usize;
    let th = target.height() as usize;
    let sw = src.width() as usize;
    let sh = src.height() as usize;
    let sx = (-dx).clamp(0.0, sw.saturating_sub(tw) as f32);
    let sy = (-dy).clamp(0.0, sh.saturating_sub(th) as f32);

    let fx = (sx.fract() * 65536.0) as u32 & 0xFFFF;
    let fy = (sy.fract() * 65536.0) as u32 & 0xFFFF;
    let ix = sx as usize;
    let iy = sy as usize;

    let sdata = src.data();
    let dst = target.data_mut();

    if !bilinear || (fx == 0 && fy == 0) {
        for row in 0..th {
            let src_row = (iy + row) * sw + ix;
            let dst_row = row * tw;
            let (s, d) = (
                &sdata[src_row * 4..(src_row + tw) * 4],
                &mut dst[dst_row * 4..(dst_row + tw) * 4],
            );
            d.copy_from_slice(s);
        }
        return;
    }

    let wx = fx;
    let wx_inv = 65536 - wx;
    let wy = fy;
    let wy_inv = 65536 - wy;
    let swm1 = sw - 1;
    let shm1 = sh - 1;

    // Per-channel bilinear in packed u32 (one load per pixel instead of four,
    // one store instead of four — the loop is latency-bound). Each byte's
    // products stay well under 2^32, so lanes never interfere.
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    unsafe fn lerp4(
        sdata: &[u8],
        i00: usize,
        i10: usize,
        i01: usize,
        i11: usize,
        di: usize,
        wx: u32,
        wx_inv: u32,
        wy: u32,
        wy_inv: u32,
        dst: &mut [u8],
    ) {
        let a = u32::from_ne_bytes([
            *sdata.get_unchecked(i00),
            *sdata.get_unchecked(i00 + 1),
            *sdata.get_unchecked(i00 + 2),
            *sdata.get_unchecked(i00 + 3),
        ]);
        let b = u32::from_ne_bytes([
            *sdata.get_unchecked(i01),
            *sdata.get_unchecked(i01 + 1),
            *sdata.get_unchecked(i01 + 2),
            *sdata.get_unchecked(i01 + 3),
        ]);
        let d = u32::from_ne_bytes([
            *sdata.get_unchecked(i10),
            *sdata.get_unchecked(i10 + 1),
            *sdata.get_unchecked(i10 + 2),
            *sdata.get_unchecked(i10 + 3),
        ]);
        let e = u32::from_ne_bytes([
            *sdata.get_unchecked(i11),
            *sdata.get_unchecked(i11 + 1),
            *sdata.get_unchecked(i11 + 2),
            *sdata.get_unchecked(i11 + 3),
        ]);
        let mut out = 0u32;
        for c in 0..4 {
            let shift = c * 8;
            let av = (a >> shift) & 0xFF;
            let bv = (b >> shift) & 0xFF;
            let dv = (d >> shift) & 0xFF;
            let ev = (e >> shift) & 0xFF;
            let top = (av * wx_inv + bv * wx) >> 16;
            let bot = (dv * wx_inv + ev * wx) >> 16;
            out |= ((top * wy_inv + bot * wy) >> 16) << shift;
        }
        dst[di..di + 4].copy_from_slice(&out.to_ne_bytes());
    }

    // Interior rows/columns: `ix + tw <= sw` and `iy + th <= sh` (both clamped
    // above), so `x0 + 1`/`y0 + 1` stay in bounds except on the last
    // column/row, which are handled after the hot loop. All indices are
    // verified in-bounds above the `unsafe` calls.
    for row in 0..th - 1 {
        let r0 = (iy + row) * sw;
        let r1 = r0 + sw;
        let drow = row * tw;
        for col in 0..tw - 1 {
            let i00 = (r0 + ix + col) * 4;
            let i10 = (r1 + ix + col) * 4;
            let di = (drow + col) * 4;
            // SAFETY: i01/i11 are the next column (col + 1 < tw, in bounds);
            // di + 4 < target size; rows in bounds per above.
            unsafe { lerp4(sdata, i00, i10, i00 + 4, i10 + 4, di, wx, wx_inv, wy, wy_inv, dst) };
        }
        // Last column of this row: clamp x1.
        let i00 = (r0 + ix + tw - 1) * 4;
        let i10 = (r1 + ix + tw - 1) * 4;
        let di = (drow + tw - 1) * 4;
        let x1 = (ix + tw - 1 + 1).min(swm1);
        let j0 = (r0 + x1) * 4;
        let j1 = (r1 + x1) * 4;
        // SAFETY: j0/j1 clamped within source, di within target.
        unsafe { lerp4(sdata, i00, i10, j0, j1, di, wx, wx_inv, wy, wy_inv, dst) };
    }
    // Last row: clamp y1.
    let r0 = (iy + th - 1) * sw;
    let r1 = (iy + th - 1 + 1).min(shm1) * sw;
    let drow = (th - 1) * tw;
    for col in 0..tw - 1 {
        let i00 = (r0 + ix + col) * 4;
        let i10 = (r1 + ix + col) * 4;
        let di = (drow + col) * 4;
        // SAFETY: in bounds as in the interior loop.
        unsafe { lerp4(sdata, i00, i10, i00 + 4, i10 + 4, di, wx, wx_inv, wy, wy_inv, dst) };
    }
    // Last column of the last row (both clamps).
    let i00 = (r0 + ix + tw - 1) * 4;
    let i10 = (r1 + ix + tw - 1) * 4;
    let di = (drow + tw - 1) * 4;
    let x1 = (ix + tw - 1 + 1).min(swm1);
    let j0 = (r0 + x1) * 4;
    let j1 = (r1 + x1) * 4;
    // SAFETY: all clamped in bounds.
    unsafe { lerp4(sdata, i00, i10, j0, j1, di, wx, wx_inv, wy, wy_inv, dst) };
}

impl Background {
    pub fn load(cfg: &BackgroundConfig, palette: &breadlock_ui::theme::Palette) -> Self {
        let fallback =
            || Background::Color(breadlock_ui::theme::tiny_skia_color(&palette.background));

        if cfg.blur {
            tracing::warn!(
                "background.blur is not implemented yet (planned v2 feature, needs a wlr-screencopy \
                 capture) — showing the configured background unblurred"
            );
        }

        match cfg.mode {
            BackgroundMode::Color => fallback(),
            BackgroundMode::Image => {
                if cfg.path.is_empty() {
                    tracing::warn!("background.mode = \"image\" but background.path is empty, falling back to palette color");
                    return fallback();
                }
                match Pixmap::load_png(&cfg.path) {
                    Ok(pixmap) => Background::Image(ImageBg {
                        source: pixmap,
                        ken_burns: cfg.ken_burns,
                        cache: RefCell::new(Vec::new()),
                    }),
                    Err(err) => {
                        tracing::warn!(path = %cfg.path, %err, "failed to load background image (PNG only in v1), falling back to palette color");
                        fallback()
                    }
                }
            }
        }
    }

    /// True when this background needs continuous redraws (Ken Burns pan).
    pub fn ken_burns(&self) -> bool {
        matches!(self, Background::Image(bg) if bg.ken_burns)
    }

    /// Paints this background into `target`, cover-fit (scaled uniformly to
    /// fill the surface, cropping any overflow — never letterboxed). `t_secs`
    /// is the monotonic clock: with Ken Burns enabled the image slowly pans
    /// and zooms along a smooth Lissajous-ish drift, so consecutive frames
    /// differ slightly but never jump.
    ///
    /// The expensive downscale happens at most once per output size (see
    /// [`ImageBg::cache`]); steady-state frames are a 1:1 blit plus a small
    /// translation, so the software renderer can hold its frame budget even
    /// with a multi-megapixel wallpaper.
    ///
    /// `smooth` asks for sub-pixel bilinear panning. The locker passes `true`
    /// on its slow idle frames (where the ~1 px/frame drift is visible) and
    /// `false` on 60 fps animation frames (where the pan moves < 0.2 px and
    /// the ~20 ms/frame bilinear would blow the frame budget).
    pub fn paint(&self, target: &mut Pixmap, t_secs: f32, smooth: bool) {
        match self {
            Background::Color(c) => target.fill(*c),
            Background::Image(bg) => {
                let (tw, th) = (target.width() as f32, target.height() as f32);
                let (sw, sh) = (bg.source.width() as f32, bg.source.height() as f32);
                if sw <= 0.0 || sh <= 0.0 {
                    return;
                }
                let mut cache = bg.cache.borrow_mut();
                let tw_px = target.width();
                let th_px = target.height();
                let hit = cache
                    .iter()
                    .position(|c| c.target_w == tw_px && c.target_h == th_px);
                if let Some(i) = hit {
                    // LRU: most-recently used at the end.
                    if i + 1 != cache.len() {
                        let entry = cache.remove(i);
                        cache.push(entry);
                    }
                } else {
                    let cover = (tw / sw).max(th / sh);
                    let scale = cover * if bg.ken_burns { KENBURNS_ZOOM } else { 1.0 };
                    let scaled_w = (sw * scale).round().max(1.0) as u32;
                    let scaled_h = (sh * scale).round().max(1.0) as u32;
                    let Some(mut pixmap) = Pixmap::new(scaled_w, scaled_h) else {
                        tracing::error!(
                            "failed to allocate {scaled_w}x{scaled_h} scaled wallpaper — falling back to a palette-color background"
                        );
                        drop(cache);
                        target.fill(breadlock_ui::theme::tiny_skia_color(
                            &breadlock_ui::theme::Palette::default().background,
                        ));
                        return;
                    };
                    pixmap.fill(tiny_skia::Color::BLACK);
                    // The one real downscale in the pipeline: bilinear so the
                    // cached layer is smooth (per-frame draws are pure copies
                    // and don't re-filter).
                    let paint = PixmapPaint {
                        quality: tiny_skia::FilterQuality::Bilinear,
                        ..Default::default()
                    };
                    pixmap.draw_pixmap(
                        0,
                        0,
                        bg.source.as_ref(),
                        &paint,
                        Transform::from_scale(scale, scale),
                        None,
                    );
                    if cache.len() >= SCALED_CACHE_SLOTS {
                        cache.remove(0);
                    }
                    cache.push(ScaledBg {
                        pixmap,
                        pan_x: scaled_w as f32 - tw,
                        pan_y: scaled_h as f32 - th,
                        target_w: tw_px,
                        target_h: th_px,
                    });
                }
                let scaled = cache.last().expect("cache populated above");
                target.fill(tiny_skia::Color::BLACK);
                let (tx, ty) = if bg.ken_burns {
                    let phase = t_secs * TAU / KENBURNS_PERIOD_S;
                    // Sin/cos offset by a quarter cycle: the pan traces a slow
                    // ellipse, starting from a corner.
                    (
                        -scaled.pan_x * (0.5 + 0.5 * phase.sin()),
                        -scaled.pan_y * (0.5 + 0.5 * phase.cos()),
                    )
                } else {
                    (0.0, 0.0)
                };
                // The cached pixmap is already output-sized, so this per-frame
                // draw is a 1:1 copy with at most a translation. `draw_pixmap`
                // runs the full raster pipeline per pixel (~20 ms for a
                // full-screen layer), which is the dominant software-render
                // cost — so do the blit directly instead: rows are memcpy'd
                // (nearest sampling on an already-correct-size image is
                // pixel-identical, and the pan offsets quantize the same way
                // tiny-skia's nearest filter does).
                blit_translate(target, &scaled.pixmap, tx, ty, smooth);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4x4 pixmap whose pixel at (x, y) is `(x * 63, y * 63, 0, 255)` —
    /// every pixel is distinct, so a shifted copy is easy to assert.
    fn source_grid() -> Pixmap {
        let mut p = Pixmap::new(4, 4).unwrap();
        for y in 0..4 {
            for x in 0..4 {
                p.pixels_mut()[y * 4 + x] = tiny_skia::PremultipliedColorU8::from_rgba(
                    (x * 63) as u8,
                    (y * 63) as u8,
                    0,
                    255,
                )
                .unwrap();
            }
        }
        p
    }

    #[test]
    fn blit_translate_copies_shifted_region() {
        let src = source_grid();
        let mut dst = Pixmap::new(2, 2).unwrap();
        // Shift the 4x4 source by (-1, -1): the visible region is src[1..3, 1..3].
        blit_translate(&mut dst, &src, -1.0, -1.0, false);
        let px = dst.pixels();
        assert_eq!(px[0].red(), 63, "(0,0) should be src(1,1) red");
        assert_eq!(px[0].green(), 63, "(0,0) should be src(1,1) green");
        assert_eq!(px[1].red(), 126, "(1,0) should be src(2,1) red");
        assert_eq!(px[1].green(), 63);
        assert_eq!(px[2].red(), 63, "(0,1) should be src(1,2) red");
        assert_eq!(px[2].green(), 126);
        assert_eq!(px[3].red(), 126, "(1,1) should be src(2,2)");
        assert_eq!(px[3].green(), 126);
    }

    #[test]
    fn blit_translate_clamps_within_source() {
        // An offset larger than the overhang must clamp, not read out of
        // bounds or leave uninitialized rows.
        let src = source_grid();
        let mut dst = Pixmap::new(2, 2).unwrap();
        blit_translate(&mut dst, &src, -99.0, -99.0, false);
        // Clamped to the bottom-right 2x2 of the source.
        let px = dst.pixels();
        assert_eq!(px[0].red(), 126);
        assert_eq!(px[0].green(), 126);
        assert_eq!(px[3].red(), 189);
        assert_eq!(px[3].green(), 189);
    }

    #[test]
    fn ken_burns_pan_never_exposes_edges() {
        // A small solid-color image panned through a full cycle must cover
        // the whole target at every phase — no black borders.
        let mut source = Pixmap::new(80, 40).unwrap();
        source.fill(tiny_skia::Color::from_rgba8(200, 30, 30, 255));
        let bg = Background::Image(ImageBg {
            source,
            ken_burns: true,
            cache: RefCell::new(Vec::new()),
        });
        let mut target = Pixmap::new(60, 30).unwrap();
        for i in 0..90 {
            bg.paint(&mut target, i as f32, true);
            assert!(
                target.pixels().iter().all(|p| p.red() == 200 && p.green() == 30),
                "frame {i} exposed an edge"
            );
        }
    }

    #[test]
    fn bilinear_shift_matches_fractional_position() {
        // A row of (0..255, 0, 0, 255): a half-pixel right shift should give
        // the exact average of each adjacent pair.
        let mut src = Pixmap::new(8, 1).unwrap();
        for x in 0..8 {
            src.pixels_mut()[x] =
                tiny_skia::PremultipliedColorU8::from_rgba((x * 32) as u8, 0, 0, 255).unwrap();
        }
        let mut dst = Pixmap::new(6, 1).unwrap();
        // Shift by (-0.5, 0): visible region starts at src 0.5 → each output
        // pixel averages src[x] and src[x + 1].
        blit_translate(&mut dst, &src, -0.5, 0.0, true);
        let px = dst.pixels();
        assert_eq!(px[0].red(), 16, "0.5px shift averages neighbors");
        assert_eq!(px[1].red(), ((32 + 64) / 2) as u8);
        assert_eq!(px[5].red(), ((160 + 192) / 2) as u8);
    }

    #[test]
    fn static_image_keeps_cover_fit() {
        // Without Ken Burns the image is cover-fit exactly: still no edges.
        let mut source = Pixmap::new(80, 40).unwrap();
        source.fill(tiny_skia::Color::from_rgba8(200, 30, 30, 255));
        let bg = Background::Image(ImageBg {
            source,
            ken_burns: false,
            cache: RefCell::new(Vec::new()),
        });
        let mut target = Pixmap::new(60, 30).unwrap();
        bg.paint(&mut target, 0.0, true);
        assert!(target.pixels().iter().all(|p| p.red() == 200 && p.green() == 30));
    }

    #[test]
    fn scaled_cache_keeps_a_slot_per_target_size() {
        // Two output sizes (two monitors) must not thrash a single slot.
        let mut source = Pixmap::new(80, 40).unwrap();
        source.fill(tiny_skia::Color::from_rgba8(200, 30, 30, 255));
        let image = ImageBg {
            source,
            ken_burns: false,
            cache: RefCell::new(Vec::new()),
        };
        let bg = Background::Image(image);
        let mut a = Pixmap::new(60, 30).unwrap();
        let mut b = Pixmap::new(40, 20).unwrap();
        bg.paint(&mut a, 0.0, false);
        bg.paint(&mut b, 0.0, false);
        bg.paint(&mut a, 0.0, false);
        let Background::Image(image) = &bg else {
            panic!("expected image background");
        };
        let cache = image.cache.borrow();
        assert_eq!(
            cache.len(),
            2,
            "two target sizes should occupy two slots, got {} slots",
            cache.len()
        );
        assert!(cache.iter().any(|c| c.target_w == 60 && c.target_h == 30));
        assert!(cache.iter().any(|c| c.target_w == 40 && c.target_h == 20));
    }
}

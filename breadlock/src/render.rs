//! Frame composition: paints one full lock-screen frame (background,
//! password pill, clock, status line) into a `tiny_skia::Pixmap`, then
//! copies it into a Wayland `wl_shm` buffer.
//!
//! tiny-skia's in-memory pixel format is byte-order RGBA; `wl_shm`'s
//! `Argb8888` format is host-endian `0xAARRGGBB`, i.e. byte-order BGRA on
//! little-endian machines. [`blit_to_shm`] does the swizzle.

use crate::background::Background;
use breadlock_ui::painter::{rounded_rect, tokens, TextRenderer};
use breadlock_ui::theme::tiny_skia_color;
use std::time::Instant;
use tiny_skia::{Color, Paint, Pixmap};

/// Lock-appear duration: overlay fades in and eases up from below rest.
pub const APPEAR_MS: u64 = 450;
/// Unlock-fade duration: overlay fades out with a slight upward drift.
pub const UNLOCK_MS: u64 = 400;
/// Redraw cadence while an animation is in flight (~60 Hz).
pub const ANIM_FRAME_MS: u64 = 16;

const APPEAR_SLIDE_PX: f32 = 28.0;
const UNLOCK_DRIFT_PX: f32 = 20.0;
const DIM_ALPHA: f32 = 0.28;

pub struct FrameInputs<'a> {
    pub width: u32,
    pub height: u32,
    pub background: &'a Background,
    pub palette: &'a breadlock_ui::theme::Palette,
    pub font_family: &'a str,
    pub clock_text: &'a str,
    pub password_len: usize,
    /// True while showing a failed-attempt state (red pill). No animated
    /// shake in v1 — just a color/status-text indicator.
    pub failed: bool,
    pub status_text: Option<&'a str>,
    /// Raw 0..1 lock-appear progress (pre-ease). 1 is rest pose.
    pub appear_t: f32,
    /// Raw 0..1 unlock-fade progress (pre-ease). 0 when not unlocking.
    pub unlock_t: f32,
}

/// Ease-out cubic. `t` is clamped to 0..1.
pub fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    let inv = 1.0 - t;
    1.0 - inv * inv * inv
}

/// Linear 0..1 progress since `started` over `duration_ms`.
pub fn unit_progress(started: Instant, duration_ms: u64) -> f32 {
    let dur = duration_ms as f32 / 1000.0;
    if dur <= 0.0 {
        return 1.0;
    }
    (started.elapsed().as_secs_f32() / dur).clamp(0.0, 1.0)
}

/// Overlay alpha and y-offset (positive is down) from raw 0..1 progress.
pub fn overlay_motion(appear_t: f32, unlock_t: f32) -> (f32, f32) {
    let appear = ease_out_cubic(appear_t);
    let unlock = ease_out_cubic(unlock_t);
    let alpha = (appear * (1.0 - unlock)).clamp(0.0, 1.0);
    let y = APPEAR_SLIDE_PX * (1.0 - appear) - UNLOCK_DRIFT_PX * unlock;
    (alpha, y)
}

fn faded(mut color: Color, alpha: f32) -> Color {
    color.apply_opacity(alpha);
    color
}

/// Composes one frame. Returns `None` only if `width`/`height` are degenerate
/// (a `0x0` `configure`, which some compositors send transiently).
pub fn compose(text: &mut TextRenderer, inputs: &FrameInputs) -> Option<Pixmap> {
    let mut pixmap = Pixmap::new(inputs.width, inputs.height)?;
    inputs.background.paint(&mut pixmap);

    let (alpha, y_off) = overlay_motion(inputs.appear_t, inputs.unlock_t);
    if alpha <= 0.0 {
        return Some(pixmap);
    }

    let (w, h) = (inputs.width as f32, inputs.height as f32);
    let surface_color = faded(tiny_skia_color(&inputs.palette.color0), alpha);
    let accent_color = faded(tiny_skia_color(&inputs.palette.color4), alpha);
    let on_surface = faded(
        tiny_skia_color(breadlock_ui::theme::ink_on(&inputs.palette.color0)),
        alpha,
    );
    let red_color = faded(tiny_skia_color(&inputs.palette.color1), alpha);

    // Translucent veil over the (static) wallpaper — fades with the chrome.
    if let Some(rect) = tiny_skia::Rect::from_xywh(0.0, 0.0, w, h) {
        let mut paint = Paint::default();
        paint.set_color(
            Color::from_rgba(0.0, 0.0, 0.0, DIM_ALPHA * alpha).unwrap_or(Color::TRANSPARENT),
        );
        pixmap.fill_rect(rect, &paint, tiny_skia::Transform::identity(), None);
    }

    // Clock, large, centered in the upper third.
    let clock_size = 64.0;
    let clock_w = text.measure_line(inputs.clock_text, inputs.font_family, clock_size);
    text.draw_line(
        &mut pixmap,
        inputs.clock_text,
        inputs.font_family,
        clock_size,
        faded(Color::WHITE, alpha),
        (w - clock_w) / 2.0,
        h * 0.28 + y_off,
    );

    // Password pill, centered; turns red while showing a failed attempt.
    let pill_w = 280.0_f32.min(w - tokens::SPACE_XL as f32 * 2.0);
    let pill_h = 48.0;
    let pill_x = (w - pill_w) / 2.0;
    let pill_y = h * 0.5 + y_off;

    if let Some(path) = rounded_rect(
        pill_x,
        pill_y,
        pill_w,
        pill_h,
        tokens::RADIUS_SECONDARY as f32,
    ) {
        let mut paint = Paint::default();
        paint.set_color(if inputs.failed {
            red_color
        } else {
            surface_color
        });
        paint.anti_alias = true;
        pixmap.fill_path(
            &path,
            &paint,
            tiny_skia::FillRule::Winding,
            tiny_skia::Transform::identity(),
            None,
        );
    }

    // Password dots — one filled circle per typed character, capped so a
    // very long password can't overflow the pill.
    let dot_r = 5.0;
    let dot_gap = 18.0;
    let max_dots = ((pill_w - tokens::SPACE_LG as f32 * 2.0) / dot_gap)
        .floor()
        .max(1.0) as usize;
    let shown_dots = inputs.password_len.min(max_dots);
    if shown_dots > 0 {
        let dots_w = (shown_dots as f32 - 1.0).max(0.0) * dot_gap;
        let start_x = pill_x + (pill_w - dots_w) / 2.0;
        let dot_y = pill_y + pill_h / 2.0;
        for i in 0..shown_dots {
            if let Some(path) =
                tiny_skia::PathBuilder::from_circle(start_x + i as f32 * dot_gap, dot_y, dot_r)
            {
                let mut paint = Paint::default();
                paint.set_color(if inputs.failed {
                    faded(Color::WHITE, alpha)
                } else {
                    accent_color
                });
                paint.anti_alias = true;
                pixmap.fill_path(
                    &path,
                    &paint,
                    tiny_skia::FillRule::Winding,
                    tiny_skia::Transform::identity(),
                    None,
                );
            }
        }
    }

    // Status line (e.g. "wrong password" / "checking…") below the pill.
    if let Some(status) = inputs.status_text {
        let status_size = tokens::FONT_SIZE_SECONDARY as f32;
        let status_w = text.measure_line(status, inputs.font_family, status_size);
        text.draw_line(
            &mut pixmap,
            status,
            inputs.font_family,
            status_size,
            on_surface,
            (w - status_w) / 2.0,
            pill_y + pill_h + tokens::SPACE_MD as f32,
        );
    }

    Some(pixmap)
}

/// Copies a composed frame into a `wl_shm` `Argb8888` buffer, swizzling
/// tiny-skia's RGBA byte order to the host-endian `0xAARRGGBB` `wl_shm`
/// expects (BGRA bytes on little-endian, which is every target this ships
/// on).
pub fn blit_to_shm(pixmap: &Pixmap, shm_bytes: &mut [u8]) {
    for (src, dst) in pixmap.pixels().iter().zip(shm_bytes.chunks_exact_mut(4)) {
        dst[0] = src.blue();
        dst[1] = src.green();
        dst[2] = src.red();
        dst[3] = src.alpha();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blit_swizzles_rgba_to_bgra() {
        let mut pixmap = Pixmap::new(1, 1).unwrap();
        pixmap.fill(Color::from_rgba8(10, 20, 30, 255));
        let mut shm = vec![0u8; 4];
        blit_to_shm(&pixmap, &mut shm);
        assert_eq!(shm, vec![30, 20, 10, 255]);
    }

    #[test]
    fn compose_handles_empty_password_and_no_status() {
        let bg = Background::Color(Color::BLACK);
        let palette = breadlock_ui::theme::Palette::default();
        let mut text = TextRenderer::new();
        let inputs = FrameInputs {
            width: 400,
            height: 300,
            background: &bg,
            palette: &palette,
            font_family: "sans-serif",
            clock_text: "12:34",
            password_len: 0,
            failed: false,
            status_text: None,
            appear_t: 1.0,
            unlock_t: 0.0,
        };
        let pixmap = compose(&mut text, &inputs).unwrap();
        assert_eq!((pixmap.width(), pixmap.height()), (400, 300));
    }

    #[test]
    fn ease_out_cubic_bounds_and_shape() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        assert_eq!(ease_out_cubic(-1.0), 0.0);
        assert_eq!(ease_out_cubic(2.0), 1.0);
        // Ease-out sits above the linear diagonal in the middle of the curve.
        assert!(ease_out_cubic(0.5) > 0.5);
    }

    #[test]
    fn overlay_motion_appear_starts_below_and_fades_in() {
        let (a0, y0) = overlay_motion(0.0, 0.0);
        assert_eq!(a0, 0.0);
        assert!(y0 > 0.0, "clock/pill should start below rest, got y={y0}");

        let (a1, y1) = overlay_motion(1.0, 0.0);
        assert_eq!(a1, 1.0);
        assert_eq!(y1, 0.0);
    }

    #[test]
    fn overlay_motion_unlock_fades_out_and_drifts_up() {
        let (a, y) = overlay_motion(1.0, 1.0);
        assert_eq!(a, 0.0);
        assert!(y < 0.0, "unlock should drift up from rest, got y={y}");
    }
}

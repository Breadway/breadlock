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
use tiny_skia::{Color, Paint, Pixmap};

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
}

/// Composes one frame. Returns `None` only if `width`/`height` are degenerate
/// (a `0x0` `configure`, which some compositors send transiently).
pub fn compose(text: &mut TextRenderer, inputs: &FrameInputs) -> Option<Pixmap> {
    let mut pixmap = Pixmap::new(inputs.width, inputs.height)?;
    inputs.background.paint(&mut pixmap);

    let (w, h) = (inputs.width as f32, inputs.height as f32);
    let surface_color = tiny_skia_color(&inputs.palette.color0);
    let accent_color = tiny_skia_color(&inputs.palette.color4);
    let on_surface = tiny_skia_color(breadlock_ui::theme::ink_on(&inputs.palette.color0));
    let red_color = tiny_skia_color(&inputs.palette.color1);

    // Clock, large, centered in the upper third.
    let clock_size = 64.0;
    let clock_w = text.measure_line(inputs.clock_text, inputs.font_family, clock_size);
    text.draw_line(
        &mut pixmap,
        inputs.clock_text,
        inputs.font_family,
        clock_size,
        Color::WHITE,
        (w - clock_w) / 2.0,
        h * 0.28,
    );

    // Password pill, centered; turns red while showing a failed attempt.
    let pill_w = 280.0_f32.min(w - tokens::SPACE_XL as f32 * 2.0);
    let pill_h = 48.0;
    let pill_x = (w - pill_w) / 2.0;
    let pill_y = h * 0.5;

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
                    Color::WHITE
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
        };
        let pixmap = compose(&mut text, &inputs).unwrap();
        assert_eq!((pixmap.width(), pixmap.height()), (400, 300));
    }
}

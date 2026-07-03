//! Lock-screen background: a solid palette color, or a static image scaled
//! to cover the surface. Live blur-of-desktop (hyprlock-style) is a v2
//! follow-up (see README) — `blur = true` is accepted but only logs a
//! warning in v1.

use breadlock_ui::config::{Background as BackgroundConfig, BackgroundMode};
use tiny_skia::{Pixmap, PixmapPaint, Transform};

pub enum Background {
    Color(tiny_skia::Color),
    Image(Pixmap),
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
                    Ok(pixmap) => Background::Image(pixmap),
                    Err(err) => {
                        tracing::warn!(path = %cfg.path, %err, "failed to load background image (PNG only in v1), falling back to palette color");
                        fallback()
                    }
                }
            }
        }
    }

    /// Paints this background into `target`, cover-fit (scaled uniformly to
    /// fill the surface, cropping any overflow — never letterboxed).
    pub fn paint(&self, target: &mut Pixmap) {
        match self {
            Background::Color(c) => target.fill(*c),
            Background::Image(source) => {
                let (tw, th) = (target.width() as f32, target.height() as f32);
                let (sw, sh) = (source.width() as f32, source.height() as f32);
                if sw <= 0.0 || sh <= 0.0 {
                    return;
                }
                let scale = (tw / sw).max(th / sh);
                target.fill(tiny_skia::Color::BLACK);
                target.draw_pixmap(
                    0,
                    0,
                    source.as_ref(),
                    &PixmapPaint::default(),
                    Transform::from_scale(scale, scale),
                    None,
                );
            }
        }
    }
}

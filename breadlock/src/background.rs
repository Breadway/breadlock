//! Lock-screen background: a solid palette color, or a static image scaled
//! to cover the surface. Live blur-of-desktop (hyprlock-style) is a v2
//! follow-up (see README) — `blur = true` is accepted but only logs a
//! warning in v1. `ken_burns = true` adds a slow, continuous pan+zoom to
//! image backgrounds (opt-in: it keeps the background redrawing at a low
//! frame rate while locked).

use breadlock_ui::config::{Background as BackgroundConfig, BackgroundMode};
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
    /// `(source, ken_burns)` — the flag decides whether `paint` pans over
    /// time or draws statically.
    Image(Pixmap, bool),
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
                    Ok(pixmap) => Background::Image(pixmap, cfg.ken_burns),
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
        matches!(self, Background::Image(_, true))
    }

    /// Paints this background into `target`, cover-fit (scaled uniformly to
    /// fill the surface, cropping any overflow — never letterboxed). `t_secs`
    /// is the monotonic clock: with Ken Burns enabled the image slowly pans
    /// and zooms along a smooth Lissajous-ish drift, so consecutive frames
    /// differ slightly but never jump.
    pub fn paint(&self, target: &mut Pixmap, t_secs: f32) {
        match self {
            Background::Color(c) => target.fill(*c),
            Background::Image(source, ken_burns) => {
                let (tw, th) = (target.width() as f32, target.height() as f32);
                let (sw, sh) = (source.width() as f32, source.height() as f32);
                if sw <= 0.0 || sh <= 0.0 {
                    return;
                }
                let cover = (tw / sw).max(th / sh);
                let (scale, tx, ty) = if *ken_burns {
                    let scale = cover * KENBURNS_ZOOM;
                    // Pan range: how far the scaled image overhangs each axis.
                    let pan_x = (sw * scale - tw).max(0.0);
                    let pan_y = (sh * scale - th).max(0.0);
                    let phase = t_secs * TAU / KENBURNS_PERIOD_S;
                    // Sin/cos offset by a quarter cycle: the pan traces a slow
                    // ellipse, starting from a corner.
                    (
                        scale,
                        -pan_x * (0.5 + 0.5 * phase.sin()),
                        -pan_y * (0.5 + 0.5 * phase.cos()),
                    )
                } else {
                    (cover, 0.0, 0.0)
                };
                target.fill(tiny_skia::Color::BLACK);
                target.draw_pixmap(
                    0,
                    0,
                    source.as_ref(),
                    &PixmapPaint::default(),
                    // scale first (image coords → scaled), then translate into
                    // the pan position.
                    Transform::from_translate(tx, ty).pre_concat(Transform::from_scale(scale, scale)),
                    None,
                );
            }
        }
    }
}

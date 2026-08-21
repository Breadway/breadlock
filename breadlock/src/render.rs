//! Frame composition: paints one full lock-screen frame (background, clock,
//! date, password pill, dots/caret, status line) into a `tiny_skia::Pixmap`,
//! then copies it into a Wayland `wl_shm` buffer.
//!
//! Motion: every effect is driven by raw 0..1 progress inputs (see
//! [`FrameInputs`]) that `state.rs` computes from timestamps; this module only
//! turns progress into pixels, so the whole timeline is unit-testable off a
//! compositor (see `breadlock-preview`, which renders frames to PNG).
//!
//! tiny-skia's in-memory pixel format is byte-order RGBA; `wl_shm`'s
//! `Argb8888` format is host-endian `0xAARRGGBB`, i.e. byte-order BGRA on
//! little-endian machines. [`blit_to_shm`] does the swizzle.

use crate::background::Background;
use breadlock_ui::painter::{rounded_rect, tokens, TextRenderer};
use breadlock_ui::theme::tiny_skia_color;
use std::f32::consts::PI;
use std::time::Instant;
use tiny_skia::{Color, Paint, Pixmap, Rect, Transform};

/// Lock-appear duration: elements ease in on a small stagger (see the
/// `*_DELAY_MS` consts) instead of one uniform fade.
pub const APPEAR_MS: u64 = 450;
/// Total unlock duration: [`FLASH_MS`] of green success flash, then a
/// fade-out with a slight upward drift.
pub const UNLOCK_MS: u64 = 650;
/// Green success-flash phase at the start of the unlock.
pub const FLASH_MS: u64 = 250;
/// Wrong-password shake duration (the red pill stays up until
/// `input.fail_timeout_ms`, which outlives the shake).
pub const SHAKE_MS: u64 = 380;
/// Newest password-dot pop-in duration.
pub const DOT_POP_MS: u64 = 200;
/// Minute-rollover crossfade duration.
pub const CLOCK_CROSSFADE_MS: u64 = 300;
/// Redraw cadence while any fast animation is in flight (~60 Hz).
pub const ANIM_FRAME_MS: u64 = 16;
/// Cadence while only slow effects are running (idle breath, Ken Burns pan).
pub const SLOW_FRAME_MS: u64 = 62;
/// Status-line slide-in duration ("Checking…" / "Wrong password" rise in).
pub const STATUS_SLIDE_MS: u64 = 200;
/// Idle breathing (config `animation.breathe`): a subtle glow pulse on the
/// pill every few seconds. Only the active window redraws (low-duty-cycle
/// timer in state.rs), so idle CPU stays near zero.
pub const BREATHE_PERIOD_MS: u64 = 4000;
pub const BREATHE_ACTIVE_MS: u64 = 1200;
/// How long after the lock appears before the first breath.
pub const BREATHE_INITIAL_DELAY_MS: u64 = 1500;

const APPEAR_SLIDE_PX: f32 = 28.0;
const UNLOCK_DRIFT_PX: f32 = 20.0;
/// Parallax: during the unlock fade each element drifts up at a slightly
/// different speed (clock furthest, status least) for a sense of depth.
const DRIFT_CLOCK: f32 = 1.25;
const DRIFT_DATE: f32 = 1.25;
const DRIFT_PILL: f32 = 1.0;
const DRIFT_STATUS: f32 = 0.8;
/// Peak glow multiplier added to the pill shadow during an idle breath, and
/// the accent-ring alpha at the breath peak (sketch's `breathe` keyframes).
const BREATHE_GLOW: f32 = 0.6;
const BREATHE_RING_ALPHA: f32 = 0.12;
/// How far the status line rises during its slide-in.
const STATUS_SLIDE_PX: f32 = 8.0;
/// Dim veil over the wallpaper: a vertical gradient, darker at the top so
/// the clock (in the upper third) sits on the deepest tone.
const DIM_ALPHA_TOP: f32 = 0.34;
const DIM_ALPHA_BOTTOM: f32 = 0.16;
/// Pill hairline-border alpha (sketch: `1px solid rgba(255,255,255,.08)`).
const PILL_BORDER_ALPHA: f32 = 0.10;
/// Fake drop-shadow layers under the pill (tiny-skia has no blur filter):
/// `(grow_px, black_alpha)` — a few concentric copies at fading alpha read
/// as a soft shadow when drawn under the fill.
const PILL_SHADOW: [(f32, f32); 3] = [(2.5, 0.20), (5.0, 0.11), (7.5, 0.05)];
/// Clock/date scale with the surface, clamped to the sketch's CSS ranges
/// (`clamp(34px, 7.5vw, 60px)` and `clamp(11px, 1.6vw, 14px)`).
const CLOCK_SIZE_MIN: f32 = 34.0;
const CLOCK_SIZE_MAX: f32 = 60.0;
const DATE_SIZE_MIN: f32 = 11.0;
const DATE_SIZE_MAX: f32 = 14.0;
/// Fraction of the shake window over which the pill tints to red (smooth
/// transition instead of an instant color swap).
const SHAKE_RED_FRAC: f32 = 150.0 / SHAKE_MS as f32;
/// Fraction of the unlock window that is the green flash.
const FLASH_FRAC: f32 = FLASH_MS as f32 / UNLOCK_MS as f32;
/// Entrance stagger: each element's appear starts this many ms into the
/// `APPEAR_MS` window, so the clock leads and the status trails.
const CLOCK_DELAY_MS: u64 = 0;
const DATE_DELAY_MS: u64 = 80;
const PILL_DELAY_MS: u64 = 100;
const STATUS_DELAY_MS: u64 = 160;
/// Dot/caret geometry. Dot diameter matches the sketch's 6–9px range.
const DOT_R: f32 = 4.5;
const DOT_GAP: f32 = 18.0;
const CARET_W: f32 = 2.0;
/// Seconds between caret blinks while idle; solid for the first `CARET_HOLD_S`
/// after a keystroke (terminal-style).
const CARET_BLINK_HZ: f32 = 1.8;
const CARET_HOLD_S: f32 = 0.5;

pub struct FrameInputs<'a> {
    pub width: u32,
    pub height: u32,
    pub background: &'a Background,
    pub palette: &'a breadlock_ui::theme::Palette,
    pub font_family: &'a str,
    pub clock_text: &'a str,
    /// Date line under the clock. Empty string hides it.
    pub date_text: &'a str,
    /// Minute-rollover crossfade: `(previous clock text, raw 0..1 progress)`.
    pub clock_old: Option<(&'a str, f32)>,
    pub password_len: usize,
    /// True while showing a failed attempt (red pill + shake + red status).
    pub failed: bool,
    /// Raw 0..1 progress of the wrong-password shake. 0 when not failed.
    pub failed_t: f32,
    /// Raw 0..1 progress of the newest dot's pop-in. 1 when no pop is live.
    pub dot_pop_t: f32,
    /// Seconds since the most recent keystroke (caret solid/blink behavior).
    pub keystroke_age: Option<f32>,
    /// Monotonic seconds since app start (idle caret blink cadence, Ken Burns
    /// pan phase).
    pub t_secs: f32,
    /// Idle-breathing envelope: 0 when no breath is active, ramping 0..1..0
    /// (one sine hump) over the active window. Scales the pill's glow.
    pub breathe_t: f32,
    /// Status-line slide-in progress (0..1, 1 settled).
    pub status_t: f32,
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

/// Ease-out-back: overshoots past 1 (~5%) then settles — used for the pill's
/// entrance scale so it pops instead of sliding.
pub fn ease_out_back(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    let c1 = 1.70158;
    let c3 = c1 + 1.0;
    1.0 + c3 * (t - 1.0).powi(3) + c1 * (t - 1.0).powi(2)
}

/// Horizontal shake offset in px for raw progress `t` in 0..1 — a damped
/// sinusoid that starts and ends at rest.
pub fn damped_shake_x(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    let env = (1.0 - t) * (1.0 - t);
    env * 9.0 * (PI * 5.5 * t).sin()
}

/// Linear 0..1 progress since `started` over `duration_ms`.
pub fn unit_progress(started: Instant, duration_ms: u64) -> f32 {
    let dur = duration_ms as f32 / 1000.0;
    if dur <= 0.0 {
        return 1.0;
    }
    (started.elapsed().as_secs_f32() / dur).clamp(0.0, 1.0)
}

/// Idle-breath envelope: a single sine hump over the active window (0 at the
/// start and end, 1 at the peak).
pub fn breathe_envelope(t: f32) -> f32 {
    (PI * t.clamp(0.0, 1.0)).sin()
}

/// Appear progress for a single staggered element: raw overall progress
/// `appear_t` is spread over the `APPEAR_MS` window; the element only starts
/// moving `delay_ms` in.
fn staggered_t(appear_t: f32, delay_ms: u64) -> f32 {
    if APPEAR_MS <= delay_ms {
        return appear_t;
    }
    let window = (APPEAR_MS - delay_ms) as f32;
    ((appear_t * APPEAR_MS as f32 - delay_ms as f32) / window).clamp(0.0, 1.0)
}

/// Overlay alpha and y-offset (positive is down) from raw 0..1 progress —
/// used for the full-screen dim veil, which fades with the whole chrome.
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

fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: f32, y: f32| x + (y - x) * t;
    Color::from_rgba(
        mix(a.red(), b.red()),
        mix(a.green(), b.green()),
        mix(a.blue(), b.blue()),
        mix(a.alpha(), b.alpha()),
    )
    .unwrap_or(a)
}

/// Composes one frame. Returns `None` only if `width`/`height` are degenerate
/// (a `0x0` `configure`, which some compositors send transiently).
pub fn compose(text: &mut TextRenderer, inputs: &FrameInputs) -> Option<Pixmap> {
    let mut pixmap = Pixmap::new(inputs.width, inputs.height)?;
    inputs.background.paint(&mut pixmap, inputs.t_secs);

    // Overall chrome fade: appear eased in, unlock eased out. The unlock
    // `fade` multiplies every element below.
    let unlock = ease_out_cubic(inputs.unlock_t);
    let fade = 1.0 - unlock;
    let (veil_alpha, _) = overlay_motion(inputs.appear_t, inputs.unlock_t);
    if veil_alpha <= 0.0 {
        return Some(pixmap);
    }

    let (w, h) = (inputs.width as f32, inputs.height as f32);
    let surface_color = faded(tiny_skia_color(&inputs.palette.color0), veil_alpha);
    let accent_color = faded(tiny_skia_color(&inputs.palette.color4), veil_alpha);
    let green_color = faded(tiny_skia_color(&inputs.palette.color2), veil_alpha);
    let on_surface = faded(
        tiny_skia_color(breadlock_ui::theme::ink_on(&inputs.palette.color0)),
        veil_alpha,
    );
    let red_color = faded(tiny_skia_color(&inputs.palette.color1), veil_alpha);

    // Translucent veil over the (static) wallpaper — a vertical gradient
    // (deeper at the top) that fades in with the chrome.
    if let Some(shader) = tiny_skia::LinearGradient::new(
        tiny_skia::Point::from_xy(0.0, 0.0),
        tiny_skia::Point::from_xy(0.0, h),
        vec![
            tiny_skia::GradientStop::new(
                0.0,
                Color::from_rgba(0.0, 0.0, 0.0, DIM_ALPHA_TOP * veil_alpha)
                    .unwrap_or(Color::TRANSPARENT),
            ),
            tiny_skia::GradientStop::new(
                1.0,
                Color::from_rgba(0.0, 0.0, 0.0, DIM_ALPHA_BOTTOM * veil_alpha)
                    .unwrap_or(Color::TRANSPARENT),
            ),
        ],
        tiny_skia::SpreadMode::Pad,
        Transform::identity(),
    ) {
        let mut paint = Paint::default();
        paint.shader = shader;
        if let Some(rect) = Rect::from_xywh(0.0, 0.0, w, h) {
            pixmap.fill_rect(rect, &paint, Transform::identity(), None);
        }
    }

    // Per-element staggered entrance.
    let clock_e = ease_out_cubic(staggered_t(inputs.appear_t, CLOCK_DELAY_MS));
    let date_e = ease_out_cubic(staggered_t(inputs.appear_t, DATE_DELAY_MS));
    let pill_t = staggered_t(inputs.appear_t, PILL_DELAY_MS);
    let pill_e = ease_out_cubic(pill_t);
    let pill_scale = ease_out_back(pill_t);
    let status_e = ease_out_cubic(staggered_t(inputs.appear_t, STATUS_DELAY_MS));
    // Per-element vertical motion: the appear part is uniform, the unlock
    // drift is scaled per element for parallax.
    let elem_y = |e: f32, drift: f32| APPEAR_SLIDE_PX * (1.0 - e) - UNLOCK_DRIFT_PX * unlock * drift;

    // ---- Clock, large, centered in the upper third (size scales with the
    // surface). A minute rollover crossfades old text out (drifting up) while
    // the new fades in from below.
    let clock_size = (w * 0.075).clamp(CLOCK_SIZE_MIN, CLOCK_SIZE_MAX);
    // Rest-pose anchors: every element drifts from its own rest position, so
    // the clock+date and pill+status clusters move as units. (Anchoring the
    // date/status to the already-drifted clock/pill *and* adding their own
    // `elem_y` would drift them twice, sliding them up into their anchors
    // during the unlock.)
    let clock_y_rest = h * 0.28;
    let clock_y = clock_y_rest + elem_y(clock_e, DRIFT_CLOCK);
    let clock_alpha = clock_e * fade;
    match inputs.clock_old {
        Some((old, t)) => {
            let t = t.clamp(0.0, 1.0);
            let old_w = text.measure_line(old, inputs.font_family, clock_size);
            text.draw_line(
                &mut pixmap,
                old,
                inputs.font_family,
                clock_size,
                faded(Color::WHITE, clock_alpha * (1.0 - t)),
                (w - old_w) / 2.0,
                clock_y - 6.0 * t,
            );
            let new_w = text.measure_line(inputs.clock_text, inputs.font_family, clock_size);
            text.draw_line(
                &mut pixmap,
                inputs.clock_text,
                inputs.font_family,
                clock_size,
                faded(Color::WHITE, clock_alpha * t),
                (w - new_w) / 2.0,
                clock_y + 6.0 * (1.0 - t),
            );
        }
        None => {
            let clock_w = text.measure_line(inputs.clock_text, inputs.font_family, clock_size);
            text.draw_line(
                &mut pixmap,
                inputs.clock_text,
                inputs.font_family,
                clock_size,
                faded(Color::WHITE, clock_alpha),
                (w - clock_w) / 2.0,
                clock_y,
            );
        }
    }

    // ---- Date line under the clock (hidden when date_text is empty). The
    // clock's `origin_y` anchors its *top*, so the date is placed below the
    // clock's actual glyph box (exact per font, cached) with a small gap.
    if !inputs.date_text.is_empty() {
        let date_size = (w * 0.016).clamp(DATE_SIZE_MIN, DATE_SIZE_MAX);
        let (clock_top, clock_height) =
            text.measure_box(inputs.clock_text, inputs.font_family, clock_size);
        let date_y = clock_y_rest + elem_y(date_e, DRIFT_DATE) + clock_top + clock_height
            + tokens::SPACE_SM as f32;
        let date_w = text.measure_line(inputs.date_text, inputs.font_family, date_size);
        text.draw_line(
            &mut pixmap,
            inputs.date_text,
            inputs.font_family,
            date_size,
            faded(Color::WHITE, date_e * fade * 0.82),
            (w - date_w) / 2.0,
            date_y,
        );
    }

    // ---- Password pill, centered. Red while failed (tinting in smoothly over
    // the first part of the shake), green during the success flash.
    let pill_w = 280.0_f32.min(w - tokens::SPACE_XL as f32 * 2.0);
    let pill_h = 48.0;
    let pill_x = (w - pill_w) / 2.0;
    let pill_y_rest = h * 0.5;
    let pill_y = pill_y_rest + elem_y(pill_e, DRIFT_PILL);
    let pill_alpha = pill_e * fade;
    // Idle breath: glow multiplier on the shadow/border (1 at rest, up to
    // 1 + BREATHE_GLOW at the breath peak).
    let breathe = 1.0 + BREATHE_GLOW * inputs.breathe_t;

    let base_pill = if inputs.failed {
        lerp_color(surface_color, red_color, (inputs.failed_t / SHAKE_RED_FRAC).clamp(0.0, 1.0))
    } else {
        surface_color
    };
    let pill_color = if inputs.unlock_t > 0.0 {
        green_color
    } else {
        base_pill
    };
    // The pill scales about its center (ease-out-back overshoot) instead of
    // rising like the text; while unlocking it stays at rest scale.
    let scale = if inputs.unlock_t > 0.0 { 1.0 } else { pill_scale };
    let shake_x = if inputs.failed { damped_shake_x(inputs.failed_t) } else { 0.0 };
    let cx = pill_x + pill_w / 2.0;
    let cy = pill_y + pill_h / 2.0;
    let pill_xf = Transform::from_row(
        scale,
        0.0,
        0.0,
        scale,
        cx * (1.0 - scale) + shake_x,
        cy * (1.0 - scale),
    );

    if let Some(path) =
        rounded_rect(pill_x, pill_y, pill_w, pill_h, tokens::RADIUS_SECONDARY as f32)
    {
        // Soft drop shadow first (under the fill): concentric expanded copies
        // offset downward at fading alpha. The idle breath scales the glow.
        for (grow, alpha) in PILL_SHADOW {
            if let Some(shadow_path) = rounded_rect(
                pill_x - grow,
                pill_y - grow + 3.0,
                pill_w + grow * 2.0,
                pill_h + grow * 2.0,
                tokens::RADIUS_SECONDARY as f32 + grow,
            ) {
                let mut paint = Paint::default();
                paint.set_color(faded(Color::BLACK, alpha * pill_alpha * breathe));
                paint.anti_alias = true;
                pixmap.fill_path(
                    &shadow_path,
                    &paint,
                    tiny_skia::FillRule::Winding,
                    pill_xf,
                    None,
                );
            }
        }

        let mut paint = Paint::default();
        paint.set_color(faded(pill_color, pill_alpha));
        paint.anti_alias = true;
        pixmap.fill_path(
            &path,
            &paint,
            tiny_skia::FillRule::Winding,
            pill_xf,
            None,
        );

        // Hairline border for depth — dropped on the wrong/success states
        // (the sketch sets `border-color: transparent` there).
        if !inputs.failed && inputs.unlock_t == 0.0 {
            let mut stroke = tiny_skia::Stroke::default();
            stroke.width = 1.0;
            let mut paint = Paint::default();
            paint.set_color(faded(
                Color::WHITE,
                PILL_BORDER_ALPHA * pill_alpha * (1.0 + 0.4 * inputs.breathe_t),
            ));
            pixmap.stroke_path(&path, &paint, &stroke, pill_xf, None);
        }

        // Idle breath: a faint accent ring blooms around the pill at the
        // breath peak (matches the sketch's `breathe` keyframes).
        if inputs.breathe_t > 0.0 && !inputs.failed && inputs.unlock_t == 0.0 {
            let mut stroke = tiny_skia::Stroke::default();
            stroke.width = 1.5;
            let mut paint = Paint::default();
            paint.set_color(faded(accent_color, BREATHE_RING_ALPHA * inputs.breathe_t * pill_alpha));
            pixmap.stroke_path(&path, &paint, &stroke, pill_xf, None);
        }

        // Success flash: expanding accent ring around the pill for the first
        // `FLASH_MS` of the unlock.
        if inputs.unlock_t > 0.0 && inputs.unlock_t < FLASH_FRAC {
            let flash_t = inputs.unlock_t / FLASH_FRAC;
            let mut stroke = tiny_skia::Stroke::default();
            stroke.width = 2.0 + 16.0 * flash_t;
            let mut paint = Paint::default();
            paint.set_color(faded(green_color, 0.55 * (1.0 - flash_t) * pill_alpha));
            pixmap.stroke_path(&path, &paint, &stroke, pill_xf, None);
        }
    }

    // ---- Password dots — one filled circle per typed character, capped so a
    // very long password can't overflow the pill. The newest dot pops in with
    // an overshoot; the rest sit at rest size.
    let max_dots = ((pill_w - tokens::SPACE_LG as f32 * 2.0) / DOT_GAP)
        .floor()
        .max(1.0) as usize;
    let shown_dots = inputs.password_len.min(max_dots);
    let dot_y = pill_y + pill_h / 2.0;
    if shown_dots > 0 {
        let start_x = start_x_for(shown_dots, pill_x, pill_w);
        for i in 0..shown_dots {
            let newest = i == shown_dots - 1;
            let r = if newest && inputs.dot_pop_t < 1.0 {
                (DOT_R * ease_out_back(inputs.dot_pop_t)).max(0.4)
            } else {
                DOT_R
            };
            // Success: dots flip accent → white in a quick left-to-right
            // cascade over the green flash (each dot finishes (i+1)/n through
            // the flash), instead of all flipping at once.
            let dot_color = if inputs.failed {
                Color::WHITE
            } else if inputs.unlock_t > 0.0 {
                let flash_t = (inputs.unlock_t / FLASH_FRAC).clamp(0.0, 1.0);
                let cascade = (flash_t * shown_dots as f32 - i as f32).clamp(0.0, 1.0);
                lerp_color(accent_color, Color::WHITE, cascade)
            } else {
                accent_color
            };
            if let Some(path) =
                tiny_skia::PathBuilder::from_circle(start_x + i as f32 * DOT_GAP, dot_y, r)
            {
                let mut paint = Paint::default();
                paint.set_color(faded(dot_color, pill_alpha));
                paint.anti_alias = true;
                pixmap.fill_path(
                    &path,
                    &paint,
                    tiny_skia::FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
        }
    }

    // ---- Empty pill: a centered "enter password" hint instead of dots. The
    // caret only appears with the first typed character, so the pill reads as
    // an input field rather than an empty dark bar. Centered on the exact
    // glyph box (origin anchors the text top, not the baseline).
    if shown_dots == 0 && !inputs.failed {
        let hint = "Enter password";
        let hint_size = tokens::FONT_SIZE_BASE as f32;
        let hint_w = text.measure_line(hint, inputs.font_family, hint_size);
        let (hint_top, hint_height) = text.measure_box(hint, inputs.font_family, hint_size);
        let hint_y = pill_y + (pill_h - hint_height) / 2.0 - hint_top;
        text.draw_line(
            &mut pixmap,
            hint,
            inputs.font_family,
            hint_size,
            faded(on_surface, pill_alpha * 0.5),
            (w - hint_w) / 2.0,
            hint_y,
        );
    } else if shown_dots > 0 {
        // ---- Caret after the last dot: solid for half a second after a
        // keystroke, then blinking at ~1.8 Hz.
        let caret_x = start_x_for(shown_dots, pill_x, pill_w)
            + (shown_dots - 1) as f32 * DOT_GAP
            + DOT_R
            + 6.0;
        let blink = match inputs.keystroke_age {
            Some(age) if age < CARET_HOLD_S => 1.0,
            Some(age) => ((age - CARET_HOLD_S) * CARET_BLINK_HZ) % 1.0,
            None => (inputs.t_secs * CARET_BLINK_HZ) % 1.0,
        };
        if blink < 0.5 {
            let caret_color = if inputs.failed || inputs.unlock_t > 0.0 {
                Color::WHITE
            } else {
                accent_color
            };
            let caret_h = pill_h * 0.5;
            if let Some(path) = rounded_rect(
                caret_x,
                dot_y - caret_h / 2.0,
                CARET_W,
                caret_h,
                1.0,
            ) {
                let mut paint = Paint::default();
                paint.set_color(faded(caret_color, pill_alpha));
                paint.anti_alias = true;
                pixmap.fill_path(
                    &path,
                    &paint,
                    tiny_skia::FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
        }
    }

    // ---- Status line below the pill (e.g. "wrong password" / "checking…").
    // Slides up 8px with a fade when it appears (state.rs resets `status_t`
    // on every auth-state change).
    if let Some(status) = inputs.status_text {
        let status_size = tokens::FONT_SIZE_SECONDARY as f32;
        let status_w = text.measure_line(status, inputs.font_family, status_size);
        let status_anim = ease_out_cubic(inputs.status_t);
        let status_alpha = status_e * fade * status_anim;
        let color = if inputs.failed { red_color } else { on_surface };
        text.draw_line(
            &mut pixmap,
            status,
            inputs.font_family,
            status_size,
            faded(color, status_alpha),
            (w - status_w) / 2.0,
            pill_y_rest + pill_h + tokens::SPACE_MD as f32 + elem_y(status_e, DRIFT_STATUS)
                + STATUS_SLIDE_PX * (1.0 - status_anim),
        );
    }

    Some(pixmap)
}

/// Recomputes the left edge of the dot row (shared by the dot loop and the
/// caret placement — kept out of `compose` to avoid a long-lived binding).
fn start_x_for(shown_dots: usize, pill_x: f32, pill_w: f32) -> f32 {
    let dots_w = (shown_dots as f32 - 1.0).max(0.0) * DOT_GAP;
    pill_x + (pill_w - dots_w) / 2.0
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

    fn inputs<'a>(
        bg: &'a Background,
        palette: &'a breadlock_ui::theme::Palette,
        text: &'a str,
        date: &'a str,
        password_len: usize,
        failed: bool,
        failed_t: f32,
        dot_pop_t: f32,
        appear_t: f32,
        unlock_t: f32,
    ) -> FrameInputs<'a> {
        FrameInputs {
            width: 400,
            height: 300,
            background: bg,
            palette,
            font_family: "sans-serif",
            clock_text: text,
            date_text: date,
            clock_old: None,
            password_len,
            failed,
            failed_t,
            dot_pop_t,
            keystroke_age: None,
            t_secs: 0.0,
            breathe_t: 0.0,
            status_t: 1.0,
            status_text: None,
            appear_t,
            unlock_t,
        }
    }

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
            date_text: "Friday · Aug 21",
            clock_old: None,
            password_len: 0,
            failed: false,
            failed_t: 0.0,
            dot_pop_t: 1.0,
            keystroke_age: None,
            t_secs: 0.0,
            breathe_t: 0.0,
            status_t: 1.0,
            status_text: None,
            appear_t: 1.0,
            unlock_t: 0.0,
        };
        let pixmap = compose(&mut text, &inputs).unwrap();
        assert_eq!((pixmap.width(), pixmap.height()), (400, 300));
    }

    #[test]
    fn compose_renders_failed_and_unlock_states() {
        let bg = Background::Color(Color::BLACK);
        let palette = breadlock_ui::theme::Palette::default();
        let mut text = TextRenderer::new();
        // Wrong-password shake mid-flight.
        let failed = inputs(&bg, &palette, "12:34", "Friday · Aug 21", 4, true, 0.3, 0.4, 1.0, 0.0);
        assert!(compose(&mut text, &failed).is_some());
        // Success flash phase of the unlock.
        let success = inputs(&bg, &palette, "12:34", "Friday · Aug 21", 4, false, 0.0, 1.0, 1.0, 0.12);
        assert!(compose(&mut text, &success).is_some());
        // Fully faded unlock returns just the background.
        let done = inputs(&bg, &palette, "12:34", "Friday · Aug 21", 4, false, 0.0, 1.0, 1.0, 1.0);
        assert!(compose(&mut text, &done).is_some());
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
    fn ease_out_back_overshoots_past_one() {
        assert_eq!(ease_out_back(0.0), 0.0);
        assert_eq!(ease_out_back(1.0), 1.0);
        assert!(
            (0..=20).any(|i| ease_out_back(i as f32 / 20.0) > 1.0),
            "ease-out-back must overshoot past 1 somewhere in (0, 1)"
        );
    }

    #[test]
    fn damped_shake_starts_and_ends_at_rest_and_stays_bounded() {
        assert_eq!(damped_shake_x(0.0), 0.0);
        assert_eq!(damped_shake_x(1.0), 0.0);
        for i in 0..=40 {
            let x = damped_shake_x(i as f32 / 40.0);
            assert!(
                x.abs() < 9.5,
                "shake amplitude must stay bounded, got {x} at t={}",
                i as f32 / 40.0
            );
        }
    }

    #[test]
    fn staggered_t_spreads_elements_across_the_window() {
        // Clock (delay 0) starts immediately; pill (delay 100ms of 450ms)
        // only begins after ~22% of the window.
        assert_eq!(staggered_t(0.0, CLOCK_DELAY_MS), 0.0);
        assert_eq!(staggered_t(0.0, PILL_DELAY_MS), 0.0);
        assert!(staggered_t(0.1, CLOCK_DELAY_MS) > 0.0);
        assert_eq!(staggered_t(0.1, PILL_DELAY_MS), 0.0);
        assert_eq!(staggered_t(1.0, PILL_DELAY_MS), 1.0);
        // Monotonic: later raw progress never regresses an element.
        let mut prev = 0.0f32;
        for i in 0..=20 {
            let t = staggered_t(i as f32 / 20.0, STATUS_DELAY_MS);
            assert!(t >= prev, "staggered progress regressed: {t} < {prev}");
            prev = t;
        }
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

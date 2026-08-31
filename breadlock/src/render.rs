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
use breadlock_ui::painter::{rounded_rect, tokens, TextRenderer, Weight};
use breadlock_ui::theme::tiny_skia_color;
use std::f32::consts::PI;
use std::time::Instant;
use tiny_skia::{Color, Paint, Pixmap, Transform};

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
/// the clock (in the upper third) sits on the deepest tone. `pub(crate)` for
/// the GPU background shader, which applies the same gradient.
pub(crate) const DIM_ALPHA_TOP: f32 = 0.34;
pub(crate) const DIM_ALPHA_BOTTOM: f32 = 0.16;

/// Darkens a full-screen pixmap with the vertical dim veil, in place:
/// premultiplied pixels scale by `1 - lerp(DIM_ALPHA_TOP, DIM_ALPHA_BOTTOM,
/// y/h) * veil_alpha` (equivalent to blending a black gradient over it). A
/// single pass over the surface — the software renderer's largest recurring
/// cost was the full-screen gradient fill/blit, so this keeps it cheap.
fn dim_rows(pixmap: &mut Pixmap, veil_alpha: f32) {
    let w = pixmap.width() as usize;
    let h = pixmap.height() as usize;
    let data = pixmap.data_mut();
    for y in 0..h {
        let a = (DIM_ALPHA_TOP + (DIM_ALPHA_BOTTOM - DIM_ALPHA_TOP) * (y as f32 / h as f32))
            * veil_alpha;
        let k = 1.0 - a;
        let row = y * w * 4;
        for px in 0..w {
            let i = row + px * 4;
            data[i] = (data[i] as f32 * k) as u8;
            data[i + 1] = (data[i + 1] as f32 * k) as u8;
            data[i + 2] = (data[i + 2] as f32 * k) as u8;
            // Keep alpha: the GPU veil shader writes A=1, and scaling A here
            // made the software path translucent against a compositor that
            // expected an opaque lock surface.
        }
    }
}
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
    /// The actual password text — only read when `reveal` is true (hold-to-
    /// reveal renders the plain characters instead of dots).
    pub password: &'a str,
    /// True while the reveal key (Tab) is held — dots render as the plain
    /// characters.
    pub reveal: bool,
    /// Caps Lock state — shows the caps chip when on.
    pub caps_lock: bool,
    /// Active keyboard layout index — shown next to the caps chip when non-0.
    pub layout_index: u32,
    /// Idle auto-dim progress 0..1 (0 = disabled/not idle) — deepens the dim
    /// veil after `animation.idle_dim_after_secs` of no keystrokes.
    pub idle_dim: f32,
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
    /// Now-playing / battery line under the clock (D-Bus status). Empty
    /// string hides it.
    pub info_text: &'a str,
    /// Raw 0..1 lock-appear progress (pre-ease). 1 is rest pose.
    pub appear_t: f32,
    /// Raw 0..1 unlock-fade progress (pre-ease). 0 when not unlocking.
    pub unlock_t: f32,
    /// Sub-pixel bilinear panning for the background. True on slow idle frames
    /// (the Ken Burns drift is ~1 px/frame there and integer steps read as
    /// judder); false on 60 fps animation frames, where the pan moves < 0.2 px
    /// per frame and the bilinear pass would blow the 16 ms budget.
    pub smooth_pan: bool,
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

/// Maps raw unlock progress so the first [`FLASH_FRAC`] stays at rest pose
/// (green flash on top of a fully-opaque chrome) and only the remainder
/// eases the fade/drift.
fn unlock_fade_t(unlock_t: f32) -> f32 {
    if unlock_t <= FLASH_FRAC {
        0.0
    } else {
        ((unlock_t - FLASH_FRAC) / (1.0 - FLASH_FRAC)).clamp(0.0, 1.0)
    }
}

/// Overlay alpha and y-offset (positive is down) from raw 0..1 progress —
/// used for the full-screen dim veil, which fades with the whole chrome.
/// Unlock fade starts *after* the success flash (see [`unlock_fade_t`]).
pub fn overlay_motion(appear_t: f32, unlock_t: f32) -> (f32, f32) {
    let appear = ease_out_cubic(appear_t);
    let unlock = ease_out_cubic(unlock_fade_t(unlock_t));
    let alpha = (appear * (1.0 - unlock)).clamp(0.0, 1.0);
    let y = APPEAR_SLIDE_PX * (1.0 - appear) - UNLOCK_DRIFT_PX * unlock;
    (alpha, y)
}

/// How much extra dim the idle auto-dim adds on top of the base veil at full
/// progress (`idle_dim = 1`). Both the software `dim_rows` and the GPU
/// background shader scale by this so the two paths stay identical.
pub(crate) const IDLE_DIM_MAX: f32 = 0.25;
/// How long the idle auto-dim takes to ramp from 0 to full, in milliseconds.
pub(crate) const IDLE_DIM_RAMP_MS: u64 = 8000;

/// Background dim alpha with the idle auto-dim folded in. The base veil is
/// at most 1.0 once the appear finishes, so the idle deepens *beyond* that
/// (up to `IDLE_DIM_MAX` extra) — it can legally exceed 1.0 because it only
/// scales the background dim, never a color alpha. Shared by the software
/// `dim_rows` and the GPU background shader (`u_veil_alpha`) so an
/// idle-dimmed screen looks identical on both renderers. Rides the same
/// appear/unlock envelope as the base veil so there's no residual darkening
/// as the lock releases.
pub fn veil_alpha(appear_t: f32, unlock_t: f32, idle_dim: f32) -> f32 {
    let (base, _) = overlay_motion(appear_t, unlock_t);
    base + IDLE_DIM_MAX * idle_dim * base
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

/// Bounding rect of the lock-screen chrome (clock, date, pill, status) in
/// surface pixels — the GPU path uses it to know which region of the chrome
/// texture was drawn (and therefore needs uploading each frame).
///
/// Empty is `(+∞, +∞, −∞, −∞)` so [`ChromeRect::expand`] can seed from the
/// first real box. `(0,0,0,0)` as a start made `0.min(clock_x)` stick at the
/// origin and the GPU uploaded a huge/wrong dirty rect.
#[derive(Debug, Clone, Copy)]
pub struct ChromeRect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl Default for ChromeRect {
    fn default() -> Self {
        Self {
            x0: f32::INFINITY,
            y0: f32::INFINITY,
            x1: f32::NEG_INFINITY,
            y1: f32::NEG_INFINITY,
        }
    }
}

impl ChromeRect {
    fn expand(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        self.x0 = self.x0.min(x0);
        self.y0 = self.y0.min(y0);
        self.x1 = self.x1.max(x1);
        self.y1 = self.y1.max(y1);
    }

    #[allow(dead_code)]
    fn is_empty(&self) -> bool {
        self.x1 <= self.x0 || self.y1 <= self.y0
    }
}

/// Composes one frame. Returns `None` only if `width`/`height` are degenerate
/// (a `0x0` `configure`, which some compositors send transiently).
pub fn compose(text: &mut TextRenderer, inputs: &FrameInputs) -> Option<Pixmap> {
    let mut pixmap = Pixmap::new(inputs.width, inputs.height)?;
    compose_impl(&mut pixmap, text, inputs, None);
    Some(pixmap)
}

/// Composes only the chrome (clock/date/pill/status) into a transparent
/// `pixmap`, returning the bounding rect of everything drawn. The background
/// and veil are the GPU's job in the accelerated path; each element applies
/// its appear/unlock alpha once (the veil lives on the wallpaper, not baked
/// into chrome colors).
pub fn compose_chrome(
    pixmap: &mut Pixmap,
    text: &mut TextRenderer,
    inputs: &FrameInputs,
) -> ChromeRect {
    pixmap.fill(Color::TRANSPARENT);
    let mut rect = ChromeRect::default();
    compose_impl(pixmap, text, inputs, Some(&mut rect));
    rect
}

/// Shared body of [`compose`] / [`compose_chrome`]. With `rects`, the
/// background/veil are skipped (chrome-only) and each drawn element's box is
/// recorded.
fn compose_impl(
    pixmap: &mut Pixmap,
    text: &mut TextRenderer,
    inputs: &FrameInputs,
    mut rects: Option<&mut ChromeRect>,
) {
    if rects.is_none() {
        inputs
            .background
            .paint(pixmap, inputs.t_secs, inputs.smooth_pan);
    }

    // Overall chrome fade: appear eased in, unlock eased out *after* the
    // success flash. Skipping when `base_veil == 0` at appear t=0 flashed
    // undimmed wallpaper (and left the GPU chrome rect empty); only skip
    // once the unlock has fully finished.
    if inputs.unlock_t >= 1.0 {
        return;
    }
    let unlock = ease_out_cubic(unlock_fade_t(inputs.unlock_t));
    let fade = 1.0 - unlock;
    let (base_veil, _) = overlay_motion(inputs.appear_t, inputs.unlock_t);
    let bg_veil = veil_alpha(inputs.appear_t, inputs.unlock_t, inputs.idle_dim);

    let (w, h) = (inputs.width as f32, inputs.height as f32);
    // Palette colors are *not* pre-multiplied by the veil: each element
    // applies its appear/unlock alpha once via `faded(..., elem_alpha)`.
    // Pre-fading here and again with `pill_alpha` made the pill/status
    // fainter than the clock (which only faded once).
    let surface_color = tiny_skia_color(&inputs.palette.color0);
    let accent_color = tiny_skia_color(&inputs.palette.color4);
    let green_color = tiny_skia_color(&inputs.palette.color2);
    let on_surface = tiny_skia_color(breadlock_ui::theme::ink_on(&inputs.palette.color0));
    let red_color = tiny_skia_color(&inputs.palette.color1);

    // Translucent veil over the (static) wallpaper — a vertical gradient
    // (deeper at the top) that fades with the whole chrome. Applied in place
    // as a per-pixel multiply (premultiplied pixels scale by `1 - a` for a
    // black overlay), which is far cheaper than a full-surface gradient
    // fill/blit every frame. Skipped in the chrome-only path (the GPU shader
    // applies the same veil to the background). `bg_veil` may exceed 1.0
    // (idle auto-dim), which is fine — it only scales a multiply.
    if rects.is_none() && base_veil > 0.0 {
        dim_rows(pixmap, bg_veil);
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
    let elem_y =
        |e: f32, drift: f32| APPEAR_SLIDE_PX * (1.0 - e) - UNLOCK_DRIFT_PX * unlock * drift;

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
    if let Some(r) = rects.as_deref_mut() {
        let old_w = inputs
            .clock_old
            .map(|(t, _)| {
                text.measure_line_weighted(t, inputs.font_family, clock_size, Weight::BOLD)
            })
            .unwrap_or(0.0);
        let new_w = text.measure_line_weighted(
            inputs.clock_text,
            inputs.font_family,
            clock_size,
            Weight::BOLD,
        );
        let cw = old_w.max(new_w);
        r.expand(
            (w - cw) / 2.0,
            clock_y,
            (w + cw) / 2.0,
            clock_y + clock_size,
        );
    }
    match inputs.clock_old {
        Some((old, t)) => {
            let t = t.clamp(0.0, 1.0);
            let old_w =
                text.measure_line_weighted(old, inputs.font_family, clock_size, Weight::BOLD);
            text.draw_line_weighted(
                pixmap,
                old,
                inputs.font_family,
                clock_size,
                faded(Color::WHITE, clock_alpha * (1.0 - t)),
                (w - old_w) / 2.0,
                clock_y - 6.0 * t,
                Weight::BOLD,
            );
            let new_w = text.measure_line_weighted(
                inputs.clock_text,
                inputs.font_family,
                clock_size,
                Weight::BOLD,
            );
            text.draw_line_weighted(
                pixmap,
                inputs.clock_text,
                inputs.font_family,
                clock_size,
                faded(Color::WHITE, clock_alpha * t),
                (w - new_w) / 2.0,
                clock_y + 6.0 * (1.0 - t),
                Weight::BOLD,
            );
        }
        None => {
            let clock_w = text.measure_line_weighted(
                inputs.clock_text,
                inputs.font_family,
                clock_size,
                Weight::BOLD,
            );
            text.draw_line_weighted(
                pixmap,
                inputs.clock_text,
                inputs.font_family,
                clock_size,
                faded(Color::WHITE, clock_alpha),
                (w - clock_w) / 2.0,
                clock_y,
                Weight::BOLD,
            );
        }
    }

    // ---- Date line under the clock (hidden when date_text is empty) and
    // now-playing / battery under that. Info is *not* nested under the date
    // — an empty `date_format` used to hide the status line too.
    let date_size = (w * 0.016).clamp(DATE_SIZE_MIN, DATE_SIZE_MAX);
    let (clock_top, clock_height) = text.measure_box_weighted(
        inputs.clock_text,
        inputs.font_family,
        clock_size,
        Weight::BOLD,
    );
    let mut below_y = clock_y_rest
        + elem_y(date_e, DRIFT_DATE)
        + clock_top
        + clock_height
        + tokens::SPACE_SM as f32;
    if !inputs.date_text.is_empty() {
        let date_y = below_y;
        let date_w = text.measure_line(inputs.date_text, inputs.font_family, date_size);
        if let Some(r) = rects.as_mut() {
            r.expand(
                (w - date_w) / 2.0,
                date_y,
                (w + date_w) / 2.0,
                date_y + date_size,
            );
        }
        text.draw_line(
            pixmap,
            inputs.date_text,
            inputs.font_family,
            date_size,
            faded(Color::WHITE, date_e * fade * 0.82),
            (w - date_w) / 2.0,
            date_y,
        );
        below_y = date_y + date_size + tokens::SPACE_XS as f32;
    }
    if !inputs.info_text.is_empty() {
        let info_size = date_size * 0.85;
        let info_y = below_y;
        let info_anim = ease_out_cubic(staggered_t(inputs.appear_t, DATE_DELAY_MS + 120));
        let info_shown = ellipsize(
            text,
            inputs.info_text,
            inputs.font_family,
            info_size,
            w * 0.70,
        );
        let info_w = text.measure_line(&info_shown, inputs.font_family, info_size);
        if let Some(r) = rects.as_mut() {
            r.expand(
                (w - info_w) / 2.0,
                info_y,
                (w + info_w) / 2.0,
                info_y + info_size,
            );
        }
        text.draw_line(
            pixmap,
            &info_shown,
            inputs.font_family,
            info_size,
            faded(Color::WHITE, info_anim * fade * 0.6),
            (w - info_w) / 2.0,
            info_y,
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
        lerp_color(
            surface_color,
            red_color,
            (inputs.failed_t / SHAKE_RED_FRAC).clamp(0.0, 1.0),
        )
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
    let scale = if inputs.unlock_t > 0.0 {
        1.0
    } else {
        pill_scale
    };
    let shake_x = if inputs.failed {
        damped_shake_x(inputs.failed_t)
    } else {
        0.0
    };
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
    // Map a point through the same scale-about-center + shake as the pill
    // path, so dots/caret/hint/reveal travel with it.
    let map_pill = |x: f32, y: f32| {
        (
            x * scale + cx * (1.0 - scale) + shake_x,
            y * scale + cy * (1.0 - scale),
        )
    };
    // Chrome rect: pad for the shadow layers, breath/success rings, the
    // shake offset and the scale overshoot. Include shake so a failed
    // frame's dirty AABB actually moves with the pill.
    if let Some(r) = rects.as_deref_mut() {
        const PILL_PAD: f32 = 26.0;
        r.expand(
            pill_x + shake_x - PILL_PAD,
            pill_y - PILL_PAD,
            pill_x + shake_x + pill_w + PILL_PAD,
            pill_y + pill_h + PILL_PAD,
        );
    }

    if let Some(path) = rounded_rect(
        pill_x,
        pill_y,
        pill_w,
        pill_h,
        tokens::RADIUS_SECONDARY as f32,
    ) {
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
        pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, pill_xf, None);

        // Hairline border for depth — dropped on the wrong/success states
        // (the sketch sets `border-color: transparent` there).
        if !inputs.failed && inputs.unlock_t == 0.0 {
            let stroke = tiny_skia::Stroke {
                width: 1.0,
                ..Default::default()
            };
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
            let stroke = tiny_skia::Stroke {
                width: 1.5,
                ..Default::default()
            };
            let mut paint = Paint::default();
            paint.set_color(faded(
                accent_color,
                BREATHE_RING_ALPHA * inputs.breathe_t * pill_alpha,
            ));
            pixmap.stroke_path(&path, &paint, &stroke, pill_xf, None);
        }

        // Success flash: expanding accent ring around the pill for the first
        // `FLASH_MS` of the unlock.
        if inputs.unlock_t > 0.0 && inputs.unlock_t < FLASH_FRAC {
            let flash_t = inputs.unlock_t / FLASH_FRAC;
            let stroke = tiny_skia::Stroke {
                width: 2.0 + 16.0 * flash_t,
                ..Default::default()
            };
            let mut paint = Paint::default();
            paint.set_color(faded(green_color, 0.55 * (1.0 - flash_t) * pill_alpha));
            pixmap.stroke_path(&path, &paint, &stroke, pill_xf, None);
        }
    }

    // ---- Caps Lock / layout chip: a small centered pill above the password
    // pill, only when Caps Lock is on or a non-default layout is active. A
    // tiny floating hint so the user can't be confused by all-caps input.
    if inputs.caps_lock || inputs.layout_index > 0 {
        let mut label = String::new();
        if inputs.caps_lock {
            label.push_str("Caps Lock");
        }
        if inputs.layout_index > 0 {
            if !label.is_empty() {
                label.push_str(" · ");
            }
            label.push_str(&format!("Layout {}", inputs.layout_index + 1));
        }
        let chip_size = tokens::FONT_SIZE_SECONDARY as f32;
        let chip_w = text.measure_line(&label, inputs.font_family, chip_size)
            + tokens::SPACE_MD as f32 * 2.0;
        let chip_h = chip_size * 1.9;
        let chip_x = (w - chip_w) / 2.0;
        // Clear of the pill: chip bottom sits a full SPACE_LG above the pill
        // top, so the two never touch even with the pill's glow/shadow.
        let chip_y = pill_y - chip_h - tokens::SPACE_LG as f32;
        let chip_alpha = pill_e * fade;
        if let Some(r) = rects.as_mut() {
            r.expand(chip_x, chip_y, chip_x + chip_w, chip_y + chip_h);
        }
        if let Some(path) = rounded_rect(chip_x, chip_y, chip_w, chip_h, chip_h / 2.0) {
            let mut paint = Paint::default();
            // Slightly lifted surface color so it reads as a separate chip.
            paint.set_color(faded(surface_color, chip_alpha));
            paint.anti_alias = true;
            pixmap.fill_path(
                &path,
                &paint,
                tiny_skia::FillRule::Winding,
                Transform::identity(),
                None,
            );
            let stroke = tiny_skia::Stroke {
                width: 1.0,
                ..Default::default()
            };
            let mut paint = Paint::default();
            paint.set_color(faded(Color::WHITE, PILL_BORDER_ALPHA * chip_alpha));
            pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
        let (chip_top, chip_height) = text.measure_box(&label, inputs.font_family, chip_size);
        let label_y = chip_y + (chip_h - chip_height) / 2.0 - chip_top;
        let label_w = text.measure_line(&label, inputs.font_family, chip_size);
        text.draw_line(
            pixmap,
            &label,
            inputs.font_family,
            chip_size,
            faded(on_surface, chip_alpha),
            (w - label_w) / 2.0,
            label_y,
        );
    }

    // ---- Password dots — one filled circle per typed character, capped so a
    // very long password can't overflow the pill. The newest dot pops in with
    // an overshoot; the rest sit at rest size. While the reveal key (Tab) is
    // held, the plain characters are drawn instead.
    let max_dots = ((pill_w - tokens::SPACE_LG as f32 * 2.0) / DOT_GAP)
        .floor()
        .max(1.0) as usize;
    let shown_dots = inputs.password_len.min(max_dots);
    let dot_y = pill_y + pill_h / 2.0;
    // Skip in-pill text while the appear scale is ~0 (cosmic-text panics on
    // a zero font size). Paths still go through `pill_xf` and collapse.
    let contents_live = scale >= 0.05;
    if inputs.reveal && shown_dots > 0 && contents_live {
        // Hold-to-reveal: render the actual password, centered, capped to
        // the pill width (truncate with a trailing ellipsis on overflow).
        // Measured into locals first — `text` is borrowed mutably by
        // `draw_line`, so all `measure_*` calls must happen up front.
        let reveal_size = tokens::FONT_SIZE_BASE as f32;
        let reveal_rendered = ellipsize(
            text,
            inputs.password,
            inputs.font_family,
            reveal_size,
            pill_w - tokens::SPACE_LG as f32 * 2.0,
        );
        let (reveal_top, reveal_height) =
            text.measure_box(&reveal_rendered, inputs.font_family, reveal_size);
        let reveal_y = pill_y + (pill_h - reveal_height) / 2.0 - reveal_top;
        let reveal_w = text.measure_line(&reveal_rendered, inputs.font_family, reveal_size);
        let (rx, ry) = map_pill((w - reveal_w) / 2.0, reveal_y);
        if let Some(r) = rects.as_mut() {
            r.expand(
                pill_x + shake_x + tokens::SPACE_LG as f32,
                pill_y,
                pill_x + shake_x + pill_w - tokens::SPACE_LG as f32,
                pill_y + pill_h,
            );
        }
        text.draw_line(
            pixmap,
            &reveal_rendered,
            inputs.font_family,
            reveal_size * scale,
            faded(on_surface, pill_alpha),
            rx,
            ry,
        );
    } else if shown_dots > 0 {
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
                pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, pill_xf, None);
            }
        }
    }

    // ---- Empty pill: a centered "enter password" hint instead of dots. The
    // caret only appears with the first typed character, so the pill reads as
    // an input field rather than an empty dark bar. Centered on the exact
    // glyph box (origin anchors the text top, not the baseline).
    if shown_dots == 0 && !inputs.failed && contents_live {
        let hint = "Enter password";
        let hint_size = tokens::FONT_SIZE_BASE as f32;
        let hint_w = text.measure_line(hint, inputs.font_family, hint_size);
        let (hint_top, hint_height) = text.measure_box(hint, inputs.font_family, hint_size);
        let hint_y = pill_y + (pill_h - hint_height) / 2.0 - hint_top;
        let (hx, hy) = map_pill((w - hint_w) / 2.0, hint_y);
        text.draw_line(
            pixmap,
            hint,
            inputs.font_family,
            hint_size * scale,
            faded(on_surface, pill_alpha * 0.5),
            hx,
            hy,
        );
    } else if shown_dots > 0 && !inputs.reveal {
        // ---- Caret after the last dot: solid for half a second after a
        // keystroke, then blinking at ~1.8 Hz. Hidden during hold-to-reveal
        // (it used to sit at the last *dot* slot, overlapping the text).
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
            if let Some(path) = rounded_rect(caret_x, dot_y - caret_h / 2.0, CARET_W, caret_h, 1.0)
            {
                let mut paint = Paint::default();
                paint.set_color(faded(caret_color, pill_alpha));
                paint.anti_alias = true;
                pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, pill_xf, None);
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
        let status_y = pill_y_rest
            + pill_h
            + tokens::SPACE_MD as f32
            + elem_y(status_e, DRIFT_STATUS)
            + STATUS_SLIDE_PX * (1.0 - status_anim);
        if let Some(r) = rects.as_mut() {
            r.expand(
                (w - status_w) / 2.0,
                status_y,
                (w + status_w) / 2.0,
                status_y + status_size,
            );
        }
        text.draw_line(
            pixmap,
            status,
            inputs.font_family,
            status_size,
            faded(color, status_alpha),
            (w - status_w) / 2.0,
            status_y,
        );
    }
}

/// Recomputes the left edge of the dot row (shared by the dot loop and the
/// caret placement — kept out of `compose` to avoid a long-lived binding).
fn start_x_for(shown_dots: usize, pill_x: f32, pill_w: f32) -> f32 {
    let dots_w = (shown_dots as f32 - 1.0).max(0.0) * DOT_GAP;
    pill_x + (pill_w - dots_w) / 2.0
}

/// Truncates `password` (or any single-line string) so it fits in `max_w`,
/// appending an ellipsis when trimmed. Shared by hold-to-reveal and the
/// now-playing/battery line. Kept out of `compose` so the borrow of `text`
/// ends before the draw call.
fn ellipsize(
    text: &mut TextRenderer,
    password: &str,
    font_family: &str,
    size: f32,
    max_w: f32,
) -> String {
    let mut shown = password;
    let mut ellipsis = "";
    loop {
        let candidate = format!("{shown}{ellipsis}");
        if text.measure_line(&candidate, font_family, size) <= max_w || shown.is_empty() {
            return candidate;
        }
        // Trim one char at a time until it fits.
        shown = &shown[..shown
            .char_indices()
            .nth_back(1)
            .map(|(i, _)| i)
            .unwrap_or(0)];
        ellipsis = "…";
    }
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
    fn dim_rows_darkens_top_more_than_bottom() {
        // 2 wide × 4 tall: top row is y/h = 0, bottom row is y/h = 0.75.
        let mut p = Pixmap::new(2, 4).unwrap();
        p.fill(Color::WHITE);
        dim_rows(&mut p, 1.0);
        let px = p.pixels();
        let top = px[0];
        let bottom = px[2 * 3];
        // DIM_ALPHA_TOP (0.34) > DIM_ALPHA_BOTTOM (0.16): top row darker.
        assert!(
            top.red() < bottom.red(),
            "top {} should be darker than bottom {}",
            top.red(),
            bottom.red()
        );
        // White at top dim 0.34 → 255 * (1 - 0.34) = 168.
        assert_eq!(top.red(), 168);
        // Bottom row is y/h = 0.75 → dim = 0.34 + (0.16 - 0.34) * 0.75 = 0.205.
        let expected = (255.0 * (1.0 - 0.205)) as u8;
        assert_eq!(bottom.red(), expected);
        // GPU veil keeps A=1; software must not scale alpha.
        assert_eq!(top.alpha(), 255);
        assert_eq!(bottom.alpha(), 255);
    }

    #[test]
    fn dim_rows_noop_at_zero_alpha() {
        let mut p = Pixmap::new(2, 2).unwrap();
        p.fill(Color::from_rgba8(100, 150, 200, 255));
        let before = p.pixels().to_vec();
        dim_rows(&mut p, 0.0);
        assert_eq!(p.pixels(), before.as_slice());
    }

    #[allow(clippy::too_many_arguments)]
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
            password: "",
            reveal: false,
            caps_lock: false,
            layout_index: 0,
            idle_dim: 0.0,
            failed,
            failed_t,
            dot_pop_t,
            keystroke_age: None,
            t_secs: 0.0,
            breathe_t: 0.0,
            status_t: 1.0,
            status_text: None,
            info_text: "",
            appear_t,
            unlock_t,
            smooth_pan: false,
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
            password: "",
            reveal: false,
            caps_lock: false,
            layout_index: 0,
            idle_dim: 0.0,
            failed: false,
            failed_t: 0.0,
            dot_pop_t: 1.0,
            keystroke_age: None,
            t_secs: 0.0,
            breathe_t: 0.0,
            status_t: 1.0,
            status_text: None,
            info_text: "",
            appear_t: 1.0,
            unlock_t: 0.0,
            smooth_pan: false,
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
        let failed = inputs(
            &bg,
            &palette,
            "12:34",
            "Friday · Aug 21",
            4,
            true,
            0.3,
            0.4,
            1.0,
            0.0,
        );
        let failed_px = compose(&mut text, &failed).expect("failed compose");
        assert_eq!((failed_px.width(), failed_px.height()), (400, 300));
        assert!(failed_px.pixels().iter().any(|p| p.alpha() > 0));
        // Success flash phase of the unlock: still fully opaque chrome (flash
        // holds rest pose), not already faded.
        let success = inputs(
            &bg,
            &palette,
            "12:34",
            "Friday · Aug 21",
            4,
            false,
            0.0,
            1.0,
            1.0,
            0.12,
        );
        let success_px = compose(&mut text, &success).expect("success compose");
        let rest = inputs(
            &bg,
            &palette,
            "12:34",
            "Friday · Aug 21",
            4,
            false,
            0.0,
            1.0,
            1.0,
            0.0,
        );
        let rest_px = compose(&mut text, &rest).expect("rest compose");
        // Flash frame should not be a near-empty fade — plenty of chrome left.
        let flash_lit = success_px.pixels().iter().filter(|p| p.alpha() > 0).count();
        let rest_lit = rest_px.pixels().iter().filter(|p| p.alpha() > 0).count();
        assert!(
            flash_lit as f32 > rest_lit as f32 * 0.5,
            "success flash should keep chrome visible, lit {flash_lit} vs rest {rest_lit}"
        );
        // Fully faded unlock returns just the background.
        let done = inputs(
            &bg,
            &palette,
            "12:34",
            "Friday · Aug 21",
            4,
            false,
            0.0,
            1.0,
            1.0,
            1.0,
        );
        let done_px = compose(&mut text, &done).expect("done compose");
        assert_eq!((done_px.width(), done_px.height()), (400, 300));
    }

    #[test]
    fn veil_alpha_idle_dim_deepens_past_base() {
        // Rest pose, no idle: base appear alpha only (1.0).
        assert_eq!(veil_alpha(1.0, 0.0, 0.0), 1.0);
        // Mid-appear, no idle: base alpha.
        let base = veil_alpha(0.5, 0.0, 0.0);
        assert!(base > 0.0 && base < 1.0);
        // Full idle dim deepens *past* the base (background-only alpha, so
        // exceeding 1.0 is legal — it scales the dim multiply, not a color).
        let idle = veil_alpha(0.5, 0.0, 1.0);
        assert!(idle > base, "idle dim should deepen the veil");
        // At rest with full idle: 1.0 + 0.25 * 1.0.
        assert!((veil_alpha(1.0, 0.0, 1.0) - 1.25).abs() < 1e-6);
        // Idle dim alone can't darken a screen that hasn't appeared yet
        // (base 0 keeps the whole term 0).
        assert_eq!(veil_alpha(0.0, 0.0, 1.0), 0.0);
        // During unlock, idle dim can't push past the fade-out.
        assert_eq!(veil_alpha(1.0, 1.0, 1.0), 0.0);
    }

    #[test]
    fn reveal_fit_truncates_long_passwords() {
        let mut text = TextRenderer::new();
        // Short password fits unchanged.
        assert_eq!(
            ellipsize(&mut text, "hunter2", "sans-serif", 14.0, 200.0),
            "hunter2"
        );
        // A very long one is trimmed and ends with an ellipsis.
        let long = "a".repeat(200);
        let fitted = ellipsize(&mut text, &long, "sans-serif", 14.0, 60.0);
        assert!(
            fitted.ends_with('…'),
            "trimmed reveal should end with an ellipsis"
        );
        assert!(fitted.len() < long.len());
        // And it actually fits the budget.
        assert!(text.measure_line(&fitted, "sans-serif", 14.0) <= 60.0);
    }

    #[test]
    fn compose_renders_caps_chip_and_reveal() {
        let bg = Background::Color(Color::BLACK);
        let palette = breadlock_ui::theme::Palette::default();
        let mut text = TextRenderer::new();
        let mut base = inputs(
            &bg,
            &palette,
            "12:34",
            "Friday · Aug 21",
            4,
            false,
            0.0,
            1.0,
            1.0,
            0.0,
        );
        base.caps_lock = true;
        base.password = "hunter2";
        // Caps chip visible, no reveal: dots path.
        assert!(compose(&mut text, &base).is_some());
        // Reveal: plain characters instead of dots.
        base.reveal = true;
        assert!(compose(&mut text, &base).is_some());
        // Non-default layout shows the layout chip too.
        base.caps_lock = false;
        base.layout_index = 1;
        assert!(compose(&mut text, &base).is_some());
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

    #[test]
    fn overlay_motion_holds_rest_during_success_flash() {
        // First FLASH_FRAC of unlock_t is the green flash at full opacity.
        let (a_rest, y_rest) = overlay_motion(1.0, 0.0);
        let (a_flash, y_flash) = overlay_motion(1.0, FLASH_FRAC * 0.5);
        assert!(
            (a_flash - a_rest).abs() < 1e-6,
            "flash must not fade chrome, got {a_flash}"
        );
        assert!(
            (y_flash - y_rest).abs() < 1e-6,
            "flash must not drift chrome, got {y_flash}"
        );
        // After the flash, fade/drift begin.
        let (a_fade, y_fade) = overlay_motion(1.0, (FLASH_FRAC + 1.0) * 0.5);
        assert!(a_fade < a_rest, "post-flash should fade, got {a_fade}");
        assert!(y_fade < y_rest, "post-flash should drift up, got {y_fade}");
    }

    #[test]
    fn compose_chrome_rect_contains_clock_and_pill() {
        let bg = Background::Color(Color::BLACK);
        let palette = breadlock_ui::theme::Palette::default();
        let mut text = TextRenderer::new();
        let mut pixmap = Pixmap::new(400, 300).unwrap();
        let inputs = inputs(
            &bg,
            &palette,
            "12:34",
            "Friday · Aug 21",
            4,
            false,
            0.0,
            1.0,
            1.0,
            0.0,
        );
        let rect = compose_chrome(&mut pixmap, &mut text, &inputs);
        assert!(
            rect.x1 > rect.x0 && rect.y1 > rect.y0,
            "chrome rect must be non-empty, got {rect:?}"
        );
        // Clock sits at h*0.28 with glyph height ~ clock_size (400*0.075=30).
        assert!(
            rect.y0 < 300.0 * 0.28 + 40.0,
            "rect must cover the clock band"
        );
        // Pill sits at h*0.5; with the 26px pad the rect must reach it.
        assert!(
            rect.y1 > 300.0 * 0.5 + 24.0,
            "rect must cover the pill band"
        );
        // Both are horizontally centered.
        assert!(
            rect.x0 < 200.0 && rect.x1 > 200.0,
            "rect must straddle center"
        );
        // Origin-stuck Default(0,0,…) used to pass the checks above (0.min
        // never leaves 0, and 0 < 200 && x1 > 200 still holds). Fail that.
        assert!(
            rect.x0 > 0.0 && rect.y0 > 0.0,
            "chrome rect must not be stuck at the origin, got {rect:?}"
        );
    }

    #[test]
    fn compose_chrome_rect_empty_when_unlock_finished() {
        let bg = Background::Color(Color::BLACK);
        let palette = breadlock_ui::theme::Palette::default();
        let mut text = TextRenderer::new();
        let mut pixmap = Pixmap::new(400, 300).unwrap();
        // unlock_t = 1 → chrome is gone; appear_t = 0 still draws (invisible)
        // chrome so the GPU dirty rect is valid from the first frame.
        let inputs = inputs(
            &bg,
            &palette,
            "12:34",
            "Friday · Aug 21",
            4,
            false,
            0.0,
            1.0,
            1.0,
            1.0,
        );
        let rect = compose_chrome(&mut pixmap, &mut text, &inputs);
        assert!(
            rect.is_empty(),
            "finished unlock must yield an empty rect, got {rect:?}"
        );
        assert!(
            pixmap.pixels().iter().all(|p| p.alpha() == 0),
            "finished unlock must leave the pixmap transparent"
        );
    }

    #[test]
    fn compose_chrome_rect_valid_at_appear_start() {
        let bg = Background::Color(Color::BLACK);
        let palette = breadlock_ui::theme::Palette::default();
        let mut text = TextRenderer::new();
        let mut pixmap = Pixmap::new(400, 300).unwrap();
        let inputs = inputs(
            &bg,
            &palette,
            "12:34",
            "Friday · Aug 21",
            4,
            false,
            0.0,
            1.0,
            0.0,
            0.0,
        );
        let rect = compose_chrome(&mut pixmap, &mut text, &inputs);
        assert!(
            !rect.is_empty() && rect.x0 > 0.0 && rect.y0 > 0.0,
            "appear t=0 must still produce a real chrome rect, got {rect:?}"
        );
    }

    #[test]
    fn compose_chrome_failed_shake_shifts_rect_x() {
        let bg = Background::Color(Color::BLACK);
        let palette = breadlock_ui::theme::Palette::default();
        let mut text = TextRenderer::new();
        let mut pixmap = Pixmap::new(400, 300).unwrap();
        let rest = inputs(
            &bg,
            &palette,
            "12:34",
            "Friday · Aug 21",
            4,
            true,
            0.0,
            1.0,
            1.0,
            0.0,
        );
        let r0 = compose_chrome(&mut pixmap, &mut text, &rest);
        let mid = inputs(
            &bg,
            &palette,
            "12:34",
            "Friday · Aug 21",
            4,
            true,
            0.3,
            1.0,
            1.0,
            0.0,
        );
        let r1 = compose_chrome(&mut pixmap, &mut text, &mid);
        assert!(
            (r0.x0 - r1.x0).abs() > 0.5 || (r0.x1 - r1.x1).abs() > 0.5,
            "failed shake must shift chrome rect x, rest={r0:?} shaken={r1:?}"
        );
        // Chrome-only pixmaps: full `compose()` is dominated by the opaque
        // wallpaper, so a 4px pill shake would not move that centroid.
        let mut rest_px = Pixmap::new(400, 300).unwrap();
        compose_chrome(&mut rest_px, &mut text, &rest);
        let mut shaken_px = Pixmap::new(400, 300).unwrap();
        compose_chrome(&mut shaken_px, &mut text, &mid);
        let centroid_x = |p: &Pixmap| -> f32 {
            let mut sx = 0.0f32;
            let mut n = 0.0f32;
            for (i, px) in p.pixels().iter().enumerate() {
                if px.alpha() > 32 {
                    sx += (i as u32 % p.width()) as f32;
                    n += 1.0;
                }
            }
            if n > 0.0 {
                sx / n
            } else {
                0.0
            }
        };
        let dx = (centroid_x(&shaken_px) - centroid_x(&rest_px)).abs();
        assert!(
            dx > 0.2,
            "pill contents should shake with the pill, centroid dx={dx}"
        );
    }

    #[test]
    fn compose_chrome_info_text_draws_without_date() {
        let bg = Background::Color(Color::BLACK);
        let palette = breadlock_ui::theme::Palette::default();
        let mut text = TextRenderer::new();
        let mut pixmap = Pixmap::new(400, 300).unwrap();
        let mut with_info = inputs(&bg, &palette, "12:34", "", 0, false, 0.0, 1.0, 1.0, 0.0);
        with_info.info_text = "Battery 87% · charging";
        let with = compose_chrome(&mut pixmap, &mut text, &with_info);
        let mut pixmap2 = Pixmap::new(400, 300).unwrap();
        let without_info = inputs(&bg, &palette, "12:34", "", 0, false, 0.0, 1.0, 1.0, 0.0);
        let without = compose_chrome(&mut pixmap2, &mut text, &without_info);
        // The pill still sits below the info line, so y1 is pill-dominated.
        // The info line must still produce extra chrome pixels and a rect
        // that is not stuck at the origin.
        let lit = |p: &Pixmap| p.pixels().iter().filter(|px| px.alpha() > 32).count();
        assert!(
            lit(&pixmap) > lit(&pixmap2),
            "info line must draw extra chrome when date_text is empty (with {} lit, without {})",
            lit(&pixmap),
            lit(&pixmap2)
        );
        assert!(with.x0 > 0.0 && with.y0 > 0.0 && !with.is_empty());
        assert!(without.x0 > 0.0 && !without.is_empty());
    }

    #[test]
    fn compose_chrome_status_text_expands_the_rect_downward() {
        let bg = Background::Color(Color::BLACK);
        let palette = breadlock_ui::theme::Palette::default();
        let mut text = TextRenderer::new();
        let mut pixmap = Pixmap::new(400, 300).unwrap();
        let mut with_status = inputs(
            &bg,
            &palette,
            "12:34",
            "Friday · Aug 21",
            4,
            true,
            0.3,
            1.0,
            1.0,
            0.0,
        );
        with_status.status_text = Some("Wrong password");
        let rect = compose_chrome(&mut pixmap, &mut text, &with_status);
        // Status sits below the pill: pill bottom is h*0.5 + 24 (half of 48px),
        // status adds SPACE_MD + its glyph box after that.
        assert!(
            rect.y1 > 300.0 * 0.5 + 48.0 + 20.0,
            "status must push the rect below the pill, got y1={}",
            rect.y1
        );
    }

    #[test]
    fn gpu_split_is_pixel_identical_to_full_compose() {
        // The GPU path draws the dimmed background in a shader and then
        // composites the software chrome (colors pre-faded by the veil alpha)
        // over it with premultiplied source-over. That split must produce the
        // exact same pixels as the single-pass software compose — this is the
        // invariant that keeps the two renderers in sync.
        let bg = Background::Color(Color::from_rgba8(40, 60, 80, 255));
        let palette = breadlock_ui::theme::Palette::default();
        let mut text = TextRenderer::new();
        let inputs = inputs(
            &bg,
            &palette,
            "12:34",
            "Friday · Aug 21",
            4,
            false,
            0.0,
            1.0,
            1.0,
            0.0,
        );

        // Full single-pass compose.
        let full = compose(&mut text, &inputs).unwrap();

        // Split: dim the background, then composite the chrome over it.
        let mut split = Pixmap::new(400, 300).unwrap();
        inputs
            .background
            .paint(&mut split, inputs.t_secs, inputs.smooth_pan);
        let (veil_alpha, _) = overlay_motion(inputs.appear_t, inputs.unlock_t);
        if veil_alpha > 0.0 {
            dim_rows(&mut split, veil_alpha);
        }
        let mut chrome = Pixmap::new(400, 300).unwrap();
        let mut text2 = TextRenderer::new();
        compose_chrome(&mut chrome, &mut text2, &inputs);
        // Premultiplied source-over, exactly what the GPU's
        // glBlendFunc(GL_ONE, GL_ONE_MINUS_SRC_ALPHA) performs.
        split.draw_pixmap(
            0,
            0,
            chrome.as_ref(),
            &tiny_skia::PixmapPaint {
                blend_mode: tiny_skia::BlendMode::SourceOver,
                ..Default::default()
            },
            Transform::default(),
            None,
        );

        // The split path rounds twice (chrome into an 8-bit pixmap, then the
        // composite into 8-bit) where the single pass rounds once, so
        // bit-exact equality is impossible — the invariant is that the split
        // stays within a couple of ULPs (measured: max 3 on this input, with
        // >95% of pixels bit-identical), and never diverges structurally.
        let diff = split
            .pixels()
            .iter()
            .zip(full.pixels())
            .map(|(a, b)| {
                (a.red() as i32 - b.red() as i32)
                    .abs()
                    .max((a.green() as i32 - b.green() as i32).abs())
                    .max((a.blue() as i32 - b.blue() as i32).abs())
                    .max((a.alpha() as i32 - b.alpha() as i32).abs())
            })
            .collect::<Vec<_>>();
        let identical = diff.iter().filter(|d| **d == 0).count();
        let max_diff = diff.iter().copied().max().unwrap_or(0);
        assert!(
            max_diff <= 3,
            "GPU-style split must stay within double-rounding ULP range, got max diff {max_diff}"
        );
        assert!(
            identical > split.pixels().len() * 95 / 100,
            "most pixels should be bit-identical, got {identical}/{} identical",
            split.pixels().len()
        );
    }
}

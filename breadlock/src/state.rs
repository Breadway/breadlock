use smithay_client_toolkit::compositor::CompositorState;
use smithay_client_toolkit::output::OutputState;
use smithay_client_toolkit::reexports::calloop::channel::Sender;
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::LoopHandle;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::seat::SeatState;
use smithay_client_toolkit::session_lock::{SessionLock, SessionLockState, SessionLockSurface};
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use std::time::{Duration, Instant};
use wayland_client::protocol::{wl_keyboard, wl_output, wl_seat, wl_shm};
use wayland_client::{Connection, QueueHandle};

use crate::auth::AuthOutcome;
use crate::background::Background;
use crate::config::Config;
use crate::render;

/// Reserved password buffer size. Typing past this is ignored so `String`
/// never reallocates (an old unzeroized heap buffer would leak).
pub(crate) const PASSWORD_CAP: usize = 256;

/// Per-output lock surface plus the size the compositor last `configure`d it
/// to (0x0 until the first configure arrives). `output` is kept so
/// `output_destroyed` can find and drop the surface belonging to an unplugged
/// monitor — without it, hotplug/unplug cycles only ever grow `surfaces`.
pub struct LockSurface {
    pub surface: SessionLockSurface,
    pub output: wl_output::WlOutput,
    pub width: u32,
    pub height: u32,
    /// `wl_surface` buffer scale. 1 until `scale_factor_changed`. Always >= 1.
    pub scale: i32,
    /// EGL-backed renderer for this surface (created on first `configure`);
    /// `None` when the GPU path is unavailable, in which case the software
    /// wl_shm path is used.
    pub gpu: Option<crate::gpu::GpuSurface>,
    /// Reused shm pool + current buffer (software path). Not recreated every
    /// frame; SlotPool waits for compositor release before reuse.
    pub shm_pool: Option<SlotPool>,
    pub shm_buffer: Option<Buffer>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthState {
    Idle,
    /// A PAM check is running on its own thread; input other than Escape is
    /// ignored until it resolves so a second Enter can't race the first
    /// attempt. Escape cancels the wait (the in-flight libpam call is not
    /// aborted; its result is ignored).
    Checking,
    /// The password was rejected by PAM — an ordinary wrong-password
    /// outcome the user can retry. Input is not blocked.
    Failed,
    /// PAM `acct_mgmt` rejected the account (locked, expired, etc.).
    AccountInvalid,
    /// PAM itself failed to initialize (e.g. `/etc/pam.d/breadlock` is
    /// missing or unreadable), or the process username could not be
    /// resolved — a config/deployment problem, not something the user's
    /// password can fix. Rendered with a distinct message so a broken
    /// install doesn't look like an endless string of typos.
    ConfigError,
}

pub struct AppState {
    pub loop_handle: LoopHandle<'static, AppState>,
    pub conn: Connection,
    pub compositor_state: CompositorState,
    pub output_state: OutputState,
    pub registry_state: RegistryState,
    pub seat_state: SeatState,
    pub shm: Shm,
    pub session_lock_state: SessionLockState,
    pub session_lock: Option<SessionLock>,
    pub surfaces: Vec<LockSurface>,
    pub keyboard: Option<wl_keyboard::WlKeyboard>,
    /// Seat that owns [`Self::keyboard`]. `remove_capability` only releases
    /// the keyboard if that seat lost Keyboard.
    pub keyboard_seat: Option<wl_seat::WlSeat>,

    pub config: Config,
    pub palette: breadlock_ui::theme::Palette,
    /// Per-output resolved palette cache, keyed by `wl_output` name. Filled
    /// lazily by [`AppState::palette_for_surface`] so a redraw doesn't re-read
    /// and re-parse `palettes/<output>.json` from disk on every frame. Cleared
    /// wholesale by the [`crate::theme_watch`] callback when pywal regenerates
    /// the palette files. `RefCell` because `palette_for_surface` runs from
    /// the `&self`-shaped middle of `redraw_surface` (other `&self` borrows
    /// are live), but only ever on the single event-loop thread.
    pub output_palettes:
        std::cell::RefCell<std::collections::HashMap<String, breadlock_ui::theme::Palette>>,
    /// File watcher that invalidates [`AppState::output_palettes`] on a
    /// palette change. `None` when the watch could not be armed — then
    /// `palette_for_surface` falls back to reading from disk each call so a
    /// live palette change is still reflected (just less cheaply).
    pub theme_watch: Option<crate::theme_watch::ThemeWatch>,
    pub background: Background,
    /// GPU background renderer (EGL/GLES2). `None` falls back to the
    /// fully-software path.
    pub gpu: Option<crate::gpu::GpuRenderer>,
    pub text_renderer: breadlock_ui::painter::TextRenderer,

    pub username: String,
    /// Wrapped in `Zeroizing` so the buffer is wiped on every drop/replace
    /// (e.g. when `submit()` swaps in a fresh one) rather than just
    /// deallocated with the password bytes left sitting in freed heap
    /// memory. Individual edits (backspace, clear) still need their own
    /// explicit zeroing — see `input/keyboard.rs` — since `Zeroizing` only
    /// hooks `Drop`, not in-place mutation.
    pub password: zeroize::Zeroizing<String>,
    /// Character count shown in the pill after submit (secret already
    /// moved to the auth thread). Used until Idle or the user types again.
    pub password_display_len: usize,
    pub auth_state: AuthState,
    pub auth_tx: Sender<AuthOutcome>,
    /// Bumped on each submit / Escape-cancel. Late PAM results whose
    /// generation does not match are ignored.
    pub auth_generation: u64,
    /// Bumped each time Failed / AccountInvalid / ConfigError is set.
    /// The fail-clear timer captures this and only clears if it still matches.
    pub failed_generation: u64,
    /// When the current PAM check entered Checking — drives `checking_dots`.
    pub checking_started: Option<Instant>,

    /// Monotonic clock reference — drives the idle caret blink cadence.
    pub started: Instant,
    /// First-frame timestamp for the lock-appear animation. `None` until
    /// the first non-degenerate redraw so the fade starts when the surface
    /// is actually visible, not when the process starts.
    pub appear_started: Option<Instant>,
    /// Set on PAM success. While `Some`, lock surfaces stay up and the
    /// overlay fades out; compositor `unlock()` happens only after the
    /// fade completes. Dying mid-fade leaves the session locked (fail-secure).
    pub unlocking: Option<Instant>,
    /// Timestamp of the most recent keystroke that grew the password — drives
    /// the newest-dot pop-in and the caret's solid-then-blink behavior.
    pub last_keystroke: Option<Instant>,
    /// When the failed state was entered — drives the wrong-password shake.
    /// Cleared (with the failed state) by typing or `fail_timeout_ms`.
    pub failed_at: Option<Instant>,
    /// Clock text drawn last frame; a change starts a minute-rollover
    /// crossfade instead of a hard text swap.
    pub last_clock_text: String,
    /// Outgoing clock string + when its crossfade started. Kept until the
    /// fade completes so later frames still pass the previous string.
    pub clock_from: Option<(String, Instant)>,
    /// When the current status line appeared ("Checking…" / "Wrong password") —
    /// drives its slide-in. Reset whenever `auth_state` changes (see
    /// `last_auth_state`).
    pub status_anim_started: Option<Instant>,
    /// The `auth_state` from the last frame — a change resets the status
    /// slide-in so a freshly appearing status rises in instead of popping.
    pub last_auth_state: AuthState,
    /// When the current idle-breath window started (glow pulse). `None`
    /// between breaths.
    pub breathe_started: Option<Instant>,
    /// When the next idle-breath window is due — the 1s clock tick arms the
    /// animation timer once it's due, so idle CPU stays near zero.
    pub breathe_next_at: Option<Instant>,
    /// True while a ~16ms animation timer is registered on the event loop.
    pub anim_timer_armed: bool,

    /// Caps Lock is on (from the last keyboard modifier update) — drives the
    /// small "Caps Lock" chip so the user isn't mystified by uppercase-only
    /// input. Stale until the first modifier update arrives.
    pub caps_lock: bool,
    /// Active keyboard layout index (0-based) — shown next to the caps chip
    /// when a non-default layout is selected.
    pub layout_index: u32,
    /// True while the user holds the reveal key (Tab) — dots render as the
    /// plain characters while held.
    pub reveal_held: bool,
    /// Last keystroke/activity timestamp — drives the idle auto-dim ramp
    /// (`animation.idle_dim_after_secs`). Any key press resets it.
    pub last_activity: Instant,
    /// Consecutive failed password attempts this session — drives the
    /// "N failed attempts" status line. Reset on a successful auth.
    pub failed_attempts: u32,
    /// Latest D-Bus snapshot (now-playing / battery) from the status poller.
    /// Empty fields render nothing; replaced wholesale on each poll.
    pub status_info: crate::status::StatusInfo,

    pub exit: bool,
}

impl AppState {
    /// Composes and uploads a fresh frame for one lock surface. A `0x0` size
    /// (surfaces are created at that size before their first `configure`) is
    /// skipped rather than allocating a degenerate shm pool.
    pub fn redraw_surface(
        &mut self,
        qh: &QueueHandle<Self>,
        surface: &SessionLockSurface,
        width: u32,
        height: u32,
    ) {
        if width == 0 || height == 0 {
            return;
        }

        if self.appear_started.is_none() {
            self.appear_started = Some(Instant::now());
        }

        let now = Instant::now();
        let clock_text = chrono::Local::now()
            .format(&self.config.appearance.clock.format)
            .to_string();
        let date_text = chrono::Local::now()
            .format(&self.config.appearance.clock.date_format)
            .to_string();
        // A status line appearing (or changing) resets its slide-in.
        if self.auth_state != self.last_auth_state {
            self.status_anim_started = Some(now);
            self.last_auth_state = self.auth_state;
        }
        // Idle breath: one sine hump over the active window. When the window
        // ends, schedule the next one a full period out (the 1s clock tick
        // re-arms the animation timer once it's due).
        let breathe_t = if let Some(started) = self.breathe_started {
            let p = render::unit_progress(started, render::BREATHE_ACTIVE_MS);
            if p >= 1.0 {
                self.breathe_started = None;
                self.breathe_next_at =
                    Some(started + Duration::from_millis(render::BREATHE_PERIOD_MS));
                0.0
            } else {
                render::breathe_envelope(p)
            }
        } else {
            0.0
        };
        // While a PAM check runs, the status dots tick to signal progress.
        let status_text = match self.auth_state {
            AuthState::Checking => {
                let started = self.checking_started.unwrap_or(now);
                Some(format!("Checking{}", checking_dots(started)))
            }
            AuthState::Failed => {
                // Repeat failures get a counter so the user can tell the
                // locker apart from a stuck/corrupt one ("Wrong password"
                // alone reads identically every time).
                let n = self.failed_attempts.max(1);
                Some(if n > 1 {
                    format!("Wrong password — {n} failed attempts")
                } else {
                    "Wrong password".to_string()
                })
            }
            AuthState::AccountInvalid => Some("Account locked or expired".to_string()),
            AuthState::ConfigError => Some(
                "PAM config error — check logs (breadlock service not set up correctly)"
                    .to_string(),
            ),
            AuthState::Idle => None,
        };
        // D-Bus status line under the clock: now-playing and/or battery,
        // joined with a dot separator. Fades in with the appear animation
        // (render.rs keys `info_text` off `appear_t`, so no per-frame state
        // is needed here).
        let mut info_parts: Vec<&str> = Vec::new();
        if self.config.status.now_playing && !self.status_info.now_playing.is_empty() {
            info_parts.push(&self.status_info.now_playing);
        }
        if self.config.status.battery && !self.status_info.battery.is_empty() {
            info_parts.push(&self.status_info.battery);
        }
        let info_text = info_parts.join("  ·  ");

        // Idle auto-dim: ramp 0..1 over IDLE_DIM_RAMP_MS once the configured
        // idle threshold elapses with no keystrokes. 0 when disabled.
        let idle_dim = if self.config.animation.idle_dim_after_secs > 0 {
            let idle_s = self.last_activity.elapsed().as_secs_f64()
                - self.config.animation.idle_dim_after_secs as f64;
            if idle_s <= 0.0 {
                0.0
            } else {
                (idle_s / (render::IDLE_DIM_RAMP_MS as f64 / 1000.0)).min(1.0) as f32
            }
        } else {
            0.0
        };
        let status_t = self
            .status_anim_started
            .map(|t| render::unit_progress(t, render::STATUS_SLIDE_MS))
            .unwrap_or(1.0);

        // Minute rollover: keep the previous clock text in `clock_from`
        // until the crossfade completes. Do not overwrite the outgoing string.
        if let Some((_, started)) = self.clock_from {
            if render::unit_progress(started, render::CLOCK_CROSSFADE_MS) >= 1.0 {
                self.clock_from = None;
            }
        }
        if self.clock_from.is_none()
            && !self.last_clock_text.is_empty()
            && clock_text != self.last_clock_text
        {
            self.clock_from = Some((self.last_clock_text.clone(), now));
        }
        self.last_clock_text = clock_text.clone();
        let clock_old = self.clock_from.as_ref().map(|(from, started)| {
            (
                from.as_str(),
                render::unit_progress(*started, render::CLOCK_CROSSFADE_MS),
            )
        });

        let appear_t = self
            .appear_started
            .map(|t| render::unit_progress(t, render::APPEAR_MS))
            .unwrap_or(0.0);
        let unlock_t = self
            .unlocking
            .map(|t| render::unit_progress(t, render::UNLOCK_MS))
            .unwrap_or(0.0);
        let failed_t = self
            .failed_at
            .map(|t| render::unit_progress(t, render::SHAKE_MS))
            .unwrap_or(0.0);
        let dot_pop_t = self
            .last_keystroke
            .map(|t| render::unit_progress(t, render::DOT_POP_MS))
            .unwrap_or(1.0);

        let password_len = if self.password.is_empty() {
            self.password_display_len
        } else {
            self.password.chars().count()
        };

        let output_palette = self.palette_for_surface(surface);
        let inputs = render::FrameInputs {
            width,
            height,
            background: &self.background,
            palette: &output_palette,
            font_family: &self.config.appearance.font.family,
            clock_text: &clock_text,
            date_text: &date_text,
            clock_old,
            password_len,
            password: &self.password,
            reveal: self.reveal_held,
            caps_lock: self.caps_lock,
            layout_index: self.layout_index,
            idle_dim,
            failed: matches!(
                self.auth_state,
                AuthState::Failed | AuthState::AccountInvalid | AuthState::ConfigError
            ),
            failed_t,
            dot_pop_t,
            keystroke_age: self.last_keystroke.map(|t| t.elapsed().as_secs_f32()),
            t_secs: self.started.elapsed().as_secs_f32(),
            breathe_t,
            status_t,
            status_text: status_text.as_deref(),
            info_text: &info_text,
            appear_t,
            unlock_t,
            smooth_pan: !self.fast_anim_in_progress(),
        };

        // GPU path: the EGL surface renders the wallpaper (pan/veil in the
        // shader) and the software-composed chrome on top. Disjoint-field
        // borrows of `self` make `gpu` + `surfaces` + `text_renderer`
        // simultaneously mutable.
        let wants_gpu = self.gpu.is_some()
            && self
                .surfaces
                .iter()
                .any(|s| s.surface.wl_surface() == surface.wl_surface() && s.gpu.is_some());
        if wants_gpu {
            let Some(renderer) = self.gpu.as_mut() else {
                return;
            };
            let Some(lock_surface) = self
                .surfaces
                .iter_mut()
                .find(|s| s.surface.wl_surface() == surface.wl_surface())
            else {
                return;
            };
            let Some(gpu_surface) = lock_surface.gpu.as_mut() else {
                return;
            };
            if renderer.render_frame(gpu_surface, &inputs, &mut self.text_renderer) {
                self.arm_anim_if_needed(qh);
                return;
            }
            tracing::warn!("GPU frame failed — dropping EGL window and falling back to software");
        }

        // An EGL window on this wl_surface makes a later shm attach illegal;
        // Drop of GpuSurface destroys the native window first.
        if wants_gpu {
            if let Some(s) = self
                .surfaces
                .iter_mut()
                .find(|s| s.surface.wl_surface() == surface.wl_surface())
            {
                s.gpu = None;
            }
        }

        let Some(pixmap) = render::compose(&mut self.text_renderer, &inputs) else {
            return;
        };

        self.present_shm(surface, width, height, &pixmap);
        self.arm_anim_if_needed(qh);
    }

    fn present_shm(
        &mut self,
        surface: &SessionLockSurface,
        width: u32,
        height: u32,
        pixmap: &tiny_skia::Pixmap,
    ) {
        let Some(px) = (width as usize).checked_mul(height as usize) else {
            return;
        };
        let Some(len) = px.checked_mul(4) else {
            return;
        };
        if len == 0 {
            return;
        }
        if width > i32::MAX as u32 || height > i32::MAX as u32 {
            return;
        }
        let stride = match (width as usize).checked_mul(4) {
            Some(s) if s <= i32::MAX as usize => s as i32,
            _ => return,
        };

        let idx = self
            .surfaces
            .iter()
            .position(|s| s.surface.wl_surface() == surface.wl_surface());
        let Some(idx) = idx else {
            return;
        };

        if self.surfaces[idx].shm_pool.is_none() {
            match SlotPool::new(len, &self.shm) {
                Ok(pool) => self.surfaces[idx].shm_pool = Some(pool),
                Err(err) => {
                    tracing::error!(%err, "failed to allocate shm pool for lock surface redraw");
                    return;
                }
            }
        }

        let lock = &mut self.surfaces[idx];
        if let Some(buf) = &lock.shm_buffer {
            if buf.height() != height as i32 || buf.stride() != stride {
                lock.shm_buffer = None;
            }
        }

        let mut reused = false;
        if let Some(pool) = lock.shm_pool.as_mut() {
            if let Some(buf) = lock.shm_buffer.as_ref() {
                if let Some(canvas) = pool.canvas(buf) {
                    render::blit_to_shm(pixmap, canvas);
                    reused = true;
                }
            }
        }
        if !reused {
            let Some(pool) = lock.shm_pool.as_mut() else {
                return;
            };
            let (new_buf, canvas) = match pool.create_buffer(
                width as i32,
                height as i32,
                stride,
                wl_shm::Format::Argb8888,
            ) {
                Ok(pair) => pair,
                Err(err) => {
                    tracing::error!(%err, "failed to create shm buffer for lock surface redraw");
                    return;
                }
            };
            render::blit_to_shm(pixmap, canvas);
            lock.shm_buffer = Some(new_buf);
        }

        let Some(buf) = lock.shm_buffer.as_ref() else {
            return;
        };
        if buf.attach_to(surface.wl_surface()).is_err() {
            return;
        }
        surface
            .wl_surface()
            .damage_buffer(0, 0, width as i32, height as i32);
        surface.wl_surface().commit();
    }

    /// Resolve the palette for the monitor showing `surface`, reading it from
    /// the per-output cache (populated on first use) rather than parsing the
    /// on-disk JSON every frame. Falls back to the global palette when the
    /// output has no name, and — if the theme watcher isn't armed — to an
    /// uncached per-call disk read so a palette change is still picked up.
    /// Palette for the monitor showing `surface`. Served from the per-output
    /// cache (populated on first use, invalidated by the theme watcher) so a
    /// redraw doesn't parse `palettes/<output>.json` off disk every frame.
    /// Falls back to the global palette when the output has no name, and to an
    /// uncached per-call read when the theme watcher couldn't be armed.
    fn palette_for_surface(&self, surface: &SessionLockSurface) -> breadlock_ui::theme::Palette {
        let name = self
            .surfaces
            .iter()
            .find(|s| s.surface.wl_surface() == surface.wl_surface())
            .and_then(|s| self.output_state.info(&s.output))
            .and_then(|info| info.name);
        let Some(name) = name else {
            return self.palette.clone();
        };
        if self.theme_watch.is_none() {
            return breadlock_ui::theme::load_palette_for(&name);
        }
        if let Some(cached) = self.output_palettes.borrow().get(&name) {
            return cached.clone();
        }
        let palette = breadlock_ui::theme::load_palette_for(&name);
        self.output_palettes
            .borrow_mut()
            .insert(name, palette.clone());
        palette
    }

    /// Drop every cached per-output palette so the next redraw re-reads them.
    /// Called by the [`crate::theme_watch`] callback on a pywal regeneration.
    pub fn invalidate_palette_cache(&self) {
        self.output_palettes.borrow_mut().clear();
    }

    /// Redraws every currently-configured surface — used for the clock tick
    /// and after any password/auth-state change.
    pub fn redraw_all(&mut self, qh: &QueueHandle<Self>) {
        let surfaces: Vec<(SessionLockSurface, u32, u32)> = self
            .surfaces
            .iter()
            .map(|s| {
                let scale = s.scale.max(1) as u32;
                (
                    s.surface.clone(),
                    s.width.saturating_mul(scale),
                    s.height.saturating_mul(scale),
                )
            })
            .collect();
        for (surface, width, height) in surfaces {
            self.redraw_surface(qh, &surface, width, height);
        }
        self.complete_unlock_if_ready();
    }

    fn appear_in_progress(&self) -> bool {
        self.appear_started
            .map(|t| t.elapsed() < Duration::from_millis(render::APPEAR_MS))
            .unwrap_or(true)
    }

    fn unlock_in_progress(&self) -> bool {
        self.unlocking
            .map(|t| t.elapsed() < Duration::from_millis(render::UNLOCK_MS))
            .unwrap_or(false)
    }

    fn failed_shake_in_progress(&self) -> bool {
        self.failed_at
            .map(|t| t.elapsed() < Duration::from_millis(render::SHAKE_MS))
            .unwrap_or(false)
    }

    fn dot_pop_in_progress(&self) -> bool {
        self.last_keystroke
            .map(|t| t.elapsed() < Duration::from_millis(render::DOT_POP_MS))
            .unwrap_or(false)
    }

    fn clock_fade_in_progress(&self) -> bool {
        self.clock_from
            .as_ref()
            .map(|(_, t)| t.elapsed() < Duration::from_millis(render::CLOCK_CROSSFADE_MS))
            .unwrap_or(false)
    }

    fn status_slide_in_progress(&self) -> bool {
        self.status_anim_started
            .map(|t| t.elapsed() < Duration::from_millis(render::STATUS_SLIDE_MS))
            .unwrap_or(false)
    }

    fn breathe_in_progress(&self) -> bool {
        self.breathe_started.is_some()
    }

    /// An idle breath is due when the cycle timer says so (and no breath is
    /// already running). The 1s clock tick calls `redraw_all`, which arms the
    /// animation timer through here — so the screen stays asleep between
    /// breaths.
    fn breathe_due(&self) -> bool {
        if !self.config.animation.breathe || self.breathe_started.is_some() {
            return false;
        }
        self.breathe_next_at
            .map(|t| Instant::now() >= t)
            .unwrap_or(false)
    }

    fn idle_dim_in_progress(&self) -> bool {
        if self.config.animation.idle_dim_after_secs == 0 {
            return false;
        }
        let idle_s = self.last_activity.elapsed().as_secs_f64();
        let threshold = self.config.animation.idle_dim_after_secs as f64;
        let ramp_s = render::IDLE_DIM_RAMP_MS as f64 / 1000.0;
        idle_s > threshold && idle_s < threshold + ramp_s
    }

    fn caret_blink_in_progress(&self) -> bool {
        if self.unlocking.is_some() {
            return false;
        }
        let len = if self.password.is_empty() {
            self.password_display_len
        } else {
            self.password.chars().count()
        };
        len > 0
    }

    /// Any effect still running that needs the animation timer: the fast ones
    /// (entrance, unlock flash+fade, shake, dot pop, clock rollover, status
    /// slide, a live PAM check) plus the slow ones (idle breath, Ken Burns
    /// pan, idle dim ramp, caret blink) which run at a reduced cadence — see
    /// `tick_animation`.
    fn anim_in_progress(&self) -> bool {
        self.unlocking.is_some()
            || self.appear_in_progress()
            || self.failed_shake_in_progress()
            || self.dot_pop_in_progress()
            || self.clock_fade_in_progress()
            || self.status_slide_in_progress()
            || self.breathe_in_progress()
            || self.breathe_due()
            || self.auth_state == AuthState::Checking
            || self.background.ken_burns()
            || self.idle_dim_in_progress()
            || self.caret_blink_in_progress()
    }

    /// Keep requesting frames while any effect is running.
    fn arm_anim_if_needed(&mut self, qh: &QueueHandle<Self>) {
        if self.anim_timer_armed || !self.anim_in_progress() {
            return;
        }
        // A breath that's due starts its window now, so the first ticked
        // frame already shows the start of the hump.
        if self.breathe_due() {
            self.breathe_started = Some(Instant::now());
        }
        self.anim_timer_armed = true;
        let qh = qh.clone();
        if self
            .loop_handle
            .insert_source(
                Timer::from_duration(Duration::from_millis(render::ANIM_FRAME_MS)),
                move |_, _, state| state.tick_animation(&qh),
            )
            .is_err()
        {
            tracing::error!("failed to arm lock animation timer");
            self.anim_timer_armed = false;
        }
    }

    /// A 60 fps animation is in flight (everything except the slow idle
    /// effects: idle breath, Ken Burns pan, idle dim, caret blink). Drives
    /// both the timer cadence and whether background frames get sub-pixel
    /// panning.
    fn fast_anim_in_progress(&self) -> bool {
        self.appear_in_progress()
            || self.unlock_in_progress()
            || self.failed_shake_in_progress()
            || self.dot_pop_in_progress()
            || self.clock_fade_in_progress()
            || self.status_slide_in_progress()
            || self.auth_state == AuthState::Checking
    }

    fn tick_animation(&mut self, qh: &QueueHandle<Self>) -> TimeoutAction {
        self.redraw_all(qh);
        if self.unlocking.is_some() && !self.unlock_in_progress() {
            self.anim_timer_armed = false;
            TimeoutAction::Drop
        } else if self.anim_in_progress() {
            // Slow effects (idle breath, Ken Burns, dim, caret) don't need
            // 60fps — halve the redraw cost for them. Everything else stays
            // at ~60Hz.
            let fast = self.fast_anim_in_progress();
            TimeoutAction::ToDuration(Duration::from_millis(if fast {
                render::ANIM_FRAME_MS
            } else {
                render::SLOW_FRAME_MS
            }))
        } else {
            self.anim_timer_armed = false;
            TimeoutAction::Drop
        }
    }

    /// After the unlock fade reaches t==1, send compositor `unlock` and
    /// exit. Not called until then — dying mid-fade stays locked.
    pub fn complete_unlock_if_ready(&mut self) {
        let Some(started) = self.unlocking else {
            return;
        };
        if started.elapsed() < Duration::from_millis(render::UNLOCK_MS) {
            return;
        }
        if let Some(lock) = self.session_lock.take() {
            tracing::info!("unlock fade complete");
            lock.unlock();
            crate::bread_events::emit_unlocked();
        }
        self.exit = true;
    }

    /// After a failed attempt, clears the red UI once `input.fail_timeout_ms`
    /// has elapsed — unless a newer fail (or the user typing) has moved the
    /// generation. Input is not blocked during Failed.
    pub fn schedule_clear_failed(&self, qh: QueueHandle<Self>) {
        let timeout = Duration::from_millis(self.config.input.fail_timeout_ms);
        let gen = self.failed_generation;
        let _ =
            self.loop_handle
                .insert_source(Timer::from_duration(timeout), move |_, _, state| {
                    if fail_timer_applies(gen, state.failed_generation, state.auth_state) {
                        state.auth_state = AuthState::Idle;
                        state.failed_at = None;
                        state.password_display_len = 0;
                        state.redraw_all(&qh);
                    }
                    TimeoutAction::Drop
                });
    }

    /// Record a Failed / AccountInvalid / ConfigError and bump the
    /// generation so an older fail-clear timer cannot wipe this one.
    pub fn enter_fail(&mut self, next: AuthState) {
        self.auth_state = next;
        self.failed_at = Some(Instant::now());
        self.checking_started = None;
        self.failed_generation = self.failed_generation.wrapping_add(1);
    }
}

/// The animated ellipsis for the "Checking" status while a PAM check runs:
/// cycles "", ".", "..", "…" every ~500ms (driven by time since `started`).
fn checking_dots(started: Instant) -> &'static str {
    match (started.elapsed().as_secs_f32() * 2.0) as usize % 4 {
        0 => "",
        1 => ".",
        2 => "..",
        _ => "…",
    }
}

/// A fail-clear timer only fires if its captured generation is still current
/// and the UI is still in a fail-style state.
fn fail_timer_applies(timer_gen: u64, current_gen: u64, auth: AuthState) -> bool {
    timer_gen == current_gen
        && matches!(
            auth,
            AuthState::Failed | AuthState::AccountInvalid | AuthState::ConfigError
        )
}

impl ShmHandler for AppState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for AppState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checking_dots_with_stale_instant_is_not_empty() {
        let started = Instant::now() - Duration::from_millis(750);
        assert_ne!(checking_dots(started), "");
    }

    #[test]
    fn checking_dots_at_now_is_empty_or_dot() {
        // Fresh Instant: elapsed ≈ 0 → "".
        assert_eq!(checking_dots(Instant::now()), "");
    }

    #[test]
    fn fail_timer_ignores_stale_generation() {
        assert!(!fail_timer_applies(1, 2, AuthState::Failed));
        assert!(fail_timer_applies(3, 3, AuthState::Failed));
        assert!(fail_timer_applies(1, 1, AuthState::AccountInvalid));
        assert!(fail_timer_applies(1, 1, AuthState::ConfigError));
        assert!(!fail_timer_applies(1, 1, AuthState::Idle));
        assert!(!fail_timer_applies(1, 1, AuthState::Checking));
    }
}

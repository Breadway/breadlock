use smithay_client_toolkit::compositor::CompositorState;
use smithay_client_toolkit::output::OutputState;
use smithay_client_toolkit::reexports::calloop::channel::Sender;
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::LoopHandle;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::seat::SeatState;
use smithay_client_toolkit::session_lock::{SessionLock, SessionLockState, SessionLockSurface};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use std::time::Duration;
use wayland_client::protocol::{wl_keyboard, wl_output, wl_shm};
use wayland_client::{Connection, QueueHandle};

use crate::auth::AuthResult;
use crate::background::Background;
use crate::config::Config;
use crate::render;

/// Per-output lock surface plus the size the compositor last `configure`d it
/// to (0x0 until the first configure arrives). `output` is kept so
/// `output_destroyed` can find and drop the surface belonging to an unplugged
/// monitor — without it, hotplug/unplug cycles only ever grow `surfaces`.
pub struct LockSurface {
    pub surface: SessionLockSurface,
    pub output: wl_output::WlOutput,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthState {
    Idle,
    /// A PAM check is running on its own thread; input is ignored until it
    /// resolves so a second Enter can't race the first attempt.
    Checking,
    /// The password (or account state) was rejected by PAM — an ordinary
    /// wrong-password/locked-account outcome the user can retry.
    Failed,
    /// PAM itself failed to initialize (e.g. `/etc/pam.d/breadlock` is
    /// missing or unreadable) — this is a config/deployment problem, not
    /// something the user's password can fix. Rendered with a distinct
    /// message so a broken install doesn't look like an endless string of
    /// typos with no way to discover the real cause. See `main.rs`'s
    /// auth-result callback, which is the only place this is set.
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

    pub config: Config,
    pub palette: breadlock_ui::theme::Palette,
    pub background: Background,
    pub text_renderer: breadlock_ui::painter::TextRenderer,

    pub username: String,
    /// Wrapped in `Zeroizing` so the buffer is wiped on every drop/replace
    /// (e.g. when `submit()` swaps in a fresh one) rather than just
    /// deallocated with the password bytes left sitting in freed heap
    /// memory. Individual edits (backspace, clear) still need their own
    /// explicit zeroing — see `input/keyboard.rs` — since `Zeroizing` only
    /// hooks `Drop`, not in-place mutation.
    pub password: zeroize::Zeroizing<String>,
    pub auth_state: AuthState,
    pub auth_tx: Sender<AuthResult>,

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

        let clock_text = chrono::Local::now()
            .format(&self.config.appearance.clock.format)
            .to_string();
        let status_text = match self.auth_state {
            AuthState::Checking => Some("Checking…".to_string()),
            AuthState::Failed => Some("Wrong password".to_string()),
            AuthState::ConfigError => {
                Some("PAM config error — check logs (breadlock service not set up correctly)".to_string())
            }
            AuthState::Idle => None,
        };

        let inputs = render::FrameInputs {
            width,
            height,
            background: &self.background,
            palette: &self.palette,
            font_family: &self.config.appearance.font.family,
            clock_text: &clock_text,
            password_len: self.password.len(),
            failed: matches!(self.auth_state, AuthState::Failed | AuthState::ConfigError),
            status_text: status_text.as_deref(),
        };

        let Some(pixmap) = render::compose(&mut self.text_renderer, &inputs) else {
            return;
        };

        let stride = width as usize * 4;
        let pool =
            smithay_client_toolkit::shm::raw::RawPool::new(stride * height as usize, &self.shm);
        let mut pool = match pool {
            Ok(pool) => pool,
            Err(err) => {
                tracing::error!(%err, "failed to allocate shm pool for lock surface redraw");
                return;
            }
        };
        render::blit_to_shm(&pixmap, pool.mmap());

        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            wl_shm::Format::Argb8888,
            (),
            qh,
        );

        surface.wl_surface().attach(Some(&buffer), 0, 0);
        surface
            .wl_surface()
            .damage_buffer(0, 0, width as i32, height as i32);
        surface.wl_surface().commit();
        buffer.destroy();
    }

    /// Redraws every currently-configured surface — used for the clock tick
    /// and after any password/auth-state change.
    pub fn redraw_all(&mut self, qh: &QueueHandle<Self>) {
        let surfaces: Vec<(SessionLockSurface, u32, u32)> = self
            .surfaces
            .iter()
            .map(|s| (s.surface.clone(), s.width, s.height))
            .collect();
        for (surface, width, height) in surfaces {
            self.redraw_surface(qh, &surface, width, height);
        }
    }

    /// After a failed attempt, clears the "wrong password" state (and
    /// re-enables the red pill) once `input.fail_timeout_ms` has elapsed —
    /// unless the user already cleared it themselves by typing again.
    pub fn schedule_clear_failed(&self, qh: QueueHandle<Self>) {
        let timeout = Duration::from_millis(self.config.input.fail_timeout_ms);
        let _ =
            self.loop_handle
                .insert_source(Timer::from_duration(timeout), move |_, _, state| {
                    if matches!(state.auth_state, AuthState::Failed | AuthState::ConfigError) {
                        state.auth_state = AuthState::Idle;
                        state.redraw_all(&qh);
                    }
                    TimeoutAction::Drop
                });
    }
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

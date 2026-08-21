mod auth;
mod background;
mod bread_events;
mod config;
mod gpu;
mod input;
mod lock;
mod render;
mod state;

use smithay_client_toolkit::compositor::CompositorState;
use smithay_client_toolkit::output::OutputState;
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::registry::RegistryState;
use smithay_client_toolkit::seat::SeatState;
use smithay_client_toolkit::session_lock::SessionLockState;
use smithay_client_toolkit::shm::Shm;
use std::time::Duration;
use wayland_client::globals::registry_queue_init;
use wayland_client::{protocol::wl_buffer, Connection, QueueHandle};

use background::Background;
use bread_utils::singleton::{try_acquire, Acquire};
use state::{AppState, AuthState, LockSurface};

#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Lock,
    Listen,
    Help,
}

fn parse_mode<I, S>(args: I) -> Result<Mode, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut args = args.into_iter();
    match args.next().as_ref().map(|s| s.as_ref()) {
        None => Ok(Mode::Lock),
        Some("listen") if args.next().is_none() => Ok(Mode::Listen),
        Some("-h" | "--help" | "help") => Ok(Mode::Help),
        Some("listen") => Err("listen takes no arguments".into()),
        Some(other) => Err(format!("unknown argument '{other}'")),
    }
}

fn print_usage() {
    eprintln!(
        "Usage: breadlock [listen]\n\
         \n\
         (no args)   lock this session — hypridle lock_cmd / Super+L via loginctl lock-session\n\
         listen      subscribe to bread.command.lock.lock / unlock so both work while unlocked\n\
         \n\
         Session-level: loginctl lock-session / unlock-session.\n\
         See EVENTS.md for the bus contract."
    );
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    match parse_mode(std::env::args().skip(1)) {
        Ok(Mode::Lock) => run_lock(),
        Ok(Mode::Listen) => run_listen(),
        Ok(Mode::Help) => print_usage(),
        Err(err) => {
            eprintln!("breadlock: {err}");
            print_usage();
            std::process::exit(2);
        }
    }
}

/// Long-running subscriber so `bread.command.lock.lock` / `.unlock` work
/// while the session is unlocked. The locker process also subscribes;
/// this path is what actually starts breadlock (the same no-args
/// invocation hypridle uses) and what runs `loginctl unlock-session`
/// when a locker is up. One listen process per session.
fn run_listen() {
    let _guard = match try_acquire(bread_events::LISTEN_APP) {
        Ok(Acquire::Acquired(g)) => g,
        Ok(Acquire::HeldByOther(pid)) => {
            tracing::info!(?pid, "breadlock listen already running");
            return;
        }
        Err(err) => {
            tracing::error!(%err, "failed to acquire listen singleton");
            std::process::exit(1);
        }
    };

    // Common when started early in the session (exec-once). The locker we
    // spawn needs WAYLAND_DISPLAY; we stay up either way so a later command
    // still has a subscriber.
    for _ in 0..20 {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        tracing::warn!("WAYLAND_DISPLAY not set; spawned breadlock will fail until it is");
    }

    let _commands = bread_events::subscribe_commands();
    tracing::info!("listening for bread.command.lock.lock / unlock");
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

fn run_lock() {
    let _locker_guard = match try_acquire(bread_events::APP_ID) {
        Ok(Acquire::Acquired(g)) => Some(g),
        Ok(Acquire::HeldByOther(pid)) => {
            tracing::info!(?pid, "session already locked by another breadlock; exiting");
            return;
        }
        Err(err) => {
            // Refusing to lock because flock failed would be worse than
            // running without the singleton — hypridle still needs a locker.
            tracing::warn!(%err, "could not acquire lock singleton; continuing");
            None
        }
    };

    // Honor bread.command.lock.lock / unlock while this locker is up
    // (already-locked is bread.lock.lock.done; unlock is loginctl, not
    // compositor unlock()). Unlocked-path commands need `breadlock listen`.
    let _commands = bread_events::subscribe_commands();

    let username = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| {
            tracing::error!("neither $USER nor $LOGNAME is set — refusing to start without a username to authenticate");
            std::process::exit(1);
        });

    let config = config::load();
    let palette = breadlock_ui::theme::load_palette();
    let background = Background::load(&config.appearance.background, &palette);

    let conn = Connection::connect_to_env().expect("failed to connect to the Wayland display — breadlock must run inside an active Wayland session");
    // GPU background rendering (EGL/GLES2). Any failure is non-fatal: the
    // software renderer takes over. `run_lock` is only ever entered in Lock
    // mode (the listen subscriber never renders), so no mode check here.
    let gpu = gpu::GpuRenderer::new(&conn, &config.appearance.background, &palette);
    if gpu.is_some() {
        tracing::info!("GPU background rendering enabled (EGL/GLES2)");
    } else {
        tracing::warn!("GPU background rendering unavailable — using the software renderer");
    }
    let (globals, event_queue) =
        registry_queue_init::<AppState>(&conn).expect("failed to initialize Wayland registry");
    let qh: QueueHandle<AppState> = event_queue.handle();
    let mut event_loop: EventLoop<AppState> =
        EventLoop::try_new().expect("failed to create the calloop event loop");
    let loop_handle = event_loop.handle();

    let auth_result_qh = qh.clone();
    let auth_tx = auth::register(&loop_handle, move |state: &mut AppState, result| {
        match result {
            Ok(()) => {
                // Keep the lock surfaces up and fade the overlay out.
                // Compositor unlock() runs only after UNLOCK_MS — dying
                // mid-fade is fail-secure (session stays locked).
                tracing::info!("authenticated, fading out");
                if state.unlocking.is_none() {
                    state.unlocking = Some(std::time::Instant::now());
                }
            }
            Err(err) => {
                match err {
                    // A broken PAM setup (missing/invalid /etc/pam.d/breadlock,
                    // context init failure) is a config problem, not a typo —
                    // rendering it identically to "wrong password" would lock
                    // the user out with zero indication of what's actually
                    // wrong. Log loudly and show a distinct on-screen message.
                    auth::AuthError::ContextInit => {
                        tracing::error!(
                            %err,
                            "PAM context initialization failed — check /etc/pam.d/breadlock exists and is valid; authentication cannot succeed until this is fixed"
                        );
                        state.auth_state = AuthState::ConfigError;
                        state.failed_at = Some(std::time::Instant::now());
                    }
                    auth::AuthError::Authenticate | auth::AuthError::AccountInvalid => {
                        tracing::warn!(%err, "authentication failed");
                        state.auth_state = AuthState::Failed;
                        state.failed_at = Some(std::time::Instant::now());
                    }
                }
                state.schedule_clear_failed(auth_result_qh.clone());
            }
        }
        state.redraw_all(&auth_result_qh);
    });

    let compositor_state =
        CompositorState::bind(&globals, &qh).expect("compositor global not advertised");
    let output_state = OutputState::new(&globals, &qh);
    let shm = Shm::bind(&globals, &qh).expect("wl_shm global not advertised");
    let session_lock_state = SessionLockState::new(&globals, &qh);

    let mut app_state = AppState {
        loop_handle: loop_handle.clone(),
        conn: conn.clone(),
        compositor_state,
        output_state,
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        shm,
        session_lock_state,
        session_lock: None,
        surfaces: Vec::new(),
        keyboard: None,
        config,
        palette,
        background,
        gpu,
        text_renderer: breadlock_ui::painter::TextRenderer::new(),
        username,
        // Pre-reserve capacity so ordinary typing doesn't reallocate — a
        // reallocation leaves the old (unzeroized) backing buffer, with the
        // password bytes still in it, on the heap.
        password: zeroize::Zeroizing::new(String::with_capacity(128)),
        auth_state: AuthState::Idle,
        auth_tx,
        started: std::time::Instant::now(),
        appear_started: None,
        unlocking: None,
        last_keystroke: None,
        failed_at: None,
        last_clock_text: String::new(),
        clock_anim_started: None,
        status_anim_started: None,
        last_auth_state: AuthState::Idle,
        breathe_started: None,
        breathe_next_at: Some(
            std::time::Instant::now()
                + std::time::Duration::from_millis(render::BREATHE_INITIAL_DELAY_MS),
        ),
        anim_timer_armed: false,
        exit: false,
    };

    // Request the lock immediately — locking the session is the entire
    // purpose of this process, not a toggle. Per ext-session-lock-v1, a lock
    // surface must exist for every output *before* the compositor sends
    // `locked` (see SessionLockHandler::locked's doc comment upstream).
    let session_lock = app_state
        .session_lock_state
        .lock(&qh)
        .expect("compositor does not support ext-session-lock-v1 — cannot lock the session");
    for output in app_state.output_state.outputs() {
        let surface = app_state.compositor_state.create_surface(&qh);
        let lock_surface = session_lock.create_lock_surface(surface, &output, &qh);
        app_state.surfaces.push(LockSurface {
            surface: lock_surface,
            output,
            width: 0,
            height: 0,
            gpu: None,
        });
    }
    app_state.session_lock = Some(session_lock);

    WaylandSource::new(conn, event_queue)
        .insert(loop_handle.clone())
        .expect("failed to register the Wayland source on the event loop");

    // Redraw every surface once a second so the clock stays live even with
    // no keyboard input.
    loop_handle
        .insert_source(
            Timer::from_duration(Duration::from_secs(1)),
            move |_, _, state| {
                state.redraw_all(&qh);
                TimeoutAction::ToDuration(Duration::from_secs(1))
            },
        )
        .expect("failed to register the clock-tick timer");

    // A dispatch error here is the one path that can end this process while
    // the session lock is still up: `SessionLockInner::drop` deliberately
    // does *not* send `unlock`, only `destroy` (see the crate's own doc
    // comment — "choosing not to unlock here results in us failing secure"),
    // so an abrupt exit stays fail-secure at the protocol level; the failure
    // mode is a frozen/unusable lock screen (Hyprland's "lock client
    // crashed" state), not an unlocked one. We do NOT call `.unlock()` from
    // here — doing so on an error path would make an unattended failure
    // capable of unlocking the session, i.e. turn a fail-secure bug into a
    // fail-open one. Instead: tolerate a burst of transient errors (a single
    // `dispatch()` hiccup shouldn't be fatal) and only give up, loudly, after
    // several consecutive failures.
    const MAX_CONSECUTIVE_DISPATCH_ERRORS: u32 = 5;
    let mut consecutive_errors = 0u32;
    while !app_state.exit {
        match event_loop.dispatch(Duration::from_millis(250), &mut app_state) {
            Ok(()) => {
                consecutive_errors = 0;
                // Backup if the 16ms anim timer failed to register: the
                // 250ms dispatch timeout (or the 1s clock tick) still
                // completes a finished unlock fade.
                app_state.complete_unlock_if_ready();
            }
            Err(err) => {
                consecutive_errors += 1;
                tracing::error!(
                    %err,
                    consecutive_errors,
                    "event loop dispatch failed — session remains locked (fail-secure); \
                     if this persists the lock screen may become unresponsive and require \
                     a VT switch or `loginctl` to recover"
                );
                if consecutive_errors >= MAX_CONSECUTIVE_DISPATCH_ERRORS {
                    tracing::error!(
                        "giving up after {consecutive_errors} consecutive dispatch failures; \
                         exiting WITHOUT unlocking — this is intentional (fail-secure), but \
                         the screen will likely be stuck and need a VT switch to recover"
                    );
                    break;
                }
            }
        }
    }

    // Make sure the compositor actually receives the unlock/destroy
    // requests queued above (from a successful auth) before the process
    // exits. This is a no-op if we got here via the dispatch-error path
    // above, since nothing queued an unlock in that case.
    let _ = app_state.conn.roundtrip();
}

smithay_client_toolkit::delegate_compositor!(AppState);
smithay_client_toolkit::delegate_output!(AppState);
smithay_client_toolkit::delegate_session_lock!(AppState);
smithay_client_toolkit::delegate_shm!(AppState);
smithay_client_toolkit::delegate_seat!(AppState);
smithay_client_toolkit::delegate_keyboard!(AppState);
smithay_client_toolkit::delegate_registry!(AppState);
wayland_client::delegate_noop!(AppState: ignore wl_buffer::WlBuffer);

#[cfg(test)]
mod tests {
    use super::{parse_mode, Mode};

    #[test]
    fn parse_mode_no_args_is_lock() {
        let args: [&str; 0] = [];
        assert_eq!(parse_mode(args), Ok(Mode::Lock));
    }

    #[test]
    fn parse_mode_listen() {
        assert_eq!(parse_mode(["listen"]), Ok(Mode::Listen));
    }

    #[test]
    fn parse_mode_help() {
        assert_eq!(parse_mode(["--help"]), Ok(Mode::Help));
        assert_eq!(parse_mode(["-h"]), Ok(Mode::Help));
        assert_eq!(parse_mode(["help"]), Ok(Mode::Help));
    }

    #[test]
    fn parse_mode_rejects_unknown_and_extra_listen_args() {
        assert!(parse_mode(["unlock"]).is_err());
        assert!(parse_mode(["listen", "--foreground"]).is_err());
    }
}

mod auth;
mod background;
mod config;
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
use state::{AppState, AuthState, LockSurface};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

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
                tracing::info!("authenticated, unlocking");
                if let Some(lock) = state.session_lock.take() {
                    lock.unlock();
                }
                state.exit = true;
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
                    }
                    auth::AuthError::Authenticate | auth::AuthError::AccountInvalid => {
                        tracing::warn!(%err, "authentication failed");
                        state.auth_state = AuthState::Failed;
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
        text_renderer: breadlock_ui::painter::TextRenderer::new(),
        username,
        // Pre-reserve capacity so ordinary typing doesn't reallocate — a
        // reallocation leaves the old (unzeroized) backing buffer, with the
        // password bytes still in it, on the heap.
        password: zeroize::Zeroizing::new(String::with_capacity(128)),
        auth_state: AuthState::Idle,
        auth_tx,
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
            Ok(()) => consecutive_errors = 0,
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

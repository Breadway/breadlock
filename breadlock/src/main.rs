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
                tracing::warn!(%err, "authentication failed");
                state.auth_state = AuthState::Failed;
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
        password: String::new(),
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

    while !app_state.exit {
        if let Err(err) = event_loop.dispatch(Duration::from_millis(250), &mut app_state) {
            tracing::error!(%err, "event loop dispatch failed");
            break;
        }
    }

    // Make sure the compositor actually receives the unlock/destroy
    // requests queued above before the process exits.
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

use smithay_client_toolkit::session_lock::{
    SessionLock, SessionLockHandler, SessionLockSurface, SessionLockSurfaceConfigure,
};
use wayland_client::{Connection, QueueHandle};

use crate::state::AppState;

impl SessionLockHandler for AppState {
    fn locked(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, session_lock: SessionLock) {
        tracing::info!("session locked");
        self.session_lock = Some(session_lock);
    }

    /// The compositor denied the lock request, or ended an active lock out
    /// from under us (e.g. protocol error). Either way there's no lock left
    /// to protect, so the only sane move is to exit — staying resident
    /// unlocked would be worse than not running at all.
    fn finished(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _session_lock: SessionLock,
    ) {
        tracing::warn!("compositor ended the session lock; exiting");
        self.session_lock = None;
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        surface: SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        _serial: u32,
    ) {
        let (width, height) = configure.new_size;
        if let Some(s) = self
            .surfaces
            .iter_mut()
            .find(|s| s.surface.wl_surface() == surface.wl_surface())
        {
            s.width = width;
            s.height = height;
        }
        self.redraw_surface(qh, &surface, width, height);
    }
}

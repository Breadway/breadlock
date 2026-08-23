use smithay_client_toolkit::session_lock::{
    SessionLock, SessionLockHandler, SessionLockSurface, SessionLockSurfaceConfigure,
};
use wayland_client::{Connection, QueueHandle};

use crate::state::AppState;

impl SessionLockHandler for AppState {
    fn locked(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, session_lock: SessionLock) {
        tracing::info!("session locked");
        self.session_lock = Some(session_lock);
        crate::bread_events::emit_locked();
    }

    /// The compositor denied the lock request, or ended an active lock out
    /// from under us (e.g. protocol error). Either way there's no lock left
    /// to protect, so the only sane move is to exit — staying resident
    /// unlocked would be worse than not running at all.
    ///
    /// If `locked` already arrived, dropping the object sends `destroy()`
    /// which is a protocol error; send `unlock_and_destroy` first.
    fn finished(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _session_lock: SessionLock,
    ) {
        // PAM unlock already took the stored lock; don't unlock/emit again.
        let Some(lock) = self.session_lock.take() else {
            self.exit = true;
            return;
        };
        if lock.is_locked() {
            tracing::warn!("compositor ended an active session lock; unlocking then exiting");
            lock.unlock();
            crate::bread_events::emit_unlocked();
        } else {
            tracing::warn!("compositor ended the session lock before it was acquired; exiting");
        }
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
        let (buf_w, buf_h) = if let Some(s) = self
            .surfaces
            .iter_mut()
            .find(|s| s.surface.wl_surface() == surface.wl_surface())
        {
            s.width = width;
            s.height = height;
            let scale = s.scale.max(1);
            surface.wl_surface().set_buffer_scale(scale);
            let buf_w = width.saturating_mul(scale as u32);
            let buf_h = height.saturating_mul(scale as u32);
            // Lazily wrap the surface in EGL on its first (sized) configure;
            // resize the EGL window on subsequent ones. Size is buffer pixels.
            if let Some(renderer) = &self.gpu {
                match &mut s.gpu {
                    None => s.gpu = renderer.create_surface(surface.wl_surface(), buf_w, buf_h),
                    Some(gs) => gs.resize(buf_w, buf_h),
                }
            }
            (buf_w, buf_h)
        } else {
            (width, height)
        };
        self.redraw_surface(qh, &surface, buf_w, buf_h);
    }
}

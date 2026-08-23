use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use wayland_client::protocol::{wl_output, wl_surface};
use wayland_client::{Connection, QueueHandle};

use crate::state::{AppState, LockSurface};

impl CompositorHandler for AppState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        // Protocol: buffer scale must be > 0. Treat 0 (or negative) as 1.
        let scale = new_factor.max(1);
        let (lock_surface, width, height) = {
            let Some(s) = self
                .surfaces
                .iter_mut()
                .find(|s| s.surface.wl_surface() == surface)
            else {
                return;
            };
            s.scale = scale;
            surface.set_buffer_scale(scale);
            let width = s.width.saturating_mul(scale as u32);
            let height = s.height.saturating_mul(scale as u32);
            if let Some(gs) = s.gpu.as_mut() {
                gs.resize(width, height);
            }
            (s.surface.clone(), width, height)
        };
        self.redraw_surface(qh, &lock_surface, width, height);
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for AppState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    /// A monitor appeared. If we're already locked, give it a lock surface
    /// too — the initial set (for outputs present at lock time) is created
    /// once in `main`, right after `SessionLockState::lock`.
    fn new_output(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        // SCTK also fires `new_output` for outputs already bound at
        // registry-init; `main` already created a lock surface for those.
        // One lock surface per output is a protocol requirement.
        if self.surfaces.iter().any(|s| s.output == output) {
            return;
        }
        let Some(session_lock) = self.session_lock.clone() else {
            return;
        };
        let surface = CompositorState::create_surface(&self.compositor_state, qh);
        let lock_surface = session_lock.create_lock_surface(surface, &output, qh);
        self.surfaces.push(LockSurface {
            surface: lock_surface,
            output,
            width: 0,
            height: 0,
            scale: 1,
            gpu: None,
            shm_pool: None,
            shm_buffer: None,
        });
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    /// A monitor disappeared (unplug, or Hyprland dropping/recreating it on
    /// a mode change). Drop the lock surface tied to it — otherwise
    /// `surfaces` only ever grows across hotplug cycles and `redraw_all`
    /// keeps trying to commit to a surface whose output is gone.
    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.surfaces.retain(|s| s.output != output);
    }
}

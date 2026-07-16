use smithay_client_toolkit::seat::keyboard::{
    KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers,
};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use wayland_client::protocol::{wl_keyboard, wl_seat, wl_surface};
use wayland_client::{Connection, QueueHandle};
use zeroize::Zeroize;

use crate::auth;
use crate::state::{AppState, AuthState};

impl SeatHandler for AppState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            // Plain `get_keyboard` never populates SCTK's internal repeat
            // timer, so `KeyboardHandler::repeat_key` below only ever fires
            // for compositors that implement server-side key repeat
            // (wl_keyboard >= v10's "repeated" pseudo key-state) themselves —
            // Hyprland does not reliably do this. `get_keyboard_with_repeat`
            // registers SCTK's own client-side repeat timer driven by the
            // compositor's `repeat_info` (delay/rate); if a compositor *does*
            // do server-side repeat it advertises `rate = 0`, which this
            // timer already treats as disabled, so the two mechanisms can't
            // double-fire.
            let repeat_qh = qh.clone();
            let loop_handle = self.loop_handle.clone();
            match self.seat_state.get_keyboard_with_repeat(
                qh,
                &seat,
                None,
                loop_handle,
                Box::new(move |state: &mut AppState, _keyboard, event| {
                    state.handle_key(&repeat_qh, event);
                }),
            ) {
                Ok(keyboard) => self.keyboard = Some(keyboard),
                Err(err) => tracing::error!(%err, "failed to bind keyboard"),
            }
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard {
            if let Some(keyboard) = self.keyboard.take() {
                keyboard.release();
            }
        }
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {
    }
}

impl KeyboardHandler for AppState {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
        _raw: &[u32],
        _keysyms: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        self.handle_key(qh, event);
    }

    fn repeat_key(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        self.handle_key(qh, event);
    }

    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _modifiers: Modifiers,
        _raw_modifiers: RawModifiers,
        _layout: u32,
    ) {
    }
}

impl AppState {
    fn handle_key(&mut self, qh: &QueueHandle<Self>, event: KeyEvent) {
        // Ignore all input while a PAM check is in flight so a fast second
        // Enter can't race the first attempt.
        if self.auth_state == AuthState::Checking {
            return;
        }

        match event.keysym {
            Keysym::Return | Keysym::KP_Enter => self.submit(),
            Keysym::BackSpace => {
                if let Some((idx, _)) = self.password.char_indices().last() {
                    // Plain `String::pop()` shrinks the logical length but
                    // leaves the removed character's bytes sitting in the
                    // buffer's spare capacity. Zero them explicitly before
                    // truncating.
                    //
                    // SAFETY: `idx` comes from `char_indices()`, so it is a
                    // valid char boundary; the retained prefix `[..idx]`
                    // is untouched and still valid UTF-8, and we truncate to
                    // exactly that boundary immediately after zeroing the
                    // (now-discarded) tail.
                    unsafe {
                        self.password.as_mut_vec()[idx..].zeroize();
                    }
                    self.password.truncate(idx);
                }
                self.clear_failed_state();
            }
            Keysym::Escape => {
                self.password.zeroize();
                self.clear_failed_state();
            }
            _ => {
                if let Some(text) = event.utf8 {
                    // Return/BackSpace/Escape are handled above by keysym;
                    // this guards against a compositor also sending utf8 for
                    // those (defensive — filters any stray control chars).
                    for ch in text.chars().filter(|c| !c.is_control()) {
                        self.password.push(ch);
                    }
                    self.clear_failed_state();
                }
            }
        }

        self.redraw_all(qh);
    }

    fn clear_failed_state(&mut self) {
        if matches!(self.auth_state, AuthState::Failed | AuthState::ConfigError) {
            self.auth_state = AuthState::Idle;
        }
    }

    fn submit(&mut self) {
        if self.password.is_empty() {
            return;
        }
        self.auth_state = AuthState::Checking;
        // Hand ownership of the buffer to the auth thread; re-reserve
        // capacity up front so the next password typed doesn't reallocate
        // (see the `password` field doc in state.rs). The taken buffer is
        // zeroized automatically when it's dropped at the end of the PAM
        // check (`auth::spawn_check`/`pam::check`).
        let password = std::mem::replace(
            &mut self.password,
            zeroize::Zeroizing::new(String::with_capacity(128)),
        );
        auth::spawn_check(self.username.clone(), password, self.auth_tx.clone());
    }
}

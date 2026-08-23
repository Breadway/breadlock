use smithay_client_toolkit::seat::keyboard::{
    KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers,
};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use std::time::Instant;
use wayland_client::protocol::{wl_keyboard, wl_seat, wl_surface};
use wayland_client::{Connection, QueueHandle};
use zeroize::Zeroize;

use crate::auth;
use crate::state::{AppState, AuthState, PASSWORD_CAP};

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
            self.try_bind_keyboard(qh, &seat);
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability != Capability::Keyboard {
            return;
        }
        // Only release if THIS seat owns the bound keyboard.
        if self.keyboard_seat.as_ref() != Some(&seat) {
            return;
        }
        if let Some(keyboard) = self.keyboard.take() {
            keyboard.release();
        }
        self.keyboard_seat = None;
        self.bind_keyboard_from_available_seats(qh);
    }

    fn remove_seat(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        if self.keyboard_seat.as_ref() != Some(&seat) {
            return;
        }
        if let Some(keyboard) = self.keyboard.take() {
            keyboard.release();
        }
        self.keyboard_seat = None;
        self.bind_keyboard_from_available_seats(qh);
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
        qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
    ) {
        // Tab-held then focus leave would otherwise leave plaintext on screen.
        if self.reveal_held {
            self.reveal_held = false;
            self.redraw_all(qh);
        }
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
        qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        // Letting go of the reveal key (Tab) drops the plain-text view back
        // to dots. Any other release doesn't change state.
        if event.keysym == Keysym::Tab && self.reveal_held {
            self.reveal_held = false;
            self.redraw_all(qh);
        }
    }

    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        modifiers: Modifiers,
        _raw_modifiers: RawModifiers,
        layout: u32,
    ) {
        let changed = self.caps_lock != modifiers.caps_lock || self.layout_index != layout;
        self.caps_lock = modifiers.caps_lock;
        self.layout_index = layout;
        // A modifier update is still "activity" — it follows a key press, so
        // don't let the idle auto-dim start counting while typing.
        self.last_activity = Instant::now();
        if changed {
            self.redraw_all(qh);
        }
    }
}

impl AppState {
    fn try_bind_keyboard(&mut self, qh: &QueueHandle<Self>, seat: &wl_seat::WlSeat) {
        if self.keyboard.is_some() {
            return;
        }
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
            seat,
            None,
            loop_handle,
            Box::new(move |state: &mut AppState, _keyboard, event| {
                state.handle_key(&repeat_qh, event);
            }),
        ) {
            Ok(keyboard) => {
                self.keyboard = Some(keyboard);
                self.keyboard_seat = Some(seat.clone());
            }
            Err(err) => tracing::error!(%err, "failed to bind keyboard"),
        }
    }

    fn bind_keyboard_from_available_seats(&mut self, qh: &QueueHandle<Self>) {
        if self.keyboard.is_some() {
            return;
        }
        let seats: Vec<wl_seat::WlSeat> = self.seat_state.seats().collect();
        for seat in seats {
            if self.keyboard.is_some() {
                return;
            }
            if self
                .seat_state
                .info(&seat)
                .is_some_and(|info| info.has_keyboard)
            {
                self.try_bind_keyboard(qh, &seat);
            }
        }
    }

    fn handle_key(&mut self, qh: &QueueHandle<Self>, event: KeyEvent) {
        // Unlock fade: auth already succeeded; surfaces stay up until it ends.
        if self.unlocking.is_some() {
            return;
        }

        // Escape during Checking cancels the wait (generation bump so a
        // late PAM result cannot unlock). libpam itself is not aborted.
        if self.auth_state == AuthState::Checking {
            if event.keysym == Keysym::Escape {
                self.auth_generation = self.auth_generation.wrapping_add(1);
                self.auth_state = AuthState::Idle;
                self.checking_started = None;
                self.password_display_len = 0;
                self.last_activity = Instant::now();
                self.redraw_all(qh);
            }
            return;
        }

        // Any key counts as activity — it resets the idle auto-dim ramp even
        // when it doesn't change the password (e.g. pressing Enter on an
        // empty field).
        self.last_activity = Instant::now();

        // Hold-to-reveal (Tab): show the plain characters while held. Tab
        // itself produces no utf8, so it can't corrupt the password.
        if event.keysym == Keysym::Tab && self.config.input.reveal_hold {
            self.reveal_held = true;
            self.redraw_all(qh);
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
                self.password_display_len = 0;
                self.clear_failed_state();
            }
            _ => {
                if let Some(text) = event.utf8 {
                    // Return/BackSpace/Escape are handled above by keysym;
                    // this guards against a compositor also sending utf8 for
                    // those (defensive — filters any stray control chars).
                    let mut grew = false;
                    for ch in text.chars().filter(|c| !c.is_control()) {
                        if try_push_password(&mut self.password, ch) {
                            grew = true;
                        } else {
                            break;
                        }
                    }
                    if grew {
                        // Only keystrokes that *grew* the password re-prime the
                        // newest-dot pop-in and the caret's solid phase (see the
                        // `last_keystroke` field doc in state.rs).
                        self.last_keystroke = Some(Instant::now());
                        self.clear_failed_state();
                    }
                }
            }
        }

        self.redraw_all(qh);
    }

    fn clear_failed_state(&mut self) {
        if matches!(
            self.auth_state,
            AuthState::Failed | AuthState::AccountInvalid | AuthState::ConfigError
        ) {
            self.auth_state = AuthState::Idle;
            // Drop the red-pill tint and shake offsets; `failed_at` is also
            // cleared so `schedule_clear_failed`'s timer is a no-op unless
            // its generation still matches a later fail.
            self.failed_at = None;
            self.password_display_len = 0;
        }
    }

    fn submit(&mut self) {
        if self.password.is_empty() {
            return;
        }
        if self.username.is_empty() {
            self.enter_fail(AuthState::ConfigError);
            return;
        }
        self.password_display_len = password_char_count(&self.password);
        self.auth_state = AuthState::Checking;
        self.checking_started = Some(Instant::now());
        self.auth_generation = self.auth_generation.wrapping_add(1);
        // Hand ownership of the buffer to the auth thread; re-reserve
        // capacity up front so the next password typed doesn't reallocate
        // (see the `password` field doc in state.rs). The taken buffer is
        // zeroized automatically when it's dropped at the end of the PAM
        // check (`auth::spawn_check`/`pam::check`).
        let password = std::mem::replace(
            &mut self.password,
            zeroize::Zeroizing::new(String::with_capacity(PASSWORD_CAP)),
        );
        auth::spawn_check(
            self.username.clone(),
            password,
            self.auth_generation,
            self.auth_tx.clone(),
        );
    }
}

/// Push `ch` only if it fits in the already-reserved capacity (no realloc,
/// so an old unzeroized heap buffer is never leaked).
pub(crate) fn try_push_password(password: &mut String, ch: char) -> bool {
    let extra = ch.len_utf8();
    if password.len().saturating_add(extra) > password.capacity() {
        return false;
    }
    password.push(ch);
    true
}

/// Character count for the password pill — never `String::len()` (UTF-8).
pub(crate) fn password_char_count(password: &str) -> usize {
    password.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_cap_ignores_push_that_would_realloc() {
        let mut s = String::with_capacity(8);
        assert!(try_push_password(&mut s, 'a'));
        while try_push_password(&mut s, 'x') {}
        let cap = s.capacity();
        let len = s.len();
        assert!(!try_push_password(&mut s, 'y'));
        assert_eq!(s.len(), len);
        assert_eq!(s.capacity(), cap);
    }

    #[test]
    fn password_char_count_is_not_byte_len() {
        let mut s = String::with_capacity(16);
        assert!(try_push_password(&mut s, 'é'));
        assert_eq!(s.len(), 2);
        assert_eq!(password_char_count(&s), 1);
    }

    #[test]
    fn reserved_capacity_is_256() {
        assert_eq!(PASSWORD_CAP, 256);
        let s = String::with_capacity(PASSWORD_CAP);
        assert!(s.capacity() >= PASSWORD_CAP);
    }
}

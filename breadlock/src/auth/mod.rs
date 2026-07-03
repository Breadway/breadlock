//! Bridges blocking PAM auth (see [`pam`]) onto the calloop event loop that
//! drives the Wayland connection and rendering.
//!
//! Each attempt runs on its own throwaway OS thread (libpam's conversation
//! callback is synchronous FFI and must never block the render loop); the
//! result is posted back through a `calloop::channel` registered once at
//! startup, so the main loop just gets an ordinary event.

pub mod pam;

pub use pam::AuthError;

use smithay_client_toolkit::reexports::calloop::channel::{self, Sender};
use smithay_client_toolkit::reexports::calloop::LoopHandle;

pub type AuthResult = Result<(), AuthError>;

/// Registers the receiving half of the auth-result channel on the event
/// loop and returns the `Sender` to hand to [`spawn_check`] on each attempt.
pub fn register<Data: 'static>(
    loop_handle: &LoopHandle<'static, Data>,
    mut on_result: impl FnMut(&mut Data, AuthResult) + 'static,
) -> Sender<AuthResult> {
    let (tx, channel) = channel::channel();
    loop_handle
        .insert_source(channel, move |event, _, data| {
            if let channel::Event::Msg(result) = event {
                on_result(data, result);
            }
        })
        .expect("failed to register auth-result channel on event loop");
    tx
}

/// Spawns a PAM check for `username`/`password` on its own thread; the
/// outcome arrives later as an event on the loop registered via
/// [`register`]. `password` is moved in and dropped as soon as the PAM
/// conversation consumes it — it is never logged.
pub fn spawn_check(username: String, password: String, result_tx: Sender<AuthResult>) {
    std::thread::spawn(move || {
        let result = pam::check(&username, &password);
        let _ = result_tx.send(result);
    });
}

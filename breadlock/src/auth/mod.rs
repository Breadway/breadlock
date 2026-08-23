//! Bridges blocking PAM auth (see [`pam`]) onto the calloop event loop that
//! drives the Wayland connection and rendering.
//!
//! Each attempt runs on its own throwaway OS thread (libpam's conversation
//! callback is synchronous FFI and must never block the render loop); the
//! result is posted back through a `calloop::channel` registered once at
//! startup, so the main loop just gets an ordinary event.

pub mod pam;

pub use pam::{username_from_process, AuthError};

use smithay_client_toolkit::reexports::calloop::channel::{self, Sender};
use smithay_client_toolkit::reexports::calloop::LoopHandle;
use std::time::Duration;

pub type AuthResult = Result<(), AuthError>;

/// Posted back to the event loop: the attempt's generation so a timed-out
/// or Escape-cancelled check cannot apply a late result.
pub type AuthOutcome = (u64, AuthResult);

/// libpam has no cancel; if it hangs we surface Authenticate after this
/// and ignore whatever it eventually returns (generation mismatch).
const PAM_TIMEOUT: Duration = Duration::from_secs(30);

/// Registers the receiving half of the auth-result channel on the event
/// loop and returns the `Sender` to hand to [`spawn_check`] on each attempt.
pub fn register<Data: 'static>(
    loop_handle: &LoopHandle<'static, Data>,
    mut on_result: impl FnMut(&mut Data, u64, AuthResult) + 'static,
) -> Sender<AuthOutcome> {
    let (tx, channel) = channel::channel();
    loop_handle
        .insert_source(channel, move |event, _, data| {
            if let channel::Event::Msg((generation, result)) = event {
                on_result(data, generation, result);
            }
        })
        .expect("failed to register auth-result channel on event loop");
    tx
}

/// Spawns a PAM check for `username`/`password` on its own thread; the
/// outcome arrives later as an event on the loop registered via
/// [`register`]. `password` is moved in and dropped as soon as the PAM
/// conversation consumes it — it is never logged. It's a `Zeroizing<String>`
/// so the buffer is wiped the moment it goes out of scope at the end of this
/// closure, rather than just deallocated with the bytes intact.
///
/// `generation` is echoed back with the result so the event loop can
/// drop timed-out or cancelled attempts. libpam itself is not aborted.
pub fn spawn_check(
    username: String,
    password: zeroize::Zeroizing<String>,
    generation: u64,
    result_tx: Sender<AuthOutcome>,
) {
    std::thread::spawn(move || {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = pam::check(&username, &password);
            let _ = done_tx.send(result);
        });
        let result = match done_rx.recv_timeout(PAM_TIMEOUT) {
            Ok(result) => result,
            Err(_) => {
                tracing::warn!(
                    timeout_s = PAM_TIMEOUT.as_secs(),
                    "PAM check timed out; treating as authentication failure"
                );
                Err(AuthError::Authenticate)
            }
        };
        let _ = result_tx.send((generation, result));
    });
}

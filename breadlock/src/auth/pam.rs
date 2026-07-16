//! Blocking PAM authentication. libpam's conversation callback is synchronous
//! FFI, so [`check`] must never be called on the render/event-loop thread —
//! see [`super`] for the thread hand-off.

use pam_client2::conv_mock::Conversation;
use pam_client2::{Context, Flag};
use zeroize::Zeroize;

/// The PAM service name — matches `/etc/pam.d/breadlock`
/// (packaging/pam.d/breadlock), which is what actually determines the auth
/// stack (pam_unix, pam_faillock, etc.). This string is just the lookup key.
const SERVICE: &str = "breadlock";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    #[error("PAM authentication failed")]
    Authenticate,
    #[error("PAM account validation failed (expired, locked, etc.)")]
    AccountInvalid,
    #[error("failed to initialize PAM context")]
    ContextInit,
}

/// Verifies `password` for `username` via PAM. Runs `authenticate` +
/// `acct_mgmt` (no `open_session` — the graphical session is already open;
/// this only re-proves who's sitting at the keyboard).
pub fn check(username: &str, password: &str) -> Result<(), AuthError> {
    // `Conversation::with_credentials` copies `password` into its own
    // `String` field (it has to — PAM's conversation callback is invoked
    // later, synchronously, by libpam via FFI). That struct has no Drop/
    // zeroize of its own, so we reach back in and zero it explicitly below
    // before `ctx` (and the conversation it owns) is dropped.
    let conv = Conversation::with_credentials(username, password);
    let mut ctx =
        Context::new(SERVICE, Some(username), conv).map_err(|_| AuthError::ContextInit)?;

    let result = ctx
        .authenticate(Flag::NONE)
        .map_err(|_| AuthError::Authenticate)
        .and_then(|()| {
            ctx.acct_mgmt(Flag::NONE)
                .map_err(|_| AuthError::AccountInvalid)
        });

    ctx.conversation_mut().password.zeroize();

    result
}

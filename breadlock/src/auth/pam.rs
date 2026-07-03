//! Blocking PAM authentication. libpam's conversation callback is synchronous
//! FFI, so [`check`] must never be called on the render/event-loop thread —
//! see [`super`] for the thread hand-off.

use pam_client2::conv_mock::Conversation;
use pam_client2::{Context, Flag};

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
    let conv = Conversation::with_credentials(username, password);
    let mut ctx =
        Context::new(SERVICE, Some(username), conv).map_err(|_| AuthError::ContextInit)?;
    ctx.authenticate(Flag::NONE)
        .map_err(|_| AuthError::Authenticate)?;
    ctx.acct_mgmt(Flag::NONE)
        .map_err(|_| AuthError::AccountInvalid)?;
    Ok(())
}

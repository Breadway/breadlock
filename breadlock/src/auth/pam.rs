//! Blocking PAM authentication. libpam's conversation callback is synchronous
//! FFI, so [`check`] must never be called on the render/event-loop thread —
//! see [`super`] for the thread hand-off.

use pam_client2::conv_mock::Conversation;
use pam_client2::{Context, Flag};
use std::ffi::CStr;
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

/// Copy a NUL-terminated `passwd.pw_name` into an owned `String`.
fn cstr_to_username(ptr: *const libc::c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: `ptr` is a non-null C string from getpwuid_r (into our buffer)
    // or a test fixture.
    let cstr = unsafe { CStr::from_ptr(ptr) };
    let name = cstr.to_str().ok()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_owned())
    }
}

/// Passwd lookup of `uid` via `getpwuid_r`. Grows the scratch buffer on
/// `ERANGE`. Returns `None` if the user is unknown or the name is not UTF-8.
pub fn username_from_uid(uid: libc::uid_t) -> Option<String> {
    let mut pwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut buflen = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    if buflen <= 0 {
        buflen = 1024;
    }
    let mut buf = vec![0u8; buflen as usize];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    loop {
        let rc = unsafe {
            libc::getpwuid_r(
                uid,
                pwd.as_mut_ptr(),
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE {
            let next = buf.len().saturating_mul(2).max(buf.len() + 1024);
            if next == buf.len() {
                return None;
            }
            buf.resize(next, 0);
            continue;
        }
        if rc != 0 || result.is_null() {
            return None;
        }
        break;
    }
    // SAFETY: getpwuid_r wrote a `passwd` and `result` is non-null; `pw_name`
    // points into `buf`, which we copy out before `buf` drops.
    let pwd = unsafe { pwd.assume_init() };
    cstr_to_username(pwd.pw_name)
}

/// Prefer the first non-empty of passwd name, `$USER`, `$LOGNAME`.
pub(crate) fn pick_username(
    passwd: Option<&str>,
    user: Option<&str>,
    logname: Option<&str>,
) -> Option<String> {
    for candidate in [passwd, user, logname] {
        if let Some(s) = candidate.filter(|s| !s.is_empty()) {
            return Some(s.to_owned());
        }
    }
    None
}

/// Username for PAM: `getuid` + `getpwuid_r`, then `$USER` / `$LOGNAME`.
/// Logs a warning when the passwd lookup fails. `None` if nothing resolved.
pub fn username_from_process() -> Option<String> {
    let uid = unsafe { libc::getuid() };
    let from_passwd = username_from_uid(uid);
    if from_passwd.is_none() {
        tracing::warn!(
            uid,
            "passwd lookup for process uid failed; falling back to $USER / $LOGNAME"
        );
    }
    pick_username(
        from_passwd.as_deref(),
        std::env::var("USER").ok().as_deref(),
        std::env::var("LOGNAME").ok().as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn cstr_to_username_copies_nul_terminated_name() {
        let raw = CString::new("breadway").unwrap();
        assert_eq!(
            cstr_to_username(raw.as_ptr()),
            Some("breadway".to_string())
        );
    }

    #[test]
    fn cstr_to_username_rejects_empty_and_null() {
        let empty = CString::new("").unwrap();
        assert_eq!(cstr_to_username(empty.as_ptr()), None);
        assert_eq!(cstr_to_username(std::ptr::null()), None);
    }

    #[test]
    fn pick_username_prefers_passwd_then_user_then_logname() {
        assert_eq!(
            pick_username(Some("from-pw"), Some("from-user"), Some("from-log")),
            Some("from-pw".into())
        );
        assert_eq!(
            pick_username(None, Some("from-user"), Some("from-log")),
            Some("from-user".into())
        );
        assert_eq!(
            pick_username(None, None, Some("from-log")),
            Some("from-log".into())
        );
        assert_eq!(pick_username(Some(""), Some(""), Some("")), None);
        assert_eq!(pick_username(None, None, None), None);
    }

    #[test]
    fn username_from_uid_of_self_is_some_or_none_without_panic() {
        let uid = unsafe { libc::getuid() };
        let _ = username_from_uid(uid);
    }
}

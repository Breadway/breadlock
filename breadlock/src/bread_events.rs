//! `bread.lock.*` event integration — optional, non-blocking. See
//! `EVENTS.md` at the repo root for the full contract. breadlock works
//! identically with or without breadd running; every `emit` here is
//! fire-and-forget (`BreadClient::emit` never blocks or errors this
//! process) so a missing or restarting breadd never affects locking
//! itself.
//!
//! `bread.command.lock.lock` and `bread.command.lock.unlock` are the
//! verbs this process honors. The locker subscribes while the session is
//! locked (already-locked is `bread.lock.lock.done`). `breadlock listen`
//! is the unlocked-path subscriber: it starts this same binary the way
//! hypridle's `lock_cmd = breadlock` does, and treats unlock as already
//! unlocked (`bread.lock.unlock.done`). Session-level equivalents are
//! `loginctl lock-session` / `loginctl unlock-session`. Unlock never
//! calls compositor `unlock()` — that stays on the PAM path.

use std::process::{Command, Stdio};
use std::thread;

use bread_utils::bread_client::{BreadClient, BreadEvent, Subscription};
use bread_utils::singleton::{try_acquire, Acquire};

/// This app's id in bread's sibling-app namespace registry
/// (`bread_shared::apps::KNOWN_APPS`) — events publish as `bread.lock.*`,
/// commands arrive on `bread.command.lock.*`.
pub const APP_ID: &str = "lock";

/// Distinct singleton for `breadlock listen` so a listen process and a
/// locker process can coexist. The locker itself uses [`APP_ID`].
pub const LISTEN_APP: &str = "lock-listen";

pub fn emit_locked() {
    BreadClient::connect(APP_ID).emit("bread.lock.locked", serde_json::json!({}));
}

pub fn emit_unlocked() {
    BreadClient::connect(APP_ID).emit("bread.lock.unlocked", serde_json::json!({}));
}

pub fn emit_lock_done() {
    BreadClient::connect(APP_ID).emit("bread.lock.lock.done", serde_json::json!({}));
}

pub fn emit_lock_failed(error: &str) {
    BreadClient::connect(APP_ID).emit(
        "bread.lock.lock.failed",
        serde_json::json!({ "error": error }),
    );
}

pub fn emit_unlock_done() {
    BreadClient::connect(APP_ID).emit("bread.lock.unlock.done", serde_json::json!({}));
}

pub fn emit_unlock_failed(error: &str) {
    BreadClient::connect(APP_ID).emit(
        "bread.lock.unlock.failed",
        serde_json::json!({ "error": error }),
    );
}

/// True when another process holds the locker singleton — i.e. breadlock
/// is already locking this session. A `try_acquire` that succeeds is
/// released immediately; this is a check, not a claim.
pub fn locker_is_running() -> bool {
    singleton_held(APP_ID)
}

fn singleton_held(app: &str) -> bool {
    match try_acquire(app) {
        Ok(Acquire::HeldByOther(_)) => true,
        Ok(Acquire::Acquired(_guard)) => false,
        Err(_) => false,
    }
}

/// Start a locker the same way hypridle's `lock_cmd = breadlock` does:
/// this binary, no args. The child is reaped on a background thread so
/// a later unlock cannot leave a zombie under `breadlock listen`.
pub fn start_locker() -> Result<(), String> {
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("breadlock"));
    let mut child = Command::new(exe)
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to start breadlock: {e}"))?;
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Honor `bread.command.lock.lock`: already locked is success; otherwise
/// start the locker. `done` means the command was acted on, not that
/// `ext-session-lock-v1` has been accepted — wait on `bread.lock.locked`
/// for the compositor confirmation.
pub fn honor_lock_command() {
    honor_lock_command_with(start_locker);
}

/// Session-level unlock (`loginctl unlock-session` on the caller's
/// session). Does not send compositor `unlock` and does not skip PAM —
/// that stays on the typed-password path. `done` means the command was
/// acted on (or the session was already unlocked), not that
/// `ext-session-lock-v1` has been released — wait on
/// `bread.lock.unlocked` for the compositor confirmation.
fn unlock_session() -> Result<(), String> {
    let status = Command::new("loginctl")
        .arg("unlock-session")
        .stdin(Stdio::null())
        .status()
        .map_err(|e| format!("failed to run loginctl unlock-session: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("loginctl unlock-session exited with {status}"))
    }
}

/// Honor `bread.command.lock.unlock`: already unlocked is success;
/// otherwise ask logind to unlock this session.
pub fn honor_unlock_command() {
    honor_unlock_command_with(locker_is_running(), unlock_session);
}

fn honor_lock_command_with(start: impl FnOnce() -> Result<(), String>) {
    if locker_is_running() {
        tracing::info!("bread.command.lock.lock: already locked");
        emit_lock_done();
        return;
    }
    match start() {
        Ok(()) => {
            tracing::info!("bread.command.lock.lock: started breadlock");
            emit_lock_done();
        }
        Err(error) => {
            tracing::error!(%error, "bread.command.lock.lock: failed to start breadlock");
            emit_lock_failed(&error);
        }
    }
}

fn honor_unlock_command_with(locked: bool, unlock: impl FnOnce() -> Result<(), String>) {
    if !locked {
        tracing::info!("bread.command.lock.unlock: already unlocked");
        emit_unlock_done();
        return;
    }
    match unlock() {
        Ok(()) => {
            tracing::info!("bread.command.lock.unlock: loginctl unlock-session");
            emit_unlock_done();
        }
        Err(error) => {
            tracing::error!(%error, "bread.command.lock.unlock: failed");
            emit_unlock_failed(&error);
        }
    }
}

/// Reacts to `bread.command.lock.*`. Unknown verbs are ignored, not stubbed.
pub fn handle_command(event: &BreadEvent) {
    let Some(verb) = event.event.strip_prefix("bread.command.lock.") else {
        return;
    };
    match verb {
        "lock" => honor_lock_command(),
        "unlock" => honor_unlock_command(),
        other => tracing::info!(verb = other, "ignoring unknown bread.command.lock verb"),
    }
}

/// Subscribe to commands addressed to this app. Keep the handle alive
/// for as long as this process should honor them.
pub fn subscribe_commands() -> Subscription {
    BreadClient::connect(APP_ID).subscribe("bread.command.lock.**", |event| {
        handle_command(&event);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(name: &str) -> BreadEvent {
        BreadEvent {
            event: name.to_string(),
            timestamp: 0,
            data: serde_json::json!({}),
        }
    }

    #[test]
    fn handle_command_ignores_unrecognized_verb() {
        handle_command(&event("bread.command.lock.pin"));
        handle_command(&event("bread.command.clip.clear"));
        handle_command(&event("bread.lock.locked"));
    }

    #[test]
    fn singleton_held_is_false_when_nothing_holds_the_name() {
        let app = format!("breadlock-test-held-false-{}", std::process::id());
        assert!(!singleton_held(&app));
    }

    #[test]
    fn singleton_held_is_true_while_this_process_holds_the_name() {
        let app = format!("breadlock-test-held-true-{}", std::process::id());
        let guard = match try_acquire(&app).unwrap() {
            Acquire::Acquired(g) => g,
            Acquire::HeldByOther(_) => panic!("expected to be the first instance"),
        };
        assert!(singleton_held(&app));
        drop(guard);
        assert!(!singleton_held(&app));
    }

    #[test]
    fn honor_lock_command_with_failed_start_does_not_panic() {
        honor_lock_command_with(|| Err("boom".into()));
    }

    #[test]
    fn honor_lock_command_with_successful_start_does_not_panic() {
        honor_lock_command_with(|| Ok(()));
    }

    #[test]
    fn honor_unlock_command_already_unlocked_does_not_call_loginctl() {
        let called = std::cell::Cell::new(false);
        honor_unlock_command_with(false, || {
            called.set(true);
            Err("should not run".into())
        });
        assert!(!called.get());
    }

    #[test]
    fn honor_unlock_command_with_failed_loginctl_does_not_panic() {
        honor_unlock_command_with(true, || Err("boom".into()));
    }

    #[test]
    fn honor_unlock_command_with_successful_loginctl_does_not_panic() {
        honor_unlock_command_with(true, || Ok(()));
    }
}

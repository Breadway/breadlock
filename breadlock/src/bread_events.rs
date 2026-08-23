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
//! unlocked (`bread.lock.unlock.done`). If the locker is running, unlock
//! is `bread.lock.unlock.failed` — only PAM at the lock screen may
//! unlock. Super+L / hypridle remain `loginctl lock-session`. Bus unlock
//! never calls compositor `unlock()` or `loginctl unlock-session`.

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Set for the life of `run_lock` so [`locker_is_running`] is true without
/// a second `try_acquire("lock")` from the locker process (flock is
/// per-process, so that check would miss ourselves).
static LOCKER_RUNNING: AtomicBool = AtomicBool::new(false);

/// RAII flag: [`locker_is_running`] is true until this drops.
pub struct LockerRunningGuard;

impl Drop for LockerRunningGuard {
    fn drop(&mut self) {
        LOCKER_RUNNING.store(false, Ordering::SeqCst);
    }
}

/// Mark this process as the locker for the life of the returned guard.
pub fn enter_lock_process() -> LockerRunningGuard {
    LOCKER_RUNNING.store(true, Ordering::SeqCst);
    LockerRunningGuard
}

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

/// True when this process is the locker, or another process holds the
/// locker singleton — i.e. breadlock is already locking this session.
pub fn locker_is_running() -> bool {
    LOCKER_RUNNING.load(Ordering::SeqCst) || singleton_held(APP_ID)
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
        .stdout(Stdio::null())
        .stderr(Stdio::null())
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
    honor_lock_command_with(locker_is_running(), start_locker);
}

/// Payload on `bread.lock.unlock.failed` while the locker is running.
/// Bus clients cannot unlock; only PAM at the lock screen can.
const UNLOCK_REFUSED_WHILE_LOCKED: &str =
    "bus unlock cannot bypass PAM; authenticate at the lock screen";

/// Honor `bread.command.lock.unlock`. Fail-secure: never compositor
/// `unlock()`, never `loginctl unlock-session`. Already unlocked is
/// `.done`; a running locker is `.failed`.
pub fn honor_unlock_command() {
    honor_unlock_command_with(locker_is_running(), emit_unlock_done, emit_unlock_failed);
}

fn honor_lock_command_with(locked: bool, start: impl FnOnce() -> Result<(), String>) {
    if locked {
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

fn honor_unlock_command_with(
    locked: bool,
    emit_done: impl FnOnce(),
    emit_failed: impl FnOnce(&str),
) {
    if !locked {
        tracing::info!("bread.command.lock.unlock: already unlocked");
        emit_done();
        return;
    }
    tracing::error!(
        error = UNLOCK_REFUSED_WHILE_LOCKED,
        "bread.command.lock.unlock: refused while locked"
    );
    emit_failed(UNLOCK_REFUSED_WHILE_LOCKED);
}

/// Reacts to `bread.command.lock.*`. Unknown verbs are ignored, not stubbed.
pub fn handle_command(event: &BreadEvent) {
    handle_command_with(event, honor_lock_command, honor_unlock_command);
}

fn handle_command_with(event: &BreadEvent, on_lock: impl FnOnce(), on_unlock: impl FnOnce()) {
    let Some(verb) = event.event.strip_prefix("bread.command.lock.") else {
        return;
    };
    match verb {
        "lock" => on_lock(),
        "unlock" => on_unlock(),
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
    use std::cell::Cell;

    fn event(name: &str) -> BreadEvent {
        BreadEvent {
            event: name.to_string(),
            timestamp: 0,
            data: serde_json::json!({}),
        }
    }

    #[test]
    fn handle_command_ignores_unrecognized_verb() {
        let lock = Cell::new(false);
        let unlock = Cell::new(false);
        handle_command_with(
            &event("bread.command.lock.pin"),
            || lock.set(true),
            || unlock.set(true),
        );
        handle_command_with(
            &event("bread.command.clip.clear"),
            || lock.set(true),
            || unlock.set(true),
        );
        handle_command_with(
            &event("bread.lock.locked"),
            || lock.set(true),
            || unlock.set(true),
        );
        assert!(!lock.get());
        assert!(!unlock.get());
    }

    #[test]
    fn handle_command_dispatches_only_lock_and_unlock() {
        let lock = Cell::new(0u32);
        let unlock = Cell::new(0u32);
        handle_command_with(
            &event("bread.command.lock.lock"),
            || lock.set(lock.get() + 1),
            || unlock.set(unlock.get() + 1),
        );
        handle_command_with(
            &event("bread.command.lock.unlock"),
            || lock.set(lock.get() + 1),
            || unlock.set(unlock.get() + 1),
        );
        handle_command_with(
            &event("bread.command.lock.pin"),
            || lock.set(lock.get() + 1),
            || unlock.set(unlock.get() + 1),
        );
        assert_eq!(lock.get(), 1);
        assert_eq!(unlock.get(), 1);
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
    fn honor_lock_command_with_failed_start_runs_start() {
        let started = Cell::new(false);
        honor_lock_command_with(false, || {
            started.set(true);
            Err("boom".into())
        });
        assert!(started.get());
    }

    #[test]
    fn honor_lock_command_with_successful_start_runs_start() {
        let started = Cell::new(false);
        honor_lock_command_with(false, || {
            started.set(true);
            Ok(())
        });
        assert!(started.get());
    }

    #[test]
    fn honor_lock_command_already_locked_does_not_start() {
        let started = Cell::new(false);
        honor_lock_command_with(true, || {
            started.set(true);
            Ok(())
        });
        assert!(!started.get());
    }

    #[test]
    fn honor_unlock_command_already_unlocked_emits_done() {
        let done = Cell::new(false);
        let failed = Cell::new(false);
        honor_unlock_command_with(false, || done.set(true), |_| failed.set(true));
        assert!(done.get());
        assert!(!failed.get());
    }

    #[test]
    fn honor_unlock_command_while_locked_emits_failed_not_done() {
        let done = Cell::new(false);
        let failed = Cell::new(false);
        honor_unlock_command_with(
            true,
            || done.set(true),
            |e| {
                assert_eq!(e, UNLOCK_REFUSED_WHILE_LOCKED);
                failed.set(true);
            },
        );
        assert!(!done.get());
        assert!(failed.get());
    }

    #[test]
    fn honor_unlock_command_while_locked_error_mentions_pam() {
        assert!(
            UNLOCK_REFUSED_WHILE_LOCKED.contains("PAM"),
            "bus unlock refusal must say it cannot bypass PAM, got {UNLOCK_REFUSED_WHILE_LOCKED:?}"
        );
    }

    #[test]
    fn enter_lock_process_makes_locker_is_running_true_without_singleton() {
        let app = format!("breadlock-test-running-flag-{}", std::process::id());
        assert!(!singleton_held(&app));
        {
            let _g = enter_lock_process();
            assert!(LOCKER_RUNNING.load(Ordering::SeqCst));
        }
        assert!(!LOCKER_RUNNING.load(Ordering::SeqCst));
    }
}

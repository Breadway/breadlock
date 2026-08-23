# breadlock — bread event integration

breadlock is a standalone session locker: it works exactly the same with
or without `breadd` running. When breadd *is* present, `breadlock`
publishes events into the shared bread automation fabric. See the parent
`bread` repo's `Documentation.md` — specifically its "Namespaces" and
"Integrating a bread\* app" sections — for the general convention this
follows.

App id: **`lock`**. Transport: `bread-utils`'s `bread_client` module
(feature `bread-client`) — `breadlock` links it directly. Each `emit` is
its own short-lived connection (`BreadClient::emit` is fire-and-forget).
Commands are received on a `BreadClient::subscribe` background thread
(reconnect/backoff) from two places:

- the locker process itself, while the session is locked
- `breadlock listen`, a tiny long-running subscriber so lock/unlock
  work while unlocked

`breadgreet` is not wired to the bus. It runs under greetd (typically as
the dedicated greeter user, before a user session exists), so breadd is
usually not there to receive anything, and login is a different lifecycle
from session lock/unlock.

## Events published (`bread.lock.*`)

| Event | Data | When |
|-------|------|------|
| `bread.lock.locked` | `{}` | The compositor accepted the `ext-session-lock-v1` request (`SessionLockHandler::locked`). Not emitted merely because breadlock started or asked to lock. |
| `bread.lock.unlocked` | `{}` | PAM authenticated successfully and breadlock sent `unlock` to the compositor, **or** the compositor ended an already-active lock (`SessionLockHandler::finished` after `locked` — breadlock sends `unlock_and_destroy` then emits this). Not emitted when the lock was never acquired (`finished` before `locked`), on a dispatch-error exit (fail-secure: the session stays locked), or a failed/typo password. |
| `bread.lock.lock.done` | `{}` | `bread.command.lock.lock` was honored: the locker was already running, or a locker process was started (same no-args invocation as hypridle's `lock_cmd = breadlock`). This is the command confirmation, not compositor proof — wait on `bread.lock.locked` if you need the session-lock protocol to have completed. |
| `bread.lock.lock.failed` | `{ "error": "<message>" }` | `bread.command.lock.lock` was received but the locker could not be started (e.g. this binary is missing from disk). |
| `bread.lock.unlock.done` | `{}` | `bread.command.lock.unlock` was honored: no locker was running (already unlocked), or `loginctl unlock-session` was invoked for this session. This is the command confirmation, not compositor proof — wait on `bread.lock.unlocked` if you need PAM + `ext-session-lock-v1` unlock. |
| `bread.lock.unlock.failed` | `{ "error": "<message>" }` | `bread.command.lock.unlock` was received but `loginctl unlock-session` could not be run (binary missing, non-zero exit). |

## Commands honored (`bread.command.lock.*`)

| Verb | Effect |
|------|--------|
| `lock` | If a locker is already running, emit `bread.lock.lock.done` and do nothing else. Otherwise start `breadlock` the same way hypridle does (`lock_cmd = breadlock`: this binary, no args) and emit `done` or `failed`. |
| `unlock` | If no locker is running, emit `bread.lock.unlock.done` (already unlocked). Otherwise run `loginctl unlock-session` on the caller's session and emit `done` or `failed`. This is session-level (logind), not a passwordless PAM bypass: breadlock does not call compositor `unlock()` for this verb. |

A Lua workflow that wants the session locked should `bread.wait` /
`bread.wait_any` on `bread.lock.lock.done` (or `.failed`) with a timeout.
To know the compositor actually locked, wait on `bread.lock.locked`.
The same pattern applies to unlock: wait on `bread.lock.unlock.done` /
`.failed` for the command, and on `bread.lock.unlocked` for PAM +
compositor unlock.

### Who is listening

`bread.command.lock.lock` / `bread.command.lock.unlock` are a silent
no-op if nobody is subscribed (bread's usual "no listener, no-op"
rule). Two subscribers exist:

1. **`breadlock listen`** — run this for the unlocked path (Hyprland
   `exec-once = breadlock listen`, a bread module, or equivalent).
   Without it, a command sent while the session is unlocked has no
   process to receive it. Unlock while already unlocked is an
   idempotent `done`.
2. **The locker process** — always subscribes once the lock screen is
   up, so `lock` during an active lock is an idempotent `done`, and
   `unlock` runs `loginctl unlock-session` rather than compositor
   `unlock()`.

### Session-level equivalent

Super+L on BOS is `loginctl lock-session`. hypridle picks that up and
runs `lock_cmd = breadlock`. That path does **not** go through the
bread command bus. It is the session-level equivalent of
`bread.command.lock.lock` + `breadlock listen`: same locker binary,
same `ext-session-lock-v1` request. Prefer `loginctl lock-session`
from a keybind; prefer the bus command from a Lua workflow.

`loginctl unlock-session` is the matching session-level unlock. The
bus verb invokes that same command. Compositor unlock after a typed
password is still PAM on this process (`bread.lock.unlocked`); a
dispatch-error or crash path still does **not** call compositor
`unlock()` (fail-secure).

### Not implemented: `pin` / `blur`

`background.blur` in `breadlock.toml` remains a documented locker
no-op (accepted, warned, surface drawn unblurred). That is appearance
config, not a bus command — do not invent `bread.command.lock.blur`
for it.

## Fail-safe behavior

- If breadd isn't installed or isn't running, `emit` is a silent no-op
  (`BreadClient::emit` never blocks or errors the caller) and the
  command subscription simply never receives anything — breadlock's
  actual lock/unlock path is entirely unaffected either way.
- If breadd restarts, the command subscription reconnects automatically
  (`BreadClient::subscribe`'s background thread has its own backoff loop);
  no restart of the locker or of `breadlock listen` is needed.

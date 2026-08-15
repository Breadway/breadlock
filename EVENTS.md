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
- `breadlock listen`, a tiny long-running subscriber so the command
  works while unlocked

`breadgreet` is not wired to the bus. It runs under greetd (typically as
the dedicated greeter user, before a user session exists), so breadd is
usually not there to receive anything, and login is a different lifecycle
from session lock/unlock.

## Events published (`bread.lock.*`)

| Event | Data | When |
|-------|------|------|
| `bread.lock.locked` | `{}` | The compositor accepted the `ext-session-lock-v1` request (`SessionLockHandler::locked`). Not emitted merely because breadlock started or asked to lock. |
| `bread.lock.unlocked` | `{}` | PAM authenticated successfully and breadlock sent `unlock` to the compositor. Not emitted on a compositor-ended lock (`finished`), a dispatch-error exit (fail-secure: the session stays locked), or a failed/typo password. |
| `bread.lock.lock.done` | `{}` | `bread.command.lock.lock` was honored: the locker was already running, or a locker process was started (same no-args invocation as hypridle's `lock_cmd = breadlock`). This is the command confirmation, not compositor proof — wait on `bread.lock.locked` if you need the session-lock protocol to have completed. |
| `bread.lock.lock.failed` | `{ "error": "<message>" }` | `bread.command.lock.lock` was received but the locker could not be started (e.g. this binary is missing from disk). |

## Commands honored (`bread.command.lock.*`)

| Verb | Effect |
|------|--------|
| `lock` | If a locker is already running, emit `bread.lock.lock.done` and do nothing else. Otherwise start `breadlock` the same way hypridle does (`lock_cmd = breadlock`: this binary, no args) and emit `done` or `failed`. |

A Lua workflow that wants the session locked should `bread.wait` /
`bread.wait_any` on `bread.lock.lock.done` (or `.failed`) with a timeout.
To know the compositor actually locked, wait on `bread.lock.locked`.

### Who is listening

`bread.command.lock.lock` is a silent no-op if nobody is subscribed
(bread's usual "no listener, no-op" rule). Two subscribers exist:

1. **`breadlock listen`** — run this for the unlocked path (Hyprland
   `exec-once = breadlock listen`, a bread module, or equivalent).
   Without it, a command sent while the session is unlocked has no
   process to receive it.
2. **The locker process** — always subscribes once the lock screen is
   up, so a command received during an active lock is an idempotent
   `done`.

### Session-level equivalent

Super+L on BOS is `loginctl lock-session`. hypridle picks that up and
runs `lock_cmd = breadlock`. That path does **not** go through the
bread command bus. It is the session-level equivalent of
`bread.command.lock.lock` + `breadlock listen`: same locker binary,
same `ext-session-lock-v1` request. Prefer `loginctl lock-session`
from a keybind; prefer the bus command from a Lua workflow.

### Not implemented: `unlock` / `pin` / `blur`

Unlock is PAM on this process only — there is no `bread.command.lock.unlock`
and none is stubbed. `background.blur` in `breadlock.toml` remains a
documented locker no-op (accepted, warned, surface drawn unblurred).
That is appearance config, not a bus command — do not invent
`bread.command.lock.blur` for it.

## Fail-safe behavior

- If breadd isn't installed or isn't running, `emit` is a silent no-op
  (`BreadClient::emit` never blocks or errors the caller) and the
  command subscription simply never receives anything — breadlock's
  actual lock/unlock path is entirely unaffected either way.
- If breadd restarts, the command subscription reconnects automatically
  (`BreadClient::subscribe`'s background thread has its own backoff loop);
  no restart of the locker or of `breadlock listen` is needed.

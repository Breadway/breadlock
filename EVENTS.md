# breadlock — bread event integration

breadlock is a standalone session locker: it works exactly the same with
or without `breadd` running. When breadd *is* present, `breadlock`
publishes events into the shared bread automation fabric. See the parent
`bread` repo's `Documentation.md` — specifically its "Namespaces" and
"Integrating a bread\* app" sections — for the general convention this
follows.

App id: **`lock`**. Transport: `bread-utils`'s `bread_client` module
(feature `bread-client`) — `breadlock` links it directly. Each `emit` is
its own short-lived connection (`BreadClient::emit` is fire-and-forget);
there is no long-running subscription half because breadlock has no
command verbs (see below).

`breadgreet` is not wired to the bus. It runs under greetd (typically as
the dedicated greeter user, before a user session exists), so breadd is
usually not there to receive anything, and login is a different lifecycle
from session lock/unlock.

## Events published (`bread.lock.*`)

| Event | Data | When |
|-------|------|------|
| `bread.lock.locked` | `{}` | The compositor accepted the `ext-session-lock-v1` request (`SessionLockHandler::locked`). Not emitted merely because breadlock started or asked to lock. |
| `bread.lock.unlocked` | `{}` | PAM authenticated successfully and breadlock sent `unlock` to the compositor. Not emitted on a compositor-ended lock (`finished`), a dispatch-error exit (fail-secure: the session stays locked), or a failed/typo password. |

## Commands honored (`bread.command.lock.*`)

None. breadlock is started by hypridle / `loginctl lock-session` (or
directly) and unlocks only via PAM on this process. There is no
`lock`/`unlock`/`pin`/`blur` verb, and none is stubbed as a no-op.

`background.blur` in `breadlock.toml` remains a documented locker no-op
(accepted, warned, surface drawn unblurred). That is appearance config,
not a bus command — do not invent `bread.command.lock.blur` for it.

## Fail-safe behavior

- If breadd isn't installed or isn't running, `emit` is a silent no-op
  (`BreadClient::emit` never blocks or errors the caller) — breadlock's
  actual lock/unlock path is entirely unaffected either way.
- There is no command subscription, so a breadd restart while the lock
  screen is up changes nothing on this side.

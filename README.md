# breadlock

Session locker and graphical [greetd](https://git.sr.ht/~kennylevinsen/greetd) greeter for [Hyprland](https://hyprland.org/) on Wayland — the bread-ecosystem replacement for `hyprlock` and `tuigreet`. BOS already ships both binaries: `breadgreet` under `cage` via greetd, and `breadlock` via hypridle (`SUPER+L` is `loginctl lock-session`).

Two binaries, one workspace: 

- **`breadlock`** — locks the *already running* Hyprland session via `ext-session-lock-v1`. Drop-in for `hyprlock`.
- **`breadgreet`** — a graphical greeter that speaks `greetd`'s own IPC protocol (the same architecture as `gtkgreet`/`regreet`). `greetd` keeps owning PAM auth, VT switching, and session launching; `breadgreet` only draws the login UI and relays the conversation. This is a deliberate choice over reimplementing a display manager from scratch — `greetd` is already installed and battle-tested.

Both use [`bread-theme`](https://git.breadway.dev/Breadway/bread-ecosystem) for palette loading, matching the rest of the bread* ecosystem (breadbar, breadbox, bos-settings).

## bread event integration

`breadlock` works the same with or without `breadd`. When `breadd` is
running, it publishes `bread.lock.locked` / `bread.lock.unlocked` and
honors `bread.command.lock.lock` / `bread.command.lock.unlock` (emits
`bread.lock.lock.done` / `.failed` and `bread.lock.unlock.done` /
`.failed`). Run `breadlock listen` so both commands work while
unlocked; the locker also subscribes while the session is locked.
Unlock is fail-secure: already-unlocked acks `bread.lock.unlock.done`;
a running locker refuses with `bread.lock.unlock.failed` (only PAM at
the lock screen unlocks). The bus never calls compositor `unlock()` or
`loginctl unlock-session`. Super+L remains `loginctl lock-session`
(hypridle then runs `breadlock`). See [EVENTS.md](EVENTS.md).
`breadgreet` is not on the bus. There is no `bakery.toml` (PAM /
pacman exception).

## Architecture

```
breadlock/
├── breadlock-ui/   shared: bread-theme wrapper, TOML config, .desktop parsing,
│                    software-rendering primitives (tiny-skia + cosmic-text,
│                    behind the "paint" feature — only breadlock needs them)
├── breadlock/       the locker (SCTK + PAM; EGL wallpaper + software chrome)
└── breadgreet/      the greeter (GTK4 + relm4 + greetd_ipc)
```

### breadlock

- **Protocol**: `ext-session-lock-v1` via [`smithay-client-toolkit`](https://docs.rs/smithay-client-toolkit) — GTK has no session-lock support, so this is a raw Wayland client, not a layer-shell surface like breadbar.
- **Rendering**: hybrid — wallpaper via EGL/GLES2 (`wl_egl_window` wrapping the lock surface); chrome (password pill, clock, status line) is still software (`tiny-skia` + `cosmic-text`, "Varela Round" by family name) and blitted over the GPU frame. If EGL init fails, the locker falls back to a fully-software `wl_shm` path.
- **Background**: a solid palette color or a PNG (cover-fit). Ken Burns (`background.ken_burns`) is opt-in: a slow pan+zoom on image backgrounds — cheap on the GPU path, a continuous software redraw if EGL is unavailable. `background.blur` is **not implemented** — the key is accepted and logs a warning; the surface is drawn unblurred. Live blur-of-desktop (hyprlock-style) would need a `wlr-screencopy` capture.
- **Auth**: [`pam-client2`](https://crates.io/crates/pam-client2) against the `breadlock` PAM service (`packaging/pam.d/breadlock`, installed to `/etc/pam.d/breadlock` by the package). Runs on its own OS thread — libpam's conversation callback is blocking FFI — and reports back through a `calloop::channel` registered on the render loop.

### breadgreet

- **Protocol**: [`greetd_ipc`](https://crates.io/crates/greetd_ipc) (greetd's own crate) over the Unix socket at `$GREETD_SOCK`: `CreateSession` → answer each `AuthMessage` via `PostAuthMessageResponse` → `StartSession` hands the resolved session command to `greetd`, which execs it and owns the VT switch away.
- **UI**: GTK4 + [relm4](https://relm4.org/), matching breadbar's stack — **without** `gtk4-layer-shell`. `greetd` hosts the greeter under a single-client kiosk compositor (`cage -s`), which already fullscreens its one client, so layer-shell's multi-surface/anchor semantics don't apply. Confirmed against ReGreet's real dependency list, which has no layer-shell dependency either.
- **Sessions**: scans `/usr/share/wayland-sessions` and `/usr/share/xsessions` for `.desktop` entries and shows a keyboard-accessible picker. The configured default (compiled-in: `bos`) is pre-selected when that stem exists; otherwise the first discovered session. `StartSession` is the chosen entry's `Exec=` argv.

## Config

Copy [`breadlock.example.toml`](breadlock.example.toml) to `~/.config/breadlock/breadlock.toml` and [`breadgreet.example.toml`](breadgreet.example.toml) to `/etc/greetd/breadgreet.toml` (or `~/.config/breadgreet/breadgreet.toml` for local testing under a normal session — `breadgreet` checks the system path first since it typically runs as the dedicated `greeter` user). Every field is optional; both binaries run with sensible defaults and no config at all.

`breadlock.toml`'s `[status]` table (both flags default on) shows now-playing (MPRIS) and battery (upower) as a small line under the clock. Polled on a background thread; degrades silently if D-Bus or the service is missing.

## Building

```sh
cargo build --release --bin breadlock --bin breadgreet
cargo test --workspace
```

Requires GTK4 (≥ 4.12), `libxkbcommon`, PAM development headers, `git` (workspace crates `bread-theme` / `bread-utils` are git deps), and `pkg-config` (gtk4-rs; also provided by `base-devel`). On Arch:

```sh
sudo pacman -S gtk4 wayland libxkbcommon pam rust cargo git pkg-config
```

`breadlock-auth-check` and `breadlock-preview` are extra, dev-only binaries in the `breadlock` package (see Verification below) — not installed by the package. Build them explicitly with `cargo build --bin breadlock-auth-check` or `--bin breadlock-preview` if you need them.

## Packaging

`packaging/arch/PKGBUILD` builds and installs both binaries plus `/etc/pam.d/breadlock`, published to the `[breadway]` pacman repo by `.forgejo/workflows/package.yml`. breadlock is a deliberate **pacman-only** exception — there is no `bakery.toml` on purpose. A PAM service and greetd greeter need a root-owned install (`/etc/pam.d/breadlock`), which bakery has no privileged path for.

BOS already wires the packaged binaries (this repo still does not ship those system files):

```toml
# /etc/greetd/config.toml — BOS default
[default_session]
command = "cage -s -- breadgreet"
```

```
# hypridle lock_cmd (BOS). SUPER+L is loginctl lock-session, which hypridle picks up.
lock_cmd = breadlock
```

`breadlock listen` is the unlocked-path subscriber for
`bread.command.lock.lock` and `bread.command.lock.unlock`. It is not
started by hypridle; add it to session startup
(`exec-once = breadlock listen`) if a Lua workflow should be able to
lock the session while it is unlocked, or to ack already-unlocked.
`bread.command.lock.unlock` does not replace PAM and does not run
`loginctl unlock-session`. Super+L / hypridle remain
`loginctl lock-session`.

## Verification (why this is safe to test without a lockout risk)

1. **PAM logic in isolation first**: `cargo run --bin breadlock-auth-check` exercises the exact PAM flow `breadlock` uses, against a typed password, with **no Wayland surface at all**. A bad `/etc/pam.d/breadlock` just prints an error here — it can never lock a session.
2. **Locker rendering/lock lifecycle nested, never against the live session**: run `breadlock` inside a nested Hyprland instance or under `cage -- breadlock`. `ext-session-lock-v1` only ever affects the compositor instance the client is connected to (scoped to `$WAYLAND_DISPLAY`), so a nested lock can never lock the real outer session. Verify the full type-password → PAM check → unlock cycle there, including the wrong-password path, before ever binding a real keybind.
3. **If testing against a live session**: keep a second TTY or SSH session open the whole time. Killing the `breadlock` process is **not** a safe unlock path — per the protocol, an abnormally-terminated lock client is expected to leave the compositor still locked. The real recovery path is "kill it, then use the second session to restart Hyprland or switch VT."
4. **breadgreet**: `cargo test -p breadgreet` runs the `greetd_ipc` framing/state-machine tests against a mock Unix-socket server — no real `greetd` or PAM involved. Manual testing against a real `greetd` should happen on a disposable VT, not by replacing the live BOS `cage -s -- breadgreet` session on VT1.

## License

MIT

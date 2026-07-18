# breadlock

Session locker and graphical [greetd](https://git.sr.ht/~kennylevinsen/greetd) greeter for [Hyprland](https://hyprland.org/) on Wayland — the bread-ecosystem replacement for `hyprlock` and the TUI greeter (`tuigreet`) BOS currently ships.

Two binaries, one workspace:

- **`breadlock`** — locks the *already running* Hyprland session via `ext-session-lock-v1`. Drop-in for `hyprlock`.
- **`breadgreet`** — a graphical greeter that speaks `greetd`'s own IPC protocol (the same architecture as `gtkgreet`/`regreet`). `greetd` keeps owning PAM auth, VT switching, and session launching; `breadgreet` only draws the login UI and relays the conversation. This is a deliberate choice over reimplementing a display manager from scratch — `greetd` is already installed and battle-tested.

Both use [`bread-theme`](https://git.breadway.dev/Breadway/bread-ecosystem) for palette loading, matching the rest of the bread* ecosystem (breadbar, breadbox, bos-settings).

## Architecture

```
breadlock/
├── breadlock-ui/   shared: bread-theme wrapper, TOML config, .desktop parsing,
│                    software-rendering primitives (tiny-skia + cosmic-text,
│                    behind the "paint" feature — only breadlock needs them)
├── breadlock/       the locker (SCTK + PAM)
└── breadgreet/      the greeter (GTK4 + relm4 + greetd_ipc)
```

### breadlock

- **Protocol**: `ext-session-lock-v1` via [`smithay-client-toolkit`](https://docs.rs/smithay-client-toolkit) — GTK has no session-lock support, so this is a raw Wayland client, not a layer-shell surface like breadbar.
- **Rendering**: fully software — `tiny-skia` composites each frame (background, rounded password pill, clock, status line) into a `wl_shm` buffer; `cosmic-text` shapes and rasterizes text (loads "Varela Round" by family name). No EGL/GL.
- **Background**: a solid palette color or a static PNG (cover-fit). Live blur-of-desktop (hyprlock-style) is a **v2 follow-up** — it needs a `wlr-screencopy` capture (`libwayshot` is the right crate when this gets picked up); `background.blur = true` is accepted today but just logs a warning.
- **Auth**: [`pam-client2`](https://crates.io/crates/pam-client2) against the `breadlock` PAM service (`packaging/pam.d/breadlock`, installed to `/etc/pam.d/breadlock` by the package). Runs on its own OS thread — libpam's conversation callback is blocking FFI — and reports back through a `calloop::channel` registered on the render loop.

### breadgreet

- **Protocol**: [`greetd_ipc`](https://crates.io/crates/greetd_ipc) (greetd's own crate) over the Unix socket at `$GREETD_SOCK`: `CreateSession` → answer each `AuthMessage` via `PostAuthMessageResponse` → `StartSession` hands the resolved session command to `greetd`, which execs it and owns the VT switch away.
- **UI**: GTK4 + [relm4](https://relm4.org/), matching breadbar's stack — **without** `gtk4-layer-shell`. `greetd` hosts the greeter under a single-client kiosk compositor (`cage -s`), which already fullscreens its one client, so layer-shell's multi-surface/anchor semantics don't apply. Confirmed against ReGreet's real dependency list, which has no layer-shell dependency either.
- **Sessions**: scans `/usr/share/wayland-sessions` and `/usr/share/xsessions` for `.desktop` entries and auto-selects the configured default (or the only one found). BOS ships one session today, so there's no picker UI in v1 — a natural v2 addition if that changes.

## Config

Copy [`breadlock.example.toml`](breadlock.example.toml) to `~/.config/breadlock/breadlock.toml` and [`breadgreet.example.toml`](breadgreet.example.toml) to `/etc/greetd/breadgreet.toml` (or `~/.config/breadgreet/breadgreet.toml` for local testing under a normal session — `breadgreet` checks the system path first since it typically runs as the dedicated `greeter` user). Every field is optional; both binaries run with sensible defaults and no config at all.

## Building

```sh
cargo build --release --bin breadlock --bin breadgreet
cargo test --workspace
```

Requires GTK4 (≥ 4.12), `libxkbcommon`, and PAM development headers. On Arch:

```sh
sudo pacman -S gtk4 wayland libxkbcommon pam rust cargo
```

`breadlock-auth-check` is a third, dev-only binary in the `breadlock` package (see Verification below) — not installed by the package, build it explicitly with `cargo build --bin breadlock-auth-check` if you need it.

## Packaging

`packaging/arch/PKGBUILD` builds and installs both binaries plus `/etc/pam.d/breadlock`, published to the `[breadway]` pacman repo by `.forgejo/workflows/package.yml`. breadlock is pacman-only — it is not in bread-ecosystem's registry and has no `bakery.toml`; a PAM/greeter component gets installed through the package manager, not the bakery curl-script channel.

**Not included, by design**: this repo does not touch `/etc/greetd/config.toml`, install a lock keybind, or wire up `hypridle`. Once packaged, wiring BOS to actually use these binaries means:

```toml
# /etc/greetd/config.toml — replace the current tuigreet line
[default_session]
command = "cage -s -- breadgreet"
```

```
# hyprland.conf
bind = SUPER, L, exec, breadlock
```

That's a separate, later BOS task — deliberately kept out of this change so the existing `tuigreet` login path stays untouched and available as a fallback while these binaries are tested.

## Verification (why this is safe to test without a lockout risk)

1. **PAM logic in isolation first**: `cargo run --bin breadlock-auth-check` exercises the exact PAM flow `breadlock` uses, against a typed password, with **no Wayland surface at all**. A bad `/etc/pam.d/breadlock` just prints an error here — it can never lock a session.
2. **Locker rendering/lock lifecycle nested, never against the live session**: run `breadlock` inside a nested Hyprland instance or under `cage -- breadlock`. `ext-session-lock-v1` only ever affects the compositor instance the client is connected to (scoped to `$WAYLAND_DISPLAY`), so a nested lock can never lock the real outer session. Verify the full type-password → PAM check → unlock cycle there, including the wrong-password path, before ever binding a real keybind.
3. **If testing against a live session**: keep a second TTY or SSH session open the whole time. Killing the `breadlock` process is **not** a safe unlock path — per the protocol, an abnormally-terminated lock client is expected to leave the compositor still locked. The real recovery path is "kill it, then use the second session to restart Hyprland or switch VT."
4. **breadgreet**: `cargo test -p breadgreet` runs the `greetd_ipc` framing/state-machine tests against a mock Unix-socket server — no real `greetd` or PAM involved. Manual testing against a real `greetd` should happen on a disposable VT, leaving the existing `tuigreet` config on VT1 untouched as a fallback.

## License

MIT

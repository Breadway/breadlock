#!/usr/bin/env bash
# Run breadgreet in a nested compositor window on your current desktop so you
# can actually drive the UI — type a username, a password, watch the spinner,
# get the shake on a wrong password — without touching your real greeter or
# rebooting.
#
#   breadgreet/scripts/preview.sh            # cairo renderer (safe everywhere)
#   breadgreet/scripts/preview.sh --gpu      # your default GSK renderer
#   breadgreet/scripts/preview.sh --typed    # force the old type-the-username flow
#
# By default breadgreet enumerates your /etc/passwd users and skips straight to
# the password prompt. Password is "bread"; any other password exercises the
# auth-error path (shake + red status line). A correct login makes breadgreet
# exit, as it would for real — that ends the script and closes the window.
# Ctrl-C in this terminal tears everything down at any point.
#
# The nested compositor opens as an ordinary window; float / resize it with
# your WM as you like (it fills whatever size it gets).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
cd "$repo"

renderer=cairo
typed=0
for arg in "$@"; do
    case "$arg" in
        --gpu) renderer="" ;;
        --typed) typed=1 ;;
        *) echo "unknown flag: $arg" >&2; exit 1 ;;
    esac
done

if [[ -z "${WAYLAND_DISPLAY:-}" ]]; then
    echo "no WAYLAND_DISPLAY — run this from inside your Wayland session" >&2
    exit 1
fi
: "${XDG_RUNTIME_DIR:=/run/user/$(id -u)}"

if ! command -v weston >/dev/null; then
    echo "need 'weston' for the nested compositor (pacman -S weston)" >&2
    exit 1
fi

echo ">> building breadgreet (debug)"
cargo build -p breadgreet

work="$(mktemp -d /tmp/breadgreet-preview.XXXXXX)"
sock="$work/greetd.sock"
conf="$work/breadgreet.toml"

# A preview config (loaded via $BREADGREET_CONFIG, so the real
# /etc/greetd/breadgreet.toml is left untouched). BOS ships breadgreet with a
# flat colour background; this points at the BOS wallpaper + Ken Burns so you
# can also see how a wallpapered greeter would look.
wallpaper="$repo/../bos/iso/airootfs/usr/share/backgrounds/bos/bread-background.png"
cat > "$conf" <<EOF
[background]
mode = "$([[ -f "$wallpaper" ]] && echo image || echo color)"
path = "$wallpaper"
ken_burns = true

[clock]
format = "%H:%M"
date_format = "%A, %B %-d"

[font]
family = "Varela Round"
EOF
[[ "$typed" == 1 ]] && printf '\n[user]\nprompt = true\n' >> "$conf"

wl_sock="breadgreet-preview-$$"
pids=()
cleanup() {
    for p in "${pids[@]:-}"; do kill "$p" 2>/dev/null || true; done
    sleep 0.3
    for p in "${pids[@]:-}"; do kill -9 "$p" 2>/dev/null || true; done
    rm -rf "$work"
}
trap cleanup EXIT INT TERM

echo ">> starting mock greetd"
python3 "$here/mock-greetd.py" "$sock" bread &
pids+=($!)
for _ in $(seq 1 40); do [[ -S "$sock" ]] && break; sleep 0.1; done

echo ">> starting nested compositor"
weston --width=1400 --height=900 --socket="$wl_sock" >"$work/weston.log" 2>&1 &
pids+=($!)
for _ in $(seq 1 60); do [[ -S "$XDG_RUNTIME_DIR/$wl_sock" ]] && break; sleep 0.1; done

echo ">> launching breadgreet  (password: bread)"
[[ -n "$renderer" ]] && export GSK_RENDERER="$renderer"
WAYLAND_DISPLAY="$wl_sock" \
    BREADGREET_CONFIG="$conf" \
    GREETD_SOCK="$sock" \
    "$repo/target/debug/breadgreet" || true

echo ">> breadgreet exited"

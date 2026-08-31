//! D-Bus status integration — now-playing (MPRIS) and battery (upower).
//!
//! Both are polled on a single background thread (zbus's blocking API has no
//! place on the render loop) and the result is posted back through a
//! `calloop::channel`, mirroring how [`crate::auth`] bridges PAM. Session and
//! system bus connections are opened once in that thread and reused; a failed
//! call drops the connection so the next tick reconnects. Missing or broken
//! D-Bus (headless CI, a session without upower, etc.) just yields empty
//! status — this module never blocks or fails the locker.

use smithay_client_toolkit::reexports::calloop::channel::{self, Sender};
use smithay_client_toolkit::reexports::calloop::LoopHandle;
use std::collections::HashMap;
use zbus::zvariant::{Dict, OwnedValue, Value};

/// One snapshot of the system status, rendered as a small line under the
/// clock when either field is present.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatusInfo {
    /// `"{title} — {artist}"` for the currently-playing MPRIS player (the
    /// first one advertising `PlaybackStatus == "Playing"`, else the first
    /// paused one). Playing players with no title fall back to artist, the
    /// player name, or `"Playing"`. Empty when nothing is playing or MPRIS
    /// is unreachable.
    pub now_playing: String,
    /// `"87% · charging"`-style summary from upower's display device.
    /// Empty when there is no battery or upower is unreachable.
    pub battery: String,
}

/// Registers the receiving half of the status channel on the event loop and
/// returns the `Sender` the background poller hands snapshots to.
pub fn register<Data: 'static>(
    loop_handle: &LoopHandle<'static, Data>,
    mut on_update: impl FnMut(&mut Data, StatusInfo) + 'static,
) -> Sender<StatusInfo> {
    let (tx, channel) = channel::channel();
    loop_handle
        .insert_source(channel, move |event, _, data| {
            if let channel::Event::Msg(info) = event {
                on_update(data, info);
            }
        })
        .expect("failed to register status channel on event loop");
    tx
}

/// How often the background thread re-queries D-Bus.
const POLL_SECS: u64 = 3;

const MPRIS_FIELD_MAX: usize = 80;
const MPRIS_LINE_MAX: usize = 120;

/// UPower Device Type for a battery. DisplayDevice on a desktop is often
/// some other kind (line power) with `Percentage == 0`.
const UPOWER_TYPE_BATTERY: u32 = 2;

/// Spawns the poller thread. It runs for the life of the process (the locker
/// exits on unlock), re-querying every [`POLL_SECS`] seconds and forwarding
/// each snapshot. When both `now_playing` and `battery` are false, returns
/// immediately without touching D-Bus.
pub fn spawn_poller(tx: Sender<StatusInfo>, now_playing: bool, battery: bool) {
    if !now_playing && !battery {
        return;
    }
    std::thread::spawn(move || {
        let mut session: Option<zbus::blocking::Connection> = None;
        let mut system: Option<zbus::blocking::Connection> = None;
        loop {
            let info = poll_once(&mut session, &mut system, now_playing, battery);
            if tx.send(info).is_err() {
                // Event loop gone (unlocked) — nothing left to report.
                return;
            }
            std::thread::sleep(std::time::Duration::from_secs(POLL_SECS));
        }
    });
}

fn poll_once(
    session: &mut Option<zbus::blocking::Connection>,
    system: &mut Option<zbus::blocking::Connection>,
    now_playing: bool,
    battery: bool,
) -> StatusInfo {
    StatusInfo {
        now_playing: if now_playing {
            poll_now_playing(session)
        } else {
            String::new()
        },
        battery: if battery {
            poll_battery(system)
        } else {
            String::new()
        },
    }
}

fn poll_now_playing(session: &mut Option<zbus::blocking::Connection>) -> String {
    if session.is_none() {
        *session = zbus::blocking::Connection::session().ok();
    }
    match session.as_ref().map(poll_now_playing_on) {
        Some(Ok(line)) => line,
        Some(Err(())) => {
            *session = None;
            String::new()
        }
        None => String::new(),
    }
}

fn poll_now_playing_on(conn: &zbus::blocking::Connection) -> Result<String, ()> {
    let names = conn
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "ListNames",
            &(),
        )
        .and_then(|reply| reply.body().deserialize::<Vec<String>>())
        .map_err(|_| ())?;

    let mut paused: Option<String> = None;
    for name in names
        .iter()
        .filter(|n| n.starts_with("org.mpris.MediaPlayer2."))
    {
        let Some((status, title, artist)) = read_player(conn, name) else {
            continue;
        };
        let line = format_now_playing(title.as_deref(), artist.as_deref(), name);
        match status.as_str() {
            "Playing" => return Ok(line),
            "Paused" if paused.is_none() => paused = Some(line),
            _ => {}
        }
    }
    Ok(paused.unwrap_or_default())
}

fn read_player(
    conn: &zbus::blocking::Connection,
    name: &str,
) -> Option<(String, Option<String>, Option<String>)> {
    let props = conn
        .call_method(
            Some(name),
            "/org/mpris/MediaPlayer2",
            Some("org.freedesktop.DBus.Properties"),
            "GetAll",
            &("org.mpris.MediaPlayer2.Player",),
        )
        .ok()?;
    let dict: HashMap<String, OwnedValue> = props.body().deserialize().ok()?;

    let status = dict
        .get("PlaybackStatus")
        .and_then(|v| v.downcast_ref::<&str>().ok())
        .unwrap_or("")
        .to_string();
    let mut title = None;
    let mut artist = None;
    if let Some(metadata) = dict
        .get("Metadata")
        .and_then(|v| v.downcast_ref::<Dict>().ok())
    {
        title = metadata
            .get::<&str, &str>(&"xesam:title")
            .ok()
            .flatten()
            .map(str::to_string);
        artist = metadata
            .get::<&str, Value>(&"xesam:artist")
            .ok()
            .flatten()
            .and_then(|v| match v {
                Value::Array(arr) => {
                    let joined = arr
                        .iter()
                        .filter_map(|e| e.downcast_ref::<&str>().ok())
                        .collect::<Vec<_>>()
                        .join(", ");
                    if joined.is_empty() {
                        None
                    } else {
                        Some(joined)
                    }
                }
                _ => None,
            });
    }

    Some((status, title, artist))
}

/// Builds the now-playing line. Title and artist are newline-stripped and
/// capped; a Playing player with neither still yields the player name (or
/// `"Playing"`) so it is not outranked by a later titled Paused player.
fn format_now_playing(title: Option<&str>, artist: Option<&str>, player: &str) -> String {
    let title = title.map(sanitize_mpris_field).filter(|s| !s.is_empty());
    let artist = artist.map(sanitize_mpris_field).filter(|s| !s.is_empty());
    let line = match (title, artist) {
        (Some(t), Some(a)) => format!("{t} — {a}"),
        (Some(t), None) => t,
        (None, Some(a)) => a,
        (None, None) => mpris_player_fallback(player),
    };
    truncate_chars(&line, MPRIS_LINE_MAX)
}

fn sanitize_mpris_field(s: &str) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&collapsed, MPRIS_FIELD_MAX)
}

fn mpris_player_fallback(bus_name: &str) -> String {
    bus_name
        .strip_prefix("org.mpris.MediaPlayer2.")
        .and_then(|rest| rest.split('.').next())
        .filter(|s| !s.is_empty())
        .unwrap_or("Playing")
        .to_string()
}

fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        None => s.to_string(),
        Some((idx, _)) => s[..idx].to_string(),
    }
}

fn poll_battery(system: &mut Option<zbus::blocking::Connection>) -> String {
    if system.is_none() {
        *system = zbus::blocking::Connection::system().ok();
    }
    match system.as_ref().map(poll_battery_on) {
        Some(Ok(line)) => line,
        Some(Err(())) => {
            *system = None;
            String::new()
        }
        None => String::new(),
    }
}

fn poll_battery_on(conn: &zbus::blocking::Connection) -> Result<String, ()> {
    let path = conn
        .call_method(
            Some("org.freedesktop.UPower"),
            "/org/freedesktop/UPower",
            Some("org.freedesktop.UPower"),
            "GetDisplayDevice",
            &(),
        )
        .and_then(|reply| {
            reply
                .body()
                .deserialize::<zbus::zvariant::OwnedObjectPath>()
        })
        .map_err(|_| ())?;
    let props = conn
        .call_method(
            Some("org.freedesktop.UPower"),
            path.as_str(),
            Some("org.freedesktop.DBus.Properties"),
            "GetAll",
            &("org.freedesktop.UPower.Device",),
        )
        .and_then(|reply| reply.body().deserialize::<HashMap<String, OwnedValue>>())
        .map_err(|_| ())?;
    // DisplayDevice always exists; without a battery IsPresent is false
    // and Percentage is often 0. Missing IsPresent is treated as absent.
    let present = props
        .get("IsPresent")
        .and_then(|v| v.downcast_ref::<bool>().ok())
        .unwrap_or(false);
    if let Some(kind) = props.get("Type").and_then(|v| v.downcast_ref::<u32>().ok()) {
        if kind != UPOWER_TYPE_BATTERY {
            return Ok(String::new());
        }
    }
    let Some(pct) = props
        .get("Percentage")
        .and_then(|v| v.downcast_ref::<f64>().ok())
    else {
        return Ok(String::new());
    };
    let state = props
        .get("State")
        .and_then(|v| v.downcast_ref::<u32>().ok())
        .unwrap_or(0);
    Ok(format_battery(present, pct, state))
}

fn format_battery(present: bool, pct: f64, state: u32) -> String {
    if !present {
        return String::new();
    }
    // UPower Device state: 1 charging, 2 discharging, 3 empty, 4 full.
    let suffix = match state {
        1 => " · charging",
        2 => "",
        4 => " · full",
        _ => "",
    };
    format!("{pct:.0}%{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_battery_absent_is_empty() {
        assert_eq!(format_battery(false, 0.0, 0), "");
        assert_eq!(format_battery(false, 87.4, 1), "");
    }

    #[test]
    fn format_battery_present_covers_common_states() {
        assert_eq!(format_battery(true, 87.4, 1), "87% · charging");
        assert_eq!(format_battery(true, 43.0, 2), "43%");
        assert_eq!(format_battery(true, 100.0, 4), "100% · full");
        assert_eq!(format_battery(true, 2.0, 3), "2%");
        // Laptop at 0% still has a battery; desktops are filtered via IsPresent.
        assert_eq!(format_battery(true, 0.0, 2), "0%");
    }

    #[test]
    fn format_now_playing_joins_title_and_artist() {
        assert_eq!(
            format_now_playing(
                Some("Paranoid Android"),
                Some("Radiohead"),
                "org.mpris.MediaPlayer2.spotify"
            ),
            "Paranoid Android — Radiohead"
        );
        assert_eq!(
            format_now_playing(Some("Untitled"), None, "org.mpris.MediaPlayer2.mpv"),
            "Untitled"
        );
    }

    #[test]
    fn format_now_playing_playing_without_title_uses_fallback() {
        assert_eq!(
            format_now_playing(None, Some("Radiohead"), "org.mpris.MediaPlayer2.spotify"),
            "Radiohead"
        );
        assert_eq!(
            format_now_playing(None, None, "org.mpris.MediaPlayer2.spotify"),
            "spotify"
        );
        assert_eq!(
            format_now_playing(None, None, "org.mpris.MediaPlayer2.firefox.instance1"),
            "firefox"
        );
        assert_eq!(format_now_playing(None, None, ""), "Playing");
        assert_eq!(
            format_now_playing(Some("\n\n"), None, "org.mpris.MediaPlayer2.mpv"),
            "mpv"
        );
    }

    #[test]
    fn format_now_playing_strips_newlines_and_truncates() {
        assert_eq!(
            format_now_playing(Some("foo\nbar"), Some("a\r\nb"), "org.mpris.MediaPlayer2.x"),
            "foo bar — a b"
        );
        let title = "T".repeat(100);
        let titled = format_now_playing(Some(&title), None, "org.mpris.MediaPlayer2.x");
        assert_eq!(titled.chars().count(), MPRIS_FIELD_MAX);
        assert!(!titled.contains('\n'));
        let artist = "A".repeat(100);
        let combined = format_now_playing(Some(&title), Some(&artist), "org.mpris.MediaPlayer2.x");
        assert_eq!(combined.chars().count(), MPRIS_LINE_MAX);
        assert!(combined.starts_with('T'));
        assert!(!combined.contains('\n'));
    }

    #[test]
    fn spawn_poller_both_false_returns() {
        let (tx, _rx) = channel::channel();
        spawn_poller(tx, false, false);
    }
}

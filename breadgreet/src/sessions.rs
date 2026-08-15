//! Session discovery: scans the standard greetd-greeter session directories
//! for `.desktop` entries, lists them for the picker, and resolves the
//! chosen entry's `Exec=` line for `greetd`'s `StartSession`.
//!
//! Default selection matches by `.desktop` file stem (`bos` compiled-in,
//! overridable via `[sessions].default`). If that stem is missing, the
//! first entry from `wayland_dirs` then `xsessions_dirs` is used.

use breadlock_ui::desktop_entry::scan_dir;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// `.desktop` file stem (`bos` for `bos.desktop`) — used to match
    /// `[sessions].default`.
    pub stem: String,
    pub name: String,
    pub exec: Vec<String>,
}

/// Every installed session, `wayland_dirs` first then `xsessions_dirs`.
/// Each directory is sorted by stem (see [`scan_dir`]).
pub fn list(wayland_dirs: &[String], xsessions_dirs: &[String]) -> Vec<Session> {
    let mut all = Vec::new();
    for dir in wayland_dirs.iter().chain(xsessions_dirs) {
        for (stem, entry) in scan_dir(Path::new(dir)) {
            all.push(Session {
                stem,
                name: entry.name,
                exec: split_exec(&entry.exec),
            });
        }
    }
    all
}

/// Index of the configured default stem, or `0` if it is absent. Callers
/// with an empty list should not use this as a subscript.
pub fn default_index(sessions: &[Session], default: &str) -> usize {
    sessions.iter().position(|s| s.stem == default).unwrap_or(0)
}

/// Scans `wayland_dirs` then `xsessions_dirs` (in that order) and returns
/// the entry matching `default` (by `.desktop` file stem), falling back to
/// the first entry found in either directory. `None` if nothing is
/// installed — the greeter has no session to offer.
pub fn discover(
    wayland_dirs: &[String],
    xsessions_dirs: &[String],
    default: &str,
) -> Option<Session> {
    let all = list(wayland_dirs, xsessions_dirs);
    let idx = default_index(&all, default);
    all.into_iter().nth(idx)
}

/// Splits a `.desktop` `Exec=` line into an argv. Only handles plain
/// whitespace-separated commands (BOS's own `hyprland.desktop` is
/// `Exec=Hyprland`) — full field-code (`%f`, `%u`, …) and quoting support
/// isn't needed for a greeter that never launches file-manager-style
/// entries.
fn split_exec(exec: &str) -> Vec<String> {
    exec.split_whitespace()
        .filter(|arg| !arg.starts_with('%'))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_exec_drops_field_codes() {
        assert_eq!(split_exec("Hyprland"), vec!["Hyprland"]);
        assert_eq!(split_exec("gnome-session %U"), vec!["gnome-session"]);
    }

    #[test]
    fn discover_returns_none_when_no_directories_exist() {
        assert!(discover(
            &["/nonexistent/a".to_string()],
            &["/nonexistent/b".to_string()],
            "hyprland"
        )
        .is_none());
    }

    fn write_fixture(dir: &std::path::Path, stem: &str, name: &str, exec: &str) {
        std::fs::write(
            dir.join(format!("{stem}.desktop")),
            format!("[Desktop Entry]\nName={name}\nExec={exec}\n"),
        )
        .unwrap();
    }

    #[test]
    fn discover_prefers_configured_default_over_first_entry() {
        let dir = std::env::temp_dir().join(format!(
            "breadgreet-test-sessions-discover-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        write_fixture(&dir, "aaa", "A", "a-cmd");
        write_fixture(&dir, "hyprland", "Hyprland", "Hyprland");

        let dir_str = dir.to_str().unwrap().to_string();
        let session = discover(&[dir_str], &[], "hyprland").unwrap();
        assert_eq!(session.stem, "hyprland");
        assert_eq!(session.name, "Hyprland");
        assert_eq!(session.exec, vec!["Hyprland"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_returns_all_sessions_wayland_then_x() {
        let pid = std::process::id();
        let wayland = std::env::temp_dir().join(format!("breadgreet-test-sessions-list-w-{pid}"));
        let x11 = std::env::temp_dir().join(format!("breadgreet-test-sessions-list-x-{pid}"));
        std::fs::create_dir_all(&wayland).unwrap();
        std::fs::create_dir_all(&x11).unwrap();
        write_fixture(&wayland, "bos", "BOS", "/usr/local/bin/bos-session");
        write_fixture(&wayland, "hyprland", "Hyprland", "Hyprland");
        write_fixture(&x11, "openbox", "Openbox", "openbox-session");

        let listed = list(
            &[wayland.to_str().unwrap().to_string()],
            &[x11.to_str().unwrap().to_string()],
        );
        let stems: Vec<&str> = listed.iter().map(|s| s.stem.as_str()).collect();
        assert_eq!(stems, vec!["bos", "hyprland", "openbox"]);
        assert_eq!(listed[0].exec, vec!["/usr/local/bin/bos-session"]);

        std::fs::remove_dir_all(&wayland).ok();
        std::fs::remove_dir_all(&x11).ok();
    }

    #[test]
    fn default_index_prefers_bos_then_first() {
        let sessions = vec![
            Session {
                stem: "aaa".into(),
                name: "A".into(),
                exec: vec!["a".into()],
            },
            Session {
                stem: "bos".into(),
                name: "BOS".into(),
                exec: vec!["bos-session".into()],
            },
        ];
        assert_eq!(default_index(&sessions, "bos"), 1);
        assert_eq!(default_index(&sessions, "missing"), 0);
        assert_eq!(default_index(&[], "bos"), 0);
    }
}

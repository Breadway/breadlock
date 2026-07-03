//! Session discovery: scans the standard greetd-greeter session directories
//! for `.desktop` entries. BOS effectively ships one session (Hyprland via
//! `bos-session`), so v1 has no picker UI — it just auto-selects the
//! configured default (or the only entry found) and resolves its `Exec=`
//! line to hand to `greetd`'s `StartSession`.

use breadlock_ui::desktop_entry::{scan_dir, DesktopEntry};
use std::path::Path;

pub struct Session {
    pub name: String,
    pub exec: Vec<String>,
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
    let mut all: Vec<(String, DesktopEntry)> = Vec::new();
    for dir in wayland_dirs.iter().chain(xsessions_dirs) {
        all.extend(scan_dir(Path::new(dir)));
    }

    let chosen = all
        .iter()
        .find(|(stem, _)| stem == default)
        .or_else(|| all.first())?;

    Some(Session {
        name: chosen.1.name.clone(),
        exec: split_exec(&chosen.1.exec),
    })
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

    #[test]
    fn discover_prefers_configured_default_over_first_entry() {
        let dir = std::env::temp_dir().join("breadgreet-test-sessions-discover");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("aaa.desktop"),
            "[Desktop Entry]\nName=A\nExec=a-cmd\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("hyprland.desktop"),
            "[Desktop Entry]\nName=Hyprland\nExec=Hyprland\n",
        )
        .unwrap();

        let dir_str = dir.to_str().unwrap().to_string();
        let session = discover(&[dir_str], &[], "hyprland").unwrap();
        assert_eq!(session.name, "Hyprland");
        assert_eq!(session.exec, vec!["Hyprland"]);

        std::fs::remove_dir_all(&dir).ok();
    }
}

//! Session discovery: scans the standard greetd-greeter session directories
//! for `.desktop` entries, lists them for the picker, and resolves the
//! chosen entry's `Exec=` line for `greetd`'s `StartSession`.
//!
//! Default selection matches by `.desktop` file stem (`bos` compiled-in,
//! overridable via `[sessions].default`). If that stem is missing, the
//! first entry from `wayland_dirs` then `xsessions_dirs` is used.

use breadlock_ui::desktop_entry::scan_dir;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    Wayland,
    X11,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// `.desktop` file stem (`bos` for `bos.desktop`) — used to match
    /// `[sessions].default`.
    pub stem: String,
    pub name: String,
    pub exec: Vec<String>,
    /// Which directory list this entry came from — drives `XDG_SESSION_TYPE`.
    pub kind: SessionKind,
}

impl Session {
    /// Environment greetd should apply to the started session.
    pub fn start_env(&self) -> Vec<String> {
        let session_type = match self.kind {
            SessionKind::Wayland => "wayland",
            SessionKind::X11 => "x11",
        };
        let desktop = if self.stem.is_empty() {
            self.name.as_str()
        } else {
            self.stem.as_str()
        };
        vec![
            format!("XDG_SESSION_TYPE={session_type}"),
            format!("XDG_SESSION_DESKTOP={desktop}"),
            format!("XDG_CURRENT_DESKTOP={desktop}"),
        ]
    }
}

/// Every installed session, `wayland_dirs` first then `xsessions_dirs`.
/// Each directory is sorted by stem (see [`scan_dir`]).
pub fn list(wayland_dirs: &[String], xsessions_dirs: &[String]) -> Vec<Session> {
    let mut all = Vec::new();
    collect_into(&mut all, wayland_dirs, SessionKind::Wayland);
    collect_into(&mut all, xsessions_dirs, SessionKind::X11);
    all
}

fn collect_into(all: &mut Vec<Session>, dirs: &[String], kind: SessionKind) {
    for dir in dirs {
        for (stem, entry) in scan_dir(Path::new(dir)) {
            all.push(Session {
                stem,
                name: entry.name,
                exec: split_exec(&entry.exec),
                kind,
            });
        }
    }
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

/// Splits a `.desktop` `Exec=` line into an argv. Double-quoted arguments
/// are one token (Freedesktop Exec quoting). Whole-argument field codes
/// (`%f`, `%F`, …) are dropped; `%%` is a literal `%`.
fn split_exec(exec: &str) -> Vec<String> {
    tokenize_exec(exec)
        .into_iter()
        .filter(|arg| !is_field_code(arg))
        .map(|arg| unescape_percent(&arg))
        .filter(|arg| !arg.is_empty())
        .collect()
}

fn tokenize_exec(exec: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    let mut chars = exec.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '"' => in_quote = !in_quote,
            '\\' if in_quote => {
                if let Some(n) = chars.next() {
                    current.push(n);
                }
            }
            c if c.is_whitespace() && !in_quote => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

fn is_field_code(arg: &str) -> bool {
    matches!(
        arg,
        "%f" | "%F" | "%u" | "%U" | "%d" | "%D" | "%n" | "%N" | "%i" | "%c" | "%k" | "%v" | "%m"
    )
}

fn unescape_percent(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len());
    let mut chars = arg.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' && chars.peek() == Some(&'%') {
            chars.next();
            out.push('%');
        } else {
            out.push(c);
        }
    }
    out
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
    fn split_exec_quoted_arguments() {
        assert_eq!(
            split_exec(r#"wrapper "my session" --flag"#),
            vec!["wrapper", "my session", "--flag"]
        );
    }

    #[test]
    fn split_exec_double_percent_is_literal() {
        assert_eq!(split_exec("echo %%"), vec!["echo", "%"]);
        assert_eq!(split_exec(r#"echo "100%%""#), vec!["echo", "100%"]);
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
        assert_eq!(session.kind, SessionKind::Wayland);

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
        assert_eq!(listed[0].kind, SessionKind::Wayland);
        assert_eq!(listed[2].kind, SessionKind::X11);
        assert!(
            listed[2]
                .start_env()
                .contains(&"XDG_SESSION_TYPE=x11".to_string())
        );
        assert!(
            listed[0]
                .start_env()
                .contains(&"XDG_SESSION_TYPE=wayland".to_string())
        );
        assert!(
            listed[0]
                .start_env()
                .contains(&"XDG_SESSION_DESKTOP=bos".to_string())
        );

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
                kind: SessionKind::Wayland,
            },
            Session {
                stem: "bos".into(),
                name: "BOS".into(),
                exec: vec!["bos-session".into()],
                kind: SessionKind::Wayland,
            },
        ];
        assert_eq!(default_index(&sessions, "bos"), 1);
        assert_eq!(default_index(&sessions, "missing"), 0);
        assert_eq!(default_index(&[], "bos"), 0);
    }
}

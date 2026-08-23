//! Minimal freedesktop `.desktop` entry parsing — just enough to discover
//! session launchers (`Name=`, `Exec=`, `Type=`) under
//! `/usr/share/wayland-sessions` and `/usr/share/xsessions`. Also honours
//! `Hidden=` / `NoDisplay=` / `TryExec=` so we don't offer sessions that
//! menus would skip. Localized `Name[xx]=` and `Actions=` are out of scope.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopEntry {
    pub name: String,
    pub exec: String,
    pub entry_type: String,
    /// `TryExec=` if present — [`scan_dir`] skips the entry when this
    /// binary is missing from disk/`PATH`.
    pub try_exec: Option<String>,
}

/// Parses the `[Desktop Entry]` section of a `.desktop` file's contents.
/// Returns `None` if `Name=` or `Exec=` is missing, or if `Hidden=true` /
/// `NoDisplay=true`.
pub fn parse(contents: &str) -> Option<DesktopEntry> {
    let mut name = None;
    let mut exec = None;
    let mut entry_type = None;
    let mut try_exec = None;
    let mut hidden = false;
    let mut no_display = false;
    let mut in_desktop_entry = false;

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(section) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            in_desktop_entry = section == "Desktop Entry";
            continue;
        }
        if !in_desktop_entry {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            match key.trim() {
                "Name" => name = Some(value.trim().to_string()),
                "Exec" => exec = Some(value.trim().to_string()),
                "Type" => entry_type = Some(value.trim().to_string()),
                "TryExec" => {
                    let v = value.trim();
                    if !v.is_empty() {
                        try_exec = Some(v.to_string());
                    }
                }
                "Hidden" => hidden = is_desktop_true(value),
                "NoDisplay" => no_display = is_desktop_true(value),
                _ => {}
            }
        }
    }

    if hidden || no_display {
        return None;
    }

    Some(DesktopEntry {
        name: name?,
        exec: exec?,
        entry_type: entry_type.unwrap_or_else(|| "Application".to_string()),
        try_exec,
    })
}

fn is_desktop_true(value: &str) -> bool {
    value.trim().eq_ignore_ascii_case("true")
}

/// Scans a directory for `*.desktop` files, returning `(file stem, entry)`
/// pairs. Unreadable directories and unparsable entries are silently skipped
/// — a missing session directory is normal (e.g. no X11 sessions installed).
/// Entries whose `TryExec=` binary is missing are skipped too.
pub fn scan_dir(dir: &Path) -> Vec<(String, DesktopEntry)> {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut entries: Vec<(String, DesktopEntry)> = read_dir
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "desktop"))
        .filter_map(|e| {
            let stem = e.path().file_stem()?.to_str()?.to_string();
            let contents = std::fs::read_to_string(e.path()).ok()?;
            let entry = parse(&contents)?;
            if let Some(ref te) = entry.try_exec {
                if !command_exists(te) {
                    return None;
                }
            }
            Some((stem, entry))
        })
        .collect();

    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

fn command_exists(cmd: &str) -> bool {
    if cmd.contains('/') {
        is_runnable(Path::new(cmd))
    } else {
        match std::env::var_os("PATH") {
            Some(paths) => std::env::split_paths(&paths).any(|dir| is_runnable(&dir.join(cmd))),
            None => false,
        }
    }
}

fn is_runnable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    meta.is_file() && meta.permissions().mode() & 0o111 != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const HYPRLAND_DESKTOP: &str = "[Desktop Entry]\n\
        Name=Hyprland\n\
        Comment=An intelligent dynamic tiling Wayland compositor\n\
        Exec=Hyprland\n\
        Type=Application\n";

    fn unique_temp_dir(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "breadlock-ui-test-sessions-{name}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_name_exec_type() {
        let e = parse(HYPRLAND_DESKTOP).unwrap();
        assert_eq!(e.name, "Hyprland");
        assert_eq!(e.exec, "Hyprland");
        assert_eq!(e.entry_type, "Application");
        assert_eq!(e.try_exec, None);
    }

    #[test]
    fn ignores_keys_outside_desktop_entry_section() {
        let contents = "[Desktop Action foo]\nName=Not this one\n\
            [Desktop Entry]\nName=Real\nExec=real-cmd\n";
        let e = parse(contents).unwrap();
        assert_eq!(e.name, "Real");
        assert_eq!(e.exec, "real-cmd");
    }

    #[test]
    fn missing_exec_returns_none() {
        assert!(parse("[Desktop Entry]\nName=Broken\n").is_none());
    }

    #[test]
    fn missing_type_defaults_to_application() {
        let e = parse("[Desktop Entry]\nName=X\nExec=x\n").unwrap();
        assert_eq!(e.entry_type, "Application");
    }

    #[test]
    fn hidden_or_nodisplay_returns_none() {
        assert!(parse("[Desktop Entry]\nName=X\nExec=x\nHidden=true\n").is_none());
        assert!(parse("[Desktop Entry]\nName=X\nExec=x\nNoDisplay=true\n").is_none());
        assert!(parse("[Desktop Entry]\nName=X\nExec=x\nHidden=false\n").is_some());
        assert!(parse("[Desktop Entry]\nName=X\nExec=x\nNoDisplay=false\n").is_some());
    }

    #[test]
    fn scan_dir_on_missing_directory_returns_empty() {
        assert!(scan_dir(Path::new("/nonexistent/wayland-sessions")).is_empty());
    }

    #[test]
    fn scan_dir_finds_and_sorts_desktop_files() {
        let dir = unique_temp_dir("scan");
        std::fs::write(dir.join("zzz.desktop"), HYPRLAND_DESKTOP).unwrap();
        std::fs::write(dir.join("aaa.desktop"), "[Desktop Entry]\nName=A\nExec=a\n").unwrap();
        std::fs::write(dir.join("not-a-session.txt"), "ignored").unwrap();

        let found = scan_dir(&dir);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].0, "aaa");
        assert_eq!(found[1].0, "zzz");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_dir_skips_hidden_nodisplay_and_missing_tryexec() {
        let dir = unique_temp_dir("skip");
        std::fs::write(
            dir.join("hidden.desktop"),
            "[Desktop Entry]\nName=Hidden\nExec=hidden\nHidden=true\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("nodisp.desktop"),
            "[Desktop Entry]\nName=NoDisp\nExec=nodisp\nNoDisplay=true\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("gone.desktop"),
            "[Desktop Entry]\nName=Gone\nExec=gone\nTryExec=/no/such/breadlock-tryexec\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("ok.desktop"),
            "[Desktop Entry]\nName=Ok\nExec=ok\n",
        )
        .unwrap();

        let found = scan_dir(&dir);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "ok");

        std::fs::remove_dir_all(&dir).ok();
    }
}

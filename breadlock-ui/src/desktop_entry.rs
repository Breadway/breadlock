//! Minimal freedesktop `.desktop` entry parsing — just enough to discover
//! session launchers (`Name=`, `Exec=`, `Type=`) under
//! `/usr/share/wayland-sessions` and `/usr/share/xsessions`. BOS only ships
//! one session today, so this deliberately doesn't handle the full spec
//! (localized `Name[xx]=`, `Exec=` quoting/field codes, `Actions=`, etc.) —
//! only the three keys a greeter needs to list and launch a session.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopEntry {
    pub name: String,
    pub exec: String,
    pub entry_type: String,
}

/// Parses the `[Desktop Entry]` section of a `.desktop` file's contents.
/// Returns `None` if `Name=` or `Exec=` is missing.
pub fn parse(contents: &str) -> Option<DesktopEntry> {
    let mut name = None;
    let mut exec = None;
    let mut entry_type = None;
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
                _ => {}
            }
        }
    }

    Some(DesktopEntry {
        name: name?,
        exec: exec?,
        entry_type: entry_type.unwrap_or_else(|| "Application".to_string()),
    })
}

/// Scans a directory for `*.desktop` files, returning `(file stem, entry)`
/// pairs. Unreadable directories and unparsable entries are silently skipped
/// — a missing session directory is normal (e.g. no X11 sessions installed).
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
            Some((stem, parse(&contents)?))
        })
        .collect();

    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    const HYPRLAND_DESKTOP: &str = "[Desktop Entry]\n\
        Name=Hyprland\n\
        Comment=An intelligent dynamic tiling Wayland compositor\n\
        Exec=Hyprland\n\
        Type=Application\n";

    #[test]
    fn parses_name_exec_type() {
        let e = parse(HYPRLAND_DESKTOP).unwrap();
        assert_eq!(e.name, "Hyprland");
        assert_eq!(e.exec, "Hyprland");
        assert_eq!(e.entry_type, "Application");
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
    fn scan_dir_on_missing_directory_returns_empty() {
        assert!(scan_dir(Path::new("/nonexistent/wayland-sessions")).is_empty());
    }

    #[test]
    fn scan_dir_finds_and_sorts_desktop_files() {
        let dir = std::env::temp_dir().join("breadlock-ui-test-sessions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("zzz.desktop"), HYPRLAND_DESKTOP).unwrap();
        std::fs::write(dir.join("aaa.desktop"), "[Desktop Entry]\nName=A\nExec=a\n").unwrap();
        std::fs::write(dir.join("not-a-session.txt"), "ignored").unwrap();

        let found = scan_dir(&dir);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].0, "aaa");
        assert_eq!(found[1].0, "zzz");

        std::fs::remove_dir_all(&dir).ok();
    }
}

use breadlock_ui::config::Appearance;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(flatten)]
    pub appearance: Appearance,
    pub sessions: Sessions,
    pub user: User,
}

/// Who the greeter logs in. By default breadgreet enumerates the system's
/// human accounts (`/etc/passwd`) and skips the username field: one account
/// goes straight to the password prompt, several offer a picker. These keys
/// override that.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct User {
    /// Force this login name and don't enumerate. Empty = auto-detect.
    pub name: String,
    /// Always ask for the username by hand (the pre-enumeration behaviour).
    pub prompt: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Sessions {
    pub wayland_dirs: Vec<String>,
    pub xsessions_dirs: Vec<String>,
    /// `.desktop` file stem (without extension) to pre-select in the picker.
    /// Falls back to the first discovered session if this stem is missing.
    pub default: String,
}

impl Default for Sessions {
    fn default() -> Self {
        Self {
            wayland_dirs: vec!["/usr/share/wayland-sessions".to_string()],
            xsessions_dirs: vec!["/usr/share/xsessions".to_string()],
            default: "bos".to_string(),
        }
    }
}

/// `breadgreet` commonly runs as the dedicated `greeter` system user (per
/// BOS's `/etc/greetd/config.toml` `user = "greeter"`), so a fixed system
/// path is checked first; XDG is the fallback for local dev/testing under a
/// normal user session.
///
/// `$BREADGREET_CONFIG` overrides both — an explicit file to load, used by
/// `scripts/preview.sh` so a preview doesn't have to touch the real
/// `/etc/greetd/breadgreet.toml`.
pub fn load() -> Config {
    load_with_override(std::env::var_os("BREADGREET_CONFIG"))
}

/// [`load`] with the `$BREADGREET_CONFIG` value passed in explicitly, so the
/// resolution order is testable without mutating process-global env state
/// (which parallel `cargo test` threads race on).
fn load_with_override(explicit: Option<std::ffi::OsString>) -> Config {
    if let Some(explicit) = explicit {
        return breadlock_ui::config::load_or_default(std::path::Path::new(&explicit));
    }
    let system_path = std::path::Path::new("/etc/greetd/breadgreet.toml");
    if system_path.exists() {
        return breadlock_ui::config::load_or_default(system_path);
    }
    breadlock_ui::config::load_or_default(&xdg_config_path())
}

pub(crate) fn xdg_config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn xdg_config_path() -> PathBuf {
    xdg_config_dir().join("breadgreet").join("breadgreet.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_sessions_match_standard_greetd_dirs() {
        let s = Sessions::default();
        assert_eq!(s.wayland_dirs, vec!["/usr/share/wayland-sessions"]);
        assert_eq!(s.xsessions_dirs, vec!["/usr/share/xsessions"]);
        assert_eq!(s.default, "bos");
    }

    #[test]
    fn breadgreet_config_override_wins_over_the_search_path() {
        let path =
            std::env::temp_dir().join(format!("breadgreet-cfg-env-{}.toml", std::process::id()));
        std::fs::write(&path, "[clock]\nformat = \"%I:%M %p\"\n").unwrap();
        let cfg = load_with_override(Some(path.clone().into_os_string()));
        std::fs::remove_file(&path).ok();
        assert_eq!(cfg.appearance.clock.format, "%I:%M %p");
    }
}

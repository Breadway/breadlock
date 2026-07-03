use breadlock_ui::config::Appearance;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(flatten)]
    pub appearance: Appearance,
    pub sessions: Sessions,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Sessions {
    pub wayland_dirs: Vec<String>,
    pub xsessions_dirs: Vec<String>,
    /// `.desktop` file stem (without extension) to auto-select.
    pub default: String,
}

impl Default for Sessions {
    fn default() -> Self {
        Self {
            wayland_dirs: vec!["/usr/share/wayland-sessions".to_string()],
            xsessions_dirs: vec!["/usr/share/xsessions".to_string()],
            default: "hyprland".to_string(),
        }
    }
}

/// `breadgreet` commonly runs as the dedicated `greeter` system user (per
/// BOS's `/etc/greetd/config.toml` `user = "greeter"`), so a fixed system
/// path is checked first; XDG is the fallback for local dev/testing under a
/// normal user session.
pub fn load() -> Config {
    let system_path = std::path::Path::new("/etc/greetd/breadgreet.toml");
    if system_path.exists() {
        return breadlock_ui::config::load_or_default(system_path);
    }
    breadlock_ui::config::load_or_default(&xdg_config_path())
}

fn xdg_config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("breadgreet").join("breadgreet.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_sessions_match_standard_greetd_dirs() {
        let s = Sessions::default();
        assert_eq!(s.wayland_dirs, vec!["/usr/share/wayland-sessions"]);
        assert_eq!(s.xsessions_dirs, vec!["/usr/share/xsessions"]);
        assert_eq!(s.default, "hyprland");
    }
}

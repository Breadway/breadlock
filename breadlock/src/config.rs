use breadlock_ui::config::Appearance;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(flatten)]
    pub appearance: Appearance,
    pub input: Input,
    pub animation: Animation,
    pub status: Status,
}

/// System-status line under the clock (D-Bus). Both default on; they are
/// polled on a background thread and degrade silently when D-Bus or the
/// relevant service is unavailable.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Status {
    /// Show the currently-playing MPRIS track under the clock.
    pub now_playing: bool,
    /// Show the upower battery percentage under the clock.
    pub battery: bool,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            now_playing: true,
            battery: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Input {
    /// How long the red "wrong password" UI stays up. Input is not blocked
    /// during this window — typing or Escape clears it immediately.
    pub fail_timeout_ms: u64,
    /// Hold `Tab` to reveal the typed password as plain characters instead
    /// of dots. Off by default: plaintext would sit in compositor buffers
    /// while held. Tab can never be part of a password (it produces no
    /// utf8), so holding it is always safe to use as a reveal gesture.
    pub reveal_hold: bool,
}

impl Default for Input {
    fn default() -> Self {
        Self {
            fail_timeout_ms: 800,
            reveal_hold: false,
        }
    }
}

/// Idle animation toggles. Everything here runs on a low-duty-cycle timer so
/// the software-rendered lock screen doesn't burn CPU while idle.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Animation {
    /// Subtle glow pulse on the password pill every few seconds — proves the
    /// screen is live, not frozen. Runs only during a short active window of
    /// each cycle (see `BREATHE_*` in render.rs).
    pub breathe: bool,
    /// Deepen the dim veil after this many seconds of no keystrokes (0 =
    /// off). A gentle extra darkening for OLED/burn-in and late-night
    /// comfort; ramps in over a few seconds once the idle threshold hits.
    pub idle_dim_after_secs: u64,
}

impl Default for Animation {
    fn default() -> Self {
        Self {
            breathe: true,
            idle_dim_after_secs: 0,
        }
    }
}

pub fn load() -> Config {
    breadlock_ui::config::load_or_default(&config_path())
}

fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("breadlock").join("breadlock.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_expected_fail_timeout() {
        assert_eq!(Config::default().input.fail_timeout_ms, 800);
    }

    #[test]
    fn default_reveal_hold_is_off() {
        assert!(!Config::default().input.reveal_hold);
    }

    #[test]
    fn default_animation_breathe_is_on() {
        assert!(Config::default().animation.breathe);
    }

    #[test]
    fn status_defaults_on() {
        let cfg = Config::default();
        assert!(cfg.status.now_playing);
        assert!(cfg.status.battery);
    }

    #[test]
    fn status_can_be_turned_off() {
        let toml = "[status]\nnow_playing = false\nbattery = false\n";
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(!cfg.status.now_playing);
        assert!(!cfg.status.battery);
    }

    #[test]
    fn flattened_appearance_parses_alongside_input() {
        let toml = "[clock]\nformat = \"%H:%M:%S\"\n[input]\nfail_timeout_ms = 1200\n";
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.appearance.clock.format, "%H:%M:%S");
        assert_eq!(cfg.input.fail_timeout_ms, 1200);
    }
}

use breadlock_ui::config::Appearance;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(flatten)]
    pub appearance: Appearance,
    pub input: Input,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Input {
    /// How long the "wrong password" shake shows before input re-enables.
    pub fail_timeout_ms: u64,
}

impl Default for Input {
    fn default() -> Self {
        Self {
            fail_timeout_ms: 800,
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
    fn flattened_appearance_parses_alongside_input() {
        let toml = "[clock]\nformat = \"%H:%M:%S\"\n[input]\nfail_timeout_ms = 1200\n";
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.appearance.clock.format, "%H:%M:%S");
        assert_eq!(cfg.input.fail_timeout_ms, 1200);
    }
}

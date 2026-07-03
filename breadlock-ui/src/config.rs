use serde::Deserialize;
use std::path::Path;

/// Appearance settings shared by `breadlock.toml` and `breadgreet.toml`.
/// `breadgreet` embeds this and adds its own `[sessions]` table.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub background: Background,
    pub clock: Clock,
    pub font: Font,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackgroundMode {
    Color,
    Image,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Background {
    pub mode: BackgroundMode,
    pub path: String,
    /// v2 feature flag — no-op (with a warning) in v1, which only supports a
    /// static color or image background.
    pub blur: bool,
}

impl Default for Background {
    fn default() -> Self {
        Self {
            mode: BackgroundMode::Color,
            path: String::new(),
            blur: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Clock {
    pub format: String,
}

impl Default for Clock {
    fn default() -> Self {
        Self {
            format: "%H:%M".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Font {
    pub family: String,
}

impl Default for Font {
    fn default() -> Self {
        Self {
            family: bread_theme::tokens::FONT_FAMILY
                .split(',')
                .next()
                .unwrap_or("Varela Round")
                .trim()
                .to_string(),
        }
    }
}

/// Reads and parses a TOML config file, falling back to `T::default()` if the
/// file is missing or malformed — every bread* app runs with sensible
/// defaults and no required config.
pub fn load_or_default<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_design_system() {
        let a = Appearance::default();
        assert_eq!(a.background.mode, BackgroundMode::Color);
        assert_eq!(a.clock.format, "%H:%M");
        assert_eq!(a.font.family, "Varela Round");
    }

    #[test]
    fn missing_file_falls_back_to_default() {
        let a: Appearance = load_or_default(Path::new("/nonexistent/breadlock-test.toml"));
        assert_eq!(a.font.family, "Varela Round");
    }

    #[test]
    fn parses_partial_toml_with_defaults_for_rest() {
        let dir = std::env::temp_dir().join("breadlock-ui-test-partial.toml");
        std::fs::write(&dir, "[clock]\nformat = \"%I:%M %p\"\n").unwrap();
        let a: Appearance = load_or_default(&dir);
        assert_eq!(a.clock.format, "%I:%M %p");
        assert_eq!(a.background.mode, BackgroundMode::Color);
        std::fs::remove_file(&dir).ok();
    }
}

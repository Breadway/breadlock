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
    /// Slow Ken Burns pan on image backgrounds (a gentle drift + zoom instead
    /// of a static image). CPU cost: the background redraws continuously at a
    /// low frame rate while locked, so this is opt-in.
    pub ken_burns: bool,
}

impl Default for Background {
    fn default() -> Self {
        Self {
            mode: BackgroundMode::Color,
            path: String::new(),
            blur: false,
            ken_burns: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Clock {
    pub format: String,
    /// strftime format for the date line under the clock. Empty string hides
    /// the date. `%A` = full weekday, `%b` = abbreviated month, `%d` = day.
    pub date_format: String,
}

impl Default for Clock {
    fn default() -> Self {
        Self {
            format: "%H:%M".to_string(),
            date_format: "%A · %b %d".to_string(),
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

/// Reads and parses a TOML config file. A missing file is a silent
/// `T::default()`; a present but malformed file prints a warning (with the
/// path) and also falls back to `T::default()`.
pub fn load_or_default<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    match std::fs::read_to_string(path) {
        Ok(s) => match toml::from_str(&s) {
            Ok(parsed) => parsed,
            Err(err) => {
                eprintln!(
                    "warning: failed to parse {}: {err} — using defaults",
                    path.display()
                );
                T::default()
            }
        },
        Err(_) => T::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_design_system() {
        let a = Appearance::default();
        assert_eq!(a.background.mode, BackgroundMode::Color);
        assert!(
            !a.background.ken_burns,
            "Ken Burns must be opt-in (CPU cost)"
        );
        assert_eq!(a.clock.format, "%H:%M");
        assert_eq!(a.clock.date_format, "%A · %b %d");
        assert_eq!(a.font.family, "Varela Round");
    }

    #[test]
    fn missing_file_falls_back_to_default() {
        let a: Appearance = load_or_default(Path::new("/nonexistent/breadlock-test.toml"));
        assert_eq!(a.font.family, "Varela Round");
    }

    #[test]
    fn parses_partial_toml_with_defaults_for_rest() {
        let path = std::env::temp_dir().join(format!(
            "breadlock-ui-test-partial-{}.toml",
            std::process::id()
        ));
        std::fs::write(&path, "[clock]\nformat = \"%I:%M %p\"\n").unwrap();
        let a: Appearance = load_or_default(&path);
        assert_eq!(a.clock.format, "%I:%M %p");
        assert_eq!(a.background.mode, BackgroundMode::Color);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn invalid_toml_falls_back_to_default() {
        let path = std::env::temp_dir().join(format!(
            "breadlock-ui-test-invalid-{}.toml",
            std::process::id()
        ));
        std::fs::write(&path, "this is not = toml [[[").unwrap();
        let a: Appearance = load_or_default(&path);
        assert_eq!(a.clock.format, "%H:%M");
        assert_eq!(a.font.family, "Varela Round");
        std::fs::remove_file(&path).ok();
    }
}

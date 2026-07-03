pub use bread_theme::{ink_on, load_palette, Palette};

/// Parse a `#rrggbb` hex colour. Falls back to opaque black on malformed input
/// (palette slots are always produced by [`bread_theme`], which guarantees
/// valid hex, but a user-supplied override in a future config field might not).
pub fn parse_hex(hex: &str) -> (u8, u8, u8) {
    let h = hex.trim_start_matches('#');
    let byte = |i: usize| u8::from_str_radix(h.get(i..i + 2).unwrap_or("00"), 16).unwrap_or(0);
    (byte(0), byte(2), byte(4))
}

#[cfg(feature = "paint")]
pub fn tiny_skia_color(hex: &str) -> tiny_skia::Color {
    let (r, g, b) = parse_hex(hex);
    tiny_skia::Color::from_rgba8(r, g, b, 255)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_hex() {
        assert_eq!(parse_hex("#1e1e2e"), (0x1e, 0x1e, 0x2e));
        assert_eq!(parse_hex("89b4fa"), (0x89, 0xb4, 0xfa));
    }

    #[test]
    fn malformed_hex_falls_back_to_black() {
        assert_eq!(parse_hex("nope"), (0, 0, 0));
    }
}

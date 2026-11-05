//! Color parsing — one implementation shared by bindings, layout and renderer.
//!
//! Supported syntaxes (after binding evaluation):
//!   `#rgb` `#rrggbb` `#rrggbbaa`
//!   `rgb(r,g,b)` / `rgba(r,g,b,a)` — `a` is a 0..1 fraction when `<= 1.0`
//!   (so `rgba(0,0,0,1)` is opaque, CSS-style) and a 0..255 byte when `> 1`
//!   (`rgba(0,0,0,128)` ≈ 50%)
//!   `transparent`, `white`, `black`

pub type Color = (u8, u8, u8, u8);

pub const TRANSPARENT: Color = (0, 0, 0, 0);

pub fn parse_color_str(s: &str) -> Option<Color> {
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("transparent") || s.eq_ignore_ascii_case("none") {
        return Some(TRANSPARENT);
    }
    if let Some(hex) = s.strip_prefix('#') {
        let ok = |h: &str| u8::from_str_radix(h, 16).ok();
        return match hex.len() {
            3 => Some((ok(&hex[0..1].repeat(2))?, ok(&hex[1..2].repeat(2))?, ok(&hex[2..3].repeat(2))?, 255)),
            4 => Some((
                ok(&hex[0..1].repeat(2))?,
                ok(&hex[1..2].repeat(2))?,
                ok(&hex[2..3].repeat(2))?,
                ok(&hex[3..4].repeat(2))?,
            )),
            6 => Some((ok(&hex[0..2])?, ok(&hex[2..4])?, ok(&hex[4..6])?, 255)),
            8 => Some((ok(&hex[0..2])?, ok(&hex[2..4])?, ok(&hex[4..6])?, ok(&hex[6..8])?)),
            _ => None,
        };
    }
    let lower = s.to_ascii_lowercase();
    if let Some(inner) = lower.strip_prefix("rgba(").and_then(|i| i.strip_suffix(')')) {
        return parse_channels(inner, true);
    }
    if let Some(inner) = lower.strip_prefix("rgb(").and_then(|i| i.strip_suffix(')')) {
        return parse_channels(inner, false);
    }
    match lower.as_str() {
        "white" => Some((255, 255, 255, 255)),
        "black" => Some((0, 0, 0, 255)),
        "red" => Some((255, 0, 0, 255)),
        "green" => Some((0, 128, 0, 255)),
        "blue" => Some((0, 0, 255, 255)),
        _ => None,
    }
}

fn parse_channels(inner: &str, has_alpha: bool) -> Option<Color> {
    let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();
    if has_alpha && parts.len() != 4 {
        return None;
    }
    if !has_alpha && parts.len() != 3 {
        return None;
    }
    let r: u8 = parts[0].parse().ok()?;
    let g: u8 = parts[1].parse().ok()?;
    let b: u8 = parts[2].parse().ok()?;
    let a = if has_alpha {
        alpha_channel(parts[3])?
    } else {
        255
    };
    Some((r, g, b, a))
}

/// Alpha channel: `<= 1.0` is a CSS-style fraction (1.0 = opaque), values
/// above 1 are a 0..255 byte. The old "contains a dot means fraction" rule
/// made the authoring form `rgba(0,0,0,1)` parse as 1/255 = nearly invisible.
fn alpha_channel(s: &str) -> Option<u8> {
    let value: f32 = s.trim().parse().ok()?;
    if !value.is_finite() {
        return None;
    }
    if value <= 1.0 {
        Some((value.clamp(0.0, 1.0) * 255.0).round() as u8)
    } else {
        Some(value.clamp(0.0, 255.0).round() as u8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_forms() {
        assert_eq!(parse_color_str("#fff"), Some((255, 255, 255, 255)));
        assert_eq!(parse_color_str("#ff2a6d"), Some((255, 42, 109, 255)));
        assert_eq!(parse_color_str("#ff2a6d40"), Some((255, 42, 109, 64)));
    }

    #[test]
    fn rgba_forms() {
        assert_eq!(parse_color_str("rgba(255,42,109,0.5)"), Some((255, 42, 109, 128)));
        assert_eq!(parse_color_str("rgba(255,42,109,128)"), Some((255, 42, 109, 128)));
        assert_eq!(parse_color_str("rgb(1,2,3)"), Some((1, 2, 3, 255)));
    }

    #[test]
    fn alpha_one_is_opaque_not_one_byte() {
        // CSS semantics: 1 = fully opaque. Previously parsed as 1/255.
        assert_eq!(parse_color_str("rgba(0,0,0,1)"), Some((0, 0, 0, 255)));
        assert_eq!(parse_color_str("rgba(0,0,0,1.0)"), Some((0, 0, 0, 255)));
        assert_eq!(parse_color_str("rgba(0,0,0,0)"), Some((0, 0, 0, 0)));
        assert_eq!(parse_color_str("rgba(0,0,0,0.25)"), Some((0, 0, 0, 64)));
        // > 1 is a byte value (the old dual 0..1/0..255 authoring rule).
        assert_eq!(parse_color_str("rgba(0,0,0,2)"), Some((0, 0, 0, 2)));
        assert_eq!(parse_color_str("rgba(0,0,0,255)"), Some((0, 0, 0, 255)));
        assert_eq!(parse_color_str("rgba(0,0,0,256)"), Some((0, 0, 0, 255)));
        assert_eq!(parse_color_str("rgba(0,0,0,NaN)"), None);
    }

    #[test]
    fn named_and_transparent() {
        assert_eq!(parse_color_str("transparent"), Some(TRANSPARENT));
        assert_eq!(parse_color_str("white"), Some((255, 255, 255, 255)));
        assert_eq!(parse_color_str(""), Some(TRANSPARENT));
        assert_eq!(parse_color_str("not-a-color"), None);
    }
}

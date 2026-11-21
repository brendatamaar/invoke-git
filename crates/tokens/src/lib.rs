//! Design tokens for Invoke Git, generated from `docs/design/tokens.json`.
//!
//! The UI reads colours, type, spacing, radii and fixed sizes from here and
//! never hard-codes them (AGENTS.md rule 7). Everything is a plain `const`
//! with no GPUI types, so a GPUI layer converts at its own edge:
//! `Rgba::to_u32` gives `0xRRGGBB`, sizes are `f32` logical pixels.
//!
//! `build.rs` reads the JSON on every build, so the consts cannot drift from
//! it. The crate uses only std and does no IO at run time.

/// A colour with 8-bit channels and a float alpha (`1.0` is opaque).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: f32,
}

impl Rgba {
    pub const fn new(r: u8, g: u8, b: u8, a: f32) -> Self {
        Self { r, g, b, a }
    }

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self::new(r, g, b, 1.0)
    }

    /// `0xRRGGBB`, ignoring alpha.
    pub const fn to_u32(self) -> u32 {
        ((self.r as u32) << 16) | ((self.g as u32) << 8) | self.b as u32
    }

    /// WCAG 2.x relative luminance of the colour, ignoring alpha.
    pub fn relative_luminance(self) -> f64 {
        fn linear(channel: u8) -> f64 {
            let c = f64::from(channel) / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * linear(self.r) + 0.7152 * linear(self.g) + 0.0722 * linear(self.b)
    }

    /// WCAG 2.x contrast ratio between two opaque colours, from 1.0 to 21.0.
    pub fn contrast_ratio(self, other: Rgba) -> f64 {
        let (a, b) = (self.relative_luminance(), other.relative_luminance());
        let (light, dark) = if a >= b { (a, b) } else { (b, a) };
        (light + 0.05) / (dark + 0.05)
    }
}

/// The two themes. Dark is the default and comes first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

impl Theme {
    pub const ALL: [Theme; 2] = [Theme::Dark, Theme::Light];

    pub const fn palette(self) -> &'static Palette {
        match self {
            Theme::Dark => &DARK,
            Theme::Light => &LIGHT,
        }
    }
}

/// One layer of a CSS-style box shadow.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShadowLayer {
    pub offset_x: f32,
    pub offset_y: f32,
    pub blur: f32,
    pub spread: f32,
    pub color: Rgba,
}

/// A box shadow: layers in CSS order (first is on top).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shadow {
    pub layers: &'static [ShadowLayer],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontFamily {
    /// The interface face, see [`SANS_STACK`].
    Sans,
    /// The code face, see [`MONO_STACK`].
    Mono,
}

impl FontFamily {
    /// Family names in preference order.
    pub const fn stack(self) -> &'static [&'static str] {
        match self {
            FontFamily::Sans => &SANS_STACK,
            FontFamily::Mono => &MONO_STACK,
        }
    }
}

/// A named text style. Sizes are logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextStyle {
    pub name: &'static str,
    pub family: FontFamily,
    pub font_size: f32,
    pub line_height: f32,
    pub font_weight: u16,
    /// Letter spacing in em (`0.02` is `0.02em`).
    pub letter_spacing_em: f32,
}

impl Palette {
    /// Graph lane colour for the `index`th opened lane (0-based); lanes
    /// take `lane-1` to `lane-8` in rotation.
    pub const fn lane(&self, index: usize) -> Rgba {
        match index % 8 {
            0 => self.lane_1,
            1 => self.lane_2,
            2 => self.lane_3,
            3 => self.lane_4,
            4 => self.lane_5,
            5 => self.lane_6,
            6 => self.lane_7,
            _ => self.lane_8,
        }
    }
}

include!(concat!(env!("OUT_DIR"), "/generated.rs"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_rotates_through_eight() {
        let p = Theme::Dark.palette();
        assert_eq!(p.lane(0), p.lane_1);
        assert_eq!(p.lane(7), p.lane_8);
        assert_eq!(p.lane(8), p.lane_1);
        assert_eq!(p.lane(19), p.lane_4);
    }

    #[test]
    fn contrast_extremes() {
        let black = Rgba::rgb(0, 0, 0);
        let white = Rgba::rgb(255, 255, 255);
        assert!((black.contrast_ratio(white) - 21.0).abs() < 1e-9);
        assert!((white.contrast_ratio(white) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn to_u32_packs_rrggbb() {
        assert_eq!(Rgba::rgb(0x8b, 0x95, 0xfa).to_u32(), 0x8b95fa);
    }
}

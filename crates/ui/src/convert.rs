//! Pure conversions from the `tokens` crate to GPUI values. No window or
//! app is needed, so these are unit and property tested.

use gpui::{Hsla, Pixels, SharedString, px};

/// A token colour as a GPUI `Rgba` (channels `0.0..=1.0`).
pub fn rgba(color: tokens::Rgba) -> gpui::Rgba {
    gpui::Rgba {
        r: f32::from(color.r) / 255.0,
        g: f32::from(color.g) / 255.0,
        b: f32::from(color.b) / 255.0,
        a: color.a,
    }
}

/// A token colour as a GPUI `Hsla`.
pub fn hsla(color: tokens::Rgba) -> Hsla {
    rgba(color).into()
}

/// A token size (logical pixels) as GPUI pixels.
pub fn pixels(value: f32) -> Pixels {
    px(value)
}

/// The first family of `stack` that is installed, in preference order.
///
/// When none is installed (a machine without Inter and without the Windows
/// fallbacks), the last entry is used: the stacks end in a generic name.
pub fn resolve_family(stack: &[&str], installed: &[String]) -> SharedString {
    let found = stack
        .iter()
        .find(|name| installed.iter().any(|family| family == **name));
    let name = found
        .or_else(|| stack.last())
        .copied()
        .unwrap_or("sans-serif");
    SharedString::from(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn rgba_maps_the_extremes() {
        let white = rgba(tokens::Rgba::rgb(255, 255, 255));
        assert_eq!((white.r, white.g, white.b, white.a), (1.0, 1.0, 1.0, 1.0));
        let black = rgba(tokens::Rgba::new(0, 0, 0, 0.5));
        assert_eq!((black.r, black.g, black.b, black.a), (0.0, 0.0, 0.0, 0.5));
    }

    #[test]
    fn falls_back_to_the_last_entry() {
        let stack = ["Inter", "Segoe UI", "sans-serif"];
        assert_eq!(resolve_family(&stack, &[]).as_ref(), "sans-serif");
        assert_eq!(
            resolve_family(&stack, &["Segoe UI".to_string()]).as_ref(),
            "Segoe UI"
        );
    }

    #[test]
    fn every_token_colour_converts_to_gpui_channels_in_range() {
        for theme in tokens::Theme::ALL {
            for (name, color) in theme.palette().colors() {
                let c = rgba(color);
                for channel in [c.r, c.g, c.b, c.a] {
                    assert!((0.0..=1.0).contains(&channel), "{name}");
                }
            }
        }
    }

    proptest! {
        /// Converting back to 8-bit channels gives the token value: no
        /// channel is shifted or swapped on the way into GPUI.
        #[test]
        fn rgba_round_trips_through_8_bit_channels(r: u8, g: u8, b: u8, a in 0.0f32..=1.0) {
            let c = rgba(tokens::Rgba::new(r, g, b, a));
            prop_assert_eq!((c.r * 255.0).round() as u8, r);
            prop_assert_eq!((c.g * 255.0).round() as u8, g);
            prop_assert_eq!((c.b * 255.0).round() as u8, b);
            prop_assert_eq!(c.a, a);
        }

        /// The chosen family is the first stack entry that is installed.
        #[test]
        fn resolve_family_prefers_the_earliest_installed(mask in 0u8..32) {
            let stack = ["a", "b", "c", "d", "generic"];
            let installed: Vec<String> = stack
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, n)| n.to_string())
                .collect();
            let expected = stack
                .iter()
                .find(|n| installed.iter().any(|i| i == **n))
                .unwrap_or(&"generic");
            let chosen = resolve_family(&stack, &installed);
            prop_assert_eq!(chosen.as_ref(), *expected);
        }
    }
}

use gpui::{App, IntoElement, ParentElement, RenderOnce, Styled, Window, div, px, relative};
use tokens::{RADIUS_SM, SPACE_1};

use crate::convert::hsla;
use crate::theme::ThemeExt;

/// A determinate progress bar. Shown only when git reports a percentage.
#[derive(IntoElement)]
pub struct ProgressBar {
    fraction: f32,
}

impl ProgressBar {
    /// `fraction` is clamped to `0.0..=1.0`; NaN counts as 0.
    pub fn new(fraction: f32) -> Self {
        Self {
            fraction: clamp_fraction(fraction),
        }
    }
}

/// The fraction drawn for any input.
pub fn clamp_fraction(fraction: f32) -> f32 {
    if fraction.is_nan() {
        0.0
    } else {
        fraction.clamp(0.0, 1.0)
    }
}

impl RenderOnce for ProgressBar {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = cx.palette();
        div()
            .w_full()
            .h(px(SPACE_1))
            .rounded(px(RADIUS_SM))
            .bg(hsla(p.bg_raised))
            .child(
                div()
                    .h_full()
                    .w(relative(self.fraction))
                    .rounded(px(RADIUS_SM))
                    .bg(hsla(p.accent)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn fraction_is_always_drawable(value in proptest::num::f32::ANY) {
            let drawn = clamp_fraction(value);
            prop_assert!((0.0..=1.0).contains(&drawn));
        }

        #[test]
        fn in_range_values_are_kept(value in 0.0f32..=1.0) {
            prop_assert_eq!(clamp_fraction(value), value);
        }
    }
}

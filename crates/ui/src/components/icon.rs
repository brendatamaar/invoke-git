//! Minimal 16px outline vector icon set (1.5px stroke, per `docs/design/README.md`).
//!
//! Icons are defined as normalized `[0.0, 1.0]` stroke polylines and drawn with
//! GPUI's `PathBuilder::stroke`, so there are zero external asset dependencies
//! and every icon inherits any theme colour cleanly.

use gpui::{App, IntoElement, PathBuilder, RenderOnce, Styled, Window, canvas, point, px};
use tokens::{Rgba, SPACE_4};

use crate::convert::hsla;
use crate::theme::ThemeExt;

/// Default outline icon size (16px).
pub const ICON_SIZE: f32 = SPACE_4;
/// Compact outline icon size (14px) for dense 20px buttons.
pub const ICON_SIZE_SM: f32 = 14.0;
const STROKE_WIDTH: f32 = 1.5;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IconName {
    Branch,
    Tag,
    Stash,
    Commit,
    Fetch,
    Pull,
    Push,
    Search,
    Undo,
    Plus,
    More,
    Sun,
    Moon,
    History,
    Changes,
    PullRequest,
    Close,
    ChevronDown,
    ChevronRight,
    Worktree,
    Command,
    Trash,
    Refresh,
    FileText,
    PanelTop,
}

impl IconName {
    #[cfg(test)]
    pub const ALL: [IconName; 25] = [
        IconName::Branch,
        IconName::Tag,
        IconName::Stash,
        IconName::Commit,
        IconName::Fetch,
        IconName::Pull,
        IconName::Push,
        IconName::Search,
        IconName::Undo,
        IconName::Plus,
        IconName::More,
        IconName::Sun,
        IconName::Moon,
        IconName::History,
        IconName::Changes,
        IconName::PullRequest,
        IconName::Close,
        IconName::ChevronDown,
        IconName::ChevronRight,
        IconName::Worktree,
        IconName::Command,
        IconName::Trash,
        IconName::Refresh,
        IconName::FileText,
        IconName::PanelTop,
    ];
}

/// Normalized `[0.0, 1.0]` stroke polylines for `name`.
pub fn icon_polylines(name: IconName) -> &'static [&'static [(f32, f32)]] {
    match name {
        IconName::Branch => &[
            &[(0.3, 0.18), (0.3, 0.82)],
            &[(0.7, 0.3), (0.7, 0.48), (0.58, 0.62), (0.3, 0.62)],
            &[
                (0.22, 0.18),
                (0.3, 0.10),
                (0.38, 0.18),
                (0.3, 0.26),
                (0.22, 0.18),
            ],
            &[
                (0.62, 0.22),
                (0.7, 0.14),
                (0.78, 0.22),
                (0.7, 0.30),
                (0.62, 0.22),
            ],
            &[
                (0.22, 0.82),
                (0.3, 0.74),
                (0.38, 0.82),
                (0.3, 0.90),
                (0.22, 0.82),
            ],
        ],
        IconName::Tag => &[
            &[
                (0.16, 0.16),
                (0.50, 0.16),
                (0.86, 0.52),
                (0.52, 0.86),
                (0.16, 0.50),
                (0.16, 0.16),
            ],
            &[(0.34, 0.34), (0.38, 0.38)],
        ],
        IconName::Stash => &[
            &[(0.16, 0.42), (0.16, 0.82), (0.84, 0.82), (0.84, 0.42)],
            &[
                (0.12, 0.22),
                (0.88, 0.22),
                (0.88, 0.42),
                (0.12, 0.42),
                (0.12, 0.22),
            ],
            &[(0.40, 0.58), (0.60, 0.58)],
        ],
        IconName::Commit => &[
            &[(0.12, 0.5), (0.34, 0.5)],
            &[(0.66, 0.5), (0.88, 0.5)],
            &[
                (0.34, 0.5),
                (0.42, 0.34),
                (0.58, 0.34),
                (0.66, 0.5),
                (0.58, 0.66),
                (0.42, 0.66),
                (0.34, 0.5),
            ],
        ],
        IconName::Fetch => &[
            &[(0.50, 0.16), (0.50, 0.68)],
            &[(0.28, 0.46), (0.50, 0.68), (0.72, 0.46)],
            &[(0.20, 0.84), (0.80, 0.84)],
        ],
        IconName::Pull => &[
            &[(0.50, 0.16), (0.50, 0.80)],
            &[(0.26, 0.56), (0.50, 0.80), (0.74, 0.56)],
        ],
        IconName::Push => &[
            &[(0.50, 0.84), (0.50, 0.20)],
            &[(0.26, 0.44), (0.50, 0.20), (0.74, 0.44)],
        ],
        IconName::Search => &[
            &[
                (0.44, 0.18),
                (0.62, 0.24),
                (0.68, 0.44),
                (0.62, 0.62),
                (0.44, 0.68),
                (0.24, 0.62),
                (0.18, 0.44),
                (0.24, 0.24),
                (0.44, 0.18),
            ],
            &[(0.62, 0.62), (0.86, 0.86)],
        ],
        IconName::Undo => &[
            &[
                (0.20, 0.38),
                (0.64, 0.38),
                (0.80, 0.52),
                (0.80, 0.68),
                (0.64, 0.82),
                (0.36, 0.82),
            ],
            &[(0.38, 0.20), (0.20, 0.38), (0.38, 0.56)],
        ],
        IconName::Plus => &[&[(0.50, 0.20), (0.50, 0.80)], &[(0.20, 0.50), (0.80, 0.50)]],
        IconName::More => &[
            &[(0.22, 0.48), (0.26, 0.52)],
            &[(0.48, 0.48), (0.52, 0.52)],
            &[(0.74, 0.48), (0.78, 0.52)],
        ],
        IconName::Sun => &[
            &[
                (0.50, 0.32),
                (0.64, 0.38),
                (0.68, 0.50),
                (0.64, 0.62),
                (0.50, 0.68),
                (0.36, 0.62),
                (0.32, 0.50),
                (0.36, 0.38),
                (0.50, 0.32),
            ],
            &[(0.50, 0.12), (0.50, 0.22)],
            &[(0.50, 0.78), (0.50, 0.88)],
            &[(0.12, 0.50), (0.22, 0.50)],
            &[(0.78, 0.50), (0.88, 0.50)],
            &[(0.22, 0.22), (0.29, 0.29)],
            &[(0.71, 0.71), (0.78, 0.78)],
            &[(0.78, 0.22), (0.71, 0.29)],
            &[(0.29, 0.71), (0.22, 0.78)],
        ],
        IconName::Moon => &[&[
            (0.64, 0.18),
            (0.46, 0.24),
            (0.40, 0.42),
            (0.46, 0.62),
            (0.64, 0.72),
            (0.82, 0.68),
            (0.72, 0.82),
            (0.50, 0.86),
            (0.28, 0.78),
            (0.18, 0.56),
            (0.22, 0.32),
            (0.40, 0.18),
            (0.64, 0.18),
        ]],
        IconName::History => &[
            &[
                (0.22, 0.38),
                (0.34, 0.20),
                (0.56, 0.16),
                (0.76, 0.28),
                (0.84, 0.50),
                (0.76, 0.72),
                (0.54, 0.84),
                (0.30, 0.78),
            ],
            &[(0.18, 0.22), (0.22, 0.38), (0.38, 0.36)],
            &[(0.52, 0.34), (0.52, 0.52), (0.66, 0.60)],
        ],
        IconName::Changes => &[
            &[
                (0.22, 0.16),
                (0.62, 0.16),
                (0.78, 0.32),
                (0.78, 0.84),
                (0.22, 0.84),
                (0.22, 0.16),
            ],
            &[(0.38, 0.46), (0.62, 0.46)],
            &[(0.50, 0.34), (0.50, 0.58)],
            &[(0.38, 0.70), (0.62, 0.70)],
        ],
        IconName::PullRequest => &[
            &[(0.28, 0.28), (0.28, 0.76)],
            &[(0.72, 0.72), (0.72, 0.40), (0.58, 0.28), (0.48, 0.28)],
            &[(0.56, 0.18), (0.48, 0.28), (0.56, 0.38)],
            &[
                (0.20, 0.22),
                (0.28, 0.14),
                (0.36, 0.22),
                (0.28, 0.30),
                (0.20, 0.22),
            ],
            &[
                (0.20, 0.78),
                (0.28, 0.70),
                (0.36, 0.78),
                (0.28, 0.86),
                (0.20, 0.78),
            ],
            &[
                (0.64, 0.78),
                (0.72, 0.70),
                (0.80, 0.78),
                (0.72, 0.86),
                (0.64, 0.78),
            ],
        ],
        IconName::Close => &[&[(0.24, 0.24), (0.76, 0.76)], &[(0.76, 0.24), (0.24, 0.76)]],
        IconName::ChevronDown => &[&[(0.24, 0.38), (0.50, 0.64), (0.76, 0.38)]],
        IconName::ChevronRight => &[&[(0.38, 0.24), (0.64, 0.50), (0.38, 0.76)]],
        IconName::Worktree => &[&[
            (0.16, 0.28),
            (0.42, 0.28),
            (0.50, 0.38),
            (0.84, 0.38),
            (0.84, 0.78),
            (0.16, 0.78),
            (0.16, 0.28),
        ]],
        IconName::Command => &[
            &[
                (0.34, 0.34),
                (0.66, 0.34),
                (0.66, 0.66),
                (0.34, 0.66),
                (0.34, 0.34),
            ],
            &[
                (0.34, 0.34),
                (0.20, 0.34),
                (0.20, 0.20),
                (0.34, 0.20),
                (0.34, 0.34),
            ],
            &[
                (0.66, 0.34),
                (0.80, 0.34),
                (0.80, 0.20),
                (0.66, 0.20),
                (0.66, 0.34),
            ],
            &[
                (0.34, 0.66),
                (0.20, 0.66),
                (0.20, 0.80),
                (0.34, 0.80),
                (0.34, 0.66),
            ],
            &[
                (0.66, 0.66),
                (0.80, 0.66),
                (0.80, 0.80),
                (0.66, 0.80),
                (0.66, 0.66),
            ],
        ],
        IconName::Trash => &[
            &[(0.20, 0.28), (0.80, 0.28)],
            &[(0.38, 0.28), (0.38, 0.16), (0.62, 0.16), (0.62, 0.28)],
            &[(0.28, 0.28), (0.32, 0.84), (0.68, 0.84), (0.72, 0.28)],
            &[(0.44, 0.44), (0.44, 0.70)],
            &[(0.56, 0.44), (0.56, 0.70)],
        ],
        IconName::Refresh => &[
            &[
                (0.78, 0.38),
                (0.68, 0.20),
                (0.46, 0.18),
                (0.26, 0.28),
                (0.20, 0.50),
                (0.26, 0.72),
                (0.48, 0.82),
                (0.70, 0.74),
            ],
            &[(0.82, 0.20), (0.78, 0.38), (0.60, 0.36)],
        ],
        IconName::FileText => &[
            &[
                (0.22, 0.16),
                (0.60, 0.16),
                (0.78, 0.34),
                (0.78, 0.84),
                (0.22, 0.84),
                (0.22, 0.16),
            ],
            &[(0.36, 0.48), (0.64, 0.48)],
            &[(0.36, 0.64), (0.64, 0.64)],
        ],
        IconName::PanelTop => &[
            &[
                (0.16, 0.20),
                (0.84, 0.20),
                (0.84, 0.80),
                (0.16, 0.80),
                (0.16, 0.20),
            ],
            &[(0.16, 0.44), (0.84, 0.44)],
        ],
    }
}

#[derive(IntoElement)]
pub struct Icon {
    name: IconName,
    size: f32,
    color: Option<Rgba>,
}

impl Icon {
    pub fn new(name: IconName) -> Self {
        Self {
            name,
            size: ICON_SIZE,
            color: None,
        }
    }

    pub fn size(mut self, size: f32) -> Self {
        self.size = size;
        self
    }

    pub fn small(self) -> Self {
        self.size(ICON_SIZE_SM)
    }

    pub fn color(mut self, color: Rgba) -> Self {
        self.color = Some(color);
        self
    }
}

impl RenderOnce for Icon {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let color = hsla(self.color.unwrap_or_else(|| cx.palette().ink_muted));
        let polylines = icon_polylines(self.name);
        canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                let w = f32::from(bounds.size.width);
                let h = f32::from(bounds.size.height);
                for pts in polylines {
                    if pts.len() < 2 {
                        continue;
                    }
                    let mut path = PathBuilder::stroke(px(STROKE_WIDTH));
                    path.move_to(point(
                        bounds.origin.x + px(pts[0].0 * w),
                        bounds.origin.y + px(pts[0].1 * h),
                    ));
                    for &(nx, ny) in &pts[1..] {
                        path.line_to(point(
                            bounds.origin.x + px(nx * w),
                            bounds.origin.y + px(ny * h),
                        ));
                    }
                    if let Ok(built) = path.build() {
                        window.paint_path(built, color);
                    }
                }
            },
        )
        .flex_none()
        .size(px(self.size))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_has_valid_normalized_strokes() {
        for name in IconName::ALL {
            let strokes = icon_polylines(name);
            assert!(!strokes.is_empty(), "{name:?} has no strokes");
            for stroke in strokes {
                assert!(stroke.len() >= 2, "{name:?} has a degenerate stroke");
                for &(x, y) in *stroke {
                    assert!(
                        (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y),
                        "{name:?} point ({x}, {y}) out of [0, 1]"
                    );
                }
            }
        }
    }
}

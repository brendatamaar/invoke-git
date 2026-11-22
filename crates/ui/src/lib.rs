//! The GPUI component layer (backlog task 2).
//!
//! A thin layer over GPUI: it reads every colour, size and spacing from the
//! `tokens` crate and never hard-codes them (AGENTS.md rule 7). Components
//! only render and report clicks; they hold no application state.
//!
//! Call [`init`] once at start-up. After that, `cx.palette()` gives the
//! active colours and `.text_style(&tokens::TEXT_BODY, cx)` sets a token
//! text style on any element.

pub mod components;
pub mod convert;
pub mod focus;
pub mod fonts;
pub mod summary;
pub mod tabs;
pub mod text;
pub mod theme;
pub mod welcome;

pub use components::{
    Banner, BannerKind, Button, ButtonKind, CheckState, Checkbox, Divider, Pill, ProgressBar,
    SectionLabel,
};
pub use summary::{RepoSummary, Summary};
pub use tabs::{TabBar, TabItem};
pub use text::TextStyleExt;
pub use theme::{ThemeExt, UiTheme, init, set_theme};
pub use welcome::Welcome;

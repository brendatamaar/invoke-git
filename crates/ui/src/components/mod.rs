//! Small, stateless components. Each reads the palette from `cx`.

mod banner;
mod button;
mod checkbox;
mod divider;
mod icon;
mod nav_tabs;
mod pill;
mod progress;
mod section_label;
mod segmented;

pub use banner::{Banner, BannerKind};
pub use button::{Button, ButtonKind, ButtonSize};
pub use checkbox::{CheckState, Checkbox};
pub use divider::Divider;
pub use icon::{ICON_SIZE, ICON_SIZE_SM, Icon, IconName};
pub use nav_tabs::{NavTab, NavTabs};
pub use pill::{Pill, PillTone};
pub use progress::{ProgressBar, clamp_fraction};
pub use section_label::SectionLabel;
pub use segmented::Segmented;

pub(crate) use button::text_tooltip;

//! Small, stateless components. Each reads the palette from `cx`.

mod banner;
mod button;
mod checkbox;
mod divider;
mod pill;
mod progress;
mod section_label;

pub use banner::{Banner, BannerKind};
pub use button::{Button, ButtonKind};
pub use checkbox::{CheckState, Checkbox};
pub use divider::Divider;
pub use pill::Pill;
pub use progress::{ProgressBar, clamp_fraction};
pub use section_label::SectionLabel;

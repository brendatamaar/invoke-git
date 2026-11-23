//! The GPUI component layer (backlog task 2).
//!
//! A thin layer over GPUI: it reads every colour, size and spacing from the
//! `tokens` crate and never hard-codes them (AGENTS.md rule 7). Components
//! only render and report clicks; they hold no application state.
//!
//! Call [`init`] once at start-up. After that, `cx.palette()` gives the
//! active colours and `.text_style(&tokens::TEXT_BODY, cx)` sets a token
//! text style on any element.

pub mod blame_view;
pub mod changes;
pub mod code_text;
pub mod commit_detail;
pub mod components;
pub mod convert;
pub mod diff_rows;
pub mod diff_view;
pub mod editor_model;
pub mod focus;
pub mod fonts;
pub mod graph_view;
pub mod ref_labels;
pub mod reflog_view;
pub mod scrollbar;
pub mod sidebar;
pub mod status_tree;
pub mod summary;
pub mod tabs;
pub mod text;
pub mod text_editor;
pub mod theme;
pub mod time;
pub mod welcome;

pub use changes::{ChangeList, ListRow, Section, list_rows};
pub use commit_detail::{CommitDetail, CommitFile, CommitInfo, short_hash, split_message};
pub use components::{
    Banner, BannerKind, Button, ButtonKind, CheckState, Checkbox, Divider, Pill, ProgressBar,
    SectionLabel, Segmented,
};
pub use diff_view::{
    Around, Content as DiffContent, DiffData, DiffEvent, DiffView, InputsMeta, StageMode,
};
pub use graph_view::{GraphEvent, GraphView, RowMeta};
pub use sidebar::{SideAction, SideRow, Sidebar, sidebar_rows};
pub use status_tree::{StatusTree, TreeAction, TreeRow, tree_rows};
pub use summary::{RepoSummary, Summary};
pub use tabs::{TabBar, TabItem};
pub use text::TextStyleExt;
pub use text_editor::{EditorEvent, TextEditor, bind_keys as bind_editor_keys};
pub use theme::{ThemeExt, UiTheme, init, set_theme};
pub use welcome::Welcome;

//! The active theme as a GPUI global, and the fonts it resolved at start-up.

use gpui::{App, Global, SharedString};
use tokens::{FontFamily, Palette, Theme};

use crate::convert::resolve_family;

/// The theme and the installed font families chosen for it.
#[derive(Clone)]
pub struct UiTheme {
    theme: Theme,
    sans: SharedString,
    mono: SharedString,
}

impl Global for UiTheme {}

impl UiTheme {
    pub fn theme(&self) -> Theme {
        self.theme
    }

    pub fn palette(&self) -> &'static Palette {
        self.theme.palette()
    }

    /// The installed family for a token font family.
    pub fn family(&self, family: FontFamily) -> SharedString {
        match family {
            FontFamily::Sans => self.sans.clone(),
            FontFamily::Mono => self.mono.clone(),
        }
    }
}

/// Registers the theme global (dark, the default) and resolves fonts. Call
/// once at start-up, before the first window renders.
pub fn init(cx: &mut App) {
    let installed = cx.text_system().all_font_names();
    cx.set_global(UiTheme {
        theme: Theme::default(),
        sans: resolve_family(FontFamily::Sans.stack(), &installed),
        mono: resolve_family(FontFamily::Mono.stack(), &installed),
    });
}

/// Switches theme and repaints every window.
pub fn set_theme(theme: Theme, cx: &mut App) {
    cx.global_mut::<UiTheme>().theme = theme;
    cx.refresh_windows();
}

/// `cx.ui_theme()` and `cx.palette()` in any render function.
pub trait ThemeExt {
    fn ui_theme(&self) -> &UiTheme;

    fn palette(&self) -> &'static Palette {
        self.ui_theme().palette()
    }
}

impl ThemeExt for App {
    fn ui_theme(&self) -> &UiTheme {
        self.global::<UiTheme>()
    }
}

//! Invoke Git: the binary. It wires the `ui`, `core` and `shell` crates.

#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

mod auth_prompt;
mod branch_panel;
mod clone_view;
mod commit_box;
mod commit_pane;
mod diff_load;
mod discard;
mod history_view;
mod notice;
mod paths;
mod persist;
mod repo_view;
mod session;
mod setup;
mod staging;
mod sync;
mod sync_view;
mod tabs;
mod workspace;

use gpui::{
    App, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, prelude::*, px, size,
};

use workspace::{
    CloseTab, InitRepository, NewTab, NextTab, OpenRepository, PreviousTab, Refresh, Workspace,
};

fn main() {
    gpui_platform::application().run(|cx: &mut App| {
        ui::init(cx);
        ui::bind_editor_keys(cx);
        // Reads honour the user's git config like git does, so Git for
        // Windows' `core.autocrlf=true` gives the same status and diffs.
        shell::use_user_config(true);
        cx.bind_keys([
            KeyBinding::new("ctrl-o", OpenRepository, None),
            KeyBinding::new("ctrl-shift-o", InitRepository, None),
            KeyBinding::new("ctrl-t", NewTab, None),
            KeyBinding::new("ctrl-w", CloseTab, None),
            KeyBinding::new("f5", Refresh, None),
            KeyBinding::new("ctrl-tab", NextTab, None),
            KeyBinding::new("ctrl-shift-tab", PreviousTab, None),
        ]);
        let bounds = Bounds::centered(None, size(px(1400.), px(900.)), cx);
        let restore = persist::load();
        // `invoke-git <folder>...` opens those folders too.
        let open: Vec<std::path::PathBuf> = std::env::args_os().skip(1).map(Into::into).collect();
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Invoke Git".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                let workspace = cx.new(|cx| Workspace::new(restore, open, cx));
                Workspace::register_actions(&workspace, cx);
                workspace.read(cx).focus.clone().focus(window, cx);
                workspace
            },
        )
        .expect("open the Invoke Git window");
        cx.activate(true);
    });
}

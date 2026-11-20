//! Phase 0 spike, Prototype A (diff view). Throwaway: nothing here ships.
//!
//! Usage: `spike-diff [--sweep SECONDS] [OLD NEW]` or `spike-diff --commit-box`.
//! Without paths it opens the fixtures written by `cargo xtask fixtures`.
//! `--sweep` scrolls top to bottom at constant speed once highlighting is
//! ready, prints the stats and quits. `--commit-box` opens the text-input
//! check window instead of the diff.

mod commit_box;
mod highlight;
mod model;
mod stats;
mod view;

use std::path::PathBuf;

use gpui::{App, Bounds, TitlebarOptions, WindowBounds, WindowOptions, prelude::*, px, size};

use view::Sweep;

enum Mode {
    Diff {
        old: PathBuf,
        new: PathBuf,
        sweep: Sweep,
    },
    CommitBox,
}

fn parse_args() -> Result<Mode, String> {
    let usage = "usage: spike-diff [--sweep SECONDS] [OLD NEW] | spike-diff --commit-box";
    let mut sweep = Sweep::Off;
    let mut paths = Vec::new();
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--commit-box" {
            return Ok(Mode::CommitBox);
        } else if arg == "--sweep" {
            let seconds = args
                .next()
                .and_then(|s| s.to_str()?.parse::<f32>().ok())
                .filter(|s| *s > 0.0)
                .ok_or("--sweep needs a number of seconds")?;
            sweep = Sweep::Waiting { seconds };
        } else {
            paths.push(PathBuf::from(arg));
        }
    }
    match <[PathBuf; 2]>::try_from(paths) {
        Ok([old, new]) => Ok(Mode::Diff { old, new, sweep }),
        Err(paths) if paths.is_empty() => {
            let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/diff");
            Ok(Mode::Diff {
                old: fixtures.join("old.rs"),
                new: fixtures.join("new.rs"),
                sweep,
            })
        }
        Err(_) => Err(usage.into()),
    }
}

fn window_options(title: &'static str, bounds: Bounds<gpui::Pixels>) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some(title.into()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn main() {
    let mode = match parse_args() {
        Ok(mode) => mode,
        Err(error) => {
            eprintln!("spike-diff: {error}");
            std::process::exit(2);
        }
    };
    gpui_platform::application().run(move |cx: &mut App| {
        match mode {
            Mode::Diff { old, new, sweep } => {
                let bounds = Bounds::centered(None, size(px(1400.), px(900.)), cx);
                cx.open_window(window_options("spike-diff", bounds), |_, cx| {
                    cx.new(|cx| view::DiffView::new(old, new, sweep, cx))
                })
                .expect("open the spike-diff window");
            }
            Mode::CommitBox => {
                commit_box::bind_keys(cx);
                let bounds = Bounds::centered(None, size(px(720.), px(200.)), cx);
                cx.open_window(
                    window_options("spike-diff commit box", bounds),
                    |window, cx| cx.new(|cx| commit_box::CommitBox::new(window, cx)),
                )
                .expect("open the commit box window");
            }
        }
        cx.activate(true);
    });
}

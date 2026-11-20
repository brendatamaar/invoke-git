//! Phase 0 spike, Prototype A (diff view). Throwaway: nothing here ships.
//!
//! Usage: `spike-diff [--sweep SECONDS] [OLD NEW]`. Without paths it opens the
//! fixtures written by `cargo xtask fixtures`. `--sweep` scrolls top to bottom
//! at constant speed once highlighting is ready, prints the stats and quits.

mod highlight;
mod model;
mod stats;
mod view;

use std::path::PathBuf;

use gpui::{App, Bounds, TitlebarOptions, WindowBounds, WindowOptions, prelude::*, px, size};

use view::Sweep;

fn parse_args() -> Result<(PathBuf, PathBuf, Sweep), String> {
    let mut sweep = Sweep::Off;
    let mut paths = Vec::new();
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--sweep" {
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
        Ok([old, new]) => Ok((old, new, sweep)),
        Err(paths) if paths.is_empty() => {
            let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/diff");
            Ok((fixtures.join("old.rs"), fixtures.join("new.rs"), sweep))
        }
        Err(_) => Err("usage: spike-diff [--sweep SECONDS] [OLD NEW]".into()),
    }
}

fn main() {
    let (old, new, sweep) = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("spike-diff: {error}");
            std::process::exit(2);
        }
    };
    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1400.), px(900.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("spike-diff".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| cx.new(|cx| view::DiffView::new(old, new, sweep, cx)),
        )
        .expect("open the spike-diff window");
        cx.activate(true);
    });
}

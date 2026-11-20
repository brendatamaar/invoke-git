//! Phase 0 spike, Prototype B (commit graph). Throwaway: nothing here ships.
//!
//! Usage: `spike-graph [--sweep SECONDS] [--limit N] [REPO]`. REPO defaults to
//! the kernel fixture recorded in docs/spike-results.md. `--sweep` scrolls
//! from the newest commit to 100k deep, prints the stats and quits.

mod layout;
mod meta;
mod stats;
mod view;
mod walk;

use std::path::PathBuf;

use gpui::{App, Bounds, TitlebarOptions, WindowBounds, WindowOptions, prelude::*, px, size};

use view::Sweep;

/// The read-only kernel clone from P0-1.
const DEFAULT_REPO: &str = r"C:\dev\fixtures\linux";
/// Enough for the 100k-deep gate with room to spare.
const DEFAULT_LIMIT: usize = 200_000;

struct Args {
    repo: PathBuf,
    limit: usize,
    sweep: Sweep,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        repo: PathBuf::from(DEFAULT_REPO),
        limit: DEFAULT_LIMIT,
        sweep: Sweep::Off,
    };
    let mut it = std::env::args_os().skip(1);
    let usage = "usage: spike-graph [--sweep SECONDS] [--limit N] [REPO]";
    while let Some(arg) = it.next() {
        let mut number = |name: &str| {
            it.next()
                .and_then(|s| s.to_str()?.parse::<f64>().ok())
                .filter(|n| *n > 0.0)
                .ok_or(format!("{name} needs a positive number; {usage}"))
        };
        if arg == "--sweep" {
            args.sweep = Sweep::Waiting {
                seconds: number("--sweep")? as f32,
            };
        } else if arg == "--limit" {
            args.limit = number("--limit")? as usize;
        } else {
            args.repo = PathBuf::from(arg);
        }
    }
    Ok(args)
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("spike-graph: {error}");
            std::process::exit(2);
        }
    };
    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1400.), px(900.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("spike-graph".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| cx.new(|_| view::GraphView::new(args.repo, args.limit, args.sweep)),
        )
        .expect("open the spike-graph window");
        cx.activate(true);
    });
}

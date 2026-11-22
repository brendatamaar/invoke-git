//! The diff viewer on sample text, for checking by eye on Windows:
//! `cargo run --release -p ui --example diff`. With `--fixture` it shows the
//! 50k-line files from `cargo xtask fixtures` instead (drag the scrollbar
//! from top to bottom: no row may flash plain, and nothing may stutter).

use gpui::{App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};
use ui::{DiffContent, DiffView, diff_view};

const OLD: &str = "use std::fmt;\n\nfn main() {\n\tlet name = \"world\";\n\tprintln!(\"hello {}\", name);\n\tlet total = 1 + 2;\n\t// a comment\n\tdone(total)\n}\n\nfn done(n: i32) -> i32 {\n    n * 2\n}\n";
const NEW: &str = "use std::fmt;\n\nfn main() {\n\tlet name = \"Invoke Git\";\n\tprintln!(\"hello, {}!\", name);\n\tlet total = 1 + 2 + 3;\n\t// a comment\n\tlet extra = vec![1, 2, 3];\n\tdone(total)\n}\n\nfn done(n: i64) -> i64 {\n    n * 2\n}\nfn last() {}";

fn main() {
    let fixture = std::env::args().any(|arg| arg == "--fixture");
    gpui_platform::application().run(move |cx: &mut App| {
        ui::init(cx);
        let bounds = Bounds::centered(None, size(px(1300.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Invoke Git diff viewer".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |_, cx| {
                let view = cx.new(|_| DiffView::new());
                let handle = view.clone();
                cx.spawn(async move |cx| {
                    let (path, old, new) = if fixture {
                        (
                            "fixtures/diff/new.rs",
                            std::fs::read_to_string("fixtures/diff/old.rs").unwrap_or_default(),
                            std::fs::read_to_string("fixtures/diff/new.rs").unwrap_or_default(),
                        )
                    } else {
                        ("src/main.rs", OLD.to_owned(), NEW.to_owned())
                    };
                    let data = cx
                        .background_spawn(async move { diff_view::compute(path, &old, &new) })
                        .await;
                    let diff = data.diff.clone();
                    handle.update(cx, |view, cx| {
                        view.set_content(DiffContent::Text(Box::new(data)), cx)
                    });
                    let (old, new) = cx
                        .background_spawn(async move {
                            diff_view::highlight_sides(path, &diff.old, &diff.new)
                        })
                        .await;
                    handle.update(cx, |view, cx| view.set_spans(old, new, cx));
                })
                .detach();
                view
            },
        )
        .expect("open the diff window");
        cx.activate(true);
    });
}

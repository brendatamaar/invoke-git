//! Phase 0 spike, Prototype B (commit graph). Throwaway: nothing here ships.
//! For now it only opens an empty window; the graph comes in a later task.

use gpui::{
    App, Bounds, Context, SharedString, TitlebarOptions, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, rgb, size,
};

/// `bg-app` and `ink-faint` (dark) from docs/design/tokens.json. Spike only:
/// there is no tokens crate yet.
const BG_APP: u32 = 0x0e0f11;
const INK_FAINT: u32 = 0x8a8f98;

struct Empty {
    label: SharedString,
}

impl Render for Empty {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .justify_center()
            .items_center()
            .bg(rgb(BG_APP))
            .text_color(rgb(INK_FAINT))
            .child(self.label.clone())
    }
}

fn main() {
    gpui_platform::application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("spike-graph".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|_| Empty {
                    label: "spike-graph".into(),
                })
            },
        )
        .expect("open the spike-graph window");
        cx.activate(true);
    });
}

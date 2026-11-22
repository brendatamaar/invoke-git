//! Every component in both themes, for checking by eye on Windows:
//! `cargo run -p ui --example gallery`.

use gpui::{
    App, Bounds, Context, InteractiveElement, IntoElement, ParentElement, Render, Styled,
    TitlebarOptions, Window, WindowBounds, WindowOptions, div, prelude::*, px, size,
};
use tokens::{
    SPACE_2, SPACE_3, SPACE_4, SPACE_6, TEXT_BODY, TEXT_CODE, TEXT_DISPLAY, TEXT_STYLES,
    TEXT_TITLE, Theme,
};
use ui::convert::hsla;
use ui::{
    Banner, BannerKind, Button, CheckState, Checkbox, Divider, Pill, ProgressBar, SectionLabel,
    TextStyleExt, ThemeExt,
};

struct Gallery {
    checks: [CheckState; 3],
}

fn next(state: CheckState) -> CheckState {
    match state {
        CheckState::Unchecked => CheckState::Checked,
        CheckState::Checked => CheckState::Mixed,
        CheckState::Mixed => CheckState::Unchecked,
    }
}

fn section(title: &str, body: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(SPACE_2))
        .child(SectionLabel::new(title))
        .child(body)
}

impl Render for Gallery {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let theme = cx.ui_theme().theme();
        let toggle_label = match theme {
            Theme::Dark => "Switch to light",
            Theme::Light => "Switch to dark",
        };

        let buttons = div()
            .flex()
            .gap(px(SPACE_2))
            .child(
                Button::new("toggle", toggle_label)
                    .primary()
                    .on_click(move |_, _, cx| {
                        let other = match theme {
                            Theme::Dark => Theme::Light,
                            Theme::Light => Theme::Dark,
                        };
                        ui::set_theme(other, cx);
                    }),
            )
            .child(Button::new("secondary", "Fetch"))
            .child(Button::new("danger", "Discard lines").danger())
            .child(Button::new("ghost", "Copy hash").ghost())
            .child(Button::new("disabled", "Push").disabled(true));

        let checks = div()
            .flex()
            .items_center()
            .gap(px(SPACE_3))
            .children((0..3).map(|i| {
                let state = self.checks[i];
                Checkbox::new(("check", i), state).on_click(cx.listener(move |this, _, _, cx| {
                    this.checks[i] = next(this.checks[i]);
                    cx.notify();
                }))
            }));

        let pills = div()
            .flex()
            .items_center()
            .gap(px(SPACE_2))
            .child(Pill::new("main"))
            .child(Pill::new("feat/line-staging"));

        let banners = div()
            .flex()
            .flex_col()
            .gap(px(SPACE_2))
            .child(
                Banner::new(BannerKind::Warning, "Rebasing 3/7: 2 files have conflicts.")
                    .action(Button::new("abort", "Abort rebase").ghost())
                    .action(Button::new("continue", "Continue").primary()),
            )
            .child(Banner::new(
                BannerKind::Error,
                "Push rejected: the remote has 2 new commits. Fetch and rebase to continue.",
            ));

        let progress = div()
            .flex()
            .flex_col()
            .gap(px(SPACE_2))
            .w(px(320.))
            .child(ProgressBar::new(0.0))
            .child(ProgressBar::new(0.42))
            .child(ProgressBar::new(1.0));

        let types = div()
            .flex()
            .flex_col()
            .children(TEXT_STYLES.iter().map(|style| {
                div()
                    .text_style(style, cx)
                    .text_color(hsla(p.ink))
                    .child(format!("{} — Stage the selected lines", style.name))
            }));

        let swatches = div()
            .flex()
            .flex_wrap()
            .gap(px(SPACE_2))
            .w(px(960.))
            .children(p.colors().into_iter().map(|(name, color)| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .w(px(128.))
                    .child(
                        div()
                            .h(px(SPACE_6))
                            .rounded(px(tokens::RADIUS_SM))
                            .border_1()
                            .border_color(hsla(p.line))
                            .bg(hsla(color)),
                    )
                    .child(
                        div()
                            .text_style(&TEXT_CODE, cx)
                            .text_color(hsla(p.ink_muted))
                            .child(name),
                    )
            }));

        div()
            .id("gallery")
            .size_full()
            .overflow_y_scroll()
            .bg(hsla(p.bg_app))
            .text_color(hsla(p.ink))
            .text_style(&TEXT_BODY, cx)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(SPACE_4))
                    .p(px(SPACE_6))
                    .child(
                        div()
                            .text_style(&TEXT_DISPLAY, cx)
                            .child("Invoke Git components"),
                    )
                    .child(div().text_style(&TEXT_TITLE, cx).child(format!(
                        "{} theme · Tab moves focus, Enter or Space activates",
                        match theme {
                            Theme::Dark => "Dark",
                            Theme::Light => "Light",
                        }
                    )))
                    .child(Divider::horizontal())
                    .child(section("Buttons", buttons))
                    .child(section("Checkboxes (click to cycle)", checks))
                    .child(section("Pills", pills))
                    .child(section("Banners", banners))
                    .child(section("Progress", progress))
                    .child(section("Text styles", types))
                    .child(section("Colours", swatches)),
            )
    }
}

fn main() {
    gpui_platform::application().run(|cx: &mut App| {
        ui::init(cx);
        let bounds = Bounds::centered(None, size(px(1100.), px(900.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Invoke Git components".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|_| Gallery {
                    checks: [
                        CheckState::Unchecked,
                        CheckState::Checked,
                        CheckState::Mixed,
                    ],
                })
            },
        )
        .expect("open the gallery window");
        cx.activate(true);
    });
}

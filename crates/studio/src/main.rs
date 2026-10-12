//! OmniGraph Studio. The composition root: every crate is initialised here, in layer order
//! (`docs/architecture.md`).
//!
//! Spike S4 (`specs/001-foundation/plan.md`): one window with Zed's `ui` and a live Zed
//! `Editor` running without a project. The frame from spec 002 replaces it.

use editor::Editor;
use gpui::{App, Bounds, Context, Entity, Focusable, Window, WindowBounds, WindowOptions, size};
use studio_actions::workspace::Quit;
use ui::prelude::*;

struct Placeholder {
    url: Entity<Editor>,
}

impl Render for Placeholder {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .p_6()
            .gap_3()
            .bg(cx.theme().colors().background)
            .child(Headline::new("OmniGraph Studio"))
            .child(Label::new("Not connected to a server yet.").color(Color::Muted))
            .child(
                div()
                    .w(px(480.))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .child(self.url.clone()),
            )
    }
}

fn main() {
    gpui_platform::application()
        .with_assets(studio_theme::Assets)
        .run(|cx: &mut App| {
            studio_theme::init(cx);
            editor::init(cx);
            cx.on_action(|_: &Quit, cx| cx.quit());

            let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |window, cx| {
                    let url = cx.new(|cx| {
                        let mut editor = Editor::single_line(window, cx);
                        editor.set_placeholder_text("Server URL", window, cx);
                        editor
                    });
                    window.focus(&url.focus_handle(cx), cx);
                    cx.new(|_cx| Placeholder { url })
                },
            )
            .expect("open the main window");
            cx.activate(true);
        });
}

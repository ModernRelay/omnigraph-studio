//! OmniGraph Studio. The composition root: every crate is initialised here, in layer order
//! (`docs/architecture.md`).
//!
//! Spike S0 (`specs/001-foundation/plan.md`): this opens one window rendered with Zed's `ui`
//! and `theme`. The frame from spec 002 replaces it.

use gpui::{App, Bounds, Context, Window, WindowBounds, WindowOptions, size};
use studio_actions::workspace::Quit;
use ui::prelude::*;

struct Placeholder;

impl Render for Placeholder {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .p_6()
            .gap_2()
            .bg(cx.theme().colors().background)
            .child(Headline::new("OmniGraph Studio"))
            .child(Label::new("Not connected to a server yet.").color(Color::Muted))
    }
}

fn main() {
    gpui_platform::application().run(|cx: &mut App| {
        studio_theme::init(cx);
        cx.on_action(|_: &Quit, cx| cx.quit());

        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(|_cx| Placeholder),
        )
        .expect("open the main window");
        cx.activate(true);
    });
}

//! OmniGraph Studio.

use gpui::App;

fn main() {
    gpui_platform::application()
        .with_assets(studio_theme::Assets)
        .run(|cx: &mut App| {
            studio::init(cx);
            cx.open_window(studio::window_options(cx), |window, cx| {
                studio::build_workspace(window, cx)
            })
            .expect("open the main window");
            cx.activate(true);
        });
}

//! The About window (spec 002, "About"): Studio's icon, name, version and licence, as a Mac
//! app's About panel shows them.

use std::sync::Arc;

use gpui::{
    App, AppContext as _, Bounds, Context, FocusHandle, Image, ImageFormat, Render,
    TitlebarOptions, Window, WindowBounds, WindowOptions, img, size,
};
use studio_actions::workspace::CloseActiveItem;
use studio_ui::ds;
use ui::prelude::*;

/// The icon the Linux bundle installs: the Modern Relay mark on its tile.
const ICON: &[u8] = include_bytes!("../../../resources/linux/omnigraph-studio.png");
const COPYRIGHT: &str = "Copyright © 2026 Modern Relay";
const LICENCE: &str = "Free software under the GNU GPL, version 3 or later.";

pub(crate) fn version_line() -> String {
    format!("Version {}", env!("CARGO_PKG_VERSION"))
}

/// Opens the About window, or brings the open one forward.
pub(crate) fn open_about(cx: &mut App) {
    if let Some(open) = cx
        .windows()
        .into_iter()
        .find_map(|window| window.downcast::<AboutWindow>())
    {
        open.update(cx, |_, window, _| window.activate_window())
            .ok();
        return;
    }
    let bounds = Bounds::centered(
        None,
        size(px(ds::size::ABOUT_WIDTH), px(ds::size::ABOUT_HEIGHT)),
        cx,
    );
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some("About Omnigraph Studio".into()),
            appears_transparent: true,
            traffic_light_position: None,
        }),
        is_resizable: false,
        is_minimizable: false,
        app_id: Some("com.modernrelay.omnigraph-studio".into()),
        ..Default::default()
    };
    cx.open_window(options, |window, cx| {
        let about = cx.new(|cx| AboutWindow::new(window, cx));
        let focus = about.read(cx).focus_handle.clone();
        window.focus(&focus, cx);
        about
    })
    .ok();
}

pub(crate) struct AboutWindow {
    focus_handle: FocusHandle,
    icon: Arc<Image>,
}

impl AboutWindow {
    pub(crate) fn new(_: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            icon: Arc::new(Image::from_bytes(ImageFormat::Png, ICON.to_vec())),
        }
    }
}

impl Render for AboutWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("AboutWindow")
            .track_focus(&self.focus_handle)
            // Escape and Cmd-W close it, as a Mac About panel closes. Cmd-W is File → Close
            // Tab's key, whose action macOS sends before the keymap sees the key.
            .on_action(cx.listener(|_, _: &menu::Cancel, window, _| window.remove_window()))
            .on_action(cx.listener(|_, _: &CloseActiveItem, window, _| window.remove_window()))
            .size_full()
            .items_center()
            .justify_center()
            .gap(px(ds::space::XS))
            .bg(ds::surface::window(cx))
            .child(
                div()
                    .debug_selector(|| "about-icon".into())
                    .child(img(self.icon.clone()).size(px(ds::size::ABOUT_ICON))),
            )
            .child(
                div()
                    .pt(px(ds::space::SM))
                    .debug_selector(|| "about-name".into())
                    .child(ds::text::title("Omnigraph Studio")),
            )
            .child(
                div()
                    .debug_selector(|| "about-version".into())
                    .child(ds::text::body(version_line())),
            )
            .child(
                v_flex()
                    .items_center()
                    .pt(px(ds::space::SM))
                    .debug_selector(|| "about-licence".into())
                    .child(ds::text::meta(COPYRIGHT))
                    .child(ds::text::meta(LICENCE)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};

    fn init(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
        });
    }

    fn about_windows(cx: &mut TestAppContext) -> usize {
        cx.update(|cx| {
            cx.windows()
                .into_iter()
                .filter(|window| window.downcast::<AboutWindow>().is_some())
                .count()
        })
    }

    #[test]
    fn the_version_line_names_this_build() {
        assert_eq!(
            version_line(),
            format!("Version {}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[gpui::test]
    fn about_shows_one_window_however_often_it_is_asked(cx: &mut TestAppContext) {
        init(cx);
        cx.update(open_about);
        cx.update(open_about);
        cx.run_until_parked();
        assert_eq!(about_windows(cx), 1);
    }

    #[gpui::test]
    fn the_window_shows_the_icon_name_version_and_licence(cx: &mut TestAppContext) {
        init(cx);
        let (_, cx): (_, &mut VisualTestContext) = cx.add_window_view(AboutWindow::new);
        cx.run_until_parked();
        for part in ["about-icon", "about-name", "about-version", "about-licence"] {
            assert!(cx.debug_bounds(part).is_some(), "{part} was not drawn");
        }
    }

    /// Cmd-W is File → Close Tab's key, and macOS sends the menu's action before the keymap
    /// sees the key: the window closes on that action too.
    #[gpui::test]
    fn close_tab_closes_it(cx: &mut TestAppContext) {
        init(cx);
        cx.update(open_about);
        cx.run_until_parked();
        let window = cx.update(|cx| {
            cx.windows()
                .into_iter()
                .find_map(|window| window.downcast::<AboutWindow>())
                .unwrap()
        });
        window
            .update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(studio_actions::workspace::CloseActiveItem), cx)
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(about_windows(cx), 0);
    }

    #[gpui::test]
    fn escape_closes_it(cx: &mut TestAppContext) {
        init(cx);
        cx.update(open_about);
        cx.run_until_parked();
        let window = cx.update(|cx| {
            cx.windows()
                .into_iter()
                .find_map(|window| window.downcast::<AboutWindow>())
                .unwrap()
        });
        window
            .update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(menu::Cancel), cx)
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(about_windows(cx), 0);
    }
}

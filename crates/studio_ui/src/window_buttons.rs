//! The window's own minimize, maximize and close buttons (spec 002, "The window's buttons"),
//! for where the system draws none.
//!
//! Studio's top bar takes the title bar's place (`TitlebarOptions::appears_transparent`). macOS
//! still draws its traffic lights over the bar. Windows then gives the window no title bar and
//! no buttons, so Studio draws the three caption buttons and marks each one's area
//! ([`WindowControlArea`]): the system presses them, shows Windows 11's snap layouts over
//! maximize, and closes as its own close button would. A Linux desktop that frames windows
//! itself (X11, KDE) draws its title bar above the bar. One that leaves the frame to the window
//! (GNOME on Wayland) draws nothing, and there Studio draws the buttons the desktop's layout
//! names, on the side it names, and acts on a click.
//!
//! The drawings are Zed's four generic ones on every platform, where Zed takes Windows' icon
//! font on Windows: the buttons can then be drawn, and looked at, where no Windows is at hand.

// Forked from zed-industries/zed crates/platform_title_bar/src/platforms/platform_windows.rs
// and crates/platform_title_bar/src/platforms/platform_linux.rs @ c84af5b99cbc

use std::rc::Rc;

use gpui::{
    App, Decorations, Hsla, MAX_BUTTONS_PER_SIDE, MouseButton, Rgba, Window, WindowButton,
    WindowButtonLayout, WindowControlArea, prelude::*, svg,
};
use ui::prelude::*;

use crate::ds;

/// A caption button's width on Windows, as Zed draws it; its height is the bar's.
const CAPTION_BUTTON_WIDTH: f32 = 36.;

/// The red a close button turns under the pointer on Windows.
const CLOSE_HOVER: Rgba = Rgba {
    r: 232. / 255.,
    g: 17. / 255.,
    b: 32. / 255.,
    a: 1.,
};

/// The buttons a Linux desktop shows when it names no layout: all three on the right.
pub const RIGHT_SIDE: WindowButtonLayout = WindowButtonLayout {
    left: [None; MAX_BUTTONS_PER_SIDE],
    right: [
        Some(WindowButton::Minimize),
        Some(WindowButton::Maximize),
        Some(WindowButton::Close),
    ],
};

/// What a close button Studio draws does: closes the window as the system's close would.
type Close = Rc<dyn Fn(&mut Window, &mut App)>;

/// Who draws the window's minimize, maximize and close buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowButtons {
    /// The system, or nobody: macOS's traffic lights, the title bar a Linux desktop puts above
    /// the window, and a window in full screen, which has none.
    System,
    /// Studio, as Windows' caption buttons at the bar's right end ([`caption_buttons`]).
    Windows,
    /// Studio, on a Linux desktop that leaves the frame to the window: the buttons its layout
    /// names, each side's at that end of the bar ([`frame_buttons`]).
    Linux(WindowButtonLayout),
}

impl WindowButtons {
    /// Who draws them for `window` as it is now.
    pub fn of(window: &Window, cx: &App) -> Self {
        if window.is_fullscreen() || window.is_simple_fullscreen() {
            return Self::System;
        }
        if cfg!(target_os = "windows") {
            return Self::Windows;
        }
        if cfg!(any(target_os = "linux", target_os = "freebsd"))
            && matches!(window.window_decorations(), Decorations::Client { .. })
        {
            return Self::Linux(cx.button_layout().unwrap_or(RIGHT_SIDE));
        }
        Self::System
    }

    /// The buttons Studio draws at the bar's left end: a Linux desktop's that puts some there.
    pub fn left(self) -> Option<[Option<WindowButton>; MAX_BUTTONS_PER_SIDE]> {
        match self {
            Self::Linux(layout) if layout.left[0].is_some() => Some(layout.left),
            _ => None,
        }
    }

    /// The buttons Studio draws at the bar's right end on a Linux desktop that puts some there.
    pub fn right(self) -> Option<[Option<WindowButton>; MAX_BUTTONS_PER_SIDE]> {
        match self {
            Self::Linux(layout) if layout.right[0].is_some() => Some(layout.right),
            _ => None,
        }
    }

    /// Whether Studio draws buttons at the bar's right end, which then has no inset of its own.
    pub fn at_right(self) -> bool {
        self == Self::Windows || self.right().is_some()
    }

    /// Whether the window has no frame but the one Studio draws, so a double click on the bar
    /// is Studio's to act on. Windows acts on its own, and macOS has its own call.
    pub fn frames_itself(self) -> bool {
        matches!(self, Self::Linux(_))
    }
}

/// Which drawing a button shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Glyph {
    Minimize,
    Maximize,
    Restore,
    Close,
}

impl Glyph {
    fn id(self) -> &'static str {
        match self {
            Self::Minimize => "minimize",
            Self::Maximize => "maximize",
            Self::Restore => "restore",
            Self::Close => "close",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::Minimize => IconName::GenericMinimize,
            Self::Maximize => IconName::GenericMaximize,
            Self::Restore => IconName::GenericRestore,
            Self::Close => IconName::GenericClose,
        }
    }

    /// Whether the window can do what the button asks.
    fn enabled(self, window: &Window) -> bool {
        match self {
            Self::Minimize => window.is_minimizable(),
            Self::Maximize | Self::Restore => window.is_resizable(),
            Self::Close => true,
        }
    }

    fn drawing(self, ink: Hsla) -> gpui::Svg {
        svg()
            .size(px(ds::size::ROW_ICON))
            .flex_none()
            .path(self.icon().path())
            .text_color(ink)
    }
}

/// Windows' caption buttons, for the right end of the top bar, which they fill top to bottom
/// and to the window's corner: minimize, maximize (restore while maximized) and close.
pub fn caption_buttons() -> impl IntoElement {
    CaptionButtons
}

#[derive(IntoElement)]
struct CaptionButtons;

impl RenderOnce for CaptionButtons {
    fn render(self, window: &mut Window, _: &mut App) -> impl IntoElement {
        let middle = if window.is_maximized() {
            Glyph::Restore
        } else {
            Glyph::Maximize
        };
        h_flex()
            .id("window-buttons")
            .debug_selector(|| "window-buttons".into())
            .h_full()
            .flex_none()
            .child(CaptionButton(Glyph::Minimize))
            .child(CaptionButton(middle))
            .child(CaptionButton(Glyph::Close))
    }
}

/// One caption button. It has no click of its own: its area is marked for the system, which
/// acts when the press and the release both land on it.
#[derive(IntoElement)]
struct CaptionButton(Glyph);

impl RenderOnce for CaptionButton {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let glyph = self.0;
        let enabled = glyph.enabled(window);
        let colors = cx.theme().colors();
        let (hover, hover_ink, pressed) = match glyph {
            Glyph::Close => {
                let red: Hsla = CLOSE_HOVER.into();
                (red, gpui::white(), red.opacity(0.8))
            }
            _ => (
                colors.ghost_element_hover,
                colors.text,
                colors.ghost_element_active,
            ),
        };
        let ink = if enabled {
            colors.text
        } else {
            colors.text_disabled
        };
        let area = match glyph {
            Glyph::Close => WindowControlArea::Close,
            Glyph::Maximize | Glyph::Restore => WindowControlArea::Max,
            Glyph::Minimize => WindowControlArea::Min,
        };
        h_flex()
            .id(glyph.id())
            .debug_selector(move || format!("window-button-{}", glyph.id()))
            .group(glyph.id())
            .justify_center()
            .occlude()
            .w(px(CAPTION_BUTTON_WIDTH))
            .h_full()
            .when(enabled, |button| {
                button
                    .hover(|style| style.bg(hover))
                    .active(|style| style.bg(pressed))
            })
            .window_control_area(area)
            .child(glyph.drawing(ink).when(enabled, |drawing| {
                drawing.group_hover(glyph.id(), |style| style.text_color(hover_ink))
            }))
    }
}

/// One side's buttons on a Linux desktop that leaves the frame to the window: those of
/// `buttons` the desktop supports, in its order. `close` closes the window as the system's
/// close would, asking what that asks.
pub fn frame_buttons(
    id: &'static str,
    buttons: [Option<WindowButton>; MAX_BUTTONS_PER_SIDE],
    close: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    FrameButtons {
        id,
        buttons,
        close: Rc::new(close),
    }
}

#[derive(IntoElement)]
struct FrameButtons {
    id: &'static str,
    buttons: [Option<WindowButton>; MAX_BUTTONS_PER_SIDE],
    close: Close,
}

impl RenderOnce for FrameButtons {
    fn render(self, window: &mut Window, _: &mut App) -> impl IntoElement {
        let maximized = window.is_maximized();
        let supported = window.window_controls();
        let buttons: Vec<FrameButton> = self
            .buttons
            .iter()
            .flatten()
            .filter(|button| match button {
                WindowButton::Minimize => supported.minimize,
                WindowButton::Maximize => supported.maximize,
                WindowButton::Close => true,
            })
            .map(|button| FrameButton {
                glyph: match button {
                    WindowButton::Minimize => Glyph::Minimize,
                    WindowButton::Maximize if maximized => Glyph::Restore,
                    WindowButton::Maximize => Glyph::Maximize,
                    WindowButton::Close => Glyph::Close,
                },
                close: self.close.clone(),
            })
            .collect();
        h_flex()
            .id(self.id)
            .debug_selector(move || self.id.into())
            .flex_none()
            .when(!buttons.is_empty(), |side| {
                side.gap(px(ds::space::LG))
                    .px(px(ds::space::LG))
                    // A press on a button is not the start of a window move.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .children(buttons)
            })
    }
}

/// One round button of a window that frames itself; a click acts.
#[derive(IntoElement)]
struct FrameButton {
    glyph: Glyph,
    close: Close,
}

impl RenderOnce for FrameButton {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let glyph = self.glyph;
        let enabled = glyph.enabled(window);
        let colors = cx.theme().colors();
        let (ink, hover_ink, hover) = (colors.icon, colors.icon_muted, colors.ghost_element_hover);
        let ink = if enabled { ink } else { colors.icon_disabled };
        h_flex()
            .id(glyph.id())
            .debug_selector(move || format!("window-button-{}", glyph.id()))
            .group(glyph.id())
            .justify_center()
            .rounded_full()
            .size(px(ds::size::ICON_TILE))
            .when(enabled, |button| {
                button
                    .cursor_pointer()
                    .hover(|style| style.bg(hover))
                    .active(|style| style.bg(hover))
            })
            .child(glyph.drawing(ink).when(enabled, |drawing| {
                drawing.group_hover(glyph.id(), |style| style.text_color(hover_ink))
            }))
            .on_mouse_move(|_, _, cx| cx.stop_propagation())
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                if !enabled {
                    return;
                }
                match glyph {
                    Glyph::Minimize => window.minimize_window(),
                    Glyph::Maximize | Glyph::Restore => window.zoom_window(),
                    Glyph::Close => (self.close)(window, cx),
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A desktop's layout puts each side's buttons at that end of the bar; Windows' are all at
    /// the right, and the system's (or a full-screen window's) are nowhere in the bar.
    #[test]
    fn each_side_of_the_bar_gets_its_buttons() {
        let close_left = WindowButtonLayout {
            left: [Some(WindowButton::Close), None, None],
            right: [
                Some(WindowButton::Minimize),
                Some(WindowButton::Maximize),
                None,
            ],
        };
        let linux = WindowButtons::Linux(close_left);
        assert_eq!(linux.left(), Some(close_left.left));
        assert_eq!(linux.right(), Some(close_left.right));
        assert!(linux.at_right() && linux.frames_itself());

        let right = WindowButtons::Linux(RIGHT_SIDE);
        assert_eq!(right.left(), None);
        assert_eq!(right.right(), Some(RIGHT_SIDE.right));

        let all_left = WindowButtons::Linux(WindowButtonLayout {
            left: RIGHT_SIDE.right,
            right: [None; MAX_BUTTONS_PER_SIDE],
        });
        assert!(!all_left.at_right(), "the bar keeps its inset at the right");

        assert!(WindowButtons::Windows.at_right());
        assert!(
            !WindowButtons::Windows.frames_itself(),
            "Windows acts itself"
        );
        assert_eq!(WindowButtons::Windows.left(), None);
        assert_eq!(WindowButtons::Windows.right(), None, "its own element");
        assert!(!WindowButtons::System.at_right());
        assert_eq!(WindowButtons::System.left(), None);
    }
}

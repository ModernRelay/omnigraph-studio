//! Reduced motion (constitution VII): GPUI's `reduce_motion`, which Studio's motion and GPUI's
//! own animations read, set from the `studio.reduce_motion` setting, and with `system` from the
//! system's accessibility setting. GPUI reads no system setting itself (Zed sets it from its
//! own setting alone, `crates/zed/src/zed.rs:2076` at v1.27.0), so before this Studio moved
//! things whatever the person had asked of their system.
//!
//! The system's setting is read at launch, when the setting changes and whenever a Studio
//! window comes to the front, so a change made in the system's settings is taken when the
//! person comes back.

use gpui::{App, AppContext as _};
use studio_settings::{ReduceMotion, StudioSettings};
use studio_workspace::Workspace;

pub(crate) fn init(cx: &mut App) {
    apply(cx);
    cx.observe_global::<StudioSettings>(apply).detach();
    cx.observe_new::<Workspace>(|_, window, cx| {
        let Some(window) = window else {
            return;
        };
        cx.observe_window_activation(window, |_, window, cx| {
            if window.is_window_active() {
                apply(cx);
            }
        })
        .detach();
    })
    .detach();
}

/// Sets GPUI's `reduce_motion` from the setting, reading the system's where it says to.
fn apply(cx: &mut App) {
    match StudioSettings::get(cx).studio.reduce_motion {
        ReduceMotion::On => cx.set_reduce_motion(true),
        ReduceMotion::Off => cx.set_reduce_motion(false),
        ReduceMotion::System => {
            let read = cx.background_spawn(async { system_reduces_motion() });
            cx.spawn(async move |cx| {
                let reduce = read.await;
                cx.update(|cx| {
                    // The setting may have changed while the system was asked.
                    if StudioSettings::get(cx).studio.reduce_motion == ReduceMotion::System {
                        cx.set_reduce_motion(reduce);
                    }
                });
            })
            .detach();
        }
    }
}

/// macOS: Accessibility › Display › Reduce motion.
#[cfg(target_os = "macos")]
fn system_reduces_motion() -> bool {
    objc2_app_kit::NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
}

/// Windows: Accessibility › Visual effects › Animation effects, off.
#[cfg(target_os = "windows")]
fn system_reduces_motion() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SPI_GETCLIENTAREAANIMATION, SystemParametersInfoW,
    };
    let mut animates: windows_sys::core::BOOL = 1;
    // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL through the pointer, which points at
    // one that lives through the call.
    let read = unsafe {
        SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, (&raw mut animates).cast(), 0)
    };
    read != 0 && animates == 0
}

/// Linux: the desktop's `enable-animations` (GNOME's Reduce animation, and the desktops that
/// follow its schema), read through `gsettings`; none there is no reduction.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn system_reduces_motion() -> bool {
    std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "enable-animations"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "false")
}

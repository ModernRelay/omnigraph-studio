//! Telemetry's two status bar items (spec 018, "What the person sees"): "Usage data: not
//! shared · Choose…" until the person chooses, and "Studio quit unexpectedly · Send report…"
//! while a crash report waits for an answer. Each opens a popover; neither is a dialog.

use gpui::{
    DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, Stateful, Subscription,
    Window,
};
use studio_settings::{CrashReports, UsageSharing};
use studio_telemetry::Telemetry;
use studio_ui::ds::space;
use studio_ui::menu::menu_surface;
use ui::{ButtonStyle, PopoverMenu, prelude::*};

/// The list of everything Studio may send, as the repository keeps it.
pub(crate) const WHAT_STUDIO_SENDS: &str =
    "https://github.com/ModernRelay/omnigraph-studio/blob/main/docs/telemetry.md";

/// Whether the usage item has anything to say.
pub(crate) fn asks_about_usage(telemetry: &Entity<Telemetry>, cx: &App) -> bool {
    telemetry.read(cx).asks_about_usage(cx)
}

/// Whether the crash item has anything to say.
pub(crate) fn offers_a_crash_report(telemetry: &Entity<Telemetry>, cx: &App) -> bool {
    !telemetry.read(cx).waiting_crashes().is_empty()
}

pub(crate) fn choose_usage(usage: UsageSharing, cx: &mut App) {
    studio_settings::update(cx, |settings| settings.studio.telemetry.usage = usage);
}

/// The usage item: drawn again when telemetry or the settings change.
pub(crate) struct UsageStatus {
    telemetry: Entity<Telemetry>,
    _observe: Subscription,
}

impl UsageStatus {
    pub fn new(telemetry: Entity<Telemetry>, cx: &mut Context<Self>) -> Self {
        let _observe = cx.observe(&telemetry, |_, _, cx| cx.notify());
        Self {
            telemetry,
            _observe,
        }
    }
}

impl Render for UsageStatus {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !asks_about_usage(&self.telemetry, cx) {
            return div().into_any_element();
        }
        h_flex()
            .debug_selector(|| "usage-status".into())
            .flex_none()
            .gap(px(space::XS))
            .items_center()
            .child(studio_ui::ds::text::meta("Share anonymous usage data?").single_line())
            .child(
                PopoverMenu::new("usage-choice")
                    .anchor(gpui::Anchor::BottomRight)
                    .attach(gpui::Anchor::TopRight)
                    .trigger(studio_ui::ds::button("usage-choose", "Choose…"))
                    .menu({
                        let telemetry = self.telemetry.clone();
                        move |_, cx| {
                            let telemetry = telemetry.clone();
                            Some(cx.new(|cx| UsagePopover::new(telemetry, cx)))
                        }
                    }),
            )
            .into_any_element()
    }
}

/// "Share anonymous usage data?": share it, share it counted across runs, or not.
pub(crate) struct UsagePopover {
    telemetry: Entity<Telemetry>,
    focus: FocusHandle,
}

impl UsagePopover {
    fn new(telemetry: Entity<Telemetry>, cx: &mut Context<Self>) -> Self {
        Self {
            telemetry,
            focus: cx.focus_handle(),
        }
    }

    /// Sets the choice, and the notice goes.
    fn answer(&mut self, usage: UsageSharing, cx: &mut Context<Self>) {
        choose_usage(usage, cx);
        self.telemetry
            .update(cx, |telemetry, cx| telemetry.notice_seen(cx));
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for UsagePopover {}

impl Focusable for UsagePopover {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for UsagePopover {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        popover(&self.focus, cx)
            .child(studio_ui::ds::text::heading("Share anonymous usage data?"))
            .child(studio_ui::ds::text::muted(
                "Which features are used and how fast they are, to improve Studio; never names, \
                 queries, values or server addresses, and nothing that ties one run of Studio to \
                 the next. Nothing is sent until you choose. You can also be counted across runs \
                 with a random install ID, which helps us see whether people keep using Studio.",
            ))
            .child(
                h_flex()
                    .gap(px(space::SM))
                    .flex_wrap()
                    .child(
                        studio_ui::ds::button("usage-keep", "Share")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.answer(UsageSharing::Anonymous, cx)
                            })),
                    )
                    .child(
                        studio_ui::ds::button("usage-id", "Share and count my runs").on_click(
                            cx.listener(|this, _, _, cx| {
                                this.answer(UsageSharing::WithInstallId, cx)
                            }),
                        ),
                    )
                    .child(
                        studio_ui::ds::button("usage-off", "Don't share").on_click(
                            cx.listener(|this, _, _, cx| this.answer(UsageSharing::Off, cx)),
                        ),
                    )
                    .child(
                        studio_ui::ds::button("usage-what", "What Studio sends")
                            .on_click(|_, _, cx| cx.open_url(WHAT_STUDIO_SENDS)),
                    ),
            )
    }
}

/// The crash item.
pub(crate) struct CrashStatus {
    telemetry: Entity<Telemetry>,
    _observe: Subscription,
}

impl CrashStatus {
    pub fn new(telemetry: Entity<Telemetry>, cx: &mut Context<Self>) -> Self {
        let _observe = cx.observe(&telemetry, |_, _, cx| cx.notify());
        Self {
            telemetry,
            _observe,
        }
    }
}

impl Render for CrashStatus {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !offers_a_crash_report(&self.telemetry, cx) {
            return div().into_any_element();
        }
        let telemetry = self.telemetry.clone();
        h_flex()
            .debug_selector(|| "crash-status".into())
            .flex_none()
            .gap(px(space::XS))
            .items_center()
            .child(studio_ui::ds::text::meta("Studio quit unexpectedly").single_line())
            .child(
                PopoverMenu::new("crash-report")
                    .anchor(gpui::Anchor::BottomRight)
                    .attach(gpui::Anchor::TopRight)
                    .trigger(studio_ui::ds::button("crash-open", "Send report…"))
                    .menu(move |_, cx| {
                        let telemetry = telemetry.clone();
                        Some(cx.new(|cx| CrashPopover::new(telemetry, cx)))
                    }),
            )
            .into_any_element()
    }
}

/// The report as it would be sent, and Send, Don't send and Always send.
pub(crate) struct CrashPopover {
    telemetry: Entity<Telemetry>,
    focus: FocusHandle,
}

impl CrashPopover {
    fn new(telemetry: Entity<Telemetry>, cx: &mut Context<Self>) -> Self {
        Self {
            telemetry,
            focus: cx.focus_handle(),
        }
    }
}

impl EventEmitter<DismissEvent> for CrashPopover {}

impl Focusable for CrashPopover {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for CrashPopover {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let telemetry = self.telemetry.read(cx);
        let shown: Vec<String> = telemetry
            .waiting_crashes()
            .iter()
            .map(|(_, report)| {
                serde_json::to_string_pretty(&telemetry.crash_as_sent(report)).unwrap_or_default()
            })
            .collect();
        let count = shown.len();
        let send = self.telemetry.clone();
        let drop = self.telemetry.clone();
        let always = self.telemetry.clone();
        popover(&self.focus, cx)
            .w(px(studio_ui::ds::size::MODAL_WIDTH))
            .child(studio_ui::ds::text::heading(if count == 1 {
                "Send the report of Studio's last crash?".to_owned()
            } else {
                format!("Send the reports of Studio's last {count} crashes?")
            }))
            .child(studio_ui::ds::text::muted(
                "This is what would be sent: where Studio was when it stopped, without your \
                 data. A report not answered in 7 days is deleted.",
            ))
            .child(
                div()
                    .id("crash-report-text")
                    .max_h(px(studio_ui::ds::size::PREVIEW_MAX_HEIGHT))
                    .overflow_y_scroll()
                    .child(studio_ui::ds::code_block(shown.join("\n\n"), cx)),
            )
            .child(
                h_flex()
                    .gap(px(space::SM))
                    .child(
                        studio_ui::ds::button("crash-send", "Send")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(move |_, _, _, cx| {
                                send.update(cx, |t, cx| t.send_crashes(cx));
                                cx.emit(DismissEvent);
                            })),
                    )
                    .child(
                        studio_ui::ds::button("crash-dont", "Don't send").on_click(cx.listener(
                            move |_, _, _, cx| {
                                drop.update(cx, |t, cx| t.discard_crashes(cx));
                                cx.emit(DismissEvent);
                            },
                        )),
                    )
                    .child(
                        studio_ui::ds::button("crash-always", "Always send").on_click(cx.listener(
                            move |_, _, _, cx| {
                                studio_settings::update(cx, |settings| {
                                    settings.studio.telemetry.crash_reports = CrashReports::Always;
                                });
                                always.update(cx, |t, cx| t.send_crashes(cx));
                                cx.emit(DismissEvent);
                            },
                        )),
                    ),
            )
    }
}

fn popover(focus: &FocusHandle, cx: &App) -> Stateful<Div> {
    menu_surface(v_flex(), cx)
        .id("telemetry-popover")
        .track_focus(focus)
        .occlude()
        .w(px(studio_ui::ds::size::POPOVER_MAX))
        .p(px(space::LG))
        .gap(px(space::MD))
}

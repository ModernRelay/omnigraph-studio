//! The updater's item at the right of the status bar (spec 017, "What the person sees"):
//! absent while there is nothing to say.

use gpui::{Action, Context, Entity, Render, Subscription, Window};
use semver::Version;
use studio_actions::update::{DownloadUpdate, RestartToUpdate};
use studio_update::{UpdateState, Updater};
use ui::prelude::*;

/// What the item shows for a state.
pub(crate) struct Shown {
    pub text: String,
    /// A button's label and the action it runs.
    pub button: Option<(String, Box<dyn Action>)>,
    /// Work under way that the person asked for: a spinner before the text. Work in the
    /// background shows its text alone, so nothing moves on screen unasked (constitution VII).
    pub busy: bool,
    pub error: bool,
}

pub(crate) fn shown(state: &UpdateState, running: &Version, asked: bool) -> Option<Shown> {
    let said = |text: String, busy: bool| Shown {
        text,
        button: None,
        busy: busy && asked,
        error: false,
    };
    Some(match state {
        UpdateState::Idle => return None,
        UpdateState::Checking => said("Checking for updates…".into(), true),
        UpdateState::UpToDate => said(format!("Omnigraph Studio {running} is up to date"), false),
        UpdateState::Downloading {
            version,
            percent: Some(percent),
        } => said(format!("Downloading {version} · {percent}%"), true),
        UpdateState::Downloading { version, .. } => said(format!("Downloading {version}…"), true),
        UpdateState::Installing { version } => said(format!("Installing {version}…"), true),
        UpdateState::Ready { version } => Shown {
            text: String::new(),
            button: Some((
                format!("Restart to update to {version}"),
                RestartToUpdate.boxed_clone(),
            )),
            busy: false,
            error: false,
        },
        UpdateState::Available { version, reason } => Shown {
            text: reason
                .clone()
                .unwrap_or_else(|| format!("{version} is available")),
            button: Some(("Download".into(), DownloadUpdate.boxed_clone())),
            busy: false,
            error: false,
        },
        UpdateState::Failed { message } => Shown {
            error: true,
            ..said(message.clone(), false)
        },
        UpdateState::Off => said("Updates are off in a development build.".into(), false),
    })
}

/// Whether the item has anything to say: the workspace leaves it and its rule out otherwise.
pub(crate) fn says_something(updater: &Updater) -> bool {
    shown(updater.state(), updater.running(), updater.asked()).is_some()
}

/// Draws the updater's state; drawn again only when that state changes (the updater notifies
/// on a change, a download at most once per whole percent).
pub(crate) struct UpdateStatus {
    updater: Entity<Updater>,
    _observe: Subscription,
}

impl UpdateStatus {
    pub fn new(updater: Entity<Updater>, cx: &mut Context<Self>) -> Self {
        let _observe = cx.observe(&updater, |_, _, cx| cx.notify());
        Self { updater, _observe }
    }
}

impl Render for UpdateStatus {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let updater = self.updater.read(cx);
        let Some(shown) = shown(updater.state(), updater.running(), updater.asked()) else {
            return div().into_any_element();
        };
        let text = shown.text;
        h_flex()
            .debug_selector(|| "update-status".into())
            .flex_none()
            .gap(px(studio_ui::ds::space::XS))
            .items_center()
            .when(shown.busy, |item| item.child(studio_ui::ds::spinner()))
            .when(!text.is_empty(), |item| {
                item.child(if shown.error {
                    studio_ui::ds::text::error(text).single_line()
                } else {
                    studio_ui::ds::text::meta(text).single_line()
                })
            })
            .children(shown.button.map(|(label, action)| {
                studio_ui::ds::toolbar_button("update-action", label)
                    .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
            }))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use semver::Version;

    fn text(state: UpdateState) -> Option<(String, Option<String>, bool)> {
        shown(&state, &Version::new(0, 2, 1), true)
            .map(|shown| (shown.text, shown.button.map(|(label, _)| label), shown.busy))
    }

    #[test]
    fn work_in_the_background_shows_its_progress_without_a_spinner() {
        let downloading = UpdateState::Downloading {
            version: Version::new(0, 3, 0),
            percent: Some(42),
        };
        let shown = shown(&downloading, &Version::new(0, 2, 1), false).unwrap();
        assert_eq!(shown.text, "Downloading 0.3.0 · 42%");
        assert!(!shown.busy);
    }

    #[test]
    fn each_state_says_what_the_spec_says() {
        let v = || Version::new(0, 3, 0);
        assert_eq!(text(UpdateState::Idle), None);
        assert_eq!(
            text(UpdateState::Checking),
            Some(("Checking for updates…".into(), None, true))
        );
        assert_eq!(
            text(UpdateState::UpToDate),
            Some(("Omnigraph Studio 0.2.1 is up to date".into(), None, false))
        );
        assert_eq!(
            text(UpdateState::Downloading {
                version: v(),
                percent: Some(42)
            }),
            Some(("Downloading 0.3.0 · 42%".into(), None, true))
        );
        assert_eq!(
            text(UpdateState::Downloading {
                version: v(),
                percent: None
            }),
            Some(("Downloading 0.3.0…".into(), None, true))
        );
        assert_eq!(
            text(UpdateState::Installing { version: v() }),
            Some(("Installing 0.3.0…".into(), None, true))
        );
        assert_eq!(
            text(UpdateState::Ready { version: v() }),
            Some((
                String::new(),
                Some("Restart to update to 0.3.0".into()),
                false
            ))
        );
        assert_eq!(
            text(UpdateState::Available {
                version: v(),
                reason: None
            }),
            Some(("0.3.0 is available".into(), Some("Download".into()), false))
        );
        assert_eq!(
            text(UpdateState::Available {
                version: v(),
                reason: Some("Move Omnigraph Studio to Applications to update it.".into())
            }),
            Some((
                "Move Omnigraph Studio to Applications to update it.".into(),
                Some("Download".into()),
                false
            ))
        );
        assert_eq!(
            text(UpdateState::Failed {
                message: "Couldn't check for updates: offline.".into()
            }),
            Some(("Couldn't check for updates: offline.".into(), None, false))
        );
        assert_eq!(
            text(UpdateState::Off),
            Some((
                "Updates are off in a development build.".into(),
                None,
                false
            ))
        );
    }
}

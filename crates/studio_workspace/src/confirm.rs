//! A question before an action that costs more than its click suggests (constitution VII):
//! what it does, what it costs, and the action's own button, confirmed with ↵ and dismissed
//! with Esc. Views that open many nodes at once ask with it (spec 006, "Expand all"). A close
//! that would drop unsaved work asks with [`ConfirmClose`], which only asks: the workspace, or
//! a quit across windows, says what closes.

use gpui::{App, DismissEvent, EventEmitter, FocusHandle, Focusable, Render, Window};
use ui::prelude::*;

use crate::{ModalView, Workspace};

/// What a question runs when the person agrees.
type OnConfirm = Box<dyn FnOnce(&mut Window, &mut App)>;

/// What [`confirm`] asks, and what it runs when the person agrees.
pub struct Confirm {
    title: SharedString,
    description: SharedString,
    action: SharedString,
    on_confirm: Option<OnConfirm>,
    focus_handle: FocusHandle,
}

impl EventEmitter<DismissEvent> for Confirm {}

impl ModalView for Confirm {
    /// It asks about what a tab of the graph shown would do.
    fn goes_with_graph(&self) -> bool {
        true
    }
}

impl Focusable for Confirm {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Confirm {
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
        if let Some(on_confirm) = self.on_confirm.take() {
            on_confirm(window, cx);
        }
    }
}

impl Render for Confirm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self.focus_handle.clone();
        studio_ui::ds::modal(cx)
            .key_context("Confirm")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.confirm(window, cx)))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .child(
                studio_ui::ds::ModalHeader::new(self.title.clone())
                    .description(self.description.clone())
                    .on_close(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
            )
            .child(
                studio_ui::ds::modal_footer(cx)
                    .child(
                        studio_ui::ds::cancel_button("confirm-cancel")
                            .key_binding(ui::KeyBinding::for_action_in(&menu::Cancel, &focus, cx))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        studio_ui::ds::action_button("confirm-action", self.action.clone())
                            .key_binding(ui::KeyBinding::for_action_in(&menu::Confirm, &focus, cx))
                            .on_click(cx.listener(|this, _, window, cx| this.confirm(window, cx))),
                    ),
            )
    }
}

/// Asks `title` in `window`'s workspace, with `description` saying what it costs, and runs
/// `on_confirm` when the person presses `action` or ↵. Without a workspace (a view drawn
/// alone, as in a test) it runs at once.
pub fn confirm(
    window: &mut Window,
    cx: &mut App,
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    action: impl Into<SharedString>,
    on_confirm: impl FnOnce(&mut Window, &mut App) + 'static,
) {
    let Some(workspace) = crate::workspace_for(window) else {
        on_confirm(window, cx);
        return;
    };
    let (title, description, action) = (title.into(), description.into(), action.into());
    workspace.update(cx, |workspace: &mut Workspace, cx| {
        workspace.show_modal(window, cx, move |_, cx| Confirm {
            title,
            description,
            action,
            on_confirm: Some(Box::new(on_confirm)),
            focus_handle: cx.focus_handle(),
        });
    });
}

/// Asks before an Expand all that opens `nodes` nodes and asks the server for `reads` reads
/// (spec 006, "Expand all"), and runs `expand` when the person agrees. The canvas and the tree
/// ask with it, so both word it alike.
pub fn confirm_expand(
    window: &mut Window,
    cx: &mut App,
    nodes: usize,
    reads: usize,
    expand: impl FnOnce(&mut Window, &mut App) + 'static,
) {
    let (title, description, action) = expand_question(nodes, reads);
    confirm(window, cx, title, description, action, expand);
}

/// The words of [`confirm_expand`]: a title, what it costs, and the action's label.
fn expand_question(nodes: usize, reads: usize) -> (String, String, String) {
    let reads = match reads {
        1 => "1 read".to_owned(),
        n => format!("{} reads", studio_ui::group_digits(n)),
    };
    if nodes == 0 {
        return (
            "Open these relationships?".to_owned(),
            format!("Opening them asks the server for {reads}, and shows what they hold."),
            "Open".to_owned(),
        );
    }
    let nodes = studio_ui::group_digits(nodes);
    (
        format!("Expand {nodes} nodes?"),
        format!(
            "Each node shows its relationships: that asks the server for {reads}, and draws \
             a hub for every relationship with neighbours."
        ),
        format!("Expand {nodes} nodes"),
    )
}

/// What a close asked about closes: a tab, the shown graph, the window, or Studio, quitting
/// or restarting into an update.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Closing {
    Tab,
    Graph,
    Window,
    Quit,
    Restart,
}

impl Closing {
    /// The confirm button's label: what pressing it does.
    pub(crate) fn confirm(self) -> &'static str {
        match self {
            Closing::Quit => "Quit",
            Closing::Restart => "Restart",
            Closing::Tab | Closing::Graph | Closing::Window => "Close",
        }
    }

    /// How much it closes: each holds the ones before it.
    pub(crate) fn reach(self) -> u8 {
        match self {
            Closing::Tab => 0,
            Closing::Graph => 1,
            Closing::Window => 2,
            Closing::Quit | Closing::Restart => 3,
        }
    }
}

/// What a close dialog runs when the person closes anyway: the close it asked about, as
/// whoever asked carries it out.
pub(crate) type OnClose = Box<dyn Fn(&mut Window, &mut App)>;

/// Asks before a close drops unsaved work (constitution IV), naming the tabs that hold it.
/// It only asks: what closes is the asker's [`OnClose`].
pub(crate) struct ConfirmClose {
    closing: Closing,
    /// The tabs with unsaved work: a tab's own close names the tab.
    unsaved: Vec<SharedString>,
    on_close: OnClose,
    focus_handle: FocusHandle,
}

/// Tabs a close dialog names before "and N more".
const UNSAVED_NAMED: usize = 8;

impl EventEmitter<DismissEvent> for ConfirmClose {}

impl ModalView for ConfirmClose {
    /// A tab's or the graph's question is about the graph shown; the window's and Quit's are
    /// about every graph.
    fn goes_with_graph(&self) -> bool {
        matches!(self.closing, Closing::Tab | Closing::Graph)
    }
}

impl Focusable for ConfirmClose {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ConfirmClose {
    pub(crate) fn new(
        closing: Closing,
        unsaved: Vec<SharedString>,
        on_close: OnClose,
        cx: &mut Context<Self>,
    ) -> Self {
        ConfirmClose {
            closing,
            unsaved,
            on_close,
            focus_handle: cx.focus_handle(),
        }
    }

    /// What it asks about closing.
    pub(crate) fn closing(&self) -> Closing {
        self.closing
    }

    pub(crate) fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
        (self.on_close)(window, cx);
    }

    /// The tabs it names.
    #[cfg(test)]
    pub(crate) fn unsaved(&self) -> &[SharedString] {
        &self.unsaved
    }

    pub(crate) fn header(&self) -> (SharedString, &'static str) {
        let lost = "Their unsaved changes are lost. A write in flight still finishes; the \
                    activity log keeps its outcome.";
        match self.closing {
            Closing::Tab => (
                format!(
                    "Close {}?",
                    self.unsaved.first().cloned().unwrap_or_default()
                )
                .into(),
                "It has unsaved changes or a write still running. A write in flight still \
                 finishes; the activity log keeps its outcome.",
            ),
            Closing::Graph => ("Close the graph?".into(), lost),
            Closing::Window => ("Close this window?".into(), lost),
            Closing::Quit => ("Quit Omnigraph Studio?".into(), lost),
            Closing::Restart => ("Restart to update Omnigraph Studio?".into(), lost),
        }
    }
}

impl Render for ConfirmClose {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self.focus_handle.clone();
        studio_ui::ds::modal(cx)
            .key_context("ConfirmClose")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.close(window, cx)))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .child({
                let (title, description) = self.header();
                studio_ui::ds::ModalHeader::new(title)
                    .description(SharedString::from(description))
                    .on_close(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
            })
            .when(!matches!(self.closing, Closing::Tab), |modal| {
                let more = self.unsaved.len().saturating_sub(UNSAVED_NAMED);
                modal.child(
                    studio_ui::ds::modal_body()
                        .children(
                            self.unsaved
                                .iter()
                                .take(UNSAVED_NAMED)
                                .map(|title| studio_ui::ds::text::body(title.clone())),
                        )
                        .when(more > 0, |body| {
                            body.child(studio_ui::ds::text::muted(format!("and {more} more")))
                        }),
                )
            })
            .child(
                studio_ui::ds::modal_footer(cx)
                    .child(
                        studio_ui::ds::Button::new("keep-open", "Keep open")
                            .variant(studio_ui::ds::Variant::Outline)
                            .key_binding(ui::KeyBinding::for_action_in(&menu::Cancel, &focus, cx))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        studio_ui::ds::action_button("close-anyway", self.closing.confirm())
                            .key_binding(ui::KeyBinding::for_action_in(&menu::Confirm, &focus, cx))
                            .on_click(cx.listener(|this, _, window, cx| this.close(window, cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The question before a large Expand all names the nodes and the reads, digits grouped;
    /// opening only relationships names the reads alone (spec 006).
    #[test]
    fn the_expand_question_names_its_nodes_and_reads() {
        assert_eq!(
            expand_question(1_763, 2_800),
            (
                "Expand 1,763 nodes?".to_owned(),
                "Each node shows its relationships: that asks the server for 2,800 reads, and \
                 draws a hub for every relationship with neighbours."
                    .to_owned(),
                "Expand 1,763 nodes".to_owned(),
            )
        );
        assert_eq!(
            expand_question(0, 1),
            (
                "Open these relationships?".to_owned(),
                "Opening them asks the server for 1 read, and shows what they hold.".to_owned(),
                "Open".to_owned(),
            )
        );
    }
}

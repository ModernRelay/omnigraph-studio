//! A question before an action that costs more than its click suggests (constitution VII):
//! what it does, what it costs, and the action's own button, confirmed with ↵ and dismissed
//! with Esc. Views that open many nodes at once ask with it (spec 006, "Expand all").

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

//! System notifications (spec 045, step 1): while no Studio window is in front, a conversation
//! that finishes a turn, stops short or waits for the person's answer posts one notification
//! through GPUI's `show_system_notification` (`UNUserNotificationCenter` on macOS), and clicking
//! it opens the conversation in the window that shows its graph. Studio in front posts nothing:
//! the conversation's tab and the Conversations panel already say it.
//!
//! macOS only for now (`lib.rs` declares the module there). A notification names the graph and
//! the agent's branch, never what the agent or the person said, since it can be read on a
//! locked screen and stays in Notification Center (constitution: no row data, no query text).
//! The setting is `notifications.agent_turns`.

use agent_panel::{Attention, AttentionKind};
use gpui::{Action as _, App, SharedString, SystemNotification, SystemNotificationResponse};
use studio_actions::agent::OpenConversation;
use studio_settings::StudioSettings;
use studio_workspace::Workspace;

/// What a notification's tag starts with, before the conversation's id: one notification per
/// conversation, a newer one replacing the last (HIG, Notifications: never several for one
/// thing).
const TAG_PREFIX: &str = "agent:";

pub(crate) fn init(cx: &mut App) {
    // The name the system shows a notification under where it asks for one (Windows' and
    // Linux's backends, and GPUI's test platform, which posts nothing without it). macOS takes
    // the bundle's own.
    cx.set_app_identity("com.modernrelay.omnigraph-studio", "Omnigraph Studio");
    agent_panel::on_attention(cx, |attention, cx| {
        if !StudioSettings::get(cx).notifications.agent_turns || studio_in_front(cx) {
            return;
        }
        cx.show_system_notification(notification(attention));
    })
    .detach();
    cx.on_system_notification_response(open);
}

/// Whether a Studio window has the keyboard: then Studio is the app in front.
fn studio_in_front(cx: &mut App) -> bool {
    cx.windows().into_iter().any(|window| {
        window
            .update(cx, |_, window, _| window.is_window_active())
            .unwrap_or(false)
    })
}

/// The notification for `attention`. Its title is title-case and its body one sentence, as the
/// HIG asks; neither names the app, which the system shows.
fn notification(attention: &Attention) -> SystemNotification {
    let graph = graph_name(&attention.graph);
    let (title, body) = match (attention.kind, &attention.branch) {
        (AttentionKind::Finished, Some(branch)) => (
            "Agent Finished",
            format!("Its changes on {branch} in {graph} are ready for review."),
        ),
        (AttentionKind::Finished, None) => (
            "Agent Finished",
            format!("It replied in a conversation on {graph}."),
        ),
        (AttentionKind::Stopped, _) => (
            "Agent Stopped",
            format!("Its turn on {graph} ended before it finished."),
        ),
        (AttentionKind::Waiting, Some(branch)) => (
            "Agent Is Waiting",
            format!("It needs your answer before it changes {branch} in {graph}."),
        ),
        (AttentionKind::Waiting, None) => (
            "Agent Is Waiting",
            format!("It needs your answer in a conversation on {graph}."),
        ),
    };
    SystemNotification {
        tag: format!("{TAG_PREFIX}{}", attention.session).into(),
        title: title.into(),
        body: body.into(),
        actions: Vec::new(),
    }
}

/// A graph's id, from the key conversations are filed under (`<target>@<address>/<graph id>`,
/// `GraphSession::overrides_key`).
fn graph_name(key: &str) -> &str {
    key.rsplit('/').next().unwrap_or(key)
}

/// The conversation a notification's tag names.
fn session_of(tag: &SharedString) -> Option<&str> {
    tag.strip_prefix(TAG_PREFIX).filter(|id| !id.is_empty())
}

/// A click on a notification: Studio comes to the front, and the window showing the
/// conversation's graph opens the conversation (its open tab, as the Conversations panel's row
/// does). With no window on that graph, Studio only comes to the front: a conversation opened
/// in another graph's window would speak for the wrong graph.
fn open(response: SystemNotificationResponse, cx: &mut App) {
    let Some(session) = session_of(&response.tag) else {
        return;
    };
    cx.activate(true);
    let graph = agent_panel::graph_of_conversation(session, cx);
    let window = cx.windows().into_iter().find(|window| {
        window.downcast::<Workspace>().is_some_and(|workspace| {
            workspace
                .read(cx)
                .ok()
                .and_then(|workspace| workspace.session().map(|s| s.read(cx).overrides_key()))
                .is_some_and(|key| Some(key) == graph)
        })
    });
    let Some(window) = window else {
        return;
    };
    let open = OpenConversation {
        session: session.to_owned(),
        preview: false,
    };
    window
        .update(cx, |_, window, cx| {
            window.activate_window();
            window.dispatch_action(open.boxed_clone(), cx);
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{BorrowAppContext as _, Context, IntoElement, Render, TestAppContext, Window, div};

    struct Blank;

    impl Render for Blank {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    fn attention(kind: AttentionKind, branch: Option<&str>) -> Attention {
        Attention {
            session: "s-1".into(),
            graph: "dev@http://127.0.0.1:8787/knowledge".into(),
            branch: branch.map(str::to_owned),
            kind,
        }
    }

    /// Each kind says what happened and where, naming the graph and the branch and nothing the
    /// conversation said; one tag a conversation, which a click reads back.
    #[test]
    fn a_notification_names_the_graph_and_the_branch() {
        let done = notification(&attention(AttentionKind::Finished, Some("agent/20261119")));
        assert_eq!(done.title.as_ref(), "Agent Finished");
        assert_eq!(
            done.body.as_ref(),
            "Its changes on agent/20261119 in knowledge are ready for review."
        );
        assert_eq!(done.tag.as_ref(), "agent:s-1");
        assert_eq!(session_of(&done.tag), Some("s-1"));
        assert!(
            done.actions.is_empty(),
            "a click opens it; no button only opens Studio"
        );

        let replied = notification(&attention(AttentionKind::Finished, None));
        assert_eq!(
            replied.body.as_ref(),
            "It replied in a conversation on knowledge."
        );
        let stopped = notification(&attention(AttentionKind::Stopped, Some("agent/x")));
        assert_eq!(stopped.title.as_ref(), "Agent Stopped");
        let waiting = notification(&attention(AttentionKind::Waiting, Some("agent/x")));
        assert_eq!(waiting.title.as_ref(), "Agent Is Waiting");
        assert_eq!(
            waiting.body.as_ref(),
            "It needs your answer before it changes agent/x in knowledge."
        );
        assert_eq!(session_of(&SharedString::from("agent:")), None);
        assert_eq!(session_of(&SharedString::from("other:s-1")), None);
    }

    /// A conversation that ends a turn while a Studio window is in front posts nothing; with
    /// Studio in the background it posts one notification, and a later one of the same
    /// conversation replaces it. The setting turned off posts none.
    #[gpui::test]
    fn only_studio_in_the_background_is_notified(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
            init(cx);
        });
        let (_, window) = cx.add_window_view(|_, _| Blank);
        window.update(|window, _| window.activate_window());
        window.run_until_parked();
        assert!(window.update(|window, _| window.is_window_active()));

        let record = serde_json::json!({
            "session": "s-1",
            "graph": "dev@http://127.0.0.1:8787/knowledge",
            "agent": "claude",
            "created_at": "2026-11-19T10:00:00Z",
            "updated_at": "2026-11-19T10:00:00Z",
            "branch": "agent/20261119",
            "branch_created": true,
            "pinned": false,
            "archived": false,
            "cwd": "/tmp"
        });
        window.update(|_, cx| {
            agent_panel::remember_conversation_for_test(record, cx);
            agent_panel::end_turn_for_test("s-1", AttentionKind::Finished, cx);
        });
        assert!(
            window.shown_system_notifications().is_empty(),
            "Studio in front: the tab says it"
        );

        window.deactivate_window();
        window.update(|_, cx| {
            agent_panel::end_turn_for_test("s-1", AttentionKind::Finished, cx);
            agent_panel::end_turn_for_test("s-1", AttentionKind::Waiting, cx);
        });
        let shown = window.shown_system_notifications();
        assert_eq!(shown.len(), 2);
        assert_eq!(
            shown[0].body.as_ref(),
            "Its changes on agent/20261119 in knowledge are ready for review."
        );
        let delivered = window.delivered_system_notifications();
        assert_eq!(delivered.len(), 1, "one a conversation, the newest");
        assert_eq!(delivered[0].title.as_ref(), "Agent Is Waiting");

        window.update(|_, cx| {
            cx.update_global::<StudioSettings, _>(|settings, _| {
                settings.notifications.agent_turns = false
            });
            agent_panel::end_turn_for_test("s-1", AttentionKind::Stopped, cx);
        });
        assert_eq!(window.shown_system_notifications().len(), 2, "turned off");
        // An unknown conversation is told of by nobody.
        window.update(|_, cx| {
            cx.update_global::<StudioSettings, _>(|settings, _| {
                settings.notifications.agent_turns = true
            });
            agent_panel::end_turn_for_test("s-unknown", AttentionKind::Finished, cx);
        });
        assert_eq!(window.shown_system_notifications().len(), 2);
    }

    /// A click asks the window showing the conversation's graph to open it, as a kept tab (the
    /// agent panel's own `OpenConversation`, which finds its open tab); one of a graph no
    /// window shows asks nothing. The action is caught here: a real agent tab would start an
    /// agent.
    #[gpui::test]
    fn a_click_opens_the_conversation_where_its_graph_is(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            studio_settings::init_test(cx);
            init(cx);
        });
        let (session, _) = graph_store::test_server::followed_session(
            include_str!("../../../dev/cluster/knowledge.pg"),
            |_| serde_json::json!([]),
            cx,
        );
        let (workspace, window) = cx.add_window_view(Workspace::new);
        let opened = std::rc::Rc::new(std::cell::RefCell::new(Vec::<OpenConversation>::new()));
        workspace.update_in(window, |workspace, window, cx| {
            workspace.set_session(session.clone(), window, cx);
            let opened = opened.clone();
            workspace.register_action(move |_, action: &OpenConversation, _, _| {
                opened.borrow_mut().push(action.clone())
            });
        });
        window.run_until_parked();
        let graph = window.update(|_, cx| session.read(cx).overrides_key());
        let record = |id: &str, graph: &str| {
            serde_json::json!({
                "session": id,
                "graph": graph,
                "agent": "claude",
                "created_at": "2026-11-19T10:00:00Z",
                "updated_at": "2026-11-19T10:00:00Z",
                "branch": "agent/20261119",
                "branch_created": true,
                "pinned": false,
                "archived": false,
                "cwd": "/tmp"
            })
        };
        window.update(|_, cx| {
            agent_panel::remember_conversation_for_test(record("s-here", &graph), cx);
            agent_panel::remember_conversation_for_test(record("s-elsewhere", "x@y/other"), cx);
        });
        window.simulate_system_notification_response(SystemNotificationResponse {
            tag: "agent:s-elsewhere".into(),
            action_id: None,
        });
        window.run_until_parked();
        assert!(opened.borrow().is_empty(), "no window shows that graph");

        window.simulate_system_notification_response(SystemNotificationResponse {
            tag: "agent:s-here".into(),
            action_id: None,
        });
        window.run_until_parked();
        assert_eq!(
            *opened.borrow(),
            [OpenConversation {
                session: "s-here".into(),
                preview: false
            }]
        );
    }
}

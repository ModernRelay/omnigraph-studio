//! Each UI tool of the first set (spec 032), called as an agent calls it, against every
//! feature's real handler: what it answers is what opened in the window.

use gpui::{TestAppContext, VisualTestContext};
use serde_json::{Value, json};
use studio_workspace::Workspace;
use studio_workspace::ui_tools::{self, Answer, Caller, Target};

/// A window with every feature, showing the dev graph's schema on a fake server.
fn studio(cx: &mut TestAppContext) -> (gpui::Entity<Workspace>, &mut VisualTestContext, u64) {
    cx.update(|cx| {
        studio_theme::init_test(cx);
        studio_settings::init_test(cx);
        editor::init(cx);
        command_palette::init(cx);
        type_table::init(cx);
        relationship_tree::init(cx);
        graph_canvas::init(cx);
        query_editor::init(cx);
        review::init(cx);
        history::init(cx);
        agent_panel::init(cx);
    });
    let (session, _) = graph_store::test_server::followed_session(
        include_str!("../../../dev/cluster/knowledge.pg"),
        |_| json!([]),
        cx,
    );
    let (workspace, cx) = cx.add_window_view(Workspace::new);
    let id = gpui::VisualContext::window_handle(cx).window_id().as_u64();
    // The activity log's panel, as `studio::init` adds it to every window.
    workspace.update_in(cx, |workspace, window, cx| {
        let handle = cx.entity().downgrade();
        let session = workspace.session().cloned();
        let activity = gpui::AppContext::new(cx, |cx| {
            crate::activity::ActivityLog::new(handle, session, window, cx)
        });
        workspace.set_activity_view(activity.into(), cx);
    });
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.set_session(session, window, cx)
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    (workspace, cx, id)
}

async fn run(cx: &mut VisualTestContext, id: u64, name: &str, mut input: Value) -> Answer {
    input["window"] = json!(id);
    let caller = Caller {
        agent: Some("Claude Code".into()),
        announce: false,
    };
    let answer = ui_tools::call(&mut cx.to_async(), Target::Any, caller, name, input).await;
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    answer
}

/// The tab the answer names: its thing and view.
fn opened(answer: &Answer) -> (Value, Value) {
    assert_eq!(answer.outcome, "opened", "{answer:?}");
    assert!(!answer.is_error);
    (
        answer.json["tab"]["thing"].clone(),
        answer.json["tab"]["view"].clone(),
    )
}

fn node(id: &str) -> Value {
    json!({ "kind": "node", "type_name": "Person", "id": id })
}

#[gpui::test]
async fn graph_open_type_opens_the_table(cx: &mut TestAppContext) {
    let (_, cx, id) = studio(cx);
    let answer = run(cx, id, "graph_open_type", json!({ "type_name": "Person" })).await;
    assert_eq!(
        opened(&answer),
        (
            json!({ "kind": "type", "type_name": "Person" }),
            json!("Table")
        )
    );
    assert_eq!(answer.says, "Opened Person · Table");
    let again = run(cx, id, "graph_open_type", json!({ "type_name": "Person" })).await;
    assert_eq!(again.outcome, "no_change");
    // A filter would change the open table's: refused, and the table is as it was.
    let filtered = json!({ "type_name": "Person", "filter_id": "person-000000" });
    let refused = run(cx, id, "graph_open_type", filtered).await;
    assert_eq!(refused.outcome, "not_available");
    assert!(
        refused
            .says
            .starts_with("A Person table is open in this window")
    );
    let wrong = run(cx, id, "graph_open_type", json!({ "type_name": "Planet" })).await;
    assert_eq!(wrong.outcome, "not_available");
    assert_eq!(wrong.says, "Planet is not a node or edge type of knowledge");
}

#[gpui::test]
async fn graph_open_node_page_opens_the_page(cx: &mut TestAppContext) {
    let (_, cx, id) = studio(cx);
    let input = json!({ "type_name": "Person", "id": "person-000000" });
    let answer = run(cx, id, "graph_open_node_page", input).await;
    assert_eq!(opened(&answer), (node("person-000000"), json!("Page")));
}

#[gpui::test]
async fn graph_show_node_on_canvas_opens_a_walk(cx: &mut TestAppContext) {
    let (_, cx, id) = studio(cx);
    let input = json!({ "type_name": "Person", "id": "person-000000" });
    let answer = run(cx, id, "graph_show_node_on_canvas", input).await;
    assert_eq!(opened(&answer), (node("person-000000"), json!("Canvas")));
    let selection = run(cx, id, "studio_selection", json!({})).await;
    assert_eq!(selection.json["selection"]["root"]["id"], "person-000000");
}

#[gpui::test]
async fn graph_show_node_in_tree_opens_a_tree(cx: &mut TestAppContext) {
    let (_, cx, id) = studio(cx);
    let input = json!({ "type_name": "Person", "id": "person-000000" });
    let answer = run(cx, id, "graph_show_node_in_tree", input).await;
    assert_eq!(opened(&answer), (node("person-000000"), json!("Tree")));
}

#[gpui::test]
async fn graph_open_map_and_source_open_the_schema(cx: &mut TestAppContext) {
    let (_, cx, id) = studio(cx);
    let map = run(cx, id, "graph_open_map", json!({})).await;
    assert_eq!(opened(&map), (json!({ "kind": "schema" }), json!("Map")));
    let source = run(
        cx,
        id,
        "graph_open_source",
        json!({ "type_name": "Person" }),
    )
    .await;
    assert_eq!(opened(&source).1, json!("Source"));
}

#[gpui::test]
async fn graph_open_branch_and_its_history_open_main(cx: &mut TestAppContext) {
    let (_, cx, id) = studio(cx);
    // Main merges into nothing, so its branch opens as its history.
    let branch = run(cx, id, "graph_open_branch", json!({ "branch": "main" })).await;
    assert_eq!(
        opened(&branch),
        (
            json!({ "kind": "branch", "branch": "main" }),
            json!("History")
        )
    );
    let again = run(
        cx,
        id,
        "graph_open_branch_history",
        json!({ "branch": "main" }),
    )
    .await;
    assert_eq!(again.outcome, "no_change", "the same history");
    let missing = run(cx, id, "graph_open_branch", json!({ "branch": "nope" })).await;
    assert_eq!(missing.outcome, "not_available");
}

#[gpui::test]
async fn graph_open_commit_opens_the_commit(cx: &mut TestAppContext) {
    let (_, cx, id) = studio(cx);
    let answer = run(cx, id, "graph_open_commit", json!({ "commit_id": "c1" })).await;
    assert_eq!(answer.outcome, "opened", "{answer:?}");
}

#[gpui::test]
async fn graph_new_query_with_text_opens_a_query_it_does_not_run(cx: &mut TestAppContext) {
    let (workspace, cx, id) = studio(cx);
    let text = "query people() { match { $p: Person } return { $p.slug } }";
    let answer = run(cx, id, "graph_new_query_with_text", json!({ "text": text })).await;
    assert_eq!(answer.outcome, "opened", "{answer:?}");
    workspace.read_with(cx, |workspace, cx| {
        let session = workspace.session().unwrap().read(cx);
        assert_eq!(session.reads_in_flight, 0, "not run");
    });
}

#[gpui::test]
async fn workspace_activate_panel_shows_a_panel(cx: &mut TestAppContext) {
    let (workspace, cx, id) = studio(cx);
    // The action's doc comment, the tool's description, names every panel and no other.
    let panels = workspace.read_with(cx, |workspace, _| {
        let mut ids: Vec<String> = workspace
            .panel_ids()
            .iter()
            .map(|id| id.to_string())
            .collect();
        ids.sort();
        ids
    });
    let documented =
        cx.update(|_, cx| cx.action_documentation()["studio::ActivatePanel"].to_owned());
    let mut named: Vec<String> = documented
        .split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect();
    named.sort();
    assert_eq!(named, panels);
    let answer = run(
        cx,
        id,
        "workspace_activate_panel",
        json!({ "id": "branches" }),
    )
    .await;
    assert_eq!(answer.outcome, "panel_shown", "{answer:?}");
    assert_eq!(answer.json["panel"], "branches");
}

#[gpui::test]
async fn graph_open_node_opens_its_page(cx: &mut TestAppContext) {
    let (_, cx, id) = studio(cx);
    let input = json!({ "type_name": "Person", "id": "person-000000" });
    let answer = run(cx, id, "graph_open_node", input).await;
    assert_eq!(opened(&answer), (node("person-000000"), json!("Page")));
}

/// Showing a panel leaves the keyboard where the person had it: in the query they were typing,
/// though the queries panel takes it when shown by its key.
#[gpui::test]
async fn workspace_activate_panel_leaves_the_keyboard_where_it_was(cx: &mut TestAppContext) {
    let (_, cx, id) = studio(cx);
    let text = "query people() { match { $p: Person } return { $p.slug } }";
    let opened = run(cx, id, "graph_new_query_with_text", json!({ "text": text })).await;
    assert_eq!(opened.outcome, "opened", "{opened:?}");
    let typing = cx
        .update(|window, cx| window.focused(cx))
        .expect("the query has the keys");
    let answer = run(
        cx,
        id,
        "workspace_activate_panel",
        json!({ "id": "queries" }),
    )
    .await;
    assert_eq!(answer.outcome, "panel_shown", "{answer:?}");
    let focused = cx.update(|window, cx| window.focused(cx));
    assert_eq!(focused.as_ref(), Some(&typing), "the keyboard stayed");
}

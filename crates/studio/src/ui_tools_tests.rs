//! Each UI tool of the first set (spec 032), called as an agent calls it, against every
//! feature's real handler: what it answers is what opened in the window.

use gpui::{TestAppContext, VisualTestContext};
use serde_json::{Value, json};
use studio_workspace::Workspace;
use ui_tool_server::tools::{self as ui_tools, Answer, Caller, Target};

/// A window with every feature, showing the dev graph's schema on a fake server.
fn studio(cx: &mut TestAppContext) -> (gpui::Entity<Workspace>, &mut VisualTestContext, u64) {
    let (workspace, cx, id, _) = studio_served(cx);
    (workspace, cx, id)
}

/// [`studio`], with what the fake server serves.
fn studio_served(
    cx: &mut TestAppContext,
) -> (
    gpui::Entity<Workspace>,
    &mut VisualTestContext,
    u64,
    std::sync::Arc<std::sync::Mutex<graph_store::test_server::Served>>,
) {
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
    let (session, served) = graph_store::test_server::followed_session(
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
    (workspace, cx, id, served)
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
    // With a table of the type open, a filtered one opens as a tab of its own, and the open
    // table is left as the person has it; the same filter again shows that tab.
    let filtered = json!({ "type_name": "Person", "filter_id": "person-000000" });
    let opened = run(cx, id, "graph_open_type", filtered.clone()).await;
    assert_eq!(opened.outcome, "opened", "{opened:?}");
    let tabs = run(cx, id, "studio_tabs", json!({})).await;
    assert_eq!(tabs.json["panes"][0]["tabs"].as_array().unwrap().len(), 2);
    let again = run(cx, id, "graph_open_type", filtered).await;
    assert_eq!(again.outcome, "no_change", "{again:?}");
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

/// An agent that has just made a branch names it before Studio's poll has listed it: the
/// window reads its branch list once and opens it (spec 043). A branch still not listed after
/// that read is refused as before.
#[gpui::test]
async fn graph_open_branch_reads_the_branches_for_one_just_made(cx: &mut TestAppContext) {
    let (workspace, cx, id, served) = studio_served(cx);
    served.lock().unwrap().branches.push("agent/new".into());
    let answer = run(
        cx,
        id,
        "graph_open_branch",
        json!({ "branch": "agent/new" }),
    )
    .await;
    assert_eq!(
        opened(&answer),
        (
            json!({ "kind": "branch", "branch": "agent/new" }),
            json!("Review")
        )
    );
    workspace.read_with(cx, |workspace, cx| {
        let session = workspace.session().unwrap().read(cx);
        assert!(
            session
                .branches
                .loaded()
                .unwrap()
                .contains(&"agent/new".to_owned())
        );
    });
    let missing = run(
        cx,
        id,
        "graph_open_branch",
        json!({ "branch": "agent/none" }),
    )
    .await;
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

/// Keys in a panel the tool hides do not stay there: shown in the same dock, the Data panel
/// takes the queries panel's place, and the keyboard goes to the active tab rather than back to
/// the queries panel nobody sees.
#[gpui::test]
async fn workspace_activate_panel_moves_keys_out_of_a_panel_it_hides(cx: &mut TestAppContext) {
    let (workspace, cx, id) = studio(cx);
    let opened = run(cx, id, "graph_open_type", json!({ "type_name": "Person" })).await;
    assert_eq!(opened.outcome, "opened", "{opened:?}");
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.show_panel("queries", window, cx)
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    let in_queries = cx
        .update(|window, cx| window.focused(cx))
        .expect("the panel's keys");
    let answer = run(cx, id, "workspace_activate_panel", json!({ "id": "types" })).await;
    assert_eq!(answer.outcome, "panel_shown", "{answer:?}");
    let (focused, tab) = workspace.update_in(cx, |workspace, window, cx| {
        let tab = workspace.active_item().map(|item| item.focus_handle(cx));
        (window.focused(cx), tab)
    });
    assert_ne!(focused.as_ref(), Some(&in_queries), "not the hidden panel");
    assert_eq!(focused, tab, "the active tab");
}

/// Gives the queries panel the keyboard, as its key does, and says what holds it.
fn typing_in_the_queries_panel(
    workspace: &gpui::Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> gpui::FocusHandle {
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.show_panel("queries", window, cx)
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    cx.update(|window, cx| window.focused(cx))
        .expect("the panel's keys")
}

/// A tool that opens a tab leaves the keyboard where the person had it. Opening a tab gives
/// the tab the keyboard, as a pick does, so what the person typed next went to the table: `j`
/// and `k` moved its cursor, ↵ edited the cell under it, and a second ↵ saved it.
#[gpui::test]
async fn opening_a_tab_leaves_the_keyboard_where_it_was(cx: &mut TestAppContext) {
    let (workspace, cx, id) = studio(cx);
    let typing = typing_in_the_queries_panel(&workspace, cx);
    for (tool, input) in [
        ("graph_open_type", json!({ "type_name": "Person" })),
        (
            "graph_open_node_page",
            json!({ "type_name": "Person", "id": "person-000000" }),
        ),
        ("graph_open_map", json!({})),
    ] {
        let answer = run(cx, id, tool, input).await;
        assert_eq!(answer.outcome, "opened", "{tool}: {answer:?}");
        let focused = cx.update(|window, cx| window.focused(cx));
        assert_eq!(focused.as_ref(), Some(&typing), "{tool} took the keyboard");
    }
}

/// A tab put in front of the one the person was in takes the keyboard: keys must not go on
/// driving a tab nobody sees.
#[gpui::test]
async fn a_tab_put_in_front_of_the_persons_takes_the_keyboard(cx: &mut TestAppContext) {
    let (workspace, cx, id) = studio(cx);
    let answer = run(cx, id, "graph_open_type", json!({ "type_name": "Person" })).await;
    assert_eq!(answer.outcome, "opened", "{answer:?}");
    // The person works in the table.
    let table = workspace.update_in(cx, |workspace, window, cx| {
        let table = workspace.active_item().map(|item| item.focus_handle(cx));
        let table = table.expect("the table is in front");
        window.focus(&table, cx);
        table
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    let answer = run(cx, id, "graph_open_map", json!({})).await;
    assert_eq!(answer.outcome, "opened", "{answer:?}");
    let (focused, front) = workspace.update_in(cx, |workspace, window, cx| {
        let front = workspace.active_item().map(|item| item.focus_handle(cx));
        (window.focused(cx), front.expect("the map is in front"))
    });
    let focused = focused.expect("something has the keyboard");
    assert_ne!(focused, table, "not the table behind the map");
    assert!(
        cx.update(|window, cx| front.contains_focused(window, cx)),
        "the map in front has the keyboard"
    );
}

/// A canvas tool acts on the canvas and leaves the keyboard where the person has it. That the
/// canvas never holds it, even while the call runs, is
/// `a_view_tool_acts_on_its_tab_without_the_keyboard` in `ui_tool_server`.
#[gpui::test]
async fn a_view_tool_leaves_the_keyboard_where_it_was(cx: &mut TestAppContext) {
    let (workspace, cx, id) = studio(cx);
    let input = json!({ "type_name": "Person", "id": "person-000000" });
    let answer = run(cx, id, "graph_show_node_on_canvas", input).await;
    assert_eq!(answer.outcome, "opened", "{answer:?}");
    let typing = typing_in_the_queries_panel(&workspace, cx);
    for tool in [
        "canvas_zoom_in",
        "canvas_select_node",
        "canvas_fit_to_window",
    ] {
        let answer = run(cx, id, tool, json!({})).await;
        assert!(!answer.is_error, "{tool}: {answer:?}");
        let focused = cx.update(|window, cx| window.focused(cx));
        assert_eq!(focused.as_ref(), Some(&typing), "{tool} took the keyboard");
    }
    let zoomed = run(cx, id, "studio_canvas", json!({})).await;
    assert!(
        zoomed.json["zoom"].as_f64().is_some_and(|zoom| zoom != 1.),
        "the canvas was driven: {:?}",
        zoomed.json["zoom"]
    );
}

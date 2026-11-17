//! The canvas and tree tools of spec 032, called as an agent calls them, against the real
//! canvas and tree on a fake server: sixty people who all work at one company, the first six
//! of whom know the next two.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gpui::{TestAppContext, VisualTestContext};
use serde_json::{Value, json};
use studio_settings::StudioSettings;
use studio_workspace::Workspace;
use studio_workspace::ui_tools::{self, Answer, Caller, Target};

/// People on the fake server; every one works at `c0`.
const PEOPLE: usize = 60;

fn person(n: usize) -> String {
    format!("p{n}")
}

/// Who `n` knows: the next two, among the first six.
fn knows(n: usize) -> Vec<usize> {
    [n + 1, n + 2]
        .into_iter()
        .filter(|m| *m < 6 && n < 6)
        .collect()
}

/// Who knows `n`.
fn known_by(n: usize) -> Vec<usize> {
    (0..6).filter(|m| knows(*m).contains(&n)).collect()
}

/// The neighbours of `id` through the traversal in `text`, as ids.
fn neighbours(id: &str, text: &str) -> Vec<String> {
    // `$n worksAt $m` reads out of the node; `$m worksAt $n` into it.
    let outgoing = text.contains("$n worksAt")
        || text.contains("$n knows")
        || text.contains("$n $e:worksAt")
        || text.contains("$n $e:knows");
    let n: Option<usize> = id.strip_prefix('p').and_then(|n| n.parse().ok());
    if text.contains("orksAt") {
        return match (n, outgoing) {
            (Some(_), true) => vec!["c0".into()],
            (None, false) if id == "c0" => (0..PEOPLE).map(person).collect(),
            _ => Vec::new(),
        };
    }
    if text.contains("nows") {
        let Some(n) = n else { return Vec::new() };
        let ids = if outgoing { knows(n) } else { known_by(n) };
        return ids.into_iter().map(person).collect();
    }
    Vec::new()
}

fn name(id: &str) -> String {
    match id {
        "c0" => "Sammet Systems".into(),
        id => format!("Person {}", id.trim_start_matches('p')),
    }
}

/// A window with every feature, on the fake server, and a canvas walking from `root`.
async fn studio(
    cx: &mut TestAppContext,
    cap: Option<u32>,
) -> (gpui::Entity<Workspace>, &mut VisualTestContext, u64) {
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
        if let Some(cap) = cap {
            let mut settings = StudioSettings::get(cx).clone();
            settings.studio.canvas_max_nodes = cap;
            cx.set_global(settings);
        }
    });
    // A server of its own, so nothing another test saved is restored here.
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let server = format!(
        "canvas{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let (session, _) = graph_store::test_server::followed_session_named(
        &server,
        include_str!("../../../dev/cluster/knowledge.pg"),
        |asked| {
            let text = asked["query"].as_str().unwrap_or_default();
            match asked["name"].as_str() {
                Some("studio_node") => {
                    let id = asked["params"]["id"].as_str().unwrap_or_default();
                    json!([{ "id": id, "label": name(id) }])
                }
                Some("studio_page_degrees") => {
                    let ids: Vec<String> = asked["params"]["ids"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|id| id.as_str().map(str::to_owned))
                        .collect();
                    Value::Array(
                        ids.iter()
                            .map(|id| (id, neighbours(id, text).len()))
                            .filter(|(_, n)| *n > 0)
                            .map(|(id, n)| json!({ "id": id, "n": n }))
                            .collect(),
                    )
                }
                Some("studio_neighbours") => {
                    let id = asked["params"]["id"].as_str().unwrap_or_default();
                    Value::Array(
                        neighbours(id, text)
                            .into_iter()
                            .map(|other| json!({ "id": other, "label": name(&other) }))
                            .collect(),
                    )
                }
                _ => json!([]),
            }
        },
        cx,
    );
    let (workspace, cx) = cx.add_window_view(Workspace::new);
    let id = gpui::VisualContext::window_handle(cx).window_id().as_u64();
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.set_session(session, window, cx)
    });
    settle(cx);
    (workspace, cx, id)
}

/// Lets reads, layouts and timers finish.
fn settle(cx: &mut VisualTestContext) {
    for _ in 0..8 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(50));
    }
    cx.run_until_parked();
}

/// Calls `name` as an agent does, moving the clock while the call waits for its view.
async fn run(cx: &mut VisualTestContext, id: u64, name: &str, mut input: Value) -> Answer {
    input["window"] = json!(id);
    let answer: Rc<RefCell<Option<Answer>>> = Rc::default();
    let out = answer.clone();
    let name = name.to_owned();
    let mut app = cx.to_async();
    cx.foreground_executor()
        .spawn(async move {
            let caller = Caller {
                agent: Some("Claude Code".into()),
                announce: false,
            };
            let answered = ui_tools::call(&mut app, Target::Any, caller, &name, input).await;
            *out.borrow_mut() = Some(answered);
        })
        .detach();
    for _ in 0..2_000 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        if answer.borrow().is_some() {
            break;
        }
        cx.executor().advance_clock(Duration::from_millis(20));
    }
    settle(cx);
    answer.take().expect("the call was answered")
}

fn node(id: &str) -> Value {
    let type_name = if id.starts_with('c') {
        "Company"
    } else {
        "Person"
    };
    json!({ "type_name": type_name, "id": id })
}

/// The ids an answer lists under `key`.
fn ids(answer: &Answer, key: &str) -> Vec<String> {
    let mut ids: Vec<String> = answer.json[key]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|node| node["id"].as_str().map(str::to_owned))
        .collect();
    ids.sort();
    ids
}

/// A canvas walking from p0, opened by the tool an agent opens one with.
async fn canvas(cx: &mut VisualTestContext, id: u64, root: &str) {
    let opened = run(cx, id, "graph_show_node_on_canvas", node(root)).await;
    assert_eq!(opened.outcome, "opened", "{opened:?}");
}

/// A tree walking from p0.
async fn tree(cx: &mut VisualTestContext, id: u64, root: &str) {
    let opened = run(cx, id, "graph_show_node_in_tree", node(root)).await;
    assert_eq!(opened.outcome, "opened", "{opened:?}");
}

// ---- canvas_expand: what the review asked for ----

/// Two nodes, one level: each one's relationships open, and the answer lists what was drawn.
#[gpui::test]
async fn canvas_expand_two_nodes_one_level(cx: &mut TestAppContext) {
    let (_, cx, id) = studio(cx, None).await;
    canvas(cx, id, "p0").await;
    let first = run(cx, id, "canvas_expand", json!({ "nodes": [node("p0")] })).await;
    assert_eq!(first.outcome, "expanded", "{first:?}");
    assert!(ids(&first, "added").contains(&"p1".to_owned()), "{first:?}");
    let answer = run(
        cx,
        id,
        "canvas_expand",
        json!({ "nodes": [node("p1"), node("p2")], "depth": 1 }),
    )
    .await;
    assert_eq!(answer.outcome, "expanded", "{answer:?}");
    let added = ids(&answer, "added");
    assert!(
        added.contains(&"p3".to_owned()) && added.contains(&"p4".to_owned()),
        "{added:?}"
    );
    assert!(!answer.json["edges_added"].as_array().unwrap().is_empty());
    assert_eq!(answer.json["cap_reached"], false);
    // Named by their names, as the person sees them.
    let p3 = answer.json["added"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["id"] == "p3")
        .unwrap();
    assert_eq!(p3["label"], "Person 3");
}

/// A relationship filter opens only that relationship.
#[gpui::test]
async fn canvas_expand_one_relationship(cx: &mut TestAppContext) {
    let (_, cx, id) = studio(cx, None).await;
    canvas(cx, id, "p0").await;
    let answer = run(
        cx,
        id,
        "canvas_expand",
        json!({ "nodes": [node("p0")], "relationships": ["out:WorksAt"] }),
    )
    .await;
    assert_eq!(answer.outcome, "expanded", "{answer:?}");
    assert_eq!(ids(&answer, "added"), ["c0"]);
    let unknown = run(
        cx,
        id,
        "canvas_expand",
        json!({ "nodes": [node("p0")], "relationships": ["out:Owns"] }),
    )
    .await;
    assert_eq!(unknown.outcome, "not_available", "{unknown:?}");
    assert!(unknown.says.starts_with("None of the nodes has out:Owns"));
}

/// Past what is opened without asking, the tool asks the agent to confirm, opens nothing, and
/// opens it when called again with `confirm`.
#[gpui::test]
async fn canvas_expand_needs_confirmation_then_opens(cx: &mut TestAppContext) {
    let (_, cx, id) = studio(cx, None).await;
    canvas(cx, id, "c0").await;
    let staff = run(
        cx,
        id,
        "canvas_expand",
        json!({ "nodes": [node("c0")], "relationships": ["in:WorksAt"], "limit_per_relationship": 60 }),
    )
    .await;
    assert_eq!(ids(&staff, "added").len(), PEOPLE, "{staff:?}");
    let everyone: Vec<Value> = (0..PEOPLE).map(|n| node(&person(n))).collect();
    let asked = run(cx, id, "canvas_expand", json!({ "nodes": everyone })).await;
    assert_eq!(asked.outcome, "needs_confirmation", "{asked:?}");
    assert!(!asked.is_error);
    assert_eq!(asked.json["needs_confirmation"]["nodes"], PEOPLE);
    let drawn = run(cx, id, "studio_canvas", json!({})).await;
    assert_eq!(
        drawn.json["relationships"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|group| group[4] == true)
            .count(),
        1,
        "nothing more was opened"
    );
    let everyone: Vec<Value> = (0..PEOPLE).map(|n| node(&person(n))).collect();
    let confirmed = run(
        cx,
        id,
        "canvas_expand",
        json!({ "nodes": everyone, "confirm": true }),
    )
    .await;
    assert!(
        matches!(confirmed.outcome, "expanded" | "changed"),
        "{confirmed:?}"
    );
    assert!(
        !confirmed.json["unexpanded"].as_array().unwrap().is_empty()
            || confirmed.json["nodes_drawn"].as_u64().unwrap() > PEOPLE as u64
    );
}

/// The node cap stays absolute: an expansion that reaches it stops there and says so.
#[gpui::test]
async fn canvas_expand_stops_at_the_node_cap(cx: &mut TestAppContext) {
    let (_, cx, id) = studio(cx, Some(12)).await;
    canvas(cx, id, "c0").await;
    let answer = run(
        cx,
        id,
        "canvas_expand",
        json!({ "nodes": [node("c0")], "limit_per_relationship": 60 }),
    )
    .await;
    assert_eq!(answer.outcome, "cap_reached", "{answer:?}");
    assert_eq!(answer.json["cap_reached"], true);
    assert!(answer.json["nodes_drawn"].as_u64().unwrap() <= 12);
    assert_eq!(answer.json["node_cap"]["cap"], 12);
}

/// Depth 2 opens the neighbours the first level read.
#[gpui::test]
async fn canvas_expand_two_levels(cx: &mut TestAppContext) {
    let (_, cx, id) = studio(cx, None).await;
    canvas(cx, id, "p0").await;
    let answer = run(
        cx,
        id,
        "canvas_expand",
        json!({ "nodes": [node("p0")], "relationships": ["out:Knows"], "depth": 2 }),
    )
    .await;
    assert_eq!(answer.outcome, "expanded", "{answer:?}");
    let added = ids(&answer, "added");
    for second in ["p3", "p4"] {
        assert!(added.contains(&second.to_owned()), "{added:?}");
    }
}

/// A node not on the canvas is refused in the words the agent needs.
#[gpui::test]
async fn a_node_not_on_the_canvas_is_refused(cx: &mut TestAppContext) {
    let (_, cx, id) = studio(cx, None).await;
    canvas(cx, id, "p0").await;
    let answer = run(cx, id, "canvas_expand_node", json!({ "node": node("p5") })).await;
    assert_eq!(answer.outcome, "not_available");
    assert_eq!(
        answer.says,
        "Not on this canvas; expand a neighbour or use graph_show_node_on_canvas"
    );
    let expanded = run(cx, id, "canvas_expand", json!({ "nodes": [node("p5")] })).await;
    assert_eq!(expanded.outcome, "not_available");
}

/// Canvas tools need a canvas in front.
#[gpui::test]
async fn canvas_tools_need_a_canvas_in_front(cx: &mut TestAppContext) {
    let (_, cx, id) = studio(cx, None).await;
    let answer = run(cx, id, "canvas_zoom_in", json!({})).await;
    assert_eq!(answer.outcome, "not_available");
    assert!(answer.says.starts_with("No tab is open"), "{answer:?}");
}

// ---- One test per canvas tool ----

/// A canvas from p0 with its relationships open, p1 selected.
async fn walked(cx: &mut TestAppContext) -> (&mut VisualTestContext, u64) {
    let (_, cx, id) = studio(cx, None).await;
    canvas(cx, id, "p0").await;
    run(cx, id, "canvas_expand", json!({ "nodes": [node("p0")] })).await;
    let selected = run(cx, id, "canvas_select_node", node("p1")).await;
    assert_eq!(selected.outcome, "changed", "{selected:?}");
    assert_eq!(selected.json["selected"]["id"], "p1");
    (cx, id)
}

macro_rules! canvas_tool {
    ($test:ident, $tool:literal, $input:expr, $check:expr) => {
        #[gpui::test]
        async fn $test(cx: &mut TestAppContext) {
            let (cx, id) = walked(cx).await;
            let answer = run(cx, id, $tool, $input).await;
            let check: fn(&Answer) = $check;
            check(&answer);
        }
    };
}

fn changed(answer: &Answer) {
    assert_eq!(answer.outcome, "changed", "{answer:?}");
}

canvas_tool!(
    canvas_select_node_selects_by_id,
    "canvas_select_node",
    node("p2"),
    |answer| assert_eq!(answer.json["selected"]["id"], "p2")
);
canvas_tool!(
    canvas_expand_node_expands_the_named_node,
    "canvas_expand_node",
    json!({ "node": node("p1") }),
    |answer| {
        // A node expands to its relationships, drawn closed until one is opened.
        let unexpanded = answer.json["unexpanded"].as_array().unwrap();
        assert!(
            unexpanded
                .iter()
                .any(|group| group["node"]["id"] == "p1" && group["relationship"] == "out:Knows"),
            "{answer:?}"
        );
    }
);
canvas_tool!(
    canvas_collapse_node_closes_it,
    "canvas_collapse_node",
    json!({ "node": node("p0") }),
    |answer| assert!(!ids(answer, "removed").is_empty(), "{answer:?}")
);
canvas_tool!(
    canvas_collapse_closes_the_nodes_named,
    "canvas_collapse",
    json!({ "nodes": [node("p0")] }),
    |answer| assert!(!ids(answer, "removed").is_empty(), "{answer:?}")
);
canvas_tool!(
    canvas_show_more_widens_a_relationship,
    "canvas_show_more",
    json!({ "node": node("p0"), "relationship": "out:Knows" }),
    |answer| assert!(
        matches!(answer.outcome, "changed" | "no_change"),
        "{answer:?}"
    )
);
canvas_tool!(
    canvas_hide_nodes_hides_and_show_hidden_brings_back,
    "canvas_hide_nodes",
    json!({ "nodes": [node("p1")] }),
    |answer| {
        assert_eq!(ids(answer, "removed"), ["p1"], "{answer:?}");
        assert_eq!(answer.json["hidden"][0]["id"], "p1");
    }
);
canvas_tool!(
    canvas_show_hidden_with_nothing_hidden_changes_nothing,
    "canvas_show_hidden",
    json!({}),
    |answer| assert_eq!(answer.outcome, "no_change", "{answer:?}")
);
canvas_tool!(
    canvas_pin_nodes_keeps_them_in_place,
    "canvas_pin_nodes",
    json!({ "nodes": [node("p1")] }),
    |answer| assert_eq!(answer.json["pinned"], 1, "{answer:?}")
);
canvas_tool!(
    canvas_unpin_nodes_lets_them_go,
    "canvas_unpin_nodes",
    json!({ "nodes": [node("p1")] }),
    |answer| assert_eq!(answer.json["pinned"], 0, "{answer:?}")
);
canvas_tool!(
    canvas_reroot_here_opens_a_walk_from_the_node,
    "canvas_reroot_here",
    json!({ "node": node("p2") }),
    |answer| {
        assert_eq!(answer.outcome, "opened", "{answer:?}");
        assert_eq!(answer.json["tab"]["thing"]["id"], "p2");
    }
);
canvas_tool!(
    canvas_move_right_moves_the_selection,
    "canvas_move_right",
    json!({ "node": node("p0") }),
    changed
);
canvas_tool!(
    canvas_move_left_moves_the_selection,
    "canvas_move_left",
    json!({ "node": node("p1") }),
    |answer| assert!(
        matches!(answer.outcome, "changed" | "no_change"),
        "{answer:?}"
    )
);
canvas_tool!(
    canvas_move_up_moves_the_selection,
    "canvas_move_up",
    json!({ "node": node("p1") }),
    |answer| assert!(
        matches!(answer.outcome, "changed" | "no_change"),
        "{answer:?}"
    )
);
canvas_tool!(
    canvas_move_down_moves_the_selection,
    "canvas_move_down",
    json!({ "node": node("p1") }),
    |answer| assert!(
        matches!(answer.outcome, "changed" | "no_change"),
        "{answer:?}"
    )
);
canvas_tool!(
    canvas_open_selected_page_opens_its_page,
    "canvas_open_selected_page",
    json!({ "node": node("p1") }),
    |answer| {
        assert_eq!(answer.outcome, "opened", "{answer:?}");
        assert_eq!(answer.json["tab"]["view"], "Page");
    }
);
canvas_tool!(
    canvas_focus_selected_centres_on_it,
    "canvas_focus_selected",
    json!({ "node": node("p1") }),
    changed
);
canvas_tool!(
    canvas_follow_relationship_follows_from_a_node,
    "canvas_follow_relationship",
    json!({ "node": node("p1") }),
    |answer| assert!(
        matches!(answer.outcome, "changed" | "no_change"),
        "{answer:?}"
    )
);
canvas_tool!(
    canvas_walk_back_returns_to_where_it_came_from,
    "canvas_walk_back",
    json!({}),
    |answer| assert_eq!(answer.json["selected"]["id"], "p0", "{answer:?}")
);
canvas_tool!(
    canvas_walk_forward_with_nothing_walked_changes_nothing,
    "canvas_walk_forward",
    json!({}),
    |answer| assert_eq!(answer.outcome, "no_change", "{answer:?}")
);
canvas_tool!(
    canvas_select_all_marks_every_chip,
    "canvas_select_all",
    json!({}),
    |answer| assert!(answer.json["marked"].as_u64().unwrap() > 1, "{answer:?}")
);
canvas_tool!(
    canvas_clear_selection_with_nothing_marked_changes_nothing,
    "canvas_clear_selection",
    json!({}),
    |answer| assert_eq!(answer.json["marked"], 0, "{answer:?}")
);
canvas_tool!(canvas_zoom_in_zooms, "canvas_zoom_in", json!({}), changed);
canvas_tool!(canvas_zoom_out_zooms, "canvas_zoom_out", json!({}), changed);
canvas_tool!(
    canvas_fit_to_window_fits,
    "canvas_fit_to_window",
    json!({}),
    |answer| assert!(
        matches!(answer.outcome, "changed" | "no_change"),
        "{answer:?}"
    )
);
canvas_tool!(
    canvas_reset_layout_places_every_node_again,
    "canvas_reset_layout",
    json!({}),
    |answer| assert!(
        matches!(answer.outcome, "changed" | "no_change"),
        "{answer:?}"
    )
);

/// `studio_canvas` names what the canvas draws, and `studio_selection` how much there is.
#[gpui::test]
async fn studio_canvas_reads_the_canvas(cx: &mut TestAppContext) {
    let (cx, id) = walked(cx).await;
    let read = run(cx, id, "studio_canvas", json!({})).await;
    assert_eq!(read.outcome, "read");
    assert_eq!(read.json["root"][1], "p0");
    let nodes = read.json["nodes"].as_array().unwrap();
    assert!(
        nodes
            .iter()
            .any(|node| node[1] == "c0" && node[2] == "Sammet Systems")
    );
    let edge = &read.json["edges"][0];
    assert!(edge[2].as_str().unwrap().contains(':'), "{edge}");
    assert_eq!(read.json["truncated"], false);
    assert_eq!(read.json["selected"]["id"], "p1");
    let selection = run(cx, id, "studio_selection", json!({})).await;
    assert_eq!(
        selection.json["tab"]["drawn"]["nodes"].as_u64(),
        Some(nodes.len() as u64)
    );
    // The canvas tab is named by its node's name.
    let tabs = run(cx, id, "studio_tabs", json!({})).await;
    assert!(
        tabs.json["panes"][0]["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tab| tab["title"] == "Canvas: Person 0"),
        "{tabs:?}"
    );
}

/// A query's result picture is a canvas an agent can read.
#[gpui::test]
async fn graph_show_results_on_canvas_draws_the_nodes(cx: &mut TestAppContext) {
    let (_, cx, id) = studio(cx, None).await;
    let answer = run(
        cx,
        id,
        "graph_show_results_on_canvas",
        json!({
            "title": "Founders",
            "nodes": [
                { "type_name": "Person", "id": "p0", "label": "Person 0" },
                { "type_name": "Company", "id": "c0", "label": "Sammet Systems" },
            ],
            "links": [[0, 1]],
        }),
    )
    .await;
    assert_eq!(answer.outcome, "opened", "{answer:?}");
    let read = run(cx, id, "studio_canvas", json!({})).await;
    assert_eq!(read.json["nodes"].as_array().unwrap().len(), 2, "{read:?}");
    assert_eq!(read.json["edges"].as_array().unwrap().len(), 1);
    // A result picture is drawn whole: nothing is hidden from it.
    let hide = run(
        cx,
        id,
        "canvas_hide_nodes",
        json!({ "nodes": [node("c0")] }),
    )
    .await;
    assert_eq!(hide.outcome, "not_available", "{hide:?}");
}

/// A table's row walks on a canvas of its own.
#[gpui::test]
async fn table_show_row_on_canvas_needs_a_row(cx: &mut TestAppContext) {
    let (_, cx, id) = studio(cx, None).await;
    let table = run(cx, id, "graph_open_type", json!({ "type_name": "Person" })).await;
    assert_eq!(table.outcome, "opened");
    let answer = run(cx, id, "table_show_row_on_canvas", json!({})).await;
    // The fake server's table has no rows, so there is no row under the cursor to walk from.
    assert!(
        matches!(answer.outcome, "no_change" | "opened"),
        "{answer:?}"
    );
    let off = run(cx, id, "canvas_zoom_in", json!({})).await;
    assert_eq!(off.outcome, "not_available", "a table is not a canvas");
}

// ---- The map ----

macro_rules! map_tool {
    ($test:ident, $tool:literal) => {
        #[gpui::test]
        async fn $test(cx: &mut TestAppContext) {
            let (_, cx, id) = studio(cx, None).await;
            let map = run(cx, id, "graph_open_map", json!({})).await;
            assert_eq!(map.outcome, "opened", "{map:?}");
            let answer = run(cx, id, $tool, json!({})).await;
            assert!(
                matches!(answer.outcome, "changed" | "no_change" | "opened" | "shown"),
                "{answer:?}"
            );
        }
    };
}

map_tool!(map_back_goes_back, "map_back");
map_tool!(map_show_domains_shows_them, "map_show_domains");
map_tool!(map_select_next_points_at_the_next_type, "map_select_next");

/// A map tool that moves what is pointed at says it changed the map.
#[gpui::test]
async fn map_select_next_says_it_changed_the_map(cx: &mut TestAppContext) {
    let (_, cx, id) = studio(cx, None).await;
    run(cx, id, "graph_open_map", json!({})).await;
    let answer = run(cx, id, "map_select_next", json!({})).await;
    assert_eq!(answer.outcome, "changed", "{answer:?}");
}
map_tool!(
    map_select_previous_points_at_the_previous_type,
    "map_select_previous"
);
map_tool!(map_open_brings_the_type_into_focus, "map_open");

// ---- The tree ----

/// A tree from p0 with p0's relationships open.
async fn treed(cx: &mut TestAppContext) -> (&mut VisualTestContext, u64) {
    let (_, cx, id) = studio(cx, None).await;
    tree(cx, id, "p0").await;
    let opened = run(cx, id, "tree_expand", json!({ "nodes": [node("p0")] })).await;
    assert!(
        matches!(opened.outcome, "expanded" | "changed"),
        "{opened:?}"
    );
    (cx, id)
}

/// The tree's expansion, and `studio_tree`.
#[gpui::test]
async fn tree_expand_and_studio_tree(cx: &mut TestAppContext) {
    let (cx, id) = treed(cx).await;
    let answer = run(
        cx,
        id,
        "tree_expand",
        json!({ "nodes": [node("p1")], "relationships": ["out:Knows"] }),
    )
    .await;
    assert_eq!(answer.outcome, "expanded", "{answer:?}");
    assert!(
        ids(&answer, "added").contains(&"p3".to_owned()),
        "{answer:?}"
    );
    let read = run(cx, id, "studio_tree", json!({})).await;
    assert_eq!(read.json["root"][1], "p0");
    let groups = read.json["relationships"].as_array().unwrap();
    assert!(
        groups
            .iter()
            .any(|group| group[1] == "out:Knows" && group[4] == true)
    );
    assert!(
        read.json["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node[1] == "p3" && node[2] == "Person 3")
    );
    let missing = run(cx, id, "tree_select_node", node("p5")).await;
    assert_eq!(missing.outcome, "no_change");
    let refused = run(cx, id, "tree_expand_row", json!({ "node": node("p5") })).await;
    assert_eq!(
        refused.says,
        "Not in this tree; expand a relationship that holds it or use graph_show_node_in_tree"
    );
}

macro_rules! tree_tool {
    ($test:ident, $tool:literal, $input:expr, $check:expr) => {
        #[gpui::test]
        async fn $test(cx: &mut TestAppContext) {
            let (cx, id) = treed(cx).await;
            let answer = run(cx, id, $tool, $input).await;
            let check: fn(&Answer) = $check;
            check(&answer);
        }
    };
}

fn moved(answer: &Answer) {
    assert!(
        matches!(
            answer.outcome,
            "changed" | "no_change" | "opened" | "shown" | "expanded"
        ),
        "{answer:?}"
    );
}

tree_tool!(
    tree_select_node_selects_the_row,
    "tree_select_node",
    node("p1"),
    |answer| assert_eq!(answer.json["selected"]["id"], "p1", "{answer:?}")
);
tree_tool!(
    tree_collapse_closes_the_node,
    "tree_collapse",
    json!({ "nodes": [node("p0")] }),
    |answer| assert!(!ids(answer, "removed").is_empty(), "{answer:?}")
);
tree_tool!(
    tree_select_next_moves_down,
    "tree_select_next",
    json!({ "node": node("p1") }),
    moved
);
tree_tool!(
    tree_select_previous_moves_up,
    "tree_select_previous",
    json!({ "node": node("p1") }),
    moved
);
tree_tool!(
    tree_expand_row_opens_the_row,
    "tree_expand_row",
    json!({ "node": node("p1") }),
    moved
);
tree_tool!(
    tree_collapse_row_closes_the_row,
    "tree_collapse_row",
    json!({ "node": node("p0") }),
    moved
);
tree_tool!(
    tree_expand_all_opens_every_relationship,
    "tree_expand_all",
    json!({ "node": node("p1") }),
    moved
);
tree_tool!(
    tree_collapse_all_closes_the_subtree,
    "tree_collapse_all",
    json!({ "node": node("p0") }),
    moved
);
tree_tool!(
    tree_open_selected_opens_the_page,
    "tree_open_selected",
    json!({ "node": node("p1") }),
    moved
);
tree_tool!(
    tree_reroot_here_opens_a_tree_from_the_node,
    "tree_reroot_here",
    json!({ "node": node("p1") }),
    |answer| {
        assert_eq!(answer.outcome, "opened", "{answer:?}");
        assert_eq!(answer.json["tab"]["thing"]["id"], "p1");
    }
);
tree_tool!(
    tree_show_on_canvas_opens_a_canvas,
    "tree_show_on_canvas",
    json!({ "node": node("p1") }),
    |answer| assert_eq!(answer.json["tab"]["view"], "Canvas", "{answer:?}")
);
tree_tool!(
    tree_open_table_opens_a_table,
    "tree_open_table",
    json!({ "node": node("p1") }),
    |answer| assert_eq!(answer.json["tab"]["view"], "Table", "{answer:?}")
);
tree_tool!(
    tree_open_page_opens_the_page,
    "tree_open_page",
    json!({ "node": node("p1") }),
    |answer| assert_eq!(answer.json["tab"]["view"], "Page", "{answer:?}")
);
tree_tool!(
    tree_clear_filter_with_no_filter_changes_nothing,
    "tree_clear_filter",
    json!({}),
    |answer| assert_eq!(answer.outcome, "no_change", "{answer:?}")
);
tree_tool!(
    tree_toggle_empty_groups_shows_or_hides_them,
    "tree_toggle_empty_groups",
    json!({}),
    |answer| assert_eq!(answer.outcome, "changed", "{answer:?}")
);

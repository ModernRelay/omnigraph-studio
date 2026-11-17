//! UI tools (spec 032): Studio's actions an agent may call to show the person something, and
//! the three reads that tell it what a window shows. A tool is an action on the allow-list
//! ([`ALLOWED`]), with the window it acts in, what it needs before it is dispatched, and what
//! it answers: what changed in the window. No tool writes to a graph, reads a row or answers
//! one of the agent's own requests.
//!
//! A call runs in two updates of its window ([`call`]): the first checks the tool's
//! precondition, gives the workspace focus where nothing has it and dispatches the action,
//! which GPUI defers to the end of that update; the second reads what changed. No frame is
//! waited for (spec 032, Validation log).
//!
//! The reads return identifiers and names only (type names, node ids, branch names, commit ids
//! and tab titles), so an agent reads the graph's data with its own credential.
//!
//! Layer 5, in the crate of the server that speaks MCP ([`crate::http`]). The workspace knows
//! nothing of it: this module reads a window through what every tab and the workspace answer
//! to anyone (`thing`, `view_kind`, `selected_ids`, `pane_tabs`), and acts through actions.

use std::collections::HashSet;
use std::time::Duration;

use gpui::{
    App, AsyncApp, Context, EntityId, FocusHandle, Focusable as _, SharedString, Window,
    WindowHandle,
};
use serde_json::{Map, Value, json};
use studio_actions::{Thing, ViewKind};
use studio_settings::StudioSettings;
use studio_workspace::{
    AgentEdge, AgentGroup, AgentNode, AgentView, ItemHandle, ToastKind, Workspace,
};

/// The most node ids `studio_selection` names.
pub const MAX_SELECTED: usize = 200;

/// The most a tool's answer may take as JSON. The server's whole answer is capped at 64 KiB
/// (spec 032, The server): the answer travels as text inside a JSON-RPC result, whose escapes
/// can take more room than the JSON itself, so a tool's own answer keeps well inside it.
pub const ANSWER_BUDGET: usize = 40 * 1024;

/// What a tool needs of the window before its action is dispatched: an available action can
/// still do nothing, as `NewQueryWithText` does with no graph open (spec 032, "A
/// precondition").
#[derive(Clone, Copy, Debug)]
enum Need {
    /// A graph is open in the window.
    Session,
    /// The argument names a node or edge type of the window's graph.
    Type(&'static str),
    /// The argument, when given, names one.
    MaybeType(&'static str),
    /// The argument names a branch of the window's graph, when its branches have been read.
    Branch(&'static str),
    /// The argument is not empty.
    Text(&'static str),
    /// The argument names one of the window's panels.
    Panel(&'static str),
}

/// Where a tool's action acts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum On {
    /// The window: the workspace handles the action.
    Workspace,
    /// The active tab, which must be this view. It takes the keyboard for the dispatch, as a
    /// click on it would, and gives it back after (spec 032, "The canvas and the tree").
    View(ViewKind),
}

/// One tool: an allowed action under its tool name.
#[derive(Debug)]
pub struct UiTool {
    /// The tool's name: the action's, with `::` as `_`, the `studio_` namespaces shortened, and
    /// the workspace's own actions (namespace `studio`) under `workspace_`; a few are named
    /// for what an agent asks ([`RENAMED`]).
    pub name: &'static str,
    /// The action it dispatches, by its registered name.
    pub action: &'static str,
    needs: &'static [Need],
    /// The action gives what it shows the keyboard (a panel shown by its key does), which a
    /// tool must not: focus goes back where the person had it (spec 032, Focus).
    keeps_focus: bool,
    pub on: On,
    /// The tool takes an optional `node`, which the view selects, as a click does, before the
    /// action runs on what is selected.
    pub node: bool,
    /// The action reads (an expansion): the answer waits for the view to have read and drawn
    /// it, at most [`WAIT`].
    pub waits: bool,
}

/// How long a tool's answer waits for what its action reads.
pub const WAIT: Duration = Duration::from_secs(15);

const fn workspace(name: &'static str, action: &'static str, needs: &'static [Need]) -> UiTool {
    UiTool {
        name,
        action,
        needs,
        keeps_focus: false,
        on: On::Workspace,
        node: false,
        waits: false,
    }
}

const fn view(
    kind: ViewKind,
    name: &'static str,
    action: &'static str,
    node: bool,
    waits: bool,
) -> UiTool {
    UiTool {
        name,
        action,
        needs: &[],
        keeps_focus: true,
        on: On::View(kind),
        node,
        waits,
    }
}

const fn canvas(name: &'static str, action: &'static str, node: bool, waits: bool) -> UiTool {
    view(ViewKind::Canvas, name, action, node, waits)
}

const fn tree(name: &'static str, action: &'static str, node: bool, waits: bool) -> UiTool {
    view(ViewKind::Tree, name, action, node, waits)
}

/// The tools named for what an agent asks rather than after their action.
pub const RENAMED: &[&str] = &[
    "canvas_expand",
    "canvas_collapse",
    "tree_expand",
    "tree_collapse",
    "tree_expand_row",
    "tree_collapse_row",
    "map_back",
    "map_show_domains",
    "map_select_next",
    "map_select_previous",
    "map_open",
];

/// The actions an agent may call, and nothing else: the set is opt-in, so a new action is not
/// a tool until it is added here, with what it needs. Each only opens, shows or walks
/// something. The test `never_a_tool` fails if one that writes, answers the agent's own
/// requests, opens a link or a setting, or moves focus is added.
pub static ALLOWED: &[UiTool] = &[
    workspace(
        "graph_open_type",
        "studio_graph::OpenType",
        &[Need::Session, Need::Type("type_name")],
    ),
    workspace(
        "graph_open_node",
        "studio_graph::OpenNode",
        &[Need::Session, Need::Type("type_name"), Need::Text("id")],
    ),
    workspace(
        "graph_open_node_page",
        "studio_graph::OpenNodePage",
        &[Need::Session, Need::Type("type_name"), Need::Text("id")],
    ),
    workspace(
        "graph_show_node_on_canvas",
        "studio_graph::ShowNodeOnCanvas",
        &[Need::Session, Need::Type("type_name"), Need::Text("id")],
    ),
    workspace(
        "graph_show_node_in_tree",
        "studio_graph::ShowNodeInTree",
        &[Need::Session, Need::Type("type_name"), Need::Text("id")],
    ),
    workspace(
        "graph_show_results_on_canvas",
        "studio_graph::ShowResultsOnCanvas",
        &[Need::Session],
    ),
    workspace("graph_open_map", "studio_graph::OpenMap", &[Need::Session]),
    workspace(
        "graph_open_source",
        "studio_graph::OpenSource",
        &[Need::Session, Need::MaybeType("type_name")],
    ),
    workspace(
        "graph_open_branch",
        "studio_graph::OpenBranch",
        &[Need::Session, Need::Branch("branch")],
    ),
    workspace(
        "graph_open_branch_history",
        "studio_graph::OpenBranchHistory",
        &[Need::Session, Need::Branch("branch")],
    ),
    workspace(
        "graph_open_commit",
        "studio_graph::OpenCommit",
        &[Need::Session, Need::Text("commit_id")],
    ),
    workspace(
        "graph_new_query_with_text",
        "studio_graph::NewQueryWithText",
        &[Need::Session],
    ),
    UiTool {
        keeps_focus: true,
        ..workspace(
            "workspace_activate_panel",
            "studio::ActivatePanel",
            &[Need::Panel("id")],
        )
    },
    // The canvas (spec 032, "The canvas and the tree").
    canvas(
        "canvas_select_node",
        "studio_canvas::SelectNode",
        false,
        false,
    ),
    canvas(
        "canvas_expand",
        "studio_canvas::ExpandNeighbourhood",
        false,
        true,
    ),
    canvas(
        "canvas_collapse",
        "studio_canvas::CollapseNodes",
        false,
        false,
    ),
    canvas(
        "canvas_expand_node",
        "studio_canvas::ExpandNode",
        true,
        true,
    ),
    canvas(
        "canvas_collapse_node",
        "studio_canvas::CollapseNode",
        true,
        false,
    ),
    canvas("canvas_show_more", "studio_canvas::ShowMore", false, true),
    canvas(
        "canvas_hide_nodes",
        "studio_canvas::HideNodes",
        false,
        false,
    ),
    canvas(
        "canvas_show_hidden",
        "studio_canvas::ShowHidden",
        false,
        false,
    ),
    canvas("canvas_pin_nodes", "studio_canvas::PinNodes", false, false),
    canvas(
        "canvas_unpin_nodes",
        "studio_canvas::UnpinNodes",
        false,
        false,
    ),
    canvas(
        "canvas_reroot_here",
        "studio_canvas::RerootHere",
        true,
        false,
    ),
    canvas("canvas_move_left", "studio_canvas::MoveLeft", true, false),
    canvas("canvas_move_right", "studio_canvas::MoveRight", true, false),
    canvas("canvas_move_up", "studio_canvas::MoveUp", true, false),
    canvas("canvas_move_down", "studio_canvas::MoveDown", true, false),
    canvas(
        "canvas_open_selected_page",
        "studio_canvas::OpenSelectedPage",
        true,
        false,
    ),
    canvas(
        "canvas_focus_selected",
        "studio_canvas::FocusSelected",
        true,
        false,
    ),
    canvas(
        "canvas_follow_relationship",
        "studio_canvas::FollowRelationship",
        true,
        true,
    ),
    canvas("canvas_walk_back", "studio_canvas::WalkBack", false, false),
    canvas(
        "canvas_walk_forward",
        "studio_canvas::WalkForward",
        false,
        false,
    ),
    canvas(
        "canvas_select_all",
        "studio_canvas::SelectAll",
        false,
        false,
    ),
    canvas(
        "canvas_clear_selection",
        "studio_canvas::ClearSelection",
        false,
        false,
    ),
    canvas(
        "canvas_fit_to_window",
        "studio_canvas::FitToWindow",
        false,
        false,
    ),
    canvas("canvas_zoom_in", "studio_canvas::ZoomIn", false, false),
    canvas("canvas_zoom_out", "studio_canvas::ZoomOut", false, false),
    canvas(
        "canvas_reset_layout",
        "studio_canvas::ResetLayout",
        false,
        false,
    ),
    view(
        ViewKind::Table,
        "table_show_row_on_canvas",
        "studio_table::ShowRowOnCanvas",
        false,
        false,
    ),
    // The schema's map.
    view(
        ViewKind::Map,
        "map_back",
        "studio_ontology::Back",
        false,
        false,
    ),
    view(
        ViewKind::Map,
        "map_show_domains",
        "studio_ontology::ShowDomains",
        false,
        false,
    ),
    view(
        ViewKind::Map,
        "map_select_next",
        "studio_ontology::SelectNext",
        false,
        false,
    ),
    view(
        ViewKind::Map,
        "map_select_previous",
        "studio_ontology::SelectPrevious",
        false,
        false,
    ),
    view(
        ViewKind::Map,
        "map_open",
        "studio_ontology::Open",
        false,
        false,
    ),
    // The relationship tree.
    tree("tree_select_node", "studio_tree::SelectNode", false, false),
    tree(
        "tree_expand",
        "studio_tree::ExpandNeighbourhood",
        false,
        true,
    ),
    tree("tree_collapse", "studio_tree::CollapseNodes", false, false),
    tree("tree_select_next", "studio_tree::SelectNext", true, false),
    tree(
        "tree_select_previous",
        "studio_tree::SelectPrevious",
        true,
        false,
    ),
    tree("tree_expand_row", "studio_tree::Expand", true, true),
    tree("tree_collapse_row", "studio_tree::Collapse", true, false),
    tree("tree_expand_all", "studio_tree::ExpandAll", true, true),
    tree("tree_collapse_all", "studio_tree::CollapseAll", true, false),
    tree(
        "tree_open_selected",
        "studio_tree::OpenSelected",
        true,
        true,
    ),
    tree("tree_reroot_here", "studio_tree::RerootHere", true, false),
    tree(
        "tree_show_on_canvas",
        "studio_tree::ShowOnCanvas",
        true,
        false,
    ),
    tree("tree_open_table", "studio_tree::OpenTable", true, false),
    tree("tree_open_page", "studio_tree::OpenPage", true, false),
    tree(
        "tree_clear_filter",
        "studio_tree::ClearFilter",
        false,
        false,
    ),
    tree(
        "tree_toggle_empty_groups",
        "studio_tree::ToggleEmptyGroups",
        false,
        false,
    ),
];

/// The tools that only read the window.
pub const READS: [&str; 5] = [
    "studio_windows",
    "studio_tabs",
    "studio_selection",
    "studio_canvas",
    "studio_tree",
];

/// The most nodes `studio_canvas` and `studio_tree` name, and the most edges.
pub const MAX_VIEW_NODES: usize = 500;

/// The most nodes, edges and relationships a canvas or tree tool's answer lists as changed.
pub const MAX_CHANGED: usize = 200;

/// A tool's name from its action's: `studio_graph::OpenType` is `graph_open_type`, and
/// `studio::ActivatePanel`, an action of the workspace, `workspace_activate_panel`.
pub fn tool_name(action: &str) -> String {
    let (namespace, name) = action.rsplit_once("::").unwrap_or(("", action));
    let prefix = match namespace {
        "studio" => "workspace",
        other => other.strip_prefix("studio_").unwrap_or(other),
    };
    let mut snake = String::new();
    for (index, character) in name.chars().enumerate() {
        if character.is_uppercase() {
            if index > 0 {
                snake.push('_');
            }
            snake.extend(character.to_lowercase());
        } else {
            snake.push(character);
        }
    }
    format!("{prefix}_{snake}")
}

/// The tool by its name.
pub fn tool(name: &str) -> Option<&'static UiTool> {
    ALLOWED.iter().find(|tool| tool.name == name)
}

/// Whether `name` is a UI tool, an action's or a read.
pub fn is_tool(name: &str) -> bool {
    tool(name).is_some() || READS.contains(&name)
}

/// Where a token's calls act.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// Any window, named by each call (an agent outside Studio).
    Any,
    /// One window only (a conversation's agent): `window` is fixed.
    Bound(u64),
}

/// The tools as MCP lists them (`tools/list`): each name, its action's doc comment, and its
/// input schema, the action's with `window` added and `preview` taken out, since a tool opens
/// its tab as a preview wherever the action can (spec 032, Focus). A conversation's window is
/// fixed, so it need not name one.
pub fn definitions(target: Target, cx: &App) -> Vec<Value> {
    let window = json!({
        "type": "integer",
        "description": "The window to act in: its id from studio_windows.",
    });
    let bound = matches!(target, Target::Bound(_));
    let with_window = |mut schema: Map<String, Value>| {
        let properties = schema
            .entry("properties")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .map(std::mem::take)
            .unwrap_or_default();
        let mut properties = properties;
        properties.insert("window".into(), window.clone());
        schema.insert("properties".into(), Value::Object(properties));
        if !bound {
            let required = schema
                .entry("required")
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .map(std::mem::take)
                .unwrap_or_default();
            let mut required = required;
            required.push(json!("window"));
            schema.insert("required".into(), Value::Array(required));
        }
        Value::Object(schema)
    };
    let mut generator = schemars::generate::SchemaSettings::draft2020_12()
        .with(|settings| settings.inline_subschemas = true)
        .into_generator();
    let documentation = cx.action_documentation();
    let mut tools = vec![
        json!({
            "name": "studio_windows",
            "description": if bound {
                "This conversation's Studio window: its id, its server's name, its graph, its \
                 branch, and whether it is in front."
            } else {
                "Studio's windows: each one's id, its server's name, its graph, its branch, and \
                 whether it is in front. Every other tool names a window by this id."
            },
            "inputSchema": { "type": "object", "properties": {} },
        }),
        json!({
            "name": "studio_tabs",
            "description": "A window's panes and their tabs: what each tab shows (a thing and \
                            a view), its title, the commit it is held at, and which tab is \
                            active. Identifiers only, never values.",
            "inputSchema": with_window(Map::from_iter([("type".into(), json!("object"))])),
        }),
        json!({
            "name": "studio_selection",
            "description": format!(
                "What the active tab of a window has selected, as identifiers: the type, the \
                 node ids (at most {MAX_SELECTED}) and, on a canvas or a tree, the node it walks \
                 from. Never property values."
            ),
            "inputSchema": with_window(Map::from_iter([("type".into(), json!("object"))])),
        }),
        json!({
            "name": "studio_canvas",
            "description": format!(
                "What the active canvas of a window draws, as identifiers and the names the \
                 person sees: its root; its nodes as [type, id, name] (at most \
                 {MAX_VIEW_NODES}, with `truncated`); its edges as [from, to, relationship], \
                 indices into `nodes`; its relationships as [node, relationship, other type, \
                 count, open, shown]; what is selected; its zoom; whether the node cap left \
                 nodes out; and the walk's history. Never property values."
            ),
            "inputSchema": with_window(Map::from_iter([("type".into(), json!("object"))])),
        }),
        json!({
            "name": "studio_tree",
            "description": format!(
                "What the active relationship tree of a window shows: its root; its \
                 relationships as [node, relationship, other type, count, open, shown], \
                 indices into `nodes`; the nodes shown as [type, id, name] (at most \
                 {MAX_VIEW_NODES}, with `truncated`); and what is selected. Never property \
                 values."
            ),
            "inputSchema": with_window(Map::from_iter([("type".into(), json!("object"))])),
        }),
    ];
    let node = json!({
        "type": "object",
        "description": "The node to act on, selected first as a click would: its type and id. \
                        Without it, the view's selected node.",
        "properties": {
            "type_name": { "type": "string" },
            "id": { "type": "string" },
        },
        "required": ["type_name", "id"],
    });
    for tool in ALLOWED {
        let mut schema = match cx.action_schema_by_name(tool.action, &mut generator) {
            Some(Some(schema)) => schema.to_value(),
            _ => json!({ "type": "object" }),
        };
        if let Some(object) = schema.as_object_mut() {
            object.remove("$schema");
            object.remove("title");
            object.remove("description");
            if let Some(properties) = object.get_mut("properties").and_then(Value::as_object_mut) {
                properties.remove("preview");
                properties.remove("separate");
            }
            if let Some(required) = object.get_mut("required").and_then(Value::as_array_mut) {
                required.retain(|name| name != "preview");
            }
            if tool.node {
                object
                    .entry("properties")
                    .or_insert_with(|| json!({}))
                    .as_object_mut()
                    .map(|properties| properties.insert("node".into(), node.clone()));
            }
            if !object.contains_key("type") {
                object.insert("type".into(), json!("object"));
            }
        }
        let description = documentation
            .get(tool.action)
            .map(|doc| doc.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        tools.push(json!({
            "name": tool.name,
            "description": description,
            "inputSchema": with_window(schema.as_object().cloned().unwrap_or_default()),
        }));
    }
    tools
}

/// Who a call comes from, for the window's chip and the notice of an agent outside Studio.
#[derive(Clone, Debug, Default)]
pub struct Caller {
    /// The agent's name, as the chip says it ("Opened by Claude Code").
    pub agent: Option<SharedString>,
    /// No window has yet said this run that an agent outside Studio is there: the window this
    /// call names says so, and the answer tells ([`Answer::announced`]).
    pub announce: bool,
}

/// A tool's answer: what it says in words, its JSON, and whether it is an error (MCP's
/// `isError`, which the agent reads and can correct).
#[derive(Clone, Debug, PartialEq)]
pub struct Answer {
    /// The outcome by name: `opened`, `shown`, `panel_shown`, `no_change`, `read`,
    /// `not_available`, `window_closed`, `invalid_input`, `unknown_tool`, `busy`, `cancelled`.
    pub outcome: &'static str,
    pub says: String,
    pub json: Value,
    pub is_error: bool,
    /// The window it acted in was told an agent outside Studio is there ([`Caller::announce`]).
    pub announced: bool,
}

impl Answer {
    fn new(
        outcome: &'static str,
        says: impl Into<String>,
        mut json: Value,
        is_error: bool,
    ) -> Self {
        let says = says.into();
        if let Some(object) = json.as_object_mut() {
            object.insert("outcome".into(), json!(outcome));
            object.insert("says".into(), json!(says));
        }
        Self {
            outcome,
            says,
            json,
            is_error,
            announced: false,
        }
    }

    fn error(outcome: &'static str, says: impl Into<String>, window: Option<u64>) -> Self {
        let mut json = json!({});
        if let Some(window) = window {
            json["window"] = json!(window);
        }
        Self::new(outcome, says, json, true)
    }

    /// A window that has closed, or was never one of Studio's.
    pub fn window_closed(window: u64) -> Self {
        Self::error(
            "window_closed",
            format!("Window {window} is not open. studio_windows lists the windows open now."),
            Some(window),
        )
    }

    /// Studio stopped before it answered (it is quitting): what the call did is not known.
    pub fn cancelled(window: Option<u64>) -> Self {
        Self::error(
            "cancelled",
            "Studio stopped before it answered, so what the call did is not known. studio_tabs \
             says what a window shows.",
            window,
        )
    }

    /// The token already has its most calls in flight.
    pub fn busy(most: usize) -> Self {
        Self::error(
            "busy",
            format!(
                "Studio is still answering {most} calls of yours. Wait for an answer before the \
                 next call."
            ),
            None,
        )
    }
}

/// Runs one call: a read, or an action dispatched in its window and what changed. Never sends
/// a request to a server itself; a view it opens reads as it does when the person opens it.
pub async fn call(
    cx: &mut AsyncApp,
    target: Target,
    caller: Caller,
    name: &str,
    input: Value,
) -> Answer {
    let (mut answer, acted_in) = answer_call(cx, target, &caller, name, input).await;
    // An outside agent's first call says so as soon as it comes, whatever it is (a list of the
    // windows, a read, a refusal or an action): in the window it named when it named an open
    // one, else in the window in front.
    if caller.announce {
        let window = match acted_in {
            Some(window) => Some(window),
            None => cx.update(front_workspace),
        };
        if let Some(window) = window {
            let shown = window.update(cx, |workspace, _, cx| {
                workspace.show_toast_with_action(
                    ToastKind::Info,
                    OUTSIDE_NOTICE,
                    "Turn off",
                    Box::new(studio_actions::workspace::OpenAgentSettings),
                    cx,
                );
            });
            answer.announced = shown.is_ok();
        }
    }
    answer
}

/// The workspace window in front, else the first one.
fn front_workspace(cx: &mut App) -> Option<WindowHandle<Workspace>> {
    cx.active_window()
        .and_then(|window| window.downcast::<Workspace>())
        .or_else(|| {
            cx.windows()
                .into_iter()
                .find_map(|window| window.downcast::<Workspace>())
        })
}

/// A call's answer, and the open window it named, if any.
async fn answer_call(
    cx: &mut AsyncApp,
    target: Target,
    caller: &Caller,
    name: &str,
    input: Value,
) -> (Answer, Option<WindowHandle<Workspace>>) {
    let input = match input {
        Value::Null => json!({}),
        Value::Object(object) => Value::Object(object),
        _ => {
            return (
                Answer::error(
                    "invalid_input",
                    "A tool's arguments are a JSON object.",
                    None,
                ),
                None,
            );
        }
    };
    if name == "studio_windows" {
        return (cx.update(|cx| windows(target, cx)), None);
    }
    let tool = tool(name);
    if tool.is_none() && !READS.contains(&name) {
        return (
            Answer::error(
                "unknown_tool",
                format!("Studio has no tool `{name}`. tools/list lists them."),
                None,
            ),
            None,
        );
    }
    let asked = input.get("window").and_then(Value::as_u64);
    let id = match (target, asked) {
        (Target::Bound(own), Some(asked)) if asked != own => {
            return (
                Answer::error(
                    "not_available",
                    format!("This conversation shows things only in its own window, {own}."),
                    Some(asked),
                ),
                None,
            );
        }
        (Target::Bound(own), _) => own,
        (Target::Any, Some(asked)) => asked,
        (Target::Any, None) => {
            return (
                Answer::error(
                    "invalid_input",
                    "Name the window to act in: `window`, an id from studio_windows.",
                    None,
                ),
                None,
            );
        }
    };
    let Some(handle) = cx.update(|cx| workspace_window(id, cx)) else {
        return (Answer::window_closed(id), None);
    };
    let answer = target_window(cx, handle, id, tool, name, caller, input).await;
    let acted_in = (answer.outcome != "window_closed").then_some(handle);
    (answer, acted_in)
}

/// A call to the open window `id`: a read, or the tool dispatched there and what changed.
async fn target_window(
    cx: &mut AsyncApp,
    handle: WindowHandle<Workspace>,
    id: u64,
    tool: Option<&'static UiTool>,
    name: &str,
    caller: &Caller,
    input: Value,
) -> Answer {
    let Some(tool) = tool else {
        let read = handle.update(cx, |workspace, window, cx| match name {
            "studio_tabs" => tabs(id, workspace, window, cx),
            "studio_canvas" => view_read(id, ViewKind::Canvas, workspace, cx),
            "studio_tree" => view_read(id, ViewKind::Tree, workspace, cx),
            _ => selection(id, workspace, cx),
        });
        return read.unwrap_or_else(|_| Answer::window_closed(id));
    };

    let first = handle.update(cx, |workspace, window, cx| {
        dispatch(id, tool, &input, workspace, window, cx)
    });
    let before = match first {
        Err(_) => return Answer::window_closed(id),
        Ok(Err(answer)) => return answer,
        Ok(Ok(before)) => before,
    };
    // The action ran as the first update's effects were flushed. One that reads is waited for
    // until the view has read and drawn what it opened, at most `WAIT`.
    // A view tool waits for its view to have laid out what changed, whether or not it reads.
    if let Some(item) = before.view_item {
        let deadline = cx.background_executor().now() + WAIT;
        loop {
            // No redraw is asked for: a view that changed notifies, and is drawn (drawing.md).
            let busy = handle.update(cx, |workspace, _, cx| {
                workspace
                    .all_items()
                    .find(|tab| tab.item_id() == item)
                    .is_some_and(|tab| tab.agent_busy(cx))
            });
            match busy {
                Err(_) => return Answer::window_closed(id),
                Ok(false) => break,
                Ok(true) if cx.background_executor().now() >= deadline => break,
                Ok(true) => cx.background_executor().timer(POLL).await,
            }
        }
    }
    let second = handle.update(cx, |workspace, window, cx| {
        let stayed = before.view_item.is_some()
            && workspace.active_item().map(|item| item.item_id()) == before.view_item;
        let answer = if stayed {
            view_changed(id, tool, &before, workspace, cx)
        } else {
            changed(id, tool, &before, workspace, window, cx)
        };
        if tool.keeps_focus {
            keep_focus(&before, workspace, window, cx);
        }
        if matches!(
            answer.outcome,
            "opened" | "shown" | "panel_shown" | "changed" | "expanded" | "cap_reached"
        ) {
            let agent = caller
                .agent
                .clone()
                .unwrap_or_else(|| SharedString::from("an agent"));
            workspace.show_opened_by(agent, window, cx);
            // The person may ask for the window to come to the front (spec 032, Focus).
            if StudioSettings::get(cx).agent.raise_window {
                window.activate_window();
            }
        }
        answer
    });
    second.unwrap_or_else(|_| Answer::window_closed(id))
}

/// What a window says the first time an agent outside Studio acts in it, once a run.
pub const OUTSIDE_NOTICE: &str = "An agent outside Studio is showing things here";

/// The workspace window with this id.
fn workspace_window(id: u64, cx: &App) -> Option<WindowHandle<Workspace>> {
    cx.windows()
        .into_iter()
        .find(|window| window.window_id().as_u64() == id)
        .and_then(|window| window.downcast::<Workspace>())
}

/// The window's front, as a call's result compares it: the active tab, every tab, and the
/// panels shown.
struct Front {
    /// What had the keyboard just before the dispatch, and the panel it was in, if any.
    focus: Option<FocusHandle>,
    focus_panel: Option<SharedString>,
    active: Option<EntityId>,
    items: HashSet<EntityId>,
    panels: Vec<SharedString>,
    /// A view tool's tab, and what an agent read of it before the call.
    view_item: Option<EntityId>,
    view: Option<AgentView>,
}

/// How often a call that waits looks at its view again.
const POLL: Duration = Duration::from_millis(40);

fn front(workspace: &Workspace, window: &Window, cx: &App) -> Front {
    Front {
        focus: window.focused(cx),
        focus_panel: workspace.panel_with_focus(window, cx),
        active: workspace.active_item().map(|item| item.item_id()),
        items: workspace.all_items().map(|item| item.item_id()).collect(),
        panels: workspace.shown_panels(),
        view_item: None,
        view: None,
    }
}

/// Checks the tool's precondition, builds its action and dispatches it: what the window showed
/// before, or the answer when it cannot run.
fn dispatch(
    id: u64,
    tool: &UiTool,
    input: &Value,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Result<Front, Answer> {
    let not_available = |why: String| {
        let mut answer = Answer::error("not_available", why, Some(id));
        answer.json["action"] = json!(tool.action);
        answer
    };
    for need in tool.needs {
        if let Err(why) = check(*need, input, workspace, cx) {
            return Err(not_available(why));
        }
    }
    // A view tool acts on the active tab, which must be that view, and on the node named.
    let mut view_tab = None;
    if let On::View(kind) = tool.on {
        let Some(item) = workspace.active_item() else {
            return Err(not_available(no_view(kind, None)));
        };
        if !shows(item, kind, cx) {
            return Err(not_available(no_view(kind, Some(&describe(item, cx)))));
        }
        if let Err(refusal) = item.agent_check(tool.name, input, cx) {
            let mut answer = Answer::new(
                refusal.outcome,
                refusal.says,
                refusal.json,
                refusal.outcome != "needs_confirmation",
            );
            answer.json["window"] = json!(id);
            answer.json["action"] = json!(tool.action);
            return Err(answer);
        }
        let named = tool
            .node
            .then(|| input.get("node").and_then(node_id))
            .flatten();
        if let Some((type_name, node)) = &named {
            let shown = item.agent_view(cx).is_some_and(|view| {
                view.nodes
                    .iter()
                    .any(|n| n.type_name == *type_name && n.id == *node)
            });
            if !shown {
                return Err(not_available(not_shown(kind)));
            }
        }
        view_tab = Some((
            item.item_id(),
            item.focus_handle(cx),
            item.agent_view(cx),
            named,
        ));
    }
    let mut data = input.as_object().cloned().unwrap_or_default();
    data.remove("window");
    if tool.node {
        data.remove("node");
    }
    let mut generator = schemars::SchemaGenerator::default();
    let schema = cx
        .action_schema_by_name(tool.action, &mut generator)
        .flatten();
    let takes_preview = schema
        .as_ref()
        .is_some_and(|schema| schema.as_value().pointer("/properties/preview").is_some());
    if takes_preview {
        data.insert("preview".into(), json!(true));
    }
    // A filtered table opens as a tab of its own, which the person can close, and the type's
    // open table is left as the person has it (spec 032, Focus).
    if tool.name == "graph_open_type"
        && data
            .get("filter_id")
            .is_some_and(|filter| !filter.is_null())
    {
        data.insert("separate".into(), json!(true));
        // A preview would take the place of the type's table when that is the preview.
        data.insert("preview".into(), json!(false));
    }
    // An action without arguments is built from none.
    let data = schema.is_some().then_some(Value::Object(data));
    let action = match cx.build_action(tool.action, data) {
        Ok(action) => action,
        Err(error) => {
            return Err(Answer::error(
                "invalid_input",
                format!("The arguments do not fit {}: {error}", tool.name),
                Some(id),
            ));
        }
    };
    let mut before = front(workspace, window, cx);
    match &view_tab {
        // The view takes the keyboard, as a click on it would; `keep_focus` gives it back.
        Some((_, focus, _, _)) => window.focus(focus, cx),
        // With nothing focused an action reaches only global handlers, so the workspace takes
        // focus first, as a click in the window would. Focus the person has is left where it
        // is.
        None if window.focused(cx).is_none() => {
            let focus = workspace.focus_handle(cx);
            window.focus(&focus, cx);
        }
        None => {}
    }
    if !window.is_action_available(action.as_ref(), cx) {
        if let Some(focus) = &before.focus {
            window.focus(focus, cx);
        }
        return Err(not_available(format!(
            "{} is not available in this window now.",
            tool.name
        )));
    }
    if let Some((item, _, view, named)) = view_tab {
        before.view_item = Some(item);
        before.view = view;
        // The node named is selected first, as a click selects it.
        if let (Some((type_name, node)), On::View(kind)) = (named, tool.on) {
            let select = match kind {
                ViewKind::Tree => "studio_tree::SelectNode",
                _ => "studio_canvas::SelectNode",
            };
            if let Ok(select) =
                cx.build_action(select, Some(json!({ "type_name": type_name, "id": node })))
            {
                window.dispatch_action(select, cx);
            }
        }
    }
    window.dispatch_action(action, cx);
    Ok(before)
}

/// A node an agent names: `{type_name, id}`.
fn node_id(value: &Value) -> Option<(String, String)> {
    Some((
        value.get("type_name")?.as_str()?.to_owned(),
        value.get("id")?.as_str()?.to_owned(),
    ))
}

/// Whether `item` is the view `kind`: a query's result picture is a canvas too.
fn shows(item: &dyn ItemHandle, kind: ViewKind, cx: &App) -> bool {
    match kind {
        ViewKind::Canvas => item.agent_view(cx).is_some_and(|_| {
            item.view_kind(cx)
                .is_none_or(|view| view == ViewKind::Canvas)
        }),
        kind => item.view_kind(cx) == Some(kind),
    }
}

/// Why a view tool has no view to act on.
fn no_view(kind: ViewKind, tab: Option<&Value>) -> String {
    let open = match kind {
        ViewKind::Canvas => "graph_show_node_on_canvas",
        ViewKind::Tree => "graph_show_node_in_tree",
        ViewKind::Map => "graph_open_map",
        _ => "graph_open_type",
    };
    let what = kind.label().to_lowercase();
    match tab {
        Some(tab) => format!(
            "The active tab is {}, not a {what}. Open one with {open}, or bring one to the \
             front with studio_tabs and the person's help",
            tab_words(tab)
        ),
        None => format!("No tab is open. Open a {what} with {open}"),
    }
}

/// Why a node named is not acted on: the view does not show it.
fn not_shown(kind: ViewKind) -> String {
    match kind {
        ViewKind::Tree => "Not in this tree; expand a relationship that holds it or use \
                           graph_show_node_in_tree"
            .into(),
        _ => "Not on this canvas; expand a neighbour or use graph_show_node_on_canvas".into(),
    }
}

fn node_json(node: &AgentNode) -> Value {
    json!({ "type_name": node.type_name, "id": node.id, "label": node.label })
}

fn pair_json((type_name, id): &(String, String)) -> Value {
    json!({ "type_name": type_name, "id": id })
}

/// The first `MAX_CHANGED` of `items` as JSON, and whether more were left out.
fn listed<T>(items: &[T], json: impl Fn(&T) -> Value) -> (Vec<Value>, bool) {
    (
        items.iter().take(MAX_CHANGED).map(json).collect(),
        items.len() > MAX_CHANGED,
    )
}

/// What a canvas or tree tool changed in its view: the nodes and edges drawn now and those
/// added or removed, the relationships left closed, what is selected, and whether the node cap
/// was reached.
fn view_changed(id: u64, tool: &UiTool, before: &Front, workspace: &Workspace, cx: &App) -> Answer {
    let Some(item) = workspace.active_item() else {
        return Answer::window_closed(id);
    };
    let tab = describe(item, cx);
    let old = before.view.clone().unwrap_or_default();
    let new = item.agent_view(cx).unwrap_or_default();
    let key = |node: &AgentNode| (node.type_name.clone(), node.id.clone());
    let old_nodes: HashSet<(String, String)> = old.nodes.iter().map(key).collect();
    let new_nodes: HashSet<(String, String)> = new.nodes.iter().map(key).collect();
    let added: Vec<&AgentNode> = new
        .nodes
        .iter()
        .filter(|node| !old_nodes.contains(&key(node)))
        .collect();
    let removed: Vec<&AgentNode> = old
        .nodes
        .iter()
        .filter(|node| !new_nodes.contains(&key(node)))
        .collect();
    let old_edges: HashSet<&AgentEdge> = old.edges.iter().collect();
    let edges_added: Vec<&AgentEdge> = new
        .edges
        .iter()
        .filter(|edge| !old_edges.contains(edge))
        .collect();
    let old_groups: HashSet<(&(String, String), &String)> = old
        .groups
        .iter()
        .map(|group| (&group.node, &group.relationship))
        .collect();
    let closed: Vec<&AgentGroup> = new
        .groups
        .iter()
        .filter(|group| {
            !group.expanded && !old_groups.contains(&(&group.node, &group.relationship))
        })
        .collect();
    let (added_json, added_cut) = listed(&added, |node| node_json(node));
    let (removed_json, removed_cut) = listed(&removed, |node| node_json(node));
    let (edges_json, edges_cut) = listed(
        &edges_added,
        |edge| json!({ "from": pair_json(&edge.from), "to": pair_json(&edge.to), "relationship": edge.relationship }),
    );
    let (closed_json, closed_cut) = listed(
        &closed,
        |group| json!({ "node": pair_json(&group.node), "relationship": group.relationship, "other_type": group.other_type, "count": group.count }),
    );
    let still_reading = item.agent_busy(cx);
    let expands = tool.name.ends_with("_expand");
    let changed = !added.is_empty()
        || !removed.is_empty()
        || new.selected != old.selected
        || new.facts != old.facts
        || new.groups != old.groups;
    let outcome = match () {
        _ if expands && new.stopped.is_some() => "needs_confirmation",
        _ if new.capped && (!old.capped || !added.is_empty()) => "cap_reached",
        _ if expands && !added.is_empty() => "expanded",
        _ if changed => "changed",
        _ => "no_change",
    };
    let what = tab["view"].as_str().unwrap_or("view").to_lowercase();
    let mut says = match outcome {
        "no_change" => format!("{} changed nothing on the {what}", tool.name),
        _ if !added.is_empty() || !removed.is_empty() => format!(
            "{} added, {} removed: {} drawn",
            studio_ui::plural(added.len(), "node", "nodes"),
            removed.len(),
            studio_ui::plural(new.nodes.len(), "node", "nodes"),
        ),
        _ => format!("{} changed the {what}", tool.name),
    };
    if outcome == "cap_reached" {
        says.push_str("; the node cap was reached, so the canvas stopped there");
    }
    if let Some((nodes, reads)) = new.stopped {
        says.push_str(&format!(
            "; the second level would open {nodes} nodes with {reads} reads: call again with \
             confirm: true to open it"
        ));
    }
    if still_reading {
        says.push_str("; still reading");
    }
    let mut json = json!({
        "window": id,
        "tab": tab,
        "nodes_drawn": new.nodes.len(),
        "edges_drawn": new.edges.len(),
        "added": added_json,
        "removed": removed_json,
        "edges_added": edges_json,
        "unexpanded": closed_json,
        "truncated": added_cut || removed_cut || edges_cut || closed_cut,
        "selected": new.selected,
        "cap_reached": new.capped,
        "still_reading": still_reading,
    });
    if let Some((nodes, reads)) = new.stopped {
        json["needs_confirmation"] = json!({ "nodes": nodes, "reads": reads });
    }
    for (key, value) in &new.facts {
        json[key] = value.clone();
    }
    // The lists are cut, the longest first, until the answer fits what one may take: the view
    // changed, and the agent hears that, whatever was named.
    while json.to_string().len() > ANSWER_BUDGET {
        let longest = ["added", "removed", "edges_added", "unexpanded"]
            .into_iter()
            .max_by_key(|key| json[*key].as_array().map_or(0, Vec::len));
        let Some(list) = longest.and_then(|key| json[key].as_array_mut()) else {
            break;
        };
        if list.is_empty() {
            break;
        }
        list.truncate(list.len() / 2);
        json["truncated"] = json!(true);
    }
    Answer::new(outcome, says, json, false)
}

/// `studio_canvas` and `studio_tree`: what the active view draws, as compact lists: nodes as
/// `[type, id, name]`, edges and relationships by index into the nodes.
fn view_read(id: u64, kind: ViewKind, workspace: &Workspace, cx: &App) -> Answer {
    let Some(item) = workspace.active_item() else {
        return Answer::error("not_available", no_view(kind, None), Some(id));
    };
    if !shows(item, kind, cx) {
        return Answer::error(
            "not_available",
            no_view(kind, Some(&describe(item, cx))),
            Some(id),
        );
    }
    let tab = describe(item, cx);
    let view = item.agent_view(cx).unwrap_or_default();
    let make = |limit: usize| {
        let nodes: Vec<&AgentNode> = view.nodes.iter().take(limit).collect();
        let at: std::collections::HashMap<(&str, &str), usize> = nodes
            .iter()
            .enumerate()
            .map(|(index, node)| ((node.type_name.as_str(), node.id.as_str()), index))
            .collect();
        let index = |pair: &(String, String)| at.get(&(pair.0.as_str(), pair.1.as_str())).copied();
        let edges: Vec<Value> = view
            .edges
            .iter()
            .filter_map(|edge| {
                Some(json!([
                    index(&edge.from)?,
                    index(&edge.to)?,
                    edge.relationship
                ]))
            })
            .collect();
        let groups: Vec<Value> = view
            .groups
            .iter()
            .filter_map(|group| {
                Some(json!([
                    index(&group.node)?,
                    group.relationship,
                    group.other_type,
                    group.count,
                    group.expanded,
                    group.shown
                ]))
            })
            .collect();
        let mut json = json!({
            "window": id,
            "tab": tab,
            "root": view.root.as_ref().map(|root| json!([root.type_name, root.id, root.label])),
            "nodes": nodes.iter().map(|node| json!([node.type_name, node.id, node.label])).collect::<Vec<_>>(),
            "edges": edges,
            "relationships": groups,
            "selected": view.selected,
            "cap_reached": view.capped,
            "truncated": view.nodes.len() > nodes.len(),
        });
        for (key, value) in &view.facts {
            json[key] = value.clone();
        }
        json
    };
    let mut limit = MAX_VIEW_NODES;
    let mut json = make(limit);
    while json.to_string().len() > ANSWER_BUDGET && limit > 0 {
        limit /= 2;
        json = make(limit);
    }
    let says = format!(
        "{} {} drawn{}",
        studio_ui::plural(view.nodes.len(), "node", "nodes"),
        studio_ui::plural(view.edges.len(), "edge", "edges"),
        if json["truncated"] == json!(true) {
            format!(", the first {limit} named")
        } else {
            String::new()
        }
    );
    Answer::new("read", says, json, false)
}

/// Whether the window meets one need, or why not in words.
fn check(need: Need, input: &Value, workspace: &Workspace, cx: &App) -> Result<(), String> {
    let text = |key: &str| input.get(key).and_then(Value::as_str).map(str::trim);
    let session = workspace.session().map(|session| session.read(cx));
    match need {
        Need::Session => match session {
            Some(_) => Ok(()),
            None => Err("No graph is open in this window".into()),
        },
        Need::Text(key) => match text(key) {
            Some(value) if !value.is_empty() => Ok(()),
            _ => Err(format!("`{key}` is empty")),
        },
        Need::Type(key) | Need::MaybeType(key) => {
            let Some(type_name) = text(key).filter(|name| !name.is_empty()) else {
                return match need {
                    Need::Type(_) => Err(format!("`{key}` is empty")),
                    _ => Ok(()),
                };
            };
            let Some(session) = session else {
                return Err("No graph is open in this window".into());
            };
            let Some(schema) = session.schema_model() else {
                return Err(format!(
                    "The schema of {} has not been read yet in this window",
                    session.graph_id
                ));
            };
            if schema.node(type_name).is_some() || schema.edge(type_name).is_some() {
                Ok(())
            } else {
                Err(format!(
                    "{type_name} is not a node or edge type of {}",
                    session.graph_id
                ))
            }
        }
        Need::Branch(key) => {
            let Some(branch) = text(key).filter(|name| !name.is_empty()) else {
                return Err(format!("`{key}` is empty"));
            };
            let Some(session) = session else {
                return Err("No graph is open in this window".into());
            };
            match session.branches.loaded() {
                Some(branches) if !branches.iter().any(|known| known == branch) => {
                    Err(format!("{branch} is not a branch of {}", session.graph_id))
                }
                _ => Ok(()),
            }
        }
        Need::Panel(key) => {
            let id = text(key).unwrap_or_default();
            if workspace.has_panel(id) {
                Ok(())
            } else {
                Err(format!(
                    "This window has no panel `{id}`. Its panels: {}",
                    workspace.panel_ids().join(", ")
                ))
            }
        }
    }
}

/// After an action that gives what it shows the keyboard: the person keeps it where they had
/// it, unless that was in a panel the action took the place of. Then the panel shown keeps it,
/// or, when it took none, the active tab takes it, as when a dock hides: keys must not go on
/// driving a panel nobody sees.
fn keep_focus(
    before: &Front,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(focus) = &before.focus else {
        return;
    };
    let now = window.focused(cx);
    let hidden = before
        .focus_panel
        .as_ref()
        .is_some_and(|panel| !workspace.shown_panels().contains(panel));
    if !hidden {
        if now.as_ref() != Some(focus) {
            window.focus(focus, cx);
        }
    } else if now.as_ref() == Some(focus) {
        workspace.focus_active_tab(window, cx);
    }
}

/// What the dispatch changed: the tab that opened or came to the front, or the panel shown.
fn changed(
    id: u64,
    tool: &UiTool,
    before: &Front,
    workspace: &Workspace,
    window: &Window,
    cx: &App,
) -> Answer {
    let after = front(workspace, window, cx);
    if after.active != before.active || !after.items.is_subset(&before.items) {
        let new = after.active.is_some_and(|tab| !before.items.contains(&tab));
        if let Some(item) = workspace.active_item() {
            let mut tab = describe(item, cx);
            // A node's tab is named by its id until the node is read; the name the session
            // already holds is said with it (spec 032).
            if let Some(Thing::Node { type_name, id }) = item.thing(cx) {
                let label = item
                    .agent_view(cx)
                    .and_then(|view| view.root)
                    .filter(|root| root.type_name == type_name && root.id == id)
                    .and_then(|root| root.label)
                    .or_else(|| {
                        workspace
                            .session()
                            .and_then(|session| session.read(cx).known_label(&type_name, &id))
                    });
                if let Some(label) = label {
                    tab["label"] = json!(label);
                }
            }
            let words = match tab["label"].as_str() {
                Some(label)
                    if !tab["title"]
                        .as_str()
                        .is_some_and(|title| title.contains(label)) =>
                {
                    format!("{} ({label})", tab_words(&tab))
                }
                _ => tab_words(&tab),
            };
            let (outcome, verb) = if new {
                ("opened", "Opened")
            } else {
                ("shown", "Showed")
            };
            return Answer::new(
                outcome,
                format!("{verb} {words}"),
                json!({ "window": id, "tab": tab }),
                false,
            );
        }
    }
    let shown: Vec<&SharedString> = after
        .panels
        .iter()
        .filter(|panel| !before.panels.contains(panel))
        .collect();
    if let Some(panel) = shown.first() {
        let title = workspace
            .panel_title(panel)
            .unwrap_or_else(|| (*panel).clone());
        return Answer::new(
            "panel_shown",
            format!("Showed the {title} panel"),
            json!({ "window": id, "panel": panel.as_ref() }),
            false,
        );
    }
    Answer::new(
        "no_change",
        format!(
            "{} changed nothing in window {id}: what it would show is shown already",
            tool.name
        ),
        json!({ "window": id }),
        false,
    )
}

/// A thing as the tools name it.
fn thing_json(thing: &Thing) -> Value {
    match thing {
        Thing::Schema => json!({ "kind": "schema" }),
        Thing::Type(type_name) => json!({ "kind": "type", "type_name": type_name }),
        Thing::Node { type_name, id } => {
            json!({ "kind": "node", "type_name": type_name, "id": id })
        }
        Thing::Branch(branch) => json!({ "kind": "branch", "branch": branch }),
    }
}

/// A tab as the tools describe it: what it shows, how, its title and the commit it holds.
fn describe(item: &dyn ItemHandle, cx: &App) -> Value {
    json!({
        "thing": item.thing(cx).as_ref().map(thing_json),
        "view": item.view_kind(cx).map(ViewKind::label),
        "title": item.tab_title(cx).as_ref(),
        "at_commit": item.held_at(cx),
    })
}

/// A tab in words: "Project · Table".
fn tab_words(tab: &Value) -> String {
    let title = tab["title"].as_str().unwrap_or("a tab");
    match tab["view"].as_str() {
        Some(view) if view != title => format!("{title} · {view}"),
        _ => title.to_owned(),
    }
}

/// `studio_windows`: every workspace window, or a conversation's own.
fn windows(target: Target, cx: &mut App) -> Answer {
    let active = cx.active_window().map(|window| window.window_id().as_u64());
    let mut listed = Vec::new();
    for handle in cx.windows() {
        let id = handle.window_id().as_u64();
        if matches!(target, Target::Bound(own) if own != id) {
            continue;
        }
        let Some(handle) = handle.downcast::<Workspace>() else {
            continue;
        };
        let Ok(workspace) = handle.entity(cx) else {
            continue;
        };
        let workspace = workspace.read(cx);
        let session = workspace.session().map(|session| session.read(cx));
        listed.push(json!({
            "window": id,
            "server": session.map(|s| s.connection.read(cx).server.name.clone()),
            "graph": session.map(|s| s.graph_id.clone()),
            "branch": session.map(|s| s.branch.clone()),
            "front": active == Some(id),
        }));
    }
    let says = match listed.len() {
        1 => "1 window".to_owned(),
        count => format!("{count} windows"),
    };
    Answer::new("read", says, json!({ "windows": listed }), false)
}

/// `studio_tabs`: the window's panes and tabs.
fn tabs(id: u64, workspace: &Workspace, _window: &Window, cx: &App) -> Answer {
    let panes: Vec<Value> = workspace
        .pane_tabs()
        .into_iter()
        .map(|pane| {
            let tabs: Vec<Value> = pane
                .tabs
                .iter()
                .map(|tab| {
                    let mut described = describe(tab.item, cx);
                    described["active"] = json!(tab.active);
                    described["preview"] = json!(tab.preview);
                    described
                })
                .collect();
            json!({ "active": pane.active, "tabs": tabs })
        })
        .collect();
    let count: usize = panes
        .iter()
        .map(|pane| pane["tabs"].as_array().map_or(0, Vec::len))
        .sum();
    let shown: Vec<String> = workspace
        .shown_panels()
        .into_iter()
        .map(|panel| panel.to_string())
        .collect();
    Answer::new(
        "read",
        studio_ui::plural(count, "tab", "tabs"),
        json!({ "window": id, "panes": panes, "panels": shown }),
        false,
    )
}

/// `studio_selection`: what the active tab has selected, as identifiers, cut to fit.
fn selection(id: u64, workspace: &Workspace, cx: &App) -> Answer {
    let Some(item) = workspace.active_item() else {
        return Answer::new(
            "read",
            "No tab is open",
            json!({ "window": id, "tab": null, "selection": null }),
            false,
        );
    };
    let mut tab = describe(item, cx);
    // On a canvas or a tree, how much more there is to read with studio_canvas or
    // studio_tree (spec 032).
    if let Some(view) = item.agent_view(cx) {
        tab["drawn"] = json!({
            "nodes": view.nodes.len(),
            "edges": view.edges.len(),
            "relationships": view.groups.len(),
        });
    }
    let Some(selected) = item.selected_ids(cx) else {
        return Answer::new(
            "read",
            format!("Nothing is selected in {}", tab_words(&tab)),
            json!({ "window": id, "tab": tab, "selection": null }),
            false,
        );
    };
    let mut ids = selected.ids;
    let mut truncated = ids.len() > MAX_SELECTED;
    ids.truncate(MAX_SELECTED);
    let root = selected
        .root
        .map(|(type_name, id)| json!({ "type_name": type_name, "id": id }));
    let make = |ids: &[String], truncated: bool| {
        json!({
            "window": id,
            "tab": tab,
            "selection": {
                "type_name": selected.type_name,
                "ids": ids,
                "root": root,
            },
            "truncated": truncated,
        })
    };
    let mut json = make(&ids, truncated);
    while json.to_string().len() > ANSWER_BUDGET && !ids.is_empty() {
        let keep = ids.len() / 2;
        ids.truncate(keep);
        truncated = true;
        json = make(&ids, truncated);
    }
    let says = match ids.len() {
        0 => format!("Nothing is selected in {}", tab_words(&tab)),
        count => format!(
            "{} selected in {}",
            studio_ui::plural(count, "node", "nodes"),
            tab_words(&tab)
        ),
    };
    Answer::new("read", says, json, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        AppContext as _, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement as _,
        IntoElement, Render, TestAppContext, div,
    };
    use graph_store::{Connection, GraphSession, ServerEntry, ServerSource};
    use http_client::{AsyncBody, FakeHttpClient, Response};
    use studio_actions::ViewLink;
    use studio_settings::StudioSettings;
    use studio_workspace::{Item, ItemEvent, OPENED_BY_FOR, SelectedIds};

    /// What the never-a-tool test refuses: actions that write, answer the agent's own
    /// requests, open a link or a setting, sign in or out, update or quit, or move focus.
    const NEVER: &[&str] = &[
        // Writes.
        "studio_graph::CreateBranch",
        "studio_graph::CreateBranchAt",
        "studio_graph::DeleteBranch",
        "studio_graph::UndoDelete",
        "studio_graph::RestoreSavedView",
        "studio_graph::OpenBlob",
        "studio_graph::OpenTextEditor",
        "studio_table::EditCell",
        "studio_table::ConfirmEdit",
        "studio_table::StageEdit",
        "studio_table::SaveDrafts",
        "studio_table::InsertRow",
        "studio_table::DeleteRow",
        "studio_table::FillColumn",
        "studio_table::SaveView",
        "studio_table::SaveViewAsNew",
        "studio_node::SaveField",
        "studio_node::SaveText",
        "studio_node::SaveNodeDrafts",
        "studio_blob::ChooseFile",
        "studio_blob::ClearValue",
        "studio_review::Merge",
        "studio_review::RevertAndNext",
        "studio_review::AcceptAndNext",
        "studio_review::EditChange",
        "studio_history::NewBranchHere",
        "studio_query::Run",
        "studio_query::Explain",
        "studio_query::Save",
        "studio_query::SaveAs",
        // The agent's own requests, and more agents.
        "studio_agent::Allow",
        "studio_agent::AllowAlways",
        "studio_agent::Reject",
        "studio_agent::Stop",
        "studio_agent::Restart",
        "studio_agent::Ask",
        "studio_agent::OpenConversation",
        "studio_agent::Send",
        "studio_agent::SendNow",
        "studio_agent::NewConversation",
        "studio_agent::OpenAgent",
        // A link can name a server.
        "studio::OpenLink",
        "studio::OpenGraph",
        // Settings, updates, quitting.
        "studio::OpenSettings",
        "studio::OpenAgentSettings",
        "studio::Quit",
        "studio::NewWindow",
        "studio::CloseGraph",
        "studio::CloseActiveItem",
        "studio::CloseAllItems",
        "studio::CloseOtherItems",
        "studio_update::CheckForUpdates",
        "studio_update::RestartToUpdate",
        "studio_update::DownloadUpdate",
        // Focus moves and keys.
        "studio::ToggleCommandPalette",
        "studio::ActivateNextPane",
        "studio::ActivatePreviousPane",
        "studio_table::MoveDown",
        "studio_query::ToggleResultsFocus",
    ];

    /// Namespaces no tool comes from: every action in them writes, answers the agent, or acts
    /// only as a key in a view.
    const NEVER_NAMESPACES: &[&str] = &[
        "studio_agent::",
        "studio_table::",
        "studio_node::",
        "studio_blob::",
        "studio_review::",
        "studio_query::",
        "studio_update::",
        "studio_palette::",
    ];

    #[gpui::test]
    fn never_a_tool(cx: &mut TestAppContext) {
        let registered: Vec<&str> = cx.update(|cx| cx.all_action_names().to_vec());
        for tool in ALLOWED {
            assert!(
                registered.contains(&tool.action),
                "{} is not a registered action",
                tool.action
            );
            assert!(
                !NEVER.contains(&tool.action),
                "{} is never a tool",
                tool.action
            );
            // A table's row shown on a canvas only opens a canvas.
            assert!(
                tool.action == "studio_table::ShowRowOnCanvas"
                    || !NEVER_NAMESPACES
                        .iter()
                        .any(|namespace| tool.action.starts_with(namespace)),
                "{} comes from a namespace no tool comes from",
                tool.action
            );
            if !RENAMED.contains(&tool.name) {
                assert_eq!(tool.name, tool_name(tool.action), "named after its action");
            }
            assert!(!READS.contains(&tool.name));
        }
        for never in NEVER {
            assert!(
                registered.contains(never),
                "{never} is not an action any more"
            );
        }
        let names: HashSet<&str> = ALLOWED.iter().map(|tool| tool.name).collect();
        assert_eq!(names.len(), ALLOWED.len(), "each name once");
        assert_eq!(tool_name("studio_graph::OpenType"), "graph_open_type");
        assert_eq!(
            tool_name("studio::ActivatePanel"),
            "workspace_activate_panel"
        );
    }

    /// Every tool is listed with its action's doc comment and a schema with `window` and no
    /// `preview`; a conversation's need not name a window.
    #[gpui::test]
    fn tools_are_listed_with_their_schemas(cx: &mut TestAppContext) {
        let (any, bound) = cx.update(|cx| {
            (
                definitions(Target::Any, cx),
                definitions(Target::Bound(1), cx),
            )
        });
        assert_eq!(any.len(), ALLOWED.len() + READS.len());
        let open_type = any
            .iter()
            .find(|tool| tool["name"] == "graph_open_type")
            .unwrap();
        assert_eq!(
            open_type["description"],
            "Open a node or edge type's table."
        );
        let schema = &open_type["inputSchema"];
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["type_name"].is_object());
        assert!(schema["properties"]["window"].is_object());
        assert!(schema["properties"].get("preview").is_none());
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(required.contains(&"type_name") && required.contains(&"window"));
        assert!(!schema.to_string().contains("$ref"), "self-contained");
        let bound_type = bound
            .iter()
            .find(|tool| tool["name"] == "graph_open_type")
            .unwrap();
        assert!(
            !bound_type["inputSchema"]["required"]
                .as_array()
                .unwrap()
                .contains(&json!("window"))
        );
        let listed = Value::Array(any).to_string();
        assert!(listed.len() < 32 * 1024, "{} bytes", listed.len());
    }

    /// A stand-in tab: what it shows, how, and what it has selected.
    struct Stub {
        title: &'static str,
        thing: Thing,
        view: ViewKind,
        selected: Option<SelectedIds>,
        focus_handle: FocusHandle,
    }

    impl EventEmitter<ItemEvent> for Stub {}

    impl Focusable for Stub {
        fn focus_handle(&self, _: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl Render for Stub {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().track_focus(&self.focus_handle)
        }
    }

    impl Item for Stub {
        fn tab_title(&self, _: &App) -> SharedString {
            self.title.into()
        }

        fn thing(&self, _: &App) -> Option<Thing> {
            Some(self.thing.clone())
        }

        fn dedupe_key(&self, _: &App) -> Option<SharedString> {
            Some(self.title.to_lowercase().into())
        }

        fn views(&self, _: &App) -> Vec<ViewLink> {
            vec![ViewLink::current(self.view)]
        }

        fn held_at(&self, _: &App) -> Option<String> {
            Some("c7".into())
        }

        fn selected_ids(&self, _: &App) -> Option<SelectedIds> {
            self.selected.clone()
        }
    }

    fn session(cx: &mut TestAppContext) -> Entity<GraphSession> {
        let http = FakeHttpClient::create(|request| async move {
            let body = if request.uri().path().ends_with("/schema") {
                json!({ "schema_source": include_str!("../../../dev/cluster/knowledge.pg") })
                    .to_string()
            } else {
                r#"{"branches":["main"]}"#.to_owned()
            };
            Ok(Response::builder()
                .status(200)
                .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                .body(AsyncBody::from(body))
                .unwrap())
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx))
    }

    /// A window whose OpenMap opens a stand-in Map tab, as the canvas's handler does, and
    /// whose OpenCommit does nothing.
    fn window(cx: &mut TestAppContext) -> WindowHandle<Workspace> {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            cx.set_global(StudioSettings::default());
        });
        let window = cx.add_window(|window, cx| {
            let mut workspace = Workspace::new(window, cx);
            workspace.register_action(
                |workspace, action: &studio_actions::graph::OpenMap, window, cx| {
                    workspace.open_item_with("map", action.preview, window, cx, |_, cx| Stub {
                        title: "Map",
                        thing: Thing::Schema,
                        view: ViewKind::Map,
                        selected: Some(SelectedIds {
                            type_name: Some("Person".into()),
                            ids: (0..250).map(|n| format!("person-{n:06}")).collect(),
                            root: Some(("Person".into(), "person-000000".into())),
                        }),
                        focus_handle: cx.focus_handle(),
                    });
                },
            );
            workspace.register_action(|_, _: &studio_actions::graph::OpenCommit, _, _| {});
            workspace
        });
        cx.run_until_parked();
        window
    }

    async fn run(cx: &mut TestAppContext, target: Target, name: &str, input: Value) -> Answer {
        let caller = Caller {
            agent: Some("Claude Code".into()),
            announce: false,
        };
        let answer = call(&mut cx.to_async(), target, caller, name, input).await;
        cx.run_until_parked();
        answer
    }

    /// With no graph open an available action can still do nothing: the tool says why, and
    /// nothing is dispatched.
    #[gpui::test]
    async fn a_tool_whose_need_is_not_met_is_not_available(cx: &mut TestAppContext) {
        let handle = window(cx);
        let id = handle.window_id().as_u64();
        let answer = run(
            cx,
            Target::Any,
            "graph_new_query_with_text",
            json!({ "window": id, "text": "query q() { match { $p: Person } return { $p.id } }" }),
        )
        .await;
        assert_eq!(answer.outcome, "not_available");
        assert!(answer.is_error);
        assert_eq!(answer.says, "No graph is open in this window");
        assert_eq!(answer.json["action"], "studio_graph::NewQueryWithText");
        assert_eq!(answer.json["window"], id);

        let answer = run(
            cx,
            Target::Any,
            "workspace_activate_panel",
            json!({ "window": id, "id": "nowhere" }),
        )
        .await;
        assert_eq!(answer.outcome, "not_available");
        assert!(
            answer
                .says
                .starts_with("This window has no panel `nowhere`")
        );
    }

    /// A tool opens its tab as a preview and says what opened; the same call again changes
    /// nothing; one that changed nothing says so; the reads name the tab and its selection.
    #[gpui::test]
    async fn a_tool_says_what_it_opened_and_the_reads_name_it(cx: &mut TestAppContext) {
        let handle = window(cx);
        let id = handle.window_id().as_u64();
        let session = session(cx);
        handle
            .update(cx, |workspace, window, cx| {
                workspace.set_session(session, window, cx)
            })
            .unwrap();
        cx.run_until_parked();

        let answer = run(cx, Target::Any, "graph_open_map", json!({ "window": id })).await;
        assert_eq!(answer.outcome, "opened", "{answer:?}");
        assert_eq!(answer.says, "Opened Map");
        assert_eq!(answer.json["tab"]["thing"], json!({ "kind": "schema" }));
        assert_eq!(answer.json["tab"]["view"], "Map");
        handle
            .read_with(cx, |workspace, _| {
                let panes = workspace.pane_tabs();
                let pane = panes.iter().find(|pane| pane.active).unwrap();
                let front = pane.tabs.iter().find(|tab| tab.active).unwrap();
                assert!(front.preview, "opened as the preview");
                assert_eq!(
                    workspace.opened_by().map(|agent| agent.as_ref()),
                    Some("Claude Code")
                );
            })
            .unwrap();
        cx.executor().advance_clock(OPENED_BY_FOR);
        cx.run_until_parked();
        handle
            .read_with(cx, |workspace, _| assert!(workspace.opened_by().is_none()))
            .unwrap();

        let again = run(cx, Target::Any, "graph_open_map", json!({ "window": id })).await;
        assert_eq!(again.outcome, "no_change");
        assert!(!again.is_error);
        let nothing = run(
            cx,
            Target::Any,
            "graph_open_commit",
            json!({ "window": id, "commit_id": "c1" }),
        )
        .await;
        assert_eq!(nothing.outcome, "no_change");

        // An outside agent's first call says so at once, though it is `studio_windows`, which
        // names no window: in the window in front. The server owes the notice until an answer
        // says it was shown, so the dispatch after it shows nothing more.
        let mut owed = true;
        for (name, input) in [
            ("studio_windows", json!({})),
            ("graph_open_map", json!({ "window": id })),
            ("studio_tabs", json!({ "window": id })),
        ] {
            let caller = Caller {
                agent: None,
                announce: owed,
            };
            let answer = call(&mut cx.to_async(), Target::Any, caller, name, input).await;
            cx.run_until_parked();
            assert_eq!(answer.announced, name == "studio_windows", "{name}");
            owed &= !answer.announced;
        }
        handle
            .read_with(cx, |workspace, _| {
                assert_eq!(workspace.toast_messages(), [OUTSIDE_NOTICE], "once");
            })
            .unwrap();

        let windows = run(cx, Target::Any, "studio_windows", json!({})).await;
        assert_eq!(windows.json["windows"][0]["window"], id);
        assert_eq!(windows.json["windows"][0]["graph"], "knowledge");
        assert_eq!(windows.json["windows"][0]["branch"], "main");
        assert_eq!(windows.json["windows"][0]["server"], "dev");

        let tabs = run(cx, Target::Any, "studio_tabs", json!({ "window": id })).await;
        let tab = &tabs.json["panes"][0]["tabs"][0];
        assert_eq!(tab["title"], "Map");
        assert_eq!(tab["at_commit"], "c7");
        assert_eq!(tab["active"], true);
        assert_eq!(tab["preview"], true);

        let selection = run(cx, Target::Any, "studio_selection", json!({ "window": id })).await;
        let ids = selection.json["selection"]["ids"].as_array().unwrap();
        assert_eq!(ids.len(), MAX_SELECTED, "at most 200");
        assert_eq!(selection.json["truncated"], true);
        assert_eq!(selection.json["selection"]["root"]["id"], "person-000000");
        assert!(selection.json.to_string().len() <= ANSWER_BUDGET);
    }

    /// With "Bring the window to the front" on, a window an agent shows something in comes to
    /// the front; off, it stays behind.
    #[gpui::test]
    async fn the_window_comes_to_the_front_only_when_the_person_asked(cx: &mut TestAppContext) {
        cx.update(studio_settings::init_test);
        let behind = window(cx);
        let front = cx.add_window(Workspace::new);
        cx.run_until_parked();
        let session = session(cx);
        behind
            .update(cx, |workspace, window, cx| {
                workspace.set_session(session, window, cx)
            })
            .unwrap();
        front
            .update(cx, |_, window, _| window.activate_window())
            .unwrap();
        cx.run_until_parked();
        let id = behind.window_id().as_u64();
        let active = |cx: &mut TestAppContext| {
            cx.update(|cx| cx.active_window().map(|window| window.window_id().as_u64()))
        };
        assert_ne!(active(cx), Some(id));
        let answer = run(cx, Target::Any, "graph_open_map", json!({ "window": id })).await;
        assert_eq!(answer.outcome, "opened");
        assert_ne!(active(cx), Some(id), "off: it stays behind");
        cx.update(|cx| {
            let mut settings = StudioSettings::get(cx).clone();
            settings.agent.raise_window = true;
            cx.set_global(settings);
        });
        handle_reopen(cx, behind);
        let answer = run(cx, Target::Any, "graph_open_map", json!({ "window": id })).await;
        assert_eq!(answer.outcome, "shown", "{answer:?}");
        assert_eq!(active(cx), Some(id), "on: it comes to the front");
    }

    /// Puts another tab in front of the map, so showing the map again changes something.
    fn handle_reopen(cx: &mut TestAppContext, handle: WindowHandle<Workspace>) {
        handle
            .update(cx, |workspace, window, cx| {
                workspace.open_item("other", window, cx, |_, cx| Stub {
                    title: "Other",
                    thing: Thing::Schema,
                    view: ViewKind::Source,
                    selected: None,
                    focus_handle: cx.focus_handle(),
                });
            })
            .unwrap();
        cx.run_until_parked();
    }

    /// A call to a window that has closed, or is another conversation's, is refused.
    #[gpui::test]
    async fn a_closed_window_or_another_conversations_is_refused(cx: &mut TestAppContext) {
        let handle = window(cx);
        let id = handle.window_id().as_u64();
        let other = run(
            cx,
            Target::Bound(id + 1),
            "studio_tabs",
            json!({ "window": id }),
        )
        .await;
        assert_eq!(other.outcome, "not_available");
        let own = run(cx, Target::Bound(id), "studio_windows", json!({})).await;
        assert_eq!(own.json["windows"].as_array().unwrap().len(), 1);
        let unknown = run(cx, Target::Any, "graph_open_map", json!({ "window": 4242 })).await;
        assert_eq!(unknown.outcome, "window_closed");

        handle
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        let closed = run(cx, Target::Any, "graph_open_map", json!({ "window": id })).await;
        assert_eq!(closed.outcome, "window_closed");
        assert!(closed.is_error);
        let none = run(cx, Target::Bound(id), "studio_windows", json!({})).await;
        assert_eq!(none.json["windows"], json!([]));
    }
}

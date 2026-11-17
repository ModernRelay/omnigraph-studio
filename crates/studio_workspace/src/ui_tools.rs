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
//! Layer 4, with the workspace it reads. The server that speaks MCP is `ui_tool_server`.

use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    App, AsyncApp, Context, EntityId, Focusable as _, Global, SharedString, Window, WindowHandle,
};
use serde_json::{Map, Value, json};
use studio_actions::{Thing, ViewKind};

use crate::{ToastKind, Workspace};

/// The most node ids `studio_selection` names.
pub const MAX_SELECTED: usize = 200;

/// The most a tool's answer may take as JSON. The server's whole answer is capped at 64 KiB
/// (spec 032, The server): the answer travels as text inside a JSON-RPC result, whose escapes
/// can take more room than the JSON itself, so a tool's own answer keeps well inside it.
pub const ANSWER_BUDGET: usize = 40 * 1024;

/// How long the window shows "Opened by <agent>" after a tool opened or showed something.
pub const OPENED_BY_FOR: Duration = Duration::from_secs(4);

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
    /// A filter (`filter_id`) opens a new table of the type named by the argument, never
    /// changes an open one's: the type's handler would filter a table open already (spec
    /// 032, Focus).
    NoRefilter(&'static str),
}

/// One tool: an allowed action under its tool name.
#[derive(Debug)]
pub struct UiTool {
    /// The tool's name: the action's, with `::` as `_`, the `studio_` namespaces shortened, and
    /// the workspace's own actions (namespace `studio`) under `workspace_`.
    pub name: &'static str,
    /// The action it dispatches, by its registered name.
    pub action: &'static str,
    needs: &'static [Need],
}

/// The actions an agent may call, and nothing else: the set is opt-in, so a new action is not
/// a tool until it is added here, with what it needs. Each only opens or shows something. The
/// test `never_a_tool` fails if one that writes, answers the agent's own requests, opens a link
/// or a setting, or moves focus is added.
pub static ALLOWED: &[UiTool] = &[
    UiTool {
        name: "graph_open_type",
        action: "studio_graph::OpenType",
        needs: &[
            Need::Session,
            Need::Type("type_name"),
            Need::NoRefilter("type_name"),
        ],
    },
    UiTool {
        name: "graph_open_node",
        action: "studio_graph::OpenNode",
        needs: &[Need::Session, Need::Type("type_name"), Need::Text("id")],
    },
    UiTool {
        name: "graph_open_node_page",
        action: "studio_graph::OpenNodePage",
        needs: &[Need::Session, Need::Type("type_name"), Need::Text("id")],
    },
    UiTool {
        name: "graph_show_node_on_canvas",
        action: "studio_graph::ShowNodeOnCanvas",
        needs: &[Need::Session, Need::Type("type_name"), Need::Text("id")],
    },
    UiTool {
        name: "graph_show_node_in_tree",
        action: "studio_graph::ShowNodeInTree",
        needs: &[Need::Session, Need::Type("type_name"), Need::Text("id")],
    },
    UiTool {
        name: "graph_open_map",
        action: "studio_graph::OpenMap",
        needs: &[Need::Session],
    },
    UiTool {
        name: "graph_open_source",
        action: "studio_graph::OpenSource",
        needs: &[Need::Session, Need::MaybeType("type_name")],
    },
    UiTool {
        name: "graph_open_branch",
        action: "studio_graph::OpenBranch",
        needs: &[Need::Session, Need::Branch("branch")],
    },
    UiTool {
        name: "graph_open_branch_history",
        action: "studio_graph::OpenBranchHistory",
        needs: &[Need::Session, Need::Branch("branch")],
    },
    UiTool {
        name: "graph_open_commit",
        action: "studio_graph::OpenCommit",
        needs: &[Need::Session, Need::Text("commit_id")],
    },
    UiTool {
        name: "graph_new_query_with_text",
        action: "studio_graph::NewQueryWithText",
        needs: &[Need::Session],
    },
    UiTool {
        name: "workspace_activate_panel",
        action: "studio::ActivatePanel",
        needs: &[Need::Panel("id")],
    },
];

/// The tools that only read the window.
pub const READS: [&str; 3] = ["studio_windows", "studio_tabs", "studio_selection"];

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
    ];
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
            }
            if let Some(required) = object.get_mut("required").and_then(Value::as_array_mut) {
                required.retain(|name| name != "preview");
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
    /// `not_available`, `window_closed`, `invalid_input`, `unknown_tool`, `busy`.
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
    let input = match input {
        Value::Null => json!({}),
        Value::Object(object) => Value::Object(object),
        _ => {
            return Answer::error(
                "invalid_input",
                "A tool's arguments are a JSON object.",
                None,
            );
        }
    };
    if name == "studio_windows" {
        return cx.update(|cx| windows(target, cx));
    }
    let tool = tool(name);
    if tool.is_none() && !READS.contains(&name) {
        return Answer::error(
            "unknown_tool",
            format!("Studio has no tool `{name}`. tools/list lists them."),
            None,
        );
    }
    let asked = input.get("window").and_then(Value::as_u64);
    let id = match (target, asked) {
        (Target::Bound(own), Some(asked)) if asked != own => {
            return Answer::error(
                "not_available",
                format!("This conversation shows things only in its own window, {own}."),
                Some(asked),
            );
        }
        (Target::Bound(own), _) => own,
        (Target::Any, Some(asked)) => asked,
        (Target::Any, None) => {
            return Answer::error(
                "invalid_input",
                "Name the window to act in: `window`, an id from studio_windows.",
                None,
            );
        }
    };
    let Some(handle) = cx.update(|cx| workspace_window(id, cx)) else {
        return Answer::window_closed(id);
    };
    let mut answer = target_window(cx, handle, id, tool, name, &caller, input).await;
    // An outside agent's first call to a window says so there, whatever the call did: a read,
    // a refusal or an action. A call that named no open window has shown nothing yet.
    if caller.announce && answer.outcome != "window_closed" {
        let shown = handle.update(cx, |workspace, _, cx| {
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
    answer
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
    // The action ran as the first update's effects were flushed.
    let second = handle.update(cx, |workspace, window, cx| {
        let answer = changed(id, tool, &before, workspace, cx);
        if matches!(answer.outcome, "opened" | "shown" | "panel_shown") {
            let agent = caller
                .agent
                .clone()
                .unwrap_or_else(|| SharedString::from("an agent"));
            workspace.show_opened_by(agent, window, cx);
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
    active: Option<EntityId>,
    items: HashSet<EntityId>,
    panels: Vec<SharedString>,
}

fn front(workspace: &Workspace) -> Front {
    Front {
        active: workspace.active_item().map(|item| item.item_id()),
        items: workspace.all_items().map(|item| item.item_id()).collect(),
        panels: workspace.shown_panels(),
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
    let mut data = input.as_object().cloned().unwrap_or_default();
    data.remove("window");
    let mut generator = schemars::SchemaGenerator::default();
    let takes_preview = cx
        .action_schema_by_name(tool.action, &mut generator)
        .flatten()
        .is_some_and(|schema| schema.to_value().pointer("/properties/preview").is_some());
    if takes_preview {
        data.insert("preview".into(), json!(true));
    }
    let action = match cx.build_action(tool.action, Some(Value::Object(data))) {
        Ok(action) => action,
        Err(error) => {
            return Err(Answer::error(
                "invalid_input",
                format!("The arguments do not fit {}: {error}", tool.name),
                Some(id),
            ));
        }
    };
    // With nothing focused an action reaches only global handlers, so the workspace takes
    // focus first, as a click in the window would. Focus the person has is left where it is.
    if window.focused(cx).is_none() {
        let focus = workspace.focus_handle(cx);
        window.focus(&focus, cx);
    }
    if !window.is_action_available(action.as_ref(), cx) {
        return Err(not_available(format!(
            "{} is not available in this window now.",
            tool.name
        )));
    }
    let before = front(workspace);
    window.dispatch_action(action, cx);
    Ok(before)
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
        Need::NoRefilter(key) => {
            let filtered = input
                .get("filter_id")
                .is_some_and(|filter| !filter.is_null());
            let type_name = text(key).unwrap_or_default();
            let at_commit = input.get("at_commit").and_then(Value::as_str);
            let open = workspace.all_items().any(|item| {
                item.thing(cx) == Some(Thing::Type(type_name.to_owned()))
                    && item.view_kind(cx) == Some(ViewKind::Table)
                    && item.held_at(cx).as_deref() == at_commit
            });
            if filtered && open {
                Err(format!(
                    "A {type_name} table is open in this window, and a tool does not change an \
                     open tab's filters. Call graph_open_type without filter_id to show it, or \
                     graph_show_node_in_tree to walk from the node"
                ))
            } else {
                Ok(())
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

/// What the dispatch changed: the tab that opened or came to the front, or the panel shown.
fn changed(id: u64, tool: &UiTool, before: &Front, workspace: &Workspace, cx: &App) -> Answer {
    let after = front(workspace);
    if after.active != before.active || !after.items.is_subset(&before.items) {
        let new = after.active.is_some_and(|tab| !before.items.contains(&tab));
        if let Some(item) = workspace.active_item() {
            let tab = describe(item, cx);
            let words = tab_words(&tab);
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
fn describe(item: &dyn crate::ItemHandle, cx: &App) -> Value {
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
    let active_pane = workspace.active_pane;
    let panes: Vec<Value> = workspace
        .center
        .panes()
        .into_iter()
        .filter_map(|pane_id| workspace.pane(pane_id))
        .map(|pane| {
            let tabs: Vec<Value> = pane
                .items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    let mut tab = describe(item.as_ref(), cx);
                    tab["active"] = json!(index == pane.active);
                    tab["preview"] = json!(pane.is_preview(index));
                    tab
                })
                .collect();
            json!({ "active": pane.id == active_pane, "tabs": tabs })
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
    let tab = describe(item, cx);
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

/// The tool server's address, which a conversation's agent is given (spec 032, The panel's
/// agent): `studio` sets it when the server listens, so the agent panel reaches the server
/// without depending on it.
#[derive(Clone)]
pub struct UiToolEndpoint(Rc<MakeEntry>);

/// Makes a conversation's entry for its window, by the window's id.
type MakeEntry = dyn Fn(u64, &mut App) -> Option<UiToolEntry>;

impl Global for UiToolEndpoint {}

impl UiToolEndpoint {
    pub fn new(entry: impl Fn(u64, &mut App) -> Option<UiToolEntry> + 'static) -> Self {
        Self(Rc::new(entry))
    }

    /// A new entry for a conversation in the window `window`, with a token of its own bound to
    /// that window; `None` while no server listens.
    pub fn entry_for(window: u64, cx: &mut App) -> Option<UiToolEntry> {
        let endpoint = cx.try_global::<Self>()?.clone();
        (endpoint.0)(window, cx)
    }
}

/// A conversation's way to the tool server: its address and a token bound to its window. The
/// token stops working when the entry is dropped, as the conversation ends.
pub struct UiToolEntry {
    pub url: String,
    pub token: String,
    revoke: Option<Box<dyn FnOnce()>>,
}

impl UiToolEntry {
    pub fn new(url: String, token: String, revoke: impl FnOnce() + 'static) -> Self {
        Self {
            url,
            token,
            revoke: Some(Box::new(revoke)),
        }
    }
}

impl std::fmt::Debug for UiToolEntry {
    /// The token is never written out.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UiToolEntry")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl Drop for UiToolEntry {
    fn drop(&mut self) {
        if let Some(revoke) = self.revoke.take() {
            revoke();
        }
    }
}

/// The MCP server name a conversation's agent is given the tools under. The adapters title a
/// call `mcp__studio__<tool>` (Claude) or `mcp.studio.<tool>` (Codex).
pub const SERVER_NAME: &str = "studio";

/// The tool a call's title names, when it is one of Studio's on [`SERVER_NAME`].
pub fn studio_call(title: &str) -> Option<&str> {
    let tool = title
        .strip_prefix("mcp__studio__")
        .or_else(|| title.strip_prefix("mcp.studio."))?;
    let tool = tool.split_whitespace().next().unwrap_or(tool);
    is_tool(tool).then_some(tool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Item, ItemEvent, SelectedIds};
    use gpui::{
        AppContext as _, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement as _,
        IntoElement, Render, TestAppContext, div,
    };
    use graph_store::{Connection, GraphSession, ServerEntry, ServerSource};
    use http_client::{AsyncBody, FakeHttpClient, Response};
    use studio_actions::ViewLink;
    use studio_settings::StudioSettings;

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
            assert!(
                !NEVER_NAMESPACES
                    .iter()
                    .any(|namespace| tool.action.starts_with(namespace)),
                "{} comes from a namespace no tool comes from",
                tool.action
            );
            assert_eq!(tool.name, tool_name(tool.action), "named after its action");
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
                let pane = workspace.pane(workspace.active_pane).unwrap();
                assert!(pane.is_preview(pane.active), "opened as the preview");
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

        // An outside agent's first call to a window says so there; `studio_windows`, which
        // names none, leaves it for the next call.
        let outside = Caller {
            agent: None,
            announce: true,
        };
        let listed = call(
            &mut cx.to_async(),
            Target::Any,
            outside.clone(),
            "studio_windows",
            json!({}),
        )
        .await;
        cx.run_until_parked();
        assert!(!listed.announced);
        handle
            .read_with(cx, |workspace, _| {
                assert!(workspace.toast_messages().is_empty())
            })
            .unwrap();
        let announced = call(
            &mut cx.to_async(),
            Target::Any,
            Caller {
                agent: None,
                announce: true,
            },
            "graph_open_map",
            json!({ "window": id }),
        )
        .await;
        cx.run_until_parked();
        assert_eq!(announced.outcome, "no_change");
        assert!(announced.announced);
        handle
            .read_with(cx, |workspace, _| {
                assert_eq!(workspace.toast_messages(), [OUTSIDE_NOTICE]);
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

    #[test]
    fn a_studio_call_is_known_by_either_title() {
        assert_eq!(
            studio_call("mcp__studio__graph_open_type"),
            Some("graph_open_type")
        );
        assert_eq!(
            studio_call("mcp.studio.studio_windows"),
            Some("studio_windows")
        );
        assert_eq!(studio_call("mcp__studio__run_query"), None, "not a tool");
        assert_eq!(studio_call("mcp__other__graph_open_type"), None);
    }
}

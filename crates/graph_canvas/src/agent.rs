//! What an agent does on a canvas walk (spec 032, "The canvas and the tree"): the actions a
//! person reaches only by pointer before (selecting a node by its id, expanding several nodes'
//! neighbourhoods, collapsing, showing more, hiding, pinning, starting a walk from a node), and
//! what an agent reads of the canvas: identifiers and the names the person sees, never a
//! property value.

use serde_json::{Value, json};
use studio_actions::NodeId;
use studio_actions::canvas::{
    CollapseNodes, ExpandNeighbourhood, HideNodes, PinNodes, RerootHere, SelectNode, ShowHidden,
    ShowMore, UnpinNodes,
};
use studio_workspace::{AgentEdge, AgentGroup, AgentNode, AgentRefusal, AgentView};

use super::*;

/// What a call names a node not on the canvas with.
const NOT_DRAWN: &str = "Not on this canvas; expand a neighbour or use graph_show_node_on_canvas";

fn node_ref(node: &NodeId) -> NodeRef {
    NodeRef::new(node.type_name.clone(), node.id.clone())
}

fn pair(node: &NodeRef) -> (String, String) {
    (node.type_name.clone(), node.id.clone())
}

impl GraphCanvas {
    /// The nodes an action names, else the marked ones, else the selected one.
    fn target_ids(&self, nodes: &[NodeId]) -> Vec<SceneId> {
        if !nodes.is_empty() {
            return nodes
                .iter()
                .map(|node| SceneId::Node(node_ref(node)))
                .collect();
        }
        if !self.marked.is_empty() {
            let mut marked: Vec<SceneId> = self.marked.iter().cloned().collect();
            marked.sort_by_key(|id| self.index_of(id));
            return marked;
        }
        self.selected.iter().cloned().collect()
    }

    /// The nodes among [`Self::target_ids`].
    fn target_nodes(&self, nodes: &[NodeId]) -> Vec<NodeRef> {
        self.target_ids(nodes)
            .into_iter()
            .filter_map(|id| match id {
                SceneId::Node(node) => Some(node),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn select_node(
        &mut self,
        action: &SelectNode,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // None named: the node the walk starts from.
        let node = if action.id.is_empty() {
            let Some(root) = self.exploration.read(cx).root().cloned() else {
                return;
            };
            root
        } else {
            NodeRef::new(action.type_name.clone(), action.id.clone())
        };
        let id = SceneId::Node(node);
        if let Some(index) = self.index_of(&id) {
            self.select(Some(index), cx);
        }
    }

    /// Expands the nodes' neighbourhoods through the exploration, as Expand all does: the
    /// same reads in batches, the same node cap, and the same question past what is opened
    /// without asking, unless the action brings its answer (`confirm`).
    pub(crate) fn expand_neighbourhood(
        &mut self,
        action: &ExpandNeighbourhood,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.draws_exploration() {
            return;
        }
        let expansion = &action.expansion;
        let nodes = self.target_nodes(&expansion.nodes);
        if nodes.is_empty() {
            return;
        }
        let plan = self
            .exploration
            .read(cx)
            .plan_expand(&nodes, expansion.relationships.as_deref());
        let limit = expansion.limit_per_relationship.unwrap_or(HUB_STEP).max(1);
        let exploration = self.exploration.clone();
        let relationships = expansion.relationships.clone();
        let (depth, confirm) = (expansion.depth.max(1), expansion.confirm);
        let asks = plan.opening.asks() && !confirm;
        let (opening_nodes, opening_reads) = (plan.opening.nodes, plan.opening.reads);
        let expand = move |_: &mut Window, cx: &mut App| {
            exploration.update(cx, |e, cx| {
                e.expand_planned(&plan, relationships, depth, limit, confirm, cx)
            });
        };
        if asks {
            studio_workspace::confirm_expand(window, cx, opening_nodes, opening_reads, expand);
        } else {
            expand(window, cx);
        }
    }

    pub(crate) fn collapse_nodes(
        &mut self,
        action: &CollapseNodes,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ids = self.target_ids(&action.nodes);
        self.close_all(&ids, cx);
    }

    /// Shows more of a relationship's neighbours, as its "+K more" does.
    pub(crate) fn show_more_of(
        &mut self,
        action: &ShowMore,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(key) = self.more_key(action, cx) {
            self.show_more(key, cx);
        }
    }

    /// The relationship an action shows more of: the one named, else the selected one's.
    fn more_key(&self, action: &ShowMore, cx: &App) -> Option<GroupKey> {
        match (&action.node, &action.relationship) {
            (Some(node), Some(relationship)) => {
                let schema = self.session.read(cx).schema_model()?;
                let group = schema
                    .relationships_of(&node.type_name)
                    .iter()
                    .find(|group| graph_store::relationship_key(group) == *relationship)?
                    .clone();
                Some(GroupKey::new(node_ref(node), group))
            }
            _ => match self.selected.as_ref()? {
                SceneId::Hub(key) | SceneId::More(key) => Some(key.clone()),
                _ => None,
            },
        }
    }

    pub(crate) fn hide_nodes(
        &mut self,
        action: &HideNodes,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A query's result picture draws its result whole.
        if !self.draws_exploration() {
            return;
        }
        let root = self.exploration.read(cx).root().cloned();
        let nodes: Vec<NodeRef> = self
            .target_nodes(&action.nodes)
            .into_iter()
            .filter(|node| Some(node) != root.as_ref())
            .collect();
        if nodes.is_empty() {
            return;
        }
        if let Some(SceneId::Node(selected)) = &self.selected
            && nodes.contains(selected)
        {
            self.selected = None;
        }
        self.marked
            .retain(|id| !matches!(id, SceneId::Node(node) if nodes.contains(node)));
        self.hidden.extend(nodes);
        cx.emit(ItemEvent::Selected);
        self.rebuild(cx);
    }

    pub(crate) fn show_hidden(&mut self, _: &ShowHidden, _: &mut Window, cx: &mut Context<Self>) {
        if !self.hidden.is_empty() {
            self.hidden.clear();
            self.rebuild(cx);
        }
    }

    pub(crate) fn pin_nodes(&mut self, action: &PinNodes, _: &mut Window, cx: &mut Context<Self>) {
        let ids = self.target_ids(&action.nodes);
        self.pin_marked(&ids, cx);
    }

    pub(crate) fn unpin_nodes(
        &mut self,
        action: &UnpinNodes,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ids = self.target_ids(&action.nodes);
        self.unpin(&ids, cx);
    }

    /// Starts a walk from the selected node in a tab of its own, as the tree's Re-root does and
    /// the chip menu's "Canvas from here": this walk stays as it is.
    pub(crate) fn reroot_here(
        &mut self,
        _: &RerootHere,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(SceneId::Node(node)) = self.selected.clone() else {
            return;
        };
        window.dispatch_action(
            Box::new(ShowNodeOnCanvas {
                type_name: node.type_name,
                id: node.id,
                at_commit: self.held_at(),
                preview: true,
            }),
            cx,
        );
    }

    /// What an agent reads of the canvas: its nodes with the names drawn, the edges between
    /// them by relationship, its relationships open or not, what is selected, its zoom and
    /// whether the node cap left nodes out. A schema canvas shows types, which `studio_tabs`
    /// and the schema tools name, and is not read here.
    pub(crate) fn agent_view_of(&self, cx: &App) -> Option<AgentView> {
        if self.schema_walk.is_some() {
            return None;
        }
        let exploration = self.exploration.read(cx);
        let scene = &self.scene;
        let node_of = |index: usize| match &scene.nodes.get(index)?.kind {
            SceneKind::Node { node, .. } => Some(node),
            _ => None,
        };
        let nodes = scene
            .nodes
            .iter()
            .filter_map(|chip| match &chip.kind {
                SceneKind::Node { node, .. } => Some(AgentNode {
                    type_name: node.type_name.clone(),
                    id: node.id.clone(),
                    label: Some(chip.label.clone()).filter(|label| *label != node.id),
                }),
                _ => None,
            })
            .collect();
        let mut edges = Vec::new();
        for edge in &scene.edges {
            match (&scene.nodes[edge.from].kind, node_of(edge.to)) {
                (SceneKind::Hub { key, .. }, Some(to)) => edges.push(AgentEdge {
                    from: pair(&key.node),
                    to: pair(to),
                    relationship: Some(graph_store::relationship_key(&key.group)),
                }),
                (SceneKind::Node { node, .. }, Some(to)) => edges.push(AgentEdge {
                    from: pair(node),
                    to: pair(to),
                    relationship: None,
                }),
                _ => {}
            }
        }
        let groups = scene
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(index, chip)| match &chip.kind {
                SceneKind::Hub { key, count, open } => Some(AgentGroup {
                    node: pair(&key.node),
                    relationship: graph_store::relationship_key(&key.group),
                    other_type: key.group.other_type.clone(),
                    count: *count,
                    expanded: *open,
                    shown: scene
                        .edges
                        .iter()
                        .filter(|edge| edge.from == index && node_of(edge.to).is_some())
                        .count(),
                }),
                _ => None,
            })
            .collect();
        let selected = self.selected.as_ref().map(|id| {
            let label = self
                .index_of(id)
                .map(|index| scene.nodes[index].label.clone());
            match id {
                SceneId::Node(node) | SceneId::Note(node) => json!({
                    "kind": "node",
                    "type_name": node.type_name,
                    "id": node.id,
                    "label": label,
                }),
                SceneId::Hub(key) | SceneId::More(key) => json!({
                    "kind": if matches!(id, SceneId::Hub(_)) { "relationship" } else { "more" },
                    "node": { "type_name": key.node.type_name, "id": key.node.id },
                    "relationship": graph_store::relationship_key(&key.group),
                }),
            }
        });
        let root = exploration.root().map(|root| AgentNode {
            type_name: root.type_name.clone(),
            id: root.id.clone(),
            label: exploration.state().label(root).map(str::to_owned),
        });
        let mut facts = serde_json::Map::new();
        facts.insert("zoom".into(), json!((self.zoom * 100.).round() / 100.));
        // Where the view is centred, in the layout's units: it says the camera moved.
        facts.insert(
            "center".into(),
            json!([self.center.x.round(), self.center.y.round()]),
        );
        facts.insert(
            "history".into(),
            json!({
                "back": self.selected_index().and_then(|at| self.reached_from(at)).is_some(),
                "forward": self.walked.len(),
            }),
        );
        facts.insert("marked".into(), json!(self.marked.len()));
        facts.insert("pinned".into(), json!(self.pinned.len()));
        let mut hidden: Vec<Value> = self
            .hidden
            .iter()
            .map(|node| json!({ "type_name": node.type_name, "id": node.id }))
            .collect();
        hidden.truncate(studio_workspace::ui_tools::MAX_CHANGED);
        facts.insert("hidden".into(), Value::Array(hidden));
        if let Some((left_out, most)) = &scene.capped {
            facts.insert(
                "node_cap".into(),
                json!({ "cap": scene.cap, "left_out": left_out, "mostly": most }),
            );
        }
        Some(AgentView {
            root,
            nodes,
            edges,
            groups,
            selected,
            capped: scene.capped.is_some(),
            stopped: exploration
                .deeper_stopped()
                .map(|opening| (opening.nodes, opening.reads)),
            facts,
        })
    }

    /// The canvas is still reading or laying out what a call opened.
    pub(crate) fn agent_busy_now(&self, cx: &App) -> bool {
        let exploration = self.exploration.read(cx);
        self.layout_task.is_some()
            || self.next_layout.is_some()
            || self._loaded.is_some()
            || self.layout_owed
            || self.follow.is_some()
            || exploration.reads_out() > 0
            || exploration.expanding()
    }

    /// Whether the canvas takes an agent's call: the nodes it names are drawn, the relationship
    /// named is theirs, and an expansion past what is opened without asking brings its answer.
    pub(crate) fn agent_check_call(
        &self,
        tool: &str,
        input: &Value,
        cx: &App,
    ) -> Result<(), AgentRefusal> {
        let named: Vec<NodeId> = input
            .get("nodes")
            .and_then(|nodes| serde_json::from_value(nodes.clone()).ok())
            .unwrap_or_default();
        let missing: Vec<String> = named
            .iter()
            .filter(|node| self.index_of(&SceneId::Node(node_ref(node))).is_none())
            .map(|node| format!("{} {}", node.type_name, node.id))
            .collect();
        if !missing.is_empty() {
            return Err(AgentRefusal::not_available(format!(
                "{NOT_DRAWN}: {}",
                missing.join(", ")
            )));
        }
        let targets = || self.target_nodes(&named);
        match tool {
            "canvas_expand" | "canvas_collapse" | "canvas_hide_nodes" | "canvas_pin_nodes"
            | "canvas_unpin_nodes"
                if targets().is_empty() =>
            {
                return Err(AgentRefusal::not_available(
                    "Name the nodes (`nodes`), or select or mark some first",
                ));
            }
            "canvas_expand" | "canvas_hide_nodes" | "canvas_show_hidden"
                if !self.draws_exploration() =>
            {
                return Err(AgentRefusal::not_available(
                    "This canvas shows a query's result whole; expand or hide on a walk from a \
                     node (graph_show_node_on_canvas)",
                ));
            }
            "canvas_expand" => {
                let expansion: studio_actions::Expansion =
                    serde_json::from_value(input.clone()).unwrap_or_default();
                if !(1..=2).contains(&expansion.depth) {
                    return Err(AgentRefusal::not_available("`depth` is 1 or 2"));
                }
                let plan = self
                    .exploration
                    .read(cx)
                    .plan_expand(&targets(), expansion.relationships.as_deref());
                if !plan.unknown.is_empty() {
                    let schema = self.session.read(cx).schema_model();
                    let theirs: Vec<String> = targets()
                        .iter()
                        .flat_map(|node| {
                            schema
                                .as_ref()
                                .map(|schema| {
                                    schema
                                        .relationships_of(&node.type_name)
                                        .iter()
                                        .map(graph_store::relationship_key)
                                        .collect::<Vec<_>>()
                                })
                                .unwrap_or_default()
                        })
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .collect();
                    return Err(AgentRefusal::not_available(format!(
                        "None of the nodes has {}. Theirs: {}",
                        plan.unknown.join(", "),
                        theirs.join(", ")
                    )));
                }
                if plan.opening.asks() && !expansion.confirm {
                    return Err(AgentRefusal {
                        outcome: "needs_confirmation",
                        says: format!(
                            "This would open {} nodes and ask for {} reads, past what the \
                             canvas opens without asking the person. Nothing was opened. Call \
                             again with confirm: true to open it.",
                            plan.opening.nodes, plan.opening.reads
                        ),
                        json: json!({
                            "needs_confirmation": {
                                "nodes": plan.opening.nodes,
                                "reads": plan.opening.reads,
                            }
                        }),
                    });
                }
            }
            "canvas_show_more" => {
                let action: ShowMore = serde_json::from_value(input.clone()).unwrap_or_default();
                if let Some(node) = &action.node
                    && self.index_of(&SceneId::Node(node_ref(node))).is_none()
                {
                    return Err(AgentRefusal::not_available(NOT_DRAWN));
                }
                let Some(key) = self.more_key(&action, cx) else {
                    return Err(AgentRefusal::not_available(
                        "Name the node and its relationship (`out:WorksAt`), or select a \
                         relationship or its \"+K more\" first",
                    ));
                };
                if self.index_of(&SceneId::Hub(key)).is_none() {
                    return Err(AgentRefusal::not_available(
                        "That relationship is not open on the canvas; open it with \
                         canvas_expand and `relationships`",
                    ));
                }
            }
            _ => {}
        }
        Ok(())
    }
}

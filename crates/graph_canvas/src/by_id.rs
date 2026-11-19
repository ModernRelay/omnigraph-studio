//! The canvas's actions that name their nodes by id (spec 032, "The canvas and the tree"):
//! what a person reached only by pointer before (selecting a node, expanding several nodes'
//! neighbourhoods, collapsing, showing more, hiding, pinning, starting a walk from a node).
//! With them, what the canvas says of itself to a reader that does not draw it: its outline
//! (identifiers and the names the person sees, never a property value), whether it is still
//! settling, and why it would not do what an action asks.
//!
//! Nothing here knows who asks. The palette, a key and a UI tool reach the same handlers.

use gpui::Action;
use serde_json::{Value, json};
use studio_actions::NodeId;
use studio_actions::canvas::{
    CollapseNodes, ExpandNeighbourhood, HideNodes, PinNodes, RerootHere, SelectNode, ShowHidden,
    ShowMore, UnpinNodes,
};
use studio_workspace::{OutlineEdge, OutlineGroup, OutlineNode, Refusal, ViewOutline};

use super::*;

/// The most hidden nodes the canvas names to a reader.
const MOST_HIDDEN_NAMED: usize = 200;

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
        // A node a branch removed is walked at the fork, where it still is (spec 042).
        let at_commit = self.read_at(&node);
        window.dispatch_action(
            Box::new(ShowNodeOnCanvas {
                type_name: node.type_name,
                id: node.id,
                at_commit,
                preview: true,
            }),
            cx,
        );
    }

    /// The canvas's outline: its nodes with the names drawn, the edges between them by
    /// relationship, its relationships open or not, what is selected, its zoom and whether the
    /// node cap left nodes out. A schema canvas shows types, not nodes, and has none.
    pub(crate) fn outline_of(&self, cx: &App) -> Option<ViewOutline> {
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
                SceneKind::Node { node, .. } => Some(OutlineNode {
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
                (SceneKind::Hub { key, .. }, Some(to)) => edges.push(OutlineEdge {
                    from: pair(&key.node),
                    to: pair(to),
                    relationship: Some(graph_store::relationship_key(&key.group)),
                }),
                (SceneKind::Node { node, .. }, Some(to)) => edges.push(OutlineEdge {
                    from: pair(node),
                    to: pair(to),
                    relationship: None,
                }),
                // A branch's changed relationship (spec 042): from the node its pill hangs off
                // to the node beyond it.
                (SceneKind::Link { .. }, Some(to)) => {
                    if let Some(from) = scene.nodes[edge.from].parent.and_then(node_of) {
                        edges.push(OutlineEdge {
                            from: pair(from),
                            to: pair(to),
                            relationship: None,
                        });
                    }
                }
                _ => {}
            }
        }
        let groups = scene
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(index, chip)| match &chip.kind {
                SceneKind::Hub { key, count, open } => Some(OutlineGroup {
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
                SceneId::Link(_) => json!({
                    "kind": "changed_relationship",
                    "label": label,
                }),
            }
        });
        let root = exploration.root().map(|root| OutlineNode {
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
        hidden.truncate(MOST_HIDDEN_NAMED);
        facts.insert("hidden".into(), Value::Array(hidden));
        if let Some((left_out, most)) = &scene.capped {
            facts.insert(
                "node_cap".into(),
                json!({ "cap": scene.cap, "left_out": left_out, "mostly": most }),
            );
        }
        Some(ViewOutline {
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

    /// The canvas is still reading or laying out what the last action on it opened.
    pub(crate) fn settling_now(&self, cx: &App) -> bool {
        let exploration = self.exploration.read(cx);
        self.layout_task.is_some()
            || self.next_layout.is_some()
            || self._loaded.is_some()
            || self.layout_owed
            || self.follow.is_some()
            || exploration.reads_out() > 0
            || exploration.expanding()
    }

    /// Does now what the canvas leaves for its next draw: the reads its walk owes and the
    /// layout of what the walk changed.
    pub(crate) fn catch_up_now(&mut self, cx: &mut Context<Self>) {
        if self.draws_exploration() && self.exploration.read(cx).owes_reads() {
            self.exploration.update(cx, |e, cx| e.read_owed(cx));
        }
        self.lay_out_owed(cx);
    }

    /// Why the canvas would not do what `action` asks, as it stands: a node it names is not
    /// drawn, it names none and none is marked or selected, a query's result picture is asked
    /// to expand or hide, a relationship named is not the nodes' or not open, or an expansion
    /// would open more than the canvas opens without asking.
    pub(crate) fn refusal_of(&self, action: &dyn Action, cx: &App) -> Option<Refusal> {
        let action = action.as_any();
        let expand = action.downcast_ref::<ExpandNeighbourhood>();
        let hide = action.downcast_ref::<HideNodes>();
        // The nodes the action names, when it acts on nodes.
        let named: Option<&[NodeId]> = expand
            .map(|action| action.expansion.nodes.as_slice())
            .or_else(|| hide.map(|action| action.nodes.as_slice()))
            .or_else(|| {
                action
                    .downcast_ref::<CollapseNodes>()
                    .map(|action| action.nodes.as_slice())
            })
            .or_else(|| {
                action
                    .downcast_ref::<PinNodes>()
                    .map(|action| action.nodes.as_slice())
            })
            .or_else(|| {
                action
                    .downcast_ref::<UnpinNodes>()
                    .map(|action| action.nodes.as_slice())
            });
        if let Some(named) = named {
            let missing: Vec<String> = named
                .iter()
                .filter(|node| self.index_of(&SceneId::Node(node_ref(node))).is_none())
                .map(|node| format!("{} {}", node.type_name, node.id))
                .collect();
            if !missing.is_empty() {
                return Some(Refusal::NotShown(missing));
            }
            if self.target_nodes(named).is_empty() {
                return Some(Refusal::Says(
                    "Name the nodes (`nodes`), or select or mark some first".into(),
                ));
            }
        }
        let walks = expand.is_some() || hide.is_some() || action.is::<ShowHidden>();
        if walks && !self.draws_exploration() {
            return Some(Refusal::NotAWalk);
        }
        if let Some(action) = expand {
            let expansion = &action.expansion;
            if !(1..=2).contains(&expansion.depth) {
                return Some(Refusal::Says("`depth` is 1 or 2".into()));
            }
            let targets = self.target_nodes(&expansion.nodes);
            let plan = self
                .exploration
                .read(cx)
                .plan_expand(&targets, expansion.relationships.as_deref());
            if !plan.unknown.is_empty() {
                let schema = self.session.read(cx).schema_model();
                let theirs: Vec<String> = targets
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
                return Some(Refusal::Says(format!(
                    "None of the nodes has {}. Theirs: {}",
                    plan.unknown.join(", "),
                    theirs.join(", ")
                )));
            }
            if plan.opening.asks() && !expansion.confirm {
                return Some(Refusal::AsksFirst {
                    nodes: plan.opening.nodes,
                    reads: plan.opening.reads,
                    confirms: true,
                });
            }
        }
        if let Some(action) = action.downcast_ref::<ShowMore>() {
            if let Some(node) = &action.node
                && self.index_of(&SceneId::Node(node_ref(node))).is_none()
            {
                return Some(Refusal::NotShown(Vec::new()));
            }
            let Some(key) = self.more_key(action, cx) else {
                return Some(Refusal::Says(
                    "Name the node and its relationship (`out:WorksAt`), or select a \
                     relationship or its \"+K more\" first"
                        .into(),
                ));
            };
            if self.index_of(&SceneId::Hub(key)).is_none() {
                return Some(Refusal::RelationshipClosed);
            }
        }
        None
    }
}

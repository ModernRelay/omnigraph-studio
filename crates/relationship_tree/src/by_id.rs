//! The tree's actions that name their nodes by id (spec 032, "The canvas and the tree"):
//! selecting a node's row, expanding several nodes' neighbourhoods and collapsing them. With
//! them, what the tree says of itself to a reader that does not draw it: its outline
//! (identifiers and the names the person sees, never a property value), whether it is still
//! settling, and why it would not do what an action asks.
//!
//! Nothing here knows who asks. The palette, a key and a UI tool reach the same handlers.

use gpui::Action;
use serde_json::json;
use studio_actions::NodeId;
use studio_actions::tree::{CollapseNodes, ExpandAll, ExpandNeighbourhood, SelectNode};
use studio_workspace::{OutlineEdge, OutlineGroup, OutlineNode, Refusal, ViewOutline};

use super::*;

fn node_ref(node: &NodeId) -> NodeRef {
    NodeRef::new(node.type_name.clone(), node.id.clone())
}

fn pair(node: &NodeRef) -> (String, String) {
    (node.type_name.clone(), node.id.clone())
}

impl RelationshipTree {
    /// The first row shown of `node`, the shallowest.
    fn row_of(&self, node: &NodeRef) -> Option<usize> {
        self.visible.iter().position(|ix| {
            matches!(&self.entries[*ix].kind, EntryKind::Node { node: shown, .. } if shown == node)
        })
    }

    /// The nodes an action names, else the selected row's node.
    fn target_nodes(&self, nodes: &[NodeId]) -> Vec<NodeRef> {
        if !nodes.is_empty() {
            return nodes.iter().map(node_ref).collect();
        }
        match self.selected_entry().map(|entry| &entry.kind) {
            Some(EntryKind::Node { node, .. }) => vec![node.clone()],
            _ => Vec::new(),
        }
    }

    pub(crate) fn select_node(
        &mut self,
        action: &SelectNode,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // None named: the node the tree starts from, its first row.
        let row = if action.id.is_empty() {
            (!self.visible.is_empty()).then_some(0)
        } else {
            self.row_of(&NodeRef::new(action.type_name.clone(), action.id.clone()))
        };
        if let Some(row) = row {
            self.select_row(row, cx);
        }
    }

    /// Expands the nodes' neighbourhoods through the exploration, as Expand all does, asked
    /// first past what opens without asking unless the action brings its answer.
    pub(crate) fn expand_neighbourhood(
        &mut self,
        action: &ExpandNeighbourhood,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let expansion = &action.expansion;
        let nodes = self.target_nodes(&expansion.nodes);
        if nodes.is_empty() {
            return;
        }
        let plan = self
            .exploration
            .read(cx)
            .plan_expand(&nodes, expansion.relationships.as_deref());
        let limit = expansion
            .limit_per_relationship
            .unwrap_or(self.exploration.read(cx).page_size(cx) as usize)
            .max(1);
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
        let nodes = self.target_nodes(&action.nodes);
        let paths: Vec<graph_store::ExplorePath> = self
            .entries
            .iter()
            .filter(|entry| {
                matches!(&entry.kind, EntryKind::Node { node, expanded: true, .. } if nodes.contains(node))
            })
            .map(|entry| entry.path.clone())
            .collect();
        if !paths.is_empty() {
            self.with_exploration(cx, |e, cx| e.set_open(&paths, false, cx));
        }
    }

    /// The tree's outline: its root, the rows shown (nodes with their names, and relationships
    /// open or not with their counts), and what is selected.
    pub(crate) fn outline_of(&self, cx: &App) -> Option<ViewOutline> {
        let exploration = self.exploration.read(cx);
        let state = exploration.state();
        let label = |node: &NodeRef, via: Option<&graph_store::Neighbour>| {
            via.and_then(|via| via.label.clone())
                .or_else(|| state.label(node).map(str::to_owned))
        };
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        let mut groups = Vec::new();
        for &ix in &self.visible {
            let entry = &self.entries[ix];
            match &entry.kind {
                EntryKind::Node { node, via, .. } => {
                    nodes.push(OutlineNode {
                        type_name: node.type_name.clone(),
                        id: node.id.clone(),
                        label: label(node, via.as_ref()),
                    });
                    let group = entry
                        .parent
                        .and_then(|parent| match &self.entries[parent].kind {
                            EntryKind::Group { key, .. } | EntryKind::Bucket { key, .. } => {
                                Some(key)
                            }
                            _ => None,
                        });
                    if let Some(key) = group {
                        edges.push(OutlineEdge {
                            from: pair(&key.node),
                            to: pair(node),
                            relationship: Some(graph_store::relationship_key(&key.group)),
                        });
                    }
                }
                EntryKind::Group {
                    key,
                    count,
                    expanded,
                } => groups.push(OutlineGroup {
                    node: pair(&key.node),
                    relationship: graph_store::relationship_key(&key.group),
                    other_type: key.group.other_type.clone(),
                    count: count.loaded().copied(),
                    expanded: *expanded,
                    shown: self
                        .entries
                        .iter()
                        .filter(|child| {
                            child.parent == Some(ix) && matches!(child.kind, EntryKind::Node { .. })
                        })
                        .count(),
                }),
                _ => {}
            }
        }
        let selected = self.selected_entry().and_then(|entry| match &entry.kind {
            EntryKind::Node { node, via, .. } => Some(json!({
                "kind": "node",
                "type_name": node.type_name,
                "id": node.id,
                "label": label(node, via.as_ref()),
            })),
            EntryKind::Group { key, .. } => Some(json!({
                "kind": "relationship",
                "node": { "type_name": key.node.type_name, "id": key.node.id },
                "relationship": graph_store::relationship_key(&key.group),
            })),
            _ => None,
        });
        let root = match &self.subject {
            Subject::Node(root) => Some(OutlineNode {
                type_name: root.type_name.clone(),
                id: root.id.clone(),
                label: state.label(root).map(str::to_owned),
            }),
            Subject::Set(_) => None,
        };
        let mut facts = serde_json::Map::new();
        facts.insert("filtered".into(), json!(!self.filter.is_empty()));
        facts.insert("hides_empty_relationships".into(), json!(self.hide_empty));
        Some(ViewOutline {
            root,
            nodes,
            edges,
            groups,
            selected,
            capped: false,
            stopped: exploration
                .deeper_stopped()
                .map(|opening| (opening.nodes, opening.reads)),
            facts,
        })
    }

    /// The tree is still reading or making rows of what the last action on it opened.
    pub(crate) fn settling_now(&self, cx: &App) -> bool {
        let exploration = self.exploration.read(cx);
        self.rebuild_owed
            || self._loaded.is_some()
            || exploration.reads_out() > 0
            || exploration.expanding()
    }

    /// Does now what the tree leaves for its next draw: the reads its walk owes and the rows
    /// of what the walk changed.
    pub(crate) fn catch_up_now(&mut self, cx: &mut Context<Self>) {
        if self.exploration.read(cx).owes_reads() {
            self.exploration.update(cx, |e, cx| e.read_owed(cx));
        }
        if self.rebuild_owed {
            self.rebuild(cx);
        }
    }

    /// Why the tree would not do what `action` asks, as it stands: a node it names is not
    /// shown, it names none and no node's row is selected, a relationship named is not the
    /// nodes', or it would open more than the tree opens without asking. `on` is the node
    /// whose row the action is to act on, when its caller selects one first.
    pub(crate) fn refusal_of(
        &self,
        action: &dyn Action,
        on: Option<&NodeId>,
        cx: &App,
    ) -> Option<Refusal> {
        let action = action.as_any();
        let expand = action.downcast_ref::<ExpandNeighbourhood>();
        // The nodes the action names, when it acts on nodes.
        let named: Option<&[NodeId]> = expand
            .map(|action| action.expansion.nodes.as_slice())
            .or_else(|| {
                action
                    .downcast_ref::<CollapseNodes>()
                    .map(|action| action.nodes.as_slice())
            });
        if let Some(named) = named {
            if named
                .iter()
                .any(|node| self.row_of(&node_ref(node)).is_none())
            {
                return Some(Refusal::NotShown(Vec::new()));
            }
            if self.target_nodes(named).is_empty() {
                return Some(Refusal::Says(
                    "Name the nodes (`nodes`), or select a node's row first".into(),
                ));
            }
        }
        if let Some(action) = expand {
            let expansion = &action.expansion;
            if !(1..=2).contains(&expansion.depth) {
                return Some(Refusal::Says("`depth` is 1 or 2".into()));
            }
            let plan = self.exploration.read(cx).plan_expand(
                &self.target_nodes(&expansion.nodes),
                expansion.relationships.as_deref(),
            );
            if !plan.unknown.is_empty() {
                return Some(Refusal::Says(format!(
                    "None of the nodes has {}",
                    plan.unknown.join(", ")
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
        if action.is::<ExpandAll>() {
            // The row the caller selects first, else the selected one, as Expand all opens it.
            let path = match on {
                Some(node) => self
                    .row_of(&node_ref(node))
                    .map(|row| self.entries[self.visible[row]].path.clone()),
                None => self.selected_entry().map(|entry| entry.path.clone()),
            };
            if let Some(path) = path {
                let opening = self
                    .exploration
                    .read(cx)
                    .opening(std::slice::from_ref(&path), true);
                if opening.asks() {
                    // Expand all carries no answer: the expansion that names its node does.
                    return Some(Refusal::AsksFirst {
                        nodes: opening.nodes,
                        reads: opening.reads,
                        confirms: false,
                    });
                }
            }
        }
        None
    }
}

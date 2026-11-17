//! What an agent does in a relationship tree (spec 032, "The canvas and the tree"): selecting a
//! node's row by its id, expanding several nodes' neighbourhoods and collapsing them, and what
//! an agent reads of the tree: identifiers and the names the person sees, never a property
//! value.

use serde_json::{Value, json};
use studio_actions::NodeId;
use studio_actions::tree::{CollapseNodes, ExpandNeighbourhood, SelectNode};
use studio_workspace::{AgentEdge, AgentGroup, AgentNode, AgentRefusal, AgentView};

use super::*;

/// What a call names a node the tree does not show with.
const NOT_SHOWN: &str =
    "Not in this tree; expand a relationship that holds it or use graph_show_node_in_tree";

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

    /// What an agent reads of the tree: its root, the rows shown (nodes with their names, and
    /// relationships open or not with their counts), and what is selected.
    pub(crate) fn agent_view_of(&self, cx: &App) -> Option<AgentView> {
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
                    nodes.push(AgentNode {
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
                        edges.push(AgentEdge {
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
                } => groups.push(AgentGroup {
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
            Subject::Node(root) => Some(AgentNode {
                type_name: root.type_name.clone(),
                id: root.id.clone(),
                label: state.label(root).map(str::to_owned),
            }),
            Subject::Set(_) => None,
        };
        let mut facts = serde_json::Map::new();
        facts.insert("filtered".into(), json!(!self.filter.is_empty()));
        facts.insert("hides_empty_relationships".into(), json!(self.hide_empty));
        Some(AgentView {
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

    /// The tree is still reading or making rows of what a call opened.
    pub(crate) fn agent_busy_now(&self, cx: &App) -> bool {
        let exploration = self.exploration.read(cx);
        self.rebuild_owed
            || self._loaded.is_some()
            || exploration.reads_out() > 0
            || exploration.expanding()
    }

    /// Whether the tree takes an agent's call: the nodes it names are shown, the relationship
    /// named is theirs, and an expansion past what opens without asking brings its answer.
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
        if named
            .iter()
            .any(|node| self.row_of(&node_ref(node)).is_none())
        {
            return Err(AgentRefusal::not_available(NOT_SHOWN));
        }
        let ask = |opening: graph_store::Opening, again: &str| AgentRefusal {
            outcome: "needs_confirmation",
            says: format!(
                "This would open {} rows and ask for {} reads, past what the tree opens without \
                 asking the person. Nothing was opened. {again}",
                opening.nodes, opening.reads
            ),
            json: json!({
                "needs_confirmation": { "nodes": opening.nodes, "reads": opening.reads }
            }),
        };
        match tool {
            "tree_expand" | "tree_collapse" if self.target_nodes(&named).is_empty() => {
                return Err(AgentRefusal::not_available(
                    "Name the nodes (`nodes`), or select a node's row first",
                ));
            }
            "tree_expand" => {
                let expansion: studio_actions::Expansion =
                    serde_json::from_value(input.clone()).unwrap_or_default();
                if !(1..=2).contains(&expansion.depth) {
                    return Err(AgentRefusal::not_available("`depth` is 1 or 2"));
                }
                let plan = self.exploration.read(cx).plan_expand(
                    &self.target_nodes(&named),
                    expansion.relationships.as_deref(),
                );
                if !plan.unknown.is_empty() {
                    return Err(AgentRefusal::not_available(format!(
                        "None of the nodes has {}",
                        plan.unknown.join(", ")
                    )));
                }
                if plan.opening.asks() && !expansion.confirm {
                    return Err(ask(
                        plan.opening,
                        "Call again with confirm: true to open it.",
                    ));
                }
            }
            "tree_expand_all" => {
                // The row the call names, else the selected one, as Expand all opens it.
                let path = match input
                    .get("node")
                    .and_then(|node| serde_json::from_value::<NodeId>(node.clone()).ok())
                {
                    Some(node) => self
                        .row_of(&node_ref(&node))
                        .map(|row| self.entries[self.visible[row]].path.clone()),
                    None => self.selected_entry().map(|entry| entry.path.clone()),
                };
                if let Some(path) = path {
                    let opening = self
                        .exploration
                        .read(cx)
                        .opening(std::slice::from_ref(&path), true);
                    if opening.asks() {
                        return Err(ask(
                            opening,
                            "Use tree_expand with the node and confirm: true to open it.",
                        ));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

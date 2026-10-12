//! The exploration the relationship tree (spec 005) and the graph canvas (spec 006) share: one
//! root node, the paths a person expanded from it, and the group counts and neighbour pages
//! those paths show.
//!
//! Every read is pinned to the commit the first read returned (constitution II). A commit on
//! the session's branch raises [`Exploration::newer_available`] and changes nothing until
//! [`Exploration::refresh`]. Counts come from one `builder::degree` query per relationship
//! group, run concurrently (engine change E16 would replace them); neighbours come in keyset
//! pages from `builder::neighbours`, as many as the graph's `tree_page_size` setting.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::{
    App, AppContext as _, Context, Entity, EntityId, EventEmitter, Global, Subscription, Task,
    WeakEntity,
};
use gq_language::{BuiltQuery, Direction, RelationshipGroup, SchemaModel, builder};
use graph_client::ReadError;
use graph_client::api::ReadOutput;
use serde_json::Value;

use crate::session::{GraphSession, Loadable, SessionEvent};

/// One node, by type and key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeRef {
    pub type_name: String,
    pub id: String,
}

impl NodeRef {
    pub fn new(type_name: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            type_name: type_name.into(),
            id: id.into(),
        }
    }
}

/// One step of a path from the root: into a relationship group, or into one of its nodes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Step {
    Group(RelationshipGroup),
    Node(NodeRef),
}

/// A path from the root, alternating group and node steps. The empty path is the root. A
/// graph is not a tree: the same node can sit at several paths, and each is expanded on its
/// own (spec 005, "Path tree, cycles marked").
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ExplorePath(Vec<Step>);

impl ExplorePath {
    pub fn root() -> Self {
        Self::default()
    }

    pub fn steps(&self) -> &[Step] {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    pub fn depth(&self) -> usize {
        self.0.len()
    }

    pub fn child_group(&self, group: &RelationshipGroup) -> Self {
        let mut steps = self.0.clone();
        steps.push(Step::Group(group.clone()));
        Self(steps)
    }

    pub fn child_node(&self, node: &NodeRef) -> Self {
        let mut steps = self.0.clone();
        steps.push(Step::Node(node.clone()));
        Self(steps)
    }

    pub fn parent(&self) -> Option<Self> {
        let (_, rest) = self.0.split_last()?;
        Some(Self(rest.to_vec()))
    }

    pub fn starts_with(&self, prefix: &ExplorePath) -> bool {
        self.0.starts_with(&prefix.0)
    }

    /// The group this path ends in, when it ends in one.
    pub fn last_group(&self) -> Option<&RelationshipGroup> {
        match self.0.last() {
            Some(Step::Group(group)) => Some(group),
            _ => None,
        }
    }

    /// The node this path ends at, or the node whose group it ends in.
    pub fn node<'a>(&'a self, root: &'a NodeRef) -> &'a NodeRef {
        self.0
            .iter()
            .rev()
            .find_map(|step| match step {
                Step::Node(node) => Some(node),
                Step::Group(_) => None,
            })
            .unwrap_or(root)
    }

    /// The node steps of the path, root excluded.
    pub fn nodes(&self) -> impl Iterator<Item = &NodeRef> {
        self.0.iter().filter_map(|step| match step {
            Step::Node(node) => Some(node),
            Step::Group(_) => None,
        })
    }

    /// The path with `prefix` removed, when it starts with it.
    pub fn strip_prefix(&self, prefix: &ExplorePath) -> Option<Self> {
        self.starts_with(prefix)
            .then(|| Self(self.0[prefix.0.len()..].to_vec()))
    }
}

/// One relationship group of one node: the unit counts and neighbour pages are cached by.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GroupKey {
    pub node: NodeRef,
    pub group: RelationshipGroup,
}

/// A node at the other end of a relationship group, with the edge's properties.
#[derive(Debug, Clone, PartialEq)]
pub struct Neighbour {
    pub node: NodeRef,
    pub label: Option<String>,
    /// Non-null edge properties, in schema order.
    pub edge: Vec<(String, Value)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PageStatus {
    #[default]
    Idle,
    Loading,
    Failed(String),
}

/// The neighbour pages of one relationship group read so far.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NeighbourList {
    pub rows: Vec<Neighbour>,
    pub status: PageStatus,
    /// The last page came back short: there are no more rows.
    pub exhausted: bool,
    /// The size of the page last asked for, so a short page is recognised.
    pub page_size: u32,
}

impl NeighbourList {
    /// The keyset cursor for the next page.
    pub fn next_after(&self) -> Option<&str> {
        self.rows.last().map(|row| row.node.id.as_str())
    }

    /// Whether a "Show more" row belongs after the rows, given the group's count.
    pub fn has_more(&self, count: Option<u64>) -> bool {
        !self.exhausted && count.is_none_or(|count| count > self.rows.len() as u64)
    }
}

/// Something the exploration reads.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LoadTarget {
    Root,
    Count(GroupKey),
    Page(GroupKey),
}

/// One row of the flattened exploration, in tree order. The relationship tree renders these;
/// the canvas derives its nodes, hubs and edges from them.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub path: ExplorePath,
    pub depth: usize,
    /// The index of the parent entry.
    pub parent: Option<usize>,
    pub kind: EntryKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EntryKind {
    /// A node; `via` is the edge that led here (none for the root).
    Node {
        node: NodeRef,
        via: Option<Neighbour>,
        expandable: bool,
        expanded: bool,
    },
    /// A node already on its own path; `ancestor` is the entry index of that occurrence.
    Cycle {
        node: NodeRef,
        via: Neighbour,
        ancestor: usize,
    },
    /// An edge with at most one target, shown on its node as a field.
    Field {
        key: GroupKey,
        target: Option<Neighbour>,
        loading: bool,
    },
    /// A relationship group of the parent node.
    Group {
        key: GroupKey,
        count: Loadable<u64>,
        expanded: bool,
    },
    /// "Show 50 more of N": `shown` rows are loaded, `total` is the group's count when known.
    More {
        key: GroupKey,
        shown: usize,
        total: Option<u64>,
    },
    Loading {
        target: LoadTarget,
    },
    Error {
        target: LoadTarget,
        message: String,
    },
}

/// Whether a group is shown as a field: an outgoing edge with at most one target.
pub fn is_field(schema: &SchemaModel, group: &RelationshipGroup) -> bool {
    group.direction == Direction::Outgoing
        && schema
            .edge(&group.edge)
            .is_some_and(|edge| edge.at_most_one)
}

/// The data of an exploration, without GPUI: what is expanded and what has been read.
#[derive(Debug, Clone)]
pub struct ExploreState {
    pub root: Option<NodeRef>,
    pub root_status: Loadable<()>,
    /// The roots walked before the current one, oldest first (re-rooting keeps the path).
    pub breadcrumb: Vec<NodeRef>,
    /// Labels read so far. `None` means the type has no label besides its key.
    pub labels: HashMap<NodeRef, Option<String>>,
    pub expanded: HashSet<ExplorePath>,
    pub counts: HashMap<GroupKey, Loadable<u64>>,
    pub neighbours: HashMap<GroupKey, NeighbourList>,
    /// Groups whose neighbours are narrowed on the server to ids or labels matching a text.
    pub searches: HashMap<GroupKey, String>,
    pub selection: Option<ExplorePath>,
}

impl Default for ExploreState {
    fn default() -> Self {
        Self {
            root: None,
            root_status: Loadable::NotLoaded,
            breadcrumb: Vec::new(),
            labels: HashMap::new(),
            expanded: HashSet::new(),
            counts: HashMap::new(),
            neighbours: HashMap::new(),
            searches: HashMap::new(),
            selection: None,
        }
    }
}

impl ExploreState {
    /// A new exploration at `root`, expanded one level.
    pub fn rooted(root: NodeRef) -> Self {
        let mut state = Self {
            root: Some(root),
            ..Self::default()
        };
        state.expanded.insert(ExplorePath::root());
        state.selection = Some(ExplorePath::root());
        state
    }

    pub fn label(&self, node: &NodeRef) -> Option<&str> {
        self.labels.get(node).and_then(|label| label.as_deref())
    }

    pub fn is_expanded(&self, path: &ExplorePath) -> bool {
        self.expanded.contains(path)
    }

    fn group_key(&self, path: &ExplorePath) -> Option<GroupKey> {
        let root = self.root.as_ref()?;
        let group = path.last_group()?;
        Some(GroupKey {
            node: path.node(root).clone(),
            group: group.clone(),
        })
    }

    /// Expands a node or group path. A group known to be empty stays closed.
    pub fn expand(&mut self, path: &ExplorePath) {
        if let Some(key) = self.group_key(path)
            && matches!(self.counts.get(&key), Some(Loadable::Loaded(0)))
        {
            return;
        }
        self.expanded.insert(path.clone());
    }

    /// Collapses one path; what was open below it opens again with it.
    pub fn collapse(&mut self, path: &ExplorePath) {
        self.expanded.remove(path);
    }

    /// Collapses a path and everything below it.
    pub fn collapse_subtree(&mut self, path: &ExplorePath) {
        self.expanded.retain(|expanded| !expanded.starts_with(path));
    }

    /// Expands a node path and every relationship group of the node.
    pub fn expand_all(&mut self, schema: &SchemaModel, path: &ExplorePath) {
        let Some(root) = self.root.clone() else {
            return;
        };
        if path.last_group().is_some() {
            self.expand(path);
            return;
        }
        let node = path.node(&root).clone();
        self.expanded.insert(path.clone());
        for group in schema.relationships_of(&node.type_name) {
            if !is_field(schema, &group) {
                self.expand(&path.child_group(&group));
            }
        }
    }

    /// Makes the node at `path` the root. Expansions below it are kept; the old root and the
    /// nodes on the way join the breadcrumb.
    pub fn reroot(&mut self, path: &ExplorePath) -> bool {
        let Some(root) = self.root.clone() else {
            return false;
        };
        if path.is_root() || path.last_group().is_some() {
            return false;
        }
        let new_root = path.node(&root).clone();
        self.breadcrumb.push(root);
        let on_the_way: Vec<NodeRef> = path.nodes().cloned().collect();
        self.breadcrumb
            .extend(on_the_way[..on_the_way.len() - 1].iter().cloned());
        self.expanded = self
            .expanded
            .iter()
            .filter_map(|expanded| expanded.strip_prefix(path))
            .collect();
        self.expanded.insert(ExplorePath::root());
        self.selection = Some(ExplorePath::root());
        self.root = Some(new_root);
        self.root_status = if self.labels.contains_key(self.root.as_ref().unwrap()) {
            Loadable::Loaded(())
        } else {
            Loadable::NotLoaded
        };
        true
    }

    /// Forgets everything read, keeping the root, expansions and selection.
    pub fn clear_loaded(&mut self) {
        self.root_status = Loadable::NotLoaded;
        self.labels.clear();
        self.counts.clear();
        self.neighbours.clear();
    }

    /// The exploration flattened in tree order.
    pub fn entries(&self, schema: &SchemaModel) -> Vec<Entry> {
        let mut out = Vec::new();
        let Some(root) = &self.root else {
            return out;
        };
        let mut ancestors = Vec::new();
        self.push_node(
            schema,
            root,
            None,
            ExplorePath::root(),
            0,
            None,
            &mut ancestors,
            &mut out,
        );
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn push_node(
        &self,
        schema: &SchemaModel,
        node: &NodeRef,
        via: Option<&Neighbour>,
        path: ExplorePath,
        depth: usize,
        parent: Option<usize>,
        ancestors: &mut Vec<(NodeRef, usize)>,
        out: &mut Vec<Entry>,
    ) {
        let index = out.len();
        let groups = schema.relationships_of(&node.type_name);
        let expanded = self.expanded.contains(&path);
        out.push(Entry {
            path: path.clone(),
            depth,
            parent,
            kind: EntryKind::Node {
                node: node.clone(),
                via: via.cloned(),
                expandable: !groups.is_empty(),
                expanded,
            },
        });
        if path.is_root() {
            match &self.root_status {
                Loadable::Loaded(()) => {}
                Loadable::Failed(message) => {
                    out.push(Entry {
                        path: path.clone(),
                        depth: depth + 1,
                        parent: Some(index),
                        kind: EntryKind::Error {
                            target: LoadTarget::Root,
                            message: message.clone(),
                        },
                    });
                    return;
                }
                Loadable::NotLoaded | Loadable::Loading => {
                    out.push(Entry {
                        path: path.clone(),
                        depth: depth + 1,
                        parent: Some(index),
                        kind: EntryKind::Loading {
                            target: LoadTarget::Root,
                        },
                    });
                    return;
                }
            }
        }
        if !expanded {
            return;
        }
        ancestors.push((node.clone(), index));
        for group in groups {
            let key = GroupKey {
                node: node.clone(),
                group: group.clone(),
            };
            let group_path = path.child_group(&group);
            let list = self.neighbours.get(&key);
            if is_field(schema, &group) {
                let index_of_field = out.len();
                out.push(Entry {
                    path: group_path.clone(),
                    depth: depth + 1,
                    parent: Some(index),
                    kind: EntryKind::Field {
                        key: key.clone(),
                        target: list.and_then(|list| list.rows.first().cloned()),
                        loading: list.is_none_or(|list| list.status == PageStatus::Loading),
                    },
                });
                if let Some(PageStatus::Failed(message)) = list.map(|list| &list.status) {
                    out.push(Entry {
                        path: group_path,
                        depth: depth + 2,
                        parent: Some(index_of_field),
                        kind: EntryKind::Error {
                            target: LoadTarget::Page(key),
                            message: message.clone(),
                        },
                    });
                }
                continue;
            }
            let count = self
                .counts
                .get(&key)
                .cloned()
                .unwrap_or(Loadable::NotLoaded);
            let group_expanded =
                self.expanded.contains(&group_path) && !matches!(count, Loadable::Loaded(0));
            let group_index = out.len();
            out.push(Entry {
                path: group_path.clone(),
                depth: depth + 1,
                parent: Some(index),
                kind: EntryKind::Group {
                    key: key.clone(),
                    count: count.clone(),
                    expanded: group_expanded,
                },
            });
            if let Loadable::Failed(message) = &count {
                out.push(Entry {
                    path: group_path.clone(),
                    depth: depth + 2,
                    parent: Some(group_index),
                    kind: EntryKind::Error {
                        target: LoadTarget::Count(key.clone()),
                        message: message.clone(),
                    },
                });
            }
            if !group_expanded {
                continue;
            }
            let Some(list) = list else {
                out.push(Entry {
                    path: group_path,
                    depth: depth + 2,
                    parent: Some(group_index),
                    kind: EntryKind::Loading {
                        target: LoadTarget::Page(key),
                    },
                });
                continue;
            };
            for neighbour in &list.rows {
                let node_path = group_path.child_node(&neighbour.node);
                if let Some((_, ancestor)) = ancestors.iter().find(|(n, _)| *n == neighbour.node) {
                    out.push(Entry {
                        path: node_path,
                        depth: depth + 2,
                        parent: Some(group_index),
                        kind: EntryKind::Cycle {
                            node: neighbour.node.clone(),
                            via: neighbour.clone(),
                            ancestor: *ancestor,
                        },
                    });
                } else {
                    self.push_node(
                        schema,
                        &neighbour.node,
                        Some(neighbour),
                        node_path,
                        depth + 2,
                        Some(group_index),
                        ancestors,
                        out,
                    );
                }
            }
            let tail = match &list.status {
                PageStatus::Loading => Some(EntryKind::Loading {
                    target: LoadTarget::Page(key),
                }),
                PageStatus::Failed(message) => Some(EntryKind::Error {
                    target: LoadTarget::Page(key),
                    message: message.clone(),
                }),
                PageStatus::Idle => {
                    list.has_more(count.loaded().copied())
                        .then(|| EntryKind::More {
                            shown: list.rows.len(),
                            total: count.loaded().copied(),
                            key,
                        })
                }
            };
            if let Some(kind) = tail {
                out.push(Entry {
                    path: group_path,
                    depth: depth + 2,
                    parent: Some(group_index),
                    kind,
                });
            }
        }
        ancestors.pop();
    }

    /// The reads the visible entries still need, each once, in tree order.
    pub fn missing_loads(&self, schema: &SchemaModel) -> Vec<LoadTarget> {
        if self.root.is_none() {
            return Vec::new();
        }
        if self.root_status == Loadable::NotLoaded {
            return vec![LoadTarget::Root];
        }
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for entry in self.entries(schema) {
            let target = match entry.kind {
                EntryKind::Group {
                    key,
                    count: Loadable::NotLoaded,
                    ..
                } => LoadTarget::Count(key),
                EntryKind::Field { key, .. } if !self.neighbours.contains_key(&key) => {
                    LoadTarget::Page(key)
                }
                EntryKind::Loading {
                    target: LoadTarget::Page(key),
                } if !self.neighbours.contains_key(&key) => LoadTarget::Page(key),
                _ => continue,
            };
            if seen.insert(target.clone()) {
                out.push(target);
            }
        }
        out
    }

    /// The shallowest visible path at which `node` appears.
    pub fn path_of(&self, schema: &SchemaModel, node: &NodeRef) -> Option<ExplorePath> {
        self.entries(schema)
            .into_iter()
            .filter(|entry| matches!(&entry.kind, EntryKind::Node { node: n, .. } if n == node))
            .min_by_key(|entry| entry.depth)
            .map(|entry| entry.path)
    }

    /// Applies the answer to a root read.
    pub fn apply_root(&mut self, output: &ReadOutput) {
        let Some(root) = self.root.clone() else {
            return;
        };
        match rows(output).first() {
            Some(row) => {
                let label = row.get("label").and_then(Value::as_str).map(str::to_owned);
                self.labels.insert(root, label);
                self.root_status = Loadable::Loaded(());
            }
            None => {
                self.root_status = Loadable::Failed(format!(
                    "No {} with key {} at this commit.",
                    root.type_name, root.id
                ));
            }
        }
    }

    /// Applies the answer to a count read.
    pub fn apply_count(&mut self, key: GroupKey, output: &ReadOutput) {
        let count = rows(output)
            .first()
            .and_then(|row| row.get("n"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        self.counts.insert(key, Loadable::Loaded(count));
    }

    /// Appends one page of neighbours.
    pub fn apply_page(&mut self, key: GroupKey, output: &ReadOutput, page_size: u32) {
        let edge_columns: Vec<&str> = output
            .columns
            .iter()
            .filter(|column| column.starts_with("e_"))
            .map(String::as_str)
            .collect();
        let other_type = key.group.other_type.clone();
        let page: Vec<Neighbour> = rows(output)
            .iter()
            .filter_map(|row| {
                let id = row.get("id")?.as_str()?.to_owned();
                Some(Neighbour {
                    node: NodeRef::new(other_type.clone(), id),
                    label: row.get("label").and_then(Value::as_str).map(str::to_owned),
                    edge: edge_columns
                        .iter()
                        .filter_map(|column| {
                            let value = row.get(*column)?;
                            (!value.is_null()).then(|| {
                                (column.trim_start_matches("e_").to_owned(), value.clone())
                            })
                        })
                        .collect(),
                })
            })
            .collect();
        for neighbour in &page {
            self.labels
                .insert(neighbour.node.clone(), neighbour.label.clone());
        }
        let list = self.neighbours.entry(key).or_default();
        list.exhausted = (page.len() as u32) < page_size;
        // Parallel edges to one neighbour show as one row, with the first edge's properties,
        // so every path in the tree stays unique.
        for neighbour in page {
            if !list.rows.iter().any(|row| row.node == neighbour.node) {
                list.rows.push(neighbour);
            }
        }
        list.status = PageStatus::Idle;
    }
}

fn rows(output: &ReadOutput) -> Vec<serde_json::Map<String, Value>> {
    match &output.rows {
        Value::Array(rows) => rows
            .iter()
            .filter_map(|row| row.as_object().cloned())
            .collect(),
        _ => Vec::new(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplorationEvent {
    /// The root, expansions or loaded data changed.
    Changed,
    /// The selected path changed.
    SelectionChanged,
}

#[derive(Default)]
struct ExplorationRegistry(HashMap<EntityId, WeakEntity<Exploration>>);

impl Global for ExplorationRegistry {}

/// One session's exploration. Get it with [`Exploration::for_session`]; the tree and the
/// canvas of a session share it.
pub struct Exploration {
    session: Entity<GraphSession>,
    state: ExploreState,
    pin: Option<String>,
    newer_available: bool,
    generation: u64,
    tasks: HashMap<LoadTarget, Task<()>>,
    _subscription: Subscription,
}

impl EventEmitter<ExplorationEvent> for Exploration {}

impl Exploration {
    /// Neighbours per page for this graph (setting `studio.tree_page_size`, or the graph's own).
    pub fn page_size(&self, cx: &App) -> u32 {
        self.session.read(cx).options(cx).tree_page_size.max(1)
    }

    /// The exploration of `session`, created on first use.
    pub fn for_session(session: &Entity<GraphSession>, cx: &mut App) -> Entity<Exploration> {
        let id = session.entity_id();
        let existing = cx
            .default_global::<ExplorationRegistry>()
            .0
            .get(&id)
            .and_then(WeakEntity::upgrade);
        if let Some(existing) = existing {
            return existing;
        }
        let exploration = cx.new(|cx| Exploration::new(session.clone(), cx));
        let registry = cx.default_global::<ExplorationRegistry>();
        registry.0.retain(|_, weak| weak.upgrade().is_some());
        registry.0.insert(id, exploration.downgrade());
        exploration
    }

    fn new(session: Entity<GraphSession>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&session, |this, session, event, cx| match event {
            SessionEvent::SchemaChanged => this.request_missing(cx),
            SessionEvent::BranchChanged => {
                if let Some(root) = this.state.root.clone() {
                    this.root_at(root, cx);
                }
            }
            SessionEvent::Committed { branch }
                if this.pin.is_some() && *branch == session.read(cx).branch =>
            {
                this.newer_available = true;
                cx.emit(ExplorationEvent::Changed);
                cx.notify();
            }
            _ => {}
        });
        Self {
            session,
            state: ExploreState::default(),
            pin: None,
            newer_available: false,
            generation: 0,
            tasks: HashMap::new(),
            _subscription: subscription,
        }
    }

    pub fn session(&self) -> &Entity<GraphSession> {
        &self.session
    }

    pub fn state(&self) -> &ExploreState {
        &self.state
    }

    pub fn schema(&self, cx: &App) -> Option<Arc<SchemaModel>> {
        self.session.read(cx).schema_model()
    }

    pub fn root(&self) -> Option<&NodeRef> {
        self.state.root.as_ref()
    }

    /// The commit every read of this exploration is pinned to.
    pub fn pin(&self) -> Option<&str> {
        self.pin.as_deref()
    }

    /// The branch moved since the pin; offer Refresh.
    pub fn newer_available(&self) -> bool {
        self.newer_available
    }

    /// The exploration flattened in tree order; empty until the schema loads.
    pub fn entries(&self, cx: &App) -> Vec<Entry> {
        match self.schema(cx) {
            Some(schema) => self.state.entries(&schema),
            None => Vec::new(),
        }
    }

    /// Starts a new exploration at `node`, at the branch head.
    pub fn root_at(&mut self, node: NodeRef, cx: &mut Context<Self>) {
        self.reset_reads();
        self.state = ExploreState::rooted(node);
        self.request_missing(cx);
        self.changed(cx);
        cx.emit(ExplorationEvent::SelectionChanged);
    }

    /// Selects `node` where it is already visible, else roots the exploration at it.
    pub fn reveal(&mut self, node: NodeRef, cx: &mut Context<Self>) {
        let path = self
            .schema(cx)
            .and_then(|schema| self.state.path_of(&schema, &node));
        match path {
            Some(path) => self.select(Some(path), cx),
            None => self.root_at(node, cx),
        }
    }

    /// Makes the node at `path` the root, at the same commit.
    pub fn reroot(&mut self, path: &ExplorePath, cx: &mut Context<Self>) {
        if self.state.reroot(path) {
            self.request_missing(cx);
            self.changed(cx);
            cx.emit(ExplorationEvent::SelectionChanged);
        }
    }

    /// Goes back to a root in the breadcrumb, at the same commit.
    pub fn back_to(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(node) = self.state.breadcrumb.get(index).cloned() else {
            return;
        };
        let breadcrumb = self.state.breadcrumb[..index].to_vec();
        let labels = std::mem::take(&mut self.state.labels);
        let counts = std::mem::take(&mut self.state.counts);
        let neighbours = std::mem::take(&mut self.state.neighbours);
        let mut state = ExploreState::rooted(node.clone());
        state.root_status = if labels.contains_key(&node) {
            Loadable::Loaded(())
        } else {
            Loadable::NotLoaded
        };
        state.breadcrumb = breadcrumb;
        state.labels = labels;
        state.counts = counts;
        state.neighbours = neighbours;
        self.state = state;
        self.request_missing(cx);
        self.changed(cx);
        cx.emit(ExplorationEvent::SelectionChanged);
    }

    pub fn is_expanded(&self, path: &ExplorePath) -> bool {
        self.state.is_expanded(path)
    }

    pub fn expand(&mut self, path: &ExplorePath, cx: &mut Context<Self>) {
        self.state.expand(path);
        self.request_missing(cx);
        self.changed(cx);
    }

    pub fn collapse(&mut self, path: &ExplorePath, cx: &mut Context<Self>) {
        self.state.collapse(path);
        self.changed(cx);
    }

    pub fn toggle(&mut self, path: &ExplorePath, cx: &mut Context<Self>) {
        if self.state.is_expanded(path) {
            self.collapse(path, cx);
        } else {
            self.expand(path, cx);
        }
    }

    pub fn expand_all(&mut self, path: &ExplorePath, cx: &mut Context<Self>) {
        if let Some(schema) = self.schema(cx) {
            self.state.expand_all(&schema, path);
            self.request_missing(cx);
            self.changed(cx);
        }
    }

    pub fn collapse_subtree(&mut self, path: &ExplorePath, cx: &mut Context<Self>) {
        self.state.collapse_subtree(path);
        self.changed(cx);
    }

    pub fn selection(&self) -> Option<&ExplorePath> {
        self.state.selection.as_ref()
    }

    pub fn select(&mut self, path: Option<ExplorePath>, cx: &mut Context<Self>) {
        if self.state.selection != path {
            self.state.selection = path;
            cx.emit(ExplorationEvent::SelectionChanged);
            cx.notify();
        }
    }

    /// Reads the next page of a group's neighbours.
    /// Reads a group's neighbours again, narrowed to ids starting with or labels containing
    /// `text` (spec 005: the filter for groups over one page); empty text reads them all.
    pub fn search_group(&mut self, key: &GroupKey, text: &str, cx: &mut Context<Self>) {
        let text = text.trim();
        let current = self
            .state
            .searches
            .get(key)
            .map(String::as_str)
            .unwrap_or("");
        if current == text {
            return;
        }
        if text.is_empty() {
            self.state.searches.remove(key);
        } else {
            self.state.searches.insert(key.clone(), text.to_owned());
        }
        self.state.neighbours.remove(key);
        self.start(LoadTarget::Page(key.clone()), cx);
        self.changed(cx);
    }

    /// Ends every group search, reading those groups' first pages again.
    pub fn clear_searches(&mut self, cx: &mut Context<Self>) {
        let keys: Vec<GroupKey> = self.state.searches.keys().cloned().collect();
        for key in keys {
            self.search_group(&key, "", cx);
        }
    }

    pub fn load_more(&mut self, key: &GroupKey, cx: &mut Context<Self>) {
        let ready = self
            .state
            .neighbours
            .get(key)
            .is_some_and(|list| list.status == PageStatus::Idle && !list.exhausted);
        if ready {
            self.start(LoadTarget::Page(key.clone()), cx);
            self.changed(cx);
        }
    }

    /// Reads a failed target again. Reads are idempotent, so this is safe (writes never retry).
    pub fn retry(&mut self, target: &LoadTarget, cx: &mut Context<Self>) {
        match target {
            LoadTarget::Root => self.state.root_status = Loadable::NotLoaded,
            LoadTarget::Count(key) => {
                self.state.counts.remove(key);
            }
            LoadTarget::Page(key) => {
                if let Some(list) = self.state.neighbours.get_mut(key) {
                    if list.rows.is_empty() {
                        self.state.neighbours.remove(key);
                    } else {
                        list.status = PageStatus::Idle;
                        self.start(target.clone(), cx);
                    }
                }
            }
        }
        self.request_missing(cx);
        self.changed(cx);
    }

    /// Drops the pin and everything read, and reads what is visible again at the branch head.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.reset_reads();
        self.state.clear_loaded();
        self.request_missing(cx);
        self.changed(cx);
    }

    fn reset_reads(&mut self) {
        self.generation += 1;
        self.tasks.clear();
        self.pin = None;
        self.newer_available = false;
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        cx.emit(ExplorationEvent::Changed);
        cx.notify();
    }

    fn request_missing(&mut self, cx: &mut Context<Self>) {
        let Some(schema) = self.schema(cx) else {
            return;
        };
        // Until the first read returns its commit, nothing else is read, so every read of the
        // exploration sees the same commit.
        if self.pin.is_none() && self.state.root_status == Loadable::Loading {
            return;
        }
        for target in self.state.missing_loads(&schema) {
            self.start(target, cx);
        }
    }

    fn start(&mut self, target: LoadTarget, cx: &mut Context<Self>) {
        let Some(schema) = self.schema(cx) else {
            return;
        };
        let query = match &target {
            LoadTarget::Root => {
                let Some(root) = &self.state.root else {
                    return;
                };
                self.state.root_status = Loadable::Loading;
                builder::node_label(&schema, &root.type_name, &root.id)
            }
            LoadTarget::Count(key) => {
                self.state.counts.insert(key.clone(), Loadable::Loading);
                builder::degree(&schema, &key.node.type_name, &key.group, &key.node.id)
            }
            LoadTarget::Page(key) => {
                let page_size = self.page_size(cx);
                let list = self.state.neighbours.entry(key.clone()).or_default();
                list.status = PageStatus::Loading;
                list.page_size = page_size;
                builder::neighbours(
                    &schema,
                    &key.node.type_name,
                    &key.group,
                    &key.node.id,
                    list.next_after(),
                    self.state.searches.get(key).map(String::as_str),
                    page_size,
                )
            }
        };
        match query {
            Ok(query) => self.spawn_read(target, query, cx),
            Err(error) => self.apply_failure(&target, error.to_string()),
        }
    }

    fn spawn_read(&mut self, target: LoadTarget, query: BuiltQuery, cx: &mut Context<Self>) {
        let snapshot = self.pin.clone();
        let generation = self.generation;
        let read = self
            .session
            .update(cx, |session, cx| session.read(&query, snapshot, cx));
        let key = target.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.apply(&key, result);
                this.request_missing(cx);
                this.changed(cx);
            })
            .ok();
        });
        self.tasks.insert(target, task);
    }

    fn apply(&mut self, target: &LoadTarget, result: Result<ReadOutput, ReadError>) {
        let output = match result {
            Ok(output) => output,
            Err(error) => {
                self.apply_failure(target, error.to_string());
                return;
            }
        };
        if self.pin.is_none() {
            self.pin = output.graph_commit_id.clone();
        }
        match target {
            LoadTarget::Root => self.state.apply_root(&output),
            LoadTarget::Count(key) => self.state.apply_count(key.clone(), &output),
            LoadTarget::Page(key) => {
                let page_size = self
                    .state
                    .neighbours
                    .get(key)
                    .map_or(1, |list| list.page_size.max(1));
                self.state.apply_page(key.clone(), &output, page_size)
            }
        }
    }

    fn apply_failure(&mut self, target: &LoadTarget, message: String) {
        match target {
            LoadTarget::Root => self.state.root_status = Loadable::Failed(message),
            LoadTarget::Count(key) => {
                self.state
                    .counts
                    .insert(key.clone(), Loadable::Failed(message));
            }
            LoadTarget::Page(key) => {
                self.state.neighbours.entry(key.clone()).or_default().status =
                    PageStatus::Failed(message);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use graph_client::api::ReadTargetOutput;
    use serde_json::json;

    use super::*;

    const SCHEMA: &str = "node Person {\n  slug: String @key\n  name: String\n}\n\
        node Company {\n  slug: String @key\n  name: String\n}\n\
        edge WorksAt: Person -> Company @card(0..) {\n  role: String?\n}\n\
        edge Knows: Person -> Person @card(0..)\n\
        edge Manager: Person -> Person @card(0..1)\n";

    fn schema() -> SchemaModel {
        SchemaModel::parse(SCHEMA).expect("test schema compiles")
    }

    fn person(id: &str) -> NodeRef {
        NodeRef::new("Person", id)
    }

    fn output(columns: &[&str], rows: Value) -> ReadOutput {
        ReadOutput {
            query_name: "q".into(),
            target: ReadTargetOutput {
                branch: Some("main".into()),
                snapshot: None,
            },
            row_count: rows.as_array().map_or(0, |r| r.len() as i64),
            columns: columns.iter().map(|c| c.to_string()).collect(),
            rows,
            graph_commit_id: Some("c1".into()),
        }
    }

    fn group(
        schema: &SchemaModel,
        of: &str,
        edge: &str,
        direction: Direction,
    ) -> RelationshipGroup {
        schema
            .relationships_of(of)
            .into_iter()
            .find(|g| g.edge == edge && g.direction == direction)
            .unwrap()
    }

    fn loaded_root(schema: &SchemaModel) -> ExploreState {
        let mut state = ExploreState::rooted(person("ada"));
        assert_eq!(state.missing_loads(schema), vec![LoadTarget::Root]);
        state.apply_root(&output(
            &["id", "label"],
            json!([{"id": "ada", "label": "Ada"}]),
        ));
        state
    }

    fn kinds(entries: &[Entry]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| {
                let what = match &entry.kind {
                    EntryKind::Node { node, .. } => format!("node {}", node.id),
                    EntryKind::Cycle { node, ancestor, .. } => {
                        format!("cycle {} -> {ancestor}", node.id)
                    }
                    EntryKind::Field { key, target, .. } => format!(
                        "field {} {}",
                        key.group.edge,
                        target.as_ref().map_or("-", |t| t.node.id.as_str())
                    ),
                    EntryKind::Group { key, count, .. } => format!(
                        "group {} {:?} {:?}",
                        key.group.edge,
                        key.group.direction,
                        count.loaded()
                    ),
                    EntryKind::More { shown, total, .. } => format!("more {shown}/{total:?}"),
                    EntryKind::Loading { .. } => "loading".into(),
                    EntryKind::Error { message, .. } => format!("error {message}"),
                };
                format!("{}{}", "  ".repeat(entry.depth), what)
            })
            .collect()
    }

    #[test]
    fn the_root_reads_first_and_alone() {
        let schema = schema();
        let state = ExploreState::rooted(person("ada"));
        assert_eq!(kinds(&state.entries(&schema)), ["node ada", "  loading"]);
        let state = loaded_root(&schema);
        assert_eq!(state.label(&person("ada")), Some("Ada"));
        // Groups need counts; the field needs its single target.
        let missing = state.missing_loads(&schema);
        assert_eq!(missing.len(), 5, "{missing:?}");
        assert!(missing.contains(&LoadTarget::Page(GroupKey {
            node: person("ada"),
            group: group(&schema, "Person", "Manager", Direction::Outgoing),
        })));
    }

    #[test]
    fn a_missing_root_is_an_error_row() {
        let schema = schema();
        let mut state = ExploreState::rooted(person("nobody"));
        state.apply_root(&output(&["id", "label"], json!([])));
        let entries = state.entries(&schema);
        assert!(matches!(
            &entries[1].kind,
            EntryKind::Error { target: LoadTarget::Root, message } if message.contains("nobody")
        ));
        assert!(state.missing_loads(&schema).is_empty());
    }

    #[test]
    fn rows_flatten_with_fields_groups_neighbours_and_cycles() {
        let schema = schema();
        let mut state = loaded_root(&schema);
        let knows_out = group(&schema, "Person", "Knows", Direction::Outgoing);
        let knows_in = group(&schema, "Person", "Knows", Direction::Incoming);
        let works = group(&schema, "Person", "WorksAt", Direction::Outgoing);
        let manager = group(&schema, "Person", "Manager", Direction::Outgoing);
        let ada = person("ada");
        let key = |node: &NodeRef, group: &RelationshipGroup| GroupKey {
            node: node.clone(),
            group: group.clone(),
        };
        state.apply_count(key(&ada, &works), &output(&["n"], json!([{"n": 0}])));
        state.apply_count(key(&ada, &knows_out), &output(&["n"], json!([{"n": 2}])));
        state.apply_count(key(&ada, &knows_in), &output(&["n"], json!([{"n": 1}])));
        state.apply_page(
            key(&ada, &manager),
            &output(&["id", "label"], json!([{"id": "bob", "label": "Bob"}])),
            50,
        );
        state.apply_page(
            key(&ada, &knows_out),
            &output(
                &["id", "label"],
                json!([{"id": "bob", "label": "Bob"}, {"id": "cy", "label": "Cy"}]),
            ),
            50,
        );
        let root = ExplorePath::root();
        let knows_path = root.child_group(&knows_out);
        state.expand(&knows_path);
        // Zero-count groups stay closed.
        state.expand(&root.child_group(&works));
        assert!(!state.is_expanded(&root.child_group(&works)));
        // Bob knows Ada back: a cycle row pointing at the root.
        let bob_path = knows_path.child_node(&person("bob"));
        state.expand(&bob_path);
        let bob_knows = bob_path.child_group(&knows_out);
        state.expand(&bob_knows);
        state.apply_page(
            key(&person("bob"), &knows_out),
            &output(&["id", "label"], json!([{"id": "ada", "label": "Ada"}])),
            50,
        );
        let entries = state.entries(&schema);
        let shown = kinds(&entries);
        assert_eq!(
            shown[..6],
            [
                "node ada",
                "  group Knows Outgoing Some(2)",
                "    node bob",
                "      group Knows Outgoing None",
                "        cycle ada -> 0",
                "      field Manager -",
            ],
            "{shown:#?}"
        );
        assert!(
            shown.contains(&"  group WorksAt Outgoing Some(0)".to_owned()),
            "{shown:#?}"
        );
        assert!(
            shown.contains(&"  field Manager bob".to_owned()),
            "{shown:#?}"
        );
        assert!(
            shown.contains(&"  group Knows Incoming Some(1)".to_owned()),
            "{shown:#?}"
        );
        // Parents point at the right rows.
        let bob = entries
            .iter()
            .position(|e| e.path == bob_path)
            .expect("bob is visible");
        assert_eq!(entries[bob].parent, Some(1));
        assert_eq!(
            state.path_of(&schema, &person("cy")),
            Some(knows_path.child_node(&person("cy")))
        );
    }

    #[test]
    fn paging_appends_and_offers_more_until_exhausted() {
        let schema = schema();
        let mut state = loaded_root(&schema);
        let knows = group(&schema, "Person", "Knows", Direction::Outgoing);
        let key = GroupKey {
            node: person("ada"),
            group: knows.clone(),
        };
        state.apply_count(key.clone(), &output(&["n"], json!([{"n": 3}])));
        state.expand(&ExplorePath::root().child_group(&knows));
        assert!(
            state
                .missing_loads(&schema)
                .contains(&LoadTarget::Page(key.clone()))
        );
        state.apply_page(
            key.clone(),
            &output(&["id"], json!([{"id": "p1"}, {"id": "p2"}])),
            2,
        );
        let list = &state.neighbours[&key];
        assert_eq!(list.next_after(), Some("p2"));
        assert!(!list.exhausted);
        assert!(kinds(&state.entries(&schema)).contains(&"    more 2/Some(3)".to_owned()));
        state.apply_page(key.clone(), &output(&["id"], json!([{"id": "p3"}])), 2);
        let list = &state.neighbours[&key];
        assert!(list.exhausted && list.rows.len() == 3);
        assert!(
            !kinds(&state.entries(&schema))
                .iter()
                .any(|k| k.contains("more"))
        );
        // A full last page with a known count does not offer more either.
        assert!(
            !NeighbourList {
                rows: list.rows.clone(),
                ..Default::default()
            }
            .has_more(Some(3))
        );
    }

    #[test]
    fn edge_properties_skip_nulls_and_keep_order() {
        let schema = schema();
        let mut state = loaded_root(&schema);
        let works = group(&schema, "Person", "WorksAt", Direction::Outgoing);
        let key = GroupKey {
            node: person("ada"),
            group: works,
        };
        state.apply_page(
            key.clone(),
            &output(
                &["id", "label", "e_role", "e_since"],
                json!([
                    {"id": "acme", "label": "Acme", "e_role": "lead"},
                    {"id": "acme", "label": "Acme", "e_role": "staff"},
                    {"id": "b", "e_since": null}
                ]),
            ),
            50,
        );
        let rows = &state.neighbours[&key].rows;
        assert_eq!(rows.len(), 2, "a parallel edge to acme is one row");
        assert_eq!(rows[0].node, NodeRef::new("Company", "acme"));
        assert_eq!(rows[0].edge, vec![("role".to_owned(), json!("lead"))]);
        assert!(rows[1].edge.is_empty() && rows[1].label.is_none());
    }

    #[test]
    fn collapse_keeps_children_and_collapse_subtree_drops_them() {
        let schema = schema();
        let mut state = loaded_root(&schema);
        let knows = group(&schema, "Person", "Knows", Direction::Outgoing);
        let group_path = ExplorePath::root().child_group(&knows);
        let bob = group_path.child_node(&person("bob"));
        state.expand(&group_path);
        state.expand(&bob);
        state.collapse(&group_path);
        assert!(state.is_expanded(&bob));
        state.collapse_subtree(&ExplorePath::root());
        assert!(state.expanded.is_empty());
        state.expand_all(&schema, &ExplorePath::root());
        // Every group but the field.
        assert_eq!(state.expanded.len(), 1 + 4);
    }

    #[test]
    fn reroot_keeps_expansions_below_and_extends_the_breadcrumb() {
        let schema = schema();
        let mut state = loaded_root(&schema);
        let knows = group(&schema, "Person", "Knows", Direction::Outgoing);
        let bob = ExplorePath::root()
            .child_group(&knows)
            .child_node(&person("bob"));
        let cy = bob.child_group(&knows).child_node(&person("cy"));
        state.labels.insert(person("cy"), Some("Cy".into()));
        state.expand(&cy);
        state.expand(&cy.child_group(&knows));
        assert!(state.reroot(&cy));
        assert_eq!(state.root, Some(person("cy")));
        assert_eq!(state.breadcrumb, vec![person("ada"), person("bob")]);
        assert!(state.is_expanded(&ExplorePath::root().child_group(&knows)));
        assert_eq!(state.root_status, Loadable::Loaded(()));
        assert!(!state.reroot(&ExplorePath::root()));
        let _ = schema;
    }
}

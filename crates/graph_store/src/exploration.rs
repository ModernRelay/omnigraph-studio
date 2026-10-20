//! The exploration the relationship tree (spec 005) and the graph canvas (spec 006) share: one
//! root node, the paths a person expanded from it, and the group counts and neighbour pages
//! those paths show.
//!
//! Every read is pinned to the commit the first read returned (constitution II). When the
//! session sees its branch's head at another commit ([`SessionEvent::HeadMoved`]: Studio's own
//! commit, or another writer's), the pin moves to the head if nothing the exploration shows
//! changed in between (spec 015); otherwise [`Exploration::newer_available`] is raised and
//! nothing changes until [`Exploration::refresh`]. Counts come from the session's batched counts
//! ([`GraphSession::degree`]): every count asked for in one turn (several nodes expanded,
//! a refresh) is one `builder::page_degrees` read per node type and relationship group (engine
//! change E16 would replace them); neighbours come in keyset
//! pages from `builder::neighbours`, as many as the graph's `tree_page_size` setting. A
//! relationship grouped by a property shows one row per value, from
//! `builder::neighbour_values`, each opening to the neighbours with that value.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use gpui::{
    App, AppContext as _, Context, Entity, EntityId, EventEmitter, Global, Subscription, Task,
    WeakEntity,
};
use gq_language::builder::{GroupBy, GroupOn, Narrow};
use gq_language::{BuiltQuery, Direction, RelationshipGroup, SchemaModel, builder};
use graph_client::ReadError;
use graph_client::api::ReadOutput;
use serde_json::Value;

use crate::scope::{ReadScope, ScopeSince};
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

/// One step of a path from the root: into a relationship group, into one value of the property
/// it is grouped by, or into one of its nodes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Step {
    Group(RelationshipGroup),
    Value(BucketValue),
    Node(NodeRef),
}

/// One value of a grouping property, kept as JSON text so it can key maps; `null` stands for
/// neighbours without a value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BucketValue(String);

impl BucketValue {
    pub fn new(value: &Value) -> Self {
        Self(value.to_string())
    }

    pub fn value(&self) -> Value {
        serde_json::from_str(&self.0).unwrap_or(Value::Null)
    }

    pub fn is_null(&self) -> bool {
        self.0 == "null"
    }
}

/// A relationship group narrowed to the neighbours with one value of a property.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Within {
    pub by: GroupBy,
    pub value: BucketValue,
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

    pub fn child_value(&self, value: &BucketValue) -> Self {
        let mut steps = self.0.clone();
        steps.push(Step::Value(value.clone()));
        Self(steps)
    }

    pub fn child_node(&self, node: &NodeRef) -> Self {
        let mut steps = self.0.clone();
        steps.push(Step::Node(node.clone()));
        Self(steps)
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

    /// The group this path ends in, or the group whose value it ends at, with that value.
    pub fn group_step(&self) -> Option<(&RelationshipGroup, Option<&BucketValue>)> {
        match self.0.as_slice() {
            [.., Step::Group(group)] => Some((group, None)),
            [.., Step::Group(group), Step::Value(value)] => Some((group, Some(value))),
            _ => None,
        }
    }

    /// Whether the path ends at a node (the root included), not a group or a value.
    pub fn ends_at_node(&self) -> bool {
        matches!(self.0.last(), None | Some(Step::Node(_)))
    }

    /// Whether the path passes through a value of `group`.
    fn passes_value_of(&self, group: &RelationshipGroup) -> bool {
        self.0
            .windows(2)
            .any(|pair| matches!(pair, [Step::Group(g), Step::Value(_)] if g == group))
    }

    /// The node this path ends at, or the node whose group it ends in.
    pub fn node<'a>(&'a self, root: &'a NodeRef) -> &'a NodeRef {
        self.0
            .iter()
            .rev()
            .find_map(|step| match step {
                Step::Node(node) => Some(node),
                Step::Group(_) | Step::Value(_) => None,
            })
            .unwrap_or(root)
    }

    /// The node steps of the path, root excluded.
    pub fn nodes(&self) -> impl Iterator<Item = &NodeRef> {
        self.0.iter().filter_map(|step| match step {
            Step::Node(node) => Some(node),
            Step::Group(_) | Step::Value(_) => None,
        })
    }

    /// The path with `prefix` removed, when it starts with it.
    pub fn strip_prefix(&self, prefix: &ExplorePath) -> Option<Self> {
        self.starts_with(prefix)
            .then(|| Self(self.0[prefix.0.len()..].to_vec()))
    }
}

/// One relationship group of one node, or the part of it with one value of a grouping
/// property: the unit counts and neighbour pages are cached by.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GroupKey {
    pub node: NodeRef,
    pub group: RelationshipGroup,
    pub within: Option<Within>,
}

impl GroupKey {
    pub fn new(node: NodeRef, group: RelationshipGroup) -> Self {
        Self {
            node,
            group,
            within: None,
        }
    }

    /// The part of this group with one value of `by`.
    pub fn with_value(&self, by: &GroupBy, value: &BucketValue) -> Self {
        Self {
            within: Some(Within {
                by: by.clone(),
                value: value.clone(),
            }),
            ..self.clone()
        }
    }
}

/// The values of a grouping property among a group's neighbours, most common first.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Buckets {
    pub values: Vec<(BucketValue, u64)>,
    /// Grouped by a relationship, each value's node label, when its type has one.
    pub labels: HashMap<BucketValue, String>,
    /// Every value was read; otherwise only the most common ones were.
    pub complete: bool,
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
    Values(GroupKey, GroupBy),
}

/// How many values of a grouping property one read returns, most common first.
pub const VALUES_LIMIT: u32 = 100;

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
    /// One value of the property the parent group is grouped by, and how many neighbours have
    /// it.
    Bucket {
        key: GroupKey,
        count: u64,
        expanded: bool,
    },
    /// Below a group's values: it has values beyond the most common ones read, holding `rest`
    /// neighbours when the group's count is known.
    OtherValues {
        shown: usize,
        rest: Option<u64>,
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
    /// The property each relationship's neighbours are grouped by, wherever it appears.
    pub grouping: HashMap<RelationshipGroup, GroupBy>,
    pub values: HashMap<(GroupKey, GroupBy), Loadable<Buckets>>,
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
            grouping: HashMap::new(),
            values: HashMap::new(),
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

    /// A bucket's label: grouped by a relationship, the name of the node it stands for.
    pub fn bucket_label(&self, key: &GroupKey) -> Option<&str> {
        let within = key.within.as_ref()?;
        let group = GroupKey {
            within: None,
            ..key.clone()
        };
        match self.values.get(&(group, within.by.clone()))? {
            Loadable::Loaded(buckets) => buckets.labels.get(&within.value).map(String::as_str),
            _ => None,
        }
    }

    pub fn is_expanded(&self, path: &ExplorePath) -> bool {
        self.expanded.contains(path)
    }

    fn group_key(&self, path: &ExplorePath) -> Option<GroupKey> {
        let root = self.root.as_ref()?;
        let (group, value) = path.group_step()?;
        let key = GroupKey::new(path.node(root).clone(), group.clone());
        match value {
            None => Some(key),
            Some(value) => Some(key.with_value(self.grouping.get(group)?, value)),
        }
    }

    /// Groups a relationship's neighbours by a property wherever it appears, or stops; values
    /// opened under the old grouping close.
    pub fn set_grouping(&mut self, group: &RelationshipGroup, by: Option<GroupBy>) {
        self.expanded.retain(|path| !path.passes_value_of(group));
        if let Some(selection) = &self.selection
            && selection.passes_value_of(group)
        {
            let steps = selection.steps();
            let at = steps
                .windows(2)
                .position(|pair| matches!(pair, [Step::Group(g), Step::Value(_)] if g == group))
                .unwrap_or(0);
            self.selection = Some(ExplorePath(steps[..=at].to_vec()));
        }
        match by {
            Some(by) => self.grouping.insert(group.clone(), by),
            None => self.grouping.remove(group),
        };
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
            if !is_field(schema, group) {
                self.expand(&path.child_group(group));
            }
        }
    }

    /// Makes the node at `path` the root. Expansions below it are kept; the old root and the
    /// nodes on the way join the breadcrumb.
    pub fn reroot(&mut self, path: &ExplorePath) -> bool {
        let Some(root) = self.root.clone() else {
            return false;
        };
        if path.is_root() || !path.ends_at_node() {
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
        self.values.clear();
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
                        path,
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
                        path,
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
            let key = GroupKey::new(node.clone(), group.clone());
            let group_path = path.child_group(group);
            let list = self.neighbours.get(&key);
            if is_field(schema, group) {
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
            match self.grouping.get(group) {
                Some(by) => self.push_values(
                    schema,
                    &key,
                    by,
                    &count,
                    &group_path,
                    depth + 2,
                    group_index,
                    ancestors,
                    out,
                ),
                None => self.push_list(
                    schema,
                    &key,
                    count.loaded().copied(),
                    &group_path,
                    depth + 2,
                    group_index,
                    ancestors,
                    out,
                ),
            }
        }
        ancestors.pop();
    }

    /// A group's values, each with its count, and the neighbours of each value opened.
    #[allow(clippy::too_many_arguments)]
    fn push_values(
        &self,
        schema: &SchemaModel,
        key: &GroupKey,
        by: &GroupBy,
        count: &Loadable<u64>,
        group_path: &ExplorePath,
        depth: usize,
        parent: usize,
        ancestors: &mut Vec<(NodeRef, usize)>,
        out: &mut Vec<Entry>,
    ) {
        let target = LoadTarget::Values(key.clone(), by.clone());
        let buckets = match self.values.get(&(key.clone(), by.clone())) {
            Some(Loadable::Loaded(buckets)) => buckets,
            Some(Loadable::Failed(message)) => {
                out.push(Entry {
                    path: group_path.clone(),
                    depth,
                    parent: Some(parent),
                    kind: EntryKind::Error {
                        target,
                        message: message.clone(),
                    },
                });
                return;
            }
            None | Some(Loadable::NotLoaded | Loadable::Loading) => {
                out.push(Entry {
                    path: group_path.clone(),
                    depth,
                    parent: Some(parent),
                    kind: EntryKind::Loading { target },
                });
                return;
            }
        };
        for (value, n) in &buckets.values {
            let bucket = key.with_value(by, value);
            let path = group_path.child_value(value);
            let expanded = *n > 0 && self.expanded.contains(&path);
            let index = out.len();
            out.push(Entry {
                path: path.clone(),
                depth,
                parent: Some(parent),
                kind: EntryKind::Bucket {
                    key: bucket.clone(),
                    count: *n,
                    expanded,
                },
            });
            if expanded {
                self.push_list(
                    schema,
                    &bucket,
                    Some(*n),
                    &path,
                    depth + 1,
                    index,
                    ancestors,
                    out,
                );
            }
        }
        let shown: u64 = buckets.values.iter().map(|(_, n)| n).sum();
        let rest = count.loaded().map(|total| total.saturating_sub(shown));
        if !buckets.complete && rest != Some(0) {
            out.push(Entry {
                path: group_path.clone(),
                depth,
                parent: Some(parent),
                kind: EntryKind::OtherValues {
                    shown: buckets.values.len(),
                    rest,
                },
            });
        }
    }

    /// The neighbours read so far under a group or a value, then a row to read more.
    #[allow(clippy::too_many_arguments)]
    fn push_list(
        &self,
        schema: &SchemaModel,
        key: &GroupKey,
        total: Option<u64>,
        list_path: &ExplorePath,
        depth: usize,
        parent: usize,
        ancestors: &mut Vec<(NodeRef, usize)>,
        out: &mut Vec<Entry>,
    ) {
        let Some(list) = self.neighbours.get(key) else {
            out.push(Entry {
                path: list_path.clone(),
                depth,
                parent: Some(parent),
                kind: EntryKind::Loading {
                    target: LoadTarget::Page(key.clone()),
                },
            });
            return;
        };
        for neighbour in &list.rows {
            let node_path = list_path.child_node(&neighbour.node);
            if let Some((_, ancestor)) = ancestors.iter().find(|(n, _)| *n == neighbour.node) {
                out.push(Entry {
                    path: node_path,
                    depth,
                    parent: Some(parent),
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
                    depth,
                    Some(parent),
                    ancestors,
                    out,
                );
            }
        }
        let tail = match &list.status {
            PageStatus::Loading => Some(EntryKind::Loading {
                target: LoadTarget::Page(key.clone()),
            }),
            PageStatus::Failed(message) => Some(EntryKind::Error {
                target: LoadTarget::Page(key.clone()),
                message: message.clone(),
            }),
            PageStatus::Idle => list.has_more(total).then(|| EntryKind::More {
                shown: list.rows.len(),
                total,
                key: key.clone(),
            }),
        };
        if let Some(kind) = tail {
            out.push(Entry {
                path: list_path.clone(),
                depth,
                parent: Some(parent),
                kind,
            });
        }
    }

    /// Whether an answer to `target` can change what is shown: the relationship it reads is
    /// open, or (for counts and single-target fields) the node it belongs to is. An answer read
    /// ahead for something closed shows nothing until that opens, and opening it redraws.
    pub fn shows(&self, schema: &SchemaModel, target: &LoadTarget) -> bool {
        let Some(root) = &self.root else {
            return false;
        };
        let node_open = |node: &NodeRef| {
            self.expanded
                .iter()
                .any(|path| path.ends_at_node() && path.node(root) == node)
        };
        let group_open = |key: &GroupKey| {
            self.expanded
                .iter()
                .any(|path| self.group_key(path).as_ref() == Some(key))
        };
        match target {
            LoadTarget::Root => true,
            LoadTarget::Count(key) => node_open(&key.node),
            LoadTarget::Page(key) if key.within.is_none() && is_field(schema, &key.group) => {
                node_open(&key.node)
            }
            LoadTarget::Page(key) | LoadTarget::Values(key, _) => group_open(key),
        }
    }

    /// The reads opening `path` would need that are not made yet: a node's relationship
    /// counts (and its single-target fields), or a relationship's first neighbours, or its
    /// values when it is grouped. A relationship known to be empty needs none.
    pub fn prefetch_targets(&self, schema: &SchemaModel, path: &ExplorePath) -> Vec<LoadTarget> {
        let Some(root) = &self.root else {
            return Vec::new();
        };
        if path.ends_at_node() {
            let node = path.node(root);
            return schema
                .relationships_of(&node.type_name)
                .iter()
                .filter_map(|group| {
                    let key = GroupKey::new(node.clone(), group.clone());
                    if is_field(schema, group) {
                        (!self.neighbours.contains_key(&key)).then_some(LoadTarget::Page(key))
                    } else {
                        (!self.counts.contains_key(&key)).then_some(LoadTarget::Count(key))
                    }
                })
                .collect();
        }
        let Some(key) = self.group_key(path) else {
            return Vec::new();
        };
        if matches!(self.counts.get(&key), Some(Loadable::Loaded(0))) {
            return Vec::new();
        }
        match (&key.within, self.grouping.get(&key.group)) {
            (None, Some(by)) => {
                let values = (key.clone(), by.clone());
                (!self.values.contains_key(&values))
                    .then_some(LoadTarget::Values(key, by.clone()))
                    .into_iter()
                    .collect()
            }
            _ => (!self.neighbours.contains_key(&key))
                .then_some(LoadTarget::Page(key))
                .into_iter()
                .collect(),
        }
    }

    /// The reads the visible entries still need, each once, in tree order.
    #[cfg(test)]
    pub fn missing_loads(&self, schema: &SchemaModel) -> Vec<LoadTarget> {
        self.missing_in(&self.entries(schema))
    }

    /// The reads the visible entries still need, each once, in tree order, of the state
    /// flattened as `entries`.
    fn missing_in(&self, entries: &[Entry]) -> Vec<LoadTarget> {
        if self.root.is_none() {
            return Vec::new();
        }
        if self.root_status == Loadable::NotLoaded {
            return vec![LoadTarget::Root];
        }
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for entry in entries {
            let target = match &entry.kind {
                EntryKind::Group {
                    key,
                    count: Loadable::NotLoaded,
                    ..
                } => LoadTarget::Count(key.clone()),
                EntryKind::Field { key, .. } if !self.neighbours.contains_key(key) => {
                    LoadTarget::Page(key.clone())
                }
                EntryKind::Loading {
                    target: LoadTarget::Page(key),
                } if !self.neighbours.contains_key(key) => LoadTarget::Page(key.clone()),
                EntryKind::Loading {
                    target: LoadTarget::Values(key, by),
                } if !self.values.contains_key(&(key.clone(), by.clone())) => {
                    LoadTarget::Values(key.clone(), by.clone())
                }
                _ => continue,
            };
            if seen.insert(target.clone()) {
                out.push(target);
            }
        }
        out
    }

    /// The shallowest visible path at which `node` appears.
    #[cfg(test)]
    pub fn path_of(&self, schema: &SchemaModel, node: &NodeRef) -> Option<ExplorePath> {
        path_in(&self.entries(schema), node)
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
    pub fn apply_count(&mut self, key: GroupKey, count: u64) {
        self.counts.insert(key, Loadable::Loaded(count));
    }

    /// Applies the answer to a values read, most common first and neighbours without a value
    /// last. Each value's count is its neighbour list's count.
    pub fn apply_values(&mut self, key: GroupKey, by: GroupBy, output: &ReadOutput, limit: u32) {
        let mut labels = HashMap::new();
        let mut values: Vec<(BucketValue, u64)> = rows(output)
            .iter()
            .map(|row| {
                let value = row.get("value").cloned().unwrap_or(Value::Null);
                let n = row.get("n").and_then(Value::as_u64).unwrap_or(0);
                let value = BucketValue::new(&value);
                if let Some(label) = row.get("label").and_then(Value::as_str) {
                    labels.insert(value.clone(), label.to_owned());
                }
                (value, n)
            })
            .filter(|(value, n)| !value.is_null() || *n > 0)
            .collect();
        values.sort_by_key(|(value, _)| value.is_null());
        for (value, n) in &values {
            self.counts
                .insert(key.with_value(&by, value), Loadable::Loaded(*n));
        }
        // The rows the grouped read returned under its limit: by a relationship, the count of
        // neighbours with none came from a read of its own.
        let read = rows(output)
            .iter()
            .filter(|row| by.on != GroupOn::Relationship || row.get("value").is_some())
            .count();
        let complete = (read as u32) < limit;
        self.values.insert(
            (key, by),
            Loadable::Loaded(Buckets {
                values,
                labels,
                complete,
            }),
        );
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
        let mut shown: HashSet<NodeRef> = list.rows.iter().map(|row| row.node.clone()).collect();
        for neighbour in page {
            if shown.insert(neighbour.node.clone()) {
                list.rows.push(neighbour);
            }
        }
        list.status = PageStatus::Idle;
    }
}

fn rows(output: &ReadOutput) -> Vec<&serde_json::Map<String, Value>> {
    match &*output.rows {
        Value::Array(rows) => rows.iter().filter_map(Value::as_object).collect(),
        _ => Vec::new(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplorationEvent {
    /// The root or the expansions changed: what is shown changed now.
    Changed,
    /// Reads finished whose answers change what is shown. Several often finish together (one
    /// count per relationship of a node), so a view may draw them in one go.
    Loaded,
    /// The selected path changed.
    SelectionChanged,
}

#[derive(Default)]
struct ExplorationRegistry(HashMap<EntityId, WeakEntity<Exploration>>);

impl Global for ExplorationRegistry {}

/// The shallowest of `entries` at which `node` appears.
fn path_in(entries: &[Entry], node: &NodeRef) -> Option<ExplorePath> {
    entries
        .iter()
        .filter(|entry| matches!(&entry.kind, EntryKind::Node { node: n, .. } if n == node))
        .min_by_key(|entry| entry.depth)
        .map(|entry| entry.path.clone())
}

/// The exploration's state and its flattening as last made, under the schema it was made
/// with. Any access to the state through `&mut` lets the flattening go, so it is never stale,
/// and the tree, the canvas and the reads still due share one walk of the tree per change.
#[derive(Default)]
struct Flattened {
    state: ExploreState,
    entries: RefCell<Option<MadeEntries>>,
}

/// A flattening, and the schema it was made under.
type MadeEntries = (Arc<SchemaModel>, Arc<Vec<Entry>>);

impl Flattened {
    fn entries(&self, schema: &Arc<SchemaModel>) -> Arc<Vec<Entry>> {
        let mut held = self.entries.borrow_mut();
        if let Some((made_with, entries)) = held.as_ref()
            && Arc::ptr_eq(made_with, schema)
        {
            return entries.clone();
        }
        let entries = Arc::new(self.state.entries(schema));
        *held = Some((schema.clone(), entries.clone()));
        entries
    }
}

impl Deref for Flattened {
    type Target = ExploreState;
    fn deref(&self) -> &ExploreState {
        &self.state
    }
}

impl DerefMut for Flattened {
    fn deref_mut(&mut self) -> &mut ExploreState {
        self.entries.get_mut().take();
        &mut self.state
    }
}

/// One session's exploration. Get it with [`Exploration::for_session`]; the tree and the
/// canvas of a session share it.
pub struct Exploration {
    session: Entity<GraphSession>,
    state: Flattened,
    pin: Option<String>,
    newer_available: bool,
    generation: u64,
    tasks: HashMap<LoadTarget, Task<()>>,
    /// What the exploration has read at its pin: a head move that changed none of it moves the
    /// pin without reading again (spec 015).
    scope: ReadScope,
    /// The head moved and the session is reading what changed.
    awaiting_changes: bool,
    /// A look for what is missing is due at the end of this turn: many answers come in one
    /// turn (a node's counts), and each look walks the whole tree.
    missing_due: bool,
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
        let scope = session.update(cx, |session, _| {
            let branch = session.branch.clone();
            session.read_scope(&branch)
        });
        let subscription = cx.subscribe(&session, |this, session, event, cx| match event {
            SessionEvent::SchemaChanged => this.request_missing(cx),
            // Another writer changed the schema: what is shown stays until Refresh.
            SessionEvent::SchemaMoved if this.pin.is_some() && !this.newer_available => {
                this.newer_available = true;
                this.changed(cx);
            }
            SessionEvent::BranchChanged => {
                if let Some(root) = this.state.root.clone() {
                    this.root_at(root, cx);
                }
            }
            SessionEvent::HeadMoved { branch, .. } if *branch == session.read(cx).branch => {
                this.compare_head(cx)
            }
            SessionEvent::ChangesRead { branch }
                if this.awaiting_changes && *branch == session.read(cx).branch =>
            {
                this.compare_head(cx)
            }
            _ => {}
        });
        Self {
            session,
            state: Flattened::default(),
            pin: None,
            newer_available: false,
            generation: 0,
            tasks: HashMap::new(),
            scope,
            awaiting_changes: false,
            missing_due: false,
            _subscription: subscription,
        }
    }

    /// Offers Refresh when the branch head is not the commit the exploration shows, unless
    /// nothing it reads changed in between: then the pin moves to the head.
    fn compare_head(&mut self, cx: &mut Context<Self>) {
        self.awaiting_changes = false;
        if self.newer_available {
            return;
        }
        let session = self.session.read(cx);
        let (Some(pin), Some(head)) = (self.pin.clone(), session.head(&session.branch)) else {
            return;
        };
        if pin == head {
            return;
        }
        match session.scope_since(&self.scope, &pin, head) {
            ScopeSince::Pending => self.awaiting_changes = true,
            ScopeSince::Unchanged => self.pin = Some(head.to_owned()),
            ScopeSince::Changed => {
                self.newer_available = true;
                self.changed(cx);
            }
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
    #[cfg(test)]
    pub fn pin(&self) -> Option<&str> {
        self.pin.as_deref()
    }

    /// The branch moved since the pin; offer Refresh.
    pub fn newer_available(&self) -> bool {
        self.newer_available
    }

    /// The exploration flattened in tree order; empty until the schema loads. Made once per
    /// change to it, whoever asks.
    pub fn entries(&self, cx: &App) -> Arc<Vec<Entry>> {
        match self.schema(cx) {
            Some(schema) => self.state.entries(&schema),
            None => Arc::default(),
        }
    }

    /// Starts a new exploration at `node`, at the branch head.
    pub fn root_at(&mut self, node: NodeRef, cx: &mut Context<Self>) {
        self.reset_reads(cx);
        let grouping = std::mem::take(&mut self.state.grouping);
        *self.state = ExploreState::rooted(node);
        self.state.grouping = grouping;
        self.request_missing(cx);
        self.changed(cx);
        cx.emit(ExplorationEvent::SelectionChanged);
    }

    /// Selects `node` where it is already visible, else roots the exploration at it; whether it
    /// was visible.
    pub fn reveal(&mut self, node: NodeRef, cx: &mut Context<Self>) -> bool {
        let path = self
            .schema(cx)
            .and_then(|schema| path_in(&self.state.entries(&schema), &node));
        let shown = path.is_some();
        match path {
            Some(path) => self.select(Some(path), cx),
            None => self.root_at(node, cx),
        }
        shown
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
        let values = std::mem::take(&mut self.state.values);
        let grouping = std::mem::take(&mut self.state.grouping);
        // A searched group's neighbours are its matches: the search goes with them.
        let searches = std::mem::take(&mut self.state.searches);
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
        state.values = values;
        state.grouping = grouping;
        state.searches = searches;
        *self.state = state;
        self.request_missing(cx);
        self.changed(cx);
        cx.emit(ExplorationEvent::SelectionChanged);
    }

    pub fn is_expanded(&self, path: &ExplorePath) -> bool {
        self.state.is_expanded(path)
    }

    pub fn expand(&mut self, path: &ExplorePath, cx: &mut Context<Self>) {
        self.set_open(std::slice::from_ref(path), true, cx);
    }

    pub fn collapse(&mut self, path: &ExplorePath, cx: &mut Context<Self>) {
        self.set_open(std::slice::from_ref(path), false, cx);
    }

    /// Opens (`open`) or closes every one of `paths`, as one change: views draw it once.
    pub fn set_open(&mut self, paths: &[ExplorePath], open: bool, cx: &mut Context<Self>) {
        for path in paths {
            if open {
                self.state.expand(path);
            } else {
                self.state.collapse(path);
            }
        }
        if open {
            self.request_missing(cx);
        }
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

    /// The property a relationship's neighbours are grouped by, if any.
    pub fn grouping(&self, group: &RelationshipGroup) -> Option<&GroupBy> {
        self.state.grouping.get(group)
    }

    /// Groups a relationship's neighbours by a property wherever it appears in this
    /// exploration, or stops grouping them.
    pub fn set_grouping(
        &mut self,
        group: &RelationshipGroup,
        by: Option<GroupBy>,
        cx: &mut Context<Self>,
    ) {
        if self.state.grouping.get(group) == by.as_ref() {
            return;
        }
        self.state.set_grouping(group, by);
        self.request_missing(cx);
        self.changed(cx);
        cx.emit(ExplorationEvent::SelectionChanged);
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

    /// Reads ahead what opening `path` will show, without opening it, so it opens at once
    /// (spec 006: a pill the pointer rests on). Only once the exploration has its commit, so
    /// every read stays on it.
    pub fn prefetch(&mut self, path: &ExplorePath, cx: &mut Context<Self>) {
        if self.pin.is_none() {
            return;
        }
        let Some(schema) = self.schema(cx) else {
            return;
        };
        // A target read or being read is in the state already, so it is not among these.
        for target in self.state.prefetch_targets(&schema, path) {
            self.start(target, cx);
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
    /// A root is read again at the branch head, with everything below it: one not found at
    /// the pinned commit would not be found there again, and nothing below it was shown.
    pub fn retry(&mut self, target: &LoadTarget, cx: &mut Context<Self>) {
        match target {
            LoadTarget::Root => return self.refresh(cx),
            LoadTarget::Count(key) => {
                self.state.counts.remove(key);
            }
            LoadTarget::Values(key, by) => {
                self.state.values.remove(&(key.clone(), by.clone()));
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
        self.reset_reads(cx);
        self.state.clear_loaded();
        self.request_missing(cx);
        self.changed(cx);
    }

    fn reset_reads(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.tasks.clear();
        self.pin = None;
        self.newer_available = false;
        let scope = &self.scope;
        self.session.update(cx, |session, _| {
            let branch = session.branch.clone();
            session.reset_scope(scope, &branch);
        });
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
        let entries = self.state.entries(&schema);
        for target in self.state.missing_in(&entries) {
            self.start(target, cx);
        }
    }

    fn start(&mut self, target: LoadTarget, cx: &mut Context<Self>) {
        let Some(schema) = self.schema(cx) else {
            return;
        };
        let query = match &target {
            LoadTarget::Root => {
                let Some(root) = self.state.root.clone() else {
                    return;
                };
                self.state.root_status = Loadable::Loading;
                builder::node_label(&schema, &root.type_name, &root.id)
            }
            LoadTarget::Count(key) => {
                self.state.counts.insert(key.clone(), Loadable::Loading);
                let snapshot = self.pin.clone();
                let scope = &self.scope;
                let count = self.session.update(cx, |session, cx| {
                    session.degree_in(
                        scope,
                        &key.node.type_name,
                        &key.group,
                        &key.node.id,
                        snapshot,
                        cx,
                    )
                });
                self.spawn_count(target.clone(), count, cx);
                return;
            }
            LoadTarget::Page(key) => {
                let page_size = self.page_size(cx);
                let search = self.state.searches.get(key).cloned();
                let list = self.state.neighbours.entry(key.clone()).or_default();
                list.status = PageStatus::Loading;
                list.page_size = page_size;
                let value = key.within.as_ref().map(|within| within.value.value());
                let narrow = Narrow {
                    search: search.as_deref(),
                    value: key
                        .within
                        .as_ref()
                        .zip(value.as_ref())
                        .map(|(within, value)| (&within.by, value)),
                };
                builder::neighbours(
                    &schema,
                    &key.node.type_name,
                    &key.group,
                    &key.node.id,
                    list.next_after(),
                    narrow,
                    page_size,
                )
            }
            LoadTarget::Values(key, by) => {
                self.state
                    .values
                    .insert((key.clone(), by.clone()), Loadable::Loading);
                let values = builder::neighbour_values(
                    &schema,
                    &key.node.type_name,
                    &key.group,
                    &key.node.id,
                    by,
                    VALUES_LIMIT,
                );
                // By a relationship, the neighbours with none are counted by a second read.
                if by.on == GroupOn::Relationship {
                    let without = builder::neighbours_without(
                        &schema,
                        &key.node.type_name,
                        &key.group,
                        &key.node.id,
                        by,
                    );
                    match values.and_then(|values| Ok((values, without?))) {
                        Ok((values, without)) => {
                            self.spawn_read_with(target, values, Some(without), cx)
                        }
                        Err(error) => self.apply_failure(&target, error.to_string()),
                    }
                    return;
                }
                values
            }
        };
        match query {
            Ok(query) => self.spawn_read(target, query, cx),
            Err(error) => self.apply_failure(&target, error.to_string()),
        }
    }

    fn spawn_read(&mut self, target: LoadTarget, query: BuiltQuery, cx: &mut Context<Self>) {
        self.spawn_read_with(target, query, None, cx);
    }

    /// Reads `query`, and then `without` at the same commit: the count of a relationship
    /// grouping's neighbours with none, added to the values as their "no value" bucket.
    fn spawn_read_with(
        &mut self,
        target: LoadTarget,
        query: BuiltQuery,
        without: Option<BuiltQuery>,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.pin.clone();
        let generation = self.generation;
        let scope = &self.scope;
        let read = self.session.update(cx, |session, cx| {
            session.read_in(scope, &query, snapshot.clone(), cx)
        });
        if let Some(without) = &without {
            // Its answer goes into what is shown too.
            let depends = without.depends.clone();
            self.session
                .update(cx, |session, _| session.note_in(scope, depends, None));
        }
        // Pinned, both are read at once at the pin; otherwise the first answer's commit is
        // where the second is read.
        let none_at_once = snapshot
            .as_ref()
            .zip(without.as_ref())
            .map(|(commit, without)| {
                let commit = Some(commit.clone());
                self.session
                    .update(cx, |session, cx| session.read(without, commit, cx))
            });
        let key = target.clone();
        let session = self.session.downgrade();
        let task = cx.spawn(async move |this, cx| {
            let mut result = read.await;
            if let (Some(without), Ok(output)) = (without, &result) {
                let read = match none_at_once {
                    Some(read) => read,
                    None => {
                        let commit = snapshot.or_else(|| output.graph_commit_id.clone());
                        let Ok(read) =
                            session.update(cx, |session, cx| session.read(&without, commit, cx))
                        else {
                            return;
                        };
                        read
                    }
                };
                result = match (result, read.await) {
                    (Ok(mut output), Ok(none)) => {
                        let n = rows(&none)
                            .first()
                            .and_then(|row| row.get("n"))
                            .cloned()
                            .unwrap_or(Value::from(0));
                        if let Value::Array(rows) = std::sync::Arc::make_mut(&mut output.rows) {
                            rows.push(serde_json::json!({ "n": n }));
                        }
                        Ok(output)
                    }
                    (_, Err(error)) | (Err(error), _) => Err(error),
                };
            }
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                let unpinned = this.pin.is_none();
                this.apply(&key, result);
                this.loaded(&key, cx);
                // The first answer pins the exploration: a head seen move while it was out is
                // compared now, which it could not be before.
                if unpinned && this.pin.is_some() {
                    this.compare_head(cx);
                }
            })
            .ok();
        });
        self.tasks.insert(target, task);
    }

    /// Waits for a count the session reads together with the others asked for this turn.
    fn spawn_count(
        &mut self,
        target: LoadTarget,
        count: Task<Result<u64, String>>,
        cx: &mut Context<Self>,
    ) {
        let generation = self.generation;
        let key = target.clone();
        let task = cx.spawn(async move |this, cx| {
            let count = count.await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match (&key, count) {
                    (LoadTarget::Count(group), Ok(count)) => {
                        this.state.apply_count(group.clone(), count)
                    }
                    (_, Ok(_)) => {}
                    (_, Err(message)) => this.apply_failure(&key, message),
                }
                this.loaded(&key, cx);
            })
            .ok();
        });
        self.tasks.insert(target, task);
    }

    /// After an answer: reads what it made visible, and redraws when it shows.
    fn loaded(&mut self, key: &LoadTarget, cx: &mut Context<Self>) {
        if !std::mem::replace(&mut self.missing_due, true) {
            let this = cx.entity().downgrade();
            cx.defer(move |cx| {
                this.update(cx, |this, cx| {
                    this.missing_due = false;
                    this.request_missing(cx);
                })
                .ok();
            });
        }
        // An answer read ahead for something closed shows nothing yet: nothing to draw.
        let shows = self
            .schema(cx)
            .is_some_and(|schema| self.state.shows(&schema, key));
        if shows {
            cx.emit(ExplorationEvent::Loaded);
            cx.notify();
        }
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
            LoadTarget::Count(key) => {
                let count = rows(&output)
                    .first()
                    .and_then(|row| row.get("n"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                self.state.apply_count(key.clone(), count)
            }
            LoadTarget::Values(key, by) => {
                self.state
                    .apply_values(key.clone(), by.clone(), &output, VALUES_LIMIT)
            }
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
            LoadTarget::Values(key, by) => {
                self.state
                    .values
                    .insert((key.clone(), by.clone()), Loadable::Failed(message));
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
            rows: rows.into(),
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
            .iter()
            .find(|g| g.edge == edge && g.direction == direction)
            .cloned()
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
                    EntryKind::Bucket { key, count, .. } => format!(
                        "value {} {count}",
                        key.within
                            .as_ref()
                            .map_or("-", |within| within.value.0.as_str())
                    ),
                    EntryKind::OtherValues { shown, rest } => format!("other {shown} {rest:?}"),
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
        assert!(missing.contains(&LoadTarget::Page(GroupKey::new(
            person("ada"),
            group(&schema, "Person", "Manager", Direction::Outgoing),
        ))));
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
        let key =
            |node: &NodeRef, group: &RelationshipGroup| GroupKey::new(node.clone(), group.clone());
        state.apply_count(key(&ada, &works), 0);
        state.apply_count(key(&ada, &knows_out), 2);
        state.apply_count(key(&ada, &knows_in), 1);
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
        let key = GroupKey::new(person("ada"), knows.clone());
        state.apply_count(key.clone(), 3);
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
    fn neighbours_grouped_by_their_own_relationship_carry_labels_and_skip_an_empty_none() {
        let schema = schema();
        let mut state = loaded_root(&schema);
        let knows = group(&schema, "Person", "Knows", Direction::Outgoing);
        let key = GroupKey::new(person("ada"), knows.clone());
        let by = GroupBy {
            on: GroupOn::Relationship,
            property: "out:WorksAt".into(),
        };
        state.apply_count(key.clone(), 4);
        state.set_grouping(&knows, Some(by.clone()));
        state.expand(&ExplorePath::root().child_group(&knows));
        // The grouped read, then the count of neighbours with no company, added last.
        state.apply_values(
            key.clone(),
            by.clone(),
            &output(
                &["value", "label", "n"],
                json!([{"value": "acme", "label": "Acme", "n": 3}, {"n": 1}]),
            ),
            2,
        );
        let acme = BucketValue::new(&json!("acme"));
        assert_eq!(
            state.bucket_label(&key.with_value(&by, &acme)),
            Some("Acme")
        );
        let none = BucketValue::new(&Value::Null);
        assert_eq!(state.bucket_label(&key.with_value(&by, &none)), None);
        let shown = kinds(&state.entries(&schema));
        assert!(shown.contains(&"    value null 1".to_owned()), "{shown:?}");
        assert!(
            !shown.iter().any(|row| row.starts_with("    other")),
            "one value under a limit of two is all of them: {shown:?}"
        );

        state.apply_values(
            key.clone(),
            by.clone(),
            &output(
                &["value", "label", "n"],
                json!([{"value": "acme", "label": "Acme", "n": 4}, {"n": 0}]),
            ),
            2,
        );
        let shown = kinds(&state.entries(&schema));
        assert!(
            !shown.iter().any(|row| row.contains("value null")),
            "no bucket for no neighbours: {shown:?}"
        );
    }

    #[test]
    fn a_grouped_relationship_lists_its_values_and_each_opens_to_its_neighbours() {
        let schema = schema();
        let mut state = loaded_root(&schema);
        let works_at = group(&schema, "Person", "WorksAt", Direction::Outgoing);
        let key = GroupKey::new(person("ada"), works_at.clone());
        let by = GroupBy {
            on: gq_language::builder::GroupOn::Edge,
            property: "role".into(),
        };
        let group_path = ExplorePath::root().child_group(&works_at);
        state.apply_count(key.clone(), 7);
        state.set_grouping(&works_at, Some(by.clone()));
        state.expand(&group_path);
        let missing = state.missing_loads(&schema);
        assert!(
            missing.contains(&LoadTarget::Values(key.clone(), by.clone())),
            "{missing:?}"
        );
        assert!(
            !missing.contains(&LoadTarget::Page(key.clone())),
            "a grouped relationship reads its values, not its neighbours"
        );

        // Two values came back with a limit of two: there may be more.
        state.apply_values(
            key.clone(),
            by.clone(),
            &output(
                &["value", "n"],
                json!([{"n": 3}, {"value": "lead", "n": 3}]),
            ),
            2,
        );
        let shown = kinds(&state.entries(&schema));
        let at = shown
            .iter()
            .position(|row| row == "  group WorksAt Outgoing Some(7)")
            .unwrap();
        assert_eq!(
            shown[at + 1..at + 4],
            [
                "    value \"lead\" 3",
                "    value null 3",
                "    other 2 Some(1)"
            ],
            "neighbours without a value come last"
        );

        // A value opens to the neighbours with that value, read on their own.
        let lead = BucketValue::new(&json!("lead"));
        let lead_path = group_path.child_value(&lead);
        let lead_key = key.with_value(&by, &lead);
        state.expand(&lead_path);
        assert!(
            state
                .missing_loads(&schema)
                .contains(&LoadTarget::Page(lead_key.clone()))
        );
        state.apply_page(
            lead_key.clone(),
            &output(&["id"], json!([{"id": "acme"}, {"id": "initech"}])),
            2,
        );
        let shown = kinds(&state.entries(&schema));
        assert!(shown.contains(&"      node acme".to_owned()), "{shown:#?}");
        assert!(
            shown.contains(&"      more 2/Some(3)".to_owned()),
            "a value pages with its own count: {shown:#?}"
        );
        let acme = NodeRef::new("Company", "acme");
        assert_eq!(
            state.path_of(&schema, &acme),
            Some(lead_path.child_node(&acme))
        );

        // Ending the grouping closes the values and leaves the selection on the group.
        state.selection = Some(lead_path.child_node(&acme));
        state.set_grouping(&works_at, None);
        assert!(!state.is_expanded(&lead_path));
        assert_eq!(state.selection, Some(group_path.clone()));
        assert!(
            state
                .missing_loads(&schema)
                .contains(&LoadTarget::Page(key.clone()))
        );
        assert!(
            !kinds(&state.entries(&schema))
                .iter()
                .any(|row| row.contains("value"))
        );
    }

    #[test]
    fn prefetching_reads_what_opening_would_show_and_nothing_twice() {
        let schema = schema();
        let mut state = loaded_root(&schema);
        let knows = group(&schema, "Person", "Knows", Direction::Outgoing);
        let manager = group(&schema, "Person", "Manager", Direction::Outgoing);
        let bob = ExplorePath::root()
            .child_group(&knows)
            .child_node(&person("bob"));
        // A node: its relationships' counts, and its single-target field's page.
        let targets = state.prefetch_targets(&schema, &bob);
        assert!(targets.contains(&LoadTarget::Count(GroupKey::new(
            person("bob"),
            knows.clone()
        ))));
        assert!(targets.contains(&LoadTarget::Page(GroupKey::new(person("bob"), manager))));
        assert!(
            !targets
                .iter()
                .any(|t| matches!(t, LoadTarget::Page(key) if key.group == knows)),
            "a relationship's neighbours wait for that relationship"
        );
        // A relationship: its first page, unless read or known to be empty.
        let group_path = bob.child_group(&knows);
        let key = GroupKey::new(person("bob"), knows.clone());
        assert_eq!(
            state.prefetch_targets(&schema, &group_path),
            [LoadTarget::Page(key.clone())]
        );
        state.apply_count(key.clone(), 0);
        assert!(state.prefetch_targets(&schema, &group_path).is_empty());
        // Grouped, its values instead.
        let mut state = loaded_root(&schema);
        let works_at = group(&schema, "Person", "WorksAt", Direction::Outgoing);
        let by = GroupBy {
            on: gq_language::builder::GroupOn::Edge,
            property: "role".into(),
        };
        state.set_grouping(&works_at, Some(by.clone()));
        let path = ExplorePath::root().child_group(&works_at);
        let key = GroupKey::new(person("ada"), works_at);
        assert_eq!(
            state.prefetch_targets(&schema, &path),
            [LoadTarget::Values(key.clone(), by.clone())]
        );
        state.apply_values(key, by, &output(&["value", "n"], json!([])), 100);
        assert!(state.prefetch_targets(&schema, &path).is_empty());
    }

    #[test]
    fn only_answers_for_what_is_open_change_what_is_shown() {
        let schema = schema();
        let mut state = loaded_root(&schema);
        let knows = group(&schema, "Person", "Knows", Direction::Outgoing);
        let manager = group(&schema, "Person", "Manager", Direction::Outgoing);
        let bob = person("bob");
        let bob_knows = GroupKey::new(bob.clone(), knows.clone());
        // Bob is a neighbour of the open root, not open himself.
        assert!(state.shows(&schema, &LoadTarget::Root));
        assert!(state.shows(
            &schema,
            &LoadTarget::Count(GroupKey::new(person("ada"), knows.clone()))
        ));
        assert!(!state.shows(&schema, &LoadTarget::Count(bob_knows.clone())));
        assert!(!state.shows(&schema, &LoadTarget::Page(bob_knows.clone())));
        let bob_path = ExplorePath::root().child_group(&knows).child_node(&bob);
        state.expand(&bob_path);
        assert!(state.shows(&schema, &LoadTarget::Count(bob_knows.clone())));
        assert!(
            state.shows(
                &schema,
                &LoadTarget::Page(GroupKey::new(bob.clone(), manager))
            ),
            "a single-target field shows on its open node"
        );
        assert!(!state.shows(&schema, &LoadTarget::Page(bob_knows.clone())));
        state.expand(&bob_path.child_group(&knows));
        assert!(state.shows(&schema, &LoadTarget::Page(bob_knows)));
    }

    #[test]
    fn edge_properties_skip_nulls_and_keep_order() {
        let schema = schema();
        let mut state = loaded_root(&schema);
        let works = group(&schema, "Person", "WorksAt", Direction::Outgoing);
        let key = GroupKey::new(person("ada"), works);
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

    /// A root not found at the exploration's commit is not found there again: Retry reads it
    /// at the branch head, where another writer may have made it since.
    #[gpui::test]
    async fn a_missing_root_is_retried_at_the_head(cx: &mut gpui::TestAppContext) {
        use std::sync::atomic::{AtomicBool, Ordering};

        use crate::test_server::followed_session;
        let created = Arc::new(AtomicBool::new(false));
        let exists = created.clone();
        let (session, served) = followed_session(
            "node Person {\n  slug: String @key\n  name: String\n}\n",
            move |asked| {
                // Made at c2: a read pinned to c1 still finds nothing.
                if exists.load(Ordering::SeqCst) && asked["snapshot"].is_null() {
                    json!([{ "id": "p1", "label": "Ada" }])
                } else {
                    json!([])
                }
            },
            cx,
        );
        cx.run_until_parked();
        let exploration = cx.update(|cx| Exploration::for_session(&session, cx));
        exploration.update(cx, |exploration, cx| exploration.root_at(person("p1"), cx));
        cx.run_until_parked();
        let status = |cx: &mut gpui::TestAppContext| {
            exploration.read_with(cx, |exploration, _| exploration.state().root_status.clone())
        };
        assert!(matches!(status(cx), Loadable::Failed(_)), "not there yet");
        created.store(true, Ordering::SeqCst);
        served.lock().unwrap().head = "c2".into();
        exploration.update(cx, |exploration, cx| {
            exploration.retry(&LoadTarget::Root, cx)
        });
        cx.run_until_parked();
        assert_eq!(status(cx), Loadable::Loaded(()));
    }

    /// Paths opened or closed together are one change: views draw them once, not once a path.
    #[gpui::test]
    async fn paths_opened_together_are_one_change(cx: &mut gpui::TestAppContext) {
        use crate::test_server::followed_session;
        let (session, _served) = followed_session(
            "node Person {\n  slug: String @key\n  name: String\n}\n",
            |_| json!([]),
            cx,
        );
        cx.run_until_parked();
        let exploration = cx.update(|cx| Exploration::for_session(&session, cx));
        let changes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = changes.clone();
        let _counting = cx.update(|cx| {
            cx.subscribe(&exploration, move |_, event: &ExplorationEvent, _| {
                if matches!(event, ExplorationEvent::Changed) {
                    counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            })
        });
        let paths = vec![ExplorePath::root(); 3];
        for open in [true, false] {
            exploration.update(cx, |exploration, cx| exploration.set_open(&paths, open, cx));
        }
        cx.run_until_parked();
        assert_eq!(changes.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// Another writer's commit that changed nothing the exploration shows moves its pin to the
    /// head without reading again; one that changed a shown node offers Refresh (spec 015).
    #[gpui::test]
    async fn an_exploration_moves_its_pin_past_commits_to_other_types(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::test_server::{followed_session, node_change};
        const PEOPLE_AND_NOTES: &str = "node Person {\n  slug: String @key\n  name: String\n}\nnode Note {\n  slug: String @key\n}\n";
        let (session, served) = followed_session(
            PEOPLE_AND_NOTES,
            |_| json!([{ "id": "p1", "label": "Ada", "n": 0 }]),
            cx,
        );
        cx.run_until_parked();
        let exploration = cx.update(|cx| Exploration::for_session(&session, cx));
        exploration.update(cx, |exploration, cx| exploration.root_at(person("p1"), cx));
        cx.run_until_parked();
        let pin = |cx: &mut gpui::TestAppContext| {
            exploration.read_with(cx, |exploration, _| {
                (
                    exploration.pin().map(str::to_owned),
                    exploration.newer_available(),
                )
            })
        };
        assert_eq!(pin(cx), (Some("c1".into()), false));
        let reads_before = served.lock().unwrap().feeds.len();

        served
            .lock()
            .unwrap()
            .commit("c2", vec![node_change("Note", "n1")]);
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        assert_eq!(pin(cx), (Some("c2".into()), false), "moved to the head");
        assert_eq!(served.lock().unwrap().feeds.len(), reads_before + 1);

        // Another person: the tree shows only p1.
        served
            .lock()
            .unwrap()
            .commit("c3", vec![node_change("Person", "p9")]);
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        assert_eq!(pin(cx), (Some("c3".into()), false), "another person");

        served
            .lock()
            .unwrap()
            .commit("c4", vec![node_change("Person", "p1")]);
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        assert_eq!(pin(cx), (Some("c3".into()), true), "the root changed");
    }
}

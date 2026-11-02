//! The exploration the relationship tree (spec 005) and the graph canvas (spec 006) share: one
//! root node, the paths a person expanded from it, and the group counts and neighbour pages
//! those paths show.
//!
//! Every read is pinned to the commit the first read returned (constitution II). When the
//! session sees its branch's head at another commit ([`SessionEvent::HeadMoved`]: Studio's own
//! commit, or another writer's), the pin moves to the head if nothing the exploration shows
//! changed in between (spec 015); otherwise [`Exploration::newer_available`] is raised and
//! nothing changes until [`Exploration::refresh`]. Counts come from the session's batched counts
//! ([`GraphSession::degree_in`]): every count asked for in one turn (several nodes expanded,
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
use gq_language::builder::{Filter, GroupBy, GroupOn, Narrow, PageStart};
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

/// The nodes a tree shows as its rows when it holds a set rather than one node (spec 020). Their
/// paths start with a node step under an anchor that is never drawn.
#[derive(Debug, Clone, PartialEq)]
pub enum SetRoots {
    /// These nodes, as they were listed.
    Listed(Vec<Neighbour>),
    /// A type's rows under these filters, read a page at a time.
    Filtered {
        type_name: String,
        filters: Vec<Filter>,
        total: Option<u64>,
    },
}

/// What a set's paths hang from: no node of any graph, since a type has a name.
fn set_anchor() -> NodeRef {
    NodeRef::new("", "")
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

    /// What a set's pages are kept under, as a relationship group's are.
    fn of_set(type_name: &str) -> Self {
        Self::new(
            set_anchor(),
            RelationshipGroup {
                edge: String::new(),
                direction: Direction::Outgoing,
                other_type: type_name.to_owned(),
            },
        )
    }

    /// Whether these are a set's pages, not a relationship's.
    pub fn is_set(&self) -> bool {
        self.node == set_anchor()
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

/// What the commit an answer names does to it ([`Exploration::pinned_at`]).
enum Pinned {
    /// At the pin, or the first answer, which pins the exploration: it is taken.
    Taken,
    /// Read at the head before the pin and answered elsewhere: it is read again at the pin.
    Again,
    /// Read at the pin and answered at another commit: a failure, which says so.
    Elsewhere(String),
}

/// Something the exploration reads.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LoadTarget {
    Root,
    Count(GroupKey),
    Page(GroupKey),
    Values(GroupKey, GroupBy),
}

impl LoadTarget {
    /// The relationship the read is of, for every read but the root's.
    pub fn group(&self) -> Option<&RelationshipGroup> {
        match self {
            LoadTarget::Root => None,
            LoadTarget::Count(key) | LoadTarget::Page(key) | LoadTarget::Values(key, _) => {
                Some(&key.group)
            }
        }
    }
}

/// How many values of a grouping property one read returns, most common first.
pub const VALUES_LIMIT: u32 = 100;

/// The most reads one row the pointer rests on reads ahead ([`Exploration::prefetch`]): the
/// first that opening it needs, in the order its rows show. A Person on the personal graph has
/// 92 relationships, each a count; the rest are read when the row opens. As many as a
/// connection keeps on the wire, so a read ahead never waits behind itself.
pub const PREFETCH_LIMIT: usize = 8;

/// How many counts, neighbour lists, values and labels an exploration holds before it lets go
/// of those nothing open can reach: rows the pointer rested on and never opened, and what was
/// read below a subtree closed whole. What is let go is read again at the pin when it opens,
/// mostly from the session's answers kept.
const KEPT_READS: usize = 4_096;

/// How many commits a pin carried past are remembered: reads out across that many head moves
/// that changed nothing they read still stand when they answer.
const CARRIED_PINS: usize = 16;

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
    /// Labels read so far. `None` means the type has no label besides its key.
    pub labels: HashMap<NodeRef, Option<String>>,
    pub expanded: HashSet<ExplorePath>,
    pub counts: HashMap<GroupKey, Loadable<u64>>,
    pub neighbours: HashMap<GroupKey, NeighbourList>,
    /// Groups whose neighbours are narrowed on the server to ids or labels matching a text.
    pub searches: HashMap<GroupKey, String>,
    /// The property each relationship's neighbours are grouped by, wherever it appears.
    pub grouping: HashMap<RelationshipGroup, GroupBy>,
    /// The filters each relationship's neighbours are read under, wherever it appears (spec
    /// 005, "Filtering a relationship's neighbours"): its counts, pages and values all narrow.
    pub filters: HashMap<RelationshipGroup, Vec<Filter>>,
    pub values: HashMap<(GroupKey, GroupBy), Loadable<Buckets>>,
    pub selection: Option<ExplorePath>,
    /// The rows of a tree holding a set; none for a walk from one node.
    pub set: Option<SetRoots>,
}

impl Default for ExploreState {
    fn default() -> Self {
        Self {
            root: None,
            root_status: Loadable::NotLoaded,
            labels: HashMap::new(),
            expanded: HashSet::new(),
            counts: HashMap::new(),
            neighbours: HashMap::new(),
            searches: HashMap::new(),
            grouping: HashMap::new(),
            filters: HashMap::new(),
            values: HashMap::new(),
            selection: None,
            set: None,
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

    /// Lets go of a set's count, which was the rows' number at the commit it was read at: read
    /// again at another (the branch's head moved, or another branch), the rows may number
    /// otherwise, and "Show more" goes by whether pages remain.
    fn forget_set_total(&mut self) {
        if let Some(SetRoots::Filtered { total, .. }) = &mut self.set {
            *total = None;
        }
    }

    /// A state holding `set`, nothing open and nothing selected.
    pub fn of_set(set: SetRoots) -> Self {
        let mut state = Self {
            root: Some(set_anchor()),
            root_status: Loadable::Loaded(()),
            set: Some(set),
            ..Self::default()
        };
        state.seed_listed_labels();
        state
    }

    /// The labels listed nodes came with, which no read brings back.
    fn seed_listed_labels(&mut self) {
        if let Some(SetRoots::Listed(rows)) = &self.set {
            for row in rows {
                self.labels.insert(row.node.clone(), row.label.clone());
            }
        }
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

    /// Whether `path` is the path of `key`'s group, or of its value: [`Self::group_key`] without
    /// making the key.
    fn is_group_path(&self, root: &NodeRef, path: &ExplorePath, key: &GroupKey) -> bool {
        let Some((group, value)) = path.group_step() else {
            return false;
        };
        if *group != key.group || *path.node(root) != key.node {
            return false;
        }
        match (value, &key.within) {
            (None, None) => true,
            (Some(value), Some(within)) => {
                within.value == *value && self.grouping.get(group) == Some(&within.by)
            }
            _ => false,
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

    /// Filters a relationship's neighbours wherever it appears, or stops: what was read of it
    /// (counts, pages, values) goes, to be read again under the filters; paths stay open.
    pub fn set_filters(&mut self, group: &RelationshipGroup, filters: Vec<Filter>) {
        if filters.is_empty() {
            self.filters.remove(group);
        } else {
            self.filters.insert(group.clone(), filters);
        }
        self.counts.retain(|key, _| key.group != *group);
        self.neighbours.retain(|key, _| key.group != *group);
        self.values.retain(|(key, _), _| key.group != *group);
    }

    /// The filters a relationship's neighbours are read under.
    pub fn filters_of(&self, group: &RelationshipGroup) -> &[Filter] {
        self.filters.get(group).map_or(&[], Vec::as_slice)
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

    /// Forgets everything read, keeping the root, expansions and selection.
    pub fn clear_loaded(&mut self) {
        // A set's rows are what is read again, not a root.
        self.root_status = if self.set.is_some() {
            Loadable::Loaded(())
        } else {
            Loadable::NotLoaded
        };
        self.labels.clear();
        self.counts.clear();
        self.neighbours.clear();
        self.values.clear();
        self.seed_listed_labels();
    }

    /// How many counts, neighbour lists, values and labels are held.
    fn held(&self) -> usize {
        self.counts.len() + self.neighbours.len() + self.values.len() + self.labels.len()
    }

    /// Lets go of what nothing open can reach, which no flattening shows until it opens: what
    /// was read ahead for rows that never opened, and what was read below a subtree closed
    /// whole. Kept: whatever is still being read; each open node's counts and fields; each
    /// open relationship's or value's neighbours, values and counts; the labels of the root,
    /// the nodes on open paths and the neighbours kept.
    pub fn retain_reachable(&mut self) {
        let Some(root) = self.root.clone() else {
            return;
        };
        let mut nodes: HashSet<NodeRef> = HashSet::new();
        let mut groups: HashSet<GroupKey> = HashSet::new();
        nodes.insert(root.clone());
        for path in &self.expanded {
            nodes.extend(path.nodes().cloned());
            if path.ends_at_node() {
                nodes.insert(path.node(&root).clone());
            } else if let Some(key) = self.group_key(path) {
                groups.insert(key);
            }
        }
        let base = |key: &GroupKey| GroupKey {
            within: None,
            ..key.clone()
        };
        self.counts.retain(|key, count| {
            count.is_loading()
                || (key.within.is_none() && nodes.contains(&key.node))
                || groups.contains(&base(key))
        });
        self.neighbours.retain(|key, list| {
            list.status == PageStatus::Loading
                || groups.contains(key)
                || (key.within.is_none() && nodes.contains(&key.node))
        });
        self.values
            .retain(|(key, _), values| values.is_loading() || groups.contains(key));
        let listed: HashSet<&NodeRef> = self
            .neighbours
            .values()
            .flat_map(|list| list.rows.iter().map(|row| &row.node))
            .collect();
        let roots: HashSet<&NodeRef> = match &self.set {
            Some(SetRoots::Listed(rows)) => rows.iter().map(|row| &row.node).collect(),
            _ => HashSet::new(),
        };
        self.labels.retain(|node, _| {
            nodes.contains(node) || listed.contains(node) || roots.contains(node)
        });
    }

    /// The exploration flattened in tree order.
    pub fn entries(&self, schema: &SchemaModel) -> Vec<Entry> {
        let mut out = Vec::new();
        if let Some(set) = &self.set {
            self.push_set(schema, set, &mut out);
            return out;
        }
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

    /// A set's rows, each a node at the top of the tree, then what its pages leave to say.
    fn push_set(&self, schema: &SchemaModel, set: &SetRoots, out: &mut Vec<Entry>) {
        let (key, total) = match set {
            SetRoots::Listed(_) => (None, None),
            SetRoots::Filtered {
                type_name, total, ..
            } => (Some(GroupKey::of_set(type_name)), *total),
        };
        let list = key.as_ref().and_then(|key| self.neighbours.get(key));
        let rows: &[Neighbour] = match (set, list) {
            (SetRoots::Listed(rows), _) => rows,
            (_, Some(list)) => &list.rows,
            (_, None) => &[],
        };
        for row in rows {
            let mut ancestors = Vec::new();
            self.push_node(
                schema,
                &row.node,
                None,
                ExplorePath::root().child_node(&row.node),
                0,
                None,
                &mut ancestors,
                out,
            );
        }
        let Some(key) = key else {
            return;
        };
        let tail = match list.map(|list| &list.status) {
            None | Some(PageStatus::Loading) => Some(EntryKind::Loading {
                target: LoadTarget::Page(key.clone()),
            }),
            Some(PageStatus::Failed(message)) => Some(EntryKind::Error {
                target: LoadTarget::Page(key.clone()),
                message: message.clone(),
            }),
            Some(PageStatus::Idle) => {
                list.filter(|list| list.has_more(total))
                    .map(|list| EntryKind::More {
                        shown: list.rows.len(),
                        total,
                        key: key.clone(),
                    })
            }
        };
        if let Some(kind) = tail {
            out.push(Entry {
                path: ExplorePath::root(),
                depth: 0,
                parent: None,
                kind,
            });
        }
    }

    /// Takes a page of a set: its rows come as nodes, named as the schema names them.
    pub fn apply_set_page(
        &mut self,
        key: GroupKey,
        output: &ReadOutput,
        page_size: u32,
        schema: Option<&SchemaModel>,
    ) {
        let type_name = key.group.other_type.clone();
        let label = schema
            .and_then(|schema| schema.node(&type_name))
            .and_then(|node| node.label_property())
            .map(str::to_owned);
        let page: Vec<Neighbour> = rows(output)
            .iter()
            .filter_map(|row| {
                let node = row.get("n")?.as_object()?;
                let id = node.get("@id")?.as_str()?.to_owned();
                Some(Neighbour {
                    node: NodeRef::new(type_name.clone(), id),
                    label: label
                        .as_deref()
                        .and_then(|property| node.get(property))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    edge: Vec::new(),
                })
            })
            .collect();
        for row in &page {
            self.labels.insert(row.node.clone(), row.label.clone());
        }
        let list = self.neighbours.entry(key).or_default();
        list.exhausted = (page.len() as u32) < page_size;
        let mut shown: HashSet<NodeRef> = list.rows.iter().map(|row| row.node.clone()).collect();
        for row in page {
            if shown.insert(row.node.clone()) {
                list.rows.push(row);
            }
        }
        list.status = PageStatus::Idle;
    }

    #[expect(clippy::too_many_arguments)]
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
            // Opened before its count is in (Expand All opens every relationship of a node):
            // its first page, or its values, wait for the count, so a relationship with none
            // reads nothing more. What was read ahead for it is shown.
            let held = match self.grouping.get(group) {
                Some(by) => self.values.contains_key(&(key.clone(), by.clone())),
                None => self.neighbours.contains_key(&key),
            };
            if matches!(count, Loadable::NotLoaded | Loadable::Loading) && !held {
                out.push(Entry {
                    path: group_path,
                    depth: depth + 2,
                    parent: Some(group_index),
                    kind: EntryKind::Loading {
                        target: LoadTarget::Count(key),
                    },
                });
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
    #[expect(clippy::too_many_arguments)]
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
        // How many neighbours the values shown leave out. A neighbour holds one value of a
        // property of its own. Grouped by a relationship of theirs ("by company"), one falls in
        // as many values as it has ends, so what is left is not known. By an edge property, one
        // reached by parallel edges falls in two: the count holds while the values' counts stay
        // under the group's, and is not known once they reach it.
        let shown: u64 = buckets.values.iter().map(|(_, n)| n).sum();
        let total = count.loaded().copied();
        let rest = match by.on {
            GroupOn::Node => total.map(|total| total.saturating_sub(shown)),
            GroupOn::Edge => total
                .filter(|total| shown < *total)
                .map(|total| total - shown),
            GroupOn::Relationship => None,
        };
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
    #[expect(clippy::too_many_arguments)]
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
                .any(|path| self.is_group_path(root, path, key))
        };
        match target {
            LoadTarget::Root => true,
            // A set's rows are what the tree shows.
            LoadTarget::Page(key) if key.is_set() => true,
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
        self.entries(schema)
            .iter()
            .filter(|entry| matches!(&entry.kind, EntryKind::Node { node: n, .. } if n == node))
            .min_by_key(|entry| entry.depth)
            .map(|entry| entry.path.clone())
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
                            let property = column.strip_prefix("e_")?;
                            (!value.is_null()).then(|| (property.to_owned(), value.clone()))
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

/// What makes a walk the walk it is (spec 020): its session, its root, and the commit it is held
/// at, or none for one that reads the branch head.
type WalkKey = (EntityId, NodeRef, Option<String>);

#[derive(Default)]
struct ExplorationRegistry(HashMap<WalkKey, WeakEntity<Exploration>>);

impl Global for ExplorationRegistry {}

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

impl Flattened {
    /// The state, to take an answer that changes nothing the flattening shows (read ahead for
    /// a row that is closed): the flattening made stays, so taking it walks no tree.
    fn unseen(&mut self) -> &mut ExploreState {
        &mut self.state
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

/// One walk of a session's graph: from a root, at a commit or the branch head
/// ([`Exploration::for_walk`], shared by the tree and the canvas showing it), over a set of
/// nodes ([`Exploration::for_set`]), or with no root ([`Exploration::unrooted`]).
pub struct Exploration {
    session: Entity<GraphSession>,
    state: Flattened,
    /// The commit the walk is held at (spec 020); none for one that reads the branch head.
    held: Option<String>,
    pin: Option<String>,
    newer_available: bool,
    generation: u64,
    /// The schema what is shown was read under, which it is flattened and read under: the
    /// session's, until another writer moves it ([`SessionEvent::SchemaMoved`]); then this one
    /// stays, as the commit does, until Refresh. One that names a type's nodes otherwise (spec
    /// 002, "Node names") reads them again.
    schema: Option<Arc<SchemaModel>>,
    /// The reads out, each with the number it was started under: one that finishes lets go of
    /// its own entry, not of a read of the same target started since.
    tasks: HashMap<LoadTarget, (u64, Task<()>)>,
    next_task: u64,
    /// The commits the pin was carried past by head moves that changed nothing it read
    /// ([`Self::compare_head`]), the newest last: a read sent at one of them before the move
    /// answers there, and stands, since nothing it reads changed in between.
    carried: Vec<String>,
    /// What was read ahead for the row the pointer rests on ([`Self::prefetch`]): its path and
    /// the targets started for it. The pointer moving on stops those still out that nothing
    /// opened since.
    ahead: Option<(ExplorePath, Vec<LoadTarget>)>,
    /// How many counts, neighbour lists, values and labels the state may hold before those
    /// nothing open can reach are let go ([`ExploreState::retain_reachable`]): [`KEPT_READS`],
    /// or twice what the last letting go kept, and [`KEPT_READS`] again whenever the walk
    /// starts over (a new root, Refresh, a reading again).
    prune_at: usize,
    /// What the exploration has read at its pin: a head move that changed none of it moves the
    /// pin without reading again (spec 015).
    scope: ReadScope,
    /// The head moved and the session is reading what changed.
    awaiting_changes: bool,
    /// A look for what is missing is due at the end of this turn: many answers come in one
    /// turn (a node's counts), and each look walks the whole tree.
    missing_due: bool,
    /// A look for what is missing that the session's events asked for (another branch,
    /// schema or token), owed until a view that draws the exploration is drawn
    /// ([`Self::read_owed`]): a hidden tree, a results canvas or a background tab reads nothing
    /// for it.
    owed: bool,
    _subscription: Subscription,
}

impl EventEmitter<ExplorationEvent> for Exploration {}

impl Exploration {
    /// Neighbours per page for this graph (setting `studio.tree_page_size`, or the graph's own).
    pub fn page_size(&self, cx: &App) -> u32 {
        self.session.read(cx).options(cx).tree_page_size.max(1)
    }

    /// The walk of `session` from `root`, at `at_commit` or the branch head, created on first
    /// use: the tree and the canvas showing that walk share it.
    pub fn for_walk(
        session: &Entity<GraphSession>,
        root: NodeRef,
        at_commit: Option<String>,
        cx: &mut App,
    ) -> Entity<Exploration> {
        let key = (session.entity_id(), root.clone(), at_commit.clone());
        let existing = cx
            .default_global::<ExplorationRegistry>()
            .0
            .get(&key)
            .and_then(WeakEntity::upgrade);
        if let Some(existing) = existing {
            return existing;
        }
        let exploration = cx.new(|cx| {
            let mut exploration = Exploration::new(session.clone(), cx);
            exploration.held = at_commit;
            exploration.pin = exploration.held.clone();
            exploration.root_at(root, cx);
            exploration
        });
        let registry = cx.default_global::<ExplorationRegistry>();
        registry.0.retain(|_, weak| weak.upgrade().is_some());
        registry.0.insert(key, exploration.downgrade());
        exploration
    }

    /// A walk with no root, which reads nothing: a results canvas draws none of it.
    pub fn unrooted(session: &Entity<GraphSession>, cx: &mut App) -> Entity<Exploration> {
        cx.new(|cx| Exploration::new(session.clone(), cx))
    }

    /// A tree's walk over a set of nodes: its own, since no other tab shows this set. It reads its
    /// first page at once, and nothing below a row until the row opens.
    pub fn for_set(
        session: &Entity<GraphSession>,
        set: SetRoots,
        at_commit: Option<String>,
        cx: &mut App,
    ) -> Entity<Exploration> {
        cx.new(|cx| {
            let mut exploration = Exploration::new(session.clone(), cx);
            exploration.held = at_commit;
            exploration.pin = exploration.held.clone();
            *exploration.state = ExploreState::of_set(set);
            exploration.request_missing(cx);
            exploration
        })
    }

    /// A rootless walk of its own, for a test to root and read.
    #[cfg(test)]
    fn for_session(session: &Entity<GraphSession>, cx: &mut App) -> Entity<Exploration> {
        Self::unrooted(session, cx)
    }

    /// The commit this walk is held at: it reads there, read-only, whatever the branch does.
    pub fn held_at(&self) -> Option<&str> {
        self.held.as_deref()
    }

    fn new(session: Entity<GraphSession>, cx: &mut Context<Self>) -> Self {
        let scope = session.update(cx, |session, _| {
            let branch = session.branch.clone();
            session.read_scope(&branch)
        });
        let schema = session.read(cx).schema_model();
        let subscription = cx.subscribe(&session, |this, session, event, cx| match event {
            // A held walk stays at its commit whatever the branch does.
            SessionEvent::SchemaMoved
            | SessionEvent::BranchChanged
            | SessionEvent::HeadMoved { .. }
            | SessionEvent::ChangesRead { .. }
                if this.held.is_some() => {}
            SessionEvent::SchemaChanged => {
                let now = session.read(cx).schema_model();
                match (&this.schema, &now) {
                    // The same text naming some type's nodes otherwise: what is shown is read
                    // again under the new names, at the same commit.
                    (Some(was), Some(now)) if was.source == now.source && !was.same_as(now) => {
                        this.schema = Some(now.clone());
                        this.read_again(cx);
                    }
                    // Another text: what was read under the old one is read again under it,
                    // at the head, as Refresh reads it.
                    (Some(was), Some(now)) if was.source != now.source => {
                        this.forget_at_head(cx);
                        this.owed = true;
                    }
                    _ => {
                        this.schema = now;
                        this.owe(cx);
                    }
                }
            }
            // Another writer changed the schema: what is shown stays, under the schema it was
            // read under, until Refresh. Nothing read at a commit yet is read under the new.
            SessionEvent::SchemaMoved if this.pin.is_none() => {
                this.forget_at_head(cx);
                this.owed = true;
            }
            SessionEvent::SchemaMoved if !this.newer_available => {
                this.newer_available = true;
                this.changed(cx);
            }
            SessionEvent::BranchChanged => {
                if let Some(root) = this.state.root.clone() {
                    this.start_over_at(root, cx);
                    this.owed = true;
                }
            }
            // A new token may see less than the old one did: nothing it read stays shown, and
            // what is shown is read again under the new one, at the same commit.
            SessionEvent::CredentialChanged => this.read_again(cx),
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
            held: None,
            pin: None,
            newer_available: false,
            generation: 0,
            schema,
            tasks: HashMap::new(),
            next_task: 0,
            carried: Vec::new(),
            ahead: None,
            prune_at: KEPT_READS,
            scope,
            awaiting_changes: false,
            missing_due: false,
            owed: false,
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
            // Reads out at the old pin answer there: the same answer, since nothing they read
            // changed, so they stand (`pinned_at`).
            ScopeSince::Unchanged => {
                self.pin = Some(head.to_owned());
                self.carried.retain(|commit| *commit != pin);
                self.carried.push(pin);
                if self.carried.len() > CARRIED_PINS {
                    self.carried.remove(0);
                }
            }
            ScopeSince::Changed => {
                self.newer_available = true;
                self.changed(cx);
            }
        }
    }

    pub fn state(&self) -> &ExploreState {
        &self.state
    }

    /// The schema what is shown was read under, which may be older than the session's until
    /// Refresh ([`Self::newer_available`]).
    pub fn schema(&self) -> Option<Arc<SchemaModel>> {
        self.schema.clone()
    }

    pub fn root(&self) -> Option<&NodeRef> {
        self.state.root.as_ref()
    }

    /// The commit every read of this exploration is pinned to.
    #[cfg(test)]
    pub fn pin(&self) -> Option<&str> {
        self.pin.as_deref()
    }

    /// The branch, or the schema, moved since the pin; offer Refresh.
    pub fn newer_available(&self) -> bool {
        self.newer_available
    }

    /// The exploration flattened in tree order under the schema it was read under
    /// ([`Self::schema`]); empty until the schema loads. Made once per change to it, whoever
    /// asks.
    pub fn entries(&self) -> Arc<Vec<Entry>> {
        match &self.schema {
            Some(schema) => self.state.entries(schema),
            None => Arc::default(),
        }
    }

    /// Starts a new exploration at `node`, at the branch head.
    pub fn root_at(&mut self, node: NodeRef, cx: &mut Context<Self>) {
        self.start_over_at(node, cx);
        self.request_missing(cx);
    }

    /// [`Self::root_at`] without reading: what it reads is missing, and the views hear the
    /// change.
    fn start_over_at(&mut self, node: NodeRef, cx: &mut Context<Self>) {
        self.reset_reads(cx);
        // How each relationship's neighbours are grouped and filtered stays, wherever it
        // appears, as Refresh keeps it.
        let grouping = std::mem::take(&mut self.state.grouping);
        let filters = std::mem::take(&mut self.state.filters);
        // A set stays the set: its rows are read again, with nothing open.
        *self.state = match self.state.set.take() {
            Some(set) => ExploreState::of_set(set),
            None => ExploreState::rooted(node),
        };
        self.state.grouping = grouping;
        self.state.filters = filters;
        if self.held.is_none() {
            self.state.forget_set_total();
        }
        self.changed(cx);
        cx.emit(ExplorationEvent::SelectionChanged);
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
        if let Some(schema) = self.schema() {
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

    /// The filters a relationship's neighbours are read under, wherever it appears.
    pub fn filters(&self, group: &RelationshipGroup) -> &[Filter] {
        self.state.filters_of(group)
    }

    /// Filters a relationship's neighbours wherever it appears in this exploration, or stops
    /// (spec 005, "Filtering a relationship's neighbours"): its counts, pages and values are
    /// read again under the filters, reads out for them dropped.
    pub fn set_filters(
        &mut self,
        group: &RelationshipGroup,
        filters: Vec<Filter>,
        cx: &mut Context<Self>,
    ) {
        if self.state.filters_of(group) == filters.as_slice() {
            return;
        }
        self.state.set_filters(group, filters);
        self.tasks
            .retain(|target, _| target.group().is_none_or(|g| g != group));
        self.request_missing(cx);
        self.changed(cx);
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
        // The old grouping's reads go, as a filter's do: its values and the pages and counts
        // of its values. The relationship's own count and page still hold.
        self.tasks.retain(|target, _| match target {
            LoadTarget::Values(key, _) => key.group != *group,
            LoadTarget::Count(key) | LoadTarget::Page(key) => {
                key.group != *group || key.within.is_none()
            }
            LoadTarget::Root => true,
        });
        // What those reads marked as being read goes with them, or the grouping switched back
        // to would wait on reads nobody makes. What came in stays, for a switch back: a value's
        // rows whose next page was being read stay, that page to be asked for again.
        let state = self.state.unseen();
        let ours = |key: &GroupKey| key.group == *group;
        state
            .values
            .retain(|(key, _), values| !ours(key) || !values.is_loading());
        state.counts.retain(|key, count| {
            !ours(key) || key.within.is_none() || !matches!(count, Loadable::Loading)
        });
        state.neighbours.retain(|key, list| {
            if !ours(key) || key.within.is_none() || list.status != PageStatus::Loading {
                return true;
            }
            list.status = PageStatus::Idle;
            !list.rows.is_empty()
        });
        self.request_missing(cx);
        self.changed(cx);
        cx.emit(ExplorationEvent::SelectionChanged);
    }

    pub fn selection(&self) -> Option<&ExplorePath> {
        self.state.selection.as_ref()
    }

    pub fn select(&mut self, path: Option<ExplorePath>, cx: &mut Context<Self>) {
        if self.state.selection != path {
            // The rows do not show the selection: the flattening stays.
            self.state.unseen().selection = path;
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
        // The old search's page, if still out, would land under the new one.
        let target = LoadTarget::Page(key.clone());
        self.tasks.remove(&target);
        // A group that is not open reads its first page when it opens, as missing.
        if self
            .schema
            .as_ref()
            .is_none_or(|schema| self.state.shows(schema, &target))
        {
            self.start(target, cx);
        }
        self.changed(cx);
    }

    /// Ends every group search. Those groups' first pages are read again when a view that
    /// draws the exploration is next drawn ([`Self::read_owed`]): the tree ends them on leaving
    /// a graph, which then is not drawn.
    pub fn clear_searches(&mut self, cx: &mut Context<Self>) {
        if self.state.searches.is_empty() {
            return;
        }
        for (key, _) in std::mem::take(&mut self.state.searches) {
            self.state.neighbours.remove(&key);
            self.tasks.remove(&LoadTarget::Page(key));
        }
        self.owe(cx);
    }

    /// Reads ahead what opening `path` will show, without opening it, so it opens at once
    /// (spec 006: a pill the pointer rests on): the first [`PREFETCH_LIMIT`] reads it needs.
    /// Only once the exploration has its commit, so every read stays on it. One row is read
    /// ahead at a time: what was read ahead for another and is still out stops, unless it was
    /// opened since ([`Self::end_prefetch`]).
    pub fn prefetch(&mut self, path: &ExplorePath, cx: &mut Context<Self>) {
        if self.pin.is_none() {
            return;
        }
        let Some(schema) = self.schema() else {
            return;
        };
        if self.ahead.as_ref().is_some_and(|(ahead, _)| ahead == path) {
            return;
        }
        self.stop_ahead();
        // A target read or being read is in the state already, so it is not among these.
        let targets: Vec<LoadTarget> = self
            .state
            .prefetch_targets(&schema, path)
            .into_iter()
            .take(PREFETCH_LIMIT)
            .collect();
        for target in &targets {
            self.start(target.clone(), cx);
        }
        self.ahead = Some((path.clone(), targets));
    }

    /// The pointer left the row at `path`: what was read ahead for it and is still out stops,
    /// unless it was opened since, and is read again if it opens later.
    pub fn end_prefetch(&mut self, path: &ExplorePath, _cx: &mut Context<Self>) {
        if self.ahead.as_ref().is_some_and(|(ahead, _)| ahead == path) {
            self.stop_ahead();
        }
    }

    /// Stops the reads ahead still out whose answers nothing open shows, and forgets that they
    /// were asked for. An answer in stays, as one shown does.
    fn stop_ahead(&mut self) {
        let Some((_, targets)) = self.ahead.take() else {
            return;
        };
        let Some(schema) = self.schema.clone() else {
            return;
        };
        for target in targets {
            if self.state.shows(&schema, &target) || self.tasks.remove(&target).is_none() {
                continue;
            }
            // Dropping the read's task stops it: a count left out of its batch, a read
            // nobody waits for any more.
            let state = self.state.unseen();
            match &target {
                LoadTarget::Root => {}
                LoadTarget::Count(key) => {
                    state.counts.remove(key);
                }
                LoadTarget::Page(key) => {
                    if state
                        .neighbours
                        .get(key)
                        .is_some_and(|list| list.rows.is_empty())
                    {
                        state.neighbours.remove(key);
                    } else if let Some(list) = state.neighbours.get_mut(key) {
                        list.status = PageStatus::Idle;
                    }
                }
                LoadTarget::Values(key, by) => {
                    state.values.remove(&(key.clone(), by.clone()));
                }
            }
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

    /// Drops the pin and everything read, and reads what is visible again at the branch head,
    /// under the schema the session reads under now.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.forget_at_head(cx);
        self.request_missing(cx);
    }

    /// [`Self::refresh`] without reading: what it reads is missing, and the views hear the
    /// change.
    fn forget_at_head(&mut self, cx: &mut Context<Self>) {
        self.reset_reads(cx);
        self.state.clear_loaded();
        if self.held.is_none() {
            self.state.forget_set_total();
        }
        self.changed(cx);
    }

    /// Lets go of everything read, to read what is shown again at the same commit with every
    /// path left open or closed as it was, once a view that draws it is drawn: the schema names
    /// some type's nodes otherwise, or the token changed. Reads still out are dropped, and
    /// their answers land nowhere.
    fn read_again(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.tasks.clear();
        self.ahead = None;
        self.prune_at = KEPT_READS;
        self.state.clear_loaded();
        self.owe(cx);
    }

    /// Owes a look for what is missing, and says the exploration changed: a view that draws
    /// it reads what it owes when drawn ([`Self::read_owed`]).
    fn owe(&mut self, cx: &mut Context<Self>) {
        self.owed = true;
        self.changed(cx);
    }

    /// Whether a look for what is missing waits for a view that draws the exploration.
    pub fn owes_reads(&self) -> bool {
        self.owed
    }

    /// Reads what the session's events left missing ([`Self::owes_reads`]): called by a view
    /// that draws the exploration, when it is drawn.
    pub fn read_owed(&mut self, cx: &mut Context<Self>) {
        if self.owed {
            self.request_missing(cx);
        }
    }

    /// Drops the pin: what is read next is read at the branch head, under the session's schema.
    fn reset_reads(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.tasks.clear();
        self.ahead = None;
        self.prune_at = KEPT_READS;
        self.carried.clear();
        self.pin = self.held.clone();
        self.newer_available = false;
        self.schema = self.session.read(cx).schema_model();
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
        // A look reads everything missing, what is owed included.
        self.owed = false;
        let Some(schema) = self.schema() else {
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

    /// The state, to take an answer to `target` or mark it asked for: through the flattening
    /// when the answer can change what is shown, else leaving the flattening as made, so a
    /// read ahead for a row that is closed walks no tree when it lands.
    fn state_for(&mut self, target: &LoadTarget) -> &mut ExploreState {
        let shows = match &self.schema {
            Some(schema) => self.state.shows(schema, target),
            None => true,
        };
        if shows {
            &mut self.state
        } else {
            self.state.unseen()
        }
    }

    fn start(&mut self, target: LoadTarget, cx: &mut Context<Self>) {
        // Until the first read returns its commit, nothing else is read, so every read of the
        // exploration sees the same commit: what is asked for meanwhile (a group's search) is
        // read at that commit once the root's answer pins it, as missing.
        if target != LoadTarget::Root
            && self.pin.is_none()
            && self.state.root_status == Loadable::Loading
        {
            return;
        }
        let Some(schema) = self.schema() else {
            return;
        };
        if target == LoadTarget::Root && self.state.root.is_none() {
            return;
        }
        let page_size = self.page_size(cx);
        let state = self.state_for(&target);
        match &target {
            LoadTarget::Root => state.root_status = Loadable::Loading,
            LoadTarget::Count(key) => {
                state.counts.insert(key.clone(), Loadable::Loading);
            }
            LoadTarget::Page(key) => {
                let list = state.neighbours.entry(key.clone()).or_default();
                list.status = PageStatus::Loading;
                list.page_size = page_size;
            }
            LoadTarget::Values(key, by) => {
                state
                    .values
                    .insert((key.clone(), by.clone()), Loadable::Loading);
            }
        }
        let query = match &target {
            LoadTarget::Root => {
                let Some(root) = &self.state.root else {
                    return;
                };
                builder::node_label(&schema, &root.type_name, &root.id)
            }
            LoadTarget::Count(key) if !self.state.filters_of(&key.group).is_empty() => {
                // Filtered, a count is one node's own read, not a page's batch.
                builder::neighbour_count(
                    &schema,
                    &key.node.type_name,
                    &key.group,
                    &key.node.id,
                    self.state.filters_of(&key.group),
                )
            }
            LoadTarget::Count(key) => {
                let snapshot = self.pin.clone();
                let scope = &self.scope;
                let count = self.session.update(cx, |session, cx| {
                    session.degree_in(
                        scope,
                        &key.node.type_name,
                        &key.group,
                        &key.node.id,
                        snapshot.clone(),
                        cx,
                    )
                });
                self.spawn_count(key.clone(), count, snapshot, cx);
                return;
            }
            LoadTarget::Page(key) if key.is_set() => {
                let Some(SetRoots::Filtered {
                    type_name, filters, ..
                }) = &self.state.set
                else {
                    return;
                };
                let start = self
                    .state
                    .neighbours
                    .get(key)
                    .and_then(NeighbourList::next_after)
                    .map_or(PageStart::First, |id| PageStart::After {
                        id: id.to_owned(),
                        value: Value::Null,
                    });
                builder::node_page(&schema, type_name, filters, None, &start, page_size)
            }
            LoadTarget::Page(key) => {
                let search = self.state.searches.get(key).map(String::as_str);
                let after = self
                    .state
                    .neighbours
                    .get(key)
                    .and_then(NeighbourList::next_after);
                let value = key.within.as_ref().map(|within| within.value.value());
                let narrow = Narrow {
                    search,
                    value: key
                        .within
                        .as_ref()
                        .zip(value.as_ref())
                        .map(|(within, value)| (&within.by, value)),
                    filters: self.state.filters_of(&key.group),
                };
                builder::neighbours(
                    &schema,
                    &key.node.type_name,
                    &key.group,
                    &key.node.id,
                    after.map(builder::After::Neighbour),
                    narrow,
                    page_size,
                )
            }
            LoadTarget::Values(key, by) => {
                let filters = self.state.filters_of(&key.group);
                let values = builder::neighbour_values(
                    &schema,
                    &key.node.type_name,
                    &key.group,
                    &key.node.id,
                    by,
                    filters,
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
                        filters,
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
            Ok(query) => self.spawn_read_with(target, query, None, cx),
            Err(error) => self.apply_failure(&target, error.to_string()),
        }
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
        let sent_at = snapshot.clone();
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
        let ticket = self.next_ticket();
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
                this.finished(&key, ticket);
                let unpinned = this.pin.is_none();
                if !this.apply(&key, result, sent_at.as_deref(), cx) {
                    return;
                }
                this.loaded(&key, cx);
                // The first answer pins the exploration: a head seen move while it was out is
                // compared now, which it could not be before.
                if unpinned && this.pin.is_some() {
                    this.compare_head(cx);
                }
            })
            .ok();
        });
        self.tasks.insert(target, (ticket, task));
    }

    /// A number for a read about to start, which its entry in `tasks` is kept under.
    fn next_ticket(&mut self) -> u64 {
        self.next_task += 1;
        self.next_task
    }

    /// The read numbered `ticket` for `target` finished: its entry goes, unless a read of the
    /// same target started since holds it.
    fn finished(&mut self, target: &LoadTarget, ticket: u64) {
        if self
            .tasks
            .get(target)
            .is_some_and(|(held, _)| *held == ticket)
        {
            self.tasks.remove(target);
        }
    }

    /// Waits for a count the session reads together with the others asked for this turn, read
    /// at `sent_at` (the head when `None`). Its commit is checked as every answer's is
    /// ([`Self::pinned_at`]): the first answer pins the exploration, one read at the head
    /// before the pin and answered at another commit is read again at the pin, and one read at
    /// the pin that names another commit is a failure.
    fn spawn_count(
        &mut self,
        group: GroupKey,
        count: Task<Result<(u64, Option<String>), String>>,
        sent_at: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let generation = self.generation;
        let target = LoadTarget::Count(group.clone());
        let key = target.clone();
        let ticket = self.next_ticket();
        let task = cx.spawn(async move |this, cx| {
            let count = count.await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.finished(&key, ticket);
                let unpinned = this.pin.is_none();
                match count {
                    Ok((count, commit)) => {
                        match this.pinned_at(&key, commit.as_deref(), sent_at.as_deref(), cx) {
                            Pinned::Taken => this.state_for(&key).apply_count(group, count),
                            Pinned::Again => return,
                            Pinned::Elsewhere(message) => this.apply_failure(&key, message),
                        }
                    }
                    Err(message) => this.apply_failure(&key, message),
                }
                this.loaded(&key, cx);
                // A count that pinned the exploration: a head seen move meanwhile is compared
                // now, as after the first read.
                if unpinned && this.pin.is_some() {
                    this.compare_head(cx);
                }
            })
            .ok();
        });
        self.tasks.insert(target, (ticket, task));
    }

    /// Takes the commit an answer to `target` names, read at `sent_at` (the head when `None`):
    /// the first answer pins the exploration, and every later one must be at the pin, or at a
    /// commit the pin was carried past since the read was sent ([`Self::carried`]).
    fn pinned_at(
        &mut self,
        target: &LoadTarget,
        commit: Option<&str>,
        sent_at: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Pinned {
        match (&self.pin, commit) {
            (None, commit) => {
                self.pin = commit.map(str::to_owned);
                Pinned::Taken
            }
            (Some(pin), Some(commit)) if pin != commit => {
                // Asked before the pin moved on, at a commit it was carried past: nothing the
                // exploration reads changed since, so this is the pin's answer too.
                let carried = self.carried.iter().any(|past| past == commit);
                if carried && sent_at.is_none_or(|sent| sent == commit) {
                    return Pinned::Taken;
                }
                if sent_at.is_none() {
                    self.start(target.clone(), cx);
                    return Pinned::Again;
                }
                Pinned::Elsewhere(format!(
                    "The server answered at commit {}, not at {}, which the exploration shows.",
                    studio_ui::short_commit_id(commit),
                    studio_ui::short_commit_id(pin)
                ))
            }
            _ => Pinned::Taken,
        }
    }

    /// After an answer: reads what it made visible, and redraws when it shows. An answer read
    /// ahead for something closed shows nothing yet and makes nothing visible: nothing is read
    /// for it and nothing drawn, and the tree is not walked again, since the answer left the
    /// flattening as made ([`Self::state_for`]); opening it draws.
    fn loaded(&mut self, key: &LoadTarget, cx: &mut Context<Self>) {
        self.bound_reads();
        let shows = self
            .schema()
            .is_some_and(|schema| self.state.shows(&schema, key));
        if !shows {
            return;
        }
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
        cx.emit(ExplorationEvent::Loaded);
        cx.notify();
    }

    /// Keeps what the state holds bounded: past [`KEPT_READS`] counts, neighbour lists, values
    /// and labels, those nothing open can reach go ([`ExploreState::retain_reachable`]). What
    /// stays sets the next bound at twice its size, so a large walk that is all open is not
    /// gone through again at every answer.
    fn bound_reads(&mut self) {
        if self.state.held() <= self.prune_at {
            return;
        }
        // Nothing reachable goes, so nothing shown changes: the flattening stays as made.
        self.state.unseen().retain_reachable();
        self.prune_at = KEPT_READS.max(2 * self.state.held());
    }

    /// Takes the answer to a read of `target` sent at `sent_at` (unpinned when `None`). The
    /// first answer pins the exploration, and every later one must be at the pin: one read at
    /// the head before the pin was taken, and answered at another commit, is read again at the
    /// pin; one read at the pin that names another commit is a failure. Whether it was taken
    /// (`false` when it is read again).
    fn apply(
        &mut self,
        target: &LoadTarget,
        result: Result<ReadOutput, ReadError>,
        sent_at: Option<&str>,
        cx: &mut Context<Self>,
    ) -> bool {
        let output = match result {
            Ok(output) => output,
            Err(error) => {
                self.apply_failure(target, error.to_string());
                return true;
            }
        };
        match self.pinned_at(target, output.graph_commit_id.as_deref(), sent_at, cx) {
            Pinned::Taken => {}
            Pinned::Again => return false,
            Pinned::Elsewhere(message) => {
                self.apply_failure(target, message);
                return true;
            }
        }
        let page_size = match target {
            LoadTarget::Page(key) => self
                .state
                .neighbours
                .get(key)
                .map_or(1, |list| list.page_size.max(1)),
            _ => 1,
        };
        let schema = self.schema();
        let state = self.state_for(target);
        match target {
            LoadTarget::Page(key) if key.is_set() => {
                state.apply_set_page(key.clone(), &output, page_size, schema.as_deref())
            }
            LoadTarget::Root => state.apply_root(&output),
            // An unfiltered count is read with the others asked for this turn, and taken by
            // `spawn_count`; a filtered one is this node's own read, its `n` column.
            LoadTarget::Count(key) => {
                let n = rows(&output)
                    .first()
                    .and_then(|row| row.get("n"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                state.apply_count(key.clone(), n);
            }
            LoadTarget::Values(key, by) => {
                state.apply_values(key.clone(), by.clone(), &output, VALUES_LIMIT)
            }
            LoadTarget::Page(key) => state.apply_page(key.clone(), &output, page_size),
        }
        true
    }

    fn apply_failure(&mut self, target: &LoadTarget, message: String) {
        let state = self.state_for(target);
        match target {
            LoadTarget::Root => state.root_status = Loadable::Failed(message),
            LoadTarget::Count(key) => {
                state.counts.insert(key.clone(), Loadable::Failed(message));
            }
            LoadTarget::Page(key) => {
                state.neighbours.entry(key.clone()).or_default().status =
                    PageStatus::Failed(message);
            }
            LoadTarget::Values(key, by) => {
                state
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
            page: None,
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

        // A neighbour at two companies is in two values: the values' counts add to more than
        // the group's four, so what the most common leave out is not known, and the row saying
        // there are other values stays.
        state.apply_values(
            key.clone(),
            by.clone(),
            &output(
                &["value", "label", "n"],
                json!([
                    {"value": "acme", "label": "Acme", "n": 3},
                    {"value": "globex", "label": "Globex", "n": 3},
                ]),
            ),
            2,
        );
        let shown = kinds(&state.entries(&schema));
        assert!(
            shown.iter().any(|row| row.trim() == "other 2 None"),
            "{shown:?}"
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
        // Opened before their counts are in, the groups read only their counts: a page waits for
        // its count, and a relationship with none reads nothing more.
        let pages = |state: &ExploreState| {
            state
                .missing_loads(&schema)
                .into_iter()
                .filter(|target| match target {
                    LoadTarget::Page(key) => !is_field(&schema, &key.group),
                    LoadTarget::Values(..) => true,
                    _ => false,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(pages(&state), Vec::new());
        let works = group(&schema, "Person", "WorksAt", Direction::Outgoing);
        state.apply_count(GroupKey::new(person("ada"), knows.clone()), 2);
        state.apply_count(GroupKey::new(person("ada"), works), 0);
        assert_eq!(
            pages(&state),
            vec![LoadTarget::Page(GroupKey::new(person("ada"), knows))]
        );
    }

    /// Filters set on a relationship let go of its count and page, which are read again under
    /// them: the count as the node's own read, the page with the filter's clause; taking them
    /// off reads both again unfiltered (spec 005, "Filtering a relationship's neighbours").
    #[gpui::test]
    async fn filters_on_a_relationship_read_its_count_and_page_again(
        cx: &mut gpui::TestAppContext,
    ) {
        use std::sync::Mutex;

        use gq_language::builder::FilterOp;

        use crate::test_server::followed_session;
        let asked: Arc<Mutex<Vec<Value>>> = Arc::default();
        let seen = asked.clone();
        let (session, _served) = followed_session(
            SCHEMA,
            move |asked| {
                seen.lock().unwrap().push(asked.clone());
                match asked["name"].as_str() {
                    Some("studio_node") => json!([{ "id": "p1", "label": "Ada" }]),
                    Some("studio_page_degrees") => json!([{ "id": "p1", "n": 2 }]),
                    Some("studio_neighbour_count") => json!([{ "n": 1 }]),
                    Some("studio_neighbours") => json!([{ "id": "p2", "label": "Bob" }]),
                    _ => json!([]),
                }
            },
            cx,
        );
        cx.run_until_parked();
        let schema = schema();
        let knows = group(&schema, "Person", "Knows", Direction::Outgoing);
        let key = GroupKey::new(person("p1"), knows.clone());
        let path = ExplorePath::root().child_group(&knows);
        let exploration = cx.update(|cx| Exploration::for_session(&session, cx));
        exploration.update(cx, |exploration, cx| exploration.root_at(person("p1"), cx));
        cx.run_until_parked();
        exploration.update(cx, |exploration, cx| exploration.expand(&path, cx));
        cx.run_until_parked();
        exploration.read_with(cx, |exploration, _| {
            assert_eq!(exploration.state().counts[&key], Loadable::Loaded(2));
            assert_eq!(exploration.state().neighbours[&key].rows.len(), 1);
        });
        asked.lock().unwrap().clear();

        let filter = Filter {
            property: "name".into(),
            op: FilterOp::Eq,
            value: json!("Bob"),
        };
        exploration.update(cx, |exploration, cx| {
            exploration.set_filters(&knows, vec![filter.clone()], cx);
            assert_eq!(exploration.filters(&knows), std::slice::from_ref(&filter));
            // Let go, and asked for again at once.
            assert_eq!(
                exploration.state().counts.get(&key),
                Some(&Loadable::Loading),
                "let go"
            );
            // The page follows the count, as a group's first read does.
            assert!(!exploration.state().neighbours.contains_key(&key), "let go");
        });
        cx.run_until_parked();
        let reads: Vec<(String, Value)> = asked
            .lock()
            .unwrap()
            .iter()
            .map(|asked| {
                (
                    asked["name"].as_str().unwrap_or_default().to_owned(),
                    asked["params"]["f0"].clone(),
                )
            })
            .collect();
        assert!(
            reads.contains(&("studio_neighbour_count".to_owned(), json!("Bob"))),
            "{reads:?}"
        );
        assert!(
            reads.contains(&("studio_neighbours".to_owned(), json!("Bob"))),
            "{reads:?}"
        );
        exploration.read_with(cx, |exploration, _| {
            assert_eq!(exploration.state().counts[&key], Loadable::Loaded(1));
            assert_eq!(exploration.state().neighbours[&key].rows.len(), 1);
        });

        // Taken off, the count and the page are the unfiltered ones again: the session kept
        // their answers at the pin, so nothing is sent for them.
        asked.lock().unwrap().clear();
        exploration.update(cx, |exploration, cx| {
            exploration.set_filters(&knows, Vec::new(), cx);
            // The same filters again change nothing.
            exploration.set_filters(&knows, Vec::new(), cx);
        });
        cx.run_until_parked();
        let names: Vec<String> = asked
            .lock()
            .unwrap()
            .iter()
            .map(|asked| asked["name"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert!(
            !names.contains(&"studio_neighbour_count".to_owned()),
            "{names:?}"
        );
        exploration.read_with(cx, |exploration, _| {
            assert_eq!(exploration.state().counts[&key], Loadable::Loaded(2));
            assert_eq!(exploration.state().neighbours[&key].rows.len(), 1);
        });

        // A grouping changed lets go of the reads still out for the one before (its values, and
        // the pages of its values), as a filter's change does.
        exploration.update(cx, |exploration, cx| {
            let by_name = GroupBy {
                on: GroupOn::Node,
                property: "name".into(),
            };
            let by_relationship = GroupBy {
                on: GroupOn::Relationship,
                property: "out:WorksAt".into(),
            };
            exploration.set_grouping(&knows, Some(by_name.clone()), cx);
            // A value whose first page is in and whose next is being read keeps its rows, its
            // next page to be asked for again; one with none in yet is let go.
            let ada = GroupKey::new(person("p1"), knows.clone());
            let (bob, eve) = (
                ada.with_value(&by_name, &BucketValue("Bob".into())),
                ada.with_value(&by_name, &BucketValue("Eve".into())),
            );
            let reading = |rows: Vec<Neighbour>| NeighbourList {
                rows,
                status: PageStatus::Loading,
                ..NeighbourList::default()
            };
            let row = Neighbour {
                node: person("p2"),
                label: None,
                edge: Vec::new(),
            };
            let state = exploration.state.unseen();
            state.neighbours.insert(bob.clone(), reading(vec![row]));
            state.neighbours.insert(eve.clone(), reading(Vec::new()));
            let asked = |exploration: &Exploration, by: &GroupBy| {
                exploration
                    .tasks
                    .keys()
                    .any(|target| matches!(target, LoadTarget::Values(_, b) if b == by))
            };
            assert!(asked(exploration, &by_name), "its values asked for");
            exploration.set_grouping(&knows, Some(by_relationship), cx);
            assert!(
                !asked(exploration, &by_name),
                "the first grouping's read let go"
            );
            let kept = &exploration.state.neighbours[&bob];
            assert_eq!((kept.rows.len(), &kept.status), (1, &PageStatus::Idle));
            assert!(!exploration.state.neighbours.contains_key(&eve));
            // Switched back before it answered, the first grouping is read again: its read let
            // go leaves no mark that it is being read.
            exploration.set_grouping(&knows, Some(by_name.clone()), cx);
            assert!(asked(exploration, &by_name), "read again");
            exploration.set_grouping(&knows, None, cx);
        });
        cx.run_until_parked();

        // The walk started over on another branch keeps the relationship's filters, as Refresh
        // does: its group reads under them there.
        exploration.update(cx, |exploration, cx| {
            exploration.set_filters(&knows, vec![filter.clone()], cx)
        });
        session.update(cx, |session, cx| session.set_branch("fix".into(), cx));
        cx.run_until_parked();
        exploration.read_with(cx, |exploration, _| {
            assert_eq!(exploration.filters(&knows), std::slice::from_ref(&filter));
        });
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

    /// Every read of an exploration is at the commit its root's answer pinned: a group searched
    /// while the root is out waits for that answer, though another writer commits meanwhile,
    /// and an answer at another commit is never shown with the pinned ones: read at the head
    /// before the pin was taken, it is read again at the pin; read at the pin, it fails.
    #[gpui::test]
    async fn every_read_of_an_exploration_is_at_its_roots_commit(cx: &mut gpui::TestAppContext) {
        use futures::AsyncReadExt as _;
        use gpui::AppContext as _;
        use http_client::http::Response;
        use http_client::{AsyncBody, FakeHttpClient};

        use crate::connection::{Connection, ServerEntry, ServerSource};

        type Sent = Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>;
        // The branch head another writer moves, the root read held until released, and each
        // read sent but the head probe: its name and its commit.
        let head = Arc::new(std::sync::Mutex::new("c1".to_owned()));
        let (release, held) = futures::channel::oneshot::channel::<()>();
        let held = Arc::new(std::sync::Mutex::new(Some(held)));
        let sent: Sent = Arc::default();
        let (at, gate, log) = (head.clone(), held.clone(), sent.clone());
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let (at, gate, log) = (at.clone(), gate.clone(), log.clone());
            async move {
                let mut bytes = Vec::new();
                request.into_body().read_to_end(&mut bytes).await.ok();
                let body = if path.ends_with("/schema") {
                    json!({ "schema_source": SCHEMA })
                } else if path.ends_with("/query") {
                    let asked: Value = serde_json::from_slice(&bytes).unwrap();
                    let name = asked["name"].as_str().unwrap_or_default().to_owned();
                    let snapshot = asked["snapshot"].as_str().map(str::to_owned);
                    if name != "studio_head" {
                        log.lock().unwrap().push((name.clone(), snapshot.clone()));
                    }
                    let wait = gate.lock().unwrap().take_if(|_| name == "studio_node");
                    if let Some(wait) = wait {
                        wait.await.ok();
                    }
                    let commit = snapshot.unwrap_or_else(|| at.lock().unwrap().clone());
                    // Ada has neighbours to count, so the group searched reads its page once
                    // its count is in.
                    let rows = match name.as_str() {
                        "studio_node" => json!([{ "id": "ada", "label": "Ada" }]),
                        "studio_page_degrees" => json!([{ "id": "ada", "n": 2 }]),
                        _ => json!([]),
                    };
                    json!({ "query_name": name, "target": { "branch": "main" }, "row_count": 0,
                        "columns": [], "rows": rows, "graph_commit_id": commit })
                } else {
                    json!({ "branches": ["main"] })
                };
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        cx.run_until_parked();
        let schema = schema();
        let knows = group(&schema, "Person", "Knows", Direction::Outgoing);
        let key = GroupKey::new(person("ada"), knows.clone());
        let exploration = cx.update(|cx| Exploration::for_session(&session, cx));
        exploration.update(cx, |exploration, cx| {
            exploration.root_at(person("ada"), cx);
            exploration.expand(&ExplorePath::root().child_group(&knows), cx);
            exploration.search_group(&key, "b", cx);
        });
        cx.run_until_parked();
        let taken = || std::mem::take(&mut *sent.lock().unwrap());
        assert_eq!(
            taken(),
            vec![("studio_node".to_owned(), None)],
            "nothing else while the root is out"
        );

        *head.lock().unwrap() = "c2".into();
        release.send(()).ok();
        cx.run_until_parked();
        let pin = exploration.read_with(cx, |exploration, _| exploration.pin().map(str::to_owned));
        assert_eq!(pin.as_deref(), Some("c2"), "the root's answer");
        let after = taken();
        assert!(
            after.iter().any(|(name, _)| name == "studio_neighbours"),
            "{after:?}"
        );
        assert!(
            after
                .iter()
                .all(|(_, commit)| commit.as_deref() == Some("c2")),
            "{after:?}"
        );

        // An answer at another commit, naming a neighbour the pin's does not: read at the head
        // before the pin, it is read again at the pin (whose answer the session kept, so
        // nothing is sent); read at the pin, it fails. Neither shows the neighbour.
        let elsewhere = || ReadOutput {
            graph_commit_id: Some("c3".into()),
            ..output(&["id", "label"], json!([{ "id": "zed", "label": "Zed" }]))
        };
        let page = LoadTarget::Page(key.clone());
        let list = |cx: &mut gpui::TestAppContext| {
            exploration.read_with(cx, |exploration, _| {
                let list = &exploration.state().neighbours[&key];
                (list.status.clone(), list.rows.len())
            })
        };
        let taken_now = exploration.update(cx, |exploration, cx| {
            exploration.apply(&page, Ok(elsewhere()), None, cx)
        });
        assert!(!taken_now);
        assert_eq!(list(cx), (PageStatus::Loading, 0), "read again");
        cx.run_until_parked();
        assert_eq!(list(cx), (PageStatus::Idle, 0), "at the pin");
        assert!(taken().is_empty());
        let taken_now = exploration.update(cx, |exploration, cx| {
            exploration.apply(&page, Ok(elsewhere()), Some("c2"), cx)
        });
        assert!(taken_now);
        assert!(matches!(list(cx), (PageStatus::Failed(_), 0)));

        // A count is held to the pin as every other answer is: read at the head before the pin
        // and answered at another commit, it is read again at the pin; read at the pin and
        // naming another commit, it fails. Neither shows the other commit's count.
        let counted = |cx: &mut gpui::TestAppContext| {
            exploration.read_with(cx, |exploration, _| {
                exploration.state().counts.get(&key).cloned()
            })
        };
        let at_c3 = || Task::ready(Ok((9, Some("c3".to_owned()))));
        exploration.update(cx, |exploration, cx| {
            exploration.spawn_count(key.clone(), at_c3(), None, cx)
        });
        cx.run_until_parked();
        // The pin's count, as the server answers it there.
        assert_eq!(
            counted(cx),
            Some(Loadable::Loaded(2)),
            "read again at the pin"
        );
        assert!(taken().iter().all(|(_, at)| at.as_deref() == Some("c2")));
        exploration.update(cx, |exploration, cx| {
            exploration.spawn_count(key.clone(), at_c3(), Some("c2".into()), cx)
        });
        cx.run_until_parked();
        assert!(matches!(counted(cx), Some(Loadable::Failed(_))));
        // Unpinned (the root's read failed), the first count's commit is the pin, which every
        // read after it is held to.
        exploration.update(cx, |exploration, cx| {
            exploration.pin = None;
            exploration.spawn_count(key.clone(), at_c3(), None, cx)
        });
        cx.run_until_parked();
        let pin = exploration.read_with(cx, |exploration, _| exploration.pin().map(str::to_owned));
        assert_eq!(pin.as_deref(), Some("c3"));
        assert_eq!(counted(cx), Some(Loadable::Loaded(9)));
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

    /// Another writer's schema leaves the exploration as it was read: its rows flattened under
    /// the schema they were read under, a dropped relationship still shown and an added one not
    /// yet, with Refresh offered; Refresh takes the new schema at the head (constitution II).
    #[gpui::test]
    async fn an_exploration_keeps_the_schema_it_was_read_under_until_refresh(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::test_server::followed_session;
        const PERSON: &str = "node Person {\n  slug: String @key\n  name: String\n}\n";
        let (session, served) = followed_session(
            &format!("{PERSON}edge Knows: Person -> Person\n"),
            |_| json!([{ "id": "p1", "label": "Ada", "n": 0 }]),
            cx,
        );
        cx.run_until_parked();
        let exploration = cx.update(|cx| Exploration::for_session(&session, cx));
        exploration.update(cx, |exploration, cx| exploration.root_at(person("p1"), cx));
        cx.run_until_parked();
        let shown = |cx: &mut gpui::TestAppContext| {
            exploration.read_with(cx, |exploration, _| {
                let groups = kinds(&exploration.entries())
                    .into_iter()
                    .filter(|row| row.trim_start().starts_with("group"))
                    .collect::<Vec<_>>();
                (groups, exploration.newer_available())
            })
        };
        let knows = vec![
            "  group Knows Outgoing Some(0)".to_owned(),
            "  group Knows Incoming Some(0)".to_owned(),
        ];
        assert_eq!(shown(cx), (knows.clone(), false));

        served.lock().unwrap().schema = format!("{PERSON}edge Follows: Person -> Person\n");
        served.lock().unwrap().commit("c2", Vec::new());
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        let moved = session.read_with(cx, |session, _| {
            session.schema_model().unwrap().edge("Follows").is_some()
        });
        assert!(moved, "the session reads under the new schema");
        assert_eq!(shown(cx), (knows, true), "as read, with Refresh offered");

        exploration.update(cx, |exploration, cx| exploration.refresh(cx));
        cx.run_until_parked();
        assert_eq!(
            shown(cx),
            (
                vec![
                    "  group Follows Outgoing Some(0)".to_owned(),
                    "  group Follows Incoming Some(0)".to_owned(),
                ],
                false
            )
        );
    }

    /// A read sent at the pin and still out when another writer's commit, which changed nothing
    /// the exploration reads, carried the pin to the head answers at the old pin: it stands,
    /// since nothing it reads changed in between, for a page and a count alike. An answer at a
    /// commit the pin never was at is still a failure.
    #[gpui::test]
    async fn an_answer_at_a_commit_the_pin_was_carried_past_stands(cx: &mut gpui::TestAppContext) {
        use crate::test_server::{followed_session, node_change};
        let (session, served) = followed_session(
            SCHEMA,
            |_| json!([{ "id": "p1", "label": "Ada", "n": 0 }]),
            cx,
        );
        cx.run_until_parked();
        let exploration = cx.update(|cx| Exploration::for_session(&session, cx));
        exploration.update(cx, |exploration, cx| exploration.root_at(person("p1"), cx));
        cx.run_until_parked();
        let pin = |cx: &mut gpui::TestAppContext| {
            exploration.read_with(cx, |exploration, _| exploration.pin().map(str::to_owned))
        };
        assert_eq!(pin(cx).as_deref(), Some("c1"));

        // Another writer changes a company: nothing the exploration reads.
        served
            .lock()
            .unwrap()
            .commit("c2", vec![node_change("Company", "k9")]);
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        assert_eq!(pin(cx).as_deref(), Some("c2"), "carried to the head");

        let schema = schema();
        let knows = group(&schema, "Person", "Knows", Direction::Outgoing);
        let key = GroupKey::new(person("p1"), knows);
        let at = |commit: &str| ReadOutput {
            graph_commit_id: Some(commit.into()),
            ..output(&["id", "label"], json!([{ "id": "p2", "label": "Bo" }]))
        };
        let page = LoadTarget::Page(key.clone());
        let list = |cx: &mut gpui::TestAppContext| {
            exploration.read_with(cx, |exploration, _| {
                let list = &exploration.state().neighbours[&key];
                (list.status.clone(), list.rows.len())
            })
        };
        exploration.update(cx, |exploration, cx| {
            assert!(exploration.apply(&page, Ok(at("c1")), Some("c1"), cx));
        });
        assert_eq!(
            list(cx),
            (PageStatus::Idle, 1),
            "the old pin's answer stands"
        );
        exploration.update(cx, |exploration, cx| {
            exploration.state.neighbours.remove(&key);
            exploration.apply(&page, Ok(at("c9")), Some("c1"), cx);
        });
        assert!(
            matches!(list(cx), (PageStatus::Failed(_), 0)),
            "a commit the pin never was at"
        );

        exploration.update(cx, |exploration, cx| {
            let answer = Task::ready(Ok((5, Some("c1".to_owned()))));
            exploration.spawn_count(key.clone(), answer, Some("c1".into()), cx)
        });
        cx.run_until_parked();
        let counted = exploration.read_with(cx, |exploration, _| {
            exploration.state().counts[&key].clone()
        });
        assert_eq!(counted, Loadable::Loaded(5));
        assert_eq!(pin(cx).as_deref(), Some("c2"));
    }

    /// A new token may see less than the old one: what the exploration read under the old one
    /// goes at once, and is read again under the new one at the same commit.
    #[gpui::test]
    async fn a_new_token_reads_what_the_exploration_shows_again(cx: &mut gpui::TestAppContext) {
        use futures::AsyncReadExt as _;
        use gpui::AppContext as _;
        use http_client::http::Response;
        use http_client::{AsyncBody, FakeHttpClient};

        use crate::connection::{Connection, ServerEntry, ServerSource};

        // Each node read: the token it went with and the commit it was pinned to.
        type Sent = Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>;
        let sent: Sent = Arc::default();
        let log = sent.clone();
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let token = request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            let log = log.clone();
            async move {
                let mut bytes = Vec::new();
                request.into_body().read_to_end(&mut bytes).await.ok();
                let body = if path.ends_with("/schema") {
                    json!({ "schema_source": SCHEMA })
                } else if path.ends_with("/query") {
                    let asked: Value = serde_json::from_slice(&bytes).unwrap();
                    let name = asked["name"].as_str().unwrap_or_default().to_owned();
                    let snapshot = asked["snapshot"].as_str().map(str::to_owned);
                    if name == "studio_node" {
                        log.lock().unwrap().push((token.clone(), snapshot));
                    }
                    // The new token may not see Ada.
                    let rows = match name.as_str() {
                        "studio_node" if token == "Bearer old" => {
                            json!([{ "id": "ada", "label": "Ada" }])
                        }
                        "studio_page_degrees" => json!([{ "id": "ada", "n": 0 }]),
                        _ => json!([]),
                    };
                    json!({ "query_name": name, "target": { "branch": "main" }, "row_count": 0,
                        "columns": [], "rows": rows, "graph_commit_id": "c1" })
                } else {
                    json!({ "branches": ["main"] })
                };
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("old".into()), http).unwrap());
        let session = cx
            .new(|cx| GraphSession::new(connection.clone(), "knowledge".into(), "main".into(), cx));
        cx.run_until_parked();
        let exploration = cx.update(|cx| Exploration::for_session(&session, cx));
        exploration.update(cx, |exploration, cx| exploration.root_at(person("ada"), cx));
        cx.run_until_parked();
        let shown = |cx: &mut gpui::TestAppContext| {
            exploration.read_with(cx, |exploration, _| {
                let state = exploration.state();
                (
                    state.root_status.clone(),
                    state.label(&person("ada")).map(str::to_owned),
                )
            })
        };
        assert_eq!(shown(cx), (Loadable::Loaded(()), Some("Ada".into())));
        assert_eq!(
            std::mem::take(&mut *sent.lock().unwrap()),
            vec![("Bearer old".to_owned(), None)]
        );

        connection.update(cx, |connection, cx| {
            connection.set_token(Some("new".into()), cx)
        });
        let (status, label) = shown(cx);
        assert_eq!(label, None, "the old token's label is gone at once");
        assert!(!matches!(status, Loadable::Loaded(())));
        cx.run_until_parked();
        // Read again once a view that draws the exploration is drawn, not before.
        assert!(sent.lock().unwrap().is_empty());
        assert!(exploration.read_with(cx, |exploration, _| exploration.owes_reads()));
        exploration.update(cx, |exploration, cx| exploration.read_owed(cx));
        cx.run_until_parked();
        // At the pin, which is the head: sent as a read of the branch whose commit is checked.
        let again: Vec<String> = std::mem::take(&mut *sent.lock().unwrap())
            .into_iter()
            .map(|(token, _)| token)
            .collect();
        assert_eq!(again, ["Bearer new"], "read again under the new token");
        let (status, label) = shown(cx);
        assert!(matches!(status, Loadable::Failed(_)), "{status:?}");
        assert_eq!(label, None);
        let pin = exploration.read_with(cx, |exploration, _| exploration.pin().map(str::to_owned));
        assert_eq!(pin.as_deref(), Some("c1"));
    }

    /// A row the pointer rests on reads ahead at most `PREFETCH_LIMIT` reads; the pointer
    /// moving on before they answer stops them and sends none, and those answered stay. Reads
    /// that finished let go of their tasks, and an answer for a row that is closed leaves the
    /// tree's flattening as it was made.
    #[gpui::test]
    async fn what_is_read_ahead_stops_when_the_pointer_moves_on(cx: &mut gpui::TestAppContext) {
        use futures::AsyncReadExt as _;
        use gpui::AppContext as _;
        use http_client::http::Response;
        use http_client::{AsyncBody, FakeHttpClient};

        use crate::connection::{Connection, ServerEntry, ServerSource};

        let mut many = String::from("node Person {\n  slug: String @key\n  name: String\n}\n");
        for edge in 0..10 {
            many.push_str(&format!("edge E{edge}: Person -> Person\n"));
        }
        let text = many.clone();
        // Reads about p2 never answer, as a slow server's would not before the pointer moves on.
        let http = FakeHttpClient::create(move |request| {
            let path = request.uri().path().to_owned();
            let text = text.clone();
            async move {
                let mut bytes = Vec::new();
                request.into_body().read_to_end(&mut bytes).await.ok();
                let body = if path.ends_with("/schema") {
                    json!({ "schema_source": text })
                } else if path.ends_with("/query") {
                    let asked: Value = serde_json::from_slice(&bytes).unwrap();
                    if asked["params"].to_string().contains("p2") {
                        futures::future::pending::<()>().await;
                    }
                    json!({ "query_name": asked["name"], "target": { "branch": "main" },
                        "row_count": 1, "columns": [],
                        "rows": [{ "id": "p1", "label": "Ada", "n": 0 }],
                        "graph_commit_id": "c1" })
                } else {
                    json!({ "branches": ["main"] })
                };
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        cx.run_until_parked();
        let exploration = cx.update(|cx| Exploration::for_session(&session, cx));
        exploration.update(cx, |exploration, cx| exploration.root_at(person("p1"), cx));
        cx.run_until_parked();
        let schema = exploration.read_with(cx, |exploration, _| exploration.schema().unwrap());
        let e0 = group(&schema, "Person", "E0", Direction::Outgoing);
        let under_e0 = |id: &str| ExplorePath::root().child_group(&e0).child_node(&person(id));
        let ahead_of = |exploration: &Exploration, id: &str| {
            exploration
                .state()
                .counts
                .iter()
                .filter(|(key, _)| key.node == person(id))
                .map(|(_, count)| count.clone())
                .collect::<Vec<_>>()
        };
        let out = |cx: &mut gpui::TestAppContext| {
            session.read_with(cx, |session, _| session.reads_in_flight)
        };
        assert_eq!(out(cx), 0);

        // Twenty relationships to count; the first eight are read ahead.
        exploration.update(cx, |exploration, cx| {
            exploration.prefetch(&under_e0("p2"), cx)
        });
        cx.run_until_parked();
        exploration.read_with(cx, |exploration, _| {
            assert_eq!(
                ahead_of(exploration, "p2"),
                vec![Loadable::Loading; PREFETCH_LIMIT]
            );
        });
        assert_eq!(out(cx), PREFETCH_LIMIT, "one read per relationship, out");

        // The pointer moves on: they stop, and nothing says they were asked for.
        exploration.update(cx, |exploration, cx| {
            exploration.end_prefetch(&under_e0("p2"), cx)
        });
        cx.run_until_parked();
        assert_eq!(out(cx), 0, "stopped");
        exploration.read_with(cx, |exploration, _| {
            assert!(ahead_of(exploration, "p2").is_empty());
            assert!(exploration.tasks.is_empty());
        });

        // Answered, they stay; finished reads let go of their tasks; and the answers, for a row
        // that is closed, leave the flattening as it was made.
        let made = exploration.read_with(cx, |exploration, _| exploration.entries());
        exploration.update(cx, |exploration, cx| {
            exploration.prefetch(&under_e0("p3"), cx)
        });
        cx.run_until_parked();
        exploration.update(cx, |exploration, cx| {
            assert_eq!(
                ahead_of(exploration, "p3"),
                vec![Loadable::Loaded(0); PREFETCH_LIMIT]
            );
            assert!(exploration.tasks.is_empty(), "finished reads let go");
            assert!(
                Arc::ptr_eq(&made, &exploration.entries()),
                "a closed row's answers leave the flattening as made"
            );
            exploration.end_prefetch(&under_e0("p3"), cx);
            assert_eq!(
                ahead_of(exploration, "p3").len(),
                PREFETCH_LIMIT,
                "answers stay"
            );
        });

        // Another row read ahead in its place stops the first's reads still out.
        exploration.update(cx, |exploration, cx| {
            exploration.prefetch(&under_e0("p2"), cx)
        });
        cx.run_until_parked();
        assert_eq!(out(cx), PREFETCH_LIMIT);
        exploration.update(cx, |exploration, cx| {
            exploration.prefetch(&under_e0("p4"), cx)
        });
        cx.run_until_parked();
        assert_eq!(out(cx), 0);
        exploration.read_with(cx, |exploration, _| {
            assert!(ahead_of(exploration, "p2").is_empty());
            assert_eq!(ahead_of(exploration, "p4").len(), PREFETCH_LIMIT);
        });
    }

    /// A relationship read ahead and left before its neighbours answered shows nothing, nor
    /// does a node's count read ahead and left before it answered: another writer's change to
    /// a node of the type at the relationship's other end, and to an edge at that node, leaves
    /// the exploration as it is, and its pin follows the head.
    #[gpui::test]
    async fn a_relationship_left_before_it_answered_changes_nothing_shown(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::test_server::{followed_session, node_change};
        let (session, served) = followed_session(
            SCHEMA,
            |asked| match asked["name"].as_str() {
                Some("studio_page_degrees") => json!([{ "id": "p1", "n": 3 }]),
                _ => json!([{ "id": "p1", "label": "Ada" }]),
            },
            cx,
        );
        cx.run_until_parked();
        let exploration = cx.update(|cx| Exploration::for_session(&session, cx));
        exploration.update(cx, |exploration, cx| exploration.root_at(person("p1"), cx));
        cx.run_until_parked();
        // Its neighbours, and a Company's counts, never answer before the pointer moves on.
        served.lock().unwrap().hold = Box::new(|asked| {
            let text = asked["query"].as_str().unwrap_or_default();
            (asked["name"] == "studio_neighbours" && text.contains("orksAt"))
                || (asked["name"] == "studio_page_degrees" && text.contains("$n: Company"))
        });
        let schema = schema();
        let works_at = group(&schema, "Person", "WorksAt", Direction::Outgoing);
        let path = ExplorePath::root().child_group(&works_at);
        exploration.update(cx, |exploration, cx| {
            exploration.prefetch(&path, cx);
            assert_eq!(exploration.tasks.len(), 1, "its first page");
        });
        cx.run_until_parked();
        exploration.update(cx, |exploration, cx| exploration.end_prefetch(&path, cx));
        cx.run_until_parked();
        // A Company under it, whose one relationship is counted when read ahead.
        let k1 = path.child_node(&NodeRef::new("Company", "k1"));
        exploration.update(cx, |exploration, cx| {
            exploration.prefetch(&k1, cx);
            assert_eq!(exploration.tasks.len(), 1, "its count");
        });
        cx.run_until_parked();
        exploration.update(cx, |exploration, cx| {
            exploration.end_prefetch(&k1, cx);
            assert!(exploration.tasks.is_empty(), "the count stopped");
        });
        cx.run_until_parked();

        let works_at_k1 = json!({
            "kind": "edge",
            "type": { "id": "WorksAt", "name": "WorksAt" },
            "id": "e9",
            "op": "insert",
            "after": { "properties": {}, "endpoints": { "from": "p5", "to": "k1" } },
        });
        served
            .lock()
            .unwrap()
            .commit("c2", vec![node_change("Company", "k9"), works_at_k1]);
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        let (pin, newer) = exploration.read_with(cx, |exploration, _| {
            (
                exploration.pin().map(str::to_owned),
                exploration.newer_available(),
            )
        });
        assert_eq!((pin.as_deref(), newer), (Some("c2"), false));
    }

    /// Nodes named otherwise in the settings while the graph is parked: nothing is said and
    /// nothing read until it is shown again, when the tree reads what it shows under the new
    /// names.
    #[gpui::test]
    async fn a_parked_graph_reads_under_new_names_only_when_shown(cx: &mut gpui::TestAppContext) {
        use crate::test_server::followed_session;
        let asked: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let log = asked.clone();
        let (session, _served) = followed_session(
            "node Person {\n  slug: String @key\n  name: String\n  title: String\n}\n",
            move |request| {
                log.lock()
                    .unwrap()
                    .push(request["name"].as_str().unwrap_or_default().to_owned());
                json!([{ "id": "p1", "label": "Ada", "n": 0 }])
            },
            cx,
        );
        cx.run_until_parked();
        let exploration = cx.update(|cx| Exploration::for_session(&session, cx));
        exploration.update(cx, |exploration, cx| exploration.root_at(person("p1"), cx));
        cx.run_until_parked();
        let heard = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = heard.clone();
        let _heard = cx.update(|cx| {
            cx.subscribe(&session, move |_, event: &SessionEvent, _| {
                if *event == SessionEvent::SchemaChanged {
                    counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            })
        });
        let heard = || heard.load(std::sync::atomic::Ordering::SeqCst);

        session.update(cx, |session, cx| session.set_shown(false, cx));
        asked.lock().unwrap().clear();
        let by_title = [("Person".to_owned(), "title".to_owned())]
            .into_iter()
            .collect();
        session.update(cx, |session, cx| session.take_labels(by_title, cx));
        cx.run_until_parked();
        assert_eq!(heard(), 0);
        assert!(asked.lock().unwrap().is_empty(), "{:?}", asked.lock());

        session.update(cx, |session, cx| session.set_shown(true, cx));
        cx.run_until_parked();
        assert_eq!(heard(), 1);
        let read_node = |asked: &Arc<std::sync::Mutex<Vec<String>>>| {
            asked
                .lock()
                .unwrap()
                .iter()
                .any(|name| name == "studio_node")
        };
        assert!(!read_node(&asked), "nothing read until the tree is drawn");
        exploration.update(cx, |exploration, cx| exploration.read_owed(cx));
        cx.run_until_parked();
        assert!(
            read_node(&asked),
            "read again under the new names: {:?}",
            asked.lock()
        );
    }

    /// Past its bound, the state lets go of what nothing open reaches and keeps the rest.
    #[test]
    fn what_nothing_open_reaches_is_let_go() {
        let schema = schema();
        let knows = group(&schema, "Person", "Knows", Direction::Outgoing);
        let works_at = group(&schema, "Person", "WorksAt", Direction::Outgoing);
        let mut state = loaded_root(&schema);
        let open = ExplorePath::root().child_group(&knows);
        state.expand(&open);
        let key = |id: &str, group: &RelationshipGroup| GroupKey::new(person(id), group.clone());
        for (id, group) in [("ada", &knows), ("ada", &works_at), ("far", &knows)] {
            state.apply_count(key(id, group), 1);
        }
        state.counts.insert(key("near", &knows), Loadable::Loading);
        let page = output(&["id", "label"], json!([{ "id": "bob", "label": "Bob" }]));
        state.apply_page(key("ada", &knows), &page, 50);
        let elsewhere = output(&["id", "label"], json!([{ "id": "zed", "label": "Zed" }]));
        state.apply_page(key("far", &knows), &elsewhere, 50);
        state.labels.insert(person("far"), Some("Far".into()));

        state.retain_reachable();
        let mut counted: Vec<_> = state.counts.keys().map(|k| k.node.id.clone()).collect();
        counted.sort();
        assert_eq!(
            counted,
            ["ada", "ada", "near"],
            "the root's and the one being read"
        );
        assert!(state.neighbours.contains_key(&key("ada", &knows)));
        assert!(!state.neighbours.contains_key(&key("far", &knows)));
        assert_eq!(state.label(&person("bob")), Some("Bob"));
        assert!(!state.labels.contains_key(&person("zed")));
        assert!(!state.labels.contains_key(&person("far")));
        assert!(state.labels.contains_key(&person("ada")), "the root's");
    }

    /// A walk is one per session, root and commit, and a held one stays at its commit whatever
    /// the branch does (spec 020).
    #[gpui::test]
    async fn a_held_walk_stays_at_its_commit(cx: &mut gpui::TestAppContext) {
        let (session, served) = crate::test_server::followed_session(
            "node Person {\n  slug: String @key\n  name: String\n}\n",
            |_| serde_json::json!([{ "id": "p1", "label": "Ada", "n": 0 }]),
            cx,
        );
        cx.run_until_parked();
        let ada = NodeRef::new("Person", "p1");
        let head = cx.update(|cx| Exploration::for_walk(&session, ada.clone(), None, cx));
        let held =
            cx.update(|cx| Exploration::for_walk(&session, ada.clone(), Some("c0".into()), cx));
        assert_ne!(head, held);
        let same = cx.update(|cx| Exploration::for_walk(&session, ada.clone(), None, cx));
        assert_eq!(head, same);
        cx.run_until_parked();
        held.read_with(cx, |held, _| {
            assert_eq!(held.pin(), Some("c0"));
            assert_eq!(held.held_at(), Some("c0"));
            assert_eq!(held.root(), Some(&ada));
        });
        served
            .lock()
            .unwrap()
            .commit("c2", vec![crate::test_server::node_change("Person", "p1")]);
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        assert!(head.read_with(cx, |head, _| head.newer_available()));
        held.read_with(cx, |held, _| {
            assert!(!held.newer_available(), "a held walk offers nothing newer");
            assert_eq!(held.pin(), Some("c0"));
        });
        held.update(cx, |held, cx| held.refresh(cx));
        cx.run_until_parked();
        assert_eq!(
            held.read_with(cx, |held, _| held.pin().map(str::to_owned))
                .as_deref(),
            Some("c0")
        );
    }

    /// A set's count is its rows' number at the commit it was read at: a set at the head read
    /// again (Refresh, another branch) lets it go, and "Show more" goes by its pages. Before, it
    /// kept the count it opened with, and stopped short on a branch with more rows. A set held
    /// at a commit keeps it.
    #[gpui::test]
    async fn a_sets_count_goes_when_it_is_read_at_another_commit(cx: &mut gpui::TestAppContext) {
        use crate::test_server::followed_session;
        let (session, _served) = followed_session(SCHEMA, |_| json!([]), cx);
        cx.run_until_parked();
        let set = SetRoots::Filtered {
            type_name: "Person".into(),
            filters: Vec::new(),
            total: Some(100),
        };
        let total = |exploration: &Entity<Exploration>, cx: &mut gpui::TestAppContext| {
            exploration.read_with(cx, |exploration, _| match &exploration.state().set {
                Some(SetRoots::Filtered { total, .. }) => *total,
                _ => panic!("a filtered set"),
            })
        };
        let at_head = cx.update(|cx| Exploration::for_set(&session, set.clone(), None, cx));
        let held = cx.update(|cx| Exploration::for_set(&session, set, Some("c1".into()), cx));
        cx.run_until_parked();
        assert_eq!(total(&at_head, cx), Some(100), "as it opened");
        at_head.update(cx, |exploration, cx| exploration.refresh(cx));
        held.update(cx, |exploration, cx| exploration.refresh(cx));
        cx.run_until_parked();
        assert_eq!(total(&at_head, cx), None);
        assert_eq!(total(&held, cx), Some(100), "held at its commit");
    }

    /// A set's rows sit at the top of the tree, each its own path, with "Show more" while pages
    /// remain; opening one reads below it alone, and the filter's first page is all it reads
    /// before that (spec 020).
    #[test]
    fn a_set_is_paged_and_its_rows_open_on_their_own() {
        let schema = schema();
        let mut state = ExploreState::of_set(SetRoots::Filtered {
            type_name: "Person".into(),
            filters: Vec::new(),
            total: Some(3),
        });
        let key = GroupKey::of_set("Person");
        assert!(key.is_set());
        assert_eq!(
            state.missing_loads(&schema),
            vec![LoadTarget::Page(key.clone())]
        );
        state.neighbours.entry(key.clone()).or_default().status = PageStatus::Loading;
        state.apply_set_page(
            key.clone(),
            &output(
                &["n"],
                json!([
                    {"n": {"@id": "ada", "name": "Ada"}},
                    {"n": {"@id": "bob", "name": "Bob"}},
                ]),
            ),
            2,
            Some(&schema),
        );
        assert_eq!(state.label(&person("ada")), Some("Ada"));
        assert_eq!(
            state.missing_loads(&schema),
            Vec::new(),
            "nothing below closed rows"
        );
        let entries = state.entries(&schema);
        assert!(matches!(entries[0].kind, EntryKind::Node { .. }));
        assert_eq!(
            entries[0].path,
            ExplorePath::root().child_node(&person("ada"))
        );
        assert_eq!(entries[0].parent, None);
        assert!(matches!(
            entries.last().map(|entry| &entry.kind),
            Some(EntryKind::More {
                shown: 2,
                total: Some(3),
                ..
            })
        ));
        state.expand(&ExplorePath::root().child_node(&person("ada")));
        assert!(
            state
                .missing_loads(&schema)
                .iter()
                .all(|target| matches!(target, LoadTarget::Count(key) | LoadTarget::Page(key) if key.node == person("ada"))),
            "only below the row opened"
        );
        state.apply_set_page(
            key,
            &output(&["n"], json!([{"n": {"@id": "cy", "name": "Cy"}}])),
            2,
            Some(&schema),
        );
        let entries = state.entries(&schema);
        assert!(
            !entries
                .iter()
                .any(|entry| matches!(entry.kind, EntryKind::More { .. })),
            "a short page is the last"
        );
        state.clear_loaded();
        assert_eq!(state.root_status, Loadable::Loaded(()), "no root to read");
    }
}

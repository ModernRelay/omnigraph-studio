//! What the canvas draws, without GPUI: the exploration as nodes, hubs (one per opened
//! relationship group) and "+K more" chips, each node drawn once, and a query's result nodes.

use std::collections::{HashMap, HashSet};

use graph_store::{
    Entry, EntryKind, ExplorePath, ExploreState, GroupKey, LoadTarget, Loadable, NodeRef,
    PageStatus,
};
use studio_actions::graph::{ChangeLink, ChangeMark, ChangeNode};

/// Neighbours a hub shows before "+K more", and how many each click on it adds.
pub const HUB_STEP: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SceneId {
    Node(NodeRef),
    Hub(GroupKey),
    More(GroupKey),
    /// What an open node's relationships say while none is drawn.
    Note(NodeRef),
    /// A relationship a branch changed, on its change's canvas (spec 042): its index among the
    /// change's links.
    Link(usize),
}

/// What is not drawn yet says, in place of it (H5 and H16 in the canvas UX report): an open
/// node's relationships, or an open relationship's neighbours.
#[derive(Debug, Clone, PartialEq)]
pub enum Note {
    /// Still being read.
    Reading,
    /// Could not be read: the reads Retry asks for again.
    Failed(Vec<LoadTarget>),
    /// Read, and there are none.
    Empty,
    /// There are some, and the node cap left them out: the cap's banner counts them.
    LeftOut,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SceneKind {
    /// A node of the graph. `root` marks where the exploration started.
    Node {
        node: NodeRef,
        root: bool,
        expanded: bool,
    },
    /// One relationship group of a node.
    Hub {
        key: GroupKey,
        count: Option<u64>,
        open: bool,
    },
    /// The neighbours of a hub not drawn yet, or what reading them says.
    More {
        key: GroupKey,
        hidden: u64,
        note: Option<Note>,
    },
    /// What an open node's relationships say while none is drawn: being read, could not be
    /// read (with Retry), or none.
    Note { node: NodeRef, note: Note },
    /// A relationship a branch changed, as a pill between its two ends (spec 042). `outgoing`
    /// when the chip it hangs off is the edge's source.
    Link { outgoing: bool },
}

impl Note {
    /// What a node's note chip says.
    pub fn of_relationships(&self) -> &'static str {
        match self {
            Note::Reading => "Reading relationships…",
            Note::Failed(_) => "Couldn't read relationships · Retry",
            Note::Empty => "No relationships",
            Note::LeftOut => "Relationships not drawn · over the node cap",
        }
    }

    /// What a relationship's "+K more" says in place of its count.
    pub fn of_neighbours(&self) -> &'static str {
        match self {
            Note::Reading => "Reading…",
            Note::Failed(_) => "Couldn't read · Retry",
            Note::Empty => "None",
            Note::LeftOut => "Not drawn · over the node cap",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SceneNode {
    pub id: SceneId,
    pub kind: SceneKind,
    pub label: String,
    /// The type or edge name that picks the color.
    pub color_key: String,
    /// The node this one hangs off, for layout.
    pub parent: Option<usize>,
    /// Where this node sits in the exploration: a node path, or a hub's group path.
    pub path: Option<ExplorePath>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SceneEdge {
    pub from: usize,
    pub to: usize,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scene {
    pub nodes: Vec<SceneNode>,
    pub edges: Vec<SceneEdge>,
    /// Nodes left out because of the node cap, and the type most of them belong to.
    pub capped: Option<(usize, String)>,
    /// The node cap the scene was built under, which its banner names: a setting changed
    /// since is not this scene's until it is built again.
    pub cap: usize,
    /// On a change's canvas, what the branch did to each chip it changed (spec 042); a node
    /// without one is an anchor, which the branch left as it was.
    pub marks: HashMap<SceneId, ChangeMark>,
}

/// `scene` without the nodes hidden (`HideNodes`), nor what hangs off them: the chips
/// whose chain of parents reaches a hidden node (spec 032, "Hide"). The walk's root is never
/// hidden.
pub fn without(scene: Scene, hidden: &HashSet<NodeRef>) -> Scene {
    if hidden.is_empty() {
        return scene;
    }
    let gone: Vec<bool> = (0..scene.nodes.len())
        .map(|index| {
            let mut at = Some(index);
            while let Some(ix) = at {
                let node = &scene.nodes[ix];
                if let SceneKind::Node {
                    node, root: false, ..
                } = &node.kind
                    && hidden.contains(node)
                {
                    return true;
                }
                at = node.parent;
            }
            false
        })
        .collect();
    let mut kept_at = vec![None; scene.nodes.len()];
    let mut nodes = Vec::new();
    for (index, node) in scene.nodes.into_iter().enumerate() {
        if !gone[index] {
            kept_at[index] = Some(nodes.len());
            nodes.push(node);
        }
    }
    for node in &mut nodes {
        node.parent = node.parent.and_then(|parent| kept_at[parent]);
    }
    let edges = scene
        .edges
        .into_iter()
        .filter_map(|edge| {
            Some(SceneEdge {
                from: kept_at[edge.from]?,
                to: kept_at[edge.to]?,
            })
        })
        .collect();
    Scene {
        nodes,
        edges,
        capped: scene.capped,
        cap: scene.cap,
        marks: scene.marks,
    }
}

impl Scene {
    #[cfg(test)]
    pub fn index_of(&self, id: &SceneId) -> Option<usize> {
        self.nodes.iter().position(|node| node.id == *id)
    }

    /// Where each chip is, for finding many by id at once: ⌘A can mark thousands, and a walk
    /// of the scene for each would be quadratic.
    pub fn indices(&self) -> HashMap<&SceneId, usize> {
        self.nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (&node.id, index))
            .collect()
    }
}

/// Every open path the exploration reaches any of the chips `ids` by, in one pass over the
/// entries; "+K more" stands for its relationship. A node or relationship reached by more than
/// one path is drawn once and shows open while any of them is, so closing it closes them all.
pub fn open_paths(entries: &[Entry], state: &ExploreState, ids: &[SceneId]) -> Vec<ExplorePath> {
    let mut nodes = HashSet::new();
    let mut hubs = HashSet::new();
    for id in ids {
        match id {
            SceneId::Node(node) => {
                nodes.insert(node);
            }
            SceneId::Hub(key) | SceneId::More(key) => {
                hubs.insert(key);
            }
            SceneId::Note(_) | SceneId::Link(_) => {}
        }
    }
    entries
        .iter()
        .filter(|entry| match &entry.kind {
            EntryKind::Node { node, expanded, .. } => *expanded && nodes.contains(node),
            EntryKind::Group { key, expanded, .. } => *expanded && hubs.contains(key),
            EntryKind::Field {
                key,
                target: Some(_),
                ..
            } => hubs.contains(key) && state.is_expanded(&entry.path),
            _ => false,
        })
        .map(|entry| entry.path.clone())
        .collect()
}

/// A query result as the canvas draws it: each node once, a line between the nodes of a row.
/// No hubs: a double click explores from a node.
pub fn results_scene(
    nodes: &[(NodeRef, Option<String>)],
    links: &[(usize, usize)],
    cap: usize,
) -> Scene {
    let capped = nodes.len().saturating_sub(cap);
    let scene_nodes: Vec<SceneNode> = nodes
        .iter()
        .take(cap)
        .map(|(node, label)| SceneNode {
            id: SceneId::Node(node.clone()),
            kind: SceneKind::Node {
                node: node.clone(),
                root: false,
                expanded: false,
            },
            label: label.clone().unwrap_or_else(|| node.id.clone()),
            color_key: node.type_name.clone(),
            parent: None,
            path: None,
        })
        .collect();
    let shown = scene_nodes.len();
    let edges = links
        .iter()
        .filter(|(from, to)| *from < shown && *to < shown && from != to)
        .map(|(from, to)| SceneEdge {
            from: *from,
            to: *to,
        })
        .collect();
    let capped = (capped > 0).then(|| {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for (node, _) in nodes.iter().skip(cap) {
            *counts.entry(node.type_name.as_str()).or_default() += 1;
        }
        (capped, most_left_out(counts).unwrap_or_default())
    });
    Scene {
        nodes: scene_nodes,
        edges,
        capped,
        cap,
        marks: HashMap::new(),
    }
}

/// A branch's change as the canvas draws it (spec 042): each node once, and each changed
/// relationship as a pill between its ends, so the line reads node, relationship, node. Laid
/// out as a walk is: from the node most changed relationships meet (the call an ingest hung
/// everything on), each pill hangs off the end it was reached from and the other end off the
/// pill; a part no relationship joins starts a walk of its own. A node a second relationship
/// reaches is drawn once, with a line to it. At most `cap` chips; the nodes left out are
/// counted, by the type most of them are.
pub fn change_scene(nodes: &[ChangeNode], links: &[ChangeLink], cap: usize) -> Scene {
    let mut scene = Scene {
        cap,
        ..Scene::default()
    };
    let mut touching: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for (index, link) in links.iter().enumerate() {
        if link.from < nodes.len() && link.to < nodes.len() && link.from != link.to {
            touching[link.from].push(index);
            touching[link.to].push(index);
        }
    }
    // Where the walks start: the nodes the most changed relationships meet, then the changed
    // nodes before their anchors, then as given.
    let mut starts: Vec<usize> = (0..nodes.len()).collect();
    starts.sort_by_key(|&index| {
        (
            std::cmp::Reverse(touching[index].len()),
            nodes[index].change.is_none(),
            index,
        )
    });
    let mut placed: Vec<Option<usize>> = vec![None; nodes.len()];
    let mut drawn_links = vec![false; links.len()];
    let node_chip = |node: &ChangeNode, parent: Option<usize>| {
        let graph_node = NodeRef::new(node.type_name.clone(), node.id.clone());
        SceneNode {
            id: SceneId::Node(graph_node.clone()),
            kind: SceneKind::Node {
                node: graph_node,
                root: false,
                expanded: false,
            },
            label: node.label.clone().unwrap_or_else(|| node.id.clone()),
            color_key: node.type_name.clone(),
            parent,
            path: None,
        }
    };
    'walks: for start in starts {
        if placed[start].is_some() {
            continue;
        }
        if scene.nodes.len() >= cap {
            break;
        }
        placed[start] = Some(scene.nodes.len());
        scene.nodes.push(node_chip(&nodes[start], None));
        let mut queue = std::collections::VecDeque::from([start]);
        while let Some(at) = queue.pop_front() {
            let Some(here) = placed[at] else { continue };
            for &index in &touching[at] {
                if drawn_links[index] {
                    continue;
                }
                let link = &links[index];
                let outgoing = link.from == at;
                let other = if outgoing { link.to } else { link.from };
                // A pill needs room for itself and, unless drawn, its other end.
                let room = 1 + usize::from(placed[other].is_none());
                if scene.nodes.len() + room > cap {
                    break 'walks;
                }
                drawn_links[index] = true;
                let pill = scene.nodes.len();
                scene.nodes.push(SceneNode {
                    id: SceneId::Link(index),
                    kind: SceneKind::Link { outgoing },
                    label: link.words.clone(),
                    color_key: String::new(),
                    parent: Some(here),
                    path: None,
                });
                scene.marks.insert(SceneId::Link(index), link.change);
                scene.edges.push(SceneEdge {
                    from: here,
                    to: pill,
                });
                let there = match placed[other] {
                    Some(there) => there,
                    None => {
                        let there = scene.nodes.len();
                        scene.nodes.push(node_chip(&nodes[other], Some(pill)));
                        placed[other] = Some(there);
                        queue.push_back(other);
                        there
                    }
                };
                scene.edges.push(SceneEdge {
                    from: pill,
                    to: there,
                });
            }
        }
    }
    for (node, at) in nodes.iter().zip(&placed) {
        if let (Some(at), Some(mark)) = (at, node.change) {
            scene.marks.insert(scene.nodes[*at].id.clone(), mark);
        }
    }
    let mut left_out: HashMap<&str, usize> = HashMap::new();
    for (node, at) in nodes.iter().zip(&placed) {
        if at.is_none() {
            *left_out.entry(node.type_name.as_str()).or_default() += 1;
        }
    }
    let hidden: usize = left_out.values().sum();
    if hidden > 0 {
        scene.capped = most_left_out(left_out).map(|type_name| (hidden, type_name));
    }
    scene
}

/// The exploration as the canvas draws it. `limits` holds how many neighbours each opened hub
/// shows; a hub without an entry shows [`HUB_STEP`].
pub fn explore_scene(
    entries: &[Entry],
    state: &ExploreState,
    limits: &HashMap<GroupKey, usize>,
    cap: usize,
) -> Scene {
    let mut scene = Scene {
        cap,
        ..Scene::default()
    };
    let mut by_id: HashMap<SceneId, usize> = HashMap::new();
    // The scene index each entry maps to (its node or hub), and entries left out.
    let mut placed: Vec<Option<usize>> = vec![None; entries.len()];
    let mut skipped = vec![false; entries.len()];
    // Entries the cap left out, with everything under them: the banner counts them, once each
    // however many paths reach them. Neighbours past a hub's limit are "+K more" instead.
    let mut capped = vec![false; entries.len()];
    let mut hidden: HashSet<SceneId> = HashSet::new();
    let mut shown_per_hub: HashMap<usize, usize> = HashMap::new();
    // The lines drawn so far, so a line is drawn once without searching every line.
    let mut lines: HashSet<(usize, usize)> = HashSet::new();
    // What each open node's relationships have come to, by its chip: its note says it while
    // none is drawn, and when one could not be read.
    let mut progress: HashMap<usize, Progress> = HashMap::new();
    let label_of = |node: &NodeRef| {
        state
            .label(node)
            .map(str::to_owned)
            .unwrap_or_else(|| node.id.clone())
    };

    let add = |scene: &mut Scene, by_id: &mut HashMap<SceneId, usize>, node: SceneNode| {
        let index = scene.nodes.len();
        by_id.insert(node.id.clone(), index);
        scene.nodes.push(node);
        index
    };

    for (index, entry) in entries.iter().enumerate() {
        if let Some(parent) = entry.parent
            && skipped[parent]
        {
            skipped[index] = true;
            if capped[parent] {
                capped[index] = true;
                left_out(entry, &by_id, &mut hidden);
            }
            continue;
        }
        let parent_scene = entry.parent.and_then(|parent| placed[parent]);
        match &entry.kind {
            // A root that could not be read is drawn as no chip: the canvas's banner says why.
            EntryKind::Node { .. }
                if entry.parent.is_none() && matches!(state.root_status, Loadable::Failed(_)) =>
            {
                skipped[index] = true;
            }
            EntryKind::Node { node, expanded, .. } => {
                let id = SceneId::Node(node.clone());
                // A hub draws `limit` neighbour edges, whether or not their nodes are new.
                if let Some(hub) = parent_scene {
                    let shown = shown_per_hub.entry(hub).or_default();
                    let limit = hub_limit(&scene.nodes[hub], limits);
                    if *shown >= limit {
                        skipped[index] = true;
                        continue;
                    }
                    *shown += 1;
                }
                if let Some(existing) = by_id.get(&id).copied() {
                    // Drawn once: another path to it adds an edge only.
                    if let Some(hub) = parent_scene {
                        push_edge(&mut scene, &mut lines, hub, existing);
                    }
                    placed[index] = Some(existing);
                    if *expanded
                        && let SceneKind::Node { expanded, .. } = &mut scene.nodes[existing].kind
                    {
                        *expanded = true;
                    }
                    continue;
                }
                if scene.nodes.len() >= cap {
                    hidden.insert(id);
                    skipped[index] = true;
                    capped[index] = true;
                    continue;
                }
                let root = entry.parent.is_none();
                let new = add(
                    &mut scene,
                    &mut by_id,
                    SceneNode {
                        id,
                        kind: SceneKind::Node {
                            node: node.clone(),
                            root,
                            expanded: *expanded,
                        },
                        label: label_of(node),
                        color_key: node.type_name.clone(),
                        parent: parent_scene,
                        path: Some(entry.path.clone()),
                    },
                );
                if let Some(hub) = parent_scene {
                    push_edge(&mut scene, &mut lines, hub, new);
                }
                placed[index] = Some(new);
            }
            EntryKind::Group {
                key,
                count,
                expanded,
            } => {
                let Some(owner) = parent_scene else {
                    skipped[index] = true;
                    continue;
                };
                let noted = progress.entry(owner).or_default();
                match count {
                    Loadable::NotLoaded | Loadable::Loading => noted.reading = true,
                    Loadable::Failed(_) => noted.failed.push(LoadTarget::Count(key.clone())),
                    Loadable::Loaded(_) => {}
                }
                // A hub is drawn once it is counted and has something: the pills do not
                // appear for every relationship and then go as their counts come in empty.
                if !matches!(count, Loadable::Loaded(n) if *n > 0) {
                    skipped[index] = true;
                    continue;
                }
                let id = SceneId::Hub(key.clone());
                // Counted toward its node's note once drawn; one the cap leaves out says so.
                if let Some(existing) = by_id.get(&id).copied() {
                    progress.entry(owner).or_default().hubs += 1;
                    placed[index] = Some(existing);
                    if *expanded
                        && let SceneKind::Hub { open, .. } = &mut scene.nodes[existing].kind
                    {
                        *open = true;
                    }
                    continue;
                }
                if scene.nodes.len() >= cap {
                    progress.entry(owner).or_default().left_out = true;
                    hidden.insert(id);
                    skipped[index] = true;
                    capped[index] = true;
                    continue;
                }
                progress.entry(owner).or_default().hubs += 1;
                let arrow = key.group.direction.arrow();
                // Searched (the Explore tree's "Find among"), the hub's neighbours are the
                // matches alone: counted once all are read, else unknown. The relationship's
                // count would offer "+K more" that no page brings.
                let count_value = match state.searches.contains_key(key) {
                    true => state
                        .neighbours
                        .get(key)
                        .filter(|list| list.exhausted)
                        .map(|list| list.rows.len() as u64),
                    false => count.loaded().copied(),
                };
                let label = match count_value {
                    Some(count) => format!("{arrow} {} · {count}", key.group.edge),
                    None => format!("{arrow} {} · …", key.group.edge),
                };
                let hub = add(
                    &mut scene,
                    &mut by_id,
                    SceneNode {
                        id,
                        kind: SceneKind::Hub {
                            key: key.clone(),
                            count: count_value,
                            open: *expanded,
                        },
                        label,
                        color_key: key.group.edge.clone(),
                        parent: Some(owner),
                        path: Some(entry.path.clone()),
                    },
                );
                push_edge(&mut scene, &mut lines, owner, hub);
                placed[index] = Some(hub);
            }
            // A relationship with at most one target is a hub like any other on the canvas, so
            // expanding a node shows its relationships first; opening the hub shows the one
            // node it reaches. (The tree shows it inline, as a field.)
            EntryKind::Field {
                key,
                target: Some(_),
                ..
            } => {
                let Some(owner) = parent_scene else {
                    continue;
                };
                let open = state.is_expanded(&entry.path);
                let hub_id = SceneId::Hub(key.clone());
                let hub = match by_id.get(&hub_id).copied() {
                    // Drawn by another path: open while either path opens it.
                    Some(existing) => {
                        if open
                            && let SceneKind::Hub { open: shown, .. } =
                                &mut scene.nodes[existing].kind
                        {
                            *shown = true;
                        }
                        existing
                    }
                    None if scene.nodes.len() < cap => {
                        let arrow = key.group.direction.arrow();
                        let hub = add(
                            &mut scene,
                            &mut by_id,
                            SceneNode {
                                id: hub_id,
                                kind: SceneKind::Hub {
                                    key: key.clone(),
                                    count: Some(1),
                                    open,
                                },
                                label: format!("{arrow} {} · 1", key.group.edge),
                                color_key: key.group.edge.clone(),
                                parent: Some(owner),
                                path: Some(entry.path.clone()),
                            },
                        );
                        push_edge(&mut scene, &mut lines, owner, hub);
                        hub
                    }
                    None => {
                        // Left out by the cap, with the target the walk puts under it.
                        progress.entry(owner).or_default().left_out = true;
                        hidden.insert(hub_id);
                        skipped[index] = true;
                        capped[index] = true;
                        continue;
                    }
                };
                // Opened, its target is the node entry the walk puts under it (a node of the
                // walk, expandable as any), drawn by the entries that follow.
                progress.entry(owner).or_default().hubs += 1;
                placed[index] = Some(hub);
            }
            EntryKind::Cycle { node, .. } => {
                if let (Some(hub), Some(existing)) = (
                    parent_scene,
                    by_id.get(&SceneId::Node(node.clone())).copied(),
                ) {
                    push_edge(&mut scene, &mut lines, hub, existing);
                }
            }
            // The canvas draws a grouped relationship as its hub: a value's neighbours hang
            // from the hub, as they do without grouping.
            EntryKind::Bucket { .. } => placed[index] = parent_scene,
            // A single target still being read, or one that could not be (its error row
            // follows), counts toward its node's note.
            EntryKind::Field {
                target: None,
                loading,
                ..
            } => {
                if let Some(owner) = parent_scene {
                    progress.entry(owner).or_default().reading |= *loading;
                }
            }
            EntryKind::Error {
                target: target @ LoadTarget::Page(_),
                ..
            } => {
                let field_owner = entry
                    .parent
                    .filter(|parent| matches!(entries[*parent].kind, EntryKind::Field { .. }))
                    .and_then(|field| entries[field].parent)
                    .and_then(|node| placed[node]);
                if let Some(owner) = field_owner {
                    progress
                        .entry(owner)
                        .or_default()
                        .failed
                        .push(target.clone());
                }
            }
            EntryKind::OtherValues { .. }
            | EntryKind::More { .. }
            | EntryKind::Loading { .. }
            | EntryKind::Error { .. } => {}
        }
    }

    // "+K more" under every open hub with neighbours not drawn.
    let hubs: Vec<usize> = (0..scene.nodes.len())
        .filter(|ix| matches!(scene.nodes[*ix].kind, SceneKind::Hub { open: true, .. }))
        .collect();
    let mut drawn_from: HashMap<usize, u64> = HashMap::new();
    for edge in &scene.edges {
        *drawn_from.entry(edge.from).or_default() += 1;
    }
    for hub in hubs {
        let SceneKind::Hub { key, count, .. } = scene.nodes[hub].kind.clone() else {
            continue;
        };
        let drawn = drawn_from.get(&hub).copied().unwrap_or(0);
        let total = count.unwrap_or(0);
        let note = neighbours_note(state, &key, drawn);
        let wanted = total > drawn || note.is_some();
        if wanted && scene.nodes.len() >= cap {
            hidden.insert(SceneId::More(key));
        } else if wanted {
            let label = match &note {
                Some(note) => note.of_neighbours().to_owned(),
                None => format!("+{} more", total - drawn),
            };
            let more = SceneNode {
                id: SceneId::More(key.clone()),
                kind: SceneKind::More {
                    key: key.clone(),
                    hidden: total.saturating_sub(drawn),
                    note,
                },
                label,
                color_key: key.group.edge.clone(),
                parent: Some(hub),
                path: scene.nodes[hub].path.clone(),
            };
            let index = add(&mut scene, &mut by_id, more);
            push_edge(&mut scene, &mut lines, hub, index);
        }
    }

    // An open node with no relationship drawn says why, and one with a relationship that
    // could not be read says so, with Retry.
    let mut owners: Vec<(usize, Progress)> = progress.into_iter().collect();
    owners.sort_by_key(|(owner, _)| *owner);
    for (owner, noted) in owners {
        let SceneKind::Node { node, .. } = &scene.nodes[owner].kind else {
            continue;
        };
        let note = if !noted.failed.is_empty() {
            Note::Failed(noted.failed)
        } else if noted.hubs > 0 {
            continue;
        } else if noted.left_out {
            Note::LeftOut
        } else if noted.reading {
            Note::Reading
        } else {
            Note::Empty
        };
        // A note left out by the cap would leave an open node saying nothing; one cut by the
        // cap is the cap's own exit for that node, so it is drawn past it. At most one a
        // drawn open node.
        if scene.nodes.len() >= cap && note != Note::LeftOut {
            continue;
        }
        let node = node.clone();
        let chip = SceneNode {
            id: SceneId::Note(node.clone()),
            label: note.of_relationships().to_owned(),
            color_key: node.type_name.clone(),
            kind: SceneKind::Note { node, note },
            parent: Some(owner),
            path: None,
        };
        let index = add(&mut scene, &mut by_id, chip);
        push_edge(&mut scene, &mut lines, owner, index);
    }

    // A chip one path left out and another drew is drawn.
    hidden.retain(|id| !by_id.contains_key(id));
    if !hidden.is_empty() {
        // The table offered is the type most of them are, or lead to.
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for id in &hidden {
            let type_name = match id {
                SceneId::Node(node) => node.type_name.as_str(),
                SceneId::Hub(key) | SceneId::More(key) => key.group.other_type.as_str(),
                SceneId::Note(node) => node.type_name.as_str(),
                SceneId::Link(_) => continue,
            };
            *counts.entry(type_name).or_default() += 1;
        }
        scene.capped = most_left_out(counts).map(|type_name| (hidden.len(), type_name));
    }
    scene
}

/// What an open node's relationships have come to.
#[derive(Debug, Default)]
struct Progress {
    /// Some are still being read.
    reading: bool,
    /// The reads that failed, for Retry.
    failed: Vec<LoadTarget>,
    /// How many are drawn.
    hubs: usize,
    /// The node cap left one or more out.
    left_out: bool,
}

/// What an open relationship's "+K more" says in place of its count, with `drawn` of its
/// neighbours drawn: its first neighbours (or values) being read, or a read that failed.
fn neighbours_note(state: &ExploreState, key: &GroupKey, drawn: u64) -> Option<Note> {
    match state.grouping.get(&key.group) {
        Some(by) => match state.values.get(&(key.clone(), by.clone())) {
            Some(Loadable::Failed(_)) => Some(Note::Failed(vec![LoadTarget::Values(
                key.clone(),
                by.clone(),
            )])),
            None | Some(Loadable::NotLoaded | Loadable::Loading) if drawn == 0 => {
                Some(Note::Reading)
            }
            _ => None,
        },
        None => match state.neighbours.get(key).map(|list| &list.status) {
            Some(PageStatus::Failed(_)) => Some(Note::Failed(vec![LoadTarget::Page(key.clone())])),
            None | Some(PageStatus::Loading) if drawn == 0 => Some(Note::Reading),
            _ => None,
        },
    }
}

/// Records `entry`, under an entry the cap left out, as not drawn, unless it is drawn by
/// another path.
fn left_out(entry: &Entry, drawn: &HashMap<SceneId, usize>, hidden: &mut HashSet<SceneId>) {
    let id = match &entry.kind {
        EntryKind::Node { node, .. } => SceneId::Node(node.clone()),
        EntryKind::Group {
            key,
            count: Loadable::Loaded(n),
            ..
        } if *n > 0 => SceneId::Hub(key.clone()),
        EntryKind::Field {
            key,
            target: Some(_),
            ..
        } => SceneId::Hub(key.clone()),
        _ => return,
    };
    if !drawn.contains_key(&id) {
        hidden.insert(id);
    }
}

/// The type most of the nodes left out belong to; of types as many, the first by name, so the
/// banner names the same one each time the scene is made.
fn most_left_out<S: AsRef<str>>(counts: impl IntoIterator<Item = (S, usize)>) -> Option<String> {
    counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.as_ref().cmp(a.0.as_ref())))
        .map(|(type_name, _)| type_name.as_ref().to_owned())
}

fn hub_limit(hub: &SceneNode, limits: &HashMap<GroupKey, usize>) -> usize {
    match &hub.kind {
        SceneKind::Hub { key, .. } => limits.get(key).copied().unwrap_or(HUB_STEP),
        _ => usize::MAX,
    }
}

/// An edge, once.
fn push_edge(scene: &mut Scene, lines: &mut HashSet<(usize, usize)>, from: usize, to: usize) {
    if lines.insert((from, to)) {
        scene.edges.push(SceneEdge { from, to });
    }
}

#[cfg(test)]
mod tests {
    use gq_language::Direction;
    use gq_language::SchemaModel;

    /// Of types with as many nodes left out, the banner names the first by name, whatever
    /// order they were counted in.
    #[test]
    fn the_banner_names_one_type_however_ties_are_counted() {
        for counts in [
            vec![("Person", 250), ("Company", 250), ("Note", 3)],
            vec![("Company", 250), ("Note", 3), ("Person", 250)],
        ] {
            assert_eq!(super::most_left_out(counts).as_deref(), Some("Company"));
        }
        assert_eq!(super::most_left_out(Vec::<(&str, usize)>::new()), None);
    }
    use graph_store::{Neighbour, NeighbourList};

    use super::*;

    /// The cap the tests draw under: the `canvas_max_nodes` setting's default, the cap in use.
    fn node_cap() -> usize {
        studio_settings::StudioOptions::default().canvas_max_nodes as usize
    }

    const SCHEMA: &str = "node Person {\n  slug: String @key\n  name: String\n}\n\
        node Company {\n  slug: String @key\n  name: String\n}\n\
        edge WorksAt: Person -> Company @card(0..)\n\
        edge Knows: Person -> Person @card(0..)\n";

    fn person(id: &str) -> NodeRef {
        NodeRef::new("Person", id)
    }

    /// Ada knows 20 people; Bob, the first, knows Ada back.
    fn exploration() -> (SchemaModel, ExploreState) {
        let schema = SchemaModel::parse(SCHEMA).unwrap();
        let mut state = ExploreState::rooted(person("ada"));
        state.root_status = Loadable::Loaded(());
        let knows = schema
            .relationships_of("Person")
            .iter()
            .find(|g| g.edge == "Knows" && g.direction == Direction::Outgoing)
            .cloned()
            .unwrap();
        let ada_knows = GroupKey::new(person("ada"), knows.clone());
        state.counts.insert(ada_knows.clone(), Loadable::Loaded(20));
        state.neighbours.insert(
            ada_knows,
            NeighbourList {
                rows: (0..20)
                    .map(|i| Neighbour {
                        node: person(&format!("p{i:02}")),
                        label: None,
                        edge: Vec::new(),
                    })
                    .collect(),
                exhausted: true,
                ..Default::default()
            },
        );
        let group = ExplorePath::root().child_group(&knows);
        state.expanded.insert(group.clone());
        let p00 = group.child_node(&person("p00"));
        state.expanded.insert(p00.clone());
        state.expanded.insert(p00.child_group(&knows));
        let p00_knows = GroupKey::new(person("p00"), knows);
        state.counts.insert(p00_knows.clone(), Loadable::Loaded(2));
        state.neighbours.insert(
            p00_knows,
            NeighbourList {
                rows: vec![
                    Neighbour {
                        node: person("ada"),
                        label: None,
                        edge: Vec::new(),
                    },
                    Neighbour {
                        node: person("p05"),
                        label: None,
                        edge: Vec::new(),
                    },
                ],
                exhausted: true,
                ..Default::default()
            },
        );
        (schema, state)
    }

    #[test]
    fn a_single_target_relationship_is_a_hub_that_opens_to_its_node() {
        let schema = SchemaModel::parse(&format!(
            "{SCHEMA}edge Employer: Person -> Company @card(0..1)\n"
        ))
        .unwrap();
        let mut state = ExploreState::rooted(person("ada"));
        state.root_status = Loadable::Loaded(());
        let employer = schema
            .relationships_of("Person")
            .iter()
            .find(|g| g.edge == "Employer")
            .cloned()
            .unwrap();
        let key = GroupKey::new(person("ada"), employer.clone());
        let acme = NodeRef::new("Company", "acme");
        state.neighbours.insert(
            key.clone(),
            NeighbourList {
                rows: vec![Neighbour {
                    node: acme.clone(),
                    label: None,
                    edge: Vec::new(),
                }],
                exhausted: true,
                ..Default::default()
            },
        );
        // Expanding Ada shows the relationship, not the company it reaches.
        let closed = explore_scene(&state.entries(&schema), &state, &HashMap::new(), node_cap());
        let hub = closed
            .index_of(&SceneId::Hub(key.clone()))
            .expect("a hub for the relationship");
        assert!(matches!(
            closed.nodes[hub].kind,
            SceneKind::Hub { open: false, .. }
        ));
        assert!(closed.index_of(&SceneId::Node(acme.clone())).is_none());
        // Opening the hub shows the one company, hanging off the hub.
        state
            .expanded
            .insert(ExplorePath::root().child_group(&employer));
        let open = explore_scene(&state.entries(&schema), &state, &HashMap::new(), node_cap());
        let hub = open.index_of(&SceneId::Hub(key)).unwrap();
        let company = open
            .index_of(&SceneId::Node(acme))
            .expect("the company once open");
        assert_eq!(open.nodes[company].parent, Some(hub));
        assert!(open.edges.iter().any(|e| e.from == hub && e.to == company));
    }

    /// The node a single-target relationship reaches is a node of the walk: it has a path,
    /// so a click or ↵ expands it to its own relationships, as any node does (H1 in the
    /// canvas UX report). Before, it was drawn with no path and only opened a page.
    #[test]
    fn a_single_target_node_expands_as_any_node() {
        let schema = SchemaModel::parse(&format!(
            "{SCHEMA}edge Employer: Person -> Company @card(0..1)\n"
        ))
        .unwrap();
        let mut state = ExploreState::rooted(person("ada"));
        state.root_status = Loadable::Loaded(());
        let employer = schema
            .relationships_of("Person")
            .iter()
            .find(|g| g.edge == "Employer")
            .cloned()
            .unwrap();
        let acme = NodeRef::new("Company", "acme");
        state.neighbours.insert(
            GroupKey::new(person("ada"), employer.clone()),
            NeighbourList {
                rows: vec![Neighbour {
                    node: acme.clone(),
                    label: None,
                    edge: Vec::new(),
                }],
                exhausted: true,
                ..Default::default()
            },
        );
        let field = ExplorePath::root().child_group(&employer);
        state.expanded.insert(field.clone());
        let scene = explore_scene(&state.entries(&schema), &state, &HashMap::new(), node_cap());
        let company = scene.index_of(&SceneId::Node(acme.clone())).unwrap();
        let acme_path = field.child_node(&acme);
        assert_eq!(scene.nodes[company].path.as_ref(), Some(&acme_path));
        // Expanded, Acme shows its own relationships once counted.
        let works_in = schema
            .relationships_of("Company")
            .iter()
            .find(|g| g.edge == "WorksAt")
            .cloned()
            .unwrap();
        let acme_works = GroupKey::new(acme.clone(), works_in);
        state.counts.insert(acme_works.clone(), Loadable::Loaded(2));
        state.expanded.insert(acme_path);
        let scene = explore_scene(&state.entries(&schema), &state, &HashMap::new(), node_cap());
        let company = scene.index_of(&SceneId::Node(acme)).unwrap();
        assert!(matches!(
            scene.nodes[company].kind,
            SceneKind::Node { expanded: true, .. }
        ));
        let hub = scene
            .index_of(&SceneId::Hub(acme_works))
            .expect("Acme's own relationship");
        assert_eq!(scene.nodes[hub].parent, Some(company));
    }

    /// The note an open node's relationships leave in place of hubs (H5, H16 in the canvas UX
    /// report): "Reading relationships…" while counted, "No relationships" when there are
    /// none, and "Couldn't read relationships · Retry", with what to read again, when a count
    /// failed; none once a relationship is drawn. Before, an open node with failed or empty
    /// counts drew nothing, as if it had no relationships either way.
    #[test]
    fn an_open_node_says_what_its_relationships_have_come_to() {
        let schema = SchemaModel::parse(SCHEMA).unwrap();
        let mut state = ExploreState::rooted(person("ada"));
        state.root_status = Loadable::Loaded(());
        let groups: Vec<GroupKey> = schema
            .relationships_of("Person")
            .iter()
            .map(|group| GroupKey::new(person("ada"), group.clone()))
            .collect();
        let note = |state: &ExploreState| {
            let scene = explore_scene(&state.entries(&schema), state, &HashMap::new(), node_cap());
            let note = scene
                .nodes
                .iter()
                .find(|node| node.id == SceneId::Note(person("ada")))
                .map(|node| (node.label.clone(), node.kind.clone()));
            if note.is_some() {
                let ada = scene.index_of(&SceneId::Node(person("ada"))).unwrap();
                let chip = scene.index_of(&SceneId::Note(person("ada"))).unwrap();
                assert_eq!(scene.nodes[chip].parent, Some(ada), "under its node");
            }
            note
        };
        assert_eq!(note(&state).unwrap().0, "Reading relationships…");
        for key in &groups {
            state.counts.insert(key.clone(), Loadable::Loaded(0));
        }
        assert_eq!(note(&state).unwrap().0, "No relationships");
        state
            .counts
            .insert(groups[0].clone(), Loadable::Failed("refused".into()));
        let (label, kind) = note(&state).unwrap();
        assert_eq!(label, "Couldn't read relationships · Retry");
        assert_eq!(
            kind,
            SceneKind::Note {
                node: person("ada"),
                note: Note::Failed(vec![LoadTarget::Count(groups[0].clone())]),
            }
        );
        state.counts.insert(groups[0].clone(), Loadable::Loaded(3));
        assert_eq!(note(&state), None, "a relationship drawn says it all");
    }

    /// An open node whose relationships the node cap left out says so, past the cap, and not
    /// "No relationships": here Bob's one pill, his employer, is cut by a cap of 3 (Ada, her
    /// Knows pill, Bob). Before, the pill was counted before the cap cut it, so Bob, open,
    /// said nothing at all.
    #[test]
    fn a_node_whose_relationships_the_cap_left_out_says_so() {
        let schema = SchemaModel::parse(&format!(
            "{SCHEMA}edge Employer: Person -> Company @card(0..1)\n"
        ))
        .unwrap();
        let group = |edge: &str, direction: Direction| {
            schema
                .relationships_of("Person")
                .iter()
                .find(|g| g.edge == edge && g.direction == direction)
                .cloned()
                .unwrap()
        };
        let (knows, employer) = (
            group("Knows", Direction::Outgoing),
            group("Employer", Direction::Outgoing),
        );
        let mut state = ExploreState::rooted(person("ada"));
        state.root_status = Loadable::Loaded(());
        for who in ["ada", "bob"] {
            for relationship in schema.relationships_of("Person") {
                state.counts.insert(
                    GroupKey::new(person(who), relationship.clone()),
                    Loadable::Loaded(0),
                );
            }
        }
        let ada_knows = GroupKey::new(person("ada"), knows.clone());
        state.counts.insert(ada_knows.clone(), Loadable::Loaded(1));
        let list = |node: NodeRef| NeighbourList {
            rows: vec![Neighbour {
                node,
                label: None,
                edge: Vec::new(),
            }],
            exhausted: true,
            ..Default::default()
        };
        state.neighbours.insert(ada_knows, list(person("bob")));
        state.neighbours.insert(
            GroupKey::new(person("ada"), employer.clone()),
            NeighbourList {
                exhausted: true,
                ..Default::default()
            },
        );
        state.neighbours.insert(
            GroupKey::new(person("bob"), employer),
            list(NodeRef::new("Company", "acme")),
        );
        let knows_path = ExplorePath::root().child_group(&knows);
        state.expanded.insert(knows_path.clone());
        state.expanded.insert(knows_path.child_node(&person("bob")));
        let scene = explore_scene(&state.entries(&schema), &state, &HashMap::new(), 3);
        let note = scene
            .nodes
            .iter()
            .find(|node| node.id == SceneId::Note(person("bob")))
            .expect("Bob says why nothing hangs off him");
        assert_eq!(note.label, "Relationships not drawn · over the node cap");
        assert!(
            scene.capped.is_some(),
            "the banner counts what the cap left out"
        );
    }

    /// An open relationship's "+K more" says "Reading…" until its first neighbours come, and
    /// "Couldn't read · Retry", with the page to read again, when they could not be read.
    /// Before, it said "+1500 more" either way, with no neighbours drawn.
    #[test]
    fn an_open_relationship_says_when_its_neighbours_are_read_or_failed() {
        let (schema, mut state) = exploration();
        let knows = schema
            .relationships_of("Person")
            .iter()
            .find(|g| g.edge == "Knows" && g.direction == Direction::Outgoing)
            .cloned()
            .unwrap();
        let key = GroupKey::new(person("ada"), knows);
        let more = |state: &ExploreState| {
            let scene = explore_scene(&state.entries(&schema), state, &HashMap::new(), node_cap());
            let index = scene.index_of(&SceneId::More(key.clone())).unwrap();
            (
                scene.nodes[index].label.clone(),
                scene.nodes[index].kind.clone(),
            )
        };
        assert_eq!(more(&state).0, "+12 more");
        let rows = state.neighbours.remove(&key).unwrap();
        assert_eq!(more(&state).0, "Reading…");
        state.neighbours.insert(
            key.clone(),
            NeighbourList {
                status: PageStatus::Failed("timed out".into()),
                ..Default::default()
            },
        );
        let (label, kind) = more(&state);
        assert_eq!(label, "Couldn't read · Retry");
        assert!(matches!(
            kind,
            SceneKind::More { note: Some(Note::Failed(targets)), .. }
                if targets == vec![LoadTarget::Page(key.clone())]
        ));
        state.neighbours.insert(key.clone(), rows);
        assert_eq!(more(&state).0, "+12 more");
    }

    /// A root that could not be read (no node with its key) draws no chip: the canvas's
    /// banner says why. Before, a chip of its key stood there, counted as "1 node".
    #[test]
    fn a_root_that_could_not_be_read_draws_no_chip() {
        let schema = SchemaModel::parse(SCHEMA).unwrap();
        let mut state = ExploreState::rooted(person("nobody"));
        state.root_status = Loadable::Failed("No Person with key nobody at this commit.".into());
        let scene = explore_scene(&state.entries(&schema), &state, &HashMap::new(), node_cap());
        assert!(scene.nodes.is_empty(), "{:?}", scene.nodes);
    }

    #[test]
    fn hubs_show_eight_then_more_and_draw_each_node_once() {
        let (schema, state) = exploration();
        let entries = state.entries(&schema);
        let scene = explore_scene(&entries, &state, &HashMap::new(), node_cap());
        let people = scene
            .nodes
            .iter()
            .filter(|n| matches!(n.kind, SceneKind::Node { .. }))
            .count();
        // Ada and eight neighbours; p05 is among them and drawn once even though p00 knows it.
        assert_eq!(people, 1 + HUB_STEP);
        let more = scene
            .nodes
            .iter()
            .find(|n| matches!(n.kind, SceneKind::More { .. }))
            .unwrap();
        assert_eq!(more.label, "+12 more");
        // The cycle back to Ada and the second path to p05 are edges, not nodes.
        let ada = scene.index_of(&SceneId::Node(person("ada"))).unwrap();
        let p05 = scene.index_of(&SceneId::Node(person("p05"))).unwrap();
        assert!(scene.edges.iter().any(|e| e.to == ada && e.from != ada));
        assert_eq!(scene.edges.iter().filter(|e| e.to == p05).count(), 2);

        let mut limits = HashMap::new();
        let key = scene
            .nodes
            .iter()
            .find_map(|n| match &n.kind {
                SceneKind::Hub { key, .. }
                    if key.node == person("ada")
                        && key.group.edge == "Knows"
                        && key.group.direction == Direction::Outgoing =>
                {
                    Some(key.clone())
                }
                _ => None,
            })
            .unwrap();
        limits.insert(key, HUB_STEP * 3);
        let wider = explore_scene(&entries, &state, &limits, node_cap());
        assert!(
            !wider
                .nodes
                .iter()
                .any(|n| matches!(n.kind, SceneKind::More { .. }))
        );
    }

    /// A hub the Explore tree searched holds the matches alone: it counts them once all are
    /// read, and offers no "+K more" of the relationship's other neighbours, which no page
    /// brings. Before, it showed the relationship's count beside three matches.
    #[test]
    fn a_searched_hub_counts_its_matches() {
        let (schema, mut state) = exploration();
        let knows = schema
            .relationships_of("Person")
            .iter()
            .find(|g| g.edge == "Knows" && g.direction == Direction::Outgoing)
            .cloned()
            .unwrap();
        let ada_knows = GroupKey::new(person("ada"), knows);
        state.searches.insert(ada_knows.clone(), "p1".into());
        let matches = NeighbourList {
            rows: ["p10", "p11", "p12"]
                .iter()
                .map(|id| Neighbour {
                    node: person(id),
                    label: None,
                    edge: Vec::new(),
                })
                .collect(),
            exhausted: true,
            ..Default::default()
        };
        state.neighbours.insert(ada_knows.clone(), matches);
        let scene = explore_scene(&state.entries(&schema), &state, &HashMap::new(), node_cap());
        let hub = scene.index_of(&SceneId::Hub(ada_knows.clone())).unwrap();
        assert!(matches!(
            scene.nodes[hub].kind,
            SceneKind::Hub { count: Some(3), .. }
        ));
        assert!(scene.index_of(&SceneId::More(ada_knows)).is_none());
    }

    /// p05, reached from Ada and from p00, is one chip, open while either path is: closing it
    /// closes both, so it does not stay open by the path not drawn.
    #[test]
    fn a_node_reached_twice_is_closed_by_both_paths() {
        let (schema, mut state) = exploration();
        let knows = schema
            .relationships_of("Person")
            .iter()
            .find(|g| g.edge == "Knows" && g.direction == Direction::Outgoing)
            .cloned()
            .unwrap();
        let from_ada = ExplorePath::root()
            .child_group(&knows)
            .child_node(&person("p05"));
        let from_p00 = ExplorePath::root()
            .child_group(&knows)
            .child_node(&person("p00"))
            .child_group(&knows)
            .child_node(&person("p05"));
        let p05 = SceneId::Node(person("p05"));
        let opened = |state: &ExploreState| {
            let scene = explore_scene(&state.entries(&schema), state, &HashMap::new(), node_cap());
            let chip = &scene.nodes[scene.index_of(&p05).unwrap()];
            matches!(chip.kind, SceneKind::Node { expanded: true, .. })
        };
        state.expand(&from_p00);
        assert!(opened(&state), "open by the path not drawn first");
        state.expand(&from_ada);
        let mut paths = open_paths(&state.entries(&schema), &state, std::slice::from_ref(&p05));
        paths.sort_by_key(|path| format!("{path:?}"));
        let mut both = vec![from_ada, from_p00];
        both.sort_by_key(|path| format!("{path:?}"));
        assert_eq!(paths, both);
        for path in &paths {
            state.collapse(path);
        }
        assert!(!opened(&state));
        assert!(open_paths(&state.entries(&schema), &state, &[p05]).is_empty());
    }

    /// Bob is reached directly and through Cid; his employer, opened by the second path only,
    /// is one hub that shows open, with the company it reaches.
    #[test]
    fn a_single_target_hub_reached_twice_is_open_by_either_path() {
        let schema = SchemaModel::parse(&format!(
            "{SCHEMA}edge Employer: Person -> Company @card(0..1)\n"
        ))
        .unwrap();
        let group = |edge: &str| {
            schema
                .relationships_of("Person")
                .iter()
                .find(|g| g.edge == edge && g.direction == Direction::Outgoing)
                .cloned()
                .unwrap()
        };
        let (knows, employer) = (group("Knows"), group("Employer"));
        let (ada, bob, cid) = (person("ada"), person("bob"), person("cid"));
        let acme = NodeRef::new("Company", "acme");
        let list = |nodes: Vec<NodeRef>| NeighbourList {
            rows: nodes
                .into_iter()
                .map(|node| Neighbour {
                    node,
                    label: None,
                    edge: Vec::new(),
                })
                .collect(),
            exhausted: true,
            ..Default::default()
        };
        let mut state = ExploreState::rooted(ada.clone());
        state.root_status = Loadable::Loaded(());
        // A hub is drawn once counted.
        let ada_knows_key = GroupKey::new(ada, knows.clone());
        state
            .counts
            .insert(ada_knows_key.clone(), Loadable::Loaded(2));
        state
            .neighbours
            .insert(ada_knows_key, list(vec![bob.clone(), cid.clone()]));
        let cid_knows_key = GroupKey::new(cid.clone(), knows.clone());
        state
            .counts
            .insert(cid_knows_key.clone(), Loadable::Loaded(1));
        state
            .neighbours
            .insert(cid_knows_key, list(vec![bob.clone()]));
        let key = GroupKey::new(bob.clone(), employer.clone());
        state.counts.insert(key.clone(), Loadable::Loaded(1));
        state
            .neighbours
            .insert(key.clone(), list(vec![acme.clone()]));
        let ada_knows = ExplorePath::root().child_group(&knows);
        let direct = ada_knows.child_node(&bob);
        let cid_knows = ada_knows.child_node(&cid).child_group(&knows);
        let via_cid = cid_knows.child_node(&bob);
        for path in [
            ada_knows.clone(),
            direct,
            ada_knows.child_node(&cid),
            cid_knows,
            via_cid.clone(),
            via_cid.child_group(&employer),
        ] {
            state.expanded.insert(path);
        }
        let scene = explore_scene(&state.entries(&schema), &state, &HashMap::new(), node_cap());
        let hub = scene.index_of(&SceneId::Hub(key)).unwrap();
        assert!(matches!(
            scene.nodes[hub].kind,
            SceneKind::Hub { open: true, .. }
        ));
        assert!(scene.index_of(&SceneId::Node(acme)).is_some());
    }

    #[test]
    fn the_cap_stops_drawing_and_says_how_many_are_left_out() {
        let (schema, state) = exploration();
        let entries = state.entries(&schema);
        let mut limits = HashMap::new();
        for entry in &entries {
            if let EntryKind::Group { key, .. } = &entry.kind {
                limits.insert(key.clone(), 100);
            }
        }
        let scene = explore_scene(&entries, &state, &limits, 6);
        assert!(scene.nodes.len() <= 6);
        let (hidden, type_name) = scene.capped.clone().unwrap();
        assert!(hidden > 0);
        assert_eq!(type_name, "Person");
    }

    /// At a cap that leaves only Ada drawn, the banner counts what it left out: Ada's three
    /// counted relationship chip (the two not counted yet are not drawn, so not left out), the
    /// 20 people under it (p05 once, though p00 reaches it too), and p00's counted chip.
    #[test]
    fn the_cap_counts_relationships_and_each_node_once() {
        let (schema, state) = exploration();
        let scene = explore_scene(&state.entries(&schema), &state, &HashMap::new(), 1);
        // The cap holds every chip but the note of an open node it left with none drawn,
        // which says why and stands past it.
        let (notes, chips): (Vec<_>, Vec<_>) = scene
            .nodes
            .iter()
            .partition(|node| matches!(node.kind, SceneKind::Note { .. }));
        assert_eq!(chips.len(), 1);
        assert_eq!(notes.len(), 1);
        assert_eq!(
            notes[0].label,
            "Relationships not drawn · over the node cap"
        );
        assert_eq!(scene.capped, Some((1 + 20 + 1, "Person".into())));
    }
}

//! The node view (specs 004, 014): one row's properties, copyable, and for a node its
//! relationship groups with counts and neighbours. It shows as the peek in the inspector, and
//! at full size as a node's page in a tab of its own. Every read is pinned to the commit the row
//! was read at, or to the first read when the node was opened by id (constitution II).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use editor::{Editor, EditorEvent};
use gpui::{
    App, ClipboardItem, Context, Entity, Focusable as _, ListAlignment, ListState, Render,
    SharedString, Subscription, Task, Window, list,
};
use gq_language::builder::Depends;
use gq_language::input::{edit_text, parse_input};
use gq_language::{BuiltQuery, Direction, RelationshipGroup, SchemaModel, builder};
use graph_store::scope::{ReadScope, ScopeSince};
use graph_store::{DraftEntry, DraftKey, GraphSession, Loadable, SessionEvent};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use serde_json::{Map, Value};
use studio_actions::graph::{OpenBlob, OpenNodePage, PeekNode};
use studio_actions::node::{CancelField, ExpandField, SaveField, SaveNodeDrafts};
use studio_actions::table::{SelectNextSuggestion, SelectPreviousSuggestion};
use studio_ui::ds::space;
use studio_ui::menu::ContextMenu;
use ui::prelude::*;
use ui::{Indicator, PopoverMenu, ToggleState, Tooltip};

use crate::blob::{
    IMAGE_MAX, PREVIEW_PREFIX, Preview, bytes_wanted, preview, save_value, suggested_file_name,
};
use crate::model::{
    FROM_COLUMN, Notice, SaveResult, TO_COLUMN, Tone, display_value, field_rows, format_count,
    node_rows, reads_as_markdown, same_value, save_result,
};
use crate::suggest;
use graph_client::api::ReadOutput;
use graph_client::{BlobCell, BlobMeta, HistoricalGap, PropertyThen, ReadError};

/// First neighbours shown under an expanded relationship group.
pub const NEIGHBOUR_LIMIT: u32 = 8;

/// Neighbours a page reads at a time under each relationship group, with "Show more" for the
/// next (constitution IX).
/// Where a relationship group's children start: under the group's arrow, past its inset and
/// disclosure chevron.
const CHILD_INDENT: f32 = space::XS + studio_ui::ds::size::DISCLOSURE + space::SM;
const PAGE_NEIGHBOURS: u32 = 50;

/// The widest a page's text runs: the design system's reading width, as settings' is.
const PAGE_WIDTH: f32 = studio_ui::ds::size::READING_WIDTH;

/// Debounce before relationship counts are read, so moving the cursor quickly through rows
/// does not send a burst of reads.
const COUNT_DELAY: Duration = Duration::from_millis(120);
/// What a property the node holds no value for shows, borrowed like a value it holds.
static NULL: Value = Value::Null;

/// How far past the rows in view the view's list lays rows out. A group is read once its row
/// is laid out, so this is how far ahead of a scroll the groups are read: a few rows.
const DRAW_AHEAD: f32 = 160.;

/// One item of the view's list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    /// Everything above the relationship groups, ending with the Relationships heading.
    Head,
    /// "This type takes part in no relationships".
    Note(&'static str),
    Group(usize),
    /// The row the groups with no neighbours fold into.
    Empty,
}

/// Where a relationship group's row sits in the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    /// Among the groups listed first: known to have neighbours, its count failed, or the
    /// person is working in it.
    Listed,
    /// Among the groups counted only once their rows are drawn, in schema order after the
    /// listed ones. It stays there whatever it holds, so a count arriving moves no row in view.
    AsDrawn,
    /// Under the empty row: counted before the view laid it out, and known to have none.
    Folded,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PeekSubject {
    Node {
        type_name: String,
        id: String,
    },
    Edge {
        edge_type: String,
        id: String,
        from_type: String,
        to_type: String,
    },
}

impl PeekSubject {
    fn type_name(&self) -> &str {
        match self {
            PeekSubject::Node { type_name, .. } => type_name,
            PeekSubject::Edge { edge_type, .. } => edge_type,
        }
    }

    fn id(&self) -> &str {
        match self {
            PeekSubject::Node { id, .. } | PeekSubject::Edge { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Neighbour {
    pub id: String,
    pub label: Option<String>,
    /// The edge that relates it, which removing the relationship names; `None` when it was
    /// read without it (a table's chips).
    pub edge_id: Option<String>,
}

/// What the table already read about one of the row's relationship groups, at the peek's
/// commit: the count, and the first neighbours when it read the row's whole list. The peek
/// shows them rather than reading them again.
#[derive(Debug, Clone, PartialEq)]
pub struct KnownRelation {
    pub count: i64,
    /// The first `min(count, NEIGHBOUR_LIMIT)` neighbours by id, as the peek's own read
    /// orders them; `None` when the table did not read them all.
    pub first: Option<Vec<Neighbour>>,
}

struct Group {
    group: RelationshipGroup,
    count: Loadable<i64>,
    /// The view laid the group out without its count: it is read once its row is drawn, and
    /// keeps its place among such groups (`Place::AsDrawn`).
    as_drawn: bool,
    /// Its row was drawn: a group not counted yet is read then, and a small one opens.
    drawn: bool,
    expanded: bool,
    neighbours: Loadable<Vec<Neighbour>>,
    /// The next page of neighbours being read (a page's "Show more").
    more: Option<Task<()>>,
    _task: Option<Task<()>>,
}

/// One Blob property of the peeked row: what it holds, and its preview.
struct BlobSlot {
    property: String,
    meta: Loadable<BlobMeta>,
    preview: Loadable<Preview>,
    /// The value's first bytes, to name a saved file for its kind.
    head: Vec<u8>,
    save: Option<SaveState>,
    _task: Option<Task<()>>,
    _save: Option<Task<()>>,
}

/// One property being edited in place (spec 014): its editor, the value it was read at, which
/// the save is bound to, and why the last save did not land.
struct FieldEdit {
    property: gq_language::Property,
    old: Value,
    editor: Entity<Editor>,
    notice: Option<Notice>,
    _blur: Subscription,
}

/// Adding a relationship from the node: the search over the other type and what it found.
struct AddEdge {
    /// The relationship group it adds to.
    index: usize,
    editor: Entity<Editor>,
    found: Loadable<Vec<Neighbour>>,
    /// The text `found` answers: what was typed when they were read.
    answers: Option<String>,
    highlighted: usize,
    _search: Option<Task<()>>,
    _changed: Subscription,
}

/// One relationship written from the node.
enum EdgeChange {
    /// To the node with this id.
    Add(String),
    /// The edge with this id.
    Remove(String),
}

/// Saving a value to a file: how far it got, or how it ended.
#[derive(Debug, Clone)]
enum SaveState {
    Saving { done: u64, of: u64 },
    Saved(std::path::PathBuf),
    Failed(String),
}

pub struct NodePeek {
    session: Entity<GraphSession>,
    /// The branch the view reads: the session's, since the view last followed it there.
    branch: String,
    blobs: Vec<BlobSlot>,
    schema: Option<Arc<SchemaModel>>,
    subject: PeekSubject,
    snapshot: Option<String>,
    values: Loadable<Map<String, Value>>,
    groups: Vec<Group>,
    /// The table's relationship reads for this row, by `builder::relationship_key`.
    known: HashMap<String, KnownRelation>,
    copied: Option<String>,
    /// The branch's head moved past the commit the peek read: what it shows may be older.
    newer: bool,
    /// The peek shows a commit chosen on purpose (a table opened at a commit), not the head.
    holds_commit: bool,
    /// Drawn as a node's page rather than the inspector's peek.
    page: bool,
    /// The peek follows a moving cursor: a group drawn waits a moment before it is read, so
    /// rows the cursor passes read nothing.
    debounce: bool,
    /// Text values with markup, rendered as markdown: by property, the text and its view.
    markdown: HashMap<String, (String, Entity<Markdown>)>,
    /// Text values found to read as themselves, by property: not parsed again each frame.
    plain_text: HashMap<String, String>,
    /// Properties being edited; a changed one stays open until it is saved or cancelled.
    edits: HashMap<String, FieldEdit>,
    /// Properties whose write is out, shown with their new value and a dot.
    saving: HashSet<String>,
    /// A relationship being added.
    adding: Option<AddEdge>,
    /// The edge whose × was pressed once: the next press removes it.
    removing: Option<String>,
    /// Relationship groups with a write out.
    edge_writes: HashSet<usize>,
    /// Why a group's last write did not land.
    group_notices: HashMap<usize, Notice>,
    /// The relationship groups with no neighbours are listed too; by default they fold into
    /// one row at the end.
    show_empty: bool,
    /// The node's drafts being saved together.
    saving_drafts: bool,
    /// Why the drafts' last save did not land; the second press of Discard drops them.
    drafts_notice: Option<Notice>,
    confirm_discard: bool,
    /// What the view shows, as read (spec 015): the session decides from it whether a head move
    /// changed anything shown. `None` while the view holds a commit.
    scope: Option<ReadScope>,
    /// The view draws as a list, a page and the peek alike: its head, then each relationship
    /// group shown. Only what is in view is laid out, and a group is read once its row is, so
    /// a node with dozens of groups neither draws nor reads them all.
    list: ListState,
    list_items: Vec<Item>,
    /// Properties the commit shown does not hold under their current name, and what they were
    /// then. The node is read without them.
    absent: HashMap<String, PropertyThen>,
    /// Properties relationship groups read (a neighbour's label, an edge's properties) that the
    /// commit shown does not hold, by owner type and property: groups are read without them.
    groups_absent: Vec<(String, String)>,
    /// The head moved and the session is reading what changed: the view decides when it is in.
    awaiting_changes: bool,
    /// The node's read: a newer one replaces it, so an older answer never lands over it.
    _node_read: Option<Task<()>>,
    /// Takes the copied mark down; a refresh keeps it, and a second copy starts it again.
    _copied: Option<Task<()>>,
    _subscription: Subscription,
}

impl NodePeek {
    /// A peek of a row the caller already holds (`values`) read at `snapshot`, or of a node
    /// opened by id (`values` is `None`), which is read first. `known` holds what the caller
    /// read about the row's relationships at `snapshot`; the rest is read. `debounce` delays
    /// the relationship counts, for a peek that follows a moving cursor.
    pub fn new(
        session: Entity<GraphSession>,
        subject: PeekSubject,
        values: Option<Map<String, Value>>,
        snapshot: Option<String>,
        known: HashMap<String, KnownRelation>,
        debounce: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(
            session, subject, values, snapshot, known, debounce, false, cx,
        )
    }

    /// A node's page: the node read by id at `at_commit`, held there, or at the branch head.
    pub fn page(
        session: Entity<GraphSession>,
        type_name: String,
        id: String,
        at_commit: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let holds = at_commit.is_some();
        let subject = PeekSubject::Node { type_name, id };
        let mut page = Self::build(
            session,
            subject,
            None,
            at_commit,
            HashMap::new(),
            false,
            true,
            cx,
        );
        if holds {
            page.hold_commit();
        }
        page
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        session: Entity<GraphSession>,
        subject: PeekSubject,
        values: Option<Map<String, Value>>,
        snapshot: Option<String>,
        known: HashMap<String, KnownRelation>,
        debounce: bool,
        page: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let schema = session.read(cx).schema_model();
        let subscription = cx.subscribe(&session, |this: &mut Self, session, event, cx| {
            // Its node's drafts are the session's, kept from a table or another view too.
            if let SessionEvent::DraftsChanged = event {
                cx.notify();
                return;
            }
            // A view made before the schema was read (a page restored at start) reads its node
            // once the schema is in.
            if let SessionEvent::SchemaChanged = event
                && this.schema.is_none()
            {
                this.schema = session.read(cx).schema_model();
                if this.schema.is_some() && this.values.loaded().is_none() {
                    this.load_node(cx);
                }
                cx.notify();
                return;
            }
            // Another branch: the node is read there, as a table reads its rows again. What was
            // being edited was the branch left's; its drafts wait for it in the session.
            if let SessionEvent::BranchChanged = event
                && !this.holds_commit
                && session.read(cx).branch != this.branch
            {
                this.branch = session.read(cx).branch.clone();
                this.edits.clear();
                this.adding = None;
                this.removing = None;
                this.group_notices.clear();
                this.drafts_notice = None;
                this.confirm_discard = false;
                this.refresh(cx);
                return;
            }
            // Another writer changed the schema: the view stays as read until Refresh.
            if let SessionEvent::SchemaMoved = event
                && !this.holds_commit
            {
                this.newer = true;
                cx.notify();
                return;
            }
            // While its own writes are out the view compares when they end: the head may be its
            // own commit.
            let moved = match event {
                SessionEvent::HeadMoved { branch, .. } => Some(branch),
                SessionEvent::ChangesRead { branch } if this.awaiting_changes => Some(branch),
                _ => None,
            };
            if moved.is_some_and(|branch| *branch == session.read(cx).branch)
                && this.saving.is_empty()
                && this.edge_writes.is_empty()
            {
                this.compare_head(cx);
            }
        });
        cx.on_release(|this: &mut Self, cx| this.release(cx))
            .detach();
        let branch = session.read(cx).branch.clone();
        let scope = session.update(cx, |session, _| session.read_scope(&branch));
        let mut this = Self {
            session,
            branch,
            blobs: Vec::new(),
            schema,
            subject,
            snapshot,
            values: Loadable::NotLoaded,
            groups: Vec::new(),
            known,
            copied: None,
            newer: false,
            holds_commit: false,
            page,
            debounce,
            markdown: HashMap::new(),
            plain_text: HashMap::new(),
            edits: HashMap::new(),
            saving: HashSet::new(),
            adding: None,
            removing: None,
            edge_writes: HashSet::new(),
            group_notices: HashMap::new(),
            saving_drafts: false,
            drafts_notice: None,
            confirm_discard: false,
            show_empty: false,
            scope: Some(scope),
            list: ListState::new(0, ListAlignment::Top, px(DRAW_AHEAD)),
            list_items: Vec::new(),
            absent: HashMap::new(),
            groups_absent: Vec::new(),
            awaiting_changes: false,
            _node_read: None,
            _copied: None,
            _subscription: subscription,
        };
        match values {
            Some(values) => {
                // The row the caller read at the commit is what the view shows of the node.
                let depends = match &this.subject {
                    PeekSubject::Node { type_name, id } => Depends::Node {
                        type_name: type_name.clone(),
                        id: id.clone(),
                    },
                    PeekSubject::Edge { edge_type, .. } => Depends::Types {
                        nodes: Vec::new(),
                        edges: vec![edge_type.clone()],
                    },
                };
                this.note_shown(depends, None, cx);
                this.values = Loadable::Loaded(values);
                this.load_groups(cx);
                this.load_blobs(debounce, cx);
            }
            None => this.load_node(cx),
        }
        this
    }

    /// What names the node: its label, or its id until it is read or when it has none.
    pub fn title(&self) -> String {
        self.label().unwrap_or_else(|| self.subject.id().to_owned())
    }

    /// The branch moved past the commit shown.
    pub fn is_newer(&self) -> bool {
        self.newer
    }

    /// The commit a node opened from this view is read at: the one it holds, else the head.
    fn held_commit(&self) -> Option<String> {
        self.holds_commit.then(|| self.snapshot.clone()).flatten()
    }

    /// Opening this node's page, at the commit the view holds or the head; `None` for an edge.
    fn open_page_action(&self) -> Option<OpenNodePage> {
        let PeekSubject::Node { type_name, id } = &self.subject else {
            return None;
        };
        Some(OpenNodePage {
            type_name: type_name.clone(),
            id: id.clone(),
            at_commit: self.held_commit(),
        })
    }

    /// Neighbours read at a time under a group.
    fn neighbour_limit(&self) -> u32 {
        if self.page {
            PAGE_NEIGHBOURS
        } else {
            NEIGHBOUR_LIMIT
        }
    }

    /// Whether a group with `count` neighbours opens by itself: a small one in the peek, every
    /// one that has any on a page.
    fn opens_by_itself(&self, count: i64) -> bool {
        count > 0 && (self.page || count <= i64::from(NEIGHBOUR_LIMIT))
    }

    fn label(&self) -> Option<String> {
        let values = self.values.loaded()?;
        if let PeekSubject::Edge { .. } = &self.subject {
            let from = values.get(FROM_COLUMN)?.as_str()?;
            let to = values.get(TO_COLUMN)?.as_str()?;
            return Some(format!("{from} → {to}"));
        }
        let schema = self.schema.as_ref()?;
        let node = schema.node(self.subject.type_name())?;
        let label = node.label_property()?;
        (Some(label) != node.key.as_deref())
            .then(|| values.get(label)?.as_str().map(str::to_owned))
            .flatten()
    }

    /// The peek shows the commit it was given whatever the branch does: no Refresh is offered.
    pub fn hold_commit(&mut self) {
        self.holds_commit = true;
        self.newer = false;
        // A held commit never moves, so nothing the branch does concerns it.
        self.scope = None;
        self.awaiting_changes = false;
    }

    /// Reads a node again at the branch head, with its relationships and Blobs, under the
    /// session's schema.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let schema = self
            .session
            .read(cx)
            .schema_model()
            .or_else(|| self.schema.clone());
        self.schema = schema;
        self.newer = false;
        self.snapshot = None;
        // What the table read belongs to the old commit.
        self.known.clear();
        self.groups.clear();
        self.absent.clear();
        self.groups_absent.clear();
        // What it showed is let go; what it reads now is told again, under the new schema too.
        if let Some(scope) = &self.scope {
            self.session.update(cx, |session, _| {
                let branch = session.branch.clone();
                session.reset_scope(scope, &branch);
            });
        }
        self.load_node(cx);
        cx.notify();
    }

    fn load_node(&mut self, cx: &mut Context<Self>) {
        let PeekSubject::Node { type_name, id } = &self.subject else {
            return;
        };
        let Some(schema) = self.schema.clone() else {
            self.values = Loadable::Failed("The schema is still loading.".into());
            return;
        };
        // At a commit made before a property was added or renamed, the whole node is refused:
        // the rest of it is read as fields.
        let fields = !self.absent.is_empty();
        let query = if fields {
            let without: Vec<String> = self.absent.keys().cloned().collect();
            builder::node_fields_by_id(&schema, type_name, id, &without)
        } else {
            builder::node_by_id(&schema, type_name, id)
        };
        let query = match query {
            Ok(query) => query,
            Err(error) => {
                self.values = Loadable::Failed(error.to_string());
                return;
            }
        };
        let type_name = type_name.clone();
        self.values = Loadable::Loading;
        // At the head the session knows, a node kept there answers at once: a table's page
        // read it already.
        let snapshot = self.snapshot.clone().or_else(|| {
            let session = self.session.read(cx);
            session.head(&session.branch).map(str::to_owned)
        });
        let read = self.read_shown(&query, snapshot, cx);
        self._node_read = Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(output) => {
                        if this.snapshot.is_none() {
                            this.snapshot = output.graph_commit_id.clone();
                        }
                        let rows = if fields {
                            field_rows(&output.rows)
                        } else {
                            node_rows(&output.rows)
                        };
                        let values = rows.into_iter().next().map(|row| row.values);
                        match values {
                            Some(values) => {
                                this.values = Loadable::Loaded(values);
                                this.load_groups(cx);
                                this.load_blobs(false, cx);
                            }
                            None => {
                                this.values = Loadable::Failed(
                                    "This node does not exist at this commit.".into(),
                                );
                            }
                        }
                    }
                    Err(ReadError::Refused(refusal)) => match refusal.historical_gap() {
                        Some(HistoricalGap::Property {
                            owner,
                            property,
                            then,
                        }) if owner == type_name
                            && !this.absent.contains_key(&property)
                            && this
                                .schema
                                .as_ref()
                                .and_then(|schema| schema.node(&type_name))
                                .is_some_and(|node| node.property(&property).is_some()) =>
                        {
                            // Once per property, so this ends.
                            this.absent.insert(property, then);
                            this.load_node(cx);
                        }
                        Some(HistoricalGap::Type { name }) if name == type_name => {
                            this.values = Loadable::Failed(format!(
                                "{type_name} did not exist at this commit."
                            ));
                        }
                        _ => this.values = Loadable::Failed(refusal.message),
                    },
                    Err(error) => this.values = Loadable::Failed(error.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// The subject's Blob properties.
    fn blob_properties(&self) -> Vec<String> {
        let Some(schema) = &self.schema else {
            return Vec::new();
        };
        let properties = match &self.subject {
            PeekSubject::Node { type_name, .. } => schema.node(type_name).map(|n| &n.properties),
            PeekSubject::Edge { edge_type, .. } => schema.edge(edge_type).map(|e| &e.properties),
        };
        properties
            .into_iter()
            .flatten()
            .filter(|p| {
                p.kind == gq_language::ScalarKind::Blob && !self.absent.contains_key(&p.name)
            })
            .map(|p| p.name.clone())
            .collect()
    }

    /// Frees the images this peek decoded; GPUI would keep them otherwise.
    /// The peek goes, as when the table's cursor moves on: what its open fields hold is kept
    /// as drafts on its branch, a refused field's too, and a Blob being saved to a file still
    /// lands there.
    fn release(&mut self, cx: &mut App) {
        self.release_images(cx);
        for slot in &mut self.blobs {
            if let Some(save) = slot._save.take() {
                save.detach();
            }
        }
        for (name, edit) in std::mem::take(&mut self.edits) {
            let text = edit.editor.read(cx).text(cx);
            let unchanged =
                parse_input(&edit.property, &text).is_ok_and(|value| same_value(&value, &edit.old));
            if let Some(key) = self.draft_key(&name).filter(|_| !unchanged) {
                let entry = DraftEntry {
                    old: edit.old,
                    text,
                };
                self.session.update(cx, |session, cx| {
                    session.set_draft_on(&self.branch, key, entry, cx)
                });
            }
        }
    }

    fn release_images(&self, cx: &mut App) {
        for slot in &self.blobs {
            if let Loadable::Loaded(Preview::Image(image)) = &slot.preview {
                image.release(cx);
            }
        }
    }

    /// Reads each Blob property's metadata, then enough of a managed value to preview it: its
    /// first bytes, or the whole of an image that fits. Pinned to the peek's commit.
    fn load_blobs(&mut self, debounce: bool, cx: &mut Context<Self>) {
        self.release_images(cx);
        let edge = matches!(self.subject, PeekSubject::Edge { .. });
        let type_name = self.subject.type_name().to_owned();
        let id = self.subject.id().to_owned();
        self.blobs = self
            .blob_properties()
            .into_iter()
            .map(|property| BlobSlot {
                property,
                meta: Loadable::Loading,
                preview: Loadable::NotLoaded,
                head: Vec::new(),
                save: None,
                _task: None,
                _save: None,
            })
            .collect();
        for index in 0..self.blobs.len() {
            let cell = BlobCell {
                edge,
                type_name: type_name.clone(),
                id: id.clone(),
                property: self.blobs[index].property.clone(),
            };
            let session = self.session.downgrade();
            let snapshot = self.snapshot.clone();
            let scope = self.scope.clone();
            self.blobs[index]._task = Some(cx.spawn(async move |this, cx| {
                if debounce {
                    cx.background_executor().timer(COUNT_DELAY).await;
                }
                let Ok(read) = session.update(cx, |session, cx| match &scope {
                    Some(scope) => session.blob_meta_in(scope, cell.clone(), snapshot.clone(), cx),
                    None => session.blob_meta(cell.clone(), snapshot.clone(), cx),
                }) else {
                    return;
                };
                let meta = read.await;
                let size = match &meta {
                    Ok(BlobMeta::Managed { size, .. }) => Some(*size),
                    _ => None,
                };
                this.update(cx, |this, cx| {
                    this.blobs[index].meta = match meta {
                        Ok(meta) => Loadable::Loaded(meta),
                        Err(error) => Loadable::Failed(error.to_string()),
                    };
                    if size.is_some() {
                        this.blobs[index].preview = Loadable::Loading;
                    }
                    cx.notify();
                })
                .ok();
                let Some(size) = size else { return };
                let mut bytes = Vec::new();
                let mut wanted = size.min(PREVIEW_PREFIX);
                // The prefix, then the rest of an image that fits.
                while (bytes.len() as u64) < wanted {
                    let from = bytes.len() as u64;
                    let Ok(read) = session.update(cx, |session, cx| {
                        session.blob_bytes(cell.clone(), snapshot.clone(), from..wanted, cx)
                    }) else {
                        return;
                    };
                    match read.await {
                        Ok(chunk) if !chunk.is_empty() => bytes.extend(chunk),
                        Ok(_) => break,
                        Err(error) => {
                            this.update(cx, |this, cx| {
                                this.blobs[index].preview = Loadable::Failed(error.to_string());
                                cx.notify();
                            })
                            .ok();
                            return;
                        }
                    }
                    if from == 0 {
                        wanted = bytes_wanted(&bytes, size).min(IMAGE_MAX);
                    }
                }
                let head = bytes.iter().take(16).copied().collect();
                let svg = cx.update(|cx| cx.svg_renderer());
                let shown = cx
                    .background_spawn(async move { preview(bytes, size, &svg) })
                    .await;
                this.update(cx, |this, cx| {
                    this.blobs[index].head = head;
                    this.blobs[index].preview = Loadable::Loaded(shown);
                    cx.notify();
                })
                .ok();
            }));
        }
    }

    /// Saves a managed value to a file the person picks: read in ranges at the peek's commit and
    /// written to a file beside it, renamed into place when complete, so memory holds one range
    /// and a stopped save leaves no partial file under the chosen name.
    fn save_blob(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(slot) = self.blobs.get(index) else {
            return;
        };
        let Loadable::Loaded(BlobMeta::Managed { size, .. }) = slot.meta else {
            return;
        };
        let id = self.subject.id().to_owned();
        let name = suggested_file_name(&id, &slot.property, &slot.head);
        let directory = std::env::home_dir()
            .map(|home| home.join("Downloads"))
            .filter(|dir| dir.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        let chosen = cx.prompt_for_new_path(&directory, Some(&name));
        let cell = BlobCell {
            edge: matches!(self.subject, PeekSubject::Edge { .. }),
            type_name: self.subject.type_name().to_owned(),
            id,
            property: slot.property.clone(),
        };
        let session = self.session.downgrade();
        let snapshot = self.snapshot.clone();
        self.blobs[index]._save = Some(cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = chosen.await else {
                return;
            };
            let set = |state: SaveState, cx: &mut gpui::AsyncApp| {
                this.update(cx, |this, cx| {
                    if let Some(slot) = this.blobs.get_mut(index) {
                        slot.save = Some(state);
                    }
                    cx.notify();
                })
                .ok();
            };
            set(SaveState::Saving { done: 0, of: size }, cx);
            let saved = save_value(session, cell, snapshot, size, path, cx, |done, cx| {
                set(SaveState::Saving { done, of: size }, cx)
            })
            .await;
            set(
                match saved {
                    Ok(path) => SaveState::Saved(path),
                    Err(error) => SaveState::Failed(error),
                },
                cx,
            );
        }));
        cx.notify();
    }

    fn render_blobs(&self, cx: &mut Context<Self>) -> AnyElement {
        let border = studio_ui::ds::surface::rule(cx);
        v_flex()
            .gap(px(space::MD))
            .children(self.blobs.iter().enumerate().map(|(index, slot)| {
                let facts = match &slot.meta {
                    Loadable::Loaded(BlobMeta::Managed { size, .. }) => {
                        studio_ui::format_bytes(*size)
                    }
                    Loadable::Loaded(BlobMeta::External { .. }) => "external reference".into(),
                    Loadable::Loaded(BlobMeta::Null) => "null".into(),
                    Loadable::Failed(_) => "could not be read".into(),
                    _ => "reading…".into(),
                };
                let body: Option<AnyElement> = match (&slot.meta, &slot.preview) {
                    (Loadable::Failed(error), _) | (_, Loadable::Failed(error)) => {
                        Some(studio_ui::ds::text::error(error.clone()).into_any_element())
                    }
                    (Loadable::Loaded(BlobMeta::External { uri }), _) => Some(
                        v_flex()
                            .gap(px(space::XS))
                            .child(studio_ui::CopyableValue::new(
                                ("blob-uri", index),
                                Some(uri.clone()),
                            ))
                            .child(studio_ui::ds::text::meta(
                                "Studio does not fetch objects outside the graph.",
                            ))
                            .into_any_element(),
                    ),
                    (_, Loadable::Loaded(Preview::Empty)) => {
                        Some(studio_ui::ds::text::muted("Empty value").into_any_element())
                    }
                    (_, Loadable::Loaded(Preview::Image(image))) => Some(
                        div()
                            .w_full()
                            .flex()
                            .justify_center()
                            .p(px(space::MD))
                            .rounded(px(studio_ui::ds::radii(cx).md))
                            .border_1()
                            .border_color(border)
                            .child(
                                gpui::img(image.source())
                                    .max_w_full()
                                    .max_h(px(studio_ui::ds::size::PREVIEW_MAX_HEIGHT))
                                    .object_fit(gpui::ObjectFit::Contain),
                            )
                            .into_any_element(),
                    ),
                    (_, Loadable::Loaded(Preview::Text { text, more })) => Some(
                        v_flex()
                            .gap(px(space::XS))
                            .child(studio_ui::ds::code_block(text.clone(), cx))
                            .when(*more > 0, |d| {
                                d.child(studio_ui::ds::text::meta(format!(
                                    "{} more",
                                    studio_ui::format_bytes(*more)
                                )))
                            })
                            .into_any_element(),
                    ),
                    (_, Loadable::Loaded(Preview::Bytes { hex, more })) => Some(
                        v_flex()
                            .gap(px(space::XS))
                            .child(studio_ui::ds::code_block(hex.clone(), cx))
                            .when(*more > 0, |d| {
                                d.child(studio_ui::ds::text::meta(format!(
                                    "{} more",
                                    studio_ui::format_bytes(*more)
                                )))
                            })
                            .into_any_element(),
                    ),
                    _ => None,
                };
                let managed = matches!(slot.meta, Loadable::Loaded(BlobMeta::Managed { .. }));
                let save: Option<AnyElement> =
                    match &slot.save {
                        Some(SaveState::Saving { done, of }) => Some(
                            studio_ui::ds::text::meta(format!(
                                "Saving {}%",
                                (*done * 100).checked_div(*of).unwrap_or(100)
                            ))
                            .into_any_element(),
                        ),
                        Some(SaveState::Saved(path)) => {
                            let path = path.clone();
                            Some(
                                studio_ui::ds::button(("show-saved", index), "Show in Finder")
                                    .style(ButtonStyle::Subtle)
                                    .color(Color::Muted)
                                    .tooltip(Tooltip::text(format!("Saved to {}", path.display())))
                                    .on_click(move |_, _, cx| cx.reveal_path(&path))
                                    .into_any_element(),
                            )
                        }
                        Some(SaveState::Failed(error)) => {
                            Some(studio_ui::ds::text::error_note(error.clone()).into_any_element())
                        }
                        None if managed => Some(
                            h_flex()
                                .gap(px(space::XXS))
                                .child({
                                    let open = OpenBlob {
                                        type_name: self.subject.type_name().to_owned(),
                                        id: self.subject.id().to_owned(),
                                        property: slot.property.clone(),
                                        edge: matches!(self.subject, PeekSubject::Edge { .. }),
                                        at_commit: self.snapshot.clone(),
                                    };
                                    studio_ui::ds::button(("open-blob", index), "Open")
                                        .style(ButtonStyle::Subtle)
                                        .color(Color::Muted)
                                        .tooltip(Tooltip::text("Show the whole value in a tab"))
                                        .on_click(move |_, window, cx| {
                                            window.dispatch_action(Box::new(open.clone()), cx)
                                        })
                                })
                                .child(
                                    studio_ui::ds::button(("save-blob", index), "Save…")
                                        .style(ButtonStyle::Subtle)
                                        .color(Color::Muted)
                                        .tooltip(Tooltip::text("Save the whole value to a file"))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.save_blob(index, cx)
                                        })),
                                )
                                .into_any_element(),
                        ),
                        None => None,
                    };
                v_flex()
                    .gap(px(space::XS))
                    .child(
                        h_flex()
                            .gap(px(space::MD))
                            .items_center()
                            .child(studio_ui::ds::icon::meta(IconName::File))
                            .child(studio_ui::ds::text::body(slot.property.clone()))
                            .child(studio_ui::ds::text::meta(facts))
                            .child(div().flex_1())
                            .children(save),
                    )
                    .children(body)
            }))
            .into_any_element()
    }

    /// Lays out the node's relationship groups. What the table read for them is shown as read;
    /// the rest are read as their rows are drawn ([`Self::drawn`]), so a type with dozens of
    /// relationships reads only those in view.
    fn load_groups(&mut self, cx: &mut Context<Self>) {
        let PeekSubject::Node { type_name, .. } = self.subject.clone() else {
            return;
        };
        let Some(schema) = self.schema.clone() else {
            return;
        };
        self.groups = schema
            .relationships_of(&type_name)
            .iter()
            .map(|group| Group {
                group: group.clone(),
                count: Loadable::NotLoaded,
                as_drawn: true,
                drawn: false,
                expanded: false,
                neighbours: Loadable::NotLoaded,
                more: None,
                _task: None,
            })
            .collect();
        for index in 0..self.groups.len() {
            let key = builder::relationship_key(&self.groups[index].group);
            let Some(known) = self.known.get(&key).cloned() else {
                continue;
            };
            // What the table read for the row's relationship is what the view shows of it.
            let relationship = self.groups[index].group.clone();
            if let PeekSubject::Node { id, .. } = &self.subject {
                let listed = known
                    .first
                    .as_ref()
                    .map(|first| first.iter().map(|n| n.id.clone()).collect());
                let depends = Depends::EdgesAt {
                    edge: relationship.edge.clone(),
                    nodes: vec![id.clone()],
                    listed: Some((relationship.other_type.clone(), "id".to_owned())),
                };
                self.note_shown(depends, listed, cx);
            }
            // A small group shows its neighbours without a click; those the table did not read
            // are read once its row is drawn.
            let opens = self.opens_by_itself(known.count);
            let group = &mut self.groups[index];
            group.count = Loadable::Loaded(known.count);
            group.as_drawn = false;
            if let Some(first) = known.first {
                group.neighbours = Loadable::Loaded(first);
                group.expanded = opens;
            }
        }
        cx.notify();
    }

    /// A group's row is laid out, in view or just past it: a group not counted yet is read, and
    /// a small one counted already opens. Once per group; the work waits until the frame is
    /// drawn.
    fn drawn(&mut self, index: usize, window: &Window, cx: &mut Context<Self>) {
        let Some(group) = self.groups.get_mut(index) else {
            return;
        };
        if std::mem::replace(&mut group.drawn, true) {
            return;
        }
        cx.defer_in(window, move |this, _, cx| {
            let Some(group) = this.groups.get(index) else {
                return;
            };
            // The groups were laid out again meanwhile: this one is read when it is drawn.
            if !group.drawn {
                return;
            }
            match group.count {
                Loadable::NotLoaded => this.load_group(index, this.debounce, false, cx),
                Loadable::Loaded(n)
                    if !group.expanded
                        && matches!(group.neighbours, Loadable::NotLoaded)
                        && this.opens_by_itself(n) =>
                {
                    this.toggle_group(index, cx)
                }
                _ => {}
            }
        });
    }

    /// Reads a group's first neighbours, one more than the view shows: when they fit, that read
    /// is also the group's exact count, so most groups cost one read rather than a count and
    /// then a page (spec 015). A group past the page shows its first neighbours and reads its
    /// count through the session's batched counts. `debounce` delays the read, for a peek that
    /// follows a moving cursor; `open` opens the group when it has any neighbours, as after the
    /// person's own write to it, where otherwise only a small one opens by itself.
    fn load_group(&mut self, index: usize, debounce: bool, open: bool, cx: &mut Context<Self>) {
        let PeekSubject::Node { type_name, id } = self.subject.clone() else {
            return;
        };
        let Some(schema) = self.schema.clone() else {
            return;
        };
        let limit = self.neighbour_limit();
        let without = self.groups_absent.clone();
        let Some(group) = self.groups.get_mut(index) else {
            return;
        };
        group.count = Loadable::Loading;
        group.neighbours = Loadable::Loading;
        group.expanded = false;
        group.more = None;
        let query = match builder::neighbours_without_properties(
            &schema,
            &type_name,
            &group.group,
            &id,
            None,
            builder::Narrow::default(),
            limit + 1,
            &without,
        ) {
            Ok(query) => query,
            Err(error) => {
                group.count = Loadable::Failed(error.to_string());
                group.neighbours = Loadable::NotLoaded;
                return;
            }
        };
        let session = self.session.downgrade();
        let snapshot = self.snapshot.clone();
        let relationship = group.group.clone();
        let read_without = without;
        let scope = self.scope.clone();
        group._task = Some(cx.spawn(async move |this, cx| {
            if debounce {
                cx.background_executor().timer(COUNT_DELAY).await;
            }
            let Ok(read) = session.update(cx, |session, cx| match &scope {
                Some(scope) => session.read_in(scope, &query, snapshot.clone(), cx),
                None => session.read(&query, snapshot.clone(), cx),
            }) else {
                return;
            };
            let first = match read.await {
                Ok(output) => neighbours_from(&output.rows),
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.group_refused(index, &relationship, &read_without, error, open, cx);
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            let fits = first.len() <= limit as usize;
            let mut shown = first;
            shown.truncate(limit as usize);
            let count = if fits {
                Ok(shown.len() as u64)
            } else {
                // More than a page: the exact count, batched with every other one asked now.
                let Ok(count) = session.update(cx, |session, cx| match &scope {
                    Some(scope) => {
                        session.degree_in(scope, &type_name, &relationship, &id, snapshot, cx)
                    }
                    None => session.degree(&type_name, &relationship, &id, snapshot, cx),
                }) else {
                    return;
                };
                count.await
            };
            this.update(cx, |this, cx| {
                let count = match count {
                    Ok(n) => Loadable::Loaded(i64::try_from(n).unwrap_or(i64::MAX)),
                    Err(error) => Loadable::Failed(error),
                };
                let opens = matches!(
                    count,
                    Loadable::Loaded(n) if if open { n > 0 } else { this.opens_by_itself(n) }
                );
                if let Some(group) = this.groups.get_mut(index) {
                    group.count = count;
                    group.neighbours = Loadable::Loaded(shown);
                    // A small group shows its neighbours without a click.
                    group.expanded = opens;
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// A group's read was refused. At a commit made before its edge type or the type at its
    /// other end existed, it had no neighbours; one made before a property it reads (the
    /// neighbour's label, an edge property) was added or renamed is read again without it, once
    /// per property: `read_without` is what this read left out already.
    fn group_refused(
        &mut self,
        index: usize,
        relationship: &RelationshipGroup,
        read_without: &[(String, String)],
        error: ReadError,
        open: bool,
        cx: &mut Context<Self>,
    ) {
        let gap = match &error {
            ReadError::Refused(refusal) => refusal.historical_gap(),
            _ => None,
        };
        let ends = [&relationship.edge, &relationship.other_type];
        match gap {
            Some(HistoricalGap::Type { name }) if ends.contains(&&name) => {
                if let Some(group) = self.groups.get_mut(index) {
                    group.count = Loadable::Loaded(0);
                    group.neighbours = Loadable::Loaded(Vec::new());
                    group.expanded = false;
                }
            }
            Some(HistoricalGap::Property {
                owner, property, ..
            }) if ends.contains(&&owner)
                && !read_without
                    .iter()
                    .any(|(o, p)| *o == owner && *p == property) =>
            {
                // Another group's read may have found it first.
                if !self
                    .groups_absent
                    .iter()
                    .any(|(o, p)| *o == owner && *p == property)
                {
                    self.groups_absent.push((owner, property));
                }
                self.load_group(index, false, open, cx);
            }
            _ => {
                if let Some(group) = self.groups.get_mut(index) {
                    group.count = Loadable::Failed(error.to_string());
                    group.neighbours = Loadable::NotLoaded;
                }
            }
        }
    }

    fn toggle_group(&mut self, index: usize, cx: &mut Context<Self>) {
        let PeekSubject::Node { type_name, id } = self.subject.clone() else {
            return;
        };
        let Some(schema) = self.schema.clone() else {
            return;
        };
        let limit = self.neighbour_limit();
        let without = self.groups_absent.clone();
        let Some(group) = self.groups.get_mut(index) else {
            return;
        };
        group.expanded = !group.expanded;
        if !group.expanded || !matches!(group.neighbours, Loadable::NotLoaded) {
            cx.notify();
            return;
        }
        let query = match builder::neighbours_without_properties(
            &schema,
            &type_name,
            &group.group,
            &id,
            None,
            builder::Narrow::default(),
            limit,
            &without,
        ) {
            Ok(query) => query,
            Err(error) => {
                group.neighbours = Loadable::Failed(error.to_string());
                cx.notify();
                return;
            }
        };
        group.neighbours = Loadable::Loading;
        let snapshot = self.snapshot.clone();
        let read = self.read_shown(&query, snapshot, cx);
        let task = cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, cx| {
                if let Some(group) = this.groups.get_mut(index) {
                    group.neighbours = match result {
                        Ok(output) => Loadable::Loaded(neighbours_from(&output.rows)),
                        Err(error) => Loadable::Failed(error.to_string()),
                    };
                }
                cx.notify();
            })
            .ok();
        });
        if let Some(group) = self.groups.get_mut(index) {
            group._task = Some(task);
        }
        cx.notify();
    }

    /// Reads a group's next page of neighbours, after the last one shown.
    fn show_more(&mut self, index: usize, cx: &mut Context<Self>) {
        let PeekSubject::Node { type_name, id } = self.subject.clone() else {
            return;
        };
        let Some(schema) = self.schema.clone() else {
            return;
        };
        let without = self.groups_absent.clone();
        let Some(group) = self.groups.get(index) else {
            return;
        };
        let Some(after) = group
            .neighbours
            .loaded()
            .and_then(|shown| shown.last())
            .map(|last| last.id.clone())
        else {
            return;
        };
        if group.more.is_some() {
            return;
        }
        let query = match builder::neighbours_without_properties(
            &schema,
            &type_name,
            &group.group,
            &id,
            Some(&after),
            builder::Narrow::default(),
            PAGE_NEIGHBOURS,
            &without,
        ) {
            Ok(query) => query,
            Err(_) => return,
        };
        let snapshot = self.snapshot.clone();
        let read = self.read_shown(&query, snapshot, cx);
        let task = cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, cx| {
                if let Some(group) = this.groups.get_mut(index) {
                    group.more = None;
                    match result {
                        Ok(output) => {
                            if let Loadable::Loaded(shown) = &mut group.neighbours {
                                shown.extend(neighbours_from(&output.rows));
                            }
                        }
                        Err(error) => group.neighbours = Loadable::Failed(error.to_string()),
                    }
                }
                cx.notify();
            })
            .ok();
        });
        if let Some(group) = self.groups.get_mut(index) {
            group.more = Some(task);
        }
        cx.notify();
    }

    // ---- editing (spec 014) ----

    /// Whether this view edits `property` in place: a node's property, read at the head, of a
    /// kind a table cell edits too.
    fn edits_property(&self, property: &gq_language::Property) -> bool {
        matches!(self.subject, PeekSubject::Node { .. })
            && !self.holds_commit
            && property.editable_in_cell()
            && self.values.loaded().is_some()
    }

    fn value_of(&self, name: &str) -> Value {
        self.values
            .loaded()
            .and_then(|values| values.get(name))
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn draft_key(&self, name: &str) -> Option<DraftKey> {
        let PeekSubject::Node { type_name, id } = &self.subject else {
            return None;
        };
        Some(DraftKey {
            type_name: type_name.clone(),
            id: id.clone(),
            property: name.to_owned(),
        })
    }

    /// The property's unsaved value, kept in the session's drafts.
    fn draft_of(&self, name: &str, cx: &App) -> Option<DraftEntry> {
        let key = self.draft_key(name)?;
        self.session.read(cx).draft(&key).cloned()
    }

    /// The node's unsaved values, by property.
    fn node_drafts(&self, cx: &App) -> Vec<(String, DraftEntry)> {
        let PeekSubject::Node { type_name, id } = &self.subject else {
            return Vec::new();
        };
        self.session
            .read(cx)
            .drafts()
            .filter(|(key, _)| key.type_name == *type_name && key.id == *id)
            .map(|(key, entry)| (key.property.clone(), entry.clone()))
            .collect()
    }

    fn put_draft(&mut self, name: &str, old: Value, text: String, cx: &mut Context<Self>) {
        if let Some(key) = self.draft_key(name) {
            self.session.update(cx, |session, cx| {
                session.set_draft(key, DraftEntry { old, text }, cx)
            });
        }
    }

    fn drop_draft(&mut self, name: &str, cx: &mut Context<Self>) {
        let branch = self.branch.clone();
        self.drop_draft_on(&branch, name, cx);
    }

    fn drop_draft_on(&mut self, branch: &str, name: &str, cx: &mut Context<Self>) {
        if let Some(key) = self.draft_key(name) {
            self.session
                .update(cx, |session, cx| session.remove_draft_on(branch, &key, cx));
        }
    }

    /// Opens a property's editor with its value, or focuses it when it is open.
    fn start_edit(
        &mut self,
        property: gq_language::Property,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = property.name.clone();
        if let Some(edit) = self.edits.get(&name) {
            window.focus(&edit.editor.focus_handle(cx), cx);
            return;
        }
        if !self.edits_property(&property) || self.saving.contains(&name) {
            return;
        }
        // A draft opens with its text, its save bound to the value it was typed over: a
        // change made elsewhere since then shows as a conflict rather than being overwritten.
        let (old, text) = match self.draft_of(&name, cx) {
            Some(draft) => (draft.old, draft.text),
            None => {
                let old = self.value_of(&name);
                let text = edit_text(&old);
                (old, text)
            }
        };
        // Text, which reads as markdown, takes line breaks (⇧↵); other values are one line.
        let lines = reads_as_markdown(&property);
        let editor = cx.new(|cx| {
            let mut editor = if lines {
                Editor::auto_height(1, 16, window, cx)
            } else {
                Editor::single_line(window, cx)
            };
            editor.set_text(text, window, cx);
            editor.set_placeholder_text(&property.display_type(), window, cx);
            editor.set_text_style_refinement(studio_ui::ds::field_text(cx));
            editor
        });
        let focus = editor.focus_handle(cx);
        let left = name.clone();
        let blur = cx.on_blur(&focus, window, move |this, _, cx| {
            this.left_field(&left, cx)
        });
        window.focus(&focus, cx);
        self.edits.insert(
            name,
            FieldEdit {
                property,
                old,
                editor,
                notice: None,
                _blur: blur,
            },
        );
        cx.notify();
    }

    /// Leaving a field closes it: a changed value is kept as a draft, the session's (spec 014),
    /// which a table of the type shows too and ⌘S saves with the node's others; one typed back
    /// to what the node holds lets the draft go. Nothing is written by leaving a field.
    fn left_field(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(edit) = self.edits.get(name) else {
            return;
        };
        // A field with a notice stays open, so its reason stays in view.
        if edit.notice.is_some() {
            return;
        }
        let text = edit.editor.read(cx).text(cx);
        let unchanged =
            parse_input(&edit.property, &text).is_ok_and(|value| same_value(&value, &edit.old));
        let old = edit.old.clone();
        self.edits.remove(name);
        if unchanged {
            self.drop_draft(name, cx);
        } else {
            self.put_draft(name, old, text, cx);
            self.confirm_discard = false;
        }
        cx.notify();
    }

    /// The open field that has the keyboard.
    fn focused_field(&self, window: &Window, cx: &App) -> Option<String> {
        self.edits
            .iter()
            .find(|(_, edit)| edit.editor.focus_handle(cx).is_focused(window))
            .map(|(name, _)| name.clone())
    }

    fn on_save_field(&mut self, _: &SaveField, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(name) = self.focused_field(window, cx) {
            self.save_field(&name, window, cx);
        }
    }

    /// ⌘⇧↵: the text field in an editor of its own, read at the commit it was being edited at
    /// so its save stays bound to what was read, with what was typed so far.
    fn on_expand_field(&mut self, _: &ExpandField, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(name) = self.focused_field(window, cx) {
            self.expand(&name, window, cx);
        }
    }

    fn expand(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let PeekSubject::Node { type_name, id } = self.subject.clone() else {
            return;
        };
        // A changed field is kept as a draft first: an editor of the property already open only
        // comes forward, and one whose read fails holds the text where nothing shows it.
        let typed = match self.edits.remove(name) {
            Some(edit) => {
                let text = edit.editor.read(cx).text(cx);
                let unchanged = parse_input(&edit.property, &text)
                    .is_ok_and(|value| same_value(&value, &edit.old));
                if !unchanged {
                    self.put_draft(name, edit.old, text.clone(), cx);
                }
                Some(text)
            }
            None => self.draft_of(name, cx).map(|draft| draft.text),
        };
        window.dispatch_action(
            Box::new(studio_actions::graph::OpenTextEditor {
                type_name,
                id,
                property: name.to_owned(),
                read_at: self.snapshot.clone(),
                text: typed,
            }),
            cx,
        );
        cx.notify();
    }

    fn on_cancel_field(&mut self, _: &CancelField, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(name) = self.focused_field(window, cx) {
            self.edits.remove(&name);
            cx.notify();
        }
    }

    /// ↵: writes the field's text, parsed for its type; an unchanged value sends nothing.
    fn save_field(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.edits.get_mut(name) else {
            return;
        };
        let text = edit.editor.read(cx).text(cx);
        let new = match parse_input(&edit.property, &text) {
            Ok(value) => value,
            Err(message) => {
                edit.notice = Some(Notice::new(Tone::Invalid, message));
                cx.notify();
                return;
            }
        };
        let Some(edit) = self.edits.remove(name) else {
            return;
        };
        if same_value(&new, &edit.old) {
            cx.notify();
            return;
        }
        self.write_value(edit.property, edit.old, new, text, window, cx);
    }

    /// Writes one property bound to the value it was read at, as a table cell saves (spec 004):
    /// it changes nothing if the value changed elsewhere meanwhile.
    fn write_value(
        &mut self,
        property: gq_language::Property,
        old: Value,
        new: Value,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let PeekSubject::Node { type_name, id } = self.subject.clone() else {
            return;
        };
        let Some(schema) = self.schema.clone() else {
            return;
        };
        let name = property.name.clone();
        let write = match crate::write::write_property(
            &self.session,
            &self.branch,
            &schema,
            &type_name,
            &id,
            &name,
            &old,
            &new,
            cx,
        ) {
            Ok(write) => write,
            Err(error) => {
                let notice = Notice::new(Tone::Invalid, error);
                self.reopen(property, old, text, notice, window, cx);
                return;
            }
        };
        // The new value shows, with a dot, while the write is out.
        if let Loadable::Loaded(values) = &mut self.values {
            values.insert(name.clone(), new);
        }
        self.saving.insert(name);
        let branch = self.branch.clone();
        // Detached: closing the page or moving the peek never cancels a write.
        cx.spawn_in(window, async move |this, cx| {
            let result = write.await;
            this.update_in(cx, |this, window, cx| {
                if this.branch == branch {
                    this.finish_write(property, old, text, result, window, cx);
                    return;
                }
                // The view reads another branch now; the property's draft was the branch left's.
                this.saving.remove(&property.name);
                if let SaveResult::Saved { .. } = result {
                    this.drop_draft_on(&branch, &property.name, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn finish_write(
        &mut self,
        property: gq_language::Property,
        old: Value,
        text: String,
        result: SaveResult,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = property.name.clone();
        self.saving.remove(&name);
        match result {
            SaveResult::Saved { commit, parent } => {
                self.drop_draft(&name, cx);
                // Made on the commit shown, the view holds the new commit's state; otherwise
                // someone else committed in between and the view is behind.
                if parent.is_some() && parent == self.snapshot {
                    self.snapshot = Some(commit);
                } else {
                    self.newer = true;
                }
            }
            SaveResult::NoMatch => {
                self.restore(&name, old.clone());
                self.newer = true;
                let notice = Notice::new(
                    Tone::Conflict,
                    "Changed elsewhere while you were editing; Refresh reads it now",
                );
                self.reopen(property, old, text, notice, window, cx);
            }
            SaveResult::Refused(notice) | SaveResult::Unknown(notice) => {
                self.restore(&name, old.clone());
                self.reopen(property, old, text, notice, window, cx);
            }
        }
        if self.saving.is_empty() {
            self.compare_head(cx);
        }
        cx.notify();
    }

    fn restore(&mut self, name: &str, value: Value) {
        if let Loadable::Loaded(values) = &mut self.values {
            values.insert(name.to_owned(), value);
        }
    }

    /// Opens the field again with what was typed and why it did not land (constitution III).
    fn reopen(
        &mut self,
        property: gq_language::Property,
        old: Value,
        text: String,
        notice: Notice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = property.name.clone();
        self.start_edit(property, window, cx);
        if let Some(edit) = self.edits.get_mut(&name) {
            edit.old = old;
            edit.notice = Some(notice);
            edit.editor
                .update(cx, |editor, cx| editor.set_text(text, window, cx));
        }
    }

    /// Reads what the view shows, through its scope while it follows the branch.
    fn read_shown(
        &self,
        query: &BuiltQuery,
        snapshot: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<ReadOutput, ReadError>> {
        let scope = self.scope.clone();
        self.session.update(cx, |session, cx| match &scope {
            Some(scope) => session.read_in(scope, query, snapshot, cx),
            None => session.read(query, snapshot, cx),
        })
    }

    /// Something the view shows that the caller read for it (the table's row, its
    /// relationships), with the nodes it lists when known.
    fn note_shown(&self, depends: Depends, listed: Option<Vec<String>>, cx: &mut Context<Self>) {
        if let Some(scope) = &self.scope {
            self.session
                .update(cx, |session, _| session.note_in(scope, depends, listed));
        }
    }

    /// Whether the branch moved past the commit shown, once this view's writes are in. A move
    /// that changed nothing the view shows moves it to the head without reading again; one
    /// whose changes are not known yet waits for the session to read them.
    fn compare_head(&mut self, cx: &mut Context<Self>) {
        self.awaiting_changes = false;
        let Some(scope) = &self.scope else {
            return;
        };
        let session = self.session.read(cx);
        let (Some(head), Some(pin)) = (session.head(&session.branch), self.snapshot.clone()) else {
            return;
        };
        if head == pin {
            self.newer = false;
        } else if !self.newer {
            match session.scope_since(scope, &pin, head) {
                ScopeSince::Pending => self.awaiting_changes = true,
                ScopeSince::Unchanged => self.snapshot = Some(head.to_owned()),
                ScopeSince::Changed => self.newer = true,
            }
        }
        cx.notify();
    }

    // ---- relationships (spec 014) ----

    /// Whether this view adds and removes a node's relationships: a node read at the head.
    fn edits_relationships(&self) -> bool {
        matches!(self.subject, PeekSubject::Node { .. })
            && !self.holds_commit
            && self.values.loaded().is_some()
    }

    /// The edge properties a new relationship of this group must be given; such a one is added
    /// from its own table, whose form asks for them.
    fn required_edge_properties(&self, index: usize) -> Vec<String> {
        let (Some(schema), Some(group)) = (&self.schema, self.groups.get(index)) else {
            return Vec::new();
        };
        schema
            .edge(&group.group.edge)
            .map(|edge| {
                edge.properties
                    .iter()
                    .filter(|p| !p.nullable && !p.is_key)
                    .map(|p| p.name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether the group holds at most one neighbour (`@card(..1)` seen from its source).
    fn holds_one(&self, index: usize) -> bool {
        let (Some(schema), Some(group)) = (&self.schema, self.groups.get(index)) else {
            return false;
        };
        group.group.direction == Direction::Outgoing
            && schema
                .edge(&group.group.edge)
                .is_some_and(|edge| edge.at_most_one)
    }

    /// "+ Add": a search over the other type, this node already the relationship's end.
    fn start_add(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .adding
            .as_ref()
            .is_some_and(|adding| adding.index == index)
        {
            return;
        }
        let Some(group) = self.groups.get(index) else {
            return;
        };
        let other = group.group.other_type.clone();
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(&format!("Search {other} by name or id…"), window, cx);
            editor.set_text_style_refinement(studio_ui::ds::field_text(cx));
            editor
        });
        let changed = cx.subscribe_in(&editor, window, |this, _, event: &EditorEvent, _, cx| {
            if let EditorEvent::BufferEdited = event {
                this.search(cx);
            }
        });
        window.focus(&editor.focus_handle(cx), cx);
        self.removing = None;
        self.group_notices.remove(&index);
        self.adding = Some(AddEdge {
            index,
            editor,
            found: Loadable::NotLoaded,
            answers: None,
            highlighted: 0,
            _search: None,
            _changed: changed,
        });
        if let Some(group) = self.groups.get(index)
            && !group.expanded
        {
            self.toggle_group(index, cx);
        }
        self.search(cx);
        cx.notify();
    }

    /// Reads the other type's nodes matching what is typed, eight at a time, after a pause.
    fn search(&mut self, cx: &mut Context<Self>) {
        let Some(schema) = self.schema.clone() else {
            return;
        };
        let Some(adding) = self.adding.as_mut() else {
            return;
        };
        let Some(group) = self.groups.get(adding.index) else {
            return;
        };
        let text = adding.editor.read(cx).text(cx).trim().to_owned();
        let other = &group.group.other_type;
        let search = suggest::search_nodes(&self.session, &schema, other, &text, None, cx);
        adding._search = Some(cx.spawn(async move |this, cx| {
            let result = search.await;
            this.update(cx, |this, cx| {
                if let Some(adding) = this.adding.as_mut() {
                    adding.found = match result {
                        Ok(rows) => Loadable::Loaded(neighbours_from(&rows)),
                        Err(error) => Loadable::Failed(error),
                    };
                    adding.answers = Some(text);
                    adding.highlighted = 0;
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn move_match(&mut self, step: isize, cx: &mut Context<Self>) {
        if let Some(adding) = self.adding.as_mut()
            && let Loadable::Loaded(found) = &adding.found
            && !found.is_empty()
        {
            let last = found.len() as isize - 1;
            adding.highlighted = (adding.highlighted as isize + step).clamp(0, last) as usize;
            cx.notify();
        }
    }

    fn cancel_add(&mut self, cx: &mut Context<Self>) {
        self.adding = None;
        cx.notify();
    }

    /// Adds the relationship to the highlighted node; on a group that holds one, it replaces
    /// the one there, which is removed first so the limit holds (rule D2 makes that two
    /// commits).
    /// ↵ in the search: the highlighted node of the results for what is typed now. While those
    /// are being read it does nothing, so a node found for earlier text is never written to.
    fn confirm_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(adding) = self.adding.as_ref() else {
            return;
        };
        let typed = adding.editor.read(cx).text(cx);
        if adding.answers.as_deref() == Some(typed.trim()) {
            self.add_highlighted(window, cx);
        }
    }

    fn add_highlighted(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(adding) = self.adding.as_ref() else {
            return;
        };
        let Some(picked) = adding
            .found
            .loaded()
            .and_then(|found| found.get(adding.highlighted))
            .cloned()
        else {
            return;
        };
        let index = adding.index;
        self.adding = None;
        let replaced = self
            .holds_one(index)
            .then(|| {
                self.groups[index]
                    .neighbours
                    .loaded()
                    .and_then(|shown| shown.first())
                    .and_then(|first| first.edge_id.clone())
            })
            .flatten();
        self.write_edge(index, EdgeChange::Add(picked.id), replaced, window, cx);
    }

    /// The first click on a neighbour's × asks; the second removes the relationship.
    fn press_remove(
        &mut self,
        index: usize,
        edge_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.removing.as_deref() == Some(edge_id.as_str()) {
            self.removing = None;
            self.write_edge(index, EdgeChange::Remove(edge_id), None, window, cx);
        } else {
            self.removing = Some(edge_id);
            cx.notify();
        }
    }

    /// Writes one relationship change, after removing `first` when it is given, each as its
    /// own commit; the group is read again at the view's commit, which follows its own
    /// commits.
    fn write_edge(
        &mut self,
        index: usize,
        change: EdgeChange,
        first: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (PeekSubject::Node { id, .. }, Some(schema)) =
            (self.subject.clone(), self.schema.clone())
        else {
            return;
        };
        let Some(group) = self.groups.get(index).map(|group| group.group.clone()) else {
            return;
        };
        let mut queries = Vec::new();
        if let Some(edge_id) = &first {
            queries.push((
                builder::delete_edge(&schema, &group.edge, edge_id),
                "Replace",
            ));
        }
        queries.push(match &change {
            EdgeChange::Add(other) => {
                let (from, to) = match group.direction {
                    Direction::Outgoing => (id.as_str(), other.as_str()),
                    Direction::Incoming => (other.as_str(), id.as_str()),
                };
                (
                    builder::insert_edge(&schema, &group.edge, from, to, &[]),
                    "Add",
                )
            }
            EdgeChange::Remove(edge_id) => (
                builder::delete_edge(&schema, &group.edge, edge_id),
                "Remove",
            ),
        });
        let mut built = Vec::new();
        for (query, verb) in queries {
            match query {
                Ok(query) => built.push((query, verb)),
                Err(error) => {
                    self.group_notices
                        .insert(index, Notice::new(Tone::Invalid, error.to_string()));
                    cx.notify();
                    return;
                }
            }
        }
        let branch = self.branch.clone();
        let other = match &change {
            EdgeChange::Add(other) => other.clone(),
            EdgeChange::Remove(_) => group.other_type.clone(),
        };
        self.edge_writes.insert(index);
        self.group_notices.remove(&index);
        let session = self.session.clone();
        let edge = group.edge;
        let shown = self.snapshot.clone();
        // Detached: closing the page or moving the peek never cancels a write.
        cx.spawn_in(window, async move |this, cx| {
            // A replace is two commits, the removal then the addition. The page follows its own
            // commits from the one it shows while each is on top of the one before.
            let mut outcome = None;
            let mut reached = shown.clone();
            let mut followed = true;
            let mut landed = 0;
            for (query, verb) in built {
                let description = format!("{verb} {edge} of {id} · {other}");
                let write = session.update(cx, |session, cx| {
                    session.write_on(&branch, &query, None, description, None, cx)
                });
                let result = save_result(&write.await, &branch);
                let saved = match &result {
                    SaveResult::Saved { commit, parent } => {
                        followed &= parent.is_some() && *parent == reached;
                        reached = Some(commit.clone());
                        landed += 1;
                        true
                    }
                    _ => false,
                };
                outcome = Some(result);
                if !saved {
                    break;
                }
            }
            this.update(cx, |this, cx| {
                this.edge_writes.remove(&index);
                // The view reads another branch now: its toasts tell how the writes went.
                if this.branch != branch {
                    cx.notify();
                    return;
                }
                if landed > 0 {
                    if followed && this.snapshot == shown {
                        this.snapshot = reached;
                    } else {
                        this.newer = true;
                    }
                }
                // After the removal landed, a failed addition leaves the group without the
                // relationship it replaced: said so, not "nothing was written".
                let partly = |message: &str| {
                    format!(
                        "The relationship it replaces was removed, but the new one was not \
                         added. {message}"
                    )
                };
                match outcome {
                    Some(SaveResult::Saved { .. }) | None => {}
                    Some(SaveResult::NoMatch) => {
                        let message = "Nothing changed: it was already so, or changed elsewhere";
                        let message = if landed > 0 {
                            partly(message)
                        } else {
                            message.to_owned()
                        };
                        this.group_notices
                            .insert(index, Notice::new(Tone::Conflict, message));
                    }
                    Some(SaveResult::Refused(mut notice) | SaveResult::Unknown(mut notice)) => {
                        if landed > 0 {
                            notice.message = partly(&notice.message);
                        }
                        this.group_notices.insert(index, notice);
                    }
                }
                this.reload_group(index, cx);
                if this.saving.is_empty() && this.edge_writes.is_empty() {
                    this.compare_head(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Reads a group's count and neighbours again at the view's commit.
    fn reload_group(&mut self, index: usize, cx: &mut Context<Self>) {
        self.load_group(index, false, true, cx);
    }

    /// ⌘S: the node's drafts as one mutation, bound to the values the view shows, so one commit.
    /// A draft that does not parse opens instead, and nothing is sent.
    fn save_drafts(&mut self, _: &SaveNodeDrafts, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving_drafts || !self.edits_relationships() {
            return;
        }
        // An open field joins the others.
        let open: Vec<String> = self.edits.keys().cloned().collect();
        for name in open {
            self.left_field(&name, cx);
        }
        let (PeekSubject::Node { type_name, id }, Some(schema)) =
            (self.subject.clone(), self.schema.clone())
        else {
            return;
        };
        let mut cells = Vec::new();
        for (name, draft) in self.node_drafts(cx) {
            // A field's own save under way keeps its draft until it lands; sent again, the two
            // writes would race on one value.
            if self.saving.contains(&name) {
                continue;
            }
            let Some(property) = schema
                .node(&type_name)
                .and_then(|node| node.property(&name))
                .cloned()
            else {
                continue;
            };
            // Bound to the value the draft was typed over (spec 014).
            let old = draft.old.clone();
            match parse_input(&property, &draft.text) {
                Ok(new) if same_value(&new, &old) => self.drop_draft(&name, cx),
                Ok(new) => cells.push((name, old, new)),
                Err(message) => {
                    let notice = Notice::new(Tone::Invalid, message);
                    self.reopen(property, old, draft.text, notice, window, cx);
                    return;
                }
            }
        }
        if cells.is_empty() {
            cx.notify();
            return;
        }
        let branch = self.branch.clone();
        // A list among them, which a write's `where` cannot compare, is compared at the head
        // first (`write_properties`).
        let write = match crate::write::write_properties(
            &self.session,
            &branch,
            &schema,
            &type_name,
            &id,
            &cells,
            cx,
        ) {
            Ok(write) => write,
            Err(error) => {
                self.drafts_notice = Some(Notice::new(Tone::Invalid, error));
                cx.notify();
                return;
            }
        };
        self.saving_drafts = true;
        self.drafts_notice = None;
        for (name, _, _) in &cells {
            self.saving.insert(name.clone());
        }
        cx.spawn_in(window, async move |this, cx| {
            let result = write.await;
            this.update(cx, |this, cx| {
                this.saving_drafts = false;
                for (name, _, _) in &cells {
                    this.saving.remove(name);
                }
                // The view reads another branch now: the drafts saved were the branch left's.
                if this.branch != branch {
                    if let SaveResult::Saved { .. } = result {
                        for (name, _, _) in &cells {
                            this.drop_draft_on(&branch, name, cx);
                        }
                    }
                    cx.notify();
                    return;
                }
                match result {
                    SaveResult::Saved { commit, parent } => {
                        for (name, _, new) in cells {
                            this.restore(&name, new);
                            this.drop_draft(&name, cx);
                        }
                        if parent.is_some() && parent == this.snapshot {
                            this.snapshot = Some(commit);
                        } else {
                            this.newer = true;
                        }
                    }
                    SaveResult::NoMatch => {
                        this.newer = true;
                        this.drafts_notice = Some(Notice::new(
                            Tone::Conflict,
                            "A value changed elsewhere; nothing was written and your changes \
                             are kept. Refresh reads it now.",
                        ));
                    }
                    SaveResult::Refused(notice) | SaveResult::Unknown(notice) => {
                        this.drafts_notice = Some(notice);
                    }
                }
                if this.saving.is_empty() && this.edge_writes.is_empty() {
                    this.compare_head(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Drops the node's drafts, on the second press of Discard.
    fn discard_drafts(&mut self, cx: &mut Context<Self>) {
        if !self.confirm_discard {
            self.confirm_discard = true;
            cx.notify();
            return;
        }
        self.confirm_discard = false;
        self.drafts_notice = None;
        for (name, _) in self.node_drafts(cx) {
            self.drop_draft(&name, cx);
        }
        cx.notify();
    }

    /// The bar a node with drafts shows under its title: how many, Discard, and Save (⌘S), one
    /// commit to the branch.
    fn render_drafts_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let count = self.node_drafts(cx).len();
        if count == 0 && !self.saving_drafts {
            return None;
        }
        let branch = self.session.read(cx).branch.clone();
        let label = match count {
            1 => "1 change not saved".to_owned(),
            n => format!("{n} changes not saved"),
        };
        Some(
            h_flex()
                .gap(px(space::SM))
                .items_center()
                .child(Indicator::dot().color(Color::Accent))
                .child(studio_ui::ds::text::body(label))
                .children(self.drafts_notice.as_ref().map(|notice| {
                    studio_ui::ds::text::meta(notice.message.clone()).color(notice.tone.color())
                }))
                .child(div().flex_1())
                .child(
                    studio_ui::ds::row_button(
                        "peek-discard-drafts",
                        if self.confirm_discard {
                            "Discard them"
                        } else {
                            "Discard"
                        },
                    )
                    .style(if self.confirm_discard {
                        ButtonStyle::Tinted(ui::TintColor::Error)
                    } else {
                        ButtonStyle::Subtle
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.discard_drafts(cx))),
                )
                .child(
                    studio_ui::ds::row_button(
                        "peek-save-drafts",
                        if self.saving_drafts {
                            "Saving…"
                        } else {
                            "Save"
                        },
                    )
                    .style(ButtonStyle::Filled)
                    .disabled(self.saving_drafts)
                    .tooltip(move |_, cx| {
                        Tooltip::for_action(format!("One commit to {branch}"), &SaveNodeDrafts, cx)
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.save_drafts(&SaveNodeDrafts, window, cx)
                    })),
                )
                .into_any_element(),
        )
    }

    /// Copies a property's value as it is when clicked.
    fn copy(&mut self, name: String, cx: &mut Context<Self>) {
        let text = studio_ui::value_text(&self.value_of(&name));
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.copied = Some(name);
        cx.notify();
        self._copied = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1500))
                .await;
            this.update(cx, |this, cx| {
                this.copied = None;
                cx.notify();
            })
            .ok();
        }));
    }

    /// Keeps one markdown view per text value with markup, made again only when its text
    /// changes; a value that reads as itself draws as a label.
    fn sync_markdown(&mut self, cx: &mut Context<Self>) {
        let Loadable::Loaded(values) = &self.values else {
            self.markdown.clear();
            self.plain_text.clear();
            return;
        };
        // Runs every frame: a text is parsed only when it is new, so an unchanged view costs a
        // comparison per property.
        let Some(schema) = self.schema.clone() else {
            return;
        };
        let mut wanted: HashSet<String> = HashSet::new();
        for property in listed_properties(&schema, &self.subject) {
            let name = property.name.clone();
            let Some(text) = values.get(&name).and_then(Value::as_str) else {
                continue;
            };
            if !reads_as_markdown(property) {
                continue;
            }
            if self
                .markdown
                .get(&name)
                .is_some_and(|(shown, _)| shown == text)
                || self
                    .plain_text
                    .get(&name)
                    .is_some_and(|plain| plain == text)
            {
                wanted.insert(name);
                continue;
            }
            if studio_ui::text::is_plain(text) {
                self.plain_text.insert(name.clone(), text.to_owned());
                self.markdown.remove(&name);
            } else {
                self.plain_text.remove(&name);
                let source: SharedString = text.to_owned().into();
                let view = cx.new(|cx| Markdown::new(source, None, None, cx));
                self.markdown.insert(name.clone(), (text.to_owned(), view));
            }
            wanted.insert(name);
        }
        self.markdown.retain(|name, _| wanted.contains(name));
        self.plain_text.retain(|name, _| wanted.contains(name));
    }

    fn render_properties(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let values = match &self.values {
            Loadable::Loaded(values) => values,
            Loadable::Failed(error) => {
                return studio_ui::ds::text::error(error.clone()).into_any_element();
            }
            _ => {
                return studio_ui::ds::text::muted("Reading…").into_any_element();
            }
        };
        let hover = studio_ui::ds::surface::hover(cx);
        let properties = self
            .schema
            .as_deref()
            .map(|schema| listed_properties(schema, &self.subject))
            .unwrap_or_default();
        let label_width = if self.page {
            studio_ui::ds::size::FIELD_LABEL_WIDE
        } else {
            studio_ui::ds::size::FIELD_LABEL
        };
        // Pulled out by the rows' hover inset, as a relationship group's rows are.
        v_flex()
            .mx(px(-space::XS))
            .gap(px(space::XXS))
            .children(
                properties
                    .into_iter()
                    .enumerate()
                    .map(|(index, property)| {
                        let name = property.name.as_str();
                        let ty = property.display_type();
                        // Borrowed: this runs at every draw, and a text value can be long.
                        let value = values.get(name).unwrap_or(&NULL);
                        let title = match name {
                            FROM_COLUMN => "from".to_owned(),
                            TO_COLUMN => "to".to_owned(),
                            _ => name.to_owned(),
                        };
                        let copied = self.copied.as_deref() == Some(name);
                        let editable = self.edits_property(property);
                        let editing = self.edits.contains_key(name);
                        let saving = self.saving.contains(name);
                        // An unsaved value shows what was typed, with a dot, and opens on a click.
                        let draft = (editable && !editing && !saving)
                            .then(|| self.draft_of(name, cx))
                            .flatten();
                        // An enum is a menu of its values and a boolean a switch, each saving
                        // as it is chosen; other values open an editor on a click.
                        let picker = (editable && !editing && !saving && draft.is_none())
                            .then(|| self.render_picker(index, property, value, cx))
                            .flatten();
                        let opens_editor = editable && picker.is_none() && !editing && !saving;
                        let body = match (self.edits.get(name), picker) {
                            _ if let Some(then) = self.absent.get(name) => studio_ui::ds::text::muted(
                                absence_text(then, true),
                            )
                            .into_any_element(),
                            (Some(edit), _) => self.render_field_edit(edit, window, cx),
                            (None, _) if draft.is_some() => {
                                let text = draft.as_ref().map_or("", |d| d.text.as_str());
                                h_flex()
                                    .gap(px(space::SM))
                                    .child(
                                        studio_ui::ds::text::body(if text.trim().is_empty() { "—".to_owned() } else { studio_ui::text::first_line(text) }),
                                    )
                                    .child(Indicator::dot().color(Color::Accent))
                                    .into_any_element()
                            }
                            (None, Some(picker)) => picker,
                            (None, None) => {
                                self.render_value(name, Some(property), value, window, cx)
                            }
                        };
                        // Only a click's own: the copy reads the value as it is then.
                        let edit_property = opens_editor.then(|| property.clone());
                        let copy_name = name.to_owned();
                        // Text opens in the editor from a button the row shows on hover.
                        let expands = opens_editor && reads_as_markdown(property);
                        let group = SharedString::from(format!("peek-property-{index}"));
                        let expand_name = name.to_owned();
                        h_flex()
                            .id(("peek-property", index))
                            .group(group.clone())
                            .px(px(space::XS))
                            .py(px(space::XXS))
                            .gap(px(space::MD))
                            .items_start()
                            .rounded(px(studio_ui::ds::radii(cx).sm))
                            .when(!editing, |row| row.hover(move |s| s.bg(hover)))
                            .when(opens_editor || !editable, |row| row.cursor_pointer())
                            .when(!editing, |row| {
                                row.tooltip(Tooltip::text(if draft.is_some() {
                                    format!(
                                        "Not saved; it holds “{}”. Click to edit; ⌘S saves this node's changes together",
                                        display_value(Some(property), value)
                                    )
                                } else if opens_editor {
                                    format!("{ty} · click to edit")
                                } else if editable {
                                    ty.clone()
                                } else {
                                    format!("{ty} · click to copy")
                                }))
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if let Some(property) = edit_property.clone() {
                                    this.start_edit(property, window, cx);
                                } else if !editable {
                                    this.copy(copy_name.clone(), cx)
                                }
                            }))
                            .child(
                                div()
                                    .w(px(label_width))
                                    .flex_none()
                                    .pt(px(if editing {
                                        space::SM
                                    } else {
                                        0.
                                    }))
                                    .child(
                                        studio_ui::ds::text::muted(title)
                                            .truncate(),
                                    ),
                            )
                            .child(div().flex_1().min_w_0().child(body))
                            .when(expands, |row| {
                                row.child(
                                    studio_ui::ds::row_icon_button(
                                        ("peek-open-editor", index),
                                        IconName::Maximize,
                                    )
                                    .icon_color(Color::Muted)
                                    .visible_on_hover(group.clone())
                                    .tooltip(Tooltip::text("Open in the editor"))
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.expand(&expand_name, window, cx)
                                        },
                                    )),
                                )
                            })
                            .when(saving, |row| {
                                row.child(
                                    div()
                                        .id(("peek-saving", index))
                                        .pt(px(space::SM))
                                        .tooltip(Tooltip::text("Saving…"))
                                        .child(Indicator::dot().color(Color::Warning)),
                                )
                            })
                            .when(copied, |d| {
                                d.child(
                                    studio_ui::ds::text::meta("Copied")
                                        .color(Color::Success),
                                )
                            })
                    }),
            )
            .into_any_element()
    }

    /// A value as it reads: markdown when it has markup, else a label.
    fn render_value(
        &self,
        name: &str,
        property: Option<&gq_language::Property>,
        value: &Value,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match self.markdown.get(name) {
            // GPUI's text takes its size from the element around it, so the labels' size is
            // set there too.
            Some((_, view)) => div()
                .text_size(studio_ui::ds::text::body_size(cx))
                .child(MarkdownElement::new(view.clone(), value_style(window, cx)))
                .into_any_element(),
            None => studio_ui::ds::text::body(display_value(property, value))
                .color(if value.is_null() {
                    Color::Disabled
                } else {
                    Color::Default
                })
                .into_any_element(),
        }
    }

    /// An enum's menu of values or a boolean's switch, each writing as it is chosen; `None` for
    /// a property edited as text.
    fn render_picker(
        &self,
        index: usize,
        property: &gq_language::Property,
        value: &Value,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if property.list {
            return None;
        }
        let this = cx.entity().downgrade();
        if let Some(options) = property.enum_values.clone() {
            let current = value.as_str().map(str::to_owned);
            let nullable = property.nullable;
            let property = property.clone();
            let shown = current.clone().unwrap_or_else(|| "—".into());
            // The menu's label on the values' edge: the button's own padding sits outside it.
            return Some(
                div()
                    .ml(px(-space::XS))
                    .child(
                        PopoverMenu::new(SharedString::from(format!("peek-enum-{index}")))
                            .trigger(
                                studio_ui::ds::row_button(
                                    SharedString::from(format!("peek-enum-button-{index}")),
                                    shown,
                                )
                                .style(ButtonStyle::Subtle)
                                .end_icon(Icon::new(IconName::ChevronDown)),
                            )
                            .anchor(gpui::Anchor::TopLeft)
                            .menu(move |window, cx| {
                                let (this, property, options, current) = (
                                    this.clone(),
                                    property.clone(),
                                    options.clone(),
                                    current.clone(),
                                );
                                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                    let choose = |chosen: Value| {
                                        let (this, property) = (this.clone(), property.clone());
                                        move |window: &mut Window, cx: &mut App| {
                                            this.update(cx, |this, cx| {
                                                let old = this.value_of(&property.name);
                                                if !same_value(&old, &chosen) {
                                                    let text = edit_text(&chosen);
                                                    this.write_value(
                                                        property.clone(),
                                                        old,
                                                        chosen.clone(),
                                                        text,
                                                        window,
                                                        cx,
                                                    );
                                                }
                                            })
                                            .ok();
                                        }
                                    };
                                    for option in &options {
                                        menu = menu.toggleable_entry(
                                            option.clone(),
                                            current.as_deref() == Some(option.as_str()),
                                            IconPosition::Start,
                                            None,
                                            choose(Value::String(option.clone())),
                                        );
                                    }
                                    if nullable {
                                        menu = menu.separator().toggleable_entry(
                                            "None",
                                            current.is_none(),
                                            IconPosition::Start,
                                            None,
                                            choose(Value::Null),
                                        );
                                    }
                                    menu
                                }))
                            }),
                    )
                    .into_any_element(),
            );
        }
        if property.kind == gq_language::ScalarKind::Bool {
            let on = value.as_bool().unwrap_or(false);
            let property = property.clone();
            return Some(
                studio_ui::ds::switch(("peek-switch", index), on.into(), cx)
                    .on_click(move |state, window, cx| {
                        let chosen = Value::Bool(*state == ToggleState::Selected);
                        this.update(cx, |this, cx| {
                            let old = this.value_of(&property.name);
                            let text = edit_text(&chosen);
                            this.write_value(property.clone(), old, chosen, text, window, cx);
                        })
                        .ok();
                    })
                    .into_any_element(),
            );
        }
        None
    }

    /// An open field: its editor, a clear button for a nullable value, why the last save did
    /// not land, and the keys with the branch it writes to.
    fn render_field_edit(
        &self,
        edit: &FieldEdit,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let lines = reads_as_markdown(&edit.property);
        let invalid = edit
            .notice
            .as_ref()
            .is_some_and(|notice| matches!(notice.tone, Tone::Invalid | Tone::Refused));
        let branch = self.session.read(cx).branch.clone();
        let keys = if lines {
            format!("↵ save · ⇧↵ line break · ⌘⇧↵ editor · esc cancel · write · {branch}")
        } else {
            format!("↵ save · esc cancel · write · {branch}")
        };
        let clear = (edit.property.nullable && !edit.old.is_null()).then(|| {
            let (property, old) = (edit.property.clone(), edit.old.clone());
            studio_ui::ds::row_icon_button(
                SharedString::from(format!("peek-clear-{}", edit.property.name)),
                IconName::Close,
            )
            .icon_color(Color::Muted)
            .tooltip(Tooltip::text("Set to none"))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.edits.remove(&property.name);
                this.write_value(
                    property.clone(),
                    old.clone(),
                    Value::Null,
                    String::new(),
                    window,
                    cx,
                );
            }))
        });
        let hint = edit
            .property
            .enum_values
            .as_ref()
            .map(|values| format!("One of: {}", values.join(", ")));
        // The frame reaches past the values' edge by its padding, its border and the editor's
        // own inset, so the text stays where it read before the edit.
        let reach = px(space::MD + 1.) + editor_inset(window, cx);
        v_flex()
            .key_context("NodeField")
            .w_full()
            .gap(px(space::XXS))
            .child(
                studio_ui::ds::field_frame(invalid, cx)
                    .ml(-reach)
                    .when(lines, |frame| {
                        frame
                            .h_auto()
                            .min_h(px(studio_ui::ds::size::CONTROL))
                            .py(px(space::XS))
                            .items_start()
                    })
                    .child(studio_ui::ds::field_input(edit.editor.clone()))
                    .children(clear)
                    .children(lines.then(|| {
                        let name = edit.property.name.clone();
                        studio_ui::ds::row_icon_button(
                            SharedString::from(format!("peek-expand-{name}")),
                            IconName::Maximize,
                        )
                        .icon_color(Color::Muted)
                        .tooltip(|_, cx| {
                            Tooltip::for_action("Open in the editor", &ExpandField, cx)
                        })
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.expand(&name, window, cx)),
                        )
                    })),
            )
            .children(edit.notice.clone().map(|notice| {
                let unknown = notice.tone == Tone::Unknown;
                h_flex()
                    .gap(px(space::SM))
                    .child(Indicator::dot().color(notice.tone.color()))
                    .child(studio_ui::ds::text::meta(notice.message).color(notice.tone.color()))
                    // Whether the save landed is in the branch's history.
                    .when(unknown, |row| {
                        row.child(
                            studio_ui::ds::row_button("peek-open-history", "Open history")
                                .style(ButtonStyle::Subtle)
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(
                                        Box::new(studio_actions::graph::OpenHistory),
                                        cx,
                                    )
                                }),
                        )
                    })
            }))
            .children(hint.map(studio_ui::ds::text::meta))
            .child(studio_ui::ds::text::meta(keys))
            .into_any_element()
    }

    /// Where a group's row sits. One the view laid out without its count keeps its place
    /// among those read as they are drawn. Of the others, one known to have none waits under
    /// the empty row, so opening that row moves nothing, unless a write or its outcome is out
    /// on it; the rest are listed first.
    fn place(&self, index: usize) -> Place {
        let Some(group) = self.groups.get(index) else {
            return Place::Folded;
        };
        let working = self.edge_writes.contains(&index) || self.group_notices.contains_key(&index);
        place_of(group, working)
    }

    /// Whether a group under the empty row shows: the row is open, or the person is adding to
    /// that group.
    fn folded_shown(&self, index: usize) -> bool {
        self.show_empty
            || self
                .adding
                .as_ref()
                .is_some_and(|adding| adding.index == index)
    }

    /// The row the groups with no neighbours fold into, with how many; a click shows them.
    fn render_empty_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let hover = studio_ui::ds::surface::hover(cx);
        let folded = (0..self.groups.len())
            .filter(|&index| self.place(index) == Place::Folded)
            .count();
        let open = self.show_empty;
        h_flex()
            .id("peek-empty-groups")
            .px(px(space::XS))
            .py(px(space::XXS))
            .gap(px(space::SM))
            .rounded(px(studio_ui::ds::radii(cx).sm))
            .hover(move |s| s.bg(hover))
            .cursor_pointer()
            .tooltip(Tooltip::text(if open {
                "Hide the relationships with none"
            } else {
                "Show the relationships with none, to add one"
            }))
            .on_click(cx.listener(|this, _, _, cx| {
                this.show_empty = !this.show_empty;
                cx.notify();
            }))
            .child(studio_ui::ds::icon::meta(if open {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            }))
            .child(studio_ui::ds::text::muted("Empty"))
            .child(div().flex_1())
            .child(studio_ui::ds::text::muted(format_count(folded as i64)).buffer_font(cx))
            .into_any_element()
    }

    /// One relationship group: its header (direction, name, the other type, "+", the count),
    /// then an open search, its notice and, when open, its neighbours.
    fn render_group(&self, index: usize, cx: &mut Context<Self>) -> Div {
        let hover = studio_ui::ds::surface::hover(cx);
        let group = &self.groups[index];
        let icon = match group.group.direction {
            Direction::Outgoing => IconName::ArrowRight,
            Direction::Incoming => IconName::ArrowLeft,
        };
        // Until its count is read a group says nothing about what it holds.
        let count = match &group.count {
            Loadable::Loaded(n) => format_count(*n),
            Loadable::Failed(_) => "?".into(),
            Loadable::NotLoaded | Loadable::Loading => "…".into(),
        };
        let failed = match &group.count {
            Loadable::Failed(error) => Some(error.clone()),
            _ => None,
        };
        let header_group = SharedString::from(format!("peek-group-{index}"));
        let add = self
            .edits_relationships()
            .then(|| self.render_add_button(index, header_group.clone(), cx));
        let header = h_flex()
            .id(("peek-group", index))
            .group(header_group)
            .px(px(space::XS))
            .py(px(space::XXS))
            .gap(px(space::SM))
            .rounded(px(studio_ui::ds::radii(cx).sm))
            .hover(move |s| s.bg(hover))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_group(index, cx)))
            .when_some(failed, |d, error| d.tooltip(Tooltip::text(error)))
            .child(studio_ui::ds::icon::meta(if group.expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            }))
            .child(studio_ui::ds::icon::meta(icon))
            .child(studio_ui::ds::text::body(gq_language::traversal_name(
                &group.group.edge,
            )))
            .child(studio_ui::ds::text::muted(group.group.other_type.clone()))
            .child(div().flex_1())
            .when(self.edge_writes.contains(&index), |header| {
                header.child(Indicator::dot().color(Color::Warning))
            })
            .children(add)
            .child(studio_ui::ds::text::muted(count).buffer_font(cx));
        let adding = self
            .adding
            .as_ref()
            .filter(|adding| adding.index == index)
            .map(|adding| self.render_add_row(adding, cx));
        let notice = self.group_notices.get(&index).map(|notice| {
            h_flex()
                .pl(px(CHILD_INDENT))
                .gap(px(space::SM))
                .child(Indicator::dot().color(notice.tone.color()))
                .child(studio_ui::ds::text::meta(notice.message.clone()).color(notice.tone.color()))
        });
        let body = group
            .expanded
            .then(|| self.render_neighbours(index, group, cx));
        v_flex()
            .child(header)
            .children(adding)
            .children(notice)
            .children(body)
    }

    /// A group's "+": a search for the node to relate; a relationship that needs properties
    /// opens its own table, whose form asks for them.
    fn render_add_button(
        &self,
        index: usize,
        header: SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let group = &self.groups[index].group;
        let required = self.required_edge_properties(index);
        let replaces = self.holds_one(index)
            && matches!(self.groups[index].count, Loadable::Loaded(n) if n > 0);
        let tooltip = if !required.is_empty() {
            format!(
                "{} needs {}: add it in its table",
                group.edge,
                required.join(", ")
            )
        } else if replaces {
            format!("Replace the {}", group.other_type)
        } else {
            format!("Add a {}", group.other_type)
        };
        let edge = group.edge.clone();
        studio_ui::ds::row_icon_button(("peek-add", index), IconName::Plus)
            .icon_color(Color::Muted)
            .when(!self.page, |button| button.visible_on_hover(header))
            .tooltip(Tooltip::text(tooltip))
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                if required.is_empty() {
                    this.start_add(index, window, cx);
                } else {
                    window.dispatch_action(
                        Box::new(studio_actions::graph::OpenType {
                            type_name: edge.clone(),
                            at_commit: None,
                            filter_id: None,
                            via: None,
                            preview: false,
                        }),
                        cx,
                    );
                }
            }))
            .into_any_element()
    }

    /// The search a "+" opens: what is typed, the nodes it finds (↑↓, ↵ adds), and the branch
    /// the relationship is written to.
    fn render_add_row(&self, adding: &AddEdge, cx: &mut Context<Self>) -> AnyElement {
        let index = adding.index;
        let other = self.groups[index].group.other_type.clone();
        let hover = studio_ui::ds::surface::highlight(cx);
        let selected = studio_ui::ds::surface::highlight(cx);
        let branch = self.session.read(cx).branch.clone();
        let replaces = self.holds_one(index)
            && matches!(self.groups[index].count, Loadable::Loaded(n) if n > 0);
        let found = match &adding.found {
            Loadable::Loaded(found) if found.is_empty() => {
                studio_ui::ds::text::meta("No match").into_any_element()
            }
            Loadable::Loaded(found) => v_flex()
                .children(found.iter().enumerate().map(|(n, node)| {
                    let highlighted = n == adding.highlighted;
                    h_flex()
                        .id(SharedString::from(format!("peek-found-{index}-{n}")))
                        .px(px(space::XS))
                        .py(px(space::XXS))
                        .gap(px(space::MD))
                        .rounded(px(studio_ui::ds::radii(cx).sm))
                        .cursor_pointer()
                        .when(highlighted, |row| row.bg(selected))
                        .when(!highlighted, |row| row.hover(move |s| s.bg(hover)))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if let Some(adding) = this.adding.as_mut() {
                                adding.highlighted = n;
                            }
                            this.add_highlighted(window, cx);
                        }))
                        .child(studio_ui::type_swatch(&other, cx))
                        .child(
                            studio_ui::ds::text::body(
                                node.label.clone().unwrap_or_else(|| node.id.clone()),
                            )
                            .truncate(),
                        )
                        .when(node.label.is_some(), |row| {
                            row.child(studio_ui::ds::text::meta(node.id.clone()).truncate())
                        })
                }))
                .into_any_element(),
            Loadable::Failed(error) => {
                studio_ui::ds::text::error_note(error.clone()).into_any_element()
            }
            _ => studio_ui::ds::text::meta("Searching…").into_any_element(),
        };
        let keys = if replaces {
            format!("↑↓ choose · ↵ replace · esc cancel · write · {branch}")
        } else {
            format!("↑↓ choose · ↵ add · esc cancel · write · {branch}")
        };
        v_flex()
            .key_context("EdgeSearch")
            .on_action(cx.listener(|this, _: &SelectNextSuggestion, _, cx| this.move_match(1, cx)))
            .on_action(
                cx.listener(|this, _: &SelectPreviousSuggestion, _, cx| this.move_match(-1, cx)),
            )
            .on_action(
                cx.listener(|this, _: &menu::Confirm, window, cx| this.confirm_add(window, cx)),
            )
            .on_action(cx.listener(|this, _: &menu::Cancel, _, cx| this.cancel_add(cx)))
            .pl(px(CHILD_INDENT))
            .py(px(space::XS))
            .gap(px(space::XS))
            .child(
                studio_ui::ds::field_frame(false, cx)
                    .child(studio_ui::ds::icon::meta(IconName::MagnifyingGlass))
                    .child(studio_ui::ds::field_input(adding.editor.clone())),
            )
            .child(found)
            .child(studio_ui::ds::text::meta(keys))
            .into_any_element()
    }

    /// A neighbour's ×, shown under the pointer: the first press asks, the second removes the
    /// relationship (the node stays).
    fn render_remove(
        &self,
        index: usize,
        n: usize,
        edge_id: String,
        row: SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let asked = self.removing.as_deref() == Some(edge_id.as_str());
        let edge = self.groups[index].group.edge.clone();
        if asked {
            return studio_ui::ds::row_button(
                SharedString::from(format!("peek-remove-{index}-{n}")),
                "Remove",
            )
            .style(ButtonStyle::Tinted(ui::TintColor::Error))
            .tooltip(Tooltip::text(format!(
                "Remove this {edge}; both nodes stay"
            )))
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.press_remove(index, edge_id.clone(), window, cx)
            }))
            .into_any_element();
        }
        studio_ui::ds::row_icon_button(
            SharedString::from(format!("peek-remove-{index}-{n}")),
            IconName::Close,
        )
        .icon_color(Color::Muted)
        .visible_on_hover(row)
        .tooltip(Tooltip::text(format!("Remove this {edge}")))
        .on_click(cx.listener(move |this, _, window, cx| {
            cx.stop_propagation();
            this.press_remove(index, edge_id.clone(), window, cx)
        }))
        .into_any_element()
    }

    fn render_neighbours(&self, index: usize, group: &Group, cx: &mut Context<Self>) -> AnyElement {
        let hover = studio_ui::ds::surface::hover(cx);
        match &group.neighbours {
            Loadable::Loaded(neighbours) if neighbours.is_empty() => div()
                .pl(px(CHILD_INDENT))
                .child(studio_ui::ds::text::meta("None at this commit"))
                .into_any_element(),
            Loadable::Loaded(neighbours) => {
                let more = match &group.count {
                    Loadable::Loaded(n) if *n > neighbours.len() as i64 => {
                        Some(*n - neighbours.len() as i64)
                    }
                    _ => None,
                };
                let other = group.group.other_type.clone();
                let at_commit = self.held_commit();
                let show_more = more.map(|more| {
                    let label = format!("+{} more", format_count(more));
                    if self.page {
                        let reading = group.more.is_some();
                        studio_ui::ds::row_button(
                            SharedString::from(format!("peek-more-{index}")),
                            if reading {
                                "Reading…".to_owned()
                            } else {
                                label
                            },
                        )
                        .style(ButtonStyle::Subtle)
                        .disabled(reading)
                        .on_click(cx.listener(move |this, _, _, cx| this.show_more(index, cx)))
                        .into_any_element()
                    } else {
                        // The peek shows the first few; the page lists them all.
                        let open = self.open_page_action();
                        div()
                            .id(SharedString::from(format!("peek-more-{index}")))
                            .px(px(space::XS))
                            .cursor_pointer()
                            .tooltip(Tooltip::text("Open the node's page to see them all"))
                            .on_click(move |_, window, cx| {
                                if let Some(open) = open.clone() {
                                    window.dispatch_action(Box::new(open), cx)
                                }
                            })
                            .child(studio_ui::ds::text::meta(format!("{label} · Open page")))
                            .into_any_element()
                    }
                });
                let removes = self.edits_relationships();
                v_flex()
                    .pl(px(CHILD_INDENT - space::XS))
                    .children(neighbours.iter().enumerate().map(|(n, neighbour)| {
                        let (peek, page) = node_actions(&other, &neighbour.id, at_commit.clone());
                        let row_group = SharedString::from(format!("peek-neighbour-{index}-{n}"));
                        let remove = neighbour
                            .edge_id
                            .clone()
                            .filter(|_| removes)
                            .map(|edge_id| {
                                self.render_remove(index, n, edge_id, row_group.clone(), cx)
                            });
                        h_flex()
                            .id(SharedString::from(format!("peek-neighbour-{index}-{n}")))
                            .group(row_group)
                            .px(px(space::XS))
                            .py(px(space::XXS))
                            .gap(px(space::MD))
                            .rounded(px(studio_ui::ds::radii(cx).sm))
                            .hover(move |s| s.bg(hover))
                            .cursor_pointer()
                            .tooltip(Tooltip::text(format!(
                                "Peek {} · ⌘-click opens its page",
                                neighbour.id
                            )))
                            .on_click(move |event, window, cx| {
                                if event.modifiers().platform {
                                    window.dispatch_action(Box::new(page.clone()), cx)
                                } else {
                                    window.dispatch_action(Box::new(peek.clone()), cx)
                                }
                            })
                            .child(studio_ui::type_swatch(&other, cx))
                            .child(
                                studio_ui::ds::text::body(
                                    neighbour
                                        .label
                                        .clone()
                                        .unwrap_or_else(|| neighbour.id.clone()),
                                )
                                .truncate(),
                            )
                            .when(neighbour.label.is_some(), |d| {
                                d.child(studio_ui::ds::text::meta(neighbour.id.clone()).truncate())
                            })
                            .child(div().flex_1())
                            .children(remove)
                    }))
                    .children(show_more)
                    .into_any_element()
            }
            Loadable::Failed(error) => div()
                .pl(px(CHILD_INDENT))
                .child(studio_ui::ds::text::error_note(error.clone()))
                .into_any_element(),
            _ => div()
                .pl(px(CHILD_INDENT))
                .child(studio_ui::ds::text::meta("Reading…"))
                .into_any_element(),
        }
    }

    fn render_endpoints(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let PeekSubject::Edge {
            from_type, to_type, ..
        } = &self.subject
        else {
            return None;
        };
        let values = self.values.loaded()?;
        let hover = studio_ui::ds::surface::hover(cx);
        let at_commit = self.held_commit();
        let endpoint = |label: &'static str, ty: &String, key: &str| {
            let id = values.get(key).and_then(Value::as_str)?.to_owned();
            let (peek, page) = node_actions(ty, &id, at_commit.clone());
            Some(
                h_flex()
                    .id(label)
                    .px(px(space::XS))
                    .py(px(space::XXS))
                    .gap(px(space::MD))
                    .rounded(px(studio_ui::ds::radii(cx).sm))
                    .hover(move |s| s.bg(hover))
                    .cursor_pointer()
                    .tooltip(Tooltip::text(format!("Peek {id} · ⌘-click opens its page")))
                    .on_click(move |event, window, cx| {
                        if event.modifiers().platform {
                            window.dispatch_action(Box::new(page.clone()), cx)
                        } else {
                            window.dispatch_action(Box::new(peek.clone()), cx)
                        }
                    })
                    .child(
                        div()
                            .w(px(studio_ui::ds::size::SHORT_LABEL))
                            .child(studio_ui::ds::text::muted(label)),
                    )
                    .child(studio_ui::type_swatch(ty, cx))
                    .child(studio_ui::ds::text::body(id))
                    .child(studio_ui::ds::text::meta(ty.clone())),
            )
        };
        Some(
            v_flex()
                .gap(px(space::XXS))
                .children(endpoint("from", from_type, FROM_COLUMN))
                .children(endpoint("to", to_type, TO_COLUMN))
                .into_any_element(),
        )
    }
}

/// Peeking a node and opening its page, at the commit the view holds or the head.
fn node_actions(type_name: &str, id: &str, at_commit: Option<String>) -> (PeekNode, OpenNodePage) {
    (
        PeekNode {
            type_name: type_name.to_owned(),
            id: id.to_owned(),
            at_commit: at_commit.clone(),
        },
        OpenNodePage {
            type_name: type_name.to_owned(),
            id: id.to_owned(),
            at_commit,
        },
    )
}

/// Where a relationship group's row sits (`NodePeek::place`); `active` when the person is
/// working in it. A group read again after a write is listed until its count says it has none,
/// and one whose count failed is listed with why.
fn place_of(group: &Group, active: bool) -> Place {
    if group.as_drawn {
        Place::AsDrawn
    } else if !active && matches!(group.count, Loadable::Loaded(0)) {
        Place::Folded
    } else {
        Place::Listed
    }
}

/// How far Zed's editor sets its text in from its left edge without a gutter: the font's
/// descent, its gutter margin (`GutterDimensions::default_gutter_margin`).
fn editor_inset(window: &Window, cx: &App) -> gpui::Pixels {
    let font = window.text_style().font();
    let size = studio_ui::ds::text::body_size(cx).to_pixels(window.rem_size());
    let text = cx.text_system();
    -text.descent(text.resolve_font(&font), size)
}

/// A text value rendered as markdown as an agent reply is, with headings near the body's size,
/// at the size of the labels beside it and its soft line breaks kept, so plain lines stay lines
/// (spec 014).
fn value_style(window: &Window, cx: &App) -> MarkdownStyle {
    let mut style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx);
    style.base_text_style.font_size = studio_ui::ds::text::body_size(cx).into();
    style.base_text_style.line_height = gpui::relative(1.5);
    style.soft_break_as_hard_break = true;
    style
}

fn neighbours_from(rows: &Value) -> Vec<Neighbour> {
    rows.as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    Some(Neighbour {
                        id: row.get("id")?.as_str()?.to_owned(),
                        label: row.get("label").and_then(Value::as_str).map(str::to_owned),
                        edge_id: row
                            .get("edge_id")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn section(title: &'static str) -> impl IntoElement {
    div()
        .pt(px(space::MD))
        .pb(px(space::XS))
        .child(studio_ui::ds::text::meta(title))
}

impl Render for NodePeek {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_markdown(cx);
        self.render_list(cx)
    }
}

impl NodePeek {
    /// Whether the relationships are listed: once a node is read. While it loads, or when it
    /// does not exist at this commit, an empty list would say its type has none.
    fn shows_relationships(&self) -> bool {
        matches!(self.subject, PeekSubject::Node { .. }) && self.values.loaded().is_some()
    }

    /// What the view's list holds now, in order: the head; the groups listed; those laid out
    /// without their count, each in its place; then the empty row, with the groups under it
    /// that show.
    fn list_items(&self) -> Vec<Item> {
        let mut items = vec![Item::Head];
        if !self.shows_relationships() {
            return items;
        }
        if self.groups.is_empty() {
            items.push(Item::Note("This type takes part in no relationships."));
            return items;
        }
        let (mut listed, mut as_drawn, mut folded) = (Vec::new(), Vec::new(), Vec::new());
        for index in 0..self.groups.len() {
            match self.place(index) {
                Place::Listed => listed.push(Item::Group(index)),
                Place::AsDrawn => as_drawn.push(Item::Group(index)),
                Place::Folded => folded.push(index),
            }
        }
        items.extend(listed);
        items.extend(as_drawn);
        if !folded.is_empty() {
            items.push(Item::Empty);
            items.extend(
                folded
                    .into_iter()
                    .filter(|&index| self.folded_shown(index))
                    .map(Item::Group),
            );
        }
        items
    }

    /// The view's items as a list, told what changed since it was last drawn.
    fn render_list(&mut self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let items = self.list_items();
        if items != self.list_items {
            // Only the items between an unchanged start and an unchanged end are replaced:
            // the others keep their place, and the list keeps showing what is in view.
            let old = &self.list_items;
            let start = items
                .iter()
                .zip(old)
                .take_while(|(new, old)| new == old)
                .count();
            let end = items[start..]
                .iter()
                .rev()
                .zip(old[start..].iter().rev())
                .take_while(|(new, old)| new == old)
                .count();
            self.list
                .splice(start..old.len() - end, items.len() - start - end);
            self.list_items = items;
        }
        // What an item holds may have changed (a group's neighbours came in, a field opened):
        // those drawn are measured again, the others keep the height they had.
        self.list.remeasure();
        div()
            .id("node-peek")
            .key_context("NodeView")
            .on_action(cx.listener(Self::save_drafts))
            .on_action(cx.listener(Self::on_save_field))
            .on_action(cx.listener(Self::on_cancel_field))
            .on_action(cx.listener(Self::on_expand_field))
            .size_full()
            .child(
                list(
                    self.list.clone(),
                    cx.processor(|this, ix: usize, window, cx| {
                        this.render_list_item(ix, window, cx)
                    }),
                )
                .size_full(),
            )
    }

    fn render_list_item(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let page = self.page;
        let last = ix + 1 == self.list_items.len();
        let item = match self.list_items.get(ix).copied() {
            Some(Item::Head) => {
                let head = self.render_head(window, cx);
                let head = if page {
                    head.pt(px(space::XL)).gap(px(space::MD))
                } else {
                    head.pt(px(space::XL)).gap(px(space::SM))
                };
                head.when(self.shows_relationships(), |head| {
                    head.child(section("Relationships"))
                })
                .into_any_element()
            }
            Some(Item::Note(text)) => studio_ui::ds::text::muted(text).into_any_element(),
            Some(Item::Group(index)) if index < self.groups.len() => {
                self.drawn(index, window, cx);
                self.render_group(index, cx).into_any_element()
            }
            Some(Item::Empty) => self.render_empty_row(cx),
            _ => div().into_any_element(),
        };
        // A page reads at a document's width, with its margins; the peek fills its dock.
        div()
            .w_full()
            .map(|item| {
                if page {
                    item.max_w(px(PAGE_WIDTH)).px(px(space::PAGE))
                } else {
                    item.px(px(space::XL))
                }
            })
            .pt(px(space::XXS))
            .when(last, |item| item.pb(px(space::XL)))
            .child(match self.list_items.get(ix) {
                // A group's rows carry an `XS` inset for their hover fill; pulled out by it,
                // their text sits on the section labels' edge.
                Some(Item::Group(_) | Item::Empty) => div().mx(px(-space::XS)).child(item),
                _ => div().child(item),
            })
            .into_any_element()
    }

    /// Everything above the relationship groups: the node's type and name, its commit, drafts,
    /// endpoints, properties and Blobs.
    fn render_head(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let type_name = self.subject.type_name().to_owned();
        let id = self.subject.id().to_owned();
        let label = self.label();
        let at = self.snapshot.clone();
        let page = self.page;
        // The peek opens the node's page; the page is it.
        let open_page = (!page).then(|| self.open_page_action()).flatten();
        let has_properties = self
            .schema
            .as_ref()
            .is_some_and(|schema| !listed_properties(schema, &self.subject).is_empty());
        let properties = self.render_properties(window, cx);
        let endpoints = self.render_endpoints(cx);
        let is_node = matches!(self.subject, PeekSubject::Node { .. });
        let drafts_bar = self.render_drafts_bar(cx);
        v_flex()
            .w_full()
            .child(
                h_flex()
                    .gap(px(space::SM))
                    .child(studio_ui::type_swatch(&type_name, cx))
                    .child(
                        studio_ui::ds::text::muted(type_name.clone()),
                    )
                    .child(div().flex_1())
                    .children(open_page.map(|open| {
                        studio_ui::ds::toolbar_icon_button("peek-open-page", IconName::ArrowUpRight)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Open the node's page"))
                            .on_click(move |_, window, cx| {
                                window.dispatch_action(Box::new(open.clone()), cx)
                            })
                    }))
                    .when(is_node, |header| {
                        let explore = studio_actions::graph::ShowNodeOnCanvas {
                            type_name: type_name.clone(),
                            id: id.clone(),
                        };
                        header.child(
                            studio_ui::ds::toolbar_button("peek-explore", "Explore")
                                .style(ButtonStyle::Subtle)
                                .start_icon(
                                    Icon::new(IconName::Crosshair),
                                )
                                .tooltip(Tooltip::text("Walk the graph from this node"))
                                .on_click(move |_, window, cx| {
                                    window.dispatch_action(Box::new(explore.clone()), cx)
                                }),
                        )
                    }),
            )
            .child({
                let title = label.unwrap_or_else(|| id.clone());
                if page {
                    studio_ui::ds::text::page_title(title)
                } else {
                    studio_ui::ds::text::heading(title)
                }
            })
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_x_3()
                    .gap_y_1()
                    .child(studio_ui::ds::meta_code(IconName::Hash, id, cx))
                    .children(at.map(|commit| {
                        div()
                            .id("peek-commit")
                            .tooltip(Tooltip::text(format!("Read at commit {commit}")))
                            .child(studio_ui::ds::meta_code(
                                IconName::GitCommit,
                                studio_ui::short_commit_id(&commit),
                                cx,
                            ))
                    }))
                    // A node opened by id reads again by id; an edge is read with its table.
                    .when(self.newer && is_node, |meta| {
                        meta.child(
                            studio_ui::ds::row_button("peek-newer", "Newer data · Refresh")
                                .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                                .tooltip(Tooltip::text(
                                    "The branch moved since this was read. Refresh reads it at the head.",
                                ))
                                .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                        )
                    }),
            )
            .children(drafts_bar)
            .children(
                endpoints.map(|endpoints| {
                    v_flex()
                        .child(section("Endpoints"))
                        .child(div().mx(px(-space::XS)).child(endpoints))
                }),
            )
            .when(has_properties, |d| {
                d.child(section("Properties")).child(properties)
            })
            .when(!self.blobs.is_empty(), |d| {
                d.child(section("Blobs")).child(self.render_blobs(cx))
            })
    }
}

/// What a property the commit shown does not hold under its name was then, in a sentence's
/// case (`capital`) or a header's.
pub(crate) fn absence_text(then: &PropertyThen, capital: bool) -> String {
    let text = match then {
        PropertyThen::Absent => "added after this commit".to_owned(),
        PropertyThen::Named(name) => format!("named “{name}” at this commit"),
        PropertyThen::Unwitnessed => "not readable at this commit".to_owned(),
    };
    if capital {
        let mut chars = text.chars();
        chars
            .next()
            .map(|first| first.to_uppercase().chain(chars).collect())
            .unwrap_or_default()
    } else {
        text
    }
}

/// The properties a view lists for its subject, in schema order. Blob values are shown under
/// Blobs, with their previews.
fn listed_properties<'a>(
    schema: &'a SchemaModel,
    subject: &PeekSubject,
) -> Vec<&'a gq_language::Property> {
    let properties = match subject {
        PeekSubject::Node { type_name, .. } => schema.node(type_name).map(|node| &node.properties),
        PeekSubject::Edge { edge_type, .. } => schema.edge(edge_type).map(|edge| &edge.properties),
    };
    properties
        .into_iter()
        .flatten()
        .filter(|p| p.projectable())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use serde_json::json;

    use crate::test_support::{
        PEOPLE, changed, session, session_answering, session_followed, session_routing,
    };

    fn page(
        session: Entity<GraphSession>,
        cx: &mut TestAppContext,
    ) -> (Entity<NodePeek>, &mut VisualTestContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (view, cx) = cx.add_window_view(move |_, cx| {
            NodePeek::page(session, "Person".into(), "p1".into(), None, cx)
        });
        cx.run_until_parked();
        (view, cx)
    }

    fn edit_name(view: &Entity<NodePeek>, to: &str, cx: &mut VisualTestContext) {
        view.update_in(cx, |view, window, cx| {
            let name = view.schema.as_ref().unwrap().node("Person").unwrap();
            let property = name.property("name").unwrap().clone();
            view.start_edit(property, window, cx);
            let editor = view.edits["name"].editor.clone();
            editor.update(cx, |editor, cx| editor.set_text(to, window, cx));
            view.save_field("name", window, cx);
        });
        cx.run_until_parked();
    }

    /// A saved field shows its new value, and the view moves to its own commit without
    /// calling itself behind (spec 014).
    #[gpui::test]
    async fn a_saved_field_moves_the_view_to_its_own_commit(cx: &mut TestAppContext) {
        let (view, cx) = page(session(PEOPLE, cx), cx);
        view.read_with(cx, |view, _| {
            assert_eq!(view.snapshot.as_deref(), Some("c1"))
        });
        edit_name(&view, "Ada Lovelace", cx);
        view.read_with(cx, |view, _| {
            assert_eq!(view.value_of("name"), json!("Ada Lovelace"));
            assert_eq!(view.snapshot.as_deref(), Some("c2"), "its own commit");
            assert!(!view.newer, "its own write does not make it behind");
            assert!(view.saving.is_empty() && view.edits.is_empty());
        });
    }

    /// A page at the branch head follows the window to another branch and reads its node there.
    /// A write sent before the move lets go of the draft of the branch it was sent on, never
    /// the one of the branch shown now, and leaves what the page shows alone.
    #[gpui::test]
    async fn a_page_follows_its_branch_and_a_write_from_the_one_left_stays_there(
        cx: &mut TestAppContext,
    ) {
        let session = session(PEOPLE, cx);
        let (view, cx) = page(session.clone(), cx);
        let key = DraftKey {
            type_name: "Person".into(),
            id: "p1".into(),
            property: "name".into(),
        };
        session.update(cx, |session, cx| {
            session.set_branch("fix".into(), cx);
            let draft = DraftEntry {
                old: json!("Ada"),
                text: "Ada King".into(),
            };
            session.set_draft(key.clone(), draft, cx);
            session.set_branch("main".into(), cx);
        });
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let person = view.schema.as_ref().unwrap().node("Person").unwrap();
            let property = person.property("name").unwrap().clone();
            view.start_edit(property, window, cx);
            let editor = view.edits["name"].editor.clone();
            editor.update(cx, |editor, cx| editor.set_text("Ada Lovelace", window, cx));
            view.save_field("name", window, cx);
        });
        session.update(cx, |session, cx| session.set_branch("fix".into(), cx));
        view.read_with(cx, |view, _| assert_eq!(view.branch, "fix"));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.value_of("name"), json!("Ada"), "read on fix");
            assert!(view.saving.is_empty());
        });
        session.read_with(cx, |session, _| {
            assert_eq!(session.branch, "fix");
            assert_eq!(
                session.draft(&key).map(|draft| draft.text.as_str()),
                Some("Ada King"),
                "fix's draft stays"
            );
        });
    }

    /// A changed field left on the page is a draft of the session's, which a table of the type
    /// shows too; ⌘S on the page saves it as one commit and lets it go everywhere (spec 014).
    #[gpui::test]
    async fn a_field_left_changed_is_a_draft_the_table_shows_and_save_lands(
        cx: &mut TestAppContext,
    ) {
        let session = session(PEOPLE, cx);
        let (view, cx) = page(session.clone(), cx);
        view.update_in(cx, |view, window, cx| {
            let person = view.schema.as_ref().unwrap().node("Person").unwrap();
            let property = person.property("name").unwrap().clone();
            view.start_edit(property, window, cx);
            let editor = view.edits["name"].editor.clone();
            editor.update(cx, |editor, cx| editor.set_text("Ada Lovelace", window, cx));
            view.left_field("name", cx);
            assert!(view.edits.is_empty(), "leaving closes the field");
        });
        let key = DraftKey {
            type_name: "Person".into(),
            id: "p1".into(),
            property: "name".into(),
        };
        session.read_with(cx, |session, _| {
            assert_eq!(
                session.draft(&key).map(|d| d.text.as_str()),
                Some("Ada Lovelace")
            );
        });
        let table = cx.update(|window, cx| {
            cx.new(|cx| {
                crate::TypeTable::new(
                    gpui::WeakEntity::new_invalid(),
                    session.clone(),
                    "Person".into(),
                    None,
                    window,
                    cx,
                )
            })
        });
        table.read_with(cx, |table, _| {
            assert_eq!(
                table.draft_text("p1", "name").as_deref(),
                Some("Ada Lovelace"),
                "the table of the type shows the page's draft"
            );
        });
        view.update_in(cx, |view, window, cx| {
            view.save_drafts(&SaveNodeDrafts, window, cx)
        });
        cx.run_until_parked();
        session.read_with(cx, |session, _| assert_eq!(session.draft(&key), None));
        table.read_with(cx, |table, _| {
            assert_eq!(
                table.draft_text("p1", "name"),
                None,
                "saved, it goes everywhere"
            )
        });
        view.read_with(cx, |view, _| {
            assert_eq!(view.value_of("name"), json!("Ada Lovelace"));
            assert_eq!(view.snapshot.as_deref(), Some("c2"));
        });
    }

    const KNOWS: &str =
        "node Person {\n  slug: String @key\n  name: String\n}\nedge Knows: Person -> Person\n";

    /// A page reads each relationship group once: its first neighbours and one more, which is
    /// the group's exact count when they fit. Only a group past the page reads its count.
    #[gpui::test]
    async fn a_group_that_fits_its_page_is_read_once(cx: &mut TestAppContext) {
        let neighbours = |n: usize| {
            Value::from(
                (0..n)
                    .map(|i| json!({ "id": format!("p{:03}", i + 10), "edge_id": format!("e{i}") }))
                    .collect::<Vec<_>>(),
            )
        };
        let (session, asked) = session_routing(
            KNOWS,
            move |request| {
                let query = request["query"].as_str().unwrap_or_default();
                match request["name"].as_str() {
                    Some("studio_node") => {
                        json!([{ "n": { "@id": "p1", "slug": "p1", "name": "Ada" } }])
                    }
                    Some("studio_neighbours") if query.contains("$n $e:knows $m") => neighbours(3),
                    Some("studio_neighbours") => neighbours(PAGE_NEIGHBOURS as usize + 1),
                    Some("studio_page_degrees") => json!([{ "id": "p1", "n": 120 }]),
                    _ => json!([]),
                }
            },
            cx,
        );
        let (view, cx) = page(session, cx);
        view.read_with(cx, |view, _| {
            let group = |direction: Direction| {
                view.groups
                    .iter()
                    .find(|group| group.group.direction == direction)
                    .unwrap()
            };
            let out = group(Direction::Outgoing);
            assert_eq!(out.count, Loadable::Loaded(3), "the read was the count");
            assert_eq!(out.neighbours.loaded().map(Vec::len), Some(3));
            assert!(out.expanded, "a page opens every group with neighbours");
            let back = group(Direction::Incoming);
            assert_eq!(back.count, Loadable::Loaded(120), "counted past the page");
            assert_eq!(
                back.neighbours.loaded().map(Vec::len),
                Some(PAGE_NEIGHBOURS as usize),
                "the page shows a page"
            );
        });
        let asked = asked.lock().unwrap().clone();
        let reads = |name: &str| asked.iter().filter(|asked| *asked == name).count();
        assert_eq!(
            reads("studio_neighbours"),
            2,
            "one read per group: {asked:?}"
        );
        assert_eq!(
            reads("studio_page_degrees"),
            1,
            "a count only past the page: {asked:?}"
        );
    }

    /// A type with many relationships reads only the groups whose rows are drawn, and a few
    /// rows past them; scrolling reads the rest, each once. A group read as having none keeps
    /// its row, so its count arriving pulls no further group into view.
    #[gpui::test]
    async fn only_the_groups_drawn_are_read_and_scrolling_reads_the_rest_once(
        cx: &mut TestAppContext,
    ) {
        let mut schema = PEOPLE.to_owned();
        for n in 0..30 {
            schema.push_str(&format!("edge Rel{n:02}: Person -> Person\n"));
        }
        let read = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = read.clone();
        let (session, _) = session_routing(
            &schema,
            move |request| match request["name"].as_str() {
                Some("studio_node") => {
                    json!([{ "n": { "@id": "p1", "slug": "p1", "name": "Ada" } }])
                }
                Some("studio_neighbours") => {
                    let query = request["query"].as_str().unwrap_or_default().to_owned();
                    seen.lock().unwrap().push(query);
                    json!([])
                }
                _ => json!([]),
            },
            cx,
        );
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let window = cx.open_window(gpui::size(px(480.), px(360.)), move |_, cx| {
            NodePeek::page(session, "Person".into(), "p1".into(), None, cx)
        });
        let view = window.root(cx).unwrap();
        let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
        cx.run_until_parked();

        let drawn = |cx: &mut VisualTestContext| {
            view.read_with(cx, |view, _| {
                view.groups.iter().take_while(|group| group.drawn).count()
            })
        };
        let groups = view.read_with(cx, |view, _| view.groups.len());
        assert_eq!(groups, 60, "each relationship out and in");
        let first = drawn(cx);
        assert!(
            0 < first && first < groups / 2,
            "only the rows in view and a few past them: {first} of {groups}"
        );
        view.read_with(cx, |view, _| {
            for (index, group) in view.groups.iter().enumerate() {
                assert_eq!(group.drawn, index < first, "the rows from the top");
                let counted = matches!(group.count, Loadable::Loaded(0));
                assert_eq!(counted, group.drawn, "read once drawn, and only then");
                assert_eq!(view.place(index), Place::AsDrawn, "each keeps its row");
            }
        });
        assert_eq!(
            read.lock().unwrap().len(),
            first,
            "one read per group drawn"
        );

        let mut scrolls = 0;
        while drawn(cx) < groups {
            scrolls += 1;
            assert!(scrolls < 100, "scrolling reaches the last group");
            view.update(cx, |view, cx| {
                view.list.scroll_by(px(120.));
                cx.notify();
            });
            cx.run_until_parked();
        }
        let read = read.lock().unwrap().clone();
        let once: HashSet<&String> = read.iter().collect();
        assert_eq!(read.len(), groups, "every group read: {read:?}");
        assert_eq!(once.len(), groups, "each once");
    }

    /// A peek of a table's row shows what the table counted without reading it again, and
    /// opens a small group, reading the neighbours the table did not list once its row is
    /// drawn; the groups the table did not count are read as drawn.
    #[gpui::test]
    async fn a_peek_reads_only_what_the_table_did_not(cx: &mut TestAppContext) {
        const KNOWS_AND_LIKES: &str = "node Person {\n  slug: String @key\n  name: String\n}\nedge Knows: Person -> Person\nedge Likes: Person -> Person\n";
        let read = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = read.clone();
        let (session, _) = session_routing(
            KNOWS_AND_LIKES,
            move |request| match request["name"].as_str() {
                Some("studio_neighbours") => {
                    let query = request["query"].as_str().unwrap_or_default().to_owned();
                    seen.lock().unwrap().push(query);
                    json!([{ "id": "p010", "edge_id": "e1" }, { "id": "p011", "edge_id": "e2" }])
                }
                _ => json!([]),
            },
            cx,
        );
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        cx.run_until_parked();
        let schema = session.read_with(cx, |session, _| session.schema_model().unwrap());
        let relationship = |edge: &str, direction: Direction| {
            schema
                .relationships_of("Person")
                .iter()
                .find(|group| group.edge == edge && group.direction == direction)
                .unwrap()
                .clone()
        };
        let knows = relationship("Knows", Direction::Outgoing);
        let likes = relationship("Likes", Direction::Outgoing);
        let first = vec![Neighbour {
            id: "p010".into(),
            label: None,
            edge_id: None,
        }];
        let known = HashMap::from([
            (
                builder::relationship_key(&knows),
                KnownRelation {
                    count: 1,
                    first: Some(first),
                },
            ),
            (
                builder::relationship_key(&likes),
                KnownRelation {
                    count: 2,
                    first: None,
                },
            ),
        ]);
        let values: Map<String, Value> =
            serde_json::from_value(json!({ "@id": "p1", "slug": "p1", "name": "Ada" })).unwrap();
        let (view, cx) = cx.add_window_view(move |_, cx| {
            NodePeek::new(
                session,
                PeekSubject::Node {
                    type_name: "Person".into(),
                    id: "p1".into(),
                },
                Some(values),
                Some("c1".into()),
                known,
                false,
                cx,
            )
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            for group in &view.groups {
                assert!(group.drawn, "every row fits");
                assert!(group.expanded, "a small group opens");
                let n = group.neighbours.loaded().map(Vec::len);
                if group.group == knows {
                    assert_eq!(n, Some(1), "as the table listed them");
                } else {
                    assert_eq!(n, Some(2), "as read");
                }
            }
        });
        let read = read.lock().unwrap().clone();
        assert_eq!(
            read.len(),
            3,
            "likes' neighbours, and both groups in: {read:?}"
        );
        assert!(
            !read.iter().any(|query| query.contains("$n $e:knows $m")),
            "knows out as the table read it: {read:?}"
        );
    }

    /// At a commit made before a property was added, the whole node is refused, naming it: the
    /// page reads the rest as fields and shows the property as added after the commit.
    #[gpui::test]
    async fn a_page_at_a_commit_before_a_property_was_added_shows_the_rest(
        cx: &mut TestAppContext,
    ) {
        const BORN: &str = "node Person {\n  slug: String @key\n  name: String\n  born: Date?\n}\n";
        let (session, asked) = session_routing(
            BORN,
            |request| match request["name"].as_str() {
                Some("studio_node") => json!({
                    "refuse": "property 'born' of node type 'Person' does not exist at this snapshot"
                }),
                Some("studio_node_fields") => {
                    assert!(!request["query"].as_str().unwrap().contains("born"));
                    json!([{ "id": "p1", "v_slug": "p1", "v_name": "Ada" }])
                }
                _ => json!([]),
            },
            cx,
        );
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (view, cx) = cx.add_window_view(move |_, cx| {
            NodePeek::page(session, "Person".into(), "p1".into(), Some("c0".into()), cx)
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let values = view.values.loaded().expect("the rest is read");
            assert_eq!(values.get("name"), Some(&json!("Ada")));
            assert_eq!(values.get("@id"), Some(&json!("p1")));
            assert_eq!(
                view.absent.get("born"),
                Some(&PropertyThen::Absent),
                "added after this commit"
            );
            assert!(
                view.scope.is_none(),
                "a page held at a commit watches no reads"
            );
        });
        let names = asked.lock().unwrap().clone();
        assert_eq!(
            names
                .iter()
                .filter(|n| n.starts_with("studio_node"))
                .collect::<Vec<_>>(),
            ["studio_node", "studio_node_fields"]
        );
    }

    /// At a commit made before an edge type existed, its groups had no neighbours; one made
    /// before an edge property was added reads the group without it (spec 015).
    #[gpui::test]
    async fn groups_at_a_commit_before_their_edge_or_its_property_existed(cx: &mut TestAppContext) {
        const SCHEMA: &str = "node Person {\n  slug: String @key\n  name: String\n}\nedge Knows: Person -> Person\nedge Likes: Person -> Person {\n  since: I32?\n}\n";
        let (session, _) = session_routing(
            SCHEMA,
            |request| {
                let query = request["query"].as_str().unwrap_or_default();
                match request["name"].as_str() {
                    Some("studio_node") => {
                        json!([{ "n": { "@id": "p1", "slug": "p1", "name": "Ada" } }])
                    }
                    Some("studio_neighbours") if query.contains(":knows") => json!({
                        "refuse": "edge type 'Knows' does not exist at this snapshot"
                    }),
                    Some("studio_neighbours") if query.contains("e_since") => json!({
                        "refuse": "property 'since' of edge type 'Likes' does not exist at this snapshot"
                    }),
                    Some("studio_neighbours") => {
                        json!([{ "id": "p010", "edge_id": "e1", "label": "Grace" }])
                    }
                    _ => json!([]),
                }
            },
            cx,
        );
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (view, cx) = cx.add_window_view(move |_, cx| {
            NodePeek::page(session, "Person".into(), "p1".into(), Some("c0".into()), cx)
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            for group in &view.groups {
                match group.group.edge.as_str() {
                    "Knows" => {
                        assert_eq!(group.count, Loadable::Loaded(0), "no Knows at the commit");
                    }
                    _ => {
                        assert_eq!(group.count, Loadable::Loaded(1));
                        assert_eq!(group.neighbours.loaded().map(Vec::len), Some(1));
                    }
                }
            }
            assert_eq!(
                view.groups_absent,
                [("Likes".to_owned(), "since".to_owned())]
            );
        });
    }

    /// Another writer's commit that changed nothing the page shows moves the page to it without
    /// reading again: not the node, no edge at it, no neighbour it lists. One that changed a
    /// listed neighbour offers Refresh (spec 015).
    #[gpui::test]
    async fn a_page_moves_to_a_commit_that_changed_nothing_it_shows(cx: &mut TestAppContext) {
        let reads = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = reads.clone();
        let (session, writer) = session_followed(
            KNOWS,
            move |request| {
                let name = request["name"].as_str().unwrap_or_default();
                seen.lock().unwrap().push(name.to_owned());
                match name {
                    "studio_node" => json!([{ "n": { "@id": "p1", "slug": "p1", "name": "Ada" } }]),
                    "studio_neighbours" => json!([
                        { "id": "p010", "edge_id": "e1" },
                        { "id": "p011", "edge_id": "e2" },
                    ]),
                    _ => json!([]),
                }
            },
            cx,
        );
        let (view, cx) = page(session.clone(), cx);
        view.read_with(cx, |view, _| {
            assert_eq!(view.snapshot.as_deref(), Some("c1"));
        });
        let read_before = reads.lock().unwrap().len();

        writer.commit(
            "c2",
            vec![
                changed("node", "Person", "p999", None),
                changed("edge", "Knows", "e9", Some(("p998", "p999"))),
            ],
        );
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.snapshot.as_deref(), Some("c2"), "moved to the head");
            assert!(!view.newer);
        });
        let read_after: Vec<String> = reads.lock().unwrap()[read_before..].to_vec();
        assert_eq!(read_after, ["studio_head"], "nothing read again");

        writer.commit("c3", vec![changed("node", "Person", "p010", None)]);
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        assert!(session.read_with(cx, |session, _| matches!(
            session.changes_since("main", "c2", "c3"),
            graph_store::carry::Since::Known(_)
        )));
        view.read_with(cx, |view, _| {
            assert_eq!(view.snapshot.as_deref(), Some("c2"));
            assert!(view.newer, "a neighbour it lists changed");
        });
    }

    /// A schema changed elsewhere leaves the page as read and offers Refresh; Refresh takes it,
    /// and the page's next head move asks the feed about the relationships it added.
    #[gpui::test]
    async fn refresh_takes_a_schema_changed_elsewhere_and_watches_its_types(
        cx: &mut TestAppContext,
    ) {
        let (session, writer) = session_followed(
            PEOPLE,
            |request| match request["name"].as_str() {
                Some("studio_node") => {
                    json!([{ "n": { "@id": "p1", "slug": "p1", "name": "Ada" } }])
                }
                _ => json!([]),
            },
            cx,
        );
        let (view, cx) = page(session.clone(), cx);
        let grown = format!("{PEOPLE}edge Knows: Person -> Person\n");
        writer.set_schema(&grown);
        writer.commit("c2", vec![changed("node", "Person", "p9", None)]);
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.newer, "Refresh is offered");
            assert!(view.groups.is_empty(), "still as read: no relationships");
        });

        view.update(cx, |view, cx| view.refresh(cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(!view.newer);
            assert_eq!(view.groups.len(), 2, "Knows out and in");
        });
        writer.commit("c3", vec![changed("node", "Person", "p8", None)]);
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        let last = writer.feeds.lock().unwrap().last().cloned().unwrap();
        assert!(last.contains("type=Knows"), "{last}");
    }

    /// A copy takes the value as it is when clicked, and its mark comes down after its own
    /// pause: a refresh does not leave it up, and an earlier copy does not take it down.
    #[gpui::test]
    async fn a_copy_mark_comes_down_after_its_own_pause(cx: &mut TestAppContext) {
        let (session, _writer) = session_followed(
            PEOPLE,
            |request| match request["name"].as_str() {
                Some("studio_node") => {
                    json!([{ "n": { "@id": "p1", "slug": "p1", "name": "Ada" } }])
                }
                _ => json!([]),
            },
            cx,
        );
        let (view, cx) = page(session, cx);
        let copied = |cx: &mut VisualTestContext| view.read_with(cx, |view, _| view.copied.clone());
        view.update(cx, |view, cx| view.copy("name".into(), cx));
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("Ada")
        );
        view.update(cx, |view, cx| view.refresh(cx));
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(1500));
        cx.run_until_parked();
        assert_eq!(copied(cx), None, "down after a refresh too");

        view.update(cx, |view, cx| view.copy("name".into(), cx));
        cx.executor().advance_clock(Duration::from_millis(1000));
        view.update(cx, |view, cx| view.copy("slug".into(), cx));
        cx.executor().advance_clock(Duration::from_millis(600));
        cx.run_until_parked();
        assert_eq!(copied(cx).as_deref(), Some("slug"), "its own pause");
        cx.executor().advance_clock(Duration::from_millis(900));
        cx.run_until_parked();
        assert_eq!(copied(cx), None);
    }

    /// A peek of a row the table read shows what the table read: a commit to another node
    /// moves it along, one to its node offers Refresh, though the peek read no node itself.
    #[gpui::test]
    async fn a_peek_of_a_tables_row_follows_its_node(cx: &mut TestAppContext) {
        let (session, writer) = session_followed(PEOPLE, |_| json!([]), cx);
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        cx.run_until_parked();
        let values: Map<String, Value> =
            serde_json::from_value(json!({ "@id": "p1", "slug": "p1", "name": "Ada" })).unwrap();
        let (view, cx) = cx.add_window_view(move |_, cx| {
            NodePeek::new(
                session.clone(),
                PeekSubject::Node {
                    type_name: "Person".into(),
                    id: "p1".into(),
                },
                Some(values),
                Some("c1".into()),
                HashMap::new(),
                false,
                cx,
            )
        });
        cx.run_until_parked();
        let session = view.read_with(cx, |view, _| view.session.clone());
        let move_head = |cx: &mut VisualTestContext, commit: &str, id: &str| {
            writer.commit(commit, vec![changed("node", "Person", id, None)]);
            session.update(cx, |session, cx| session.check_heads(cx));
            cx.run_until_parked();
        };
        move_head(cx, "c2", "p9");
        view.read_with(cx, |view, _| {
            assert_eq!(view.snapshot.as_deref(), Some("c2"));
            assert!(!view.newer, "another person");
        });
        move_head(cx, "c3", "p1");
        view.read_with(cx, |view, _| {
            assert_eq!(view.snapshot.as_deref(), Some("c2"));
            assert!(view.newer, "its node changed");
        });
    }

    /// Refresh after another writer's commit reads again only the relationship groups it
    /// changed: the others are answered from the commit the page showed (spec 015).
    #[gpui::test]
    async fn refresh_reads_again_only_the_groups_a_commit_changed(cx: &mut TestAppContext) {
        const KNOWS_AND_LIKES: &str = "node Person {\n  slug: String @key\n  name: String\n}\nedge Knows: Person -> Person\nedge Likes: Person -> Person\n";
        let reads = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = reads.clone();
        let (session, writer) = session_followed(
            KNOWS_AND_LIKES,
            move |request| {
                let name = request["name"].as_str().unwrap_or_default();
                if name == "studio_neighbours" {
                    let query = request["query"].as_str().unwrap_or_default();
                    let edge = if query.contains(":likes") {
                        "likes"
                    } else {
                        "knows"
                    };
                    seen.lock().unwrap().push(edge.to_owned());
                }
                match name {
                    "studio_node" => json!([{ "n": { "@id": "p1", "slug": "p1", "name": "Ada" } }]),
                    "studio_neighbours" => json!([{ "id": "p010", "edge_id": "e1" }]),
                    _ => json!([]),
                }
            },
            cx,
        );
        let (view, cx) = page(session.clone(), cx);
        let read = |edge: &str| reads.lock().unwrap().iter().filter(|e| *e == edge).count();
        assert_eq!(
            (read("knows"), read("likes")),
            (2, 2),
            "each group out and in"
        );

        writer.commit(
            "c2",
            vec![changed("edge", "Likes", "e9", Some(("p1", "p030")))],
        );
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        assert!(
            view.read_with(cx, |view, _| view.newer),
            "an edge at the node"
        );

        view.update(cx, |view, cx| view.refresh(cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.snapshot.as_deref(), Some("c2"));
            assert!(!view.newer);
        });
        assert_eq!(
            (read("knows"), read("likes")),
            (2, 4),
            "only the groups of the edge type the commit wrote"
        );
    }

    /// A relationship is removed on the second press of its ×, as its own commit, which the
    /// view moves to (spec 014).
    #[gpui::test]
    async fn a_relationship_is_removed_on_the_second_press(cx: &mut TestAppContext) {
        let (view, cx) = page(session(KNOWS, cx), cx);
        view.update_in(cx, |view, window, cx| {
            assert!(!view.groups.is_empty(), "Person knows Person");
            view.press_remove(0, "e1".into(), window, cx);
            assert_eq!(view.removing.as_deref(), Some("e1"), "the first press asks");
            assert!(view.edge_writes.is_empty(), "nothing is written yet");
            view.press_remove(0, "e1".into(), window, cx);
            assert!(view.edge_writes.contains(&0), "the second press writes");
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.removing.is_none() && view.edge_writes.is_empty());
            assert_eq!(view.snapshot.as_deref(), Some("c2"), "its own commit");
            assert!(!view.newer && view.group_notices.is_empty());
        });
    }

    /// A node lists first the relationships it is known to have; the ones known to have none
    /// fold into one row, which lists them when opened. One the person is adding to stays
    /// listed while empty. A group laid out before it was counted keeps its place whatever its
    /// count says, so a count arriving moves no row.
    #[gpui::test]
    async fn relationships_with_none_fold_away_until_asked_for(cx: &mut TestAppContext) {
        let (view, cx) = page(session(KNOWS, cx), cx);
        view.update_in(cx, |view, window, cx| {
            assert!(view.groups.len() >= 2, "knows, out and in");
            assert_eq!(view.groups[1].count, Loadable::Loaded(0), "read as drawn");
            assert_eq!(view.place(1), Place::AsDrawn, "it has none, in its place");
            // As if the table had counted them before the page laid them out.
            for group in &mut view.groups {
                group.as_drawn = false;
            }
            view.groups[0].count = Loadable::Loaded(2);
            assert_eq!(view.place(0), Place::Listed, "it has neighbours");
            assert_eq!(view.place(1), Place::Folded, "it has none");
            assert!(!view.folded_shown(1), "folded under the empty row");
            assert_eq!(
                view.list_items()[1..],
                [Item::Group(0), Item::Empty],
                "listed, then the empty row"
            );
            view.groups[1].count = Loadable::Failed("unreachable".into());
            assert_eq!(view.place(1), Place::Listed, "a failed count says why");
            view.groups[1].count = Loadable::Loading;
            assert_eq!(view.place(1), Place::Listed, "read again after a write");
            view.groups[1].count = Loadable::Loaded(0);
            view.edge_writes.insert(1);
            assert_eq!(view.place(1), Place::Listed, "a write is out on it");
            view.edge_writes.clear();
            view.show_empty = true;
            assert_eq!(
                view.place(1),
                Place::Folded,
                "opening the row moves nothing"
            );
            assert!(view.folded_shown(1), "it shows under the open row");
            view.show_empty = false;
            view.start_add(1, window, cx);
            assert!(view.folded_shown(1), "being added to, with the row closed");
        });
    }

    /// A replace is two commits: the page follows both, and when the addition fails after the
    /// removal landed it says the old relationship is gone rather than that nothing was written.
    #[gpui::test]
    async fn a_replaced_relationship_follows_both_commits_and_reports_a_half(
        cx: &mut TestAppContext,
    ) {
        use crate::test_support::{committed, session_writing};
        let refused = json!({ "error": "__dst 'p9' not found in Person", "code": "bad_request" });
        for (second, both) in [
            ((200, committed("c3", "c2")), true),
            ((400, refused), false),
        ] {
            let session = session_writing(KNOWS, vec![(200, committed("c2", "c1")), second], cx);
            let (view, cx) = page(session, cx);
            view.update_in(cx, |view, window, cx| {
                view.snapshot = Some("c1".into());
                view.write_edge(
                    0,
                    EdgeChange::Add("p9".into()),
                    Some("e1".into()),
                    window,
                    cx,
                );
            });
            cx.run_until_parked();
            view.read_with(cx, |view, _| {
                if both {
                    assert_eq!(
                        view.snapshot.as_deref(),
                        Some("c3"),
                        "followed both commits"
                    );
                    assert!(!view.newer);
                    assert!(!view.group_notices.contains_key(&0));
                } else {
                    assert_eq!(view.snapshot.as_deref(), Some("c2"), "the removal landed");
                    let notice = view.group_notices.get(&0).expect("the half is reported");
                    assert!(
                        notice
                            .message
                            .starts_with("The relationship it replaces was removed"),
                        "{}",
                        notice.message
                    );
                }
            });
        }
    }

    /// ↵ picks from the results for what is typed now: results read for earlier text are not
    /// written to while the search for the new text is out.
    #[gpui::test]
    async fn enter_waits_for_the_results_of_what_is_typed(cx: &mut TestAppContext) {
        let refused = json!({ "error": "refused", "code": "bad_request" });
        let (view, cx) = page(session_answering(KNOWS, 400, refused, cx), cx);
        view.update_in(cx, |view, window, cx| {
            view.start_add(0, window, cx);
            let adding = view.adding.as_mut().unwrap();
            adding
                .editor
                .update(cx, |editor, cx| editor.set_text("grace", window, cx));
            adding.found = Loadable::Loaded(vec![Neighbour {
                id: "p1".into(),
                label: None,
                edge_id: None,
            }]);
            adding.answers = Some("gr".into());
            view.confirm_add(window, cx);
            assert!(
                view.adding.is_some(),
                "the results answer earlier text: nothing written"
            );
            view.adding.as_mut().unwrap().answers = Some("grace".into());
            view.confirm_add(window, cx);
            assert!(view.adding.is_none(), "the results answer what is typed");
        });
    }

    /// A relationship is added to the node chosen in the search, with this node at the end
    /// its group points from; a refusal stays on the group with its reason.
    #[gpui::test]
    async fn a_refused_relationship_says_why_on_its_group(cx: &mut TestAppContext) {
        let refused = json!({ "error": "__dst 'p9' not found in Person", "code": "bad_request" });
        let (view, cx) = page(session_answering(KNOWS, 400, refused, cx), cx);
        view.update_in(cx, |view, window, cx| {
            view.start_add(0, window, cx);
            let adding = view.adding.as_mut().unwrap();
            adding.found = Loadable::Loaded(vec![Neighbour {
                id: "p9".into(),
                label: None,
                edge_id: None,
            }]);
            view.add_highlighted(window, cx);
            assert!(view.adding.is_none(), "the search closes on a choice");
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let notice = view
                .group_notices
                .get(&0)
                .expect("the refusal stays on the group");
            assert_eq!(notice.tone, Tone::Refused);
            assert_eq!(view.snapshot.as_deref(), Some("c1"), "nothing was written");
        });
    }

    /// A refused write puts the old value back and opens the field again with what was typed
    /// and why (constitution III).
    #[gpui::test]
    async fn a_refused_field_opens_again_with_its_text_and_reason(cx: &mut TestAppContext) {
        let refused = json!({ "error": "policy denied", "code": "forbidden" });
        let session = session_answering(PEOPLE, 403, refused, cx);
        let (view, cx) = page(session.clone(), cx);
        edit_name(&view, "Ada Lovelace", cx);
        view.read_with(cx, |view, cx| {
            assert_eq!(view.value_of("name"), json!("Ada"), "nothing was written");
            let edit = &view.edits["name"];
            assert_eq!(edit.editor.read(cx).text(cx), "Ada Lovelace");
            assert_eq!(edit.notice.as_ref().map(|n| n.tone), Some(Tone::Refused));
        });

        // The peek goes (the table's cursor moved on): the text it held is kept as a draft.
        drop(view);
        cx.update(|window, _| window.remove_window());
        cx.run_until_parked();
        let key = DraftKey {
            type_name: "Person".into(),
            id: "p1".into(),
            property: "name".into(),
        };
        let draft = session.read_with(cx, |session, _| session.draft(&key).cloned());
        assert_eq!(
            draft.map(|draft| draft.text).as_deref(),
            Some("Ada Lovelace")
        );
    }
}

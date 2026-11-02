//! The node view (specs 004, 014): one row's properties, copyable, and for a node its
//! relationship groups with counts and neighbours. It shows as the peek in the inspector, and
//! at full size as a node's page in a tab of its own. Every read is pinned to the commit the row
//! was read at, or to the first read when the node was opened by id (constitution II).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use editor::{Editor, EditorEvent};
use gpui::{
    App, AsyncApp, ClipboardItem, Context, Entity, ExternalPaths, FocusHandle, Focusable,
    ListAlignment, ListState, Render, SharedString, Subscription, Task, WeakEntity, Window, list,
};
use gq_language::builder::Depends;
use gq_language::input::edit_text;
use gq_language::{BuiltQuery, Direction, RelationshipGroup, SchemaModel, builder};
use graph_store::scope::{ReadScope, ScopeSince};
use graph_store::{
    DraftEntry, DraftKey, GraphSession, HeldAs, Loadable, OutcomeKind, SessionEvent,
};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use serde_json::{Map, Value};
use studio_actions::graph::{OpenBlob, OpenNodePage, PeekNode};
use studio_actions::node::{CancelField, ExpandField, SaveField, SaveNodeDrafts};
use studio_actions::table::{SelectNextSuggestion, SelectPreviousSuggestion};
use studio_ui::ds::Tipped;
use studio_ui::ds::space;
use studio_ui::menu::ContextMenu;
use ui::prelude::*;
use ui::{PopoverMenu, ToggleState};

use crate::blob::{
    IMAGE_MAX, KIND_PREFIX, PREVIEW_PREFIX, Preview, bytes_wanted, preview, read_range,
    save_folder, save_value, suggested_file_name,
};
use crate::blob_write::{self, Binding, BlobChange, BlobEdit, BlobWritten};
use crate::model::{
    BlobsAt, FROM_COLUMN, Notice, SaveResult, TO_COLUMN, Tone, changed_elsewhere, display_value,
    edited, field_rows, node_label, node_rows, reads_as_markdown, save_result,
};
use crate::offers::Offers;
use crate::suggest;
use gq_language::input::same_value;
use graph_client::api::{ChangeOutput, ReadOutput};
use graph_client::{BlobCell, BlobMeta, HistoricalGap, PropertyThen, ReadError, WriteOutcome};

/// First neighbours shown under an expanded relationship group.
pub const NEIGHBOUR_LIMIT: u32 = 8;

/// Neighbours a page reads at a time under each relationship group, with "Show more" for the
/// next (constitution IX).
const PAGE_NEIGHBOURS: u32 = 50;

/// The widest a page's text runs: the design system's reading width, as settings' is.
const PAGE_WIDTH: f32 = studio_ui::ds::size::READING_WIDTH;

/// The longest a text with words runs in its property's box; a longer one is shown under its
/// name at the full width, where it wraps, rather than cut at the box's end.
const BOXED_TEXT: usize = 40;

/// Debounce before relationship counts are read, so moving the cursor quickly through rows
/// does not send a burst of reads.
const COUNT_DELAY: Duration = Duration::from_millis(120);
/// What a property the node holds no value for shows, borrowed like a value it holds.
static NULL: Value = Value::Null;

/// Why a field opens again when its write matched nothing and the node could not be read to
/// say what it holds now, or was read still holding the value the write was bound to.
const CHANGED_UNREAD: &str =
    "The value changed elsewhere while you were editing. Nothing was written.";

/// Why a field opens again when its write matched nothing because the node is gone.
const NODE_GONE: &str =
    "This node was deleted elsewhere while you were editing. Nothing was written.";

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
    /// The edge that relates it, which removing or replacing the relationship names; `None`
    /// when it was read without it (a table's chips).
    pub edge_id: Option<String>,
}

/// What the table already read about one of the row's relationship groups, at the peek's
/// commit: the count, and the first neighbours when it read the row's whole list. The peek
/// shows them rather than reading them again.
#[derive(Debug, Clone, PartialEq)]
pub struct KnownRelation {
    pub count: i64,
    /// The first `min(count, NEIGHBOUR_LIMIT)` neighbours by id, as the peek's own read
    /// orders them; `None` when the table did not read them all. A table reads them without
    /// their edges, so only a peek that writes no relationship is given them.
    pub first: Option<Vec<Neighbour>>,
}

/// The Blob values a table shows of the row a peek is opened from: the commit where they hold
/// ([`BlobsAt`], which the table's own Blob write may have moved past its pin), and each value
/// its cell shows, by property. The peek shows the same, and binds its Blob writes the same way.
#[derive(Debug, Clone, Default)]
pub struct TableBlobs {
    pub at: BlobsAt,
    pub shown: HashMap<String, BlobMeta>,
}

/// One of the table's own writes, as the peek of one of its rows hears it
/// ([`NodePeek::table_wrote`]).
#[derive(Debug, Clone)]
pub(crate) struct TableWrote {
    /// The commit it landed at, and the one it was made on.
    pub commit: String,
    pub parent: Option<String>,
    /// What it wrote to the peek's row: property values, and a Blob value with its metadata.
    pub values: Vec<(String, Value)>,
    pub blob: Option<(String, BlobMeta)>,
    /// The other rows of `type_name` whose values it changed, which the peek may show among a
    /// group's neighbours by their old name.
    pub type_name: String,
    pub touched: HashSet<String>,
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
    /// The commit its neighbours were read at, where their next page is read: the view's, or
    /// the one a write from a view behind reached.
    read_at: Option<String>,
    /// The next page of neighbours being read (a page's "Show more").
    more: Option<Task<()>>,
    _task: Option<Task<()>>,
}

/// How a read of a group's first edges lands ([`NodePeek::read_group`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupRead {
    /// The group read whole: its count says nothing until the read lands, when it opens if it
    /// has any after the person's own write to it (`open`), or else if it is small.
    Whole { open: bool },
    /// The neighbours of a group open with its count shown (a table's), which stays until the
    /// read gives its own.
    Neighbours,
}

/// The reads of a group's first edges: its first neighbours, one row an edge and one more than
/// the view shows (`limit`), and its edges counted, for when they do not fit.
struct EdgeReads {
    first: BuiltQuery,
    count: Result<BuiltQuery, String>,
    limit: u32,
}

/// A group's first edges as the view lists them, and how many it has.
struct FirstEdges {
    shown: Vec<Neighbour>,
    count: Loadable<i64>,
}

/// One Blob property of the peeked row: what it holds, and its preview.
struct BlobSlot {
    property: String,
    meta: Loadable<BlobMeta>,
    preview: Loadable<Preview>,
    /// The value's first bytes, as many as judge its kind ([`KIND_PREFIX`]), to name a saved
    /// file for it.
    head: Vec<u8>,
    /// Where the value it shows is held, which Save reads: the commit it was read at (`Some(None)`
    /// for the branch head), or the commit of the write that put it there; `None` when no commit
    /// the view knows holds it (a value its table shows where the table's own Blob write went
    /// past another writer's commit), which Save then says.
    bytes_at: Option<Option<String>>,
    save: Option<SaveState>,
    /// A file being stored in it or a clear, while out, or why it did not land.
    edit: Option<BlobEdit>,
    _task: Option<Task<()>>,
    _save: Option<Task<()>>,
}

impl BlobSlot {
    /// Whether a save of its value to a file is going: its file being chosen or written. One
    /// that ended is not: the value read again offers Open and Save… again.
    fn saving_to_file(&self) -> bool {
        self._save.is_some()
            && !matches!(self.save, Some(SaveState::Saved(_) | SaveState::Failed(_)))
    }
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

/// A property's write as sent: the value it was bound to, the one written, the text typed, what
/// the view showed before the new value was put in, and the property's draft as the session held
/// it then, which the write settles only while it still is.
struct FieldWrite {
    property: gq_language::Property,
    old: Value,
    new: Value,
    text: String,
    shown: Value,
    sent: Option<DraftEntry>,
}

/// A draft as ⌘S sent it: the value it was bound to, the one it asks for, and the text typed.
struct SentDraft {
    name: String,
    old: Value,
    new: Value,
    text: String,
}

impl SentDraft {
    /// The draft as ⌘S sent it, which was the session's then.
    fn entry(&self) -> DraftEntry {
        DraftEntry {
            old: self.old.clone(),
            text: self.text.clone(),
        }
    }
}

/// A read of the node sent ([`NodePeek::send_node_read`]): what its answer is landed with.
struct NodeRead {
    /// Which of the view's reads of the node it is ([`NodePeek::node_reads`]).
    sent: u64,
    /// Read as fields, without the properties the commit does not hold.
    fields: bool,
    /// The head the session held, when the read pins the view there.
    asked: Option<String>,
}

/// What a read of the node found: its values; `Some(None)` when it is gone, `None` when it
/// could not be read.
type Held = Option<Option<Map<String, Value>>>;

/// Adding a relationship from the node: the search over the other type and what it found.
struct AddEdge {
    /// The relationship group it adds to.
    index: usize,
    editor: Entity<Editor>,
    /// The nodes the search found, offered as a field that names a node offers them, the first
    /// highlighted.
    offers: Offers,
    /// How the search for `answers` went: out, read (its nodes in `offers`) or failed.
    found: Loadable<()>,
    /// The text `offers` answers: what was typed when they were read.
    answers: Option<String>,
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

/// One write of a relationship change ([`NodePeek::write_edge`]): a replace is the removal of
/// the one there, then the addition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EdgeStep {
    Add,
    Remove,
    ReplaceRemove,
    ReplaceAdd,
}

impl EdgeStep {
    /// How the write's outcome names it.
    fn verb(self) -> &'static str {
        match self {
            EdgeStep::Add | EdgeStep::ReplaceAdd => "Add",
            EdgeStep::Remove => "Remove",
            EdgeStep::ReplaceRemove => "Replace",
        }
    }

    /// The write, as the subject of a sentence.
    fn doing(self) -> &'static str {
        match self {
            EdgeStep::Add => "Adding it",
            EdgeStep::Remove => "Removing it",
            EdgeStep::ReplaceRemove => "Removing the relationship it replaces",
            EdgeStep::ReplaceAdd => "Adding the new one",
        }
    }

    /// What landing would have done, as a clause.
    fn done(self) -> &'static str {
        match self {
            EdgeStep::Add => "it was added",
            EdgeStep::Remove => "it was removed",
            EdgeStep::ReplaceRemove => "the relationship it replaces was removed",
            EdgeStep::ReplaceAdd => "the new one was added",
        }
    }

    /// What it leaves unsent when it does not land: a replace's addition waits for its removal.
    fn leaves(self) -> &'static str {
        match self {
            EdgeStep::ReplaceRemove => ", and the new one was not sent",
            _ => "",
        }
    }
}

/// How one relationship write ended ([`edge_written`]).
#[derive(Debug, Clone, PartialEq)]
enum EdgeWritten {
    /// Published on the branch as `commit`, on top of `parent`.
    Saved {
        commit: String,
        parent: Option<String>,
    },
    /// Nothing was written: refused, or it matched nothing.
    NotWritten(Notice),
    /// It is not on the branch as far as Studio can tell, though it may have been written:
    /// held as a proposal or a snapshot, of a commit kind this build does not know, or its
    /// answer lost.
    Unsettled(Notice),
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
    saving: Writing,
    /// Properties the table this peek was opened from has a save of out: shown with a dot over
    /// the value the node holds at the view's commit, and not edited here until it lands.
    saving_elsewhere: HashSet<String>,
    /// Where the Blob values the view shows hold, which a file stored in a slot is bound at.
    blobs_at: BlobsAt,
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
    /// The long texts folded under their names, by property.
    folded: HashSet<String>,
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
    /// The list's items the last frame laid out, first to last: those in view and just past it.
    items_drawn: Option<Range<usize>>,
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
    /// The reads of the node sent ([`Self::send_node_read`]). An answer lands only while its
    /// read is the newest, so a write's read of the node that comes back after the view was
    /// read again is dropped.
    node_reads: u64,
    /// Conflicts whose read of the node gave way to a newer one, waiting for what that one
    /// finds ([`Self::landed_or_newer`]).
    node_waiters: Vec<futures::channel::oneshot::Sender<Held>>,
    /// The node is read when the view is next drawn ([`Self::owe_node`]): a page restored with
    /// the workspace, or one behind another tab when its branch moves, reads nothing until it
    /// is shown.
    node_owed: bool,
    /// Refresh asking after the branch's head before it reads the node again.
    _refresh: Option<Task<()>>,
    /// Takes the copied mark down; a refresh keeps it, and a second copy starts it again.
    _copied: Option<Task<()>>,
    _subscription: Subscription,
    /// The view's keyboard: ⌘S saves its drafts while it, or a field in it, has it. A page hands
    /// its focus here ([`crate::node_page::NodePage`]), so the keys reach the view with no field
    /// open.
    focus_handle: FocusHandle,
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
        let blobs = TableBlobs::default();
        Self::build(
            session, subject, values, snapshot, known, blobs, debounce, false, cx,
        )
    }

    /// [`Self::new`] for a row a table shows: its Blob values are where the table's hold
    /// (`blobs`), which may be past `snapshot` after the table's own Blob write, and each one
    /// the table read is shown as its cell shows it.
    #[expect(clippy::too_many_arguments)]
    pub fn from_table(
        session: Entity<GraphSession>,
        subject: PeekSubject,
        values: Map<String, Value>,
        snapshot: Option<String>,
        known: HashMap<String, KnownRelation>,
        blobs: TableBlobs,
        debounce: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(
            session,
            subject,
            Some(values),
            snapshot,
            known,
            blobs,
            debounce,
            false,
            cx,
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
            TableBlobs::default(),
            false,
            true,
            cx,
        );
        if holds {
            page.hold_commit();
        }
        page
    }

    #[expect(clippy::too_many_arguments)]
    fn build(
        session: Entity<GraphSession>,
        subject: PeekSubject,
        values: Option<Map<String, Value>>,
        snapshot: Option<String>,
        known: HashMap<String, KnownRelation>,
        blobs: TableBlobs,
        debounce: bool,
        page: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let schema = session.read(cx).schema_model();
        let subscription = cx.subscribe(&session, |this: &mut Self, session, event, cx| {
            // Its node's drafts are the session's, kept from a table or another view too; what
            // the credential may do decides which of its controls write.
            if let SessionEvent::DraftsChanged | SessionEvent::AccessChanged = event {
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
                    this.owe_node(cx);
                }
                cx.notify();
                return;
            }
            // The schema names some type's nodes otherwise (spec 002, "Node names"): the title
            // takes its new name from the values held, and the neighbours shown are read again
            // under theirs; a group not read yet waits to be drawn, as before.
            if let SessionEvent::SchemaChanged = event
                && let Some(now) = session.read(cx).schema_model()
                && this
                    .schema
                    .as_ref()
                    .is_some_and(|was| was.source == now.source && !was.same_as(&now))
            {
                this.schema = Some(now);
                for index in 0..this.groups.len() {
                    if matches!(this.groups[index].neighbours, Loadable::Loaded(_)) {
                        this.load_group(index, false, cx);
                    }
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
                let left = std::mem::replace(&mut this.branch, session.read(cx).branch.clone());
                // A write still out goes on to the branch left, and is shown there again if the
                // view comes back before it lands; a file being stored tells how it ended in its
                // toast. The new branch's values show none of it, and its head moves are
                // compared again.
                this.saving_elsewhere.clear();
                for slot in &mut this.blobs {
                    slot.edit = None;
                }
                // A field open on the branch left, one a refused save opened again included, is
                // kept as a draft there, as leaving it keeps it.
                this.keep_open_fields(&left, cx);
                this.adding = None;
                this.removing = None;
                this.group_notices.clear();
                this.drafts_notice = None;
                this.confirm_discard = false;
                this._refresh = None;
                this.reload(cx);
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
            if moved.is_some_and(|branch| *branch == session.read(cx).branch) && !this.writes_out()
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
            saving: Writing::default(),
            saving_elsewhere: HashSet::new(),
            blobs_at: blobs.at,
            adding: None,
            removing: None,
            edge_writes: HashSet::new(),
            group_notices: HashMap::new(),
            saving_drafts: false,
            drafts_notice: None,
            confirm_discard: false,
            show_empty: false,
            folded: HashSet::new(),
            scope: Some(scope),
            list: ListState::new(0, ListAlignment::Top, px(DRAW_AHEAD)),
            list_items: Vec::new(),
            items_drawn: None,
            absent: HashMap::new(),
            groups_absent: Vec::new(),
            awaiting_changes: false,
            _node_read: None,
            node_reads: 0,
            node_waiters: Vec::new(),
            node_owed: false,
            focus_handle: cx.focus_handle(),
            _refresh: None,
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
                this.load_blobs(debounce, blobs.shown, cx);
                // The caller's commit may be behind the head the session has seen (a table
                // behind its branch): it is compared as a head move is, once the caller has set
                // the view up, holding its commit or telling it of its saves out.
                let view = cx.weak_entity();
                cx.defer(move |cx| {
                    view.update(cx, |this, cx| {
                        if !this.writes_out() {
                            this.compare_head(cx);
                        }
                    })
                    .ok();
                });
            }
            None => this.owe_node(cx),
        }
        this
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

    pub(crate) fn label(&self) -> Option<String> {
        let values = self.values.loaded()?;
        if let PeekSubject::Edge { .. } = &self.subject {
            let from = values.get(FROM_COLUMN)?.as_str()?;
            let to = values.get(TO_COLUMN)?.as_str()?;
            return Some(format!("{from} → {to}"));
        }
        node_label(
            self.schema.as_ref()?.node(self.subject.type_name())?,
            values,
        )
    }

    /// The peek shows the commit it was given whatever the branch does: no Refresh is offered.
    pub fn hold_commit(&mut self) {
        self.holds_commit = true;
        self.newer = false;
        // A held commit never moves, so nothing the branch does concerns it.
        self.scope = None;
        self.awaiting_changes = false;
    }

    /// Refresh: the branch's head is asked after first, and the node read again at the head the
    /// session then holds ([`Self::reload`]). The session may not hold the newest head yet, and
    /// a pin at the head it holds is one it compares the next heads with.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let branch = self.branch.clone();
        let head = self
            .session
            .update(cx, |session, cx| session.head_now(&branch, cx));
        self._refresh = Some(cx.spawn(async move |this, cx| {
            head.await;
            this.update(cx, |this, cx| this.reload(cx)).ok();
        }));
    }

    /// Reads a node again at the head the session holds, with its relationships and Blobs,
    /// under the session's schema.
    fn reload(&mut self, cx: &mut Context<Self>) {
        self.forget_shown(cx);
        self.owe_node(cx);
    }

    /// Reads the node when the view is next drawn ([`Self::node_owed`]). A read still out is
    /// let go, and what waited for it hears it found nothing: what it would show belongs to
    /// what the view let go. Until the draw the node is not being read, so a conflict's read
    /// waits for none ([`Self::landed_or_newer`]).
    fn owe_node(&mut self, cx: &mut Context<Self>) {
        self._node_read = None;
        self.wake_node_waiters(None);
        self.values = Loadable::NotLoaded;
        self.node_owed = true;
        cx.notify();
    }

    /// Lets go of what the view shows, before the node is read again at the head the session
    /// holds, under the session's schema.
    fn forget_shown(&mut self, cx: &mut Context<Self>) {
        let schema = self
            .session
            .read(cx)
            .schema_model()
            .or_else(|| self.schema.clone());
        self.schema = schema;
        self.newer = false;
        self.snapshot = None;
        self.blobs_at = BlobsAt::Pin;
        // What the table read belongs to the old commit.
        self.known.clear();
        self.groups.clear();
        self.absent.clear();
        self.groups_absent.clear();
        // So do the Blob values shown, their previews and the reads still out: nothing of them
        // shows, or is saved to a file, under the new read, which reads them again if the node
        // is there. A value being saved to a file, or a file being stored, goes on.
        self.release_images(cx);
        self.blobs.retain(|slot| {
            slot.saving_to_file() || slot.edit.as_ref().is_some_and(BlobEdit::is_saving)
        });
        for slot in &mut self.blobs {
            slot.meta = Loadable::NotLoaded;
            slot.preview = Loadable::NotLoaded;
            slot.head.clear();
            slot.bytes_at = None;
            slot._task = None;
        }
        // What it showed is let go; what it reads now is told again, under the new schema too.
        if let Some(scope) = &self.scope {
            self.session.update(cx, |session, _| {
                let branch = session.branch.clone();
                session.reset_scope(scope, &branch);
            });
        }
    }

    /// For a write on `branch` that matched nothing: the view moves to the head the session
    /// holds, as [`Self::reload`] does, and the read of the node is handed back for the write to
    /// land ([`Self::node_read_landed`]) and settle against. A Refresh waiting for the head, or
    /// a read of the node out, gives way to it. `None` when the view reads another branch now,
    /// or no read can be built.
    fn reread_at_head(
        &mut self,
        branch: &str,
        cx: &mut Context<Self>,
    ) -> Option<(Task<Result<ReadOutput, ReadError>>, NodeRead)> {
        if self.branch != branch {
            return None;
        }
        self._refresh = None;
        self._node_read = None;
        self.forget_shown(cx);
        let read = self.send_node_read(cx);
        cx.notify();
        read
    }

    fn load_node(&mut self, cx: &mut Context<Self>) {
        let Some((read, sent)) = self.send_node_read(cx) else {
            return;
        };
        self._node_read = Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, cx| {
                this.node_read_landed(result, sent, cx);
            })
            .ok();
        }));
    }

    /// Sends a read of the node at the commit the view shows, else at the head the session
    /// holds, with what its answer is landed with ([`Self::node_read_landed`]). `None` when no
    /// read can be built: the view says why.
    fn send_node_read(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<(Task<Result<ReadOutput, ReadError>>, NodeRead)> {
        let PeekSubject::Node { type_name, id } = &self.subject else {
            return None;
        };
        let Some(schema) = self.schema.clone() else {
            self.values = Loadable::Failed("The schema is still loading.".into());
            self.wake_node_waiters(None);
            return None;
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
                self.wake_node_waiters(None);
                return None;
            }
        };
        self.values = Loadable::Loading;
        // At the head the session knows, a node kept there answers at once: a table's page
        // read it already.
        let snapshot = self.snapshot.clone().or_else(|| {
            let session = self.session.read(cx);
            session.head(&session.branch).map(str::to_owned)
        });
        // Read at the head the session holds, the read pins the view there: a head the session
        // sees move while it is out finds no pin to compare, and is compared once it lands.
        let asked = self.snapshot.is_none().then(|| snapshot.clone()).flatten();
        let read = self.read_shown(&query, snapshot, cx);
        self.node_reads += 1;
        let sent = NodeRead {
            sent: self.node_reads,
            fields,
            asked,
        };
        Some((read, sent))
    }

    /// Lands the answer to a read of the node ([`Self::send_node_read`]): the view shows the
    /// node as read, pinned to the commit it was read at. Hands back what it found; `None` too
    /// when it is not the view's newest read, whose answer is dropped: the view was read again
    /// since it was sent.
    fn node_read_landed(
        &mut self,
        result: Result<ReadOutput, ReadError>,
        read: NodeRead,
        cx: &mut Context<Self>,
    ) -> Held {
        if read.sent != self.node_reads {
            return None;
        }
        let NodeRead { fields, asked, .. } = read;
        let type_name = self.subject.type_name().to_owned();
        let mut found = None;
        match result {
            Ok(output) => {
                if self.snapshot.is_none() {
                    self.snapshot = output.graph_commit_id.clone();
                    if asked.is_some() && self.snapshot == asked && !self.writes_out() {
                        self.compare_head(cx);
                    }
                }
                let rows = if fields {
                    field_rows(&output.rows)
                } else {
                    node_rows(&output.rows)
                };
                let values = rows.into_iter().next().map(|row| row.values);
                found = Some(values.clone());
                match values {
                    Some(values) => {
                        self.values = Loadable::Loaded(values);
                        self.load_groups(cx);
                        self.load_blobs(false, HashMap::new(), cx);
                    }
                    None => {
                        self.values =
                            Loadable::Failed("This node does not exist at this commit.".into());
                    }
                }
            }
            Err(ReadError::Refused(refusal)) => match refusal.historical_gap() {
                Some(HistoricalGap::Property {
                    owner,
                    property,
                    then,
                }) if owner == type_name
                    && !self.absent.contains_key(&property)
                    && self
                        .schema
                        .as_ref()
                        .and_then(|schema| schema.node(&type_name))
                        .is_some_and(|node| node.property(&property).is_some()) =>
                {
                    // Once per property, so this ends.
                    self.absent.insert(property, then);
                    self.load_node(cx);
                }
                Some(HistoricalGap::Type { name }) if name == type_name => {
                    self.values =
                        Loadable::Failed(format!("{type_name} did not exist at this commit."));
                }
                _ => self.values = Loadable::Failed(refusal.message),
            },
            Err(error) => self.values = Loadable::Failed(error.to_string()),
        }
        // Read again for a property the commit did not have: what waits hears that read.
        if !matches!(self.values, Loadable::Loading) {
            self.wake_node_waiters(found.clone());
        }
        cx.notify();
        found
    }

    /// Hands what the view's newest read of the node found to the conflicts waiting for it.
    fn wake_node_waiters(&mut self, found: Held) {
        for waiter in self.node_waiters.drain(..) {
            waiter.send(found.clone()).ok();
        }
    }

    /// Lands a conflict's read of the node at the head ([`Self::reread_at_head`]): what it
    /// found, or, when a newer read of the view's took its place and is still out (another
    /// conflict's, a Refresh), what that one finds once it lands.
    fn landed_or_newer(
        &mut self,
        landed: Option<(Result<ReadOutput, ReadError>, NodeRead)>,
        cx: &mut Context<Self>,
    ) -> Result<Held, futures::channel::oneshot::Receiver<Held>> {
        let found = landed.and_then(|(result, read)| self.node_read_landed(result, read, cx));
        if found.is_none() && matches!(self.values, Loadable::Loading) {
            let (waiter, newer) = futures::channel::oneshot::channel();
            self.node_waiters.push(waiter);
            return Err(newer);
        }
        Ok(found)
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
        let branch = self.branch.clone();
        self.keep_open_fields(&branch, cx);
    }

    /// Closes every open field, keeping what a changed one holds as its property's draft on
    /// `branch`, the branch it was typed on: a field a refused or conflicting save opened again
    /// holds text nothing else keeps. One typed back to what the node holds keeps nothing. A
    /// branch deleted meanwhile keeps none ([`GraphSession::settle_draft_on`]).
    fn keep_open_fields(&mut self, branch: &str, cx: &mut App) {
        for (name, edit) in std::mem::take(&mut self.edits) {
            let text = edit.editor.read(cx).text(cx);
            let unchanged = self.asks_nothing(&name, &edit, &text);
            if let Some(key) = self.draft_key(&name).filter(|_| !unchanged) {
                let entry = DraftEntry {
                    old: edit.old,
                    text,
                };
                self.session.update(cx, |session, cx| {
                    let held = session.draft_on(branch, &key).cloned();
                    session.settle_draft_on(branch, key, held.as_ref(), Some(entry), cx);
                });
            }
        }
    }

    /// Whether a write of the view's, or of the table it was opened from, is out: a head seen
    /// meanwhile may be its commit, and is compared once they are in.
    fn writes_out(&self) -> bool {
        self.saving.any_on(&self.branch)
            || !self.edge_writes.is_empty()
            || !self.saving_elsewhere.is_empty()
    }

    /// One of the view's own writes, or its table's, landed at `commit` on `parent`: made on the
    /// commit the view shows, the view moves there; otherwise someone else committed in between,
    /// and the view is behind. Whether it moved.
    fn follow_own_commit(&mut self, parent: Option<&str>, commit: &str) -> bool {
        let followed = parent.is_some() && parent == self.snapshot.as_deref();
        let pin = self.snapshot.clone();
        self.blobs_at
            .after_own_write(pin.as_deref(), parent, commit, followed);
        if followed {
            self.snapshot = Some(commit.to_owned());
        } else {
            self.newer = true;
        }
        followed
    }

    /// The saves of the table this peek was opened from that are out for its row (`names`):
    /// their properties show with a dot over the value the node holds, and do not open here,
    /// until they land. A head seen while they are out is compared once they are all in.
    pub(crate) fn table_saves(&mut self, names: HashSet<String>, cx: &mut Context<Self>) {
        if self.holds_commit || names == self.saving_elsewhere {
            return;
        }
        let ended = names.is_empty();
        self.saving_elsewhere = names;
        if ended && !self.writes_out() {
            self.compare_head(cx);
        }
        cx.notify();
    }

    /// A write of the table this peek was opened from landed ([`TableWrote`]): the view follows
    /// it as it follows its own writes, showing what it wrote to the node at that commit, or is
    /// behind when another writer's commit came between. It is behind too when it shows one of
    /// the other rows the write changed among a group's neighbours, whose name it would show as
    /// it was.
    pub(crate) fn table_wrote(&mut self, wrote: TableWrote, cx: &mut Context<Self>) {
        if self.holds_commit || self.values.loaded().is_none() {
            return;
        }
        if self.shows_neighbour(&wrote.type_name, &wrote.touched) {
            self.newer = true;
            cx.notify();
            return;
        }
        // A head compared as it landed may have moved the view there already, nothing it reads
        // having changed.
        let followed = self.snapshot.as_deref() == Some(wrote.commit.as_str())
            || self.follow_own_commit(wrote.parent.as_deref(), &wrote.commit);
        if followed {
            for (name, value) in wrote.values {
                self.restore(&name, value);
            }
            if let Some((property, meta)) = wrote.blob
                && let Some(index) = self.blobs.iter().position(|slot| slot.property == property)
            {
                if let Loadable::Loaded(Preview::Image(image)) = &self.blobs[index].preview {
                    image.release(cx);
                }
                self.blobs[index].preview = Loadable::NotLoaded;
                self.blobs[index].head.clear();
                self.read_blob(index, false, Some(meta), cx);
                // The table's commit holds the value it wrote, which Save reads there.
                self.blobs[index].bytes_at = Some(Some(wrote.commit.clone()));
            }
            // The head the session saw land may be this commit, compared before it was told
            // of; with nothing else out, it is compared again now.
            if !self.writes_out() {
                self.compare_head(cx);
            }
        }
        cx.notify();
    }

    /// Whether a relationship group shows one of `ids`, nodes of `type_name`, among the
    /// neighbours it read.
    fn shows_neighbour(&self, type_name: &str, ids: &HashSet<String>) -> bool {
        !ids.is_empty()
            && self.groups.iter().any(|group| {
                group.group.other_type == type_name
                    && group
                        .neighbours
                        .loaded()
                        .is_some_and(|shown| shown.iter().any(|n| ids.contains(&n.id)))
            })
    }

    /// Frees the images this peek decoded; GPUI would keep them otherwise.
    fn release_images(&self, cx: &mut App) {
        for slot in &self.blobs {
            if let Loadable::Loaded(Preview::Image(image)) = &slot.preview {
                image.release(cx);
            }
        }
    }

    /// Reads each Blob property's metadata, then enough of a managed value to preview it: its
    /// first bytes, or the whole of an image that fits. Pinned to the peek's commit, or where
    /// its values hold ([`Self::read_blob`]); a property's value in `shown` (what the table it
    /// was opened from shows) is taken as it is.
    fn load_blobs(
        &mut self,
        debounce: bool,
        mut shown: HashMap<String, BlobMeta>,
        cx: &mut Context<Self>,
    ) {
        self.release_images(cx);
        // A value being saved to a file goes on, shown on its property's new slot; one whose
        // property the view no longer lists still lands in its file. A file being stored in it
        // stays shown as going while it is out.
        type Going = (Option<SaveState>, Option<Task<()>>, Option<BlobEdit>);
        let mut going: HashMap<String, Going> = std::mem::take(&mut self.blobs)
            .into_iter()
            .filter_map(|slot| {
                let (save, task) = match slot.saving_to_file() {
                    true => (slot.save, slot._save),
                    false => (None, None),
                };
                let edit = slot.edit.filter(BlobEdit::is_saving);
                (task.is_some() || edit.is_some()).then_some((slot.property, (save, task, edit)))
            })
            .collect();
        self.blobs = self
            .blob_properties()
            .into_iter()
            .map(|property| {
                let (save, task, edit) = going.remove(&property).unwrap_or_default();
                BlobSlot {
                    property,
                    meta: Loadable::Loading,
                    preview: Loadable::NotLoaded,
                    head: Vec::new(),
                    bytes_at: None,
                    save,
                    edit,
                    _task: None,
                    _save: task,
                }
            })
            .collect();
        for (_, (_, task, _)) in going {
            if let Some(task) = task {
                task.detach();
            }
        }
        for index in 0..self.blobs.len() {
            let known = shown.remove(&self.blobs[index].property);
            self.read_blob(index, debounce, known, cx);
        }
    }

    /// The cell of the subject's Blob property `property`.
    fn blob_cell(&self, property: &str) -> BlobCell {
        BlobCell {
            edge: matches!(self.subject, PeekSubject::Edge { .. }),
            type_name: self.subject.type_name().to_owned(),
            id: self.subject.id().to_owned(),
            property: property.to_owned(),
        }
    }

    /// Reads what slot `index` holds where the values the view shows hold ([`BlobsAt`]: its
    /// commit, or its own past it), then enough of a managed value to preview it; `known` is
    /// what it holds there already (a write's receipt, or what the table it was opened from
    /// shows), which is not read again. Where they hold at no commit the view knows, a value is
    /// read at the view's commit, as a table reads a cell, and a known one is shown without a
    /// preview, since no commit is known to hold its bytes.
    fn read_blob(
        &mut self,
        index: usize,
        debounce: bool,
        known: Option<BlobMeta>,
        cx: &mut Context<Self>,
    ) {
        let Some(slot) = self.blobs.get(index) else {
            return;
        };
        let cell = self.blob_cell(&slot.property);
        let session = self.session.downgrade();
        let held_at = self
            .blobs_at
            .commit(self.snapshot.as_deref())
            .map(str::to_owned);
        let unknown = self.blobs_at == BlobsAt::Unknown;
        let snapshot = if unknown {
            self.snapshot.clone()
        } else {
            held_at.clone()
        };
        let preview_at = match (&known, unknown) {
            (Some(_), true) => None,
            (Some(_), false) => Some(held_at),
            (None, _) => Some(snapshot.clone()),
        };
        self.blobs[index].bytes_at = preview_at.clone();
        let scope = self.scope.clone();
        self.blobs[index]._task = Some(cx.spawn(async move |this, cx| {
            if debounce {
                cx.background_executor().timer(COUNT_DELAY).await;
            }
            let meta = match known {
                Some(known) => Ok(known),
                None => {
                    let Ok(read) = session.update(cx, |session, cx| match &scope {
                        Some(scope) => {
                            session.blob_meta_in(scope, cell.clone(), snapshot.clone(), cx)
                        }
                        None => session.blob_meta(cell.clone(), snapshot.clone(), cx),
                    }) else {
                        return;
                    };
                    read.await
                }
            };
            let size = match (&meta, &preview_at) {
                (Ok(BlobMeta::Managed { size, .. }), Some(_)) => Some(*size),
                _ => None,
            };
            let snapshot = preview_at.flatten();
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
                match read_range(&session, &cell, &snapshot, from, wanted, cx).await {
                    Ok(Some(chunk)) => bytes.extend(chunk),
                    Ok(None) => break,
                    Err(error) => {
                        this.update(cx, |this, cx| {
                            this.blobs[index].preview = Loadable::Failed(error);
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
            // As many bytes as judge a value's kind, so a saved file is named as the preview
            // shows it: an SVG's root can come after a declaration and comments.
            let head = bytes.iter().take(KIND_PREFIX).copied().collect();
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

    /// Why slot `property` cannot take a file here, or `None` when it can: a peek held at a
    /// commit, a branch the credential may not change, an edge's, a property that is not a
    /// Blob.
    fn blob_write_refusal(&self, property: &str, cx: &App) -> Option<String> {
        let pinned = self.held_commit();
        let denied = self.change_refusal(cx);
        blob_write::write_refusal(
            self.schema.as_deref(),
            &self.blob_cell(property),
            pinned.as_deref(),
            denied.as_deref(),
        )
    }

    /// Choose File…: the system's picker, then the file chosen stored in slot `index`.
    fn choose_blob_file(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(property) = self.blobs.get(index).map(|slot| slot.property.clone()) else {
            return;
        };
        let chosen = blob_write::choose_file(cx);
        cx.spawn(async move |this, cx| {
            if let Some(path) = chosen.await {
                this.update(cx, |this, cx| this.upload_blob(&property, path, cx))
                    .ok();
            }
        })
        .detach();
    }

    /// A drop from the file manager on slot `index`: its one file stored there.
    fn drop_blob_files(
        &mut self,
        index: usize,
        paths: &[std::path::PathBuf],
        cx: &mut Context<Self>,
    ) {
        let Some(property) = self.blobs.get(index).map(|slot| slot.property.clone()) else {
            return;
        };
        match blob_write::dropped_file(paths) {
            Ok(path) => self.upload_blob(&property, path, cx),
            Err(notice) => {
                self.blobs[index].edit = Some(BlobEdit::Failed {
                    notice,
                    again: None,
                });
                cx.notify();
            }
        }
    }

    /// Stores the file at `path` in the Blob property `property` on the branch the view reads,
    /// bound to the value the slot shows at the view's commit.
    pub(crate) fn upload_blob(
        &mut self,
        property: &str,
        path: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) {
        self.send_blob_write(property, BlobChange::Upload(path), None, cx);
    }

    /// Clear: the Blob property `property` set to null, bound to the value the slot shows.
    pub(crate) fn clear_blob(&mut self, property: &str, cx: &mut Context<Self>) {
        self.send_blob_write(property, BlobChange::Clear, None, cx);
    }

    /// Sends `change` to slot `property`, bound to `over` (what a conflict found the cell to
    /// hold at the head) or else to the value the slot shows at the view's commit. Its dot shows
    /// while it is out; detached, as every write is, so the view going never cancels it.
    fn send_blob_write(
        &mut self,
        property: &str,
        change: BlobChange,
        over: Option<BlobMeta>,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.blobs.iter().position(|slot| slot.property == property) else {
            return;
        };
        let refusal = match &change {
            BlobChange::Upload(_) => self.blob_write_refusal(property, cx),
            BlobChange::Clear if over.is_some() => self.blob_write_refusal(property, cx),
            BlobChange::Clear => blob_write::clear_refusal(
                self.schema.as_deref(),
                &self.blob_cell(property),
                self.held_commit().as_deref(),
                self.change_refusal(cx).as_deref(),
                self.blobs[index].meta.loaded(),
            ),
        };
        // One write of a slot at a time on a branch, one sent before the view left the branch
        // and came back included, which the slot no longer marks.
        if self.saving.on(&self.branch, property) {
            return;
        }
        let slot = &mut self.blobs[index];
        let shown = match (&over, slot.meta.loaded()) {
            (Some(over), _) => Some(over.clone()),
            (None, Some(shown)) => Some(shown.clone()),
            (None, None) => None,
        };
        let (Some(shown), None) = (shown, refusal.clone()) else {
            let reason = refusal.unwrap_or_else(|| {
                format!("Nothing was written: Studio has not read what {property} holds yet.")
            });
            slot.edit = Some(BlobEdit::Failed {
                notice: Notice::new(Tone::Invalid, reason),
                again: None,
            });
            cx.notify();
            return;
        };
        // Bound where the values the slots show hold: the view's commit, or past it the view's
        // own commit that holds them; where none is known (its own write made over another
        // writer's), to the validator the slot shows. A change elsewhere since is found either
        // way. A change offered again after a conflict is bound to what the head held then.
        let read_at = match &over {
            Some(_) => None,
            None => self
                .blobs_at
                .commit(self.snapshot.as_deref())
                .map(str::to_owned),
        };
        // Where the write was bound, once it is: an upload's binding is worked out as it is
        // sent, after the file is read.
        let bound_at = Rc::new(RefCell::new(read_at.clone()));
        slot.edit = Some(BlobEdit::Saving(change.clone()));
        self.saving.insert(&self.branch, property);
        let (cell, branch) = (self.blob_cell(property), self.branch.clone());
        let write = match &change {
            BlobChange::Upload(path) => {
                // One of the view's own writes, or its table's, landing while the file is read
                // moves where the values it shows hold, and every validator of the type with
                // them: the write is bound to what the slot shows as it goes. After a conflict
                // it stays bound to what the head held then; with the view gone or on another
                // branch, to what the slot showed when the file was chosen.
                let view = cx.entity().downgrade();
                let (property, chosen) = (property.to_owned(), bound_at.clone());
                let (sent_on, again) = (branch.clone(), over.is_some());
                let chose = Binding { shown, read_at };
                let binding = move |cx: &mut App| {
                    let now = view
                        .upgrade()
                        .filter(|_| !again)
                        .and_then(|view| view.read(cx).blob_binding(&property, &sent_on));
                    let binding = now.unwrap_or(chose);
                    *chosen.borrow_mut() = binding.read_at.clone();
                    binding
                };
                blob_write::upload_bound(&self.session, &branch, cell, binding, path.clone(), cx)
            }
            BlobChange::Clear => {
                blob_write::clear(&self.session, &branch, cell, shown, read_at, cx)
            }
        };
        let property = property.to_owned();
        cx.spawn(async move |this, cx| {
            let written = write.await;
            this.update(cx, |this, cx| {
                let bound_at = bound_at.borrow().clone();
                this.blob_written(&property, &branch, change, (written, bound_at), cx)
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// What a Blob write to slot `property`, sent on `branch`, is bound to now: the value the
    /// slot shows, read where the values the view shows hold ([`BlobsAt`]). `None` once the
    /// view reads another branch, or its slot holds no value it has read.
    fn blob_binding(&self, property: &str, branch: &str) -> Option<Binding> {
        if self.branch != branch {
            return None;
        }
        let slot = self.blobs.iter().find(|slot| slot.property == property)?;
        Some(Binding {
            shown: slot.meta.loaded()?.clone(),
            read_at: self
                .blobs_at
                .commit(self.snapshot.as_deref())
                .map(str::to_owned),
        })
    }

    /// Lands a Blob write of slot `property` sent on `branch`, with the commit its validator was
    /// read at (`landed`: how it ended, and that commit). A commit made on the one the view
    /// shows moves the view to it, and the slot shows the new value; over another writer's, the
    /// view is behind and says so, and a write bound to a validator read where the values shown
    /// hold leaves them holding at its commit, which the next write is bound at. One that did
    /// not land says why on its slot.
    fn blob_written(
        &mut self,
        property: &str,
        branch: &str,
        change: BlobChange,
        landed: (BlobWritten, Option<String>),
        cx: &mut Context<Self>,
    ) {
        let (written, bound_at) = landed;
        self.saving.remove(branch, property);
        let index = self.blobs.iter().position(|slot| slot.property == property);
        // The view reads another branch now, whose slots the change of branch cleared: what
        // they mark is that branch's own. The write's outcome is told by its toast.
        if self.branch != branch {
            cx.notify();
            return;
        }
        let saved = match &written {
            BlobWritten::Saved {
                commit,
                parent,
                meta,
                bound,
            } => Some((commit.clone(), parent.clone(), meta.clone(), *bound)),
            _ => None,
        };
        if let Some(index) = index {
            self.blobs[index].edit = BlobEdit::ended(written, change, property);
        }
        if let Some((commit, parent, meta, bound)) = saved {
            if let Some(commit) = &commit {
                let before = self
                    .blobs_at
                    .commit(self.snapshot.as_deref())
                    .map(str::to_owned);
                if !self.follow_own_commit(parent.as_deref(), commit)
                    && let Some(at) = bound_at.as_deref().filter(|_| bound)
                {
                    self.blobs_at
                        .after_bound_write(at, before.as_deref(), commit);
                }
            }
            if let Some(index) = index {
                if let Loadable::Loaded(Preview::Image(image)) = &self.blobs[index].preview {
                    image.release(cx);
                }
                self.blobs[index].preview = Loadable::NotLoaded;
                self.blobs[index].head.clear();
                if self.newer {
                    // The view's commit holds the value replaced: the new one is shown by its
                    // size, its preview read once the view is read again.
                    self.blobs[index].meta = Loadable::Loaded(meta);
                    self.blobs[index]._task = None;
                } else {
                    self.read_blob(index, false, Some(meta), cx);
                }
                // The write's own commit holds the value written, which Save reads there.
                if commit.is_some() {
                    self.blobs[index].bytes_at = Some(commit);
                }
            }
        }
        if !self.writes_out() {
            self.compare_head(cx);
        }
        cx.notify();
    }

    /// The change a conflict on slot `index` offers again, sent bound to what the cell held at
    /// the head.
    fn send_blob_again(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(slot) = self.blobs.get(index) else {
            return;
        };
        let Some(BlobEdit::Failed {
            again: Some((change, now)),
            ..
        }) = slot.edit.clone()
        else {
            return;
        };
        let property = slot.property.clone();
        self.send_blob_write(&property, change, Some(now), cx);
    }

    /// Why slot `index`'s Save is not offered, or `None` when it is: no commit the view knows
    /// holds the value it shows, so a read could only bring another value's bytes.
    fn save_refusal(&self, index: usize) -> Option<&'static str> {
        let slot = self.blobs.get(index)?;
        slot.bytes_at
            .is_none()
            .then_some("Studio knows no commit that holds this value. Refresh, then save it.")
    }

    /// Saves a managed value to a file the person picks: read in ranges where the value the
    /// slot shows is held (its size is that value's), and written to a file beside it, renamed
    /// into place when complete, so memory holds one range and a stopped save leaves no partial
    /// file under the chosen name.
    fn save_blob(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(slot) = self.blobs.get(index) else {
            return;
        };
        let Loadable::Loaded(BlobMeta::Managed { size, .. }) = slot.meta else {
            return;
        };
        let Some(snapshot) = slot.bytes_at.clone() else {
            return;
        };
        let id = self.subject.id().to_owned();
        let name = suggested_file_name(&id, &slot.property, &slot.head);
        let chosen = cx.prompt_for_new_path(&save_folder(), Some(&name));
        let property = slot.property.clone();
        let cell = BlobCell {
            edge: matches!(self.subject, PeekSubject::Edge { .. }),
            type_name: self.subject.type_name().to_owned(),
            id,
            property: property.clone(),
        };
        let session = self.session.downgrade();
        self.blobs[index]._save = Some(cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = chosen.await else {
                return;
            };
            // By property: the view's Blobs are read again on a refresh, the save going on.
            let set = |state: SaveState, cx: &mut gpui::AsyncApp| {
                this.update(cx, |this, cx| {
                    let slot = this.blobs.iter_mut().find(|slot| slot.property == property);
                    if let Some(slot) = slot {
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
        let drop_bg = studio_ui::ds::surface::drop_target(cx);
        v_flex()
            .gap(px(space::MD))
            .children(self.blobs.iter().enumerate().map(|(index, slot)| {
                let saving = self.saving.on(&self.branch, &slot.property);
                let facts = match &slot.meta {
                    _ if saving => format!("saving to {}…", self.branch),
                    Loadable::Loaded(BlobMeta::Managed { size, .. }) => {
                        studio_ui::format_bytes(*size)
                    }
                    Loadable::Loaded(BlobMeta::External { .. }) => "external reference".into(),
                    Loadable::Loaded(BlobMeta::Null) => "null".into(),
                    Loadable::Failed(_) => "could not be read".into(),
                    _ => "reading…".into(),
                };
                let writes = self.render_blob_writes(index, slot, cx);
                let notice = match &slot.edit {
                    Some(BlobEdit::Failed { notice, again }) => {
                        let again = again.as_ref().map(|(change, now)| {
                            studio_ui::ds::row_button(
                                ("peek-blob-again", index),
                                blob_write::again_label(change, now),
                            )
                            .style(ButtonStyle::Subtle)
                            .tip(studio_ui::ds::tooltip(format!(
                                "Write it over what {} holds now, on {}",
                                slot.property, self.branch
                            )))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.send_blob_again(index, cx)),
                            )
                            .into_any_element()
                        });
                        Some(blob_write::notice_row(
                            format!("peek-blob-{index}"),
                            notice,
                            again,
                        ))
                    }
                    _ => None,
                };
                let droppable = self.blob_write_refusal(&slot.property, cx).is_none() && !saving;
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
                        studio_ui::ds::lifted(div(), cx)
                            .w_full()
                            .flex()
                            .justify_center()
                            .p(px(space::MD))
                            .rounded(px(studio_ui::ds::radii(cx).md))
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
                let save: Option<AnyElement> = match &slot.save {
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
                                .tip(studio_ui::ds::tooltip(format!(
                                    "Saved to {}",
                                    path.display()
                                )))
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
                                let open = self.open_blob_action(&slot.property);
                                studio_ui::ds::button(("open-blob", index), "Open")
                                    .style(ButtonStyle::Subtle)
                                    .color(Color::Muted)
                                    .tip(studio_ui::ds::tooltip("Show the whole value in a tab"))
                                    .on_click(move |_, window, cx| {
                                        window.dispatch_action(Box::new(open.clone()), cx)
                                    })
                            })
                            .child({
                                let refused = self.save_refusal(index);
                                studio_ui::ds::button(("save-blob", index), "Save…")
                                    .style(ButtonStyle::Subtle)
                                    .color(Color::Muted)
                                    .disabled(refused.is_some())
                                    .tip(studio_ui::ds::tooltip(
                                        refused.unwrap_or("Save the whole value to a file"),
                                    ))
                                    .on_click(
                                        cx.listener(move |this, _, _, cx| {
                                            this.save_blob(index, cx)
                                        }),
                                    )
                            })
                            .into_any_element(),
                    ),
                    None => None,
                };
                let slot_view = v_flex()
                    .id(("peek-blob", index))
                    .gap(px(space::XS))
                    .child(
                        h_flex()
                            .gap(px(space::MD))
                            .items_center()
                            .child(studio_ui::ds::icon::meta(IconName::File))
                            .child(studio_ui::ds::text::body(slot.property.clone()))
                            .when(saving, |line| {
                                line.child(studio_ui::ds::dot(Color::Warning))
                            })
                            .child(studio_ui::ds::text::meta(facts))
                            .child(div().flex_1())
                            .children(save)
                            .child(writes),
                    )
                    .children(notice)
                    .children(body);
                // A file dropped from the file manager on the slot is stored in it.
                if droppable {
                    slot_view
                        .drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(drop_bg))
                        .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                            this.drop_blob_files(index, paths.paths(), cx)
                        }))
                        .into_any_element()
                } else {
                    slot_view.into_any_element()
                }
            }))
            .into_any_element()
    }

    /// Opening slot `property`'s value in a tab of its own: read where the values the slots
    /// show hold ([`BlobsAt`]: the view's commit, or past it the view's own commit after its own
    /// Blob write over another writer's), so it shows what the slot shows, and held there,
    /// read-only, only when the view holds its commit; a view at the branch head opens one that
    /// writes on its branch, bound to the value read at that commit. Where they hold at no
    /// commit the view knows, the tab reads the branch head, and binds to what it shows.
    fn open_blob_action(&self, property: &str) -> OpenBlob {
        OpenBlob {
            type_name: self.subject.type_name().to_owned(),
            id: self.subject.id().to_owned(),
            property: property.to_owned(),
            edge: matches!(self.subject, PeekSubject::Edge { .. }),
            at_commit: self
                .blobs_at
                .commit(self.snapshot.as_deref())
                .map(str::to_owned),
            held: self.holds_commit,
        }
    }

    /// Slot `index`'s Choose File and Clear, each disabled with its reason where the value
    /// cannot change here.
    fn render_blob_writes(
        &self,
        index: usize,
        slot: &BlobSlot,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cell = self.blob_cell(&slot.property);
        let saving = self.saving.on(&self.branch, &slot.property);
        let pinned = self.held_commit();
        let upload_refusal = self.blob_write_refusal(&slot.property, cx);
        let clear_refusal = blob_write::clear_refusal(
            self.schema.as_deref(),
            &cell,
            pinned.as_deref(),
            self.change_refusal(cx).as_deref(),
            slot.meta.loaded(),
        );
        let branch = self.branch.clone();
        let tooltip = move |refusal: Option<String>, clear: bool| {
            let (cell, branch) = (cell.clone(), branch.clone());
            move |_: &mut Window, cx: &mut App| match &refusal {
                Some(reason) => studio_ui::ds::tooltip_view(reason.clone(), cx),
                None if clear => blob_write::clear_tooltip(&cell, &branch, cx),
                None => blob_write::upload_tooltip(&cell, &branch, cx),
            }
        };
        let property = slot.property.clone();
        h_flex()
            .gap(px(space::XXS))
            .child(
                studio_ui::ds::row_icon_button(("peek-blob-choose", index), IconName::FolderOpen)
                    .icon_color(Color::Muted)
                    .aria_label("Choose File…")
                    .disabled(upload_refusal.is_some() || saving)
                    .tip_fn(tooltip(upload_refusal, false))
                    .on_click(cx.listener(move |this, _, _, cx| this.choose_blob_file(index, cx))),
            )
            .child(
                studio_ui::ds::row_icon_button(("peek-blob-clear", index), IconName::Close)
                    .icon_color(Color::Muted)
                    .aria_label("Clear")
                    .disabled(clear_refusal.is_some() || saving)
                    .tip_fn(tooltip(clear_refusal, true))
                    .on_click(cx.listener(move |this, _, _, cx| this.clear_blob(&property, cx))),
            )
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
                read_at: self.snapshot.clone(),
                more: None,
                _task: None,
            })
            .collect();
        for index in 0..self.groups.len() {
            let key = builder::relationship_key(&self.groups[index].group);
            let Some(known) = self.known.get(&key).cloned() else {
                continue;
            };
            // What the table read for the row's relationship is what the view shows of it: the
            // neighbours it listed, or none, when the view lists them itself through its own
            // reads. A count lists no neighbour; only edges at the node change it.
            let relationship = self.groups[index].group.clone();
            if let PeekSubject::Node { id, .. } = &self.subject {
                let listed = known.first.as_ref().map_or_else(Vec::new, |first| {
                    first.iter().map(|n| n.id.clone()).collect()
                });
                let depends = Depends::EdgesAt {
                    edge: relationship.edge.clone(),
                    nodes: vec![id.clone()],
                    listed: Some((relationship.other_type.clone(), "id".to_owned())),
                };
                self.note_shown(depends, Some(listed), cx);
            }
            // A small group shows its neighbours without a click; those the table did not read
            // are read once its row is drawn, with the edges that count them
            // ([`Self::read_neighbours`]).
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
                Loadable::NotLoaded => this.load_group(index, this.debounce, cx),
                Loadable::Loaded(n)
                    if !group.expanded
                        && matches!(group.neighbours, Loadable::NotLoaded)
                        && this.opens_by_itself(n) =>
                {
                    if let Some(group) = this.groups.get_mut(index) {
                        group.expanded = true;
                    }
                    this.read_neighbours(index, this.debounce, cx);
                    cx.notify();
                }
                _ => {}
            }
        });
    }

    /// Reads a group's first neighbours, one more than the view shows: when they fit, that read
    /// is also the group's exact count, so most groups cost one read rather than a count and
    /// then a page (spec 015). A group past the page shows its first neighbours and reads how
    /// many edges it has: the peek lists each edge, two to one neighbour (a person rehired by
    /// one company) as two rows, so each can be removed. `debounce` delays the read, for a peek that
    /// follows a moving cursor. Only a small group opens by itself; one the person wrote to is
    /// read by [`Self::reload_group`], which opens it.
    fn load_group(&mut self, index: usize, debounce: bool, cx: &mut Context<Self>) {
        let snapshot = self.snapshot.clone();
        self.read_group(
            index,
            snapshot,
            debounce,
            GroupRead::Whole { open: false },
            cx,
        );
    }

    /// Reads group `index`'s first edges at `snapshot` ([`read_first_edges`]), landing them as
    /// `read` says: the whole group, or the neighbours of one whose count is shown already. At
    /// a commit made before what the read names existed, it lands as [`Self::group_refused`]
    /// says, so the group and its neighbours are read alike.
    fn read_group(
        &mut self,
        index: usize,
        snapshot: Option<String>,
        debounce: bool,
        read: GroupRead,
        cx: &mut Context<Self>,
    ) {
        let Some(reads) = self.edge_reads(index) else {
            return;
        };
        let read_without = self.groups_absent.clone();
        let session = self.session.downgrade();
        let scope = self.scope.clone();
        let Some(group) = self.groups.get_mut(index) else {
            return;
        };
        let whole = matches!(read, GroupRead::Whole { .. });
        // A page being read after the first neighbours goes: they are read again.
        group.more = None;
        if whole {
            group.expanded = false;
        }
        let reads = match reads {
            Ok(reads) => reads,
            Err(error) if whole => {
                group.count = Loadable::Failed(error);
                group.neighbours = Loadable::NotLoaded;
                return;
            }
            Err(error) => {
                group.neighbours = Loadable::Failed(error);
                return;
            }
        };
        if whole {
            group.count = Loadable::Loading;
        }
        group.neighbours = Loadable::Loading;
        group.read_at = snapshot.clone();
        let relationship = group.group.clone();
        group._task = Some(cx.spawn(async move |this, cx| {
            if debounce {
                cx.background_executor().timer(COUNT_DELAY).await;
            }
            let first = read_first_edges(&session, scope.as_ref(), reads, snapshot.clone(), cx);
            let Some(first) = first.await else {
                return;
            };
            this.update(cx, |this, cx| {
                match first {
                    Ok(FirstEdges { shown, count }) => {
                        // A group read whole opens once it has any after the person's write to
                        // it, and otherwise when small; one already open stays so.
                        let opens = match (read, &count) {
                            (GroupRead::Whole { open: true }, Loadable::Loaded(n)) => Some(*n > 0),
                            (GroupRead::Whole { .. }, Loadable::Loaded(n)) => {
                                Some(this.opens_by_itself(*n))
                            }
                            (GroupRead::Whole { .. }, _) => Some(false),
                            (GroupRead::Neighbours, _) => None,
                        };
                        if let Some(group) = this.groups.get_mut(index) {
                            // The view lists each edge: its count is the edges this read
                            // counts, where a table's counts each neighbour once.
                            group.count = count;
                            group.neighbours = Loadable::Loaded(shown);
                            if let Some(opens) = opens {
                                group.expanded = opens;
                            }
                        }
                    }
                    Err(error) => this.group_refused(
                        index,
                        &relationship,
                        &read_without,
                        error,
                        snapshot,
                        read,
                        cx,
                    ),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// The reads of group `index`'s first edges ([`read_first_edges`]), or why they cannot be
    /// built; `None` for a view of an edge, before the schema is in, or past the groups.
    fn edge_reads(&self, index: usize) -> Option<Result<EdgeReads, String>> {
        let PeekSubject::Node { type_name, id } = &self.subject else {
            return None;
        };
        let (schema, group) = (self.schema.as_ref()?, self.groups.get(index)?);
        let limit = self.neighbour_limit();
        let first = builder::neighbours_without_properties(
            schema,
            type_name,
            &group.group,
            id,
            None,
            builder::Narrow::default(),
            limit + 1,
            &self.groups_absent,
        );
        Some(first.map_err(|error| error.to_string()).map(|first| {
            let count = builder::edge_degree(schema, type_name, &group.group, id)
                .map_err(|error| error.to_string());
            EdgeReads {
                first,
                count,
                limit,
            }
        }))
    }

    /// A group's read (`read`) was refused. At a commit made before its edge type or the type
    /// at its other end existed, it had no neighbours; one made before a property it reads (the
    /// neighbour's label, an edge property) was added or renamed is read again without it at
    /// the same commit (`snapshot`), once per property: `read_without` is what this read left
    /// out already. Otherwise a group read whole says why on its count, and one whose count was
    /// shown says why under it.
    #[expect(clippy::too_many_arguments)]
    fn group_refused(
        &mut self,
        index: usize,
        relationship: &RelationshipGroup,
        read_without: &[(String, String)],
        error: ReadError,
        snapshot: Option<String>,
        read: GroupRead,
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
                self.read_group(index, snapshot, false, read, cx);
            }
            _ => {
                if let Some(group) = self.groups.get_mut(index) {
                    match read {
                        GroupRead::Whole { .. } => {
                            group.count = Loadable::Failed(error.to_string());
                            group.neighbours = Loadable::NotLoaded;
                        }
                        GroupRead::Neighbours => {
                            group.neighbours = Loadable::Failed(error.to_string());
                        }
                    }
                }
            }
        }
    }

    fn toggle_group(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(group) = self.groups.get_mut(index) else {
            return;
        };
        group.expanded = !group.expanded;
        if group.expanded && matches!(group.neighbours, Loadable::NotLoaded) {
            self.read_neighbours(index, false, cx);
        }
        cx.notify();
    }

    /// Reads the first neighbours of a group whose count is known, at the view's commit, as
    /// [`Self::load_group`] reads them: the view lists each edge, so the group's count becomes
    /// the edges that read counts, where the one a table gave counts neighbours, each once
    /// however many edges reach it. `debounce` waits first, for a peek that follows a moving
    /// cursor: the rows it passes read nothing.
    fn read_neighbours(&mut self, index: usize, debounce: bool, cx: &mut Context<Self>) {
        let snapshot = self.snapshot.clone();
        self.read_group(index, snapshot, debounce, GroupRead::Neighbours, cx);
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
        // After the last edge shown: a second edge to the same neighbour comes next.
        let Some((after, after_edge)) = group
            .neighbours
            .loaded()
            .and_then(|shown| shown.last())
            .map(|last| (last.id.clone(), last.edge_id.clone()))
        else {
            return;
        };
        let after = match &after_edge {
            Some(edge) => builder::After::Edge {
                neighbour: &after,
                edge,
            },
            None => builder::After::Neighbour(&after),
        };
        if group.more.is_some() {
            return;
        }
        // At the commit its first page was read: one a write from a view behind reached is
        // not the view's own.
        let snapshot = group.read_at.clone();
        let query = match builder::neighbours_without_properties(
            &schema,
            &type_name,
            &group.group,
            &id,
            Some(after),
            builder::Narrow::default(),
            PAGE_NEIGHBOURS,
            &without,
        ) {
            Ok(query) => query,
            Err(_) => return,
        };
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

    /// Whether this view edits `property` in place: a node's property, read at the head of a
    /// branch the credential may change, of a kind a table cell edits too.
    fn edits_property(&self, property: &gq_language::Property, cx: &App) -> bool {
        matches!(self.subject, PeekSubject::Node { .. })
            && !self.holds_commit
            && property.editable_in_cell()
            && self.values.loaded().is_some()
            && self.change_refusal(cx).is_none()
    }

    /// Why the credential may not change the branch the view reads, once the access route said
    /// so (spec 002): its fields do not open, and its relationship and Blob controls are
    /// disabled with this as their reason. A view held at a commit has its own reason.
    fn change_refusal(&self, cx: &App) -> Option<String> {
        if self.holds_commit || !matches!(self.subject, PeekSubject::Node { .. }) {
            return None;
        }
        crate::write::change_refusal(&self.session, &self.branch, cx)
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

    /// The property's draft on the branch the view reads.
    fn held_draft(&self, name: &str, cx: &App) -> Option<DraftEntry> {
        let key = self.draft_key(name)?;
        self.session.read(cx).draft_on(&self.branch, &key).cloned()
    }

    /// Whether the property's draft on the branch the view reads is still `sent`
    /// ([`GraphSession::draft_as_sent`]). A property with no draft (an edge's) always is.
    fn draft_as_sent(&self, name: &str, sent: Option<&DraftEntry>, cx: &App) -> bool {
        self.draft_key(name).is_none_or(|key| {
            self.session
                .read(cx)
                .draft_as_sent(&self.branch, &key, sent)
        })
    }

    /// Settles the property's draft on `branch` once the write or read it waited on is in
    /// ([`GraphSession::settle_draft_on`]): `keep` it, or let it go, unless it is no longer
    /// `sent`, the draft as that went out. One retyped or let go elsewhere meanwhile (in a table
    /// cell, on another page) stands.
    fn settle_draft_on(
        &mut self,
        branch: &str,
        name: &str,
        sent: Option<&DraftEntry>,
        keep: Option<DraftEntry>,
        cx: &mut Context<Self>,
    ) {
        if let Some(key) = self.draft_key(name) {
            self.session.update(cx, |session, cx| {
                session.settle_draft_on(branch, key, sent, keep, cx)
            });
        }
    }

    fn put_draft(&mut self, name: &str, old: Value, text: String, cx: &mut Context<Self>) {
        if let Some(key) = self.draft_key(name) {
            self.session.update(cx, |session, cx| {
                session.set_draft(key, DraftEntry { old, text }, cx)
            });
        }
    }

    fn drop_draft(&mut self, name: &str, cx: &mut Context<Self>) {
        if let Some(key) = self.draft_key(name) {
            let branch = self.branch.clone();
            self.session
                .update(cx, |session, cx| session.remove_draft_on(&branch, &key, cx));
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
        if !self.edits_property(&property, cx)
            || self.saving.on(&self.branch, &name)
            || self.saving_elsewhere.contains(&name)
        {
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
        let unchanged = self.asks_nothing(name, edit, &text);
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
                if !self.asks_nothing(name, &edit, &text) {
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

    /// ↵: writes the field's text, parsed for its type. Text asking for the value the field is
    /// bound to sends nothing while the node still holds it, and a draft typed back to it goes;
    /// over a value the node holds since (a draft typed before a Refresh), the bound write goes,
    /// and finds the change.
    fn save_field(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.edits.get_mut(name) else {
            return;
        };
        let text = edit.editor.read(cx).text(cx);
        let new = match edited(&edit.property, &edit.old, &text) {
            Some(Ok(value)) => Some(value),
            Some(Err(message)) => {
                edit.notice = Some(Notice::new(Tone::Invalid, message));
                cx.notify();
                return;
            }
            None => None,
        };
        let shown = self.value_of(name);
        let Some(edit) = self.edits.remove(name) else {
            return;
        };
        let new = match new {
            Some(new) => new,
            None if same_value(&edit.old, &shown) => {
                self.drop_draft(name, cx);
                cx.notify();
                return;
            }
            None => edit.old.clone(),
        };
        self.write_value(edit.property, edit.old, new, text, window, cx);
    }

    /// Whether a field's text asks for nothing: for the value the field is bound to, while the
    /// node still holds it. Over a value it holds since (a draft typed before a Refresh), asking
    /// for the one typed over is a change, which its save finds.
    fn asks_nothing(&self, name: &str, edit: &FieldEdit, text: &str) -> bool {
        edited(&edit.property, &edit.old, text).is_none()
            && same_value(&edit.old, &self.value_of(name))
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
        // A field open when the access route refused the branch, or a property already being
        // written (here, or by another view of the node; a switch clicked twice before it was
        // drawn again): what was typed stays as its draft, and nothing is sent. One write of a
        // property is out at a time, so two never land in an order nobody chose.
        let writing = self.saving.on(&self.branch, &name) || self.saving_elsewhere.contains(&name);
        if writing || self.change_refusal(cx).is_some() {
            self.edits.remove(&name);
            if !same_value(&old, &new) {
                self.put_draft(&name, old, text, cx);
            }
            cx.notify();
            return;
        }
        let write = match crate::write::write_properties(
            &self.session,
            &self.branch,
            &schema,
            &type_name,
            &id,
            &[(name.clone(), old.clone(), new.clone())],
            None,
            cx,
        ) {
            Ok(write) => write,
            Err(error) => {
                let notice = Notice::new(Tone::Invalid, error);
                let held = self.held_draft(&name, cx);
                self.reopen(property, (old, text, notice), held.as_ref(), window, cx);
                return;
            }
        };
        // The new value shows, with a dot, while the write is out.
        let shown = self.value_of(&name);
        if let Loadable::Loaded(values) = &mut self.values {
            values.insert(name.clone(), new.clone());
        }
        let (session, key) = (self.session.clone(), self.draft_key(&name));
        let sent = self.held_draft(&name, cx);
        self.saving.insert(&self.branch, &name);
        let branch = self.branch.clone();
        // Detached: closing the page or moving the peek never cancels a write.
        cx.spawn_in(window, async move |this, cx| {
            let result = write.await;
            // The text of a write that did not land is kept in its draft.
            let keep = (!matches!(result, SaveResult::Saved { .. })).then(|| DraftEntry {
                old: old.clone(),
                text: text.clone(),
            });
            let settled = this.update_in(cx, |this, window, cx| {
                if this.branch == branch {
                    let field = FieldWrite {
                        property: property.clone(),
                        old: old.clone(),
                        new,
                        text: text.clone(),
                        shown,
                        sent: sent.clone(),
                    };
                    this.finish_write(field, result, window, cx);
                    return;
                }
                // The view reads another branch now; the property's draft was the branch left's.
                this.saving.remove(&branch, &property.name);
                this.settle_draft_on(&branch, &property.name, sent.as_ref(), keep.clone(), cx);
                cx.notify();
            });
            // The view went first (the table's cursor moved on, the page closed): the
            // property's draft is settled where it was typed.
            if settled.is_err()
                && let Some(key) = key
            {
                session.update(cx, |session, cx| {
                    session.settle_draft_on(&branch, key, sent.as_ref(), keep, cx)
                });
            }
        })
        .detach();
        cx.notify();
    }

    fn finish_write(
        &mut self,
        field: FieldWrite,
        result: SaveResult,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let FieldWrite {
            property,
            old,
            new,
            text,
            shown,
            sent,
        } = field;
        let name = property.name.clone();
        self.saving.remove(&self.branch, &name);
        match result {
            SaveResult::Saved { commit, parent } => {
                let branch = self.branch.clone();
                self.settle_draft_on(&branch, &name, sent.as_ref(), None, cx);
                // Made on the commit shown, the view holds the new commit's state, the value
                // written included: shown again, since a read of the node that landed while the
                // write was out (a Refresh's, another field's conflict's) put back the value it
                // replaced. Otherwise someone else committed in between and the view is behind,
                // showing what it read.
                if self.follow_own_commit(parent.as_deref(), &commit) {
                    self.restore(&name, new);
                }
            }
            SaveResult::NoMatch => {
                self.put_back(&name, &new, shown);
                self.newer = true;
                self.reread_conflict(property, (old, new, text), sent, window, cx);
            }
            SaveResult::Refused(notice) | SaveResult::Unknown(notice) => {
                self.put_back(&name, &new, shown);
                self.reopen(property, (old, text, notice), sent.as_ref(), window, cx);
            }
        }
        if !self.writes_out() {
            self.compare_head(cx);
        }
        cx.notify();
    }

    /// A field's write matched nothing: the value it was bound to changed elsewhere. The view
    /// moves to the branch's head: the head is asked after, and the node read again at the one
    /// the session then holds ([`Self::reread_at_head`]). The field opens again bound to the
    /// value the view then shows, which the notice names. When the node could not be read, or
    /// the read still finds the value the field was typed over, it stays bound to that value.
    /// A read that finds the value the write would have made has nothing left to write: an
    /// earlier try whose outcome was unknown landed, or the same change was made elsewhere. The
    /// field closes and its draft goes, as ⌘S's drafts do ([`Self::drafts_read`]), rather than
    /// the person's own change being said to be another's. The field shows as saving until the
    /// read lands. `edit` is the value the field was bound to, the value it would have written
    /// and its text; `sent` the property's draft as the write went out, which a draft retyped
    /// or let go elsewhere meanwhile no longer is, and stands.
    fn reread_conflict(
        &mut self,
        property: gq_language::Property,
        edit: (Value, Value, String),
        sent: Option<DraftEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (old, new, text) = edit;
        let name = property.name.clone();
        self.saving.insert(&self.branch, &name);
        let (session, branch, key) = (
            self.session.clone(),
            self.branch.clone(),
            self.draft_key(&name),
        );
        let head = session.update(cx, |session, cx| session.head_now(&branch, cx));
        // Detached, as the write was: the text is settled however the view ends.
        cx.spawn_in(window, async move |this, cx| {
            head.await;
            let read = this
                .update(cx, |this, cx| this.reread_at_head(&branch, cx))
                .ok()
                .flatten();
            let landed = match read {
                Some((read, sent)) => Some((read.await, sent)),
                None => None,
            };
            let found = match this.update(cx, |this, cx| this.landed_or_newer(landed, cx)) {
                Ok(Ok(found)) => found,
                Ok(Err(newer)) => newer.await.ok().flatten(),
                Err(_) => None,
            };
            let shown = this.update_in(cx, |this, window, cx| {
                this.saving.remove(&branch, &name);
                if this.branch == branch {
                    // Without a read of its own, it settles against what the view shows.
                    let now = found.or_else(|| this.values.loaded().cloned().map(Some));
                    let written = match &now {
                        Some(Some(values)) => {
                            same_value(values.get(&name).unwrap_or(&Value::Null), &new)
                        }
                        _ => false,
                    };
                    if written {
                        this.settle_draft_on(&branch, &name, sent.as_ref(), None, cx);
                        if !this.writes_out() {
                            this.compare_head(cx);
                        }
                        cx.notify();
                        return;
                    }
                    let (bound, notice) = match &now {
                        Some(Some(values)) => {
                            let value = values.get(&name).cloned().unwrap_or(Value::Null);
                            if same_value(&value, &old) {
                                // The read did not reach the change.
                                (old.clone(), Notice::new(Tone::Conflict, CHANGED_UNREAD))
                            } else {
                                let notice = changed_elsewhere(Some(&property), Some(&value));
                                (value, notice)
                            }
                        }
                        Some(None) => (old.clone(), Notice::new(Tone::Conflict, NODE_GONE)),
                        None => (old.clone(), Notice::new(Tone::Conflict, CHANGED_UNREAD)),
                    };
                    let edit = (bound, text.clone(), notice);
                    this.reopen(property.clone(), edit, sent.as_ref(), window, cx);
                } else {
                    // The view reads another branch now: the text stays a draft where it was
                    // typed.
                    let keep = Some(DraftEntry {
                        old: old.clone(),
                        text: text.clone(),
                    });
                    this.settle_draft_on(&branch, &name, sent.as_ref(), keep, cx);
                }
                if !this.writes_out() {
                    this.compare_head(cx);
                }
                cx.notify();
            });
            // The view went first: the text stays a draft where it was typed, bound to the
            // value it was typed over, since what the head holds was never shown.
            if shown.is_err()
                && let Some(key) = key
            {
                let keep = Some(DraftEntry { old, text });
                session.update(cx, |session, cx| {
                    session.settle_draft_on(&branch, key, sent.as_ref(), keep, cx)
                });
            }
        })
        .detach();
    }

    /// ⌘S matched nothing: a value its drafts were bound to changed elsewhere, and nothing was
    /// written. The view moves to the branch's head as for a field ([`Self::reread_conflict`]),
    /// and [`Self::drafts_read`] settles the drafts against what it then shows; each shows as
    /// saving until the read lands.
    fn reread_drafts(&mut self, sent: Vec<SentDraft>, cx: &mut Context<Self>) {
        for draft in &sent {
            self.saving.insert(&self.branch, &draft.name);
        }
        let branch = self.branch.clone();
        let head = self
            .session
            .update(cx, |session, cx| session.head_now(&branch, cx));
        // Detached: a view that goes leaves the drafts as they were bound, since what the head
        // holds was never shown.
        cx.spawn(async move |this, cx| {
            head.await;
            let read = this
                .update(cx, |this, cx| this.reread_at_head(&branch, cx))
                .ok()
                .flatten();
            let landed = match read {
                Some((read, node)) => Some((read.await, node)),
                None => None,
            };
            let found = match this.update(cx, |this, cx| this.landed_or_newer(landed, cx)) {
                Ok(Ok(found)) => found,
                Ok(Err(newer)) => newer.await.ok().flatten(),
                Err(_) => None,
            };
            this.update(cx, |this, cx| {
                for draft in &sent {
                    this.saving.remove(&branch, &draft.name);
                }
                // The view reads another branch now: the drafts stay as they were typed there.
                if this.branch == branch {
                    // Without a read of its own, they settle against what the view shows.
                    let now = found.or_else(|| this.values.loaded().cloned().map(Some));
                    this.drafts_read(sent, now.as_ref(), cx);
                }
                if !this.writes_out() {
                    this.compare_head(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Settles drafts ⌘S sent that matched nothing, against the node read again at the head
    /// (`now`, as [`Held`] says it), which the view shows. A draft asking for what the node
    /// holds now goes; one over a value that changed is bound to the value read; the others
    /// stay as they were bound. The drafts' notice names what changed. A draft retyped,
    /// rebound or let go while the read was out, here or in another view, stands as it is.
    fn drafts_read(
        &mut self,
        sent: Vec<SentDraft>,
        now: Option<&Option<Map<String, Value>>>,
        cx: &mut Context<Self>,
    ) {
        let mut changed = Vec::new();
        if let Some(Some(values)) = now {
            for draft in sent {
                let SentDraft {
                    name,
                    old,
                    new,
                    text,
                } = draft;
                let entry = DraftEntry {
                    old: old.clone(),
                    text: text.clone(),
                };
                if !self.draft_as_sent(&name, Some(&entry), cx) {
                    continue;
                }
                let value = values.get(&name).cloned().unwrap_or(Value::Null);
                if same_value(&value, &new) {
                    self.drop_draft(&name, cx);
                } else if !same_value(&value, &old) {
                    let shown = display_value(self.property_named(&name), &value);
                    changed.push(format!("{name} to “{shown}”"));
                    self.put_draft(&name, value, text, cx);
                }
            }
        }
        let message = match now {
            Some(Some(_)) if changed.is_empty() => {
                "Nothing was written; your changes are kept".to_owned()
            }
            Some(Some(_)) => format!(
                "Changed elsewhere: {}; nothing was written and your changes are kept",
                changed.join(", ")
            ),
            Some(None) => {
                "This node was deleted elsewhere; nothing was written and your changes are kept"
                    .to_owned()
            }
            None => "A value changed elsewhere; nothing was written and your changes are kept"
                .to_owned(),
        };
        // Drafts that all went need no word.
        let kept = !self.node_drafts(cx).is_empty();
        self.drafts_notice = kept.then(|| Notice::new(Tone::Conflict, message));
        self.confirm_discard = false;
    }

    /// The subject's property `name` in the view's schema.
    fn property_named(&self, name: &str) -> Option<&gq_language::Property> {
        let schema = self.schema.as_ref()?;
        let properties = match &self.subject {
            PeekSubject::Node { type_name, .. } => &schema.node(type_name)?.properties,
            PeekSubject::Edge { edge_type, .. } => &schema.edge(edge_type)?.properties,
        };
        properties.iter().find(|property| property.name == name)
    }

    fn restore(&mut self, name: &str, value: Value) {
        if let Loadable::Loaded(values) = &mut self.values {
            values.insert(name.to_owned(), value);
        }
    }

    /// Puts back what the view showed of a property (`shown`) before a write that did not land
    /// put `new` there: the value read, where the one the write was bound to may not be (a
    /// draft typed before a Refresh). A node read again while the write was out shows what was
    /// read, and keeps it.
    fn put_back(&mut self, name: &str, new: &Value, shown: Value) {
        if same_value(&self.value_of(name), new) {
            self.restore(name, shown);
        }
    }

    /// Opens the field again with what was typed and why it did not land (constitution III),
    /// `edit`: bound to the value it holds, with the text and the notice. A field that cannot
    /// open now (the node is being read again) keeps the text as its draft, bound alike, and the
    /// node's drafts say why. A draft retyped or let go elsewhere while the write was out (no
    /// longer `sent`) stands, and nothing reopens over it.
    fn reopen(
        &mut self,
        property: gq_language::Property,
        edit: (Value, String, Notice),
        sent: Option<&DraftEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (old, text, notice) = edit;
        let name = property.name.clone();
        if !self.draft_as_sent(&name, sent, cx) {
            return;
        }
        // Not over what the person does meanwhile (another field open, the keyboard in a table
        // cell): opened, the field would take the keyboard and what they type. The text is kept
        // as the field's draft then, and the node's drafts say why.
        let busy = self.edits.keys().any(|open| *open != name)
            || window
                .focused(cx)
                .is_some_and(|focused| !self.focus_handle.contains(&focused, window));
        if !busy {
            self.start_edit(property, window, cx);
        }
        match self.edits.get_mut(&name) {
            Some(edit) => {
                edit.old = old;
                edit.notice = Some(notice);
                edit.editor
                    .update(cx, |editor, cx| editor.set_text(text, window, cx));
            }
            None => {
                self.put_draft(&name, old, text, cx);
                self.drafts_notice = Some(notice);
                self.confirm_discard = false;
            }
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
        self.session.update(cx, |session, cx| {
            session.read_scoped(scope.as_ref(), query, snapshot, cx)
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

    /// Why group `index` takes no relationship change now: one to it is out, and a second sent
    /// meanwhile would be sent on top of a result not known yet (a second edge to the same node,
    /// a removal of one already gone).
    fn edge_write_refusal(&self, index: usize) -> Option<String> {
        self.edge_writes
            .contains(&index)
            .then(|| "A change to this relationship is being saved".to_owned())
    }

    /// "+ Add": a search over the other type, this node already the relationship's end.
    fn start_add(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .adding
            .as_ref()
            .is_some_and(|adding| adding.index == index)
            || self.change_refusal(cx).is_some()
            || self.replace_refusal(index).is_some()
            || self.edge_write_refusal(index).is_some()
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
            offers: Offers::default(),
            found: Loadable::NotLoaded,
            answers: None,
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
                    // A failed search offers nothing, not what an earlier one found.
                    match result {
                        Ok(rows) => {
                            adding.offers.set(suggest::from_node_rows(&rows), true);
                            adding.found = Loadable::Loaded(());
                        }
                        Err(error) => {
                            adding.offers.clear();
                            adding.found = Loadable::Failed(error);
                        }
                    }
                    adding.answers = Some(text);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// ↑/↓ in the search: highlights another node, around the ends.
    fn move_match(&mut self, step: isize, cx: &mut Context<Self>) {
        if let Some(adding) = self.adding.as_mut()
            && adding.offers.step(step)
        {
            cx.notify();
        }
    }

    fn cancel_add(&mut self, cx: &mut Context<Self>) {
        self.adding = None;
        cx.notify();
    }

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

    /// Adds the relationship to the highlighted node ([`Self::add_offered`]).
    fn add_highlighted(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let highlighted = self
            .adding
            .as_ref()
            .and_then(|adding| adding.offers.highlighted());
        if let Some(highlighted) = highlighted {
            self.add_offered(highlighted, window, cx);
        }
    }

    /// Adds the relationship to the node the search offers at `offer`. On a group that holds
    /// one, it replaces the one there, removed first so the limit holds: the server takes a
    /// removal and an addition only in separate mutations (one mutation either adds or removes),
    /// so a replace is two commits. Until the edge it replaces is known, nothing is written and
    /// the group is read with its edges: a lone addition would be refused for the limit. A
    /// group that cannot be replaced so ([`Self::replace_refusal`]) sends nothing, and says why.
    fn add_offered(&mut self, offer: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(adding) = self.adding.as_ref() else {
            return;
        };
        let Some(picked) = adding.offers.get(offer).map(|offer| offer.value.clone()) else {
            return;
        };
        let index = adding.index;
        if let Some(reason) = self.replace_refusal(index) {
            self.adding = None;
            self.group_notices
                .insert(index, Notice::new(Tone::Invalid, reason));
            cx.notify();
            return;
        }
        let replaced = if self.holds_one(index) {
            match self.replaced_edge(index) {
                Some(replaced) => replaced,
                None => {
                    if !matches!(self.groups[index].neighbours, Loadable::Loading) {
                        self.reload_group(index, None, cx);
                    }
                    cx.notify();
                    return;
                }
            }
        } else {
            None
        };
        self.adding = None;
        self.write_edge(index, EdgeChange::Add(picked), replaced, window, cx);
    }

    /// Why a new relationship of group `index` cannot replace the one it holds, or `None` when
    /// it can (or it holds none): each node at its source needs exactly one (`@card(1..1)`), so
    /// the removal alone is refused, as is a second one added first, and no one mutation takes
    /// both.
    fn replace_refusal(&self, index: usize) -> Option<String> {
        let (schema, group) = (self.schema.as_ref()?, self.groups.get(index)?);
        let edge = schema.edge(&group.group.edge)?;
        let replaces = self.holds_one(index) && matches!(group.count, Loadable::Loaded(n) if n > 0);
        (replaces && edge.min >= 1).then(|| {
            format!(
                "Each {from} has exactly one {name}, so Studio cannot replace it: the server takes \
                 its removal and the new one only as two commits, and each alone breaks that rule.",
                from = edge.from,
                name = edge.name,
            )
        })
    }

    /// The edge a new relationship of a group that holds one replaces: `Some(None)` when the
    /// group has none, and `None` while it is not known (its neighbours not read yet, read
    /// without their edges, or not readable).
    fn replaced_edge(&self, index: usize) -> Option<Option<String>> {
        let group = self.groups.get(index)?;
        if matches!(group.count, Loadable::Loaded(0)) {
            return Some(None);
        }
        match group.neighbours.loaded()?.first() {
            Some(first) => first.edge_id.clone().map(Some),
            None => Some(None),
        }
    }

    /// The first click on a neighbour's × asks; the second removes the relationship.
    fn press_remove(
        &mut self,
        index: usize,
        edge_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.edge_write_refusal(index).is_some() {
            return;
        }
        if self.removing.as_deref() == Some(edge_id.as_str()) {
            self.removing = None;
            self.write_edge(index, EdgeChange::Remove(edge_id), None, window, cx);
        } else {
            self.removing = Some(edge_id);
            cx.notify();
        }
    }

    /// Writes one relationship change, after removing `first` when it is given (a replace),
    /// each as its own commit: one mutation either adds or removes, so the server takes the two
    /// only apart. The addition is sent only once the removal is on the branch, and how each
    /// ended is said as it is ([`edge_written`]): the addition's after what the removal did.
    /// The group is read again where the writes landed, at the view's commit when it follows
    /// them.
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
        // One change to the group at a time: a second waits for the first's result.
        if self.edge_write_refusal(index).is_some() {
            return;
        }
        // Asked for before the access route refused the branch: nothing is sent, and the group
        // says why.
        if let Some(reason) = self.change_refusal(cx) {
            self.group_notices
                .insert(index, Notice::new(Tone::Invalid, reason));
            cx.notify();
            return;
        }
        let mut queries = Vec::new();
        if let Some(edge_id) = &first {
            queries.push((
                builder::delete_edge(&schema, &group.edge, edge_id),
                EdgeStep::ReplaceRemove,
            ));
        }
        queries.push(match &change {
            EdgeChange::Add(other) => {
                let (from, to) = match group.direction {
                    Direction::Outgoing => (id.as_str(), other.as_str()),
                    Direction::Incoming => (other.as_str(), id.as_str()),
                };
                let step = if first.is_some() {
                    EdgeStep::ReplaceAdd
                } else {
                    EdgeStep::Add
                };
                (
                    builder::insert_edge(&schema, &group.edge, from, to, &[]),
                    step,
                )
            }
            EdgeChange::Remove(edge_id) => (
                builder::delete_edge(&schema, &group.edge, edge_id),
                EdgeStep::Remove,
            ),
        });
        let mut built = Vec::new();
        for (query, step) in queries {
            match query {
                Ok(query) => built.push((query, step)),
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
        // Detached: closing the page or moving the peek never cancels a write.
        cx.spawn_in(window, async move |this, cx| {
            // A replace is two commits, the removal then the addition. The page follows its own
            // commits while each is on top of the one before, and the first is on top of the
            // commit the page shows when they land: another write of the page's, started with
            // this one, may have moved it there meanwhile.
            let mut ended = None;
            let mut first_parent: Option<String> = None;
            let mut reached: Option<String> = None;
            let mut chained = true;
            let mut landed = 0;
            for (query, step) in built {
                let description = format!("{} {edge} of {id} · {other}", step.verb());
                let write = session.update(cx, |session, cx| {
                    session.write_on(&branch, &query, None, description, None, cx)
                });
                match edge_written(&write.await, &branch, step) {
                    EdgeWritten::Saved { commit, parent } => {
                        if landed == 0 {
                            first_parent = parent.clone();
                            chained &= parent.is_some();
                        } else {
                            chained &= parent.is_some() && parent == reached;
                        }
                        reached = Some(commit);
                        landed += 1;
                    }
                    written => {
                        ended = Some(written);
                        break;
                    }
                }
            }
            this.update(cx, |this, cx| {
                this.edge_writes.remove(&index);
                // The view reads another branch now: its toasts tell how the writes went.
                if this.branch != branch {
                    cx.notify();
                    return;
                }
                // The group is read again at the view's commit when the view followed its writes
                // there. Behind them, it is read at the commit they reached, where it holds what
                // was written, and the view offers Refresh for the rest.
                let mut read_at = None;
                if landed > 0 {
                    let followed =
                        chained && first_parent.is_some() && this.snapshot == first_parent;
                    // Made one on another from where the Blob values hold, the writes hold them
                    // at the last; a gap between them leaves them where the view cannot see.
                    if let Some(reached) = &reached {
                        let pin = this.snapshot.clone();
                        let parent = first_parent.as_deref().filter(|_| chained);
                        this.blobs_at
                            .after_own_write(pin.as_deref(), parent, reached, followed);
                    }
                    if followed {
                        this.snapshot = reached;
                    } else {
                        this.newer = true;
                        read_at = reached;
                    }
                }
                // After the removal landed, an addition that did not leaves the group without
                // the relationship it replaced: said first, never "nothing was written".
                let notice = match ended {
                    None | Some(EdgeWritten::Saved { .. }) => None,
                    Some(EdgeWritten::NotWritten(mut notice)) if landed > 0 => {
                        notice.message = format!(
                            "The relationship it replaces was removed, but the new one was not \
                             added. {}",
                            notice.message
                        );
                        Some(notice)
                    }
                    Some(EdgeWritten::Unsettled(mut notice)) if landed > 0 => {
                        notice.message = format!(
                            "The relationship it replaces was removed. {}",
                            notice.message
                        );
                        Some(notice)
                    }
                    Some(EdgeWritten::NotWritten(notice) | EdgeWritten::Unsettled(notice)) => {
                        Some(notice)
                    }
                };
                if let Some(notice) = notice {
                    this.group_notices.insert(index, notice);
                }
                this.reload_group(index, read_at, cx);
                if !this.writes_out() {
                    this.compare_head(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Reads a group's count and neighbours again, at `at` or else the view's commit, and opens
    /// it when it has any.
    fn reload_group(&mut self, index: usize, at: Option<String>, cx: &mut Context<Self>) {
        let snapshot = at.or_else(|| self.snapshot.clone());
        self.read_group(index, snapshot, false, GroupRead::Whole { open: true }, cx);
    }

    /// ⌘S: the node's drafts as one mutation, each bound to the value it was typed over, so one
    /// commit.
    /// A draft that does not parse opens instead, and nothing is sent.
    fn save_drafts(&mut self, _: &SaveNodeDrafts, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving_drafts || !self.edits_relationships() || self.change_refusal(cx).is_some() {
            return;
        }
        // An open field joins the others with what it holds now. One showing a notice stays open,
        // its reason in view, so its text is taken into its draft here: the draft would else be
        // what an earlier leave kept, older than what the field shows.
        let open: Vec<String> = self.edits.keys().cloned().collect();
        for name in open {
            let typed = self
                .edits
                .get(&name)
                .filter(|edit| edit.notice.is_some())
                .map(|edit| {
                    let text = edit.editor.read(cx).text(cx);
                    (
                        self.asks_nothing(&name, edit, &text),
                        edit.old.clone(),
                        text,
                    )
                });
            if let Some((false, old, text)) = typed {
                self.put_draft(&name, old, text, cx);
            }
            self.left_field(&name, cx);
        }
        let (PeekSubject::Node { type_name, id }, Some(schema)) =
            (self.subject.clone(), self.schema.clone())
        else {
            return;
        };
        let mut sent = Vec::new();
        for (name, draft) in self.node_drafts(cx) {
            // A field's own save under way keeps its draft until it lands; sent again, the two
            // writes would race on one value.
            if self.saving.on(&self.branch, &name) {
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
            let new = match edited(&property, &old, &draft.text) {
                Some(Ok(new)) => new,
                Some(Err(message)) => {
                    let notice = Notice::new(Tone::Invalid, message);
                    let held = self.held_draft(&name, cx);
                    self.reopen(
                        property,
                        (old, draft.text, notice),
                        held.as_ref(),
                        window,
                        cx,
                    );
                    return;
                }
                // Asks for the value it was typed over, which the node still holds.
                None if same_value(&old, &self.value_of(&name)) => {
                    self.drop_draft(&name, cx);
                    continue;
                }
                // Over a value the node holds since: saved as typed, so the bound write finds
                // the change.
                None => old.clone(),
            };
            sent.push(SentDraft {
                name,
                old,
                new,
                text: draft.text,
            });
        }
        if sent.is_empty() {
            cx.notify();
            return;
        }
        let cells: Vec<(String, Value, Value)> = sent
            .iter()
            .map(|draft| (draft.name.clone(), draft.old.clone(), draft.new.clone()))
            .collect();
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
            None,
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
        for draft in &sent {
            self.saving.insert(&self.branch, &draft.name);
        }
        let session = self.session.clone();
        let keys: Vec<(DraftKey, DraftEntry)> = sent
            .iter()
            .filter_map(|draft| Some((self.draft_key(&draft.name)?, draft.entry())))
            .collect();
        cx.spawn_in(window, async move |this, cx| {
            let result = write.await;
            let saved = matches!(result, SaveResult::Saved { .. });
            let shown = this.update(cx, |this, cx| {
                this.saving_drafts = false;
                for draft in &sent {
                    this.saving.remove(&branch, &draft.name);
                }
                // The view reads another branch now: the drafts saved were the branch left's.
                if this.branch != branch {
                    if let SaveResult::Saved { .. } = result {
                        for draft in &sent {
                            let entry = draft.entry();
                            this.settle_draft_on(&branch, &draft.name, Some(&entry), None, cx);
                        }
                    }
                    cx.notify();
                    return;
                }
                match result {
                    SaveResult::Saved { commit, parent } => {
                        for draft in sent {
                            let entry = draft.entry();
                            this.settle_draft_on(&branch, &draft.name, Some(&entry), None, cx);
                            this.restore(&draft.name, draft.new);
                        }
                        this.follow_own_commit(parent.as_deref(), &commit);
                    }
                    SaveResult::NoMatch => {
                        this.newer = true;
                        // The view moves to the head, and the drafts are bound to what it
                        // holds there.
                        this.reread_drafts(sent, cx);
                    }
                    SaveResult::Refused(notice) | SaveResult::Unknown(notice) => {
                        this.drafts_notice = Some(notice);
                    }
                }
                if !this.writes_out() {
                    this.compare_head(cx);
                }
                cx.notify();
            });
            // The view went first: drafts that landed are let go, as it would have; the others
            // stay for the next view of the node.
            if shown.is_err() && saved {
                session.update(cx, |session, cx| {
                    for (key, sent) in keys {
                        session.settle_draft_on(&branch, key, Some(&sent), None, cx);
                    }
                });
            }
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
        // Held at a commit, the view writes nothing: its Save would do nothing, and its Discard
        // would act on a branch it no longer follows.
        if self.holds_commit {
            return None;
        }
        let count = self.node_drafts(cx).len();
        if count == 0 && !self.saving_drafts {
            return None;
        }
        let branch = self.session.read(cx).branch.clone();
        let refused = self.change_refusal(cx);
        let label = match count {
            1 => "1 change not saved".to_owned(),
            n => format!("{} changes not saved", studio_ui::group_digits(n)),
        };
        Some(
            h_flex()
                .gap(px(space::SM))
                .items_center()
                .child(studio_ui::ds::dot(Color::Accent))
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
                    .disabled(self.saving_drafts || refused.is_some())
                    .tip_fn(move |_, cx| match &refused {
                        Some(reason) => studio_ui::ds::tooltip_view(reason.clone(), cx),
                        None => studio_ui::ds::tooltip_for_action_view(
                            format!("One commit to {branch}"),
                            &SaveNodeDrafts,
                            cx,
                        ),
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
        // comparison per property, and nothing is made or kept.
        let Some(schema) = self.schema.clone() else {
            return;
        };
        let mut shown = 0;
        let unchanged = listed_properties(&schema, &self.subject)
            .into_iter()
            .filter(|property| reads_as_markdown(property))
            .filter_map(|property| {
                let text = values.get(&property.name).and_then(Value::as_str)?;
                Some((&property.name, text))
            })
            .all(|(name, text)| {
                shown += 1;
                self.markdown
                    .get(name)
                    .is_some_and(|(drawn, _)| drawn == text)
                    || self.plain_text.get(name).is_some_and(|drawn| drawn == text)
            });
        if unchanged && shown == self.markdown.len() + self.plain_text.len() {
            return;
        }
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

    /// The node's properties, after the reference meeting card's body (spec 014): a row per
    /// property, its kind and its name, then its value boxed (a list's items each boxed, an
    /// enum its select, a truth value its switch); then each text longer than a box holds,
    /// under its name, which folds it, at the full width.
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
        let properties = self
            .schema
            .as_deref()
            .map(|schema| listed_properties(schema, &self.subject))
            .unwrap_or_default();
        let (rows, texts): (Vec<_>, Vec<_>) =
            properties
                .into_iter()
                .enumerate()
                .partition(|(_, property)| {
                    !self.reads_long(property, values.get(&property.name).unwrap_or(&NULL))
                });
        // Pulled out by the rows' hover inset, as a relationship group's rows are.
        v_flex()
            .mx(px(-space::XS))
            .gap(px(space::XXS))
            .children(
                rows.into_iter()
                    .map(|(index, property)| self.render_property(index, property, window, cx)),
            )
            .children(
                texts
                    .into_iter()
                    .map(|(index, property)| self.render_long_text(index, property, window, cx)),
            )
            .into_any_element()
    }

    /// Whether a property's value is shown as a text under its name rather than boxed in a
    /// row: a text with markup, or one longer than a box holds and with words to wrap at (an
    /// email or an id stays in its box, cut at its end).
    fn reads_long(&self, property: &gq_language::Property, value: &Value) -> bool {
        reads_as_markdown(property)
            && (self.markdown.contains_key(&property.name)
                || value.as_str().is_some_and(|text| {
                    text.contains(char::is_whitespace) && text.chars().count() > BOXED_TEXT
                }))
    }

    /// What a property's row or text shows of its state, and what a click on it does.
    fn property_state(
        &self,
        index: usize,
        property: &gq_language::Property,
        cx: &mut Context<Self>,
    ) -> PropertyState {
        let name = property.name.as_str();
        let value = self
            .values
            .loaded()
            .and_then(|values| values.get(name))
            .unwrap_or(&NULL);
        let ty = property.display_type();
        let editable = self.edits_property(property, cx);
        // A value the credential may not change says so where it would edit.
        let refused = self
            .change_refusal(cx)
            .filter(|_| property.editable_in_cell() && !editable);
        let editing = self.edits.contains_key(name);
        let saving = self.saving.on(&self.branch, name) || self.saving_elsewhere.contains(name);
        // An unsaved value shows what was typed, with a dot, and opens on a click.
        let draft = (editable && !editing && !saving)
            .then(|| self.draft_of(name, cx))
            .flatten();
        // An enum is a menu of its values and a boolean a switch, each saving as it is chosen;
        // other values open an editor on a click.
        let picker = (editable && !editing && !saving && draft.is_none())
            .then(|| self.render_picker(index, property, value, cx))
            .flatten();
        let opens_editor = editable && picker.is_none() && !editing && !saving;
        // A boxed value may be cut at its box's end: the tooltip has it whole.
        let whole = (!value.is_null()).then(|| display_value(Some(property), value));
        let tooltip = (!editing).then(|| {
            let what = if draft.is_some() {
                format!(
                    "Not saved; it holds “{}”. Click to edit; ⌘S saves this node's changes together",
                    display_value(Some(property), value)
                )
            } else if opens_editor {
                format!("{ty} · click to edit")
            } else if editable {
                ty.clone()
            } else if let Some(reason) = &refused {
                format!("{ty} · {reason} · click to copy")
            } else {
                format!("{ty} · click to copy")
            };
            match &whole {
                Some(whole) if draft.is_none() => format!("{whole}\n{what}"),
                _ => what,
            }
        });
        PropertyState {
            editable,
            editing,
            saving,
            draft,
            picker,
            opens_editor,
            tooltip,
            copied: self.copied.as_deref() == Some(name),
        }
    }

    /// A property's row: its kind and its name, then its value as [`Self::property_state`]
    /// says, and its state (saving, copied) or its "Open in the editor" after it.
    fn render_property(
        &self,
        index: usize,
        property: &gq_language::Property,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name = property.name.as_str();
        let value = self
            .values
            .loaded()
            .and_then(|values| values.get(name))
            .unwrap_or(&NULL);
        let mut state = self.property_state(index, property, cx);
        let body = match self.edits.get(name) {
            _ if let Some(then) = self.absent.get(name) => {
                studio_ui::ds::text::muted(absence_text(then, true)).into_any_element()
            }
            Some(edit) => self.render_field_edit(edit, window, cx),
            None if let Some(draft) = &state.draft => {
                let text = draft.text.as_str();
                h_flex()
                    .gap(px(space::SM))
                    .min_w_0()
                    .child(studio_ui::ds::value_text(
                        if text.trim().is_empty() {
                            "—".to_owned()
                        } else {
                            studio_ui::text::first_line(text)
                        },
                        cx,
                    ))
                    .child(studio_ui::ds::dot(Color::Accent))
                    .into_any_element()
            }
            None => match state.picker.take() {
                Some(picker) => picker,
                None => self.render_value(Some(property), value, cx),
            },
        };
        let group = SharedString::from(format!("peek-property-{index}"));
        let editing = state.editing;
        let title = match name {
            FROM_COLUMN => "from".to_owned(),
            TO_COLUMN => "to".to_owned(),
            _ => name.to_owned(),
        };
        // A field being edited is a control's height: centred in the row as a value is.
        let value_slot = studio_ui::ds::property_value()
            .when(editing, |slot| {
                slot.py(px((studio_ui::ds::size::PROPERTY_ROW
                    - studio_ui::ds::size::CONTROL)
                    / 2.))
            })
            .child(body);
        let row = studio_ui::ds::property_row(property_icon(property), title, self.label_width())
            .id(("peek-property", index))
            .group(group.clone())
            .px(px(space::XS))
            .rounded(px(studio_ui::ds::radii(cx).sm))
            .child(value_slot)
            .children(self.property_beside(index, property, &state, group, cx));
        self.property_interaction(row, property, &state, cx)
            .into_any_element()
    }

    /// A text longer than a box holds (spec 014): its kind and its name, which fold it, and
    /// the text under them at the full width, as markdown when it has markup; a click on the
    /// text edits or copies it as a row's does.
    fn render_long_text(
        &self,
        index: usize,
        property: &gq_language::Property,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name = property.name.as_str();
        let value = self
            .values
            .loaded()
            .and_then(|values| values.get(name))
            .unwrap_or(&NULL);
        let state = self.property_state(index, property, cx);
        let open = !self.folded.contains(name) || state.editing;
        let fold_name = name.to_owned();
        let body = match self.edits.get(name) {
            _ if let Some(then) = self.absent.get(name) => {
                studio_ui::ds::text::muted(absence_text(then, true)).into_any_element()
            }
            Some(edit) => self.render_field_edit(edit, window, cx),
            None if let Some(draft) = &state.draft => h_flex()
                .gap(px(space::SM))
                .items_start()
                .child(studio_ui::ds::text::body(draft.text.clone()))
                .child(studio_ui::ds::dot(Color::Accent))
                .into_any_element(),
            None => match self.markdown.get(name) {
                // GPUI's text takes its size from the element around it, so the labels' size
                // is set there too.
                Some((_, view)) => div()
                    .text_size(studio_ui::ds::text::body_size(cx))
                    .child(MarkdownElement::new(view.clone(), value_style(window, cx)))
                    .into_any_element(),
                None => studio_ui::ds::text::body(display_value(Some(property), value))
                    .into_any_element(),
            },
        };
        let group = SharedString::from(format!("peek-property-{index}"));
        // Folded, the text's draft shows as its dot beside the name.
        let folded_draft = !open && state.draft.is_some();
        let beside = self.property_beside(index, property, &state, group.clone(), cx);
        let head_selector = format!("peek-head-{name}");
        let draft_selector = format!("peek-folded-draft-{name}");
        let head = h_flex()
            .debug_selector(move || head_selector.clone())
            .h(px(studio_ui::ds::size::PROPERTY_ROW))
            .gap(px(space::SM))
            .items_center()
            .child(studio_ui::ds::icon::meta(property_icon(property)))
            .child(
                studio_ui::ds::fold(("peek-fold", index), name.to_owned(), open)
                    .tip(studio_ui::ds::tooltip(if open { "Fold" } else { "Unfold" }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        if !this.folded.remove(&fold_name) {
                            this.folded.insert(fold_name.clone());
                        }
                        cx.notify();
                    })),
            )
            .when(folded_draft, |head| {
                head.child(
                    div()
                        .debug_selector(move || draft_selector.clone())
                        .child(studio_ui::ds::dot(Color::Accent)),
                )
            })
            .child(div().flex_1())
            .children(beside);
        let text = v_flex()
            .id(("peek-property", index))
            .group(group.clone())
            .w_full()
            .px(px(space::XS))
            .pb(px(space::XS))
            .rounded(px(studio_ui::ds::radii(cx).sm))
            .child(head)
            .when(open, |text| text.child(div().w_full().child(body)));
        self.property_interaction(text, property, &state, cx)
            .into_any_element()
    }

    /// What a property's row or text does: the hover fill, the tooltip, and a click that edits
    /// or copies.
    fn property_interaction(
        &self,
        element: gpui::Stateful<Div>,
        property: &gq_language::Property,
        state: &PropertyState,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let hover = studio_ui::ds::surface::hover(cx);
        // Only a click's own: the copy reads the value as it is then.
        let edit_property = state.opens_editor.then(|| property.clone());
        let copy_name = property.name.clone();
        let editable = state.editable;
        element
            .when(!state.editing, |row| row.hover(move |s| s.bg(hover)))
            .when(state.opens_editor || !editable, |row| row.cursor_pointer())
            .when_some(state.tooltip.clone(), |row, tooltip| {
                row.tip(studio_ui::ds::tooltip(tooltip))
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                if let Some(property) = edit_property.clone() {
                    this.start_edit(property, window, cx);
                } else if !editable {
                    this.copy(copy_name.clone(), cx)
                }
            }))
    }

    /// What shows beside a property's value, level with its first line, in its row (a long
    /// text's head): its "Open in the editor" under the pointer, its saving dot, "Copied".
    fn property_beside(
        &self,
        index: usize,
        property: &gq_language::Property,
        state: &PropertyState,
        group: SharedString,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        // Text opens in the editor from a button the row shows on hover.
        let expands = state.opens_editor && reads_as_markdown(property);
        let expand_name = property.name.clone();
        let open_selector = format!("peek-open-editor-{}", property.name);
        let beside = || {
            div()
                .h(px(studio_ui::ds::size::PROPERTY_ROW))
                .flex()
                .items_center()
        };
        let mut items = Vec::new();
        if expands {
            items.push(
                beside()
                    .debug_selector(move || open_selector.clone())
                    .child(
                        studio_ui::ds::row_icon_button(
                            ("peek-open-editor", index),
                            IconName::Maximize,
                        )
                        .icon_color(Color::Muted)
                        .visible_on_hover(group.clone())
                        .tip(studio_ui::ds::tooltip("Open in the editor"))
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.expand(&expand_name, window, cx)
                            },
                        )),
                    )
                    .into_any_element(),
            );
        }
        if state.saving {
            items.push(
                beside()
                    .child(
                        div()
                            .id(("peek-saving", index))
                            .tip(studio_ui::ds::tooltip("Saving…"))
                            .child(studio_ui::ds::dot(Color::Warning)),
                    )
                    .into_any_element(),
            );
        }
        if state.copied {
            items.push(
                beside()
                    .child(studio_ui::ds::text::meta("Copied").color(Color::Success))
                    .into_any_element(),
            );
        }
        items
    }

    /// A short value boxed: a list's items each in their box, none a muted dash.
    fn render_value(
        &self,
        property: Option<&gq_language::Property>,
        value: &Value,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match value {
            Value::Null => studio_ui::ds::text::body("—")
                .color(Color::Disabled)
                .into_any_element(),
            Value::Array(items) => studio_ui::ds::value_list()
                .children(
                    items
                        .iter()
                        .map(|item| studio_ui::ds::value_text(studio_ui::format_value(item), cx)),
                )
                .into_any_element(),
            _ => studio_ui::ds::value_text(display_value(property, value), cx).into_any_element(),
        }
    }

    /// The label column of a property row: a page's is wider, at a document's width.
    fn label_width(&self) -> f32 {
        if self.page {
            studio_ui::ds::size::FIELD_LABEL_WIDE
        } else {
            studio_ui::ds::size::FIELD_LABEL
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
            // A select, raised as the values are boxed, on their edge.
            return Some(
                div()
                    .child(
                        PopoverMenu::new(SharedString::from(format!("peek-enum-{index}")))
                            .trigger(studio_ui::ds::value_select(
                                SharedString::from(format!("peek-enum-button-{index}")),
                                shown,
                            ))
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
            .tip(studio_ui::ds::tooltip("Set to none"))
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
                        .tip_fn(|_, cx| {
                            studio_ui::ds::tooltip_for_action_view(
                                "Open in the editor",
                                &ExpandField,
                                cx,
                            )
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
                    .child(studio_ui::ds::dot(notice.tone.color()))
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
        let folded = (0..self.groups.len())
            .filter(|&index| self.place(index) == Place::Folded)
            .count();
        let open = self.show_empty;
        studio_ui::ds::fold_row("peek-empty-groups", "Empty", open, cx)
            .tip(studio_ui::ds::tooltip(if open {
                "Hide the relationships with none"
            } else {
                "Show the relationships with none, to add one"
            }))
            .on_click(cx.listener(|this, _, _, cx| {
                this.show_empty = !this.show_empty;
                cx.notify();
            }))
            .child(studio_ui::ds::text::meta(studio_ui::group_count(
                folded as i64,
            )))
            .into_any_element()
    }

    /// One relationship group: a fold row (its name, the way it points and the other type,
    /// its count, its "+") over its neighbours, one row each; an open search and its notice
    /// under the header. Unlike a property's values, neighbours are rows: each is a node to
    /// go to, and a group may hold many.
    fn render_group(&self, index: usize, cx: &mut Context<Self>) -> Div {
        let group = &self.groups[index];
        let arrow = match group.group.direction {
            Direction::Outgoing => "→",
            Direction::Incoming => "←",
        };
        let title = format!(
            "{} {arrow} {}",
            gq_language::traversal_name(&group.group.edge),
            group.group.other_type
        );
        // Until its count is read a group says nothing about what it holds.
        let count = match &group.count {
            Loadable::Loaded(n) => studio_ui::group_count(*n),
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
        let header = studio_ui::ds::fold_row(("peek-group", index), title, group.expanded, cx)
            .group(header_group)
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_group(index, cx)))
            .when_some(failed, |d, error| d.tip(studio_ui::ds::tooltip(error)))
            .when(self.edge_writes.contains(&index), |header| {
                header.child(studio_ui::ds::dot(Color::Warning))
            })
            .children(add)
            .child(studio_ui::ds::text::meta(count));
        let adding = self
            .adding
            .as_ref()
            .filter(|adding| adding.index == index)
            .map(|adding| self.render_add_row(adding, cx));
        // An unknown outcome offers History, where it is settled.
        let notice = self.group_notices.get(&index).map(|notice| {
            div().pl(px(space::XS)).child(blob_write::notice_row(
                format!("peek-group-notice-{index}"),
                notice,
                None,
            ))
        });
        let body = group
            .expanded
            .then(|| self.render_neighbours(index, group, cx));
        v_flex()
            .child(header)
            .children(adding)
            .children(notice)
            .children(body)
            // Room under an open group's rows before the next header.
            .when(group.expanded, |group| group.pb(px(space::XS)))
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
        let refused = self
            .change_refusal(cx)
            .or_else(|| self.edge_write_refusal(index))
            .or_else(|| self.replace_refusal(index));
        let tooltip = if let Some(reason) = refused.clone() {
            reason
        } else if !required.is_empty() {
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
        // In the group's header, quiet; in the peek, shown under the pointer.
        studio_ui::ds::row_icon_button(("peek-add", index), IconName::Plus)
            .icon_color(Color::Muted)
            .when(!self.page, |button| button.visible_on_hover(header))
            .disabled(refused.is_some())
            .tip(studio_ui::ds::tooltip(tooltip))
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
                            filters: None,
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
        let branch = self.session.read(cx).branch.clone();
        let replaces = self.holds_one(index)
            && matches!(self.groups[index].count, Loadable::Loaded(n) if n > 0);
        let found = match &adding.found {
            Loadable::Loaded(()) if adding.offers.is_empty() => {
                studio_ui::ds::text::meta("No match").into_any_element()
            }
            Loadable::Loaded(()) => {
                let pick = cx.listener(move |this, picked: &usize, window, cx| {
                    this.add_offered(*picked, window, cx)
                });
                studio_ui::ds::row_list(cx)
                    .id(("peek-found", index))
                    .children(adding.offers.rows(
                        "peek-found-node",
                        Rc::new(move |picked, window, cx| pick(&picked, window, cx)),
                        cx,
                    ))
                    .into_any_element()
            }
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
            .pl(px(space::XS))
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

    /// A neighbour's ×, at its chip's end, muted as the reference card's trailing marks are:
    /// the first press asks, the second removes the relationship (the node stays). Always
    /// shown: a chip cannot keep room for a button it shows only under the pointer without
    /// looking padded.
    fn render_remove(
        &self,
        index: usize,
        n: usize,
        edge_id: String,
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
            .disabled(self.edge_write_refusal(index).is_some())
            .tip(studio_ui::ds::tooltip(format!(
                "Remove this {edge}; both nodes stay"
            )))
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.press_remove(index, edge_id.clone(), window, cx)
            }))
            .into_any_element();
        }
        let refused = self
            .change_refusal(cx)
            .or_else(|| self.edge_write_refusal(index));
        studio_ui::ds::value_remove(SharedString::from(format!("peek-remove-{index}-{n}")))
            .disabled(refused.is_some())
            .tip(studio_ui::ds::tooltip(
                refused.unwrap_or_else(|| format!("Remove this {edge}")),
            ))
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.press_remove(index, edge_id.clone(), window, cx)
            }))
            .into_any_element()
    }

    /// An open group's neighbours, a row each: its type's mark and its name; a click peeks it,
    /// ⌘-click opens its page, and its × (under the pointer, held once pressed) removes the
    /// relationship. Past the ones read, "+N more".
    fn render_neighbours(&self, index: usize, group: &Group, cx: &mut Context<Self>) -> AnyElement {
        match &group.neighbours {
            Loadable::Loaded(neighbours) if neighbours.is_empty() => {
                group_note(studio_ui::ds::text::meta("None at this commit"))
            }
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
                    let label = format!("+{} more", studio_ui::group_count(more));
                    let row = studio_ui::ds::SidebarRow::new(
                        SharedString::from(format!("peek-more-{index}")),
                        if self.page && group.more.is_some() {
                            "Reading…".to_owned()
                        } else {
                            label
                        },
                    )
                    .flush()
                    .muted(true)
                    .icon(IconName::Ellipsis);
                    if self.page {
                        row.on_click(cx.listener(move |this, _, _, cx| this.show_more(index, cx)))
                    } else {
                        // The peek shows the first few; the page lists them all.
                        let open = self.open_page_action();
                        row.end(studio_ui::ds::text::meta("Open page"))
                            .tip(studio_ui::ds::tooltip(
                                "Open the node's page to see them all",
                            ))
                            .on_click(move |_, window, cx| {
                                if let Some(open) = open.clone() {
                                    window.dispatch_action(Box::new(open), cx)
                                }
                            })
                    }
                });
                let removes = self.edits_relationships();
                v_flex()
                    .children(neighbours.iter().enumerate().map(|(n, neighbour)| {
                        let (peek, page) = node_actions(&other, &neighbour.id, at_commit.clone());
                        let label = neighbour
                            .label
                            .clone()
                            .unwrap_or_else(|| neighbour.id.clone());
                        let row = studio_ui::ds::SidebarRow::new(
                            SharedString::from(format!("peek-neighbour-{index}-{n}")),
                            label.clone(),
                        )
                        .flush()
                        .start(studio_ui::type_mark(&other, &label, cx))
                        .tip(studio_ui::ds::tooltip(format!(
                            "Peek {} · ⌘-click opens its page",
                            neighbour.id
                        )))
                        .on_click(move |event, window, cx| {
                            if event.modifiers().platform {
                                window.dispatch_action(Box::new(page.clone()), cx)
                            } else {
                                window.dispatch_action(Box::new(peek.clone()), cx)
                            }
                        });
                        match neighbour.edge_id.clone().filter(|_| removes) {
                            // Pressed once, "Remove" stays until it is pressed again.
                            Some(edge_id) if self.removing.as_deref() == Some(edge_id.as_str()) => {
                                row.end(self.render_remove(index, n, edge_id, cx))
                            }
                            Some(edge_id) => row.actions(self.render_remove(index, n, edge_id, cx)),
                            None => row,
                        }
                    }))
                    .children(show_more)
                    .into_any_element()
            }
            Loadable::Failed(error) => group_note(studio_ui::ds::text::error_note(error.clone())),
            _ => group_note(studio_ui::ds::text::meta("Reading…")),
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
        let label_width = self.label_width();
        let endpoint = |label: &'static str, icon: IconName, ty: &String, key: &str| {
            let id = values.get(key).and_then(Value::as_str)?.to_owned();
            let (peek, page) = node_actions(ty, &id, at_commit.clone());
            Some(
                studio_ui::ds::property_row(icon, label, label_width)
                    .px(px(space::XS))
                    .child(
                        studio_ui::ds::property_value().child(
                            studio_ui::ds::node_value(ty, id.clone(), cx)
                                .id(label)
                                .hover(move |s| s.bg(hover))
                                .cursor_pointer()
                                .tip(studio_ui::ds::tooltip(format!(
                                    "Peek {id} · {ty} · ⌘-click opens its page"
                                )))
                                .on_click(move |event, window, cx| {
                                    if event.modifiers().platform {
                                        window.dispatch_action(Box::new(page.clone()), cx)
                                    } else {
                                        window.dispatch_action(Box::new(peek.clone()), cx)
                                    }
                                }),
                        ),
                    ),
            )
        };
        Some(
            v_flex()
                .gap(px(space::XXS))
                .children(endpoint(
                    "from",
                    IconName::ArrowLeft,
                    from_type,
                    FROM_COLUMN,
                ))
                .children(endpoint("to", IconName::ArrowRight, to_type, TO_COLUMN))
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

/// How relationship write `step` to `branch` ended, by the kind of commit its answer names as
/// the session logs it ([`OutcomeKind::written`]): only a published commit moved the branch. A
/// proposal or a snapshot moved none, and of a kind this build does not know, whether one moved
/// is not known; neither is told as written, nor is an answer lost.
fn edge_written(outcome: &WriteOutcome<ChangeOutput>, branch: &str, step: EdgeStep) -> EdgeWritten {
    let (doing, done, leaves) = (step.doing(), step.done(), step.leaves());
    if let WriteOutcome::Committed(output) = outcome
        && let Some(commit) = &output.commit
    {
        let kind = OutcomeKind::written(Some((commit.graph_commit_id.clone(), commit.kind)));
        let held = match kind {
            OutcomeKind::Held {
                commit,
                held: HeldAs::Proposal,
            } => Some(format!(
                "{doing} was held as commit {}, on no branch until it is published to {branch}: \
                 {branch} did not change{leaves}.",
                studio_ui::short_commit_id(&commit)
            )),
            OutcomeKind::Held {
                commit,
                held: HeldAs::Snapshot,
            } => Some(format!(
                "{doing} was saved as snapshot {}, on no branch: {branch} did not change{leaves}.",
                studio_ui::short_commit_id(&commit)
            )),
            OutcomeKind::Unknown { reason } => {
                let message = format!("{reason}{leaves}. Check History before trying again.");
                return EdgeWritten::Unsettled(Notice::new(Tone::Unknown, message));
            }
            OutcomeKind::Committed { .. } | OutcomeKind::Refused { .. } => None,
        };
        if let Some(message) = held {
            return EdgeWritten::Unsettled(Notice::new(Tone::Conflict, message));
        }
    }
    match save_result(outcome, branch) {
        SaveResult::Saved { commit, parent } => EdgeWritten::Saved { commit, parent },
        SaveResult::NoMatch => EdgeWritten::NotWritten(Notice::new(
            Tone::Conflict,
            "Nothing changed: it was already so, or changed elsewhere",
        )),
        SaveResult::Refused(notice) => EdgeWritten::NotWritten(notice),
        // A relationship keeps no draft: what may have landed is in the branch's history.
        SaveResult::Unknown(_) => EdgeWritten::Unsettled(Notice::new(
            Tone::Unknown,
            format!(
                "The server's answer was lost, so whether {done} is not known{leaves}. Check \
                 History before trying again."
            ),
        )),
    }
}

/// Reads a group's first edges (`reads`) at `snapshot` through `scope`: one more than the view
/// shows, which when they fit is also the group's count of edges; past them, its edges counted
/// at the same commit, a count that comes back without its number failing rather than reading
/// as none. `None` once the graph is gone; `Err` when the first read failed.
async fn read_first_edges(
    session: &WeakEntity<GraphSession>,
    scope: Option<&ReadScope>,
    reads: EdgeReads,
    snapshot: Option<String>,
    cx: &mut AsyncApp,
) -> Option<Result<FirstEdges, ReadError>> {
    let read = session
        .update(cx, |session, cx| {
            session.read_scoped(scope, &reads.first, snapshot.clone(), cx)
        })
        .ok()?;
    let mut shown = match read.await {
        Ok(output) => neighbours_from(&output.rows),
        Err(error) => return Some(Err(error)),
    };
    let limit = reads.limit as usize;
    if shown.len() <= limit {
        let count = Loadable::Loaded(shown.len() as i64);
        return Some(Ok(FirstEdges { shown, count }));
    }
    shown.truncate(limit);
    let count = match reads.count {
        Ok(query) => {
            let read = session
                .update(cx, |session, cx| {
                    session.read_scoped(scope, &query, snapshot, cx)
                })
                .ok()?;
            match read.await {
                Ok(output) => match builder::count_of(&output.rows) {
                    Some(n) => Loadable::Loaded(n),
                    None => Loadable::Failed("The count came back empty".into()),
                },
                Err(error) => Loadable::Failed(error.to_string()),
            }
        }
        Err(error) => Loadable::Failed(error),
    };
    Some(Ok(FirstEdges { shown, count }))
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

/// A line under a relationship group in place of its rows (none, reading, a failure), on the
/// rows' labels' edge.
fn group_note(note: Label) -> AnyElement {
    div()
        .min_h(px(studio_ui::ds::size::SIDEBAR_ROW))
        .pl(px(space::XS + studio_ui::ds::size::ROW_ICON + space::MD))
        .flex()
        .items_center()
        .child(note)
        .into_any_element()
}

/// The properties the view is writing, each under the branch its write goes to. A write still
/// out on a branch the view left shows again, and holds a second write of its property back,
/// when the view comes back to that branch; the branch shown sees only its own.
#[derive(Default)]
struct Writing(HashMap<String, HashSet<String>>);

impl Writing {
    fn insert(&mut self, branch: &str, property: &str) {
        self.0
            .entry(branch.to_owned())
            .or_default()
            .insert(property.to_owned());
    }

    fn remove(&mut self, branch: &str, property: &str) {
        if let Some(properties) = self.0.get_mut(branch) {
            properties.remove(property);
            if properties.is_empty() {
                self.0.remove(branch);
            }
        }
    }

    /// Whether `property` is being written on `branch`.
    fn on(&self, branch: &str, property: &str) -> bool {
        self.0
            .get(branch)
            .is_some_and(|properties| properties.contains(property))
    }

    /// Whether anything is being written on `branch`.
    fn any_on(&self, branch: &str) -> bool {
        self.0.contains_key(branch)
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

fn section(title: &'static str) -> impl IntoElement {
    div()
        .pt(px(space::MD))
        .pb(px(space::XS))
        .child(studio_ui::ds::text::meta(title))
}

impl Focusable for NodePeek {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for NodePeek {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.node_owed) {
            cx.defer_in(window, |this, _, cx| this.load_node(cx));
        }
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
        // those the last frame laid out are measured again, the others keep the height they
        // had. This frame's are noted as they are laid out.
        let count = self.list_items.len();
        if let Some(drawn) = self.items_drawn.take() {
            let drawn = drawn.start.min(count)..drawn.end.min(count);
            if !drawn.is_empty() {
                self.list.remeasure_items(drawn);
            }
        }
        // Saves the credential may not make on the branch are not registered (constitution
        // VIII); a field still open is kept as a draft when it is left.
        let writable = self.change_refusal(cx).is_none();
        div()
            .id("node-peek")
            .key_context("NodeView")
            .track_focus(&self.focus_handle)
            .when(writable, |view| {
                view.on_action(cx.listener(Self::save_drafts))
                    .on_action(cx.listener(Self::on_save_field))
            })
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
        self.items_drawn = Some(match self.items_drawn.take() {
            Some(drawn) => drawn.start.min(ix)..drawn.end.max(ix + 1),
            None => ix..ix + 1,
        });
        let item = match self.list_items.get(ix).copied() {
            Some(Item::Head) => {
                let head = self.render_head(window, cx);
                let head = if page {
                    head.pt(px(space::XL)).gap(px(space::MD))
                } else {
                    head.pt(px(space::XL)).gap(px(space::SM))
                };
                // A hairline between the node's own values and its relationships, as the
                // reference card rules its details from its participants.
                head.when(self.shows_relationships(), |head| {
                    head.child(h_flex().pt(px(space::MD)).child(studio_ui::ds::rule_h(cx)))
                        .child(section("Relationships"))
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
        let at_commit = self.held_commit();
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
                        studio_ui::ds::icon_button("peek-open-page", IconName::ArrowUpRight)
                            .icon_color(Color::Muted)
                            .tip(studio_ui::ds::tooltip("Open the node's page"))
                            .on_click(move |_, window, cx| {
                                window.dispatch_action(Box::new(open.clone()), cx)
                            })
                    }))
                    .when(is_node, |header| {
                        let explore = studio_actions::graph::ShowNodeOnCanvas {
                            type_name: type_name.clone(),
                            id: id.clone(),
                            at_commit: at_commit.clone(),
                            preview: true,
                        };
                        header.child(
                            studio_ui::ds::button("peek-canvas", "Canvas")
                                .style(ButtonStyle::Subtle)
                                .start_icon(
                                    Icon::new(IconName::Crosshair),
                                )
                                .tip(studio_ui::ds::tooltip("Walk the graph from this node"))
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
                    .gap_x(px(space::LG))
                    .gap_y(px(space::XS))
                    .child(studio_ui::ds::meta_code(IconName::Hash, id, cx))
                    .children(at.map(|commit| {
                        div()
                            .id("peek-commit")
                            .tip(studio_ui::ds::tooltip(format!("Read at commit {commit}")))
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
                                .tip(studio_ui::ds::tooltip(
                                    "The branch moved since this was read. Refresh reads it at the head.",
                                ))
                                .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                        )
                    }),
            )
            .children(drafts_bar)
            // An edge's endpoints and every property are rows, labelled; they need no heading.
            .children(endpoints.map(|endpoints| div().mx(px(-space::XS)).child(endpoints)))
            .when(has_properties, |d| d.child(properties))
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

/// What a property's row or text shows of its state, and what a click on it does
/// (`NodePeek::property_state`).
struct PropertyState {
    editable: bool,
    editing: bool,
    saving: bool,
    draft: Option<DraftEntry>,
    /// An enum's select or a boolean's switch, which writes as it is chosen.
    picker: Option<AnyElement>,
    opens_editor: bool,
    tooltip: Option<String>,
    copied: bool,
}

/// The icon of a property's kind, as its column's header and its row show it: an enum's
/// values, text, a truth value, a number, a time, a vector or a Blob.
pub(crate) fn property_icon(property: &gq_language::Property) -> IconName {
    use gq_language::ScalarKind;
    if property.enum_values.is_some() {
        return IconName::ListTodo;
    }
    match property.kind {
        ScalarKind::Text => IconName::CaseSensitive,
        ScalarKind::Bool => IconName::Check,
        ScalarKind::Integer | ScalarKind::Float => IconName::Hash,
        ScalarKind::Date | ScalarKind::DateTime => IconName::Clock,
        ScalarKind::Vector => IconName::Sparkle,
        ScalarKind::Blob => IconName::File,
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
impl NodePeek {
    /// What the view shows of property `name`, and whether it shows it saving: for tests of the
    /// views that open a peek.
    pub(crate) fn shown(&self, name: &str) -> (Value, bool) {
        let saving = self.saving.on(&self.branch, name) || self.saving_elsewhere.contains(name);
        (self.value_of(name), saving)
    }

    /// The commit the view reads.
    pub(crate) fn reads_at(&self) -> Option<&str> {
        self.snapshot.as_deref()
    }

    /// The size of the managed value slot `property` shows, once its value is in.
    pub(crate) fn blob_size(&self, property: &str) -> Option<u64> {
        let slot = self.blobs.iter().find(|slot| slot.property == property)?;
        match slot.meta.loaded()? {
            BlobMeta::Managed { size, .. } => Some(*size),
            _ => None,
        }
    }

    /// The commit the Blob values the view shows hold at ([`BlobsAt`]).
    pub(crate) fn blobs_held_at(&self) -> Option<&str> {
        self.blobs_at.commit(self.snapshot.as_deref())
    }
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

    /// The add search as it is once its read found the nodes `ids`, the first highlighted.
    fn found(adding: &mut AddEdge, ids: &[&str]) {
        let offers = ids
            .iter()
            .map(|id| crate::suggest::Suggestion {
                value: (*id).to_owned(),
                label: None,
            })
            .collect();
        adding.offers.set(offers, true);
        adding.found = Loadable::Loaded(());
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

    /// A text longer than its box holds, with words to wrap at, is shown under its name at the
    /// full width; a short one, or one without words to wrap at (an email, an id), stays boxed
    /// and is cut at its box's end (spec 014).
    #[gpui::test]
    async fn a_long_text_leaves_its_box_and_a_short_one_stays(cx: &mut TestAppContext) {
        let (view, cx) = page(session(PEOPLE, cx), cx);
        view.read_with(cx, |view, _| {
            let name = view.property_named("name").unwrap();
            assert!(!view.reads_long(name, &json!("Ada Lovelace")));
            assert!(
                !view.reads_long(
                    name,
                    &json!("ada.lovelace.countess.of.lovelace@analytical-engine.org")
                ),
                "no words to wrap at"
            );
            assert!(view.reads_long(
                name,
                &json!("Wrote the first published algorithm meant for a machine")
            ));
        });
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

    /// A long text's controls sit in its head row, beside its name: "Open in the editor" adds
    /// no line under the text, and a folded text shows its draft's dot there (spec 014).
    #[gpui::test]
    async fn a_long_texts_controls_and_folded_draft_sit_in_its_head(cx: &mut TestAppContext) {
        let session = session(PEOPLE, cx);
        let (view, cx) = page(session.clone(), cx);
        let long = "Wrote the first published algorithm meant for a machine, and notes on it";
        view.update(cx, |view, cx| {
            if let Loadable::Loaded(values) = &mut view.values {
                values.insert("name".into(), json!(long));
            }
            cx.notify();
        });
        cx.run_until_parked();
        let head = cx
            .debug_bounds("peek-head-name")
            .expect("a long text's head");
        let open = cx
            .debug_bounds("peek-open-editor-name")
            .expect("Open in the editor");
        assert!(
            open.top() >= head.top() && open.bottom() <= head.bottom(),
            "Open in the editor in the head, not under the text: {open:?} in {head:?}"
        );
        assert!(cx.debug_bounds("peek-folded-draft-name").is_none());

        view.update(cx, |view, cx| {
            view.folded.insert("name".into());
            cx.notify();
        });
        session.update(cx, |session, cx| {
            let key = DraftKey {
                type_name: "Person".into(),
                id: "p1".into(),
                property: "name".into(),
            };
            let draft = DraftEntry {
                old: json!(long),
                text: "Ada".into(),
            };
            session.set_draft(key, draft, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("peek-folded-draft-name").is_some(),
            "a folded text's draft shows its dot"
        );
    }

    /// A node page shown with no field open takes ⌘S: the page's keyboard is its view's, so the
    /// key reaches the view's drafts. Before, the page held a focus of its own outside the view,
    /// and ⌘S saved nothing until a field inside was being edited.
    #[gpui::test]
    async fn a_page_with_no_field_open_saves_its_drafts_with_cmd_s(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
            cx.bind_keys([gpui::KeyBinding::new(
                "cmd-s",
                SaveNodeDrafts,
                Some("NodeView"),
            )]);
        });
        let session = session(PEOPLE, cx);
        cx.run_until_parked();
        let (page, cx) = cx.add_window_view({
            let session = session.clone();
            move |_, cx| {
                crate::node_page::NodePage::new(
                    session,
                    "Person".into(),
                    "p1".into(),
                    None,
                    None,
                    cx,
                )
            }
        });
        cx.run_until_parked();
        let key = DraftKey {
            type_name: "Person".into(),
            id: "p1".into(),
            property: "name".into(),
        };
        session.update(cx, |session, cx| {
            let draft = DraftEntry {
                old: json!("Ada"),
                text: "Ada King".into(),
            };
            session.set_draft(key.clone(), draft, cx);
        });
        cx.update(|window, cx| {
            let focus = page.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("cmd-s");
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            assert!(session.draft(&key).is_none(), "saved, and let go")
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

    /// A page that follows the window to another branch keeps each open field as a draft on
    /// the branch it was typed on: one being typed, and one a refused save opened again, whose
    /// text nothing else holds. The new branch gets none of it.
    #[gpui::test]
    async fn open_fields_stay_drafts_on_the_branch_a_page_leaves(cx: &mut TestAppContext) {
        const TITLED: &str =
            "node Person {\n  slug: String @key\n  name: String\n  title: String?\n}\n";
        let refused = json!({ "error": "policy denied", "code": "forbidden", "disposition": { "action": "refresh", "effects": "none" } });
        let session = session_answering(TITLED, 403, refused, cx);
        let (view, cx) = page(session.clone(), cx);
        edit_name(&view, "Ada Lovelace", cx);
        view.update_in(cx, |view, window, cx| {
            assert!(view.edits["name"].notice.is_some(), "refused, open again");
            let title = view.property_named("title").cloned().unwrap();
            view.start_edit(title, window, cx);
            let editor = view.edits["title"].editor.clone();
            editor.update(cx, |editor, cx| editor.set_text("Countess", window, cx));
        });
        session.update(cx, |session, cx| session.set_branch("fix".into(), cx));
        cx.run_until_parked();
        let key = |property: &str| DraftKey {
            type_name: "Person".into(),
            id: "p1".into(),
            property: property.into(),
        };
        session.read_with(cx, |session, _| {
            let entry = |old: Value, text: &str| DraftEntry {
                old,
                text: text.into(),
            };
            let mut on_main: Vec<_> = session.drafts_on("main").collect();
            on_main.sort_by_key(|(key, _)| key.property.clone());
            assert_eq!(
                on_main,
                vec![
                    (&key("name"), &entry(json!("Ada"), "Ada Lovelace")),
                    (&key("title"), &entry(Value::Null, "Countess")),
                ]
            );
            assert_eq!(session.drafts().count(), 0, "nothing on fix");
        });
        view.read_with(cx, |view, _| {
            assert_eq!(view.branch, "fix");
            assert!(view.edits.is_empty());
        });
    }

    /// A slot's Open shows the value in a tab read at the view's commit: one the view holds
    /// opens held, read-only; one at the branch head opens writable, on the view's branch.
    #[gpui::test]
    async fn a_slot_opens_its_tab_held_only_where_the_view_holds_its_commit(
        cx: &mut TestAppContext,
    ) {
        let (view, _session, _blobs, cx) = page_with_doc(Some(b"old"), None, cx);
        view.read_with(cx, |view, _| {
            let open = view.open_blob_action("doc");
            assert_eq!(
                open.at_commit.as_deref(),
                Some("c1"),
                "read where the slot was"
            );
            assert!(!open.held, "a view at the head writes on its branch");
        });
        view.update(cx, |view, _| view.hold_commit());
        view.read_with(cx, |view, _| {
            let open = view.open_blob_action("doc");
            assert_eq!(open.at_commit.as_deref(), Some("c1"));
            assert!(open.held);
        });
    }

    /// A slot keeps as many of its value's first bytes as judge its kind, so a Save names an SVG
    /// whose root comes after its declaration and a comment as the preview shows it.
    #[gpui::test]
    async fn a_slot_names_a_saved_value_by_the_bytes_that_judge_its_kind(cx: &mut TestAppContext) {
        let svg = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!-- a square -->\n<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"4\" height=\"4\"/>\n";
        let (view, _session, _blobs, cx) = page_with_doc(Some(svg), None, cx);
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.blobs[0].head,
                svg.to_vec(),
                "the whole of a short value"
            );
            assert_eq!(
                suggested_file_name("p1", "doc", &view.blobs[0].head),
                "p1-doc.svg"
            );
        });
    }

    /// A view behind its branch binds a second file at the commit its first one made: the
    /// first moved every validator of the type, and the server applied it only while the node's
    /// table was as the view showed it, so the second is stored, not refused as changed.
    #[gpui::test]
    async fn a_views_own_blob_write_leaves_its_next_one_bound_where_it_holds(
        cx: &mut TestAppContext,
    ) {
        const SCANS: &str =
            "node Person {\n  slug: String @key\n  name: String\n  doc: Blob?\n  scan: Blob?\n}\n";
        let (session, blobs) = crate::test_support::session_with_blobs(SCANS, cx);
        blobs
            .versioned
            .store(true, std::sync::atomic::Ordering::SeqCst);
        blobs.set("Person/p1/doc", b"doc");
        blobs.set("Person/p1/scan", b"scan");
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (view, cx) = cx.add_window_view({
            let session = session.clone();
            move |_, cx| NodePeek::page(session, "Person".into(), "p1".into(), None, cx)
        });
        cx.run_until_parked();
        // Behind its branch: another writer's commit came after the view's read.
        view.update(cx, |view, _| {
            view.snapshot = Some("c0".into());
            view.newer = true;
        });
        let file = crate::test_support::upload_file("doc.txt", b"new doc", None);
        view.update(cx, |view, cx| view.upload_blob("doc", file, cx));
        cx.run_until_parked();
        // The slots hold at the write's commit now, not at the view's: a tab opened from one
        // reads there, so it shows the value written and binds to it.
        view.read_with(cx, |view, _| {
            assert_eq!(view.blobs_at, BlobsAt::Own("c2".into()));
            let open = view.open_blob_action("doc");
            assert_eq!(
                open.at_commit.as_deref(),
                Some("c2"),
                "not the replaced value"
            );
            assert!(!open.held);
        });
        let scan_now = blobs.get("Person/p1/scan").unwrap().1;
        let file = crate::test_support::upload_file("scan.txt", b"new scan", None);
        view.update(cx, |view, cx| view.upload_blob("scan", file, cx));
        cx.run_until_parked();
        {
            let writes = blobs.writes.lock().unwrap();
            assert_eq!(writes.len(), 2);
            assert_eq!(writes[1].if_match.as_deref(), Some(scan_now.as_str()));
        }
        assert_eq!(blobs.get("Person/p1/scan").unwrap().0, b"new scan");
        view.read_with(cx, |view, _| {
            assert!(
                view.blobs.iter().all(|slot| slot.edit.is_none()),
                "both stored"
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

    /// ⌘S saves what an open field holds now, one kept open by a notice too, not the older
    /// draft an earlier leave kept.
    #[gpui::test]
    async fn save_takes_what_an_open_field_with_a_notice_holds(cx: &mut TestAppContext) {
        let session = session(PEOPLE, cx);
        let (view, cx) = page(session.clone(), cx);
        view.update_in(cx, |view, window, cx| {
            let person = view.schema.as_ref().unwrap().node("Person").unwrap();
            let property = person.property("name").unwrap().clone();
            view.start_edit(property.clone(), window, cx);
            let editor = view.edits["name"].editor.clone();
            editor.update(cx, |editor, cx| editor.set_text("First", window, cx));
            view.left_field("name", cx);
            view.start_edit(property, window, cx);
            let editor = view.edits["name"].editor.clone();
            editor.update(cx, |editor, cx| editor.set_text("Second", window, cx));
            // Refused, the field stays open with its reason.
            view.edits.get_mut("name").unwrap().notice = Some(Notice::new(Tone::Refused, "no"));
            view.save_drafts(&SaveNodeDrafts, window, cx)
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.value_of("name"),
                json!("Second"),
                "what the field holds"
            );
        });
    }

    const KNOWS: &str =
        "node Person {\n  slug: String @key\n  name: String\n}\nedge Knows: Person -> Person\n";

    /// A page reads each relationship group once: its first neighbours and one more, which is
    /// the group's exact count when they fit. Only a group past the page reads its count, of
    /// edges, as the peek lists one row an edge.
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
                    Some("studio_edge_degree") => json!([{ "n": 120 }]),
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
            reads("studio_edge_degree"),
            1,
            "a count only past the page: {asked:?}"
        );

        // A write of its table to other rows: one it shows among its neighbours would show by
        // its old name at the new commit, so the view stays behind; one it does not show moves
        // it on.
        let pin = view.read_with(cx, |view, _| view.reads_at().map(str::to_owned));
        let wrote = |touched: &str, commit: &str| TableWrote {
            commit: commit.to_owned(),
            parent: pin.clone(),
            values: Vec::new(),
            blob: None,
            type_name: "Person".into(),
            touched: HashSet::from([touched.to_owned()]),
        };
        view.update(cx, |view, cx| view.table_wrote(wrote("p010", "c2"), cx));
        view.read_with(cx, |view, _| {
            assert!(view.is_newer(), "a neighbour shown was written");
            assert_eq!(view.reads_at(), pin.as_deref());
        });
        view.update(cx, |view, _| view.newer = false);
        view.update(cx, |view, cx| view.table_wrote(wrote("p999", "c2"), cx));
        view.read_with(cx, |view, _| assert_eq!(view.reads_at(), Some("c2")));
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

    /// A peek given only a table's count for a group lists the group's neighbours itself, one
    /// row an edge: its count becomes the edges it read, so with eight neighbours by nine edges
    /// the ninth is a "+1 more" away. A change to a node of the other type that it lists none
    /// of moves it along: the count lists no neighbour.
    #[gpui::test]
    async fn a_peek_given_a_count_lists_and_counts_its_own_edges(cx: &mut TestAppContext) {
        // Eight neighbours by nine edges: the first of them related twice.
        let edges: Vec<Value> = (0..9usize)
            .map(|i| {
                let neighbour = 10 + i.max(1) - 1;
                json!({ "id": format!("p{neighbour:03}"), "edge_id": format!("e{i}") })
            })
            .collect();
        let (session, writer) = session_followed(
            KNOWS,
            move |request| {
                let query = request["query"].as_str().unwrap_or_default();
                match request["name"].as_str() {
                    // As many of its edges as the read asks for.
                    Some("studio_neighbours") if query.contains("$n $e:knows $m") => {
                        let limit = query
                            .rsplit("limit ")
                            .next()
                            .and_then(|rest| rest.split_whitespace().next())
                            .and_then(|n| n.parse().ok())
                            .unwrap_or(usize::MAX);
                        Value::from(edges.iter().take(limit).cloned().collect::<Vec<_>>())
                    }
                    Some("studio_edge_degree") => json!([{ "n": 9 }]),
                    _ => json!([]),
                }
            },
            cx,
        );
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        cx.run_until_parked();
        let schema = session.read_with(cx, |session, _| session.schema_model().unwrap());
        let knows = schema
            .relationships_of("Person")
            .iter()
            .find(|group| group.direction == Direction::Outgoing)
            .unwrap()
            .clone();
        // The table counts neighbours, each once.
        let known = HashMap::from([(
            builder::relationship_key(&knows),
            KnownRelation {
                count: 8,
                first: None,
            },
        )]);
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
            let group = view.groups.iter().find(|g| g.group == knows).unwrap();
            assert!(group.expanded, "a small group opens");
            assert_eq!(group.count, Loadable::Loaded(9), "its edges");
            assert_eq!(
                group.neighbours.loaded().map(Vec::len),
                Some(8),
                "and one more"
            );
        });
        let session = view.read_with(cx, |view, _| view.session.clone());
        writer.commit("c2", vec![changed("node", "Person", "p999", None)]);
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.snapshot.as_deref(), Some("c2"), "moved along");
            assert!(!view.newer, "a person it does not list");
        });
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

        // A peek of a table held at that commit, given the counts it read: the groups it opens
        // read their neighbours the same way.
        let session = view.read_with(cx, |view, _| view.session.clone());
        let schema = session.read_with(cx, |session, _| session.schema_model().unwrap());
        let known: HashMap<String, KnownRelation> = schema
            .relationships_of("Person")
            .iter()
            .map(|group| {
                let count = if group.edge == "Knows" { 0 } else { 1 };
                let known = KnownRelation { count, first: None };
                (builder::relationship_key(group), known)
            })
            .collect();
        let values: Map<String, Value> =
            serde_json::from_value(json!({ "@id": "p1", "slug": "p1", "name": "Ada" })).unwrap();
        let peek = cx.update(|_, cx| {
            cx.new(|cx| {
                let subject = PeekSubject::Node {
                    type_name: "Person".into(),
                    id: "p1".into(),
                };
                let mut peek = NodePeek::new(
                    session,
                    subject,
                    Some(values),
                    Some("c0".into()),
                    known,
                    false,
                    cx,
                );
                peek.hold_commit();
                peek
            })
        });
        let likes = peek.read_with(cx, |peek, _| {
            peek.groups
                .iter()
                .position(|group| group.group.edge == "Likes")
                .unwrap()
        });
        peek.update(cx, |peek, cx| peek.toggle_group(likes, cx));
        cx.run_until_parked();
        peek.read_with(cx, |peek, _| {
            let group = &peek.groups[likes];
            assert_eq!(
                group.neighbours.loaded().map(Vec::len),
                Some(1),
                "read without since"
            );
            assert_eq!(group.count, Loadable::Loaded(1));
            assert_eq!(
                peek.groups_absent,
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

    /// A peek of a row of a table behind its branch compares the head the session has seen as
    /// it opens, as a head move is compared: one whose node changed since offers Refresh at
    /// once, rather than showing the table's older values as current, and one whose node did
    /// not moves to the head.
    #[gpui::test]
    async fn a_peek_of_a_table_behind_its_branch_compares_the_head_as_it_opens(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, writer) = session_followed(
            PEOPLE,
            |_| {
                json!([
                    { "n": { "@id": "p1", "slug": "p1", "name": "Ada" } },
                    { "n": { "@id": "p2", "slug": "p2", "name": "Grace" } },
                ])
            },
            cx,
        );
        let (table, cx) = cx.add_window_view({
            let session = session.clone();
            move |window, cx| {
                crate::TypeTable::new(
                    gpui::WeakEntity::new_invalid(),
                    session,
                    "Person".into(),
                    None,
                    window,
                    cx,
                )
            }
        });
        cx.run_until_parked();
        // Another writer changes p1: the table stays at c1, behind.
        writer.commit("c2", vec![changed("node", "Person", "p1", None)]);
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        let peek = table.update(cx, |table, cx| table.peek_row(0, cx));
        cx.run_until_parked();
        peek.read_with(cx, |peek, _| {
            assert_eq!(peek.reads_at(), Some("c1"), "the table's commit");
            assert!(peek.is_newer(), "its node changed since");
        });
        let peek = table.update(cx, |table, cx| table.peek_row(1, cx));
        cx.run_until_parked();
        peek.read_with(cx, |peek, _| {
            assert_eq!(peek.reads_at(), Some("c2"), "nothing it shows changed");
            assert!(!peek.is_newer());
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
    /// view moves to (spec 014). While that write is out, the group takes no other change:
    /// presses and an add send nothing more.
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
            view.press_remove(0, "e1".into(), window, cx);
            view.press_remove(0, "e1".into(), window, cx);
            assert!(view.removing.is_none(), "no press while the write is out");
            view.start_add(0, window, cx);
            assert!(view.adding.is_none(), "no add while the write is out");
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

    /// A replace is two commits, since one mutation either adds or removes: the page follows
    /// both. When the addition does not land after the removal did, it says the old
    /// relationship is gone, never that nothing was written: refused, it was not added; its
    /// answer lost, whether it was added is not known, and History is where that is settled. A
    /// removal whose answer is lost sends no addition, and says so.
    #[gpui::test]
    async fn a_replaced_relationship_follows_both_commits_and_reports_a_half(
        cx: &mut TestAppContext,
    ) {
        use crate::test_support::{committed, session_writing};
        let refused = json!({ "error": "__dst 'p9' not found in Person", "code": "bad_request", "disposition": { "action": "fix", "effects": "none" } });
        let lost = (500, json!({}));
        let cases = [
            // The second write's answer; the first's, which lands, comes first.
            (Some((200, committed("c3", "c2"))), "c3", None),
            (
                Some((400, refused)),
                "c2",
                Some((
                    Tone::Refused,
                    "The relationship it replaces was removed, but the new one was not added. \
                     Nothing was written. __dst 'p9' not found in Person",
                )),
            ),
            (
                Some(lost.clone()),
                "c2",
                Some((
                    Tone::Unknown,
                    "The relationship it replaces was removed. The server's answer was lost, so \
                     whether the new one was added is not known. Check History before trying \
                     again.",
                )),
            ),
            // The removal's answer lost: nothing more is sent.
            (
                None,
                "c1",
                Some((
                    Tone::Unknown,
                    "The server's answer was lost, so whether the relationship it replaces was \
                     removed is not known, and the new one was not sent. Check History before \
                     trying again.",
                )),
            ),
        ];
        for (second, at, said) in cases {
            let answers = match second {
                Some(second) => vec![(200, committed("c2", "c1")), second],
                None => vec![lost.clone(), (200, committed("c2", "c1"))],
            };
            let session = session_writing(KNOWS, answers, cx);
            let (view, cx) = page(session.clone(), cx);
            let before = session.read_with(cx, |session, _| session.outcomes.len());
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
            let sent = session.read_with(cx, |session, _| session.outcomes.len()) - before;
            assert_eq!(sent, if at == "c1" { 1 } else { 2 }, "writes sent");
            view.read_with(cx, |view, _| {
                assert_eq!(view.snapshot.as_deref(), Some(at), "followed what landed");
                let notice = view
                    .group_notices
                    .get(&0)
                    .map(|n| (n.tone, n.message.as_str()));
                assert_eq!(notice, said);
                if said.is_none() {
                    assert!(!view.newer);
                }
            });
        }
    }

    /// A relationship each node needs exactly one of (`@card(1..1)`) cannot be replaced: its
    /// removal alone is refused, as is a second one added first, and the server takes the two
    /// in no one mutation. Its "+" says so, and a choice sends nothing. One that may hold none
    /// is still added to while it has none.
    #[gpui::test]
    async fn a_relationship_that_needs_its_one_neighbour_is_not_replaced_and_says_why(
        cx: &mut TestAppContext,
    ) {
        const MENTOR: &str = "node Person {\n  slug: String @key\n  name: String\n}\nedge Mentor: Person -> Person @card(1..1)\n";
        let (session, _) = session_routing(
            MENTOR,
            |request| match request["name"].as_str() {
                Some("studio_node") => {
                    json!([{ "n": { "@id": "p1", "slug": "p1", "name": "Ada" } }])
                }
                Some("studio_neighbours") => json!([{ "id": "p2", "edge_id": "e1" }]),
                _ => json!([]),
            },
            cx,
        );
        let (view, cx) = page(session.clone(), cx);
        let refusal = "Each Person has exactly one Mentor, so Studio cannot replace it: the \
                       server takes its removal and the new one only as two commits, and each \
                       alone breaks that rule.";
        let before = session.read_with(cx, |session, _| session.outcomes.len());
        view.update_in(cx, |view, window, cx| {
            assert!(view.holds_one(0));
            assert_eq!(view.groups[0].count, Loadable::Loaded(1));
            assert_eq!(view.replace_refusal(0).as_deref(), Some(refusal));
            view.start_add(0, window, cx);
            assert!(view.adding.is_none(), "the search does not open");
            // A search opened before the group was counted: the choice sends nothing.
            view.groups[0].count = Loadable::NotLoaded;
            view.start_add(0, window, cx);
            view.groups[0].count = Loadable::Loaded(1);
            found(view.adding.as_mut().unwrap(), &["p3"]);
            view.add_highlighted(window, cx);
            assert!(view.adding.is_none());
            assert!(view.edge_writes.is_empty(), "nothing is written");
            let notice = view.group_notices.get(&0).expect("why");
            assert_eq!(
                (notice.tone, notice.message.as_str()),
                (Tone::Invalid, refusal)
            );
        });
        cx.run_until_parked();
        assert_eq!(
            session.read_with(cx, |session, _| session.outcomes.len()),
            before,
            "nothing was sent"
        );

        // A node without its mentor yet takes one.
        view.update(cx, |view, _| {
            view.groups[0].count = Loadable::Loaded(0);
            assert_eq!(view.replace_refusal(0), None);
        });
    }

    /// A relationship write whose commit is on no branch (a proposal or a snapshot), or of a
    /// kind this build does not know, is told as it is, as the session logs it: the view does not
    /// move to that commit, and says the branch did not change, or that whether it did is not
    /// known.
    #[gpui::test]
    async fn a_relationship_written_to_no_branch_is_told_as_it_is(cx: &mut TestAppContext) {
        use crate::test_support::{committed, session_writing};
        let of_kind = |kind: &str| {
            let mut answer = committed("c2", "c1");
            answer["commit"]["kind"] = json!(kind);
            answer
        };
        for (kind, tone, said) in [
            (
                "unpublished",
                Tone::Conflict,
                "Adding it was held as commit c2, on no branch until it is published to main: \
                 main did not change.",
            ),
            (
                "unattached",
                Tone::Conflict,
                "Adding it was saved as snapshot c2, on no branch: main did not change.",
            ),
            (
                "rebased",
                Tone::Unknown,
                "The server wrote commit c2, of a kind Studio does not know, so whether a branch \
                 moved is not known. Check History before trying again.",
            ),
        ] {
            let session = session_writing(KNOWS, vec![(200, of_kind(kind))], cx);
            let (view, cx) = page(session, cx);
            view.update_in(cx, |view, window, cx| {
                view.write_edge(0, EdgeChange::Add("p9".into()), None, window, cx);
            });
            cx.run_until_parked();
            view.read_with(cx, |view, _| {
                assert_eq!(view.snapshot.as_deref(), Some("c1"), "{kind}: did not move");
                let notice = view
                    .group_notices
                    .get(&0)
                    .map(|n| (n.tone, n.message.as_str()));
                assert_eq!(notice, Some((tone, said)), "{kind}");
            });
        }
    }

    /// Two relationship writes started together each follow the page on: the second lands on
    /// top of the first, which the page shows by then, and is no sign of newer data.
    #[gpui::test]
    async fn overlapping_relationship_writes_follow_one_another(cx: &mut TestAppContext) {
        use crate::test_support::{committed, session_writing};
        let session = session_writing(
            KNOWS,
            vec![(200, committed("c2", "c1")), (200, committed("c3", "c2"))],
            cx,
        );
        let (view, cx) = page(session, cx);
        view.update_in(cx, |view, window, cx| {
            assert!(view.groups.len() >= 2, "out and in");
            view.snapshot = Some("c1".into());
            view.write_edge(0, EdgeChange::Add("p9".into()), None, window, cx);
            view.write_edge(1, EdgeChange::Add("p8".into()), None, window, cx);
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.snapshot.as_deref(), Some("c3"));
            assert!(!view.newer, "nothing came from elsewhere");
        });
    }

    /// ↵ picks from the results for what is typed now: results read for earlier text are not
    /// written to while the search for the new text is out.
    #[gpui::test]
    async fn enter_waits_for_the_results_of_what_is_typed(cx: &mut TestAppContext) {
        let refused = json!({ "error": "refused", "code": "bad_request", "disposition": { "action": "fix", "effects": "none" } });
        let (view, cx) = page(session_answering(KNOWS, 400, refused, cx), cx);
        view.update_in(cx, |view, window, cx| {
            view.start_add(0, window, cx);
            let adding = view.adding.as_mut().unwrap();
            adding
                .editor
                .update(cx, |editor, cx| editor.set_text("grace", window, cx));
            found(adding, &["p1", "p2"]);
            adding.answers = Some("gr".into());
            // ↑ from the first goes around to the last, and ↓ back, as every offer list moves.
            view.move_match(-1, cx);
            assert_eq!(view.adding.as_ref().unwrap().offers.highlighted(), Some(1));
            view.move_match(1, cx);
            assert_eq!(view.adding.as_ref().unwrap().offers.highlighted(), Some(0));
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
        let refused = json!({ "error": "__dst 'p9' not found in Person", "code": "bad_request", "disposition": { "action": "fix", "effects": "none" } });
        let (view, cx) = page(session_answering(KNOWS, 400, refused, cx), cx);
        view.update_in(cx, |view, window, cx| {
            view.start_add(0, window, cx);
            found(view.adding.as_mut().unwrap(), &["p9"]);
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
        let refused = json!({ "error": "policy denied", "code": "forbidden", "disposition": { "action": "refresh", "effects": "none" } });
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

    /// A refused write does not open its field over another the person opened meanwhile, which
    /// would take the keyboard and what they type: its text is kept as its draft, with why.
    #[gpui::test]
    async fn a_refused_field_leaves_the_keyboard_where_it_is(cx: &mut TestAppContext) {
        let refused = json!({ "error": "policy denied", "code": "forbidden", "disposition": { "action": "refresh", "effects": "none" } });
        let schema = PEOPLE.replace("  name: String\n", "  name: String\n  email: String?\n");
        let session = session_answering(&schema, 403, refused, cx);
        let (view, cx) = page(session.clone(), cx);
        view.update_in(cx, |view, window, cx| {
            let person = view.schema.as_ref().unwrap().node("Person").unwrap();
            let name = person.property("name").unwrap().clone();
            let email = person.property("email").unwrap().clone();
            view.start_edit(name, window, cx);
            let editor = view.edits["name"].editor.clone();
            editor.update(cx, |editor, cx| editor.set_text("Ada Lovelace", window, cx));
            view.save_field("name", window, cx);
            view.start_edit(email, window, cx);
        });
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(
                !view.edits.contains_key("name"),
                "not opened over the other"
            );
            assert!(
                view.edits["email"]
                    .editor
                    .focus_handle(cx)
                    .is_focused(window)
            );
            assert_eq!(
                view.drafts_notice.as_ref().map(|notice| notice.tone),
                Some(Tone::Refused)
            );
        });
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

    /// A write out when the peek goes (the table's cursor moved on) is settled where it was
    /// typed: refused, its text is kept as the property's draft.
    #[gpui::test]
    async fn a_refused_write_whose_peek_went_keeps_its_text(cx: &mut TestAppContext) {
        let refused = json!({ "error": "policy denied", "code": "forbidden", "disposition": { "action": "refresh", "effects": "none" } });
        let session = session_answering(PEOPLE, 403, refused, cx);
        let (view, cx) = page(session.clone(), cx);
        view.update_in(cx, |view, window, cx| {
            let person = view.schema.as_ref().unwrap().node("Person").unwrap();
            let property = person.property("name").unwrap().clone();
            view.start_edit(property, window, cx);
            let editor = view.edits["name"].editor.clone();
            editor.update(cx, |editor, cx| editor.set_text("Ada Lovelace", window, cx));
            view.save_field("name", window, cx);
            assert!(view.saving.on("main", "name"), "the write is out");
        });
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
            draft,
            Some(DraftEntry {
                old: json!("Ada"),
                text: "Ada Lovelace".into(),
            })
        );
    }

    /// One write of a property is out at a time: a second, from a control drawn before the first
    /// went out, sends nothing and keeps its text as the property's draft. Before, both went,
    /// and landed in whatever order the server took them.
    #[gpui::test]
    async fn a_second_write_of_a_property_waits_as_a_draft(cx: &mut TestAppContext) {
        let session = session(PEOPLE, cx);
        let (view, cx) = page(session.clone(), cx);
        view.update_in(cx, |view, window, cx| {
            let person = view.schema.as_ref().unwrap().node("Person").unwrap();
            let property = person.property("name").unwrap().clone();
            let old = view.value_of("name");
            let first = json!("Ada Lovelace");
            view.write_value(
                property.clone(),
                old,
                first.clone(),
                "Ada Lovelace".into(),
                window,
                cx,
            );
            assert!(view.saving.on("main", "name"), "the first is out");
            view.write_value(
                property,
                first.clone(),
                json!("Ada King"),
                "Ada King".into(),
                window,
                cx,
            );
            assert_eq!(view.value_of("name"), first, "the second was not sent");
            assert_eq!(
                view.draft_of("name", cx).map(|draft| draft.text).as_deref(),
                Some("Ada King")
            );
        });
    }

    /// A write still out when the view leaves its branch shows again when the view comes back
    /// before it lands, so its property is not written a second time; the other branch shows
    /// none of it. Before, the markers went with the switch, and the property read as idle.
    #[gpui::test]
    async fn a_write_out_shows_again_on_its_branch(cx: &mut TestAppContext) {
        let session = session(PEOPLE, cx);
        let (view, cx) = page(session.clone(), cx);
        view.update_in(cx, |view, window, cx| {
            let property = view.property_named("name").unwrap().clone();
            view.start_edit(property, window, cx);
            let editor = view.edits["name"].editor.clone();
            editor.update(cx, |editor, cx| editor.set_text("Ada Lovelace", window, cx));
            view.save_field("name", window, cx);
            assert!(view.shown("name").1, "the write is out");
        });
        session.update(cx, |session, cx| session.set_branch("fix".into(), cx));
        view.read_with(cx, |view, _| {
            assert!(!view.shown("name").1, "fix writes nothing")
        });
        session.update(cx, |session, cx| session.set_branch("main".into(), cx));
        view.read_with(cx, |view, _| {
            assert!(view.shown("name").1, "shown again on main")
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert!(view.saving.is_empty(), "landed"));
    }

    /// A write refused after the view followed the window to another branch keeps its text as
    /// the property's draft on the branch it was typed on, and leaves the new branch alone.
    #[gpui::test]
    async fn a_refused_write_after_a_branch_switch_keeps_its_text(cx: &mut TestAppContext) {
        let refused = json!({ "error": "policy denied", "code": "forbidden", "disposition": { "action": "refresh", "effects": "none" } });
        let session = session_answering(PEOPLE, 403, refused, cx);
        let (view, cx) = page(session.clone(), cx);
        view.update_in(cx, |view, window, cx| {
            let person = view.schema.as_ref().unwrap().node("Person").unwrap();
            let property = person.property("name").unwrap().clone();
            view.start_edit(property, window, cx);
            let editor = view.edits["name"].editor.clone();
            editor.update(cx, |editor, cx| editor.set_text("Ada Lovelace", window, cx));
            view.save_field("name", window, cx);
            assert!(view.saving.on("main", "name"), "the write is out");
        });
        session.update(cx, |session, cx| session.set_branch("fix".into(), cx));
        cx.run_until_parked();
        let key = DraftKey {
            type_name: "Person".into(),
            id: "p1".into(),
            property: "name".into(),
        };
        session.read_with(cx, |session, _| {
            let on_main: Vec<_> = session.drafts_on("main").collect();
            let entry = DraftEntry {
                old: json!("Ada"),
                text: "Ada Lovelace".into(),
            };
            assert_eq!(on_main, vec![(&key, &entry)]);
            assert!(session.draft(&key).is_none(), "nothing on fix");
        });
        view.read_with(cx, |view, _| assert!(view.saving.is_empty()));
    }

    /// A write that does not land while the node is being read again (a Refresh's read out
    /// when it lands) cannot open its field: what was typed is kept as the property's draft,
    /// bound to the value it was typed over, and the node's drafts say why.
    #[gpui::test]
    async fn a_refused_write_during_a_refresh_keeps_its_text_and_reason(cx: &mut TestAppContext) {
        let session = session(PEOPLE, cx);
        let (view, cx) = page(session.clone(), cx);
        view.update_in(cx, |view, window, cx| {
            let person = view.schema.as_ref().unwrap().node("Person").unwrap();
            let property = person.property("name").unwrap().clone();
            view.reload(cx);
            assert!(view.values.loaded().is_none(), "read again");
            let field = FieldWrite {
                property,
                old: json!("Ada"),
                new: json!("Ada Lovelace"),
                text: "Ada Lovelace".into(),
                shown: json!("Ada"),
                sent: None,
            };
            let notice = Notice::new(Tone::Refused, "Nothing was written.");
            view.finish_write(field, SaveResult::Refused(notice), window, cx);
            assert!(view.edits.is_empty(), "the field cannot open yet");
        });
        cx.run_until_parked();
        let key = DraftKey {
            type_name: "Person".into(),
            id: "p1".into(),
            property: "name".into(),
        };
        let draft = session.read_with(cx, |session, _| session.draft(&key).cloned());
        assert_eq!(
            draft,
            Some(DraftEntry {
                old: json!("Ada"),
                text: "Ada Lovelace".into(),
            })
        );
        view.read_with(cx, |view, _| {
            assert_eq!(view.value_of("name"), json!("Ada"), "read");
            let tone = view.drafts_notice.as_ref().map(|notice| notice.tone);
            assert_eq!(tone, Some(Tone::Refused), "why it did not land");
        });
    }

    /// A field's write that lands after the node was read again while it was out (a Refresh,
    /// another field's conflict) shows the value written at its commit, not the value the read
    /// found before the write; one that lands past another writer's commit leaves the view at
    /// its own, showing what it read there.
    #[gpui::test]
    async fn a_field_saved_after_its_node_was_read_again_shows_the_value_written(
        cx: &mut TestAppContext,
    ) {
        let session = session(PEOPLE, cx);
        let (view, cx) = page(session.clone(), cx);
        let name = view.read_with(cx, |view, _| view.property_named("name").cloned().unwrap());
        let saved = |parent: &str, commit: &str, cx: &mut VisualTestContext| {
            view.update_in(cx, |view, window, cx| {
                view.saving.insert("main", "name");
                let field = FieldWrite {
                    property: name.clone(),
                    old: json!("Ada"),
                    new: json!("Ada Lovelace"),
                    text: "Ada Lovelace".into(),
                    shown: json!("Ada"),
                    sent: None,
                };
                let result = SaveResult::Saved {
                    commit: commit.into(),
                    parent: Some(parent.into()),
                };
                view.finish_write(field, result, window, cx);
            });
        };
        // Refresh read the node at c1 while the write was out: it found "Ada".
        view.update(cx, |view, cx| view.reload(cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.snapshot.as_deref(), Some("c1"));
            assert_eq!(view.value_of("name"), json!("Ada"));
        });
        saved("c1", "c2", cx);
        view.read_with(cx, |view, _| {
            assert_eq!(view.snapshot.as_deref(), Some("c2"), "its own commit");
            assert_eq!(
                view.value_of("name"),
                json!("Ada Lovelace"),
                "as written there"
            );
        });

        // Read again at c1, and the write lands past another writer's commit: the view stays at
        // c1, with what c1 holds.
        view.update(cx, |view, cx| view.reload(cx));
        cx.run_until_parked();
        saved("c5", "c6", cx);
        view.read_with(cx, |view, _| {
            assert_eq!(view.snapshot.as_deref(), Some("c1"));
            assert_eq!(view.value_of("name"), json!("Ada"), "what c1 holds");
        });
    }

    /// A write that matched nothing because what it would make is there already (an earlier
    /// try whose outcome was unknown landed, or the same change was made elsewhere) is written:
    /// the field closes, its draft goes, and nothing says the value changed elsewhere. Before,
    /// the person's own change was named as another's, and the field stayed open on it.
    #[gpui::test]
    async fn a_conflict_with_the_value_typed_closes_the_field(cx: &mut TestAppContext) {
        let (session, writer) = crate::test_support::session_routing_unmatched(
            PEOPLE,
            |request| {
                let name = match request["at"].as_str() {
                    Some("c2") => "Ada Lovelace",
                    _ => "Ada",
                };
                json!([{ "n": { "@id": "p1", "slug": "p1", "name": name } }])
            },
            cx,
        );
        let (view, cx) = page(session.clone(), cx);
        writer.commit("c2", vec![changed("node", "Person", "p1", None)]);
        edit_name(&view, "Ada Lovelace", cx);
        view.read_with(cx, |view, cx| {
            assert!(!view.edits.contains_key("name"), "the field closed");
            assert_eq!(view.value_of("name"), json!("Ada Lovelace"));
            assert!(view.draft_of("name", cx).is_none(), "no draft is left");
            assert!(view.saving.is_empty());
        });
    }

    /// A write that matched nothing moves the view to the head: the head is asked after, and
    /// the node read again at it. The field opens again bound to what the view then shows,
    /// which the notice names, so the next ↵ writes over the value seen rather than conflicting
    /// again, and a Refresh keeps that value in view. ⌘S binds its drafts there alike; a draft
    /// retyped while their read was out stands as it is.
    #[gpui::test]
    async fn a_conflict_is_bound_to_the_value_read_at_the_head(cx: &mut TestAppContext) {
        // Another writer renames Ada at c2, and again at c3.
        let (session, writer) = crate::test_support::session_routing_unmatched(
            PEOPLE,
            |request| {
                let name = match request["at"].as_str() {
                    Some("c2") => "Ada B",
                    Some("c3") => "Ada C",
                    _ => "Ada",
                };
                json!([{ "n": { "@id": "p1", "slug": "p1", "name": name } }])
            },
            cx,
        );
        let (view, cx) = page(session.clone(), cx);
        let at_head = |cx: &mut VisualTestContext, commit: &str, value: &str| {
            session.read_with(cx, |session, _| {
                assert_eq!(session.head("main"), Some(commit), "asked after")
            });
            view.read_with(cx, |view, _| {
                assert_eq!(
                    view.snapshot.as_deref(),
                    Some(commit),
                    "the view is at the head"
                );
                assert!(!view.newer);
                assert_eq!(view.value_of("name"), json!(value), "what the head holds");
                assert!(view.saving.is_empty());
            });
        };
        writer.commit("c2", vec![changed("node", "Person", "p1", None)]);
        edit_name(&view, "Ada Lovelace", cx);
        at_head(cx, "c2", "Ada B");
        view.read_with(cx, |view, cx| {
            let edit = &view.edits["name"];
            assert_eq!(edit.old, json!("Ada B"), "bound to it");
            assert_eq!(edit.editor.read(cx).text(cx), "Ada Lovelace");
            let notice = edit.notice.as_ref().expect("why it did not land");
            assert_eq!(notice.tone, Tone::Conflict);
            assert!(notice.message.contains("Ada B"), "{}", notice.message);
        });

        // Refresh reads at the head again: the value the field is bound to stays in view.
        view.update(cx, |view, cx| view.refresh(cx));
        cx.run_until_parked();
        at_head(cx, "c2", "Ada B");
        view.read_with(cx, |view, _| {
            assert_eq!(view.edits["name"].old, json!("Ada B"));
        });

        // A draft typed over the old value: ⌘S finds the change, moves the view to the head,
        // and binds the draft to what it holds there.
        let key = DraftKey {
            type_name: "Person".into(),
            id: "p1".into(),
            property: "name".into(),
        };
        view.update(cx, |view, _| view.edits.clear());
        session.update(cx, |session, cx| {
            let draft = DraftEntry {
                old: json!("Ada"),
                text: "Ada King".into(),
            };
            session.set_draft(key.clone(), draft, cx);
        });
        writer.commit("c3", vec![changed("node", "Person", "p1", None)]);
        view.update_in(cx, |view, window, cx| {
            view.save_drafts(&SaveNodeDrafts, window, cx)
        });
        cx.run_until_parked();
        let draft = session.read_with(cx, |session, _| session.draft(&key).cloned());
        assert_eq!(
            draft,
            Some(DraftEntry {
                old: json!("Ada C"),
                text: "Ada King".into(),
            })
        );
        at_head(cx, "c3", "Ada C");
        view.read_with(cx, |view, _| {
            let notice = view
                .drafts_notice
                .as_ref()
                .expect("why nothing was written");
            assert!(notice.message.contains("Ada C"), "{}", notice.message);
            assert!(!view.saving_drafts);
        });

        // Retyped (in a table cell, on another page) while the read was out, the draft stands,
        // whether the head holds the value it was sent asking for or another.
        let retyped = DraftEntry {
            old: json!("Ada C"),
            text: "Ada K.".into(),
        };
        for head_holds in ["Ada King", "Ada D"] {
            session.update(cx, |session, cx| {
                session.set_draft(key.clone(), retyped.clone(), cx)
            });
            view.update(cx, |view, cx| {
                let sent = SentDraft {
                    name: "name".into(),
                    old: json!("Ada C"),
                    new: json!("Ada King"),
                    text: "Ada King".into(),
                };
                let mut now = view.values.loaded().cloned().unwrap();
                now.insert("name".into(), json!(head_holds));
                view.drafts_read(vec![sent], Some(&Some(now)), cx);
            });
            let draft = session.read_with(cx, |session, _| session.draft(&key).cloned());
            assert_eq!(draft.as_ref(), Some(&retyped), "head holds {head_holds}");
        }
    }

    /// Refresh wants the newest, which the session may not hold yet: it asks after the head,
    /// and reads the node at it. A read of the node that lands after the view was read again
    /// since it was sent is dropped, and puts nothing older back; a conflict that sent it
    /// settles against what the newer read finds.
    #[gpui::test]
    async fn refresh_asks_after_the_head_and_an_older_read_lands_nothing(cx: &mut TestAppContext) {
        let (session, writer) = session_followed(
            PEOPLE,
            |request| {
                let name = match request["at"].as_str() {
                    Some("c2") => "Ada B",
                    _ => "Ada",
                };
                json!([{ "n": { "@id": "p1", "slug": "p1", "name": name } }])
            },
            cx,
        );
        let (view, cx) = page(session.clone(), cx);
        // A read of the node at the commit shown, out when Refresh is pressed.
        let (older, sent) = view.update(cx, |view, cx| view.send_node_read(cx)).unwrap();
        writer.commit("c2", vec![changed("node", "Person", "p1", None)]);
        view.update(cx, |view, cx| view.refresh(cx));
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            assert_eq!(session.head("main"), Some("c2"))
        });
        let at_head = |cx: &mut VisualTestContext| {
            view.read_with(cx, |view, _| {
                assert_eq!(
                    view.snapshot.as_deref(),
                    Some("c2"),
                    "read at the head asked after"
                );
                assert_eq!(view.value_of("name"), json!("Ada B"));
                assert!(!view.newer);
            });
        };
        at_head(cx);
        let answer = older.await;
        let found = view.update(cx, |view, cx| view.node_read_landed(answer, sent, cx));
        assert_eq!(found, None, "dropped");
        at_head(cx);

        let (first, first_sent) = view.update(cx, |view, cx| view.send_node_read(cx)).unwrap();
        let (second, second_sent) = view.update(cx, |view, cx| view.send_node_read(cx)).unwrap();
        let first = first.await;
        let waited = view.update(cx, |view, cx| {
            view.landed_or_newer(Some((first, first_sent)), cx)
        });
        let Err(newer) = waited else {
            panic!("the newer read is still out");
        };
        let second = second.await;
        view.update(cx, |view, cx| {
            view.node_read_landed(second, second_sent, cx)
        });
        let found = newer.await.unwrap();
        let name = found
            .flatten()
            .and_then(|values| values.get("name").cloned());
        assert_eq!(name, Some(json!("Ada B")), "what the newer read found");
    }

    /// ↵ on a field typed back to the value its draft was typed over lets the draft go, and
    /// writes nothing, while the node still holds that value. Over one it holds since, asking
    /// for the one typed over back is a change: the bound write goes, and finds what changed.
    #[gpui::test]
    async fn a_field_typed_back_and_saved_lets_its_draft_go(cx: &mut TestAppContext) {
        let (session, sent) = crate::test_support::session_recording_writes(PEOPLE, cx);
        let (view, cx) = page(session.clone(), cx);
        let key = DraftKey {
            type_name: "Person".into(),
            id: "p1".into(),
            property: "name".into(),
        };
        let typed_back = |old: &str, cx: &mut VisualTestContext| {
            session.update(cx, |session, cx| {
                let draft = DraftEntry {
                    old: json!(old),
                    text: "Ada King".into(),
                };
                session.set_draft(key.clone(), draft, cx);
            });
            edit_name(&view, old, cx);
        };
        typed_back("Ada", cx);
        session.read_with(cx, |session, _| assert_eq!(session.draft(&key), None));
        view.read_with(cx, |view, _| {
            assert!(
                view.saving.is_empty() && view.edits.is_empty(),
                "nothing written"
            );
        });
        assert!(sent.lock().unwrap().is_empty());

        typed_back("Ada B", cx);
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "the bound write goes: {sent:?}");
        let params: Vec<&str> = sent[0]["params"]
            .as_object()
            .unwrap()
            .values()
            .filter_map(Value::as_str)
            .collect();
        assert!(params.contains(&"Ada B"), "{params:?}");
    }

    /// A table reads its rows' neighbours without the edges relating them. The peek of a row
    /// at the head, which removes and replaces relationships, reads its own with their edges;
    /// the peek of a table held at a commit shows the table's.
    #[gpui::test]
    async fn a_peek_of_a_tables_row_at_the_head_reads_the_edges_it_writes(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        for at_commit in [None, Some("c1".to_owned())] {
            let (session, _) = session_routing(
                KNOWS,
                |request| match request["name"].as_str() {
                    Some("studio_page_degrees") => json!([{ "id": "p1", "n": 1 }]),
                    Some("studio_page_neighbours") => json!([{ "id": "p1", "other": "p2" }]),
                    Some("studio_neighbours") => json!([{ "id": "p2", "edge_id": "e1" }]),
                    _ => json!([
                        { "n": { "@id": "p1", "slug": "p1", "name": "Ada" } },
                        { "n": { "@id": "p2", "slug": "p2", "name": "Grace" } },
                    ]),
                },
                cx,
            );
            let held = at_commit.is_some();
            let (table, cx) = cx.add_window_view(move |window, cx| {
                crate::TypeTable::new(
                    gpui::WeakEntity::new_invalid(),
                    session,
                    "Person".into(),
                    at_commit,
                    window,
                    cx,
                )
            });
            cx.run_until_parked();
            let peek = table.update(cx, |table, cx| table.peek_row(0, cx));
            let index = peek.read_with(cx, |peek, _| {
                let index = peek
                    .groups
                    .iter()
                    .position(|group| group.group.direction == Direction::Outgoing)
                    .unwrap();
                let group = &peek.groups[index];
                assert_eq!(group.count, Loadable::Loaded(1), "as the table counted");
                let first = group.neighbours.loaded().map(|shown| shown[0].clone());
                if held {
                    assert_eq!(first.map(|n| n.id).as_deref(), Some("p2"), "the table's");
                } else {
                    assert!(first.is_none(), "not the table's, which have no edges");
                }
                index
            });
            if held {
                continue;
            }
            cx.update(|window, cx| peek.update(cx, |peek, cx| peek.drawn(index, window, cx)));
            cx.run_until_parked();
            peek.read_with(cx, |peek, _| {
                let group = &peek.groups[index];
                let shown = group.neighbours.loaded().expect("read once drawn");
                assert_eq!(shown[0].edge_id.as_deref(), Some("e1"), "with its edge");
                assert!(group.expanded);
            });
        }
    }

    /// On a relationship that holds one neighbour, adding one replaces the one there: until the
    /// edge it replaces is known, nothing is written, so the addition is never sent alone.
    #[gpui::test]
    async fn a_replace_waits_for_the_edge_it_replaces(cx: &mut TestAppContext) {
        const MENTOR: &str = "node Person {\n  slug: String @key\n  name: String\n}\nedge Mentor: Person -> Person @card(0..1)\n";
        let (session, _) = session_routing(
            MENTOR,
            |request| match request["name"].as_str() {
                Some("studio_node") => {
                    json!([{ "n": { "@id": "p1", "slug": "p1", "name": "Ada" } }])
                }
                Some("studio_neighbours") => json!([{ "id": "p2", "edge_id": "e1" }]),
                _ => json!([]),
            },
            cx,
        );
        let (view, cx) = page(session, cx);
        view.update_in(cx, |view, window, cx| {
            assert!(view.holds_one(0), "a person has one mentor");
            assert_eq!(view.replace_refusal(0), None, "one that may have none");
            // As a table hands its chips: the neighbour, not its edge.
            view.groups[0].neighbours = Loadable::Loaded(vec![Neighbour {
                id: "p2".into(),
                label: None,
                edge_id: None,
            }]);
            view.start_add(0, window, cx);
            found(view.adding.as_mut().unwrap(), &["p3"]);
            view.add_highlighted(window, cx);
            assert!(view.adding.is_some(), "the search stays open");
            assert!(view.edge_writes.is_empty(), "nothing is written");
        });
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(view.replaced_edge(0), Some(Some("e1".to_owned())));
            found(view.adding.as_mut().unwrap(), &["p3"]);
            view.add_highlighted(window, cx);
            assert!(view.adding.is_none());
            assert!(view.edge_writes.contains(&0), "the replace is written");
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert!(view.edge_writes.is_empty()));
    }

    /// An edge written from a page behind the head shows on the page: its group is read at the
    /// commit the write reached, and so is its next page, while the page stays at its own and
    /// offers Refresh.
    #[gpui::test]
    async fn an_edge_written_from_a_page_behind_shows_at_its_commit(cx: &mut TestAppContext) {
        let snapshots = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let seen = snapshots.clone();
        // The commits next pages were read at: those asked from after a neighbour.
        let pages = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let paged = pages.clone();
        // More than a page of neighbours, so there is a next one.
        let rows = Value::from(
            (0..=PAGE_NEIGHBOURS)
                .map(|i| json!({ "id": format!("p{:03}", i + 10), "edge_id": format!("e{i}") }))
                .collect::<Vec<_>>(),
        );
        let (session, _) = session_routing(
            KNOWS,
            move |request| match request["name"].as_str() {
                Some("studio_node") => {
                    json!([{ "n": { "@id": "p1", "slug": "p1", "name": "Ada" } }])
                }
                Some("studio_neighbours") => {
                    seen.lock().unwrap().push(request["snapshot"].clone());
                    if request["params"]["after"].is_string() {
                        paged.lock().unwrap().push(request["snapshot"].clone());
                    }
                    rows.clone()
                }
                Some("studio_edge_degree") => json!([{ "n": 120 }]),
                _ => json!([]),
            },
            cx,
        );
        let (view, cx) = page(session, cx);
        let before = snapshots.lock().unwrap().len();
        view.update_in(cx, |view, window, cx| {
            // As if another writer committed after the page read c0.
            view.snapshot = Some("c0".into());
            view.write_edge(0, EdgeChange::Add("p9".into()), None, window, cx);
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.newer, "behind the head");
            assert_eq!(view.snapshot.as_deref(), Some("c0"), "at its own commit");
        });
        let after = snapshots.lock().unwrap()[before..].to_vec();
        assert!(
            after.contains(&json!("c2")),
            "read where it was written: {after:?}"
        );
        assert!(!after.contains(&json!("c0")), "{after:?}");

        pages.lock().unwrap().clear();
        view.update(cx, |view, cx| view.show_more(0, cx));
        cx.run_until_parked();
        // Read at the commit its first page was read at (c2): sent as a read of the branch while
        // c2 is the head, and pinned to it when the answer names another commit.
        let pages = pages.lock().unwrap();
        assert_eq!(pages.last(), Some(&json!("c2")), "{pages:?}");
        assert!(!pages.contains(&json!("c0")), "{pages:?}");
    }

    /// A head the session sees move while a page's node is read is compared once the read pins
    /// the page: whichever lands first, another view's write to the node offers Refresh.
    #[gpui::test(iterations = 10)]
    async fn a_head_moved_while_a_page_is_read_is_compared_once_it_lands(cx: &mut TestAppContext) {
        let session = session(PEOPLE, cx);
        let (view, cx) = page(session.clone(), cx);
        let schema = session.read_with(cx, |session, _| {
            assert_eq!(session.head("main"), Some("c1"));
            session.schema_model().unwrap()
        });
        let (ada, ida) = (json!("Ada"), json!("Ida"));
        let edit = builder::RowEdit {
            id: "p1".into(),
            cells: vec![("name".into(), ada, ida)],
        };
        let query = builder::update_rows(&schema, "Person", &[edit]).unwrap();
        view.update(cx, |view, cx| view.reload(cx));
        session
            .update(cx, |session, cx| {
                session.write_on("main", &query, None, "Set name of p1".into(), None, cx)
            })
            .detach();
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.snapshot.as_deref(), Some("c1"));
            assert!(view.newer, "its node changed");
        });
    }

    /// Reading a page's Blobs again (a refresh) leaves a save to a file going: its slot keeps
    /// it, and it ends as it would have.
    #[gpui::test]
    async fn a_blob_being_saved_goes_on_through_a_refresh(cx: &mut TestAppContext) {
        const DOCS: &str = "node Person {\n  slug: String @key\n  name: String\n  doc: Blob?\n}\n";
        let (session, _) = session_routing(
            DOCS,
            |request| match request["name"].as_str() {
                Some("studio_node") => {
                    json!([{ "n": { "@id": "p1", "slug": "p1", "name": "Ada" } }])
                }
                _ => json!([]),
            },
            cx,
        );
        let (view, cx) = page(session, cx);
        let ended = std::rc::Rc::new(std::cell::Cell::new(false));
        view.update(cx, |view, cx| {
            assert_eq!(view.blobs.len(), 1, "doc");
            let done = ended.clone();
            view.blobs[0].save = Some(SaveState::Saving { done: 0, of: 10 });
            view.blobs[0]._save = Some(cx.spawn(async move |_, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(10))
                    .await;
                done.set(true);
            }));
            view.refresh(cx);
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(
                matches!(view.blobs[0].save, Some(SaveState::Saving { .. })),
                "its slot shows it"
            );
        });
        cx.executor().advance_clock(Duration::from_millis(10));
        cx.run_until_parked();
        assert!(ended.get(), "the save went on");

        // Ended, it is not carried over the next read: the value offers Open and Save… again.
        view.update(cx, |view, cx| {
            view.blobs[0].save = Some(SaveState::Saved("/saved/doc.pdf".into()));
            view.refresh(cx);
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.blobs[0].save.is_none());
            assert!(view.blobs[0]._save.is_none());
        });
    }

    const DOCS: &str = "node Person {\n  slug: String @key\n  name: String\n  doc: Blob?\n}\n";

    /// A page of p1 whose `doc` holds `bytes`, read; and the server's Blob cells.
    fn page_with_doc<'a>(
        bytes: Option<&[u8]>,
        at_commit: Option<&str>,
        cx: &'a mut TestAppContext,
    ) -> (
        Entity<NodePeek>,
        Entity<GraphSession>,
        Arc<crate::test_support::Blobs>,
        &'a mut VisualTestContext,
    ) {
        let (session, blobs) = crate::test_support::session_with_blobs(DOCS, cx);
        if let Some(bytes) = bytes {
            blobs.set("Person/p1/doc", bytes);
        }
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let at_commit = at_commit.map(str::to_owned);
        let (view, cx) = cx.add_window_view({
            let session = session.clone();
            move |_, cx| NodePeek::page(session, "Person".into(), "p1".into(), at_commit, cx)
        });
        cx.run_until_parked();
        (view, session, blobs, cx)
    }

    /// A page held at a commit saves no drafts, so it shows no bar offering to: its Save did
    /// nothing, and its Discard acted on a branch it no longer followed. A page that follows its
    /// branch shows the node's drafts.
    #[gpui::test]
    async fn only_a_page_that_follows_its_branch_shows_the_drafts_bar(cx: &mut TestAppContext) {
        for (at_commit, shown) in [(None, true), (Some("c1"), false)] {
            let (view, session, _, cx) = page_with_doc(None, at_commit, cx);
            session.update(cx, |session, cx| {
                let key = DraftKey {
                    type_name: "Person".into(),
                    id: "p1".into(),
                    property: "name".into(),
                };
                let draft = graph_store::DraftEntry {
                    old: "Ada".into(),
                    text: "Ada King".into(),
                };
                session.set_draft(key, draft, cx);
            });
            cx.run_until_parked();
            view.update(cx, |view, cx| {
                assert_eq!(view.render_drafts_bar(cx).is_some(), shown, "{at_commit:?}");
            });
        }
    }

    /// A reload lets go of the Blob values shown, as of everything read at the old commit: on a
    /// branch where the node is not, none of the old branch's values shows or is saved.
    #[gpui::test]
    async fn a_reload_lets_go_of_the_blob_values_shown(cx: &mut TestAppContext) {
        let (view, _, _, cx) = page_with_doc(Some(b"hello"), None, cx);
        view.update(cx, |view, cx| {
            assert!(matches!(view.blobs[0].meta, Loadable::Loaded(_)));
            view.forget_shown(cx);
            assert!(
                view.blobs.is_empty(),
                "nothing of the old commit's values stays"
            );
        });
    }

    /// A page not drawn (restored with the workspace behind another tab) reads nothing: not when
    /// it is made, nor when its branch moves. It reads its node once it is drawn. Before, every
    /// such page read its node, then its relationships and Blobs, at start and at each move.
    #[gpui::test]
    async fn a_page_not_drawn_reads_nothing(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let session = session(PEOPLE, cx);
        cx.run_until_parked();
        let hidden =
            cx.new(|cx| NodePeek::page(session.clone(), "Person".into(), "p1".into(), None, cx));
        cx.run_until_parked();
        session.update(cx, |session, cx| session.set_branch("fix".into(), cx));
        cx.run_until_parked();
        hidden.read_with(cx, |hidden, _| {
            assert!(hidden._node_read.is_none(), "no read sent");
            assert!(matches!(hidden.values, Loadable::NotLoaded));
            assert!(hidden.node_owed, "it reads when drawn");
        });
        // Drawn, it reads.
        let (shown, cx) = cx.add_window_view(move |_, cx| {
            NodePeek::page(session, "Person".into(), "p1".into(), None, cx)
        });
        cx.run_until_parked();
        shown.read_with(cx, |shown, _| {
            assert!(matches!(shown.values, Loadable::Loaded(_)));
        });
    }

    /// A text with markup is parsed once while it stands: a frame that finds it as drawn makes
    /// nothing, a changed text is parsed again, and one that reads as itself is a label.
    #[gpui::test]
    async fn a_texts_markdown_is_made_once_while_it_stands(cx: &mut TestAppContext) {
        let session = session(PEOPLE, cx);
        let (view, cx) = page(session, cx);
        view.update(cx, |view, cx| {
            let set = |view: &mut NodePeek, text: &str| {
                let Loadable::Loaded(values) = &mut view.values else {
                    panic!("the node is read");
                };
                values.insert("name".into(), json!(text));
            };
            let made = |view: &NodePeek| view.markdown.get("name").map(|(_, md)| md.entity_id());
            set(view, "**Ada** Lovelace");
            view.sync_markdown(cx);
            let first = made(view).expect("a markdown view");
            view.sync_markdown(cx);
            assert_eq!(made(view), Some(first), "the same text is not parsed again");
            set(view, "**Ada** King");
            view.sync_markdown(cx);
            assert!(
                made(view).is_some_and(|again| again != first),
                "a changed one is"
            );
            set(view, "Ada");
            view.sync_markdown(cx);
            assert_eq!(made(view), None);
            assert_eq!(view.plain_text.get("name").map(String::as_str), Some("Ada"));
            // A text gone (cleared) leaves nothing drawn for it, though every other is as drawn.
            let Loadable::Loaded(values) = &mut view.values else {
                panic!("the node is read");
            };
            values.insert("name".into(), Value::Null);
            view.sync_markdown(cx);
            assert!(view.plain_text.is_empty() && view.markdown.is_empty());
        });
    }

    /// Save… reads the value a slot shows where it is held, the value its size is: a file
    /// stored from a view behind its branch is read at the write's own commit, not at the view's,
    /// which holds the value it replaced. A value no commit the view knows holds (its table's
    /// own write past another writer's commit) is not offered to Save, which says why.
    #[gpui::test]
    async fn a_value_stored_from_a_view_behind_is_saved_from_where_it_is_held(
        cx: &mut TestAppContext,
    ) {
        const DOCS_AND_NOTES: &str = "node Person {\n  slug: String @key\n  name: String\n  doc: Blob?\n}\nnode Note {\n  slug: String @key\n  file: Blob?\n}\n";
        let (session, blobs) = crate::test_support::session_with_blobs(DOCS_AND_NOTES, cx);
        blobs
            .versioned
            .store(true, std::sync::atomic::Ordering::SeqCst);
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (view, cx) = cx.add_window_view({
            let session = session.clone();
            move |_, cx| NodePeek::page(session, "Person".into(), "p1".into(), None, cx)
        });
        cx.run_until_parked();
        // The view's own value, at c2, which it follows.
        let file = crate::test_support::upload_file("first.txt", b"first", None);
        view.update(cx, |view, cx| view.upload_blob("doc", file, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.reads_at(), Some("c2")));
        // Another writer's commit, c3, to another type; the view stays at c2, behind it.
        let note = BlobCell {
            edge: false,
            type_name: "Note".into(),
            id: "n1".into(),
            property: "file".into(),
        };
        session
            .update(cx, |session, cx| {
                session.blob_put_on("main", note, None, b"note".to_vec(), "Note".into(), cx)
            })
            .detach();
        cx.run_until_parked();
        view.update(cx, |view, _| {
            view.snapshot = Some("c2".into());
            view.newer = true;
        });
        // Stored from behind: "second" lands at c4, past c3.
        let file = crate::test_support::upload_file("second.txt", b"second", None);
        view.update(cx, |view, cx| view.upload_blob("doc", file, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.reads_at(), Some("c2"), "behind");
            assert_eq!(view.blob_size("doc"), Some(6));
            assert_eq!(view.save_refusal(0), None);
        });
        let target = std::env::temp_dir()
            .join(format!("studio-peek-save-{}", std::process::id()))
            .join("p1-doc.txt");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::remove_file(&target).ok();
        view.update(cx, |view, cx| view.save_blob(0, cx));
        cx.run_until_parked();
        cx.simulate_new_path_selection(|_| Some(target.clone()));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(
                matches!(&view.blobs[0].save, Some(SaveState::Saved(path)) if *path == target),
                "{:?}",
                view.blobs[0].save
            );
        });
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"second",
            "the value shown"
        );

        // A peek of a table whose own Blob write went past another writer's commit: the value
        // its cell shows is held at no commit the peek knows.
        let values: Map<String, Value> =
            serde_json::from_value(json!({ "@id": "p1", "slug": "p1", "name": "Ada" })).unwrap();
        let shown = blobs.get("Person/p1/doc").unwrap();
        let blobs_shown = TableBlobs {
            at: BlobsAt::Unknown,
            shown: HashMap::from([(
                "doc".to_owned(),
                BlobMeta::Managed {
                    size: shown.0.len() as u64,
                    etag: Some(shown.1),
                },
            )]),
        };
        let peek = cx.update(|_, cx| {
            cx.new(|cx| {
                NodePeek::from_table(
                    session.clone(),
                    PeekSubject::Node {
                        type_name: "Person".into(),
                        id: "p1".into(),
                    },
                    values,
                    Some("c2".into()),
                    HashMap::new(),
                    blobs_shown,
                    false,
                    cx,
                )
            })
        });
        cx.run_until_parked();
        peek.update(cx, |peek, cx| {
            assert_eq!(peek.blob_size("doc"), Some(6), "as the table shows it");
            assert_eq!(
                peek.save_refusal(0),
                Some("Studio knows no commit that holds this value. Refresh, then save it.")
            );
            peek.save_blob(0, cx);
            assert!(peek.blobs[0]._save.is_none(), "nothing is read");
        });
    }

    /// A file still being stored when the view moves to another branch goes on to the branch
    /// left: the new branch's slot does not show it as going, the view compares the new
    /// branch's head moves again, and the write's landing changes nothing there, not the mark
    /// of that branch's own write. Back on its branch, it holds a second write back.
    #[gpui::test]
    async fn a_blob_write_out_stays_with_the_branch_left(cx: &mut TestAppContext) {
        let (view, session, blobs, cx) = page_with_doc(Some(b"old"), None, cx);
        let (open, gate) = futures::channel::oneshot::channel();
        *blobs.hold.lock().unwrap() = Some(gate);
        let file = crate::test_support::upload_file("notes.txt", b"new notes", None);
        view.update(cx, |view, cx| view.upload_blob("doc", file, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert!(view.saving.on("main", "doc")));

        session.update(cx, |session, cx| session.set_branch("fix".into(), cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.branch, "fix");
            assert!(!view.saving.any_on("fix"), "nothing of fix is being saved");
            assert!(view.saving.on("main", "doc"), "main's write is still out");
            assert_eq!(view.blobs[0].edit, None);
        });

        // Back on main while its write is out, a second write of the slot is held back.
        session.update(cx, |session, cx| session.set_branch("main".into(), cx));
        cx.run_until_parked();
        view.update(cx, |view, cx| view.clear_blob("doc", cx));
        view.read_with(cx, |view, _| assert_eq!(view.blobs[0].edit, None));

        // On fix again, a write of fix's own goes to the slot.
        session.update(cx, |session, cx| session.set_branch("fix".into(), cx));
        cx.run_until_parked();
        view.update(cx, |view, _| {
            view.blobs[0].edit = Some(BlobEdit::Saving(BlobChange::Clear));
            view.saving.insert("fix", "doc");
        });
        open.send(()).ok();
        cx.run_until_parked();
        let writes = blobs.writes.lock().unwrap().clone();
        assert_eq!(writes.len(), 1, "the second write was held back");
        assert_eq!(writes[0].branch, "main");
        view.read_with(cx, |view, _| {
            assert!(
                matches!(view.blobs[0].edit, Some(BlobEdit::Saving(_))),
                "fix's own"
            );
            assert!(view.saving.on("fix", "doc") && !view.saving.any_on("main"));
        });
    }

    /// A file stored in a slot is bound to the ETag the slot read, sent once to the view's
    /// branch; its commit moves the view and the branch's head there, and the slot shows the
    /// new value. Clear sets it to null the same way.
    #[gpui::test]
    async fn a_file_stored_in_a_slot_is_bound_to_the_value_read(cx: &mut TestAppContext) {
        let (view, session, blobs, cx) = page_with_doc(Some(b"old"), None, cx);
        let read = blobs.get("Person/p1/doc").unwrap().1;
        view.read_with(cx, |view, _| {
            assert!(matches!(
                &view.blobs[0].meta,
                Loadable::Loaded(BlobMeta::Managed { size: 3, etag: Some(etag) }) if *etag == read
            ));
        });
        let file = crate::test_support::upload_file("notes.txt", b"new notes", None);
        view.update(cx, |view, cx| {
            view.upload_blob("doc", file, cx);
            assert!(matches!(view.blobs[0].edit, Some(BlobEdit::Saving(_))));
            assert!(view.saving.on("main", "doc"), "its dot shows");
        });
        cx.run_until_parked();
        let stored = blobs.get("Person/p1/doc").unwrap();
        assert_eq!(stored.0, b"new notes");
        {
            let writes = blobs.writes.lock().unwrap();
            assert_eq!(writes.len(), 1);
            assert_eq!(writes[0].method, "PUT");
            assert_eq!(writes[0].if_match.as_deref(), Some(read.as_str()));
            assert_eq!((writes[0].branch.as_str(), writes[0].bytes), ("main", 9));
        }
        view.read_with(cx, |view, _| {
            assert_eq!(view.snapshot.as_deref(), Some("c2"), "its own commit");
            assert!(!view.newer && view.saving.is_empty());
            assert_eq!(view.blobs[0].edit, None);
            assert!(matches!(
                &view.blobs[0].meta,
                Loadable::Loaded(BlobMeta::Managed { size: 9, etag: Some(etag) }) if *etag == stored.1
            ));
            assert!(matches!(
                &view.blobs[0].preview,
                Loadable::Loaded(Preview::Text { text, .. }) if text == "new notes"
            ));
        });
        session.read_with(cx, |session, _| {
            assert_eq!(session.head("main"), Some("c2"));
            let logged = session.outcomes.last().unwrap();
            assert_eq!(logged.description, "Upload notes.txt (9 B) to doc of p1");
            assert_eq!(
                logged.outcome,
                graph_store::OutcomeKind::Committed {
                    commit: Some("c2".into())
                }
            );
        });

        view.update(cx, |view, cx| view.clear_blob("doc", cx));
        cx.run_until_parked();
        assert_eq!(blobs.get("Person/p1/doc"), None);
        let writes = blobs.writes.lock().unwrap().clone();
        assert_eq!(writes[1].method, "DELETE");
        assert_eq!(
            writes[1].if_match.as_deref(),
            Some(stored.1.as_str()),
            "bound to the value it stored"
        );
        view.read_with(cx, |view, _| {
            assert_eq!(view.snapshot.as_deref(), Some("c3"));
            assert_eq!(view.blobs[0].meta, Loadable::Loaded(BlobMeta::Null));
        });
    }

    /// A file stored in a slot is bound as it is sent, after the file is read, to what the slot
    /// shows then. The table the peek came from saves another of the node's Blob values while
    /// the file is read: the peek follows it to its commit, where every ETag of the type has
    /// moved, and the upload carries the ETag read there and is stored. Bound when the file
    /// was chosen, it named the ETag read before and was refused as changed elsewhere, though
    /// the value it would replace had not changed.
    #[gpui::test]
    async fn a_slot_upload_is_bound_to_what_the_view_shows_as_it_is_sent(cx: &mut TestAppContext) {
        const TWO_BLOBS: &str =
            "node Person {\n  slug: String @key\n  name: String\n  doc: Blob?\n  scan: Blob?\n}\n";
        let (session, blobs) = crate::test_support::session_with_blobs(TWO_BLOBS, cx);
        blobs
            .versioned
            .store(true, std::sync::atomic::Ordering::SeqCst);
        blobs.set("Person/p1/scan", b"scan");
        blobs.set("Person/p1/doc", b"old");
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (view, cx) = cx.add_window_view({
            let session = session.clone();
            move |_, cx| NodePeek::page(session, "Person".into(), "p1".into(), None, cx)
        });
        cx.run_until_parked();
        let chosen = blobs.get("Person/p1/doc").unwrap().1;
        view.read_with(cx, |view, _| {
            assert_eq!(view.reads_at(), Some("c1"));
            assert_eq!(view.blob_size("doc"), Some(3));
        });

        // The table's save of p1's scan, out and then in at the server: c2, on c1.
        view.update(cx, |view, cx| {
            view.table_saves(HashSet::from(["scan".to_owned()]), cx)
        });
        let scan = BlobCell {
            edge: false,
            type_name: "Person".into(),
            id: "p1".into(),
            property: "scan".into(),
        };
        let saved = session.update(cx, |session, cx| {
            session.blob_put_on("main", scan, None, b"new scan".to_vec(), "scan".into(), cx)
        });
        cx.run_until_parked();
        saved.await;
        let moved = blobs.get("Person/p1/doc").unwrap().1;
        assert_ne!(moved, chosen, "the scan's write moved every ETag of Person");

        // The file is chosen, then the table's save reaches the peek while it is read.
        let file = crate::test_support::upload_file("bound-in-slot.txt", b"bound", None);
        view.update(cx, |view, cx| {
            view.upload_blob("doc", file, cx);
            let stored = blobs.get("Person/p1/scan").unwrap();
            let wrote = TableWrote {
                commit: "c2".into(),
                parent: Some("c1".into()),
                values: Vec::new(),
                blob: Some((
                    "scan".into(),
                    BlobMeta::Managed {
                        size: stored.0.len() as u64,
                        etag: Some(stored.1),
                    },
                )),
                type_name: "Person".into(),
                touched: HashSet::new(),
            };
            view.table_wrote(wrote, cx);
            view.table_saves(HashSet::new(), cx);
        });
        cx.run_until_parked();
        let writes = blobs.writes.lock().unwrap().clone();
        let sent = writes.last().unwrap();
        assert_eq!(sent.cell, "Person/p1/doc");
        assert_eq!(sent.if_match.as_deref(), Some(moved.as_str()));
        assert_eq!(blobs.get("Person/p1/doc").unwrap().0, b"bound");
        view.read_with(cx, |view, _| {
            let doc = view
                .blobs
                .iter()
                .find(|slot| slot.property == "doc")
                .unwrap();
            assert_eq!(doc.edit, None, "stored, not refused as changed elsewhere");
            assert_eq!(view.reads_at(), Some("c3"), "its own commit");
        });
    }

    /// A value changed elsewhere since the slot read it refuses the write: nothing is
    /// overwritten, the slot says what the cell holds now, and offers the file again, bound to
    /// that. Nothing is sent again by itself.
    #[gpui::test]
    async fn a_value_changed_elsewhere_is_offered_again_over_the_new_one(cx: &mut TestAppContext) {
        let (view, _session, blobs, cx) = page_with_doc(Some(b"old"), None, cx);
        // Another writer's value, of another size.
        let theirs = blobs.set("Person/p1/doc", b"their value");
        let file = crate::test_support::upload_file("mine.txt", b"mine", None);
        view.update(cx, |view, cx| view.upload_blob("doc", file, cx));
        cx.run_until_parked();
        assert_eq!(blobs.get("Person/p1/doc").unwrap().0, b"their value");
        assert_eq!(blobs.writes.lock().unwrap().len(), 1, "sent once");
        view.read_with(cx, |view, _| {
            let Some(BlobEdit::Failed { notice, again }) = &view.blobs[0].edit else {
                panic!("a conflict: {:?}", view.blobs[0].edit);
            };
            assert_eq!(notice.tone, Tone::Conflict);
            assert_eq!(
                notice.message,
                "Not uploaded: doc changed since it was read, and now holds a value of 11 B."
            );
            let (_, now) = again.as_ref().expect("offered again");
            assert_eq!(
                *now,
                BlobMeta::Managed {
                    size: 11,
                    etag: Some(theirs.clone())
                }
            );
            assert_eq!(view.snapshot.as_deref(), Some("c1"), "nothing landed");
        });
        view.update(cx, |view, cx| view.send_blob_again(0, cx));
        cx.run_until_parked();
        let writes = blobs.writes.lock().unwrap().clone();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[1].if_match.as_deref(), Some(theirs.as_str()));
        assert_eq!(blobs.get("Person/p1/doc").unwrap().0, b"mine");
        view.read_with(cx, |view, _| assert_eq!(view.blobs[0].edit, None));
    }

    /// A file over 32 MiB is refused before anything is sent; a page held at a commit stores
    /// nothing, and says why; a lost answer is unknown, settled by History, never sent again.
    #[gpui::test]
    async fn uploads_the_view_cannot_send_say_why_and_send_nothing(cx: &mut TestAppContext) {
        let (view, _session, blobs, cx) = page_with_doc(None, None, cx);
        let large = crate::test_support::upload_file(
            "huge.bin",
            b"",
            Some(graph_client::api::BLOB_PUT_MAX + 1),
        );
        view.update(cx, |view, cx| view.upload_blob("doc", large, cx));
        cx.run_until_parked();
        assert!(blobs.writes.lock().unwrap().is_empty());
        view.read_with(cx, |view, _| {
            let Some(BlobEdit::Failed {
                notice,
                again: None,
            }) = &view.blobs[0].edit
            else {
                panic!("refused: {:?}", view.blobs[0].edit);
            };
            assert_eq!(notice.tone, Tone::Invalid);
            assert!(
                notice.message.contains("at most 32 MiB"),
                "{}",
                notice.message
            );
        });

        blobs
            .lose_answers
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let file = crate::test_support::upload_file("lost.txt", b"lost", None);
        view.update(cx, |view, cx| view.upload_blob("doc", file, cx));
        cx.run_until_parked();
        assert_eq!(blobs.writes.lock().unwrap().len(), 1, "sent once");
        assert_eq!(
            blobs.writes.lock().unwrap()[0].if_match,
            None,
            "a null cell has no validator"
        );
        view.read_with(cx, |view, _| {
            let Some(BlobEdit::Failed {
                notice,
                again: None,
            }) = &view.blobs[0].edit
            else {
                panic!("unknown: {:?}", view.blobs[0].edit);
            };
            assert_eq!(notice.tone, Tone::Unknown);
            assert!(
                notice.message.contains("Check History"),
                "{}",
                notice.message
            );
        });
    }

    #[gpui::test]
    async fn a_page_held_at_a_commit_stores_no_file(cx: &mut TestAppContext) {
        let (view, _session, blobs, cx) = page_with_doc(Some(b"old"), Some("c1"), cx);
        let file = crate::test_support::upload_file("held.txt", b"held", None);
        view.update(cx, |view, cx| {
            assert!(view.blob_write_refusal("doc", cx).is_some());
            view.upload_blob("doc", file, cx);
            view.clear_blob("doc", cx);
        });
        cx.run_until_parked();
        assert!(blobs.writes.lock().unwrap().is_empty());
        view.read_with(cx, |view, _| {
            let Some(BlobEdit::Failed { notice, .. }) = &view.blobs[0].edit else {
                panic!("refused: {:?}", view.blobs[0].edit);
            };
            assert!(
                notice.message.starts_with("Read-only: this shows commit"),
                "{}",
                notice.message
            );
        });
    }

    /// A page on a branch the access route says the credential may not change edits nothing
    /// there: a field does not open, and a file stored in a slot is refused before it is sent,
    /// saying what the credential lacks.
    #[gpui::test]
    async fn a_page_on_a_branch_the_credential_may_not_change_sends_nothing(
        cx: &mut TestAppContext,
    ) {
        let (session, sent, blobs) = crate::test_support::session_with_access(
            DOCS,
            crate::test_support::read_only_access(),
            cx,
        );
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (view, cx) = cx.add_window_view(move |_, cx| {
            NodePeek::page(session, "Person".into(), "p1".into(), None, cx)
        });
        cx.run_until_parked();
        let refused = "Your credential may not change main";
        let file = crate::test_support::upload_file("refused.txt", b"new", None);
        view.update_in(cx, |view, window, cx| {
            assert_eq!(view.change_refusal(cx).as_deref(), Some(refused));
            let name = view.property_named("name").cloned().unwrap();
            assert!(!view.edits_property(&name, cx));
            view.start_edit(name, window, cx);
            assert!(view.edits.is_empty(), "the field does not open");
            assert_eq!(view.blob_write_refusal("doc", cx).as_deref(), Some(refused));
            view.upload_blob("doc", file, cx);
        });
        cx.run_until_parked();
        assert!(sent.lock().unwrap().is_empty(), "nothing was sent");
        assert!(blobs.writes.lock().unwrap().is_empty());
        view.read_with(cx, |view, _| {
            assert!(matches!(
                &view.blobs[0].edit,
                Some(BlobEdit::Failed { notice, .. }) if notice.message == refused
            ));
        });
    }
}

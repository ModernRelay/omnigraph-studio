//! One type's table (spec 004): keyset pages pinned to one commit, engine-side filters and
//! counts, a cell cursor, value-bound cell edits with their three outcomes, and a peek.
//!
//! The grid is Zed's `ui::Table` (`crates/ui/src/components/data_table.rs` at v1.26.2) in the
//! configuration Zed's `tabular_data_preview` uses: resizable columns, the first column pinned,
//! rows virtualized by `uniform_list`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use collections::IndexSet;
use editor::{Editor, EditorEvent};
use gpui::{
    Anchor, AnyView, App, ClickEvent, ClipboardItem, Context, DragMoveEvent, ElementId, Entity,
    EventEmitter, ExternalPaths, FocusHandle, Focusable, KeyContext, MouseButton, MouseDownEvent,
    Pixels, Render, ScrollStrategy, SharedString, StyleRefinement, Subscription, Task, WeakEntity,
    Window, anchored, deferred, point,
};
use gq_language::input::same_value;
use gq_language::{
    BuildError, BuiltQuery, Filter, FilterOp, PageStart, Property, RelationshipGroup, ScalarKind,
    SchemaModel, Sort, builder,
};
use graph_client::api::ReadOutput;
use graph_client::{BlobCell, BlobMeta, HistoricalGap, PropertyThen, ReadError};
use graph_store::scope::{ReadScope, ScopeSince};
use graph_store::{DraftEntry, DraftKey, GraphSession, Loadable, SessionEvent};
use serde_json::Value;
use studio_actions::blob::{ChooseFile, ClearValue};
use studio_actions::table::{
    AddFilter, Cancel, ClearFilters, ConfirmEdit, CopyCell, DeleteRow, EditCell, ExploreRow,
    FillColumn, FilterBySelection, FilterInWords, InsertRow, MoveDown, MoveLeft, MoveRight, MoveUp,
    OpenRowPage, PeekRow, RefreshTable, SaveDrafts, SelectNextSuggestion, SelectPreviousSuggestion,
    StageEdit,
};
use studio_ui::ds::Button;
use studio_ui::ds::space;
use studio_ui::menu::{ContextMenu, ContextMenuEntry};
use studio_ui::plural;
use studio_ui::{ColumnDrag, ColumnResize, GridPlace, ROW_HEIGHT, columns_in_view, grid_places};
use studio_workspace::models::{Error as ModelError, Models, Request as ModelRequest};
use studio_workspace::{Item, ItemEvent, ItemStatus, KeyHint, SavedItem, ToastKind, Workspace};
use ui::prelude::*;
use ui::{
    ColumnWidthConfig, PopoverMenu, PopoverMenuHandle, Table, TableInteractionState, TintColor,
    Tooltip,
};

use crate::blob_write::{self, Binding, BlobChange, BlobEdit, BlobWritten};
use crate::dialogs::{
    DeleteRowModal, DeleteTarget, InsertEdgeModal, InsertRowModal, SaveViewModal,
};
use crate::fill;
use crate::grouping::{GROUP_LIMIT, Grouping, Line, Lines, SECTION_FIRST_PAGE};
use crate::model::{
    ColumnLayout, ColumnRole, ColumnSpec, Notice, Paging, RELATION_CHIPS_SHOWN,
    RELATION_SAMPLE_BUDGET, Row, SaveResult, TableKind, Tone, changed_elsewhere, chips_that_fit,
    count_of, display_value, edge_columns, edge_rows, edit_text, edited, field_rows, filter_parts,
    insert_position, node_columns, node_rows, parse_filter_list, parse_filter_value, parse_input,
    preset_range, published, reads_as_markdown, relation_sample, row_matches,
};
use crate::offers::Offers;
use crate::peek::{
    KnownRelation, NEIGHBOUR_LIMIT, Neighbour, NodePeek, PeekSubject, TableBlobs, TableWrote,
};
use crate::suggest;
use crate::words;
use crate::write::{self, Bound};

/// Rows per neighbour read when a relationship column comes into view: a page's worth, as the
/// page reads do.
const RELATION_READ_CHUNK: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
}

/// A cell whose edit is not settled.
#[derive(Debug, Clone)]
enum CellState {
    /// The save is out; the cell shows the value sent with an amber dot. `stored` is what the
    /// row held at the table's commit before it, which a peek of the row shows meanwhile.
    Saving { stored: Value },
    /// An unsaved value: the session's draft for this property (spec 014), which every table,
    /// page and peek of the branch shares, with why its last save did not land. Reopening the
    /// cell restores the text and the notice.
    Draft {
        text: String,
        notice: Option<Notice>,
    },
}

struct Editing {
    row_id: String,
    column: String,
    /// The value the edit is bound to: the cell's draft's, the value it was typed over, else
    /// what the cell showed.
    old: Value,
    notice: Option<Notice>,
}

struct FilterDraft {
    property: String,
    op: FilterOp,
    error: Option<String>,
    /// The applied filter this draft edits, taken out of the bar while it is open: put back
    /// when the draft is cancelled, replaced when it is applied.
    replacing: Option<Filter>,
    /// What the value field offers for its text, the one ↵ picks, and the node search under way.
    offers: Offers,
    /// The text `offers` answers; while it differs from the field, a node search is out.
    answers: Option<String>,
    suggest_generation: u64,
    _suggest_task: Option<Task<()>>,
}

/// The sentence field's state (spec 019): idle, with what the last answer could not use;
/// calling; or failed, with why.
enum WordsState {
    Typing(Option<String>),
    Running { _call: Task<()> },
    Failed(String),
}

/// A row to fill: its id, what its target cell shows, and its source cells by column title.
type FillRow = (String, Value, Vec<(String, Value)>);

/// What one row's fill came to.
enum Outcome {
    Filled,
    Unsure,
    Skipped,
}

/// A column fill (spec 019): the target property, the source columns, and how far it is.
struct FillState {
    target: String,
    sources: Vec<String>,
    phase: FillPhase,
}

enum FillPhase {
    Typing,
    Running {
        _call: Task<()>,
        done: usize,
        total: usize,
        filled: usize,
        unsure: usize,
        skipped: usize,
    },
    Done {
        filled: usize,
        unsure: usize,
        skipped: usize,
        failed: Option<String>,
    },
}

/// Where a filter's value field finds what to offer.
enum SuggestSource {
    Values(Vec<String>),
    /// Nodes of this type, searched on the server.
    Nodes(String),
}

/// The table's drafts as a whole: whether they are being saved, what came of the last try,
/// and whether Discard is waiting for its second click.
#[derive(Default)]
struct DraftsBar {
    saving: bool,
    notice: Option<Notice>,
    confirm_discard: bool,
}

/// How one drawn line of the grid looks.
#[derive(Debug, Clone, Copy, Default)]
struct RowLook {
    picked: bool,
    dimmed: bool,
    cursor: bool,
    header: bool,
}

/// How long scrolling must pause before the drawn rows' Blob cells are read.
const BLOB_READ_AFTER: Duration = Duration::from_millis(120);
/// How many Blob cells a table reads at once: the reads share the connection's few slots with
/// the table's pages, relationships and counts, and with every other view's reads.
const BLOB_READS_AT_ONCE: usize = 4;

/// A cell's save as sent: the value it was bound to, the one written, the text typed, what the
/// row showed before the new value was put in it, and the cell's draft as the session held it
/// then, which the save settles only while it still is.
struct SavedCell {
    row_id: String,
    column: String,
    old: Value,
    new: Value,
    text: String,
    shown: Value,
    sent: Option<DraftEntry>,
}

/// One draft being saved: the cell, the value it was typed over (the save is bound to it), the
/// new value, the text to put back if the save does not land, and what the row showed before
/// the new value was put in it.
#[derive(Debug, Clone)]
struct DraftEdit {
    row_id: String,
    column: String,
    old: Value,
    new: Value,
    text: String,
    shown: Value,
}

impl DraftEdit {
    /// The draft as the batch sent it, which was the session's then.
    fn sent(&self) -> DraftEntry {
        DraftEntry {
            old: self.old.clone(),
            text: self.text.clone(),
        }
    }
}

/// The table's own inserts and deletes made on commits past its pin, which a count read at the
/// pin does not hold: the rows they added (negative for removed), and those of them through
/// `filters`, the filters they were counted under.
#[derive(Debug)]
struct CountShift {
    pin: String,
    filters: Vec<Filter>,
    rows: i64,
    matching: i64,
}

/// A Blob write as the table sent it: the cell, the change, and the commit its validator was
/// read at, where the values the cells showed held ([`Paging::blobs_at`]).
struct BlobSent {
    key: (String, String),
    change: BlobChange,
    bound_at: Option<String>,
}

/// A grouped section's read under way: how many rows it asks for, which a read dropped away
/// from the view asks for again when its line is drawn, and the read itself.
struct SectionRead {
    rows: u32,
    _task: Task<()>,
}

/// A bulk set: the property, what came of the last try, and the rows picked when the one out
/// was sent. Clearing the pick leaves a set out as it is: its outcome is shown, and a set that
/// landed clears the pick, only while those rows are the ones picked.
#[derive(Default)]
struct BulkDraft {
    property: Option<String>,
    notice: Option<Notice>,
    sending: Option<IndexSet<String>>,
}

impl BulkDraft {
    fn saving(&self) -> bool {
        self.sending.is_some()
    }
}

/// The hover group of a grid row, for the actions its cells show under the pointer.
const ROW_GROUP: &str = "table-row";

/// Where the key column's checkbox sits: the page's gutter, as the toolbar's title does,
/// since the grid runs to the document's edges. The other cells keep a cell's inset.
const KEY_INSET: f32 = space::EDGE;

/// Where the key column's text starts: after its checkbox, as far from the box as the box is
/// from the page's edge, so the box sits evenly between them and the text does not move when a
/// row's checkbox shows.
const KEY_TEXT_INSET: f32 = KEY_INSET + studio_ui::ds::size::CHECK + KEY_INSET;

use crate::peek::property_icon;

/// One row's neighbours through one relationship: how many, and the first few by id (as many
/// as the peek shows; the cell shows `RELATION_CHIPS_SHOWN`). `whole` when the row's whole list
/// was read, so these are its first ones and the peek needs no read.
struct Relation {
    count: Result<i64, String>,
    neighbours: NeighbourList,
    whole: bool,
}

/// Neighbours by id, each with its label when the other type has one.
type NeighbourList = Vec<(String, Option<String>)>;

/// A `page_neighbours` answer by row: how many neighbours it lists for each, and the first
/// [`NEIGHBOUR_LIMIT`] of them, by id.
struct NeighbourLists(HashMap<String, (i64, NeighbourList)>);

impl NeighbourLists {
    fn read(rows: &Value) -> Self {
        let mut lists: HashMap<String, (i64, NeighbourList)> = HashMap::new();
        for (id, other, label) in relation_samples(rows) {
            let (listed, first) = lists.entry(id).or_default();
            *listed += 1;
            if first.len() < NEIGHBOUR_LIMIT as usize {
                first.push((other, label));
            }
        }
        Self(lists)
    }

    /// Rows `ids` split by whether the answer lists every neighbour `degrees` counts for them:
    /// those it does with their first neighbours, and the rest, in the order given. A list the
    /// read's limit cut short, or none, is not the row's.
    fn split(
        mut self,
        ids: Vec<String>,
        degrees: &HashMap<String, i64>,
    ) -> (Vec<(String, NeighbourList)>, Vec<String>) {
        let mut whole = Vec::new();
        let mut rest = Vec::new();
        for id in ids {
            let degree = degrees.get(&id).copied().unwrap_or(0);
            match self.0.remove(&id) {
                Some((listed, first)) if listed == degree => whole.push((id, first)),
                _ => rest.push(id),
            }
        }
        (whole, rest)
    }
}

/// Values by row id and column, looked up without copying either key: every cell drawn asks
/// for its own.
struct CellMap<V>(HashMap<String, HashMap<String, V>>);

impl<V> Default for CellMap<V> {
    fn default() -> Self {
        Self(HashMap::new())
    }
}

impl<V> CellMap<V> {
    fn get(&self, row: &str, column: &str) -> Option<&V> {
        self.0.get(row)?.get(column)
    }

    fn get_mut(&mut self, row: &str, column: &str) -> Option<&mut V> {
        self.0.get_mut(row)?.get_mut(column)
    }

    fn contains(&self, row: &str, column: &str) -> bool {
        self.get(row, column).is_some()
    }

    fn insert(&mut self, row: String, column: String, value: V) {
        self.0.entry(row).or_default().insert(column, value);
    }

    fn remove(&mut self, row: &str, column: &str) -> Option<V> {
        let cells = self.0.get_mut(row)?;
        let removed = cells.remove(column);
        if cells.is_empty() {
            self.0.remove(row);
        }
        removed
    }

    fn clear(&mut self) {
        self.0.clear();
    }
}

/// What a cell shows, made the first time it is drawn ([`TypeTable::cell_text`]): its text,
/// and a date's or time's exact value, its tooltip. A date or a time reads relative to now, so
/// it is made again in a later minute than the one it was made in (`minute`).
#[derive(Clone)]
struct CellText {
    shown: SharedString,
    exact: Option<SharedString>,
    minute: Option<i64>,
}

/// `(id, other, label)` from a `page_neighbours` result.
fn relation_samples(rows: &Value) -> Vec<(String, String, Option<String>)> {
    rows.as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            Some((
                row.get("id")?.as_str()?.to_owned(),
                row.get("other")?.as_str()?.to_owned(),
                row.get("label").and_then(Value::as_str).map(str::to_owned),
            ))
        })
        .collect()
}

/// What the Group menu offers for a table's columns: its submenus, and every choice by key and
/// how it reads.
#[derive(Default)]
struct GroupingChoices {
    menus: Rc<Vec<GroupMenu>>,
    choices: Rc<Vec<(String, String)>>,
}

/// A submenu of the Group menu (see [`TypeTable::group_menus`]).
#[derive(Clone)]
struct GroupMenu {
    /// The submenu's entry: `worksAt Company` beside its arrow, `to Company`.
    label: String,
    /// What names the choices it holds elsewhere: `worksAt → Company`, `to Company`.
    prefix: String,
    icon: IconName,
    /// By each neighbour: its key and how the choice reads.
    neighbour: Option<(String, String)>,
    /// The type at the other end.
    other_type: String,
    /// By a property of it: the key and the property's name.
    properties: Vec<(String, String)>,
}

/// What a table opens showing: its filters, order, grouping and columns. The default shows
/// every row in `@id` order, every column.
#[derive(Default)]
pub struct Opening {
    pub filters: Vec<Filter>,
    pub sort: Option<Sort>,
    /// The grouping's key: a property, a relationship or a property of a neighbour.
    pub group_by: Option<String>,
    pub layout: ColumnLayout,
}

/// A relationship's direction, as filters and the Group menu show it.
fn direction_icon(direction: gq_language::Direction) -> IconName {
    match direction {
        gq_language::Direction::Outgoing => IconName::ArrowRight,
        gq_language::Direction::Incoming => IconName::ArrowLeft,
    }
}

pub struct TypeTable {
    workspace: WeakEntity<Workspace>,
    session: Entity<GraphSession>,
    type_name: String,
    branch: String,
    /// The commit this table reads, read-only; `None` follows the branch head.
    at_commit: Option<String>,
    kind: TableKind,
    schema: Option<Arc<SchemaModel>>,
    missing: bool,
    /// Refresh asking after the branch's head before it reads again: it wants the newest,
    /// which the session may not hold yet (a write refused because the branch moved is the
    /// case).
    _refresh: Option<Task<()>>,
    /// Every column the schema gives this type, in schema order.
    all_columns: Vec<ColumnSpec>,
    /// Which of them show, and in what order.
    layout: ColumnLayout,
    /// The columns shown, in order: what the grid, the cursor and editing address.
    columns: Vec<ColumnSpec>,
    /// A column header's right-click menu, where it was opened.
    header_menu: Option<(Entity<ContextMenu>, gpui::Point<Pixels>, Subscription)>,
    rows: Vec<Row>,
    /// Rows made here and shown out of their read order (first in a sorted table, or in their
    /// section), by id: the page that reaches one leaves it out, so it shows once.
    placed_here: HashSet<String>,
    paging: Paging,
    first_page: Loadable<()>,
    loading_more: bool,
    more_error: Option<String>,
    count: Loadable<i64>,
    total: Loadable<i64>,
    filters: Vec<Filter>,
    /// The order: a property then `@id`, values first and empty values last; `None` is `@id`.
    sort: Option<Sort>,
    /// Rows grouped by one property: sections by value, each read on its own.
    grouping: Option<Grouping>,
    /// The grouped grid's lines as this frame drew them.
    frame_lines: Option<Rc<Lines>>,
    /// How the lines drawn this frame look, filled by the row callback for the row styling.
    frame_looks: Rc<RefCell<HashMap<usize, RowLook>>>,
    /// The places this frame's lines draw.
    frame_places: Rc<[GridPlace]>,
    /// The columns the last drawn rows drew.
    #[cfg(test)]
    frame_drawn: Vec<bool>,
    /// A column being dragged wider or narrower: its key, and where the pointer and the column's
    /// width were when the drag began.
    resizing: Option<ColumnResize<String>>,
    /// The time relative dates are read against, taken once a frame rather than once a cell
    /// (reading the local time zone is not free), and its minute since the epoch.
    frame_now: chrono::DateTime<chrono::Local>,
    frame_minute: i64,
    /// The grid as a view of its own, drawn again whenever the table notifies.
    grid: Entity<TableGrid>,
    /// Where the grid drew the cell being edited; the cell's editor is drawn over it from
    /// outside the grid, so a keystroke does not draw the grid again.
    edit_slot: studio_ui::OverlaySlot,
    /// The axis the wheel gesture over the rows is held to ([`sideways_scroll`]).
    wheel_gesture: Rc<RefCell<gpui::OngoingScroll>>,
    /// The lines whose Blob cells were last asked for, and the pause before they are read.
    blob_lines: Option<Range<usize>>,
    _blob_wait: Option<Task<()>>,
    /// The Blob properties of the columns the last frame drew: only these cells are read, so a
    /// Blob column scrolled off sideways reads nothing.
    blob_columns: Vec<String>,
    /// Labels of nodes picked for "related to" filters, so their pills read by name.
    node_labels: HashMap<String, String>,
    /// The filter being written's property and operator menus; the offered values hide while
    /// either is open.
    filter_menus: [PopoverMenuHandle<ContextMenu>; 2],
    /// The toolbar's Views, Columns and Group menus, whose buttons keep their tooltips quiet
    /// while open.
    /// Views, Columns, Group and Sort, in that order.
    toolbar_menus: [PopoverMenuHandle<ContextMenu>; 4],
    filter_draft: Option<FilterDraft>,
    filter_editor: Entity<Editor>,
    /// Filter in words (spec 019): the toolbar's sentence field, what its call is doing, and
    /// the filters its last answer made, which the next answer replaces.
    words_editor: Entity<Editor>,
    words: WordsState,
    words_made: Vec<Filter>,
    /// Fill a column (spec 019): the instruction field and the fill under way, in the bulk bar.
    fill_editor: Entity<Editor>,
    fill: Option<FillState>,
    /// Rows picked for a bulk set (⌘-click, ⇧-click), by id, and the row ⇧ extends from.
    /// The picked rows' ids, in the order picked; checked for every row drawn.
    selection: IndexSet<String>,
    selection_anchor: Option<usize>,
    bulk: BulkDraft,
    bulk_editor: Entity<Editor>,
    drafts: DraftsBar,
    cursor: Option<Cursor>,
    editing: Option<Editing>,
    /// The open edit's editor, made for it ([`Self::fresh_cell_editor`]), and its events.
    cell_editor: Entity<Editor>,
    _cell_editor_events: Subscription,
    /// Cells with a save out, and the session's drafts for this type as of its last change:
    /// a view rebuilt from the session ([`TypeTable::sync_drafts`]), never written on its own.
    cells: HashMap<(String, String), CellState>,
    /// Why a draft's last save did not land, by cell.
    draft_notices: HashMap<(String, String), Notice>,
    /// The session's drafts changed and the cells are rebuilt once the change is done: drafts
    /// settled together announce themselves one by one.
    drafts_due: bool,
    /// What each Blob cell drawn so far holds, by row id and property, read at the page's
    /// commit; and the reads under way.
    blobs: CellMap<Loadable<BlobMeta>>,
    blob_tasks: HashMap<(String, String), Task<()>>,
    /// The Blob cells asked for and not read yet, the first drawn first: at most
    /// [`BLOB_READS_AT_ONCE`] are out at a time ([`Self::read_queued_blobs`]).
    blob_queue: std::collections::VecDeque<(String, String)>,
    /// Files dropped on Blob cells being stored, by row id and property, and why one that did
    /// not land did not.
    blob_edits: HashMap<(String, String), BlobEdit>,
    /// Each row's neighbours in the relationship columns, by row id and relationship key;
    /// read for each page at the page's commit.
    relations: CellMap<Relation>,
    /// Relationship reads out, each taking itself out when done; a reload drops them all.
    relation_tasks: HashMap<u64, Task<()>>,
    next_relation_task: u64,
    /// The relationship columns rows drew in the last frame, by key. Only these read their
    /// neighbours: a type with dozens of relationships reads the few on screen (spec 015).
    relations_drawn: HashSet<String>,
    /// Rows and relationship keys whose read is out or done, so a column scrolled back into
    /// view is not asked again.
    relations_asked: HashSet<(String, String)>,
    /// The lines the last frame drew, first to last: a relationship column reads the rows of
    /// the lines near them, not every row read so far. Ungrouped, a line is a row; grouped, a
    /// folded section's rows are on no line ([`Self::rows_of_lines`]).
    lines_drawn: Range<usize>,
    /// The lines drawn whose relationships were last asked for. A row put in or taken out
    /// empties it, and `blob_lines` too: the same places hold other rows then.
    relation_lines: Option<Range<usize>>,
    /// The lines the frame being drawn asked for last: the list also lays one line out twice a
    /// frame to measure it, so they are acted on once the frame is out ([`Self::drawn`]).
    frame_line_range: Option<Range<usize>>,
    frame_due: bool,
    /// Properties the commit shown does not hold under their current name, and what they were
    /// then. Rows are read without them.
    absent: HashMap<String, PropertyThen>,
    /// What the Group menu offers for these columns: its submenus and every choice.
    grouping_choices: GroupingChoices,
    /// What each cell shows, by row id and column, made the first time it is drawn: a text
    /// value is read as markdown to show its first line, too slow to do for every cell every
    /// frame. A cell set, or rows read again, are made again.
    cell_text: RefCell<HashMap<String, HashMap<String, CellText>>>,
    peek: Option<(String, Entity<NodePeek>)>,
    newer: bool,
    /// Writes out for this table: its own saves and the dialogs' inserts and deletes. Each moves
    /// the pin itself when it lands, so a head the session sees meanwhile waits for them.
    saves_in_flight: usize,
    /// The session saw the head move while a save was out; compared once they are all back.
    head_moved_while_saving: bool,
    /// What the table shows, as read (spec 015): a head move that changed none of it moves the
    /// pin without reading again. `None` for a table held at a commit.
    scope: Option<ReadScope>,
    /// The head moved and the session is reading what changed.
    awaiting_changes: bool,
    focus_handle: FocusHandle,
    table_state: Entity<TableInteractionState>,
    page_task: Option<Task<()>>,
    /// Each grouped section's read under way, by the section's index: a reload drops them.
    section_tasks: HashMap<usize, SectionRead>,
    /// Section reads dropped away from the view ([`Self::drop_section_reads_away_from`]), by
    /// the section's index, with how many rows each asked for: asked for again when the
    /// section's read line is drawn, a first read and a "Show more" alike.
    dropped_sections: HashMap<usize, u32>,
    /// How many of the table's own writes moved its pin ([`Self::follow_own_commit`]). A read
    /// out when one landed answers for the commit before it and is read again; the pin carried
    /// forward to a head that changed nothing the table reads ([`ScopeSince::Unchanged`])
    /// leaves a read out as valid as it was.
    own_writes: u64,
    count_task: Option<Task<()>>,
    total_task: Option<Task<()>>,
    /// What the table's own writes past its pin change in its counts: a count read at the pin
    /// lands with it added, whether it was out when they landed or is read after.
    count_shift: Option<CountShift>,
    /// The rows are read when the table is next drawn ([`Self::owe_reload`]): a table behind
    /// another tab reads nothing when it is restored, nor when the branch or the schema moves.
    rows_owed: bool,
    _subscriptions: Vec<Subscription>,
    /// How often the table drew, for the test that moving through the offers draws it and
    /// leaves the grid alone.
    #[cfg(test)]
    renders: usize,
    /// How many reads landed only to be sent again ([`Self::own_write_since`]). A read sent
    /// again at a commit that changed nothing it depends on is answered from the session's
    /// kept answers without a request, so the tests count it here.
    #[cfg(test)]
    reads_sent_again: usize,
}

impl EventEmitter<ItemEvent> for TypeTable {}

impl Focusable for TypeTable {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for TypeTable {
    fn thing(&self, _cx: &App) -> Option<studio_actions::Thing> {
        Some(studio_actions::Thing::Type(self.type_name.clone()))
    }

    /// The grid is drawn cached on its own (`TableGrid`).
    fn caches_its_parts(&self) -> bool {
        true
    }

    fn tab_title(&self, _cx: &App) -> SharedString {
        match &self.at_commit {
            Some(commit) => format!(
                "{} @ {}",
                self.type_name,
                studio_ui::short_commit_id(commit)
            )
            .into(),
            None => self.type_name.clone().into(),
        }
    }

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(IconName::Table)
    }

    fn dedupe_key(&self, _cx: &App) -> Option<SharedString> {
        Some(crate::table_key(&self.type_name, self.at_commit.as_deref()).into())
    }

    fn serialize(&self, _cx: &App) -> Option<SavedItem> {
        Some(SavedItem {
            kind: "type_table".into(),
            data: serde_json::json!({
                "type_name": self.type_name,
                "at_commit": self.at_commit,
                "sort": self.sort.as_ref().map(|sort| serde_json::json!({
                    "property": sort.property,
                    "descending": sort.descending,
                })),
                "group_by": self.grouping.as_ref().map(|g| &g.property),
                "hidden": self.layout.hidden,
                "order": self.layout.order,
                "widths": self.layout.widths,
            }),
        })
    }

    /// A save out, a draft, or an edit open that leaving its cell would keep as a draft.
    fn is_dirty(&self, cx: &App) -> bool {
        self.saves_in_flight > 0
            || self
                .cells
                .values()
                .any(|state| matches!(state, CellState::Draft { .. }))
            || self.editing.as_ref().is_some_and(|editing| {
                let text = self.cell_editor.read(cx).text(cx);
                self.edit_keeps(editing, &text)
            })
    }

    fn is_stale(&self, _cx: &App) -> bool {
        self.newer
    }

    fn status(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Option<ItemStatus> {
        Some(self.status_parts(cx))
    }
}

impl TypeTable {
    #[cfg(test)]
    pub fn new(
        workspace: WeakEntity<Workspace>,
        session: Entity<GraphSession>,
        type_name: String,
        at_commit: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::opening(
            workspace,
            session,
            type_name,
            at_commit,
            Opening::default(),
            window,
            cx,
        )
    }

    /// A table that opens filtered, ordered, grouped and with its columns as `opening` says:
    /// its first read is the one they make, not a page of every row asked for and dropped.
    pub fn opening(
        workspace: WeakEntity<Workspace>,
        session: Entity<GraphSession>,
        type_name: String,
        at_commit: Option<String>,
        opening: Opening,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let cell_editor = cx.new(|cx| Editor::single_line(window, cx));
        let cell_editor_events = cx.subscribe_in(&cell_editor, window, Self::on_cell_editor_event);
        let filter_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Value", window, cx);
            editor.set_text_style_refinement(studio_ui::ds::field_text(cx));
            editor
        });
        let words_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter in words…", window, cx);
            editor.set_text_style_refinement(studio_ui::ds::field_text(cx));
            editor
        });
        let fill_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(
                "How to fill it, for example the city from the address",
                window,
                cx,
            );
            editor.set_text_style_refinement(studio_ui::ds::field_text(cx));
            editor
        });
        let bulk_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("New value", window, cx);
            editor.set_text_style_refinement(studio_ui::ds::field_text(cx));
            editor
        });
        let grid = TableGrid::new(cx);
        let subscriptions = vec![
            cx.subscribe_in(&session, window, Self::on_session_event),
            cx.subscribe(
                &filter_editor,
                |this, _, event: &EditorEvent, cx| match event {
                    // No `notify`: the table draws again because the field it holds changed,
                    // and the grid, which does not show the field, keeps its last frame.
                    EditorEvent::BufferEdited => {
                        if let Some(draft) = &mut this.filter_draft {
                            draft.error = None;
                        }
                        this.refresh_suggestions(cx);
                    }
                    // The offered values show only while the field has focus.
                    EditorEvent::Focused | EditorEvent::Blurred => this.redraw_offers(cx),
                    _ => {}
                },
            ),
        ];
        let branch = session.read(cx).branch.clone();
        let table_state = cx.new(|cx| TableInteractionState::new(cx));
        let scope = at_commit.is_none().then(|| new_scope(&session, cx));
        let mut this = Self {
            workspace,
            session,
            type_name,
            branch,
            at_commit,
            kind: TableKind::Node,
            schema: None,
            missing: false,
            _refresh: None,
            all_columns: Vec::new(),
            layout: opening.layout,
            columns: Vec::new(),
            header_menu: None,
            rows: Vec::new(),
            placed_here: HashSet::new(),
            paging: Paging::default(),
            first_page: Loadable::NotLoaded,
            loading_more: false,
            more_error: None,
            count: Loadable::NotLoaded,
            total: Loadable::NotLoaded,
            filters: opening.filters,
            // Checked against the schema once it is read (`load_schema`).
            sort: opening.sort,
            grouping: opening.group_by.map(Grouping::new),
            frame_lines: None,
            frame_looks: Rc::default(),
            frame_places: Rc::from([]),
            #[cfg(test)]
            frame_drawn: Vec::new(),
            frame_now: chrono::Local::now(),
            frame_minute: 0,
            grid,
            edit_slot: Default::default(),
            wheel_gesture: Default::default(),
            resizing: None,
            blob_lines: None,
            _blob_wait: None,
            blob_columns: Vec::new(),
            node_labels: HashMap::new(),
            filter_menus: Default::default(),
            toolbar_menus: Default::default(),
            filter_draft: None,
            filter_editor,
            words_editor,
            words: WordsState::Typing(None),
            words_made: Vec::new(),
            fill_editor,
            fill: None,
            selection: IndexSet::default(),
            selection_anchor: None,
            bulk: BulkDraft::default(),
            drafts: DraftsBar::default(),
            bulk_editor,
            cursor: None,
            editing: None,
            cell_editor,
            _cell_editor_events: cell_editor_events,
            cells: HashMap::new(),
            draft_notices: HashMap::new(),
            drafts_due: false,
            blobs: CellMap::default(),
            blob_tasks: HashMap::new(),
            blob_queue: Default::default(),
            blob_edits: HashMap::new(),
            relations: CellMap::default(),
            relation_tasks: HashMap::new(),
            next_relation_task: 0,
            relations_drawn: HashSet::new(),
            relations_asked: HashSet::new(),
            lines_drawn: 0..0,
            relation_lines: None,
            frame_line_range: None,
            frame_due: false,
            absent: HashMap::new(),
            grouping_choices: Default::default(),
            cell_text: RefCell::new(HashMap::new()),
            peek: None,
            newer: false,
            saves_in_flight: 0,
            head_moved_while_saving: false,
            scope,
            awaiting_changes: false,
            focus_handle: cx.focus_handle(),
            table_state,
            page_task: None,
            section_tasks: HashMap::new(),
            dropped_sections: HashMap::new(),
            own_writes: 0,
            count_task: None,
            total_task: None,
            count_shift: None,
            rows_owed: false,
            _subscriptions: subscriptions,
            #[cfg(test)]
            renders: 0,
            #[cfg(test)]
            reads_sent_again: 0,
        };
        // The tab closes: an edit open is kept as its cell's draft, as leaving the cell keeps it.
        cx.on_release(|this: &mut Self, cx| {
            if let Some(editing) = this.editing.take() {
                this.keep_edit_on(&this.branch, editing, cx);
            }
        })
        .detach();
        // Drafts already kept for this type (on a node's page, in another table) show at once.
        this.sync_drafts(cx);
        this.load_schema(cx);
        this
    }

    // ---- session ----

    fn on_session_event(
        &mut self,
        _session: &Entity<GraphSession>,
        event: &SessionEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SessionEvent::SchemaChanged => self.load_schema(cx),
            // What the credential may do was read: the controls that write follow it.
            SessionEvent::AccessChanged if self.at_commit.is_none() => cx.notify(),
            // Another writer changed the schema: what the table shows was read under the old
            // one, and stays until Refresh takes the new one.
            SessionEvent::SchemaMoved if !self.newer => {
                self.newer = true;
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();
            }
            // A table pinned to a commit reads that commit whatever the branch does.
            SessionEvent::BranchChanged | SessionEvent::HeadMoved { .. }
                if self.at_commit.is_some() => {}
            SessionEvent::BranchChanged => {
                let left =
                    std::mem::replace(&mut self.branch, self.session.read(cx).branch.clone());
                // An edit open on the branch left is kept as a draft there, as leaving its cell
                // keeps it: a link or a toast's follow-up that moves the window loses nothing.
                if let Some(editing) = self.editing.take() {
                    self.keep_edit_on(&left, editing, cx);
                    if self.cell_editor.focus_handle(cx).is_focused(window) {
                        window.focus(&self.focus_handle, cx);
                    }
                }
                // A Refresh asked for on the branch left reads nothing here.
                self._refresh = None;
                // A file still being stored goes on to the branch left, and its toast tells how
                // it ended; the new branch's cells show none of it.
                self.blob_edits.clear();
                // The branch's own drafts show; the other's wait in the session.
                self.cells.clear();
                self.draft_notices.clear();
                self.sync_drafts(cx);
                self.owe_reload(cx);
            }
            SessionEvent::DraftsChanged => self.drafts_changed(cx),
            SessionEvent::HeadMoved { branch, .. } if *branch == self.branch => {
                self.compare_head(cx)
            }
            SessionEvent::ChangesRead { branch }
                if *branch == self.branch && self.awaiting_changes =>
            {
                self.compare_head(cx)
            }
            _ => {}
        }
    }

    fn load_schema(&mut self, cx: &mut Context<Self>) {
        let schema = match &self.session.read(cx).schema {
            Loadable::Loaded(schema) => schema.clone(),
            Loadable::Failed(error) => {
                self.first_page =
                    Loadable::Failed(format!("The schema could not be read: {error}"));
                cx.notify();
                return;
            }
            _ => {
                self.first_page = Loadable::Loading;
                cx.notify();
                return;
            }
        };
        // The session builds a new model on every schema read (Refresh, a recheck after a 503,
        // the Types panel); only a different schema text resets the table and reads it again.
        if self
            .schema
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &schema) || current.same_as(&schema))
        {
            return;
        }
        // The same text naming some type's nodes otherwise (spec 002, "Node names"): the table
        // keeps its columns, filters, order and grouping, its rows at its commit and an edit
        // open in them, and reads again at that commit what names neighbours: its relationship
        // cells, and the sections of a grouping by neighbours.
        if self
            .schema
            .as_ref()
            .is_some_and(|current| current.source == schema.source)
        {
            self.schema = Some(schema);
            self.relations.clear();
            self.relation_tasks.clear();
            self.relations_asked.clear();
            self.relation_lines = None;
            self.cell_text.borrow_mut().clear();
            let by_neighbours = self.grouping.as_ref().is_some_and(|grouping| {
                matches!(
                    builder::parse_group_key(&grouping.property),
                    builder::GroupKey::Relationship(..)
                )
            });
            if by_neighbours {
                // Its sections are read again; the open edit's text stays, as the cell's draft.
                if let Some(editing) = self.editing.take() {
                    self.keep_edit(editing, cx);
                }
                self.reload(true, cx);
            } else {
                self.load_shown_relations(cx);
                cx.notify();
            }
            return;
        }
        let columns = if let Some(node) = schema.node(&self.type_name) {
            self.kind = TableKind::Node;
            node_columns(node, schema.relationships_of(&self.type_name))
        } else if let Some(edge) = schema.edge(&self.type_name) {
            self.kind = TableKind::Edge;
            edge_columns(edge)
        } else {
            self.missing = true;
            self.schema = Some(schema);
            self.all_columns.clear();
            self.grouping_choices = Default::default();
            self.columns.clear();
            self.rows.clear();
            self.placed_here.clear();
            self.cell_text.borrow_mut().clear();
            cx.notify();
            return;
        };
        self.missing = false;
        let names: HashSet<&str> = columns.iter().map(|c| c.key.as_str()).collect();
        // A filter stays while its column, or its relationship, is still in the schema.
        self.filters
            .retain(|f| match builder::parse_relationship_key(&f.property) {
                Some((_, edge)) => schema.edge(edge).is_some(),
                None => f.property == builder::ID_FILTER || names.contains(f.property.as_str()),
            });
        self.all_columns = columns;
        self.show_columns(cx);
        self.schema = Some(schema);
        self.build_grouping_choices();
        // An order or a grouping asked for before the schema was read, or on a property this
        // schema no longer has, stays only where the schema allows it.
        if self
            .sort
            .as_ref()
            .is_some_and(|sort| !self.can_sort_by(sort))
        {
            self.sort = None;
        }
        if self
            .grouping
            .as_ref()
            .is_some_and(|grouping| !self.can_group_by(&grouping.property))
        {
            self.grouping = None;
        }
        self.owe_reload(cx);
    }

    /// [`Self::read_shown`] of a page from `start` (in the section `section` holds the rows of,
    /// when grouped): one the builder knows holds no row at the commit is answered here, as
    /// [`Self::count_query`] answers its count, when the commit is known.
    fn read_page(
        &self,
        query: &BuiltQuery,
        start: &PageStart,
        section: Option<&Filter>,
        snapshot: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<ReadOutput, ReadError>> {
        if let Some(commit) = snapshot.clone()
            && !self.absent.is_empty()
        {
            let mut filters = self.resolved_filters();
            filters.extend(section.cloned());
            let without: Vec<String> = self.absent.keys().cloned().collect();
            if builder::page_is_empty(&filters, self.sort.as_ref(), start, &without) {
                return Task::ready(Ok(ReadOutput::empty(&query.name, commit)));
            }
        }
        self.read_shown(query, snapshot, cx)
    }

    /// Reads what the table shows, through its scope while it follows the branch.
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

    /// Shows the columns the layout picks, keeping the cursor on its column when it still shows.
    fn show_columns(&mut self, cx: &mut Context<Self>) {
        let cursor_key = self
            .cursor
            .and_then(|c| self.columns.get(c.col))
            .map(|column| column.key.clone());
        self.columns = self.layout.shown(&self.all_columns);
        if let Some(cursor) = &mut self.cursor {
            cursor.col = cursor_key
                .and_then(|key| self.columns.iter().position(|c| c.key == key))
                .unwrap_or(0);
        }
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// Changes which columns show or their order, then shows them.
    fn arrange_columns(
        &mut self,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut ColumnLayout, &[ColumnSpec]),
    ) {
        change(&mut self.layout, &self.all_columns);
        self.show_columns(cx);
        self.load_shown_relations(cx);
    }

    /// Reads the neighbours of a relationship column that has just come into view, for the rows
    /// already read; a column that was drawn before keeps what it read.
    fn load_shown_relations(&mut self, cx: &mut Context<Self>) {
        // The lines drawn and a read's worth on either side: a column brought into view after a
        // long scroll reads what is near, and scrolling reads the rest as it comes. Grouped,
        // a folded section's rows are on no line, and read nothing.
        let near = self.lines_drawn.start.saturating_sub(RELATION_READ_CHUNK)
            ..self.lines_drawn.end + RELATION_READ_CHUNK;
        let near = self.rows_of_lines(near);
        let groups: Vec<(RelationshipGroup, Vec<String>)> = self
            .drawn_relationships()
            .into_iter()
            .map(|group| {
                let key = builder::relationship_key(&group);
                let missing = near
                    .iter()
                    .filter_map(|&ix| self.rows.get(ix))
                    .map(|row| (row.id.clone(), key.clone()))
                    .filter(|asked| !self.relations_asked.contains(asked))
                    .map(|(id, _)| id)
                    .collect();
                (group, missing)
            })
            .filter(|(_, missing): &(RelationshipGroup, Vec<String>)| !missing.is_empty())
            .collect();
        for (group, ids) in groups {
            for chunk in ids.chunks(RELATION_READ_CHUNK) {
                self.load_relation_groups(vec![group.clone()], chunk.to_vec(), cx);
            }
        }
    }

    /// The right edge of a column's header: drag to make the column wider or narrower, click
    /// twice to fit it to its type again.
    fn render_resize_handle(
        &self,
        index: usize,
        column: &ColumnSpec,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = column.key.clone();
        let width = self.layout.width(column);
        let active = self
            .resizing
            .as_ref()
            .is_some_and(|resizing| resizing.column == column.key);
        let on_start = cx.listener({
            let key = key.clone();
            move |this, event: &MouseDownEvent, _, cx| {
                this.resizing = Some(ColumnResize::new(key.clone(), event, width));
                cx.notify();
            }
        });
        let on_reset = cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.resizing = None;
            this.resize_column(&key, None, cx);
        });
        studio_ui::column_resize_handle(cx.entity_id(), index, active, on_start, on_reset, cx)
            .into_any_element()
    }

    /// Follows a column drag: the column's new width is where the pointer is now, relative to
    /// where the drag began.
    fn drag_column(&mut self, event: &DragMoveEvent<ColumnDrag>, cx: &mut Context<Self>) {
        let Some(resizing) = &self.resizing else {
            return;
        };
        if !ColumnDrag::of(event, cx.entity_id(), cx) {
            return;
        }
        let (key, width) = (resizing.column.clone(), resizing.width(event));
        self.resize_column(&key, Some(width), cx);
    }

    fn end_resize(&mut self, cx: &mut Context<Self>) {
        if self.resizing.take().is_some() {
            cx.notify();
        }
    }

    fn resize_column(&mut self, key: &str, width: Option<f32>, cx: &mut Context<Self>) {
        let before = self.layout.widths.get(key).copied();
        self.layout.set_width(key, width);
        if self.layout.widths.get(key).copied() == before {
            return;
        }
        // Saved with the tab, like the columns' order.
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// How wide the columns from `from` on are, as drawn.
    fn width_from(&self, from: usize) -> Pixels {
        let columns = self.columns.get(from..).unwrap_or_default();
        px(columns.iter().map(|c| self.layout.width(c)).sum())
    }

    // ---- reading ----

    fn page_query(
        &self,
        start: &PageStart,
        limit: u32,
        section: Option<&Filter>,
    ) -> Result<BuiltQuery, BuildError> {
        let schema = self
            .schema
            .as_ref()
            .ok_or_else(|| BuildError::UnknownType(self.type_name.clone()))?;
        let mut filters = self.resolved_filters();
        filters.extend(section.cloned());
        let sort = self.sort.as_ref();
        // At a commit made before a property was added or renamed, rows are read without it.
        let without: Vec<String> = self.absent.keys().cloned().collect();
        match self.kind {
            TableKind::Node if without.is_empty() => {
                builder::node_page(schema, &self.type_name, &filters, sort, start, limit)
            }
            TableKind::Node => builder::node_page_fields(
                schema,
                &self.type_name,
                &filters,
                sort,
                start,
                limit,
                &without,
            ),
            TableKind::Edge => builder::edge_page_without(
                schema,
                &self.type_name,
                &filters,
                sort,
                start,
                limit,
                &without,
            ),
        }
    }

    /// Orders the table by a column: ascending, then descending, then back to `@id`. The rows
    /// are read again at the same commit.
    fn cycle_sort(&mut self, property: String, cx: &mut Context<Self>) {
        self.sort = match self.sort.take() {
            Some(sort) if sort.property == property && !sort.descending => Some(Sort {
                property,
                descending: true,
            }),
            Some(sort) if sort.property == property => None,
            _ => Some(Sort {
                property,
                descending: false,
            }),
        };
        self.reload(true, cx);
    }

    /// The table's filters and order as a view, without a name.
    fn current_view(&self) -> studio_settings::SavedView {
        studio_settings::SavedView {
            name: String::new(),
            type_name: self.type_name.clone(),
            filters: self
                .filters
                .iter()
                .map(|filter| studio_settings::SavedFilter {
                    property: filter.property.clone(),
                    op: filter.op.key().to_owned(),
                    value: filter.value.clone(),
                })
                .collect(),
            sort: self.sort.as_ref().map(|sort| studio_settings::SavedSort {
                property: sort.property.clone(),
                descending: sort.descending,
            }),
            group_by: self.grouping.as_ref().map(|g| g.property.clone()),
            hidden: self.layout.hidden.clone(),
            order: self.layout.order.clone(),
        }
    }

    /// This type's saved views in this graph.
    fn saved_views(&self, cx: &App) -> Vec<studio_settings::SavedView> {
        let graph = self.session.read(cx).overrides_key();
        cx.try_global::<studio_settings::StudioSettings>()
            .map(|settings| settings.views_for(&graph, &self.type_name))
            .unwrap_or_default()
    }

    /// The saved view of `views` the table shows now, if any.
    fn active_view(&self, views: &[studio_settings::SavedView]) -> Option<String> {
        let current = self.current_view();
        views
            .iter()
            .find(|&view| {
                let hidden = |view: &studio_settings::SavedView| {
                    let mut hidden = view.hidden.clone();
                    hidden.sort();
                    hidden
                };
                view.filters == current.filters
                    && view.sort == current.sort
                    && view.group_by == current.group_by
                    && hidden(view) == hidden(&current)
                    && view.order == current.order
            })
            .map(|view| view.name.clone())
    }

    /// Shows a saved view: its filters and order, read again at the same commit. Filters on
    /// properties or relationships the schema no longer has are left out.
    fn apply_view(&mut self, view: &studio_settings::SavedView, cx: &mut Context<Self>) {
        let known: HashSet<String> = self
            .filter_choices()
            .into_iter()
            .map(|(key, _, _)| key)
            .collect();
        self.filters = view
            .filters
            .iter()
            .filter(|filter| known.contains(&filter.property))
            .filter_map(|filter| {
                Some(Filter {
                    property: filter.property.clone(),
                    op: FilterOp::from_key(&filter.op)?,
                    value: filter.value.clone(),
                })
            })
            .collect();
        self.sort = view
            .sort
            .as_ref()
            .map(|sort| Sort {
                property: sort.property.clone(),
                descending: sort.descending,
            })
            .filter(|sort| self.can_sort_by(sort));
        self.grouping = view
            .group_by
            .as_ref()
            .filter(|key| self.can_group_by(key))
            .map(Grouping::new);
        // The view's columns and their order; the widths are the tab's own, and stay.
        let widths = std::mem::take(&mut self.layout.widths);
        self.layout = ColumnLayout::new(view.hidden.clone(), view.order.clone());
        self.layout.widths = widths;
        self.show_columns(cx);
        self.filter_draft = None;
        self.reload(true, cx);
    }

    fn open_save_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let graph = self.session.read(cx).overrides_key();
        let view = self.current_view();
        let taken = self
            .saved_views(cx)
            .into_iter()
            .map(|view| view.name)
            .collect();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                SaveViewModal::new(graph, view, taken, window, cx)
            });
        });
    }

    /// Adds the row at `row` to the selection, or takes it out; with `range`, picks every row
    /// from the last one picked to it.
    fn pick_rows(&mut self, row: usize, range: bool, cx: &mut Context<Self>) {
        let Some(id) = self.rows.get(row).map(|r| r.id.clone()) else {
            return;
        };
        match self.selection_anchor.filter(|_| range) {
            Some(anchor) => {
                let between: Vec<usize> = match &self.grouping {
                    Some(grouping) => grouping.rows_between(anchor, row),
                    None => (anchor.min(row)..=anchor.max(row).min(self.rows.len() - 1)).collect(),
                };
                for ix in between {
                    if let Some(picked) = self.rows.get(ix) {
                        self.selection.insert(picked.id.clone());
                    }
                }
            }
            None => {
                if !self.selection.shift_remove(&id) {
                    self.selection.insert(id);
                }
                self.selection_anchor = Some(row);
            }
        }
        self.bulk.notice = None;
        cx.notify();
    }

    /// Drops the pick (Esc, Clear, a plain click). A set being written stays out, and Set stays
    /// disabled until it is back: it was sent once, and its outcome is its own.
    fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.selection.clear();
        self.selection_anchor = None;
        self.bulk = BulkDraft {
            sending: self.bulk.sending.take(),
            ..BulkDraft::default()
        };
        cx.notify();
    }

    /// Sets the chosen property on every selected row in one mutation, each row bound to the
    /// value it shows (spec 004, bulk set). Rows already holding the value are left out.
    fn submit_bulk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.bulk.saving() || self.change_refusal(cx).is_some() {
            return;
        }
        let (Some(property), Some(schema)) = (
            self.bulk
                .property
                .as_deref()
                .and_then(|name| self.property_of(name)),
            self.schema.clone(),
        ) else {
            return;
        };
        let text = self.bulk_editor.read(cx).text(cx);
        let new = match parse_input(&property, &text) {
            Ok(value) => value,
            Err(message) => {
                self.bulk.notice = Some(Notice::new(Tone::Invalid, message));
                cx.notify();
                return;
            }
        };
        // Grouped by a relationship, a row shows once under each neighbour: it is set once.
        let mut seen = HashSet::new();
        let rows: Vec<(String, Value)> = self
            .rows
            .iter()
            .filter(|row| self.selection.contains(&row.id) && seen.insert(row.id.as_str()))
            .map(|row| (row.id.clone(), row.value(&property.name).clone()))
            .collect();
        // Each row is bound to the value it shows: a selected row the table no longer shows (a
        // filter or a read since hid it) has none, and is not set behind the count's back.
        let hidden = self.selection.len().saturating_sub(rows.len());
        if hidden > 0 {
            self.bulk.notice = Some(Notice::new(
                Tone::Invalid,
                format!(
                    "{} in the selection {} not shown here, so nothing was set. Show {}, or \
                     select again.",
                    plural(hidden, "row", "rows"),
                    if hidden == 1 { "is" } else { "are" },
                    if hidden == 1 { "it" } else { "them" },
                ),
            ));
            cx.notify();
            return;
        }
        let rows: Vec<(String, Value)> = rows
            .into_iter()
            .filter(|(_, old)| !same_value(old, &new))
            .collect();
        if rows.is_empty() {
            self.bulk.notice = Some(Notice::new(
                Tone::Invalid,
                "Every selected row already has that value.".to_owned(),
            ));
            cx.notify();
            return;
        }
        let count = rows.len();
        let description = format!("Set {} on {count} {} rows", property.name, self.type_name);
        // Each row's value is the one the table read at its commit.
        let write = match write::write_cells(
            &self.session,
            &self.branch,
            &schema,
            &self.type_name,
            &property.name,
            &rows,
            &new,
            self.paging.pinned.clone(),
            description,
            cx,
        ) {
            Ok(write) => write,
            Err(error) => {
                self.bulk.notice = Some(Notice::new(Tone::Invalid, error));
                cx.notify();
                return;
            }
        };
        self.bulk.sending = Some(self.selection.clone());
        self.bulk.notice = None;
        self.saves_in_flight += 1;
        cx.notify();
        let (session, branch) = (self.session.clone(), self.branch.clone());
        let column = property.name;
        // Detached: a write is never cancelled by closing the tab.
        cx.spawn_in(window, async move |this, cx| {
            let bound = write.await;
            this.update_in(cx, |this, _window, cx| {
                this.finish_bulk(bound, rows, column, new, &session, &branch, cx)
            })
            .ok();
        })
        .detach();
    }

    #[expect(clippy::too_many_arguments)]
    fn finish_bulk(
        &mut self,
        bound: Bound,
        rows: Vec<(String, Value)>,
        column: String,
        new: Value,
        session: &Entity<GraphSession>,
        branch: &str,
        cx: &mut Context<Self>,
    ) {
        self.saves_in_flight = self.saves_in_flight.saturating_sub(1);
        let sent = self.bulk.sending.take();
        // The rows set were the branch left's; the table shows what it read on this one.
        if !self.shows(session, branch) {
            self.saves_settled(cx);
            cx.notify();
            return;
        }
        // The bar tells the outcome, and a set that landed clears the pick, only while the rows
        // it was sent for are the ones picked: a pick made since is the person's next, and the
        // write's toast tells this one's outcome.
        let still_picked = sent.is_some_and(|sent| sent == self.selection);
        let tell = |bulk: &mut BulkDraft, notice: Notice| {
            if still_picked {
                bulk.notice = Some(notice);
            }
        };
        match &bound {
            Bound::Changed => {
                // A row's list changed elsewhere since it was read: nothing was set. Read the
                // table at the head so every row shows what it holds.
                tell(
                    &mut self.bulk,
                    Notice::new(
                        Tone::Conflict,
                        format!(
                            "A row had changed elsewhere, so none was set; the table now shows \
                             {branch}'s head."
                        ),
                    ),
                );
                self.reload(false, cx);
            }
            // A commit on no branch, or of a kind not known, is told as the others are.
            Bound::Sent(graph_client::WriteOutcome::Committed(output))
                if published(output.commit.as_ref(), branch).is_ok() =>
            {
                let changed = output.affected_nodes as usize;
                match output.commit.as_ref() {
                    Some(commit) if changed == rows.len() => {
                        let places = self.row_places();
                        let filters = self.resolved_filters();
                        for (id, _) in &rows {
                            if let Some(at) = places.get(id) {
                                self.set_cell_at(at, &column, new.clone());
                                let fits = self.row_fits_under(&filters, at[0]);
                                self.set_dimmed_at(at, !fits);
                            }
                        }
                        if self.follow_own_commit(
                            commit.parent_commit_id.as_deref(),
                            &commit.graph_commit_id,
                        ) {
                            self.newer = true;
                        }
                        let written = rows
                            .iter()
                            .map(|(id, _)| (id.clone(), column.clone(), new.clone()))
                            .collect();
                        let parent = commit.parent_commit_id.as_deref();
                        self.tell_peek_wrote(written, None, &commit.graph_commit_id, parent, cx);
                        if still_picked {
                            self.clear_selection(cx);
                        }
                    }
                    _ => {
                        // Some rows held another value than the one shown: they were left
                        // alone. Read the table at the head so every row shows what it holds.
                        let left = rows.len().saturating_sub(changed);
                        tell(
                            &mut self.bulk,
                            Notice::new(
                                Tone::Conflict,
                                format!(
                                    "{left} of {} rows had changed elsewhere and were left \
                                     alone; the table now shows {branch}'s head.",
                                    rows.len()
                                ),
                            ),
                        );
                        self.reload(false, cx);
                    }
                }
            }
            _ => {
                if let SaveResult::Refused(notice) | SaveResult::Unknown(notice) =
                    bound.result(branch)
                {
                    tell(&mut self.bulk, notice);
                }
            }
        }
        self.saves_settled(cx);
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// The bar over the table while rows are picked: how many, setting one property on all of
    /// them, and deleting them.
    fn render_bulk_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.selection.is_empty() {
            return None;
        }
        let border = studio_ui::ds::surface::rule(cx);
        let count = self.selection.len();
        let read_only = self.kind == TableKind::Edge || self.at_commit.is_some();
        let refused = self.change_refusal(cx);
        let can_fill = refused.is_none();
        let editable: Vec<String> = self
            .columns
            .iter()
            .filter(|c| c.editable(self.kind))
            .filter_map(|c| c.property().map(|p| p.name.clone()))
            .collect();
        let chosen = self.bulk.property.clone();
        let table = cx.entity().downgrade();
        let saving = self.bulk.saving();
        let mut bar = h_flex()
            .flex_none()
            .px(px(space::LG))
            .py(px(space::SM))
            .gap(px(space::MD))
            .items_center()
            .border_b_1()
            .border_color(border)
            .child(
                studio_ui::ds::text::body(format!("{} selected", plural(count, "row", "rows")))
                    .weight(studio_ui::ds::text::weight::HEADING),
            );
        if read_only {
            bar = bar.child(studio_ui::ds::text::muted(self.read_only_label()));
        } else if let Some(reason) = refused {
            // The set stays in view, disabled, saying what the credential lacks.
            bar = bar.child(
                studio_ui::ds::button("bulk-property-button", "Set a property ▾")
                    .style(ButtonStyle::Outlined)
                    .disabled(true)
                    .tooltip(Tooltip::text(reason)),
            );
        } else {
            bar = bar
                .child(
                    PopoverMenu::new("bulk-property")
                        .trigger(
                            studio_ui::ds::button(
                                "bulk-property-button",
                                match &chosen {
                                    Some(name) => format!("Set {name} ▾"),
                                    None => "Set a property ▾".to_owned(),
                                },
                            )
                            .style(ButtonStyle::Outlined),
                        )
                        .menu(move |window, cx| {
                            let editable = editable.clone();
                            let table = table.clone();
                            Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                for name in editable {
                                    let table = table.clone();
                                    let chosen = name.clone();
                                    menu = menu.entry(name, None, move |window, cx| {
                                        let chosen = chosen.clone();
                                        table
                                            .update(cx, |table, cx| {
                                                table.bulk.property = Some(chosen);
                                                table.bulk.notice = None;
                                                let focus = table.bulk_editor.focus_handle(cx);
                                                window.focus(&focus, cx);
                                                cx.notify();
                                            })
                                            .ok();
                                    });
                                }
                                menu
                            }))
                        }),
                )
                .when(chosen.is_some(), |bar| {
                    bar.child(
                        studio_ui::ds::field_frame(false, cx)
                            .w(px(studio_ui::ds::size::FIELD))
                            .child(studio_ui::ds::field_input(self.bulk_editor.clone())),
                    )
                    .child(
                        studio_ui::ds::button(
                            "bulk-set",
                            if saving {
                                format!("Setting {count}…")
                            } else {
                                format!("Set on {count} rows")
                            },
                        )
                        .style(ButtonStyle::Tinted(TintColor::Accent))
                        .disabled(saving)
                        .tooltip(Tooltip::text(format!(
                            "One commit to {}; each row is bound to the value it shows",
                            self.branch
                        )))
                        .on_click(cx.listener(|this, _, window, cx| this.submit_bulk(window, cx))),
                    )
                });
        }
        // A model fills the chosen property from the row's other columns (spec 019).
        if !read_only && can_fill && chosen.is_some() && self.fill.is_none() {
            bar = bar.child(
                studio_ui::ds::button("bulk-fill", "Fill with a model…")
                    .style(ButtonStyle::Outlined)
                    .tooltip(|_, cx| {
                        Tooltip::for_action(
                            "Suggest each selected row's value from its other columns",
                            &FillColumn,
                            cx,
                        )
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.open_fill(window, cx))),
            );
        }
        // Deleting asks first, naming the rows, in its own dialog.
        let delete_refused = self.write_refusal(cx);
        bar = bar.child(
            studio_ui::ds::button("bulk-delete", "Delete…")
                .style(ButtonStyle::Outlined)
                .disabled(delete_refused.is_some())
                .tooltip(Tooltip::text(delete_refused.unwrap_or_else(|| {
                    format!("Delete the {} selected", plural(count, "row", "rows"))
                })))
                .on_click(
                    cx.listener(|this, _, window, cx| this.on_delete(&DeleteRow, window, cx)),
                ),
        );
        let bar =
            bar.children(self.bulk.notice.clone().map(|notice| {
                studio_ui::ds::text::body(notice.message).color(notice.tone.color())
            }))
            .child(div().flex_1())
            .child(
                studio_ui::ds::button("bulk-clear", "Clear")
                    .style(ButtonStyle::Subtle)
                    .tooltip(Tooltip::text("Clear the selection (Esc)"))
                    .on_click(cx.listener(|this, _, _, cx| this.clear_selection(cx))),
            );
        let fill_row = self
            .fill
            .as_ref()
            .map(|fill| self.render_fill(fill, border, cx));
        Some(
            v_flex()
                .flex_none()
                .child(bar)
                .children(fill_row)
                .into_any_element(),
        )
    }

    // ---- fill a column (spec 019) ----

    /// Opens the fill for the bulk bar's property, over the selected rows, from every other
    /// column the row shows.
    fn open_fill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = match self.bulk.property.clone().or_else(|| {
            let cursor = self.cursor?;
            self.columns
                .get(cursor.col)?
                .property()
                .map(|p| p.name.clone())
        }) {
            Some(target) => target,
            None => return,
        };
        if self.selection.is_empty() {
            self.bulk.notice = Some(Notice::new(
                Tone::Invalid,
                "Pick the rows to fill first (⌘-click, ⇧-click).",
            ));
            cx.notify();
            return;
        }
        let sources: Vec<String> = self
            .columns
            .iter()
            .filter(|column| column.property().is_some_and(|p| p.name != target))
            .filter(|column| {
                column
                    .property()
                    .is_some_and(|p| !matches!(p.kind, ScalarKind::Blob | ScalarKind::Vector))
            })
            .map(|column| column.key.clone())
            .collect();
        if self.fill.is_none() {
            self.fill = Some(FillState {
                target,
                sources,
                phase: FillPhase::Typing,
            });
        }
        window.focus(&self.fill_editor.focus_handle(cx), cx);
        cx.notify();
    }

    fn on_fill_column(&mut self, _: &FillColumn, window: &mut Window, cx: &mut Context<Self>) {
        self.open_fill(window, cx);
    }

    fn run_fill(&mut self, cx: &mut Context<Self>) {
        let instruction = self.fill_editor.read(cx).text(cx).trim().to_owned();
        let Some(fill) = &self.fill else {
            return;
        };
        if instruction.is_empty() || matches!(fill.phase, FillPhase::Running { .. }) {
            return;
        }
        let Some(models) = Models::try_global(cx) else {
            self.set_fill_phase(FillPhase::Done {
                filled: 0,
                unsure: 0,
                skipped: 0,
                failed: Some(ModelError::NoKey.to_string()),
            });
            cx.notify();
            return;
        };
        self.start_fill(models, instruction, cx);
    }

    fn set_fill_phase(&mut self, phase: FillPhase) {
        if let Some(fill) = &mut self.fill {
            fill.phase = phase;
        }
    }

    /// Runs the fill: a call per selected row, [`fill::AT_ONCE`] at a time, each answer a draft
    /// on its cell. A cell with a draft or an edit already is skipped, as is one whose value
    /// changed while its call was out, and a row the answer has no value for.
    fn start_fill(&mut self, models: Entity<Models>, instruction: String, cx: &mut Context<Self>) {
        let Some(fill) = &self.fill else {
            return;
        };
        let Some(property) = self.property_of(&fill.target) else {
            return;
        };
        let target = fill.target.clone();
        let sources = fill.sources.clone();
        let source_names: Vec<(String, String)> = sources
            .iter()
            .filter_map(|key| {
                self.columns
                    .iter()
                    .find(|c| c.key == *key)
                    .map(|c| (key.clone(), c.title.clone()))
            })
            .collect();
        // The rows to fill, as the selection orders them, with what each shows now.
        let rows: Vec<FillRow> = self
            .selection
            .iter()
            .take(fill::MAX_ROWS)
            .filter_map(|id| {
                let ix = self.row_index(id)?;
                let row = &self.rows[ix];
                let values = source_names
                    .iter()
                    .map(|(key, title)| (title.clone(), row.value(key).clone()))
                    .collect();
                Some((id.clone(), row.value(&target).clone(), values))
            })
            .collect();
        let total = rows.len();
        let task = cx.spawn(async move |this, cx| {
            let mut failed = None;
            for chunk in rows.chunks(fill::AT_ONCE) {
                let calls: Vec<_> = chunk
                    .iter()
                    .map(|(id, old, values)| {
                        let request = ModelRequest {
                            task: &fill::TASK,
                            instructions: fill::INSTRUCTIONS,
                            input: fill::input(&property, values, &instruction),
                            schema: fill::schema(),
                        };
                        let call = this
                            .read_with(cx, |this, cx| {
                                // A cell with a draft or an edit is the person's; left alone.
                                let busy = this.cells.contains_key(&(id.clone(), target.clone()))
                                    || this.editing.as_ref().is_some_and(|editing| {
                                        editing.row_id == *id && editing.column == target
                                    });
                                (!busy).then(|| models.read(cx).call::<fill::Answer>(request, cx))
                            })
                            .ok()
                            .flatten();
                        (id.clone(), old.clone(), call)
                    })
                    .collect();
                let answers = futures::future::join_all(calls.into_iter().map(
                    |(id, old, call)| async move {
                        let answer = match call {
                            Some(call) => Some(call.await),
                            None => None,
                        };
                        (id, old, answer)
                    },
                ))
                .await;
                let stop = this
                    .update(cx, |this, cx| {
                        let mut stop = false;
                        for (id, old, answer) in answers {
                            let outcome = match answer {
                                None => Outcome::Skipped,
                                Some(Err(error)) => {
                                    failed = Some(error.to_string());
                                    stop = true;
                                    Outcome::Skipped
                                }
                                Some(Ok(answer)) => match answer.value.value.clone() {
                                    Some(text) if answer.value.value_fits() => {
                                        // The row moved on meanwhile: its new value stands.
                                        if !this.row_shows(&id, &target, &old) {
                                            Outcome::Skipped
                                        } else if !answer.value.sure {
                                            Outcome::Unsure
                                        } else {
                                            match edited(&property, &old, &text) {
                                                Some(Ok(_)) => {
                                                    this.put_draft(
                                                        &id, &target, old, text, None, cx,
                                                    );
                                                    Outcome::Filled
                                                }
                                                // The same value, or text the property does
                                                // not take: nothing to draft.
                                                _ => Outcome::Skipped,
                                            }
                                        }
                                    }
                                    _ => Outcome::Skipped,
                                },
                            };
                            if let Some(FillState {
                                phase:
                                    FillPhase::Running {
                                        done,
                                        filled,
                                        unsure,
                                        skipped,
                                        ..
                                    },
                                ..
                            }) = &mut this.fill
                            {
                                *done += 1;
                                match outcome {
                                    Outcome::Filled => *filled += 1,
                                    Outcome::Unsure => *unsure += 1,
                                    Outcome::Skipped => *skipped += 1,
                                }
                            }
                        }
                        this.drafts.confirm_discard = false;
                        cx.notify();
                        stop
                    })
                    .unwrap_or(true);
                if stop {
                    break;
                }
            }
            this.update(cx, |this, cx| {
                if let Some(FillState {
                    phase:
                        FillPhase::Running {
                            filled,
                            unsure,
                            skipped,
                            ..
                        },
                    ..
                }) = &this.fill
                {
                    let (filled, unsure, skipped) = (*filled, *unsure, *skipped);
                    this.set_fill_phase(FillPhase::Done {
                        filled,
                        unsure,
                        skipped,
                        failed,
                    });
                }
                cx.notify();
            })
            .ok();
        });
        self.set_fill_phase(FillPhase::Running {
            _call: task,
            done: 0,
            total,
            filled: 0,
            unsure: 0,
            skipped: 0,
        });
        cx.notify();
    }

    /// The fill's row under the bulk bar: the target, the source columns, the instruction and
    /// how far the fill is.
    fn render_fill(&self, fill: &FillState, border: gpui::Hsla, cx: &mut Context<Self>) -> Div {
        let running = matches!(fill.phase, FillPhase::Running { .. });
        let table = cx.entity().downgrade();
        let choices: Vec<(String, String, bool)> = self
            .columns
            .iter()
            .filter(|column| column.property().is_some_and(|p| p.name != fill.target))
            .map(|column| {
                (
                    column.key.clone(),
                    column.title.clone(),
                    fill.sources.contains(&column.key),
                )
            })
            .collect();
        let sources_label = format!(
            "from {} ▾",
            studio_ui::plural(fill.sources.len(), "column", "columns")
        );
        let mut row = h_flex()
            .flex_none()
            .px(px(space::LG))
            .py(px(space::SM))
            .gap(px(space::MD))
            .items_center()
            .border_b_1()
            .border_color(border)
            .child(studio_ui::ds::text::body(format!("Fill {}", fill.target)))
            .child(
                PopoverMenu::new("fill-sources")
                    .trigger(
                        studio_ui::ds::button("fill-sources-button", sources_label)
                            .style(ButtonStyle::Outlined)
                            .disabled(running),
                    )
                    .menu(move |window, cx| {
                        let choices = choices.clone();
                        let table = table.clone();
                        Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                            for (key, title, on) in choices {
                                let table = table.clone();
                                menu = menu.toggleable_entry(
                                    title,
                                    on,
                                    IconPosition::Start,
                                    None,
                                    move |_, cx| {
                                        let key = key.clone();
                                        table
                                            .update(cx, |table, cx| {
                                                if let Some(fill) = &mut table.fill {
                                                    if on {
                                                        fill.sources.retain(|k| *k != key);
                                                    } else {
                                                        fill.sources.push(key);
                                                    }
                                                }
                                                cx.notify();
                                            })
                                            .ok();
                                    },
                                );
                            }
                            menu
                        }))
                    }),
            )
            .child(
                studio_ui::ds::field_frame(false, cx)
                    .w(px(studio_ui::ds::size::FIELD_WIDE))
                    .child(studio_ui::ds::field_input(self.fill_editor.clone()))
                    .when(running, |field| field.child(studio_ui::ds::spinner())),
            );
        match &fill.phase {
            FillPhase::Typing => {
                let count = self.selection.len().min(fill::MAX_ROWS);
                row = row.child(
                    studio_ui::ds::button(
                        "fill-run",
                        format!("Fill {}", studio_ui::plural(count, "row", "rows")),
                    )
                    .style(ButtonStyle::Tinted(TintColor::Accent))
                    .tooltip(Tooltip::text(format!(
                        "{} go to {}. Each answer lands as the cell's draft; save the drafts \
                         to write them",
                        fill::TASK.sends,
                        Models::model_for(fill::TASK.tier, cx)
                    )))
                    .on_click(cx.listener(|this, _, _, cx| this.run_fill(cx))),
                );
            }
            FillPhase::Running {
                done,
                total,
                filled,
                ..
            } => {
                row = row.child(studio_ui::ds::text::muted(format!(
                    "{done} of {total} answered · {filled} drafted"
                )));
            }
            FillPhase::Done {
                filled,
                unsure,
                skipped,
                failed,
            } => {
                let mut text = format!("{} drafted", studio_ui::plural(*filled, "cell", "cells"));
                if *unsure > 0 {
                    text.push_str(&format!(" · {unsure} unsure, left empty"));
                }
                if *skipped > 0 {
                    text.push_str(&format!(" · {skipped} skipped"));
                }
                if *filled > 0 {
                    text.push_str(" · save the drafts to write them (⌘S)");
                }
                row = row.child(studio_ui::ds::text::muted(text));
                if let Some(why) = failed {
                    row = row.child(studio_ui::ds::text::error(why.clone()));
                }
            }
        }
        row.child(div().flex_1()).child(
            studio_ui::ds::button("fill-close", "Close")
                .style(ButtonStyle::Subtle)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.fill = None;
                    window.focus(&this.focus_handle, cx);
                    cx.notify();
                })),
        )
    }

    /// Every column but the pinned one, in the layout's order: key, title, whether it shows.
    fn column_choices(&self) -> Vec<(String, String, bool)> {
        self.layout
            .columns(&self.all_columns)
            .into_iter()
            .skip(1)
            .map(|(column, shown)| (column.key.clone(), column.title.clone(), shown))
            .collect()
    }

    /// A column header's right-click menu: order, group and filter by the column, move it, or
    /// hide it.
    fn open_header_menu(
        &mut self,
        key: String,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(col) = self.column_index(&key) else {
            return;
        };
        let column = self.columns[col].clone();
        let pinned = col == 0;
        let last = col + 1 == self.columns.len();
        let sortable = column
            .property()
            .filter(|p| Sort::allowed(p))
            .map(|p| p.name.clone());
        let groupable = self.can_group_by(&key).then(|| key.clone());
        let filterable = !self.ops_for(&key).is_empty();
        let sort = self.sort.clone();
        let grouped = self.grouping.as_ref().map(|g| g.property.clone());
        let arranged = self.layout != ColumnLayout::default();
        // Entries line up with the checked ones when the menu has any.
        let checks = sortable.is_some() || groupable.is_some();
        let plain = move |label: &'static str| {
            let entry = ContextMenuEntry::new(label);
            if checks {
                entry.toggleable(IconPosition::Start, false)
            } else {
                entry
            }
        };
        let table = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            let run = |f: Box<TableAction>| {
                let table = table.clone();
                move |window: &mut Window, cx: &mut App| {
                    table.update(cx, |table, cx| f(table, window, cx)).ok();
                }
            };
            if let Some(name) = sortable {
                let ordered = sort.as_ref().filter(|sort| sort.property == name);
                for (label, descending) in [("Sort ascending", false), ("Sort descending", true)] {
                    let name = name.clone();
                    menu = menu.toggleable_entry(
                        label,
                        ordered.is_some_and(|sort| sort.descending == descending),
                        IconPosition::Start,
                        None,
                        run(Box::new(move |table, _, cx| {
                            table.set_sort(
                                Some(Sort {
                                    property: name.clone(),
                                    descending,
                                }),
                                cx,
                            )
                        })),
                    );
                }
            }
            if let Some(name) = groupable {
                let on = grouped.as_ref() == Some(&name);
                menu = menu.toggleable_entry(
                    "Group by this column",
                    on,
                    IconPosition::Start,
                    None,
                    run(Box::new(move |table, _, cx| {
                        table.set_grouping((!on).then(|| name.clone()), cx)
                    })),
                );
            }
            if filterable {
                let key = key.clone();
                menu = menu.item(plain("Filter…").handler(run(Box::new(
                    move |table, window, cx| table.set_draft_property(key.clone(), window, cx),
                ))));
            }
            if !pinned {
                menu = menu.separator();
                for (label, step, disabled) in
                    [("Move left", -1, col <= 1), ("Move right", 1, last)]
                {
                    let key = key.clone();
                    menu = menu.item(plain(label).disabled(disabled).handler(run(Box::new(
                        move |table, _, cx| {
                            table.arrange_columns(cx, |layout, all| layout.step(all, &key, step))
                        },
                    ))));
                }
                let key = key.clone();
                menu = menu.item(plain("Hide column").handler(run(Box::new(
                    move |table, _, cx| {
                        table.arrange_columns(cx, |layout, all| layout.toggle(all, &key))
                    },
                ))));
            }
            if arranged {
                menu = menu
                    .separator()
                    .item(
                        plain("Show all columns").handler(run(Box::new(|table, _, cx| {
                            table.arrange_columns(cx, |layout, _| layout.reset())
                        }))),
                    );
            }
            menu
        });
        let subscription = cx.subscribe_in(
            &menu,
            window,
            |this, _, _: &gpui::DismissEvent, window, cx| {
                this.header_menu = None;
                window.focus(&this.focus_handle, cx);
                cx.notify();
            },
        );
        window.focus(&menu.focus_handle(cx), cx);
        self.header_menu = Some((menu, position, subscription));
        cx.notify();
    }

    /// The toolbar's Columns menu: every column but the pinned one, checked while it shows. It
    /// stays open while columns are picked.
    fn render_columns_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // The columns not shown: counted every render, so not by matching each column.
        let hidden = self.all_columns.len().saturating_sub(self.columns.len());
        let table = cx.entity().downgrade();
        let open = self.toolbar_menus[1].is_deployed();
        let menu = PopoverMenu::new("columns-menu")
            .anchor(gpui::Anchor::TopRight)
            .with_handle(self.toolbar_menus[1].clone())
            .trigger(
                studio_ui::ds::chip_button(
                    "columns",
                    IconName::Eye,
                    "Columns",
                    (hidden > 0).then(|| format!("{hidden} hidden").into()),
                )
                .disabled(self.all_columns.len() < 2)
                .when(!open, |button| {
                    button.tooltip(Tooltip::text(
                        "Show or hide columns · drag a column's header to move it",
                    ))
                }),
            )
            .menu(move |window, cx| {
                let table = table.clone();
                Some(ContextMenu::build_persistent(
                    window,
                    cx,
                    move |mut menu, _, cx| {
                        menu = menu.keep_open_on_confirm(true);
                        let Some(entity) = table.upgrade() else {
                            return menu;
                        };
                        let (choices, arranged) = {
                            let table = entity.read(cx);
                            (
                                table.column_choices(),
                                table.layout != ColumnLayout::default(),
                            )
                        };
                        for (key, title, shown) in choices {
                            let table = table.clone();
                            menu = menu.toggleable_entry(
                                title,
                                shown,
                                IconPosition::Start,
                                None,
                                move |_, cx| {
                                    table
                                        .update(cx, |table, cx| {
                                            table.arrange_columns(cx, |layout, all| {
                                                layout.toggle(all, &key)
                                            })
                                        })
                                        .ok();
                                },
                            );
                        }
                        let table = table.clone();
                        menu.separator().item(
                            ContextMenuEntry::new("Show all columns")
                                .toggleable(IconPosition::Start, false)
                                .disabled(!arranged)
                                .handler(move |_, cx| {
                                    table
                                        .update(cx, |table, cx| {
                                            table.arrange_columns(cx, |layout, _| layout.reset())
                                        })
                                        .ok();
                                }),
                        )
                    },
                ))
            });
        let reset = cx.listener(|this, _, _, cx| {
            this.arrange_columns(cx, |layout, _| layout.reset());
            cx.notify();
        });
        studio_ui::ds::control_chip("columns-chip", menu).when(hidden > 0, |chip| {
            chip.on_clear("Show every column, in its first order", reset)
        })
    }

    /// The toolbar's Group menu: the properties rows can be grouped by, and No grouping.
    fn render_group_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let choices = self.group_choices();
        let menus = self.group_menus();
        let current = self.grouping.as_ref().map(|g| g.property.clone());
        let current_label = current.as_ref().map(|key| {
            choices
                .iter()
                .find(|(choice, _)| choice == key)
                .map_or_else(|| key.clone(), |(_, label)| label.clone())
        });
        let table = cx.entity().downgrade();
        let open = self.toolbar_menus[2].is_deployed();
        let grouped = current_label.is_some();
        let menu = PopoverMenu::new("group-menu")
            .anchor(gpui::Anchor::TopRight)
            .with_handle(self.toolbar_menus[2].clone())
            .trigger(
                studio_ui::ds::chip_button(
                    "group",
                    IconName::ListTree,
                    "Group",
                    current_label.map(SharedString::from),
                )
                .disabled(choices.is_empty())
                .when(!open, |button| {
                    button.tooltip(Tooltip::text(
                        "Group the rows by a property, a relationship or a property of a neighbour",
                    ))
                }),
            )
            .menu(move |window, cx| {
                let (table, current, choices, menus) = (
                    table.clone(),
                    current.clone(),
                    choices.clone(),
                    menus.clone(),
                );
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    let set = |property: Option<String>| {
                        let table = table.clone();
                        move |_: &mut Window, cx: &mut App| {
                            table
                                .update(cx, |table, cx| table.set_grouping(property.clone(), cx))
                                .ok();
                        }
                    };
                    menu = menu
                        .toggleable_entry(
                            "No grouping",
                            current.is_none(),
                            IconPosition::Start,
                            None,
                            set(None),
                        )
                        .separator();
                    for (key, label) in choices.iter() {
                        if !builder::parse_group_key(key).is_own_property() {
                            break;
                        }
                        menu = menu.toggleable_entry(
                            label.clone(),
                            current.as_ref() == Some(key),
                            IconPosition::Start,
                            None,
                            set(Some(key.clone())),
                        );
                    }
                    if !menus.is_empty() {
                        menu = menu.separator();
                    }
                    for group in menus.iter() {
                        let holds_current = current.as_ref().is_some_and(|current| {
                            group
                                .neighbour
                                .as_ref()
                                .is_some_and(|(key, _)| key == current)
                                || group.properties.iter().any(|(key, _)| key == current)
                        });
                        let (table, current, group) =
                            (table.clone(), current.clone(), group.clone());
                        // The arrow takes the place of the entries' check, and is the accent
                        // colour while the submenu holds the grouping.
                        menu = menu.submenu_with_colored_icon(
                            group.label.clone(),
                            group.icon,
                            if holds_current {
                                Color::Accent
                            } else {
                                Color::Muted
                            },
                            move |mut menu, _, _| {
                                let set = |property: String| {
                                    let table = table.clone();
                                    move |_: &mut Window, cx: &mut App| {
                                        table
                                            .update(cx, |table, cx| {
                                                table.set_grouping(Some(property.clone()), cx)
                                            })
                                            .ok();
                                    }
                                };
                                if let Some((key, _)) = &group.neighbour {
                                    menu = menu.toggleable_entry(
                                        format!("Each {}", group.other_type),
                                        current.as_ref() == Some(key),
                                        IconPosition::Start,
                                        None,
                                        set(key.clone()),
                                    );
                                    if !group.properties.is_empty() {
                                        menu = menu.separator();
                                    }
                                }
                                if !group.properties.is_empty() {
                                    menu = menu.header(format!("{}'s property", group.other_type));
                                }
                                for (key, name) in &group.properties {
                                    menu = menu.toggleable_entry(
                                        name.clone(),
                                        current.as_ref() == Some(key),
                                        IconPosition::Start,
                                        None,
                                        set(key.clone()),
                                    );
                                }
                                menu
                            },
                        );
                    }
                    menu
                }))
            });
        studio_ui::ds::control_chip("group-chip", menu).when(grouped, |chip| {
            chip.on_clear(
                "Stop grouping the rows",
                cx.listener(|this, _, _, cx| this.set_grouping(None, cx)),
            )
        })
    }

    /// The toolbar's Sort menu: the properties the rows can be ordered by, then the direction.
    /// A column header's menu orders by its column as well.
    fn render_sort_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let choices: Vec<(String, String)> = self
            .all_columns
            .iter()
            .filter_map(|column| {
                column
                    .property()
                    .filter(|property| Sort::allowed(property))
                    .map(|property| (property.name.clone(), column.title.clone()))
            })
            .collect();
        let current = self.sort.clone();
        let current_label = current.as_ref().map(|sort| {
            let title = choices
                .iter()
                .find(|(name, _)| *name == sort.property)
                .map_or(sort.property.as_str(), |(_, title)| title.as_str());
            SharedString::from(format!(
                "{title} {}",
                if sort.descending { "↓" } else { "↑" }
            ))
        });
        let sorted = current.is_some();
        let table = cx.entity().downgrade();
        let open = self.toolbar_menus[3].is_deployed();
        let menu = PopoverMenu::new("sort-menu")
            .anchor(gpui::Anchor::TopRight)
            .with_handle(self.toolbar_menus[3].clone())
            .trigger(
                studio_ui::ds::chip_button("sort", IconName::ChevronUpDown, "Sort", current_label)
                    .disabled(choices.is_empty())
                    .when(!open, |button| {
                        button.tooltip(Tooltip::text(
                            "Order the rows by a property · a column's header orders by it too",
                        ))
                    }),
            )
            .menu(move |window, cx| {
                let (table, current, choices) = (table.clone(), current.clone(), choices.clone());
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    let set = |sort: Option<Sort>| {
                        let table = table.clone();
                        move |_: &mut Window, cx: &mut App| {
                            table
                                .update(cx, |table, cx| table.set_sort(sort.clone(), cx))
                                .ok();
                        }
                    };
                    let descending = current.as_ref().is_some_and(|sort| sort.descending);
                    for (name, title) in choices.iter() {
                        menu = menu.toggleable_entry(
                            title.clone(),
                            current.as_ref().is_some_and(|sort| sort.property == *name),
                            IconPosition::Start,
                            None,
                            set(Some(Sort {
                                property: name.clone(),
                                descending,
                            })),
                        );
                    }
                    menu = menu.separator();
                    for (label, down) in [("Ascending", false), ("Descending", true)] {
                        let sort = current.clone().map(|sort| Sort {
                            descending: down,
                            ..sort
                        });
                        menu = menu.item(
                            ContextMenuEntry::new(label)
                                .toggleable(IconPosition::Start, sorted && descending == down)
                                .disabled(!sorted)
                                .handler(set(sort)),
                        );
                    }
                    menu
                }))
            });
        studio_ui::ds::control_chip("sort-chip", menu).when(sorted, |chip| {
            chip.on_clear(
                "Stop ordering the rows: they follow their ids",
                cx.listener(|this, _, _, cx| this.set_sort(None, cx)),
            )
        })
    }

    /// The toolbar's Views menu: this type's saved views, then save and delete.
    fn render_views_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let views = self.saved_views(cx);
        let active = self.active_view(&views);
        let can_save = !self.filters.is_empty()
            || self.sort.is_some()
            || self.grouping.is_some()
            || self.layout != ColumnLayout::default();
        let table = cx.entity().downgrade();
        let graph = self.session.read(cx).overrides_key();
        let type_name = self.type_name.clone();
        let names = match self.kind {
            TableKind::Node => self.session.read(cx).type_naming(&self.type_name),
            TableKind::Edge => None,
        };
        let open = self.toolbar_menus[0].is_deployed();
        let shown = active.is_some();
        let menu = PopoverMenu::new("views-menu")
            .anchor(gpui::Anchor::TopRight)
            .with_handle(self.toolbar_menus[0].clone())
            .trigger(
                studio_ui::ds::chip_button(
                    "views",
                    IconName::Star,
                    "Views",
                    active.clone().map(SharedString::from),
                )
                .when(!open, |button| {
                    button.tooltip(Tooltip::text(
                        "Saved filters, order, grouping and columns for this type",
                    ))
                }),
            )
            .menu(move |window, cx| {
                let views = views.clone();
                let active = active.clone();
                let table = table.clone();
                let graph = graph.clone();
                let type_name = type_name.clone();
                let names = names.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    if views.is_empty() {
                        menu = menu.header("No saved views");
                    } else {
                        menu = menu.header("Views");
                    }
                    for view in views {
                        let checked = active.as_deref() == Some(view.name.as_str());
                        let table = table.clone();
                        let shown = view.clone();
                        menu = menu.toggleable_entry(
                            view.name.clone(),
                            checked,
                            IconPosition::Start,
                            None,
                            move |_, cx| {
                                table
                                    .update(cx, |table, cx| table.apply_view(&shown, cx))
                                    .ok();
                            },
                        );
                    }
                    menu = menu.separator();
                    let save_table = table.clone();
                    menu = menu.item(
                        ContextMenuEntry::new("Save view…")
                            .disabled(!can_save)
                            .handler(move |window, cx| {
                                save_table
                                    .update(cx, |table, cx| table.open_save_view(window, cx))
                                    .ok();
                            }),
                    );
                    if let Some(name) = active.clone() {
                        let graph = graph.clone();
                        menu = menu.entry(format!("Delete “{name}”"), None, move |_, cx| {
                            let (graph, type_name, name) =
                                (graph.clone(), type_name.clone(), name.clone());
                            studio_settings::update(cx, move |settings| {
                                settings.delete_view(&graph, &type_name, &name)
                            });
                        });
                    }
                    if let Some(names) = names.clone() {
                        menu = menu.separator().submenu("Name rows by", move |menu, _, _| {
                            studio_ui::node_names::add_to(menu, &names)
                        });
                    }
                    menu
                }))
            });
        // Leaving a view shows the table as it first opens: no filter, order or grouping, and
        // every column in its first place. The view stays saved.
        studio_ui::ds::control_chip("views-chip", menu).when(shown, |chip| {
            chip.on_clear(
                "Leave this view: no filter, order or grouping, every column",
                cx.listener(|this, _, _, cx| {
                    this.apply_view(&studio_settings::SavedView::default(), cx)
                }),
            )
        })
    }

    /// Restores a remembered order, if the property can still order the table.
    /// Before the schema is read, the order is kept as asked and checked when it is.
    pub fn set_sort(&mut self, sort: Option<Sort>, cx: &mut Context<Self>) {
        if self.schema.is_none() {
            self.sort = sort;
            return;
        }
        let allowed = sort.as_ref().is_none_or(|sort| self.can_sort_by(sort));
        if allowed && sort != self.sort {
            self.sort = sort;
            self.reload(true, cx);
        }
    }

    fn can_sort_by(&self, sort: &Sort) -> bool {
        self.all_columns
            .iter()
            .filter_map(ColumnSpec::property)
            .any(|p| p.name == sort.property && Sort::allowed(p))
    }

    /// The filters with each date preset's days computed now, in local time: a preset holds no
    /// value of its own, so a row is judged against these, as the engine is asked with them.
    fn resolved_filters(&self) -> Vec<Filter> {
        // Reading the local time zone is not free: only a preset needs it.
        if !self.filters.iter().any(|filter| filter.op.is_date_preset()) {
            return self.filters.clone();
        }
        let now = chrono::Local::now();
        self.filters
            .iter()
            .map(|filter| {
                if !filter.op.is_date_preset() {
                    return filter.clone();
                }
                // Of every column, not those shown: a filter can be on a hidden one.
                let kind = self
                    .all_columns
                    .iter()
                    .filter_map(ColumnSpec::property)
                    .find(|p| p.name == filter.property)
                    .map(|p| p.kind);
                Filter {
                    value: preset_range(filter.op, kind, now),
                    ..filter.clone()
                }
            })
            .collect()
    }

    /// The count of the table's rows, through its filters (`filtered`) or of every row; `None`
    /// when it is 0 without a read: a filter on a property the commit shown does not hold
    /// matches no row there.
    fn count_query(&self, filtered: bool) -> Result<Option<BuiltQuery>, BuildError> {
        let schema = self
            .schema
            .as_ref()
            .ok_or_else(|| BuildError::UnknownType(self.type_name.clone()))?;
        let filters = if filtered {
            self.resolved_filters()
        } else {
            Vec::new()
        };
        let without: Vec<String> = self.absent.keys().cloned().collect();
        let Some(filters) = builder::filters_without(&filters, &without) else {
            return Ok(None);
        };
        match self.kind {
            TableKind::Node => builder::node_count(schema, &self.type_name, &filters),
            TableKind::Edge => builder::edge_count(schema, &self.type_name, &filters),
        }
        .map(Some)
    }

    fn rows_from(&self, rows: &Value) -> Vec<Row> {
        match self.kind {
            TableKind::Node if !self.absent.is_empty() => field_rows(rows),
            TableKind::Node => node_rows(rows),
            TableKind::Edge => self
                .schema
                .as_ref()
                .and_then(|schema| schema.edge(&self.type_name))
                .map(|edge| edge_rows(edge, rows))
                .unwrap_or_default(),
        }
    }

    /// Reads the first page again. `keep_pin` re-reads the same commit (new filters);
    /// otherwise the table moves to the branch head (Refresh).
    fn reload(&mut self, keep_pin: bool, cx: &mut Context<Self>) {
        if self.forget_rows(keep_pin, cx) {
            self.rows_owed = false;
            self.read_rows(cx);
        }
    }

    /// [`Self::reload`] at the branch head, its reads made when the table is next drawn
    /// ([`Self::rows_owed`]): what it showed goes now, so a table in view shows it reading.
    fn owe_reload(&mut self, cx: &mut Context<Self>) {
        if self.forget_rows(false, cx) {
            self.rows_owed = true;
            cx.notify();
        }
    }

    /// Lets go of the rows read, and of the reads out for them, before they are read again
    /// ([`Self::reload`]); `false` when there is nothing to read (no schema yet, or the type is
    /// gone).
    fn forget_rows(&mut self, keep_pin: bool, cx: &mut Context<Self>) -> bool {
        if self.schema.is_none() || self.missing {
            return false;
        }
        if keep_pin {
            self.paging.restart();
        } else {
            // Another commit, or another schema: what it holds is found out again, and what it
            // shows is read again.
            self.absent.clear();
            if let Some(scope) = &self.scope {
                let branch = self.branch.clone();
                self.session
                    .update(cx, |session, _| session.reset_scope(scope, &branch));
            }
            self.paging.reset();
            // A table opened at a commit stays pinned to it (constitution II).
            self.paging.pinned = self.at_commit.clone();
            self.count_shift = None;
            self.newer = false;
            cx.emit(ItemEvent::UpdateTab);
        }
        // An edit open over the rows read before is kept as its cell's draft, as leaving the
        // cell keeps it: a Refresh that lands while the person types loses nothing. It is
        // judged while its row is still held, so text over a value the row shows since stays.
        if let Some(editing) = self.editing.take() {
            self.keep_edit(editing, cx);
        }
        self.rows.clear();
        self.placed_here.clear();
        self.cell_text.borrow_mut().clear();
        // At the same commit a row's relationships and Blob cells cannot have changed: a new
        // order or filter keeps what was read and reads only rows not seen yet.
        if !keep_pin {
            self.blobs.clear();
            self.blob_tasks.clear();
            self.blob_queue.clear();
            // A file still being stored stays shown as going; why one did not land goes.
            self.blob_edits.retain(|_, edit| edit.is_saving());
            self.relations.clear();
            self.relation_tasks.clear();
            self.relations_asked.clear();
        }
        self.lines_moved();
        // The view scrolls back to its first line (below): until a frame draws it, the rows
        // near the lines drawn, and how far the next page is ahead of them, are judged there.
        self.lines_drawn = 0..0;
        self._blob_wait = None;
        self.cursor = None;
        // Picked rows stay picked by id; the row a range starts from is a place in the old
        // order.
        self.selection_anchor = None;
        self.loading_more = false;
        self.more_error = None;
        self.first_page = Loadable::Loading;
        self.count = Loadable::Loading;
        self.total = if self.filters.is_empty() {
            Loadable::NotLoaded
        } else {
            Loadable::Loading
        };
        // A page or section read for what the table showed before must not land: the grouped
        // path and a query that cannot be built start none of their own.
        self.page_task = None;
        self.section_tasks.clear();
        self.dropped_sections.clear();
        self.count_task = None;
        self.total_task = None;
        self.table_state
            .read(cx)
            .scroll_handle
            .scroll_to_item(0, ScrollStrategy::Top);
        true
    }

    /// Reads the rows [`Self::forget_rows`] let go of: the sections of a grouping, else the
    /// first page.
    fn read_rows(&mut self, cx: &mut Context<Self>) {
        if self.grouping.is_some() {
            self.read_groups(cx);
        } else {
            self.read_first_page(cx);
        }
        cx.notify();
    }

    /// Reads the first page. Read at a pin the table's own write moved since it was sent, it
    /// holds the commit before that write: a cell's old value, a deleted row, no inserted one.
    /// It is read again at the pin rather than shown there (constitution II). A pin carried
    /// forward meanwhile to a head that changed nothing the table reads keeps it: what it holds
    /// is what the head holds.
    fn read_first_page(&mut self, cx: &mut Context<Self>) {
        let size = self.page_size(cx).saturating_mul(2);
        let query = match self.page_query(&PageStart::First, size, None) {
            Ok(query) => query,
            Err(error) => {
                self.first_page = Loadable::Failed(error.to_string());
                cx.notify();
                return;
            }
        };
        // A first read goes to the head the session holds, as the groups' read does, so the pin
        // is a commit the session compares its next head with.
        let asked = self.head_asked(cx);
        let snapshot = self.paging.pinned.clone().or_else(|| asked.clone());
        let sent = self.own_writes;
        let read = self.read_page(&query, &PageStart::First, None, snapshot, cx);
        // Pinned already (a filter or the order changed), the counts read at the same commit:
        // they go now, beside the page, rather than once it is in.
        let counted = self.paging.pinned.is_some();
        if counted {
            self.load_counts(cx);
        }
        self.page_task = Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, cx| {
                if this.own_write_since(sent) {
                    this.paging.restart();
                    this.read_first_page(cx);
                    return;
                }
                this.apply_page(result, Some(counted), size, cx);
                this.pinned_at(asked, cx);
            })
            .ok();
        }));
    }

    /// Whether one of the table's own writes moved its pin since a read was sent, when
    /// [`Self::own_writes`] was `sent`: the read answers for the commit before that write, and
    /// is read again at the pin. The pin moved only to a head that changed nothing the table
    /// reads ([`Self::compare_head`]) leaves the read as it is: it holds what the head holds, so
    /// another writer's commits to other types never send it again. A read sent before the
    /// table was pinned pins it.
    fn own_write_since(&mut self, sent: u64) -> bool {
        let again = self.own_writes != sent;
        #[cfg(test)]
        {
            self.reads_sent_again += usize::from(again);
        }
        again
    }

    /// One of the table's own writes landed at `commit`, made on `parent`
    /// ([`Paging::advance_for_own_commit`]): the pin follows it when it was made on the pin, and
    /// every read out then is read again ([`Self::own_write_since`]). Whether the table is
    /// behind its branch now.
    fn follow_own_commit(&mut self, parent: Option<&str>, commit: &str) -> bool {
        let before = self.paging.pinned.clone();
        let behind = self.paging.advance_for_own_commit(parent, commit);
        if self.paging.pinned != before {
            self.own_writes += 1;
        }
        behind
    }

    /// The head the session holds as a first read is sent, which pins the table: `None` once
    /// it is pinned.
    fn head_asked(&self, cx: &App) -> Option<String> {
        if self.paging.pinned.is_some() {
            return None;
        }
        self.session.read(cx).head(&self.branch).map(str::to_owned)
    }

    /// A first read pinned the table. A head move the session saw while the read was out found
    /// no pin to compare, so a pin at the head the session held when it asked (`asked`) is
    /// compared now. A read sent before the session held a head pinned the server's, and is
    /// compared once the session sees one.
    fn pinned_at(&mut self, asked: Option<String>, cx: &mut Context<Self>) {
        if asked.is_some() && self.paging.pinned == asked {
            self.compare_head(cx);
        }
    }

    /// Rows per page for this graph (setting `studio.type_table_page_size`, or the graph's own).
    fn page_size(&self, cx: &App) -> u32 {
        self.session
            .read(cx)
            .options(cx)
            .type_table_page_size
            .max(1)
    }

    fn load_more(&mut self, cx: &mut Context<Self>) {
        if self.grouping.is_some()
            || self.loading_more
            || self.paging.exhausted
            || !matches!(self.first_page, Loadable::Loaded(()))
        {
            return;
        }
        let size = self.paging.next_page_size(self.page_size(cx));
        let start = self.paging.start.clone();
        let query = match self.page_query(&start, size, None) {
            Ok(query) => query,
            Err(error) => {
                self.more_error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        self.loading_more = true;
        self.more_error = None;
        let sent = self.own_writes;
        let read = self.read_page(&query, &start, None, self.paging.pinned.clone(), cx);
        self.page_task = Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, cx| {
                // Read before the table's own write moved the pin: read again there, from the
                // same place (see `read_first_page`).
                if this.own_write_since(sent) {
                    this.loading_more = false;
                    this.load_more(cx);
                    return;
                }
                this.apply_page(result, None, size, cx)
            })
            .ok();
        }));
        cx.notify();
    }

    /// What the rows can be grouped by: each groupable property, then what each submenu of
    /// [`Self::group_menus`] offers. Key and how the choice reads.
    fn group_choices(&self) -> Rc<Vec<(String, String)>> {
        self.grouping_choices.choices.clone()
    }

    /// The Group menu's submenus, as [`Self::build_group_menus`] built them for these columns.
    fn group_menus(&self) -> Rc<Vec<GroupMenu>> {
        self.grouping_choices.menus.clone()
    }

    /// Builds what the Group menu offers, once the columns are known: the toolbar draws it on
    /// every render, and a type with dozens of relationships offers thousands of choices.
    fn build_grouping_choices(&mut self) {
        let menus = self.build_group_menus();
        let choices = self.build_group_choices(&menus);
        self.grouping_choices = GroupingChoices {
            menus: Rc::new(menus),
            choices: Rc::new(choices),
        };
    }

    fn build_group_choices(&self, menus: &[GroupMenu]) -> Vec<(String, String)> {
        let mut choices: Vec<(String, String)> = self
            .all_columns
            .iter()
            .filter_map(ColumnSpec::property)
            .filter(|p| builder::GroupBy::allowed(p))
            .map(|p| (p.name.clone(), p.name.clone()))
            .collect();
        for menu in menus {
            choices.extend(menu.neighbour.clone());
            choices.extend(
                menu.properties
                    .iter()
                    .map(|(key, name)| (key.clone(), format!("{}.{name}", menu.prefix))),
            );
        }
        choices
    }

    /// The Group menu's submenus: in a node table one per relationship, by each neighbour or
    /// by a property of the neighbours; in an edge table one per end, by a property of the
    /// node at it.
    fn build_group_menus(&self) -> Vec<GroupMenu> {
        let Some(schema) = &self.schema else {
            return Vec::new();
        };
        let groupable = |type_name: &str| -> Vec<String> {
            schema.node(type_name).map_or_else(Vec::new, |node| {
                node.properties
                    .iter()
                    .filter(|p| builder::GroupBy::allowed(p))
                    .map(|p| p.name.clone())
                    .collect()
            })
        };
        self.all_columns
            .iter()
            .filter_map(|column| match &column.role {
                ColumnRole::Relationship(group) => Some(GroupMenu {
                    // The icon shows the direction, so the label names the traversal only.
                    label: format!(
                        "{} {}",
                        gq_language::traversal_name(&group.edge),
                        group.other_type
                    ),
                    prefix: format!("{} {}", column.title, group.other_type),
                    icon: direction_icon(group.direction),
                    neighbour: Some((column.key.clone(), column.title.clone())),
                    other_type: group.other_type.clone(),
                    properties: groupable(&group.other_type)
                        .into_iter()
                        .map(|name| (builder::neighbour_property_key(group, &name), name))
                        .collect(),
                }),
                ColumnRole::Endpoint { node_type } => {
                    let end = if column.key == crate::model::FROM_COLUMN {
                        builder::EdgeEnd::From
                    } else {
                        builder::EdgeEnd::To
                    };
                    Some(GroupMenu {
                        label: column.title.clone(),
                        prefix: column.title.clone(),
                        icon: match end {
                            builder::EdgeEnd::From => IconName::ArrowLeft,
                            builder::EdgeEnd::To => IconName::ArrowRight,
                        },
                        neighbour: None,
                        other_type: node_type.clone(),
                        properties: groupable(node_type)
                            .into_iter()
                            .map(|name| (builder::endpoint_property_key(end, &name), name))
                            .collect(),
                    })
                }
                _ => None,
            })
            .filter(|menu| menu.neighbour.is_some() || !menu.properties.is_empty())
            .collect()
    }

    /// The property a grouping key reads: the row's own, a neighbour's or an end's.
    fn group_property(&self, key: &str) -> Option<Property> {
        let schema = self.schema.as_ref()?;
        let (type_name, property) = match builder::parse_group_key(key) {
            builder::GroupKey::Property(_) => return self.property_of(key),
            builder::GroupKey::Relationship(..) => return None,
            builder::GroupKey::NeighbourProperty {
                direction,
                edge,
                property,
            } => {
                let group = schema
                    .relationships_of(&self.type_name)
                    .iter()
                    .find(|group| group.direction == direction && group.edge == edge)?;
                (group.other_type.clone(), property)
            }
            builder::GroupKey::EndpointProperty { end, property } => {
                let edge = schema.edge(&self.type_name)?;
                let node_type = match end {
                    builder::EdgeEnd::From => edge.from.clone(),
                    builder::EdgeEnd::To => edge.to.clone(),
                };
                (node_type, property)
            }
        };
        schema.node(&type_name)?.property(property).cloned()
    }

    fn can_group_by(&self, key: &str) -> bool {
        self.group_choices().iter().any(|(choice, _)| choice == key)
    }

    /// Groups the rows by a property or a relationship, or stops; the table is read again at
    /// the same commit. Before the schema is read, the grouping is kept as asked and checked
    /// when it is.
    pub fn set_grouping(&mut self, property: Option<String>, cx: &mut Context<Self>) {
        if self.schema.is_none() {
            self.grouping = property.map(Grouping::new);
            return;
        }
        let property = property.filter(|key| self.can_group_by(key));
        if self.grouping.as_ref().map(|g| &g.property) == property.as_ref() {
            return;
        }
        self.grouping = property.map(Grouping::new);
        self.reload(true, cx);
    }

    /// Reads how many rows hold each value of the grouping property, under the filters.
    fn read_groups(&mut self, cx: &mut Context<Self>) {
        let (Some(schema), Some(grouping)) = (self.schema.clone(), self.grouping.as_mut()) else {
            return;
        };
        let generation = grouping.restart();
        let property = grouping.property.clone();
        // The sections are read again, and with them which reads are owed.
        self.dropped_sections.clear();
        // At a commit made before a property was added or renamed, the groups are counted
        // without filters on it, as the rows are read; one asking for a value matches no row.
        let without: Vec<String> = self.absent.keys().cloned().collect();
        let Some(filters) = builder::filters_without(&self.resolved_filters(), &without) else {
            if let Some(grouping) = self.grouping.as_mut() {
                grouping.accept_no_rows();
            }
            self.lines_moved();
            self.first_page = Loadable::Loaded(());
            self.load_counts(cx);
            cx.notify();
            return;
        };
        // By neighbours: the rows per neighbour or per value they hold, and apart from them the
        // rows with no neighbour, or none with a value.
        let with_none = |op: FilterOp| {
            let mut none = filters.clone();
            none.push(Filter {
                property: property.clone(),
                op,
                value: Value::Null,
            });
            builder::node_count(&schema, &self.type_name, &none)
        };
        let queries = match builder::parse_group_key(&property) {
            builder::GroupKey::Relationship(..) => builder::neighbour_counts(
                &schema,
                &self.type_name,
                &filters,
                &property,
                GROUP_LIMIT,
            )
            .and_then(|counts| Ok((counts, Some(with_none(FilterOp::HasNone)?)))),
            builder::GroupKey::NeighbourProperty { .. } => builder::neighbour_value_counts(
                &schema,
                &self.type_name,
                &filters,
                &property,
                GROUP_LIMIT,
            )
            .and_then(|counts| Ok((counts, Some(with_none(FilterOp::IsEmpty)?)))),
            builder::GroupKey::EndpointProperty { .. } => builder::endpoint_value_counts(
                &schema,
                &self.type_name,
                &filters,
                &property,
                GROUP_LIMIT,
            )
            .map(|counts| (counts, None)),
            builder::GroupKey::Property(_) => builder::value_counts(
                &schema,
                &self.type_name,
                self.kind == TableKind::Edge,
                &filters,
                &property,
                GROUP_LIMIT,
            )
            .map(|counts| (counts, None)),
        };
        let (query, none) = match queries {
            Ok(queries) => queries,
            Err(error) => {
                self.first_page = Loadable::Failed(error.to_string());
                return;
            }
        };
        let snapshot = self.paging.pinned.clone();
        let sent = self.own_writes;
        // The rows with none are counted at the commit the groups are read at. Known before
        // either is read (the table's pin, or the head the session saw), both go at once;
        // otherwise the groups' answer names it.
        let known = snapshot.clone().or_else(|| {
            let session = self.session.read(cx);
            session.head(&self.branch).map(str::to_owned)
        });
        let asked = self.head_asked(cx);
        let read = self.read_shown(&query, known.clone().or(snapshot), cx);
        let none_at_once = known
            .as_ref()
            .zip(none.as_ref())
            .map(|(commit, none)| self.read_shown(none, Some(commit.clone()), cx));
        self.page_task = Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            let none = match (none_at_once, none, &result) {
                (Some(read), ..) => Some(read.await),
                (None, Some(none), Ok(output)) => {
                    let commit = output.graph_commit_id.clone();
                    let Ok(read) = this.update(cx, |this, cx| this.read_shown(&none, commit, cx))
                    else {
                        return;
                    };
                    Some(read.await)
                }
                _ => None,
            };
            this.update(cx, |this, cx| {
                // Counted before the table's own write moved the pin: counted again there.
                let current = this.grouping.as_ref().map(|g| g.generation) == Some(generation);
                if current && this.own_write_since(sent) {
                    this.read_groups(cx);
                    cx.notify();
                    return;
                }
                this.apply_groups(generation, result, none, cx);
                this.pinned_at(asked, cx);
            })
            .ok();
        }));
    }

    /// Takes the grouped counts, and when grouped by neighbours the count of rows with no
    /// neighbour, or none with a value. Either failing fails the grouping.
    fn apply_groups(
        &mut self,
        generation: u64,
        result: Result<ReadOutput, ReadError>,
        none: Option<Result<ReadOutput, ReadError>>,
        cx: &mut Context<Self>,
    ) {
        if self.grouping.as_ref().map(|g| g.generation) != Some(generation) {
            return;
        }
        let none =
            none.map(|read| read.map(|output| count_of(&output.rows).unwrap_or(0).max(0) as u64));
        let result = match (result, none) {
            (Ok(output), Some(Ok(none))) => Ok((output, Some(none))),
            (Ok(output), None) => Ok((output, None)),
            (Err(error), _) | (_, Some(Err(error))) => Err(error),
        };
        // Refused for a property the commit shown does not hold: read again without it, as the
        // rows are ([`Self::apply_page`]).
        if let Err(ReadError::Refused(refusal)) = &result
            && self.read_without(refusal.historical_gap())
        {
            self.reload(true, cx);
            return;
        }
        let Some(grouping) = self.grouping.as_mut() else {
            return;
        };
        match result {
            Ok((output, none)) => {
                if self.paging.pinned.is_none() {
                    self.paging.pinned = output.graph_commit_id.clone();
                }
                grouping.accept_counts(&output, GROUP_LIMIT);
                if let Some(none) = none {
                    grouping.add_none_section(none);
                }
                // The sections' lines replace whatever the lines drawn showed.
                self.lines_moved();
                self.first_page = Loadable::Loaded(());
                self.load_counts(cx);
            }
            Err(error) => {
                grouping.sections = Loadable::Failed(error.to_string());
                self.first_page = Loadable::Failed(error.to_string());
                self.count = Loadable::NotLoaded;
            }
        }
        cx.notify();
    }

    /// Reads the next rows of one section: its first few when it shows, a page on "Show more".
    /// A read of it dropped away from the view is owed no more once another is asked for.
    fn load_section(&mut self, index: usize, size: u32, cx: &mut Context<Self>) {
        self.dropped_sections.remove(&index);
        let Some(grouping) = &self.grouping else {
            return;
        };
        let generation = grouping.generation;
        let (Some(filter), Some(section)) =
            (grouping.filter_for(index), grouping.sections().get(index))
        else {
            return;
        };
        if section.loading || !section.has_more() {
            return;
        }
        let start = section.paging.start.clone();
        let query = self.page_query(&start, size, Some(&filter));
        let Some(section) = self.grouping.as_mut().and_then(|g| g.section_mut(index)) else {
            return;
        };
        let query = match query {
            Ok(query) => query,
            Err(error) => {
                section.error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        section.loading = true;
        section.error = None;
        let sent = self.own_writes;
        let read = self.read_page(
            &query,
            &start,
            Some(&filter),
            self.paging.pinned.clone(),
            cx,
        );
        let task = cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, cx| {
                this.section_tasks.remove(&index);
                // Read before the table's own write moved the pin: read again there, from the
                // same place (see `read_first_page`).
                let current = this.grouping.as_ref().map(|g| g.generation) == Some(generation);
                if current && this.own_write_since(sent) {
                    if let Some(section) = this.grouping.as_mut().and_then(|g| g.section_mut(index))
                    {
                        section.loading = false;
                    }
                    this.load_section(index, size, cx);
                    return;
                }
                this.apply_section_page(generation, index, result, size, cx)
            })
            .ok();
        });
        self.section_tasks.insert(
            index,
            SectionRead {
                rows: size,
                _task: task,
            },
        );
        cx.notify();
    }

    fn apply_section_page(
        &mut self,
        generation: u64,
        index: usize,
        result: Result<ReadOutput, ReadError>,
        size: u32,
        cx: &mut Context<Self>,
    ) {
        if self.grouping.as_ref().map(|g| g.generation) != Some(generation) {
            return;
        }
        // The lines in view as they were before the section took the read: its read line may
        // go with it.
        let before = self.first_line_in_view(cx);
        let page = match result {
            Ok(output) => {
                let page = self.rows_from(&output.rows);
                let sort_column = self.sort.as_ref().map(|sort| sort.property.clone());
                let nulls_follow = self.sort_nulls_follow();
                if let Some(section) = self.grouping.as_mut().and_then(|g| g.section_mut(index)) {
                    section.loading = false;
                    section.paging.accept(
                        output.graph_commit_id.as_deref(),
                        &page,
                        size,
                        size,
                        sort_column.as_deref(),
                        nulls_follow,
                    );
                }
                let page = self.not_placed_here(page);
                self.not_shown_already(page, Some(index))
            }
            // Refused for a property the commit shown does not hold: read again without it, as
            // the rows are ([`Self::apply_page`]).
            Err(ReadError::Refused(refusal)) if self.read_without(refusal.historical_gap()) => {
                self.reload(true, cx);
                return;
            }
            Err(error) => {
                if let Some(section) = self.grouping.as_mut().and_then(|g| g.section_mut(index)) {
                    section.loading = false;
                    section.error = Some(error.to_string());
                }
                cx.notify();
                return;
            }
        };
        let Some(at) = self
            .grouping
            .as_ref()
            .and_then(|g| g.sections().get(index))
            .map(|section| section.range.end)
        else {
            return;
        };
        let count = page.len();
        let ids: Vec<String> = page.iter().map(|row| row.id.clone()).collect();
        self.rows.splice(at..at, page);
        if let Some(grouping) = &mut self.grouping {
            grouping.inserted(index, count);
        }
        self.rows_inserted(at, count);
        if let Some(before) = before {
            self.hold_lines_in_view(before, at, index, cx);
        }
        self.load_relations(ids, cx);
        cx.notify();
    }

    /// The first line in view, grouped: what it shows, and how many lines there are.
    fn first_line_in_view(&self, cx: &App) -> Option<(Line, usize)> {
        let grouping = self.grouping.as_ref()?;
        let lines = grouping.line_map();
        let height = self.line_height(cx);
        let state = self.table_state.read(cx).scroll_handle.0.borrow();
        let first = (-state.base_handle.offset().y / height).floor().max(0.) as usize;
        Some((lines.get(first)?, lines.len()))
    }

    /// How tall a grid line is, as the list last laid one out.
    fn line_height(&self, cx: &App) -> Pixels {
        let state = self.table_state.read(cx).scroll_handle.0.borrow();
        state
            .last_item_size
            .map(|size| size.item.height)
            .filter(|height| *height > px(0.))
            .unwrap_or(px(ROW_HEIGHT))
    }

    /// Keeps the lines in view where they were after a section's rows went in at row `at`, in
    /// section `index`: when they went in above the first line in view (`before`, what it
    /// showed and how many lines there were), the scroll moves by the lines added, so nothing
    /// in view moves (constitution V). Every line the read added or ended (the section's read
    /// line, once it has no more) is at the section's end.
    fn hold_lines_in_view(&self, before: (Line, usize), at: usize, index: usize, cx: &App) {
        let (first, len) = before;
        let below = match first {
            Line::Row(row) => row >= at,
            Line::Header(section) => section > index,
            Line::More(section) => section >= index,
            Line::OtherValues => true,
        };
        let Some(grouping) = self.grouping.as_ref().filter(|_| below) else {
            return;
        };
        let added = grouping.line_map().len() as f32 - len as f32;
        if added == 0. {
            return;
        }
        let height = self.line_height(cx);
        let state = self.table_state.read(cx).scroll_handle.0.borrow();
        let offset = state.base_handle.offset();
        let y = (offset.y - height * added).min(Pixels::ZERO);
        state.base_handle.set_offset(point(offset.x, y));
    }

    /// `page` without the rows the table shows already, once its pin moved to one of its own
    /// commits ([`Paging::moved`]): read there, a row the table's write moved further on in the
    /// order (a title edited from "B" to "Zed"), or into another section, comes again, and stays
    /// where it was read (constitution V). A row shows once in a table, or grouped by its own
    /// property; grouped otherwise it may show under several sections, once in each (`section`).
    /// A section counts only the rows it shows, so one skipped here leaves its count to the
    /// rows still unread.
    fn not_shown_already(&self, page: Vec<Row>, section: Option<usize>) -> Vec<Row> {
        if !self.paging.moved || page.is_empty() {
            return page;
        }
        let within = match (section, &self.grouping) {
            (Some(index), Some(grouping)) if !grouping.by_own_property() => {
                grouping.sections().get(index).map(|s| s.range.clone())
            }
            _ => None,
        };
        let rows = match within {
            Some(range) => self.rows.get(range).unwrap_or_default(),
            None => self.rows.as_slice(),
        };
        let shown: HashSet<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        page.into_iter()
            .filter(|row| !shown.contains(row.id.as_str()))
            .collect()
    }

    /// `page` without the rows shown already where they were made ([`Self::placed_here`]).
    fn not_placed_here(&mut self, page: Vec<Row>) -> Vec<Row> {
        if self.placed_here.is_empty() {
            return page;
        }
        page.into_iter()
            .filter(|row| !self.placed_here.remove(&row.id))
            .collect()
    }

    /// Keeps the cursor and the selection anchor on their rows after `count` rows went in at
    /// `at`. The rows drawn are asked for again: their places hold other rows now
    /// ([`Self::lines_moved`]).
    fn rows_inserted(&mut self, at: usize, count: usize) {
        if let Some(cursor) = &mut self.cursor
            && cursor.row >= at
        {
            cursor.row += count;
        }
        if let Some(anchor) = &mut self.selection_anchor
            && *anchor >= at
        {
            *anchor += count;
        }
        self.lines_moved();
    }

    /// The lines drawn hold other rows than they did: rows went in or out, or a section folded
    /// or opened. Their relationship and Blob cells are asked for again, as the frame that draws
    /// them would not otherwise: it asks only when the lines it draws are not the ones it last
    /// asked for, and these are the same places.
    fn lines_moved(&mut self) {
        self.relation_lines = None;
        self.blob_lines = None;
    }

    fn toggle_section(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(grouping) = self.grouping.as_mut() else {
            return;
        };
        let Some(section) = grouping.section_mut(index) else {
            return;
        };
        section.expanded = !section.expanded;
        // A row folded away is not edited, deleted or stepped from.
        let folded = !section.expanded;
        if folded
            && self
                .cursor
                .is_some_and(|cursor| grouping.section_of(cursor.row) == Some(index))
        {
            self.cursor = None;
        }
        // The rows of the sections after it move up or down into the lines drawn.
        self.lines_moved();
        cx.notify();
    }

    /// Sets a cell of the row at `ix`, and of every other copy of that row: grouped by a
    /// relationship, a row related to two neighbours shows in both sections.
    fn set_cell(&mut self, ix: usize, column: &str, value: Value) {
        let places = self.places_of(ix);
        self.set_cell_at(&places, column, value);
    }

    /// Dims the row at `ix` and its copies, or shows them plainly again.
    fn set_dimmed(&mut self, ix: usize, dimmed: bool) {
        let places = self.places_of(ix);
        self.set_dimmed_at(&places, dimmed);
    }

    /// Where the row at `ix` shows: there, and at each of its copies.
    fn places_of(&self, ix: usize) -> Vec<usize> {
        self.rows
            .get(ix)
            .map_or_else(Vec::new, |row| self.places_of_id(&row.id))
    }

    /// Where the row with `id` shows, at each of its copies; nowhere when it is not loaded.
    fn places_of_id(&self, id: &str) -> Vec<usize> {
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.id == id)
            .map(|(ix, _)| ix)
            .collect()
    }

    /// Where each loaded row shows, by id: a batch looks its rows up in one walk of the table,
    /// not one per row.
    fn row_places(&self) -> HashMap<String, Vec<usize>> {
        let mut places: HashMap<String, Vec<usize>> = HashMap::new();
        for (ix, row) in self.rows.iter().enumerate() {
            places.entry(row.id.clone()).or_default().push(ix);
        }
        places
    }

    /// Sets a cell of one row at every place it shows (`places`).
    fn set_cell_at(&mut self, places: &[usize], column: &str, value: Value) {
        let Some(&first) = places.first() else {
            return;
        };
        for &ix in places {
            set_value(&mut self.rows[ix], column, value.clone());
        }
        if let Some(row) = self.cell_text.borrow_mut().get_mut(&self.rows[first].id) {
            row.remove(column);
        }
    }

    /// Puts back what a row showed (`shown`) before a save that did not land put `new` in its
    /// cell (`places`): a value the table read, where the value the save was bound to may not
    /// be (a draft typed before a Refresh), so a list's next save is not bound to the pin for
    /// it. A row read again while the save was out shows what was read, and keeps it.
    fn put_back(&mut self, places: &[usize], column: &str, new: &Value, shown: Value) {
        if places
            .first()
            .is_some_and(|&ix| same_value(self.rows[ix].value(column), new))
        {
            self.set_cell_at(places, column, shown);
        }
    }

    /// Dims one row at every place it shows (`places`), or shows it plainly again.
    fn set_dimmed_at(&mut self, places: &[usize], dimmed: bool) {
        for &ix in places {
            self.rows[ix].dimmed = dimmed;
        }
    }

    /// Whether a row still belongs where it is shown: it matches the filters and, grouped, holds
    /// its section's value.
    fn row_fits(&self, ix: usize) -> bool {
        self.row_fits_under(&self.resolved_filters(), ix)
    }

    /// [`Self::row_fits`] under `filters`, the table's filters resolved once for a batch of
    /// rows ([`Self::resolved_filters`]).
    fn row_fits_under(&self, filters: &[Filter], ix: usize) -> bool {
        let Some(row) = self.rows.get(ix) else {
            return true;
        };
        if !row_matches(filters, &row.values) {
            return false;
        }
        match &self.grouping {
            // A cell edit cannot move a row to another neighbour, or change what its neighbours
            // hold.
            Some(grouping) if !grouping.by_own_property() => true,
            Some(grouping) => grouping
                .section_of(ix)
                .and_then(|section| grouping.sections().get(section))
                .is_none_or(|section| same_value(row.value(&grouping.property), &section.value)),
            None => true,
        }
    }

    /// Takes a page: the first (`first`, whether its counts went with it), or a later one.
    fn apply_page(
        &mut self,
        result: Result<ReadOutput, ReadError>,
        first: Option<bool>,
        size: u32,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(output) => {
                let page = self.rows_from(&output.rows);
                let base = self.page_size(cx);
                let nulls_follow = self.sort_nulls_follow();
                let sort_column = self.sort.as_ref().map(|sort| sort.property.as_str());
                self.paging.accept(
                    output.graph_commit_id.as_deref(),
                    &page,
                    size,
                    base,
                    sort_column,
                    nulls_follow,
                );
                let page = self.not_placed_here(page);
                let page = self.not_shown_already(page, None);
                let ids: Vec<String> = page.iter().map(|row| row.id.clone()).collect();
                let added = !page.is_empty();
                self.rows.extend(page);
                self.load_relations(ids, cx);
                match first {
                    Some(counted) => {
                        self.first_page = Loadable::Loaded(());
                        if !counted {
                            self.load_counts(cx);
                        }
                    }
                    None => self.loading_more = false,
                }
                // The next page is asked for once no more than a page is left past the lines
                // drawn, so a scroll finds its rows read; the lines drawn ask for it as they
                // come near ([`Paging::wants_more`]). A read that showed no row and is not the
                // last (sorted, the rows with a value ran out, and those without one are next)
                // reads on at once: with none shown the table would say it has no rows while
                // its count says it has.
                let ahead = self.paging.wants_more(
                    self.rows.len(),
                    self.lines_drawn.end,
                    self.loading_more,
                );
                if ahead || (!added && !self.paging.exhausted) {
                    self.load_more(cx);
                }
            }
            Err(ReadError::Refused(refusal)) if self.read_without(refusal.historical_gap()) => {
                self.reload(true, cx);
                return;
            }
            Err(error) => {
                if first.is_some() {
                    // Counts sent with the page say nothing of rows that were not read.
                    self.count_task = None;
                    self.total_task = None;
                    self.first_page = Loadable::Failed(match &error {
                        ReadError::Refused(refusal) => match refusal.historical_gap() {
                            Some(HistoricalGap::Type { name }) if name == self.type_name => {
                                format!("{name} did not exist at this commit.")
                            }
                            _ => error.to_string(),
                        },
                        _ => error.to_string(),
                    });
                    self.count = Loadable::NotLoaded;
                } else {
                    self.loading_more = false;
                    self.more_error = Some(error.to_string());
                }
            }
        }
        cx.notify();
    }

    /// Notes a property the commit shown does not hold under its current name, so the rows are
    /// read again without it; false when the refusal names no new property of the table's type.
    /// Once per property, so the reads end.
    fn read_without(&mut self, gap: Option<HistoricalGap>) -> bool {
        let Some(HistoricalGap::Property {
            owner,
            property,
            then,
        }) = gap
        else {
            return false;
        };
        let known = self.schema.as_ref().is_some_and(|schema| match self.kind {
            TableKind::Node => schema
                .node(&self.type_name)
                .is_some_and(|node| node.property(&property).is_some()),
            TableKind::Edge => schema
                .edge(&self.type_name)
                .is_some_and(|edge| edge.properties.iter().any(|p| p.name == property)),
        });
        if owner != self.type_name || !known || self.absent.contains_key(&property) {
            return false;
        }
        self.absent.insert(property, then);
        true
    }

    /// Reads the relationship columns for one page's rows: per relationship, how many
    /// neighbours each row has, and the first few for the chips. Pinned to the page's commit.
    fn load_relations(&mut self, ids: Vec<String>, cx: &mut Context<Self>) {
        // A relationship column reads nothing until rows draw it.
        let groups = self.drawn_relationships();
        self.load_relation_groups(groups, ids, cx);
    }

    /// Once the frame being drawn is out, asks for what its last lines drawn need: their rows'
    /// relationships and Blob cells, and of the section reads out, only those near them. Else
    /// the line the list measures, asked for in between, would make every frame ask again.
    fn drawn(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.frame_due, true) {
            return;
        }
        cx.defer_in(window, |this, window, cx| {
            this.frame_due = false;
            if let Some(lines) = this.frame_line_range.take() {
                this.want_row_relations(lines.clone(), window, cx);
                this.want_blob_metas(lines.clone(), cx);
                this.drop_section_reads_away_from(lines);
            }
        });
    }

    /// The rows lines `lines` show, in line order: ungrouped, each line is its row; grouped,
    /// the rows of the open sections among them, as a folded section's rows are on no line.
    fn rows_of_lines(&self, lines: Range<usize>) -> Vec<usize> {
        match &self.grouping {
            None => (lines.start.min(self.rows.len())..lines.end.min(self.rows.len())).collect(),
            Some(grouping) => grouping
                .line_map()
                .slice(lines)
                .filter_map(|line| match line {
                    Line::Row(row) => Some(row),
                    _ => None,
                })
                .collect(),
        }
    }

    /// Notes the lines this frame draws; rows scrolled into view whose drawn relationships were
    /// never asked for read them once the frame is out.
    fn want_row_relations(
        &mut self,
        lines: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.relation_lines.as_ref() == Some(&lines) {
            return;
        }
        self.relation_lines = Some(lines.clone());
        self.lines_drawn = lines.clone();
        let groups = self.drawn_relationships();
        if groups.is_empty() {
            return;
        }
        let keys: Vec<String> = groups.iter().map(builder::relationship_key).collect();
        let rows = self.rows_of_lines(lines);
        let missing = rows.iter().filter_map(|&ix| self.rows.get(ix)).any(|row| {
            keys.iter().any(|key| {
                !self
                    .relations_asked
                    .contains(&(row.id.clone(), key.clone()))
            })
        });
        if missing {
            cx.defer_in(window, |this, _, cx| this.load_shown_relations(cx));
        }
    }

    /// The relationship columns rows drew in the last frame.
    fn drawn_relationships(&self) -> Vec<RelationshipGroup> {
        self.columns
            .iter()
            .filter(|column| self.relations_drawn.contains(&column.key))
            .filter_map(ColumnSpec::relationship)
            .cloned()
            .collect()
    }

    /// Notes the relationship columns this frame draws; one that was not drawn before reads
    /// its neighbours for the rows read so far, once the frame is out.
    fn want_relations(&mut self, drawn: &[bool], window: &mut Window, cx: &mut Context<Self>) {
        // Before the grid's first layout every column counts as drawn.
        let handle = &self.table_state.read(cx).horizontal_scroll_handle;
        if handle.bounds().size.width <= px(0.) {
            return;
        }
        let keys: HashSet<String> = self
            .columns
            .iter()
            .zip(drawn)
            .filter(|(column, drawn)| **drawn && column.relationship().is_some())
            .map(|(column, _)| column.key.clone())
            .collect();
        if keys == self.relations_drawn {
            return;
        }
        let new = !keys.is_subset(&self.relations_drawn);
        self.relations_drawn = keys;
        if new {
            cx.defer_in(window, |this, _, cx| this.load_shown_relations(cx));
        }
    }

    fn load_relation_groups(
        &mut self,
        groups: Vec<RelationshipGroup>,
        ids: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(schema) = self.schema.clone() else {
            return;
        };
        if ids.is_empty() || self.kind != TableKind::Node {
            return;
        }
        let snapshot = self.paging.pinned.clone();
        for group in groups {
            let key = builder::relationship_key(&group);
            // Rows whose relationship was read at this commit, or is being read, need no read.
            let ids: Vec<String> = ids
                .iter()
                .filter(|id| {
                    !self.relations.contains(id, &key)
                        && !self.relations_asked.contains(&((*id).clone(), key.clone()))
                })
                .cloned()
                .collect();
            if ids.is_empty() {
                continue;
            }
            self.relations_asked
                .extend(ids.iter().map(|id| (id.clone(), key.clone())));
            let Ok(degrees) = builder::page_degrees(&schema, &self.type_name, &group, &ids) else {
                continue;
            };
            let degrees = self.read_shown(&degrees, snapshot.clone(), cx);
            // The neighbours go out with the degrees, not after them: every neighbour of the
            // rows, by row then neighbour, up to the budget. Most pages' rows are all in it.
            // The degrees, in first, show the counts; the rows the answer holds whole show
            // their chips as it lands; the rows it cut short (a hub, or rows after one that
            // spent the budget) are read again, sized by their counts, so a hub never leaves
            // the rows after it bare.
            let first = builder::page_neighbours(
                &schema,
                &self.type_name,
                &group,
                &ids,
                RELATION_SAMPLE_BUDGET as u32,
            )
            .ok()
            .map(|query| self.read_shown(&query, snapshot.clone(), cx));
            let ids = ids.clone();
            let schema = schema.clone();
            let type_name = self.type_name.clone();
            let snapshot = snapshot.clone();
            self.next_relation_task += 1;
            let task_id = self.next_relation_task;
            let task = cx.spawn(async move |this, cx| {
                // How many neighbours each row has. An answer whose rows cannot be read fails
                // the column's counts, as a failed read does, rather than counting such a row as
                // having none.
                let counts: Result<HashMap<String, i64>, String> = match degrees.await {
                    Ok(output) => graph_store::page_counts(&output.rows)
                        .map(|counts| {
                            counts
                                .into_iter()
                                .map(|(id, n)| (id, i64::try_from(n).unwrap_or(i64::MAX)))
                                .collect()
                        })
                        .ok_or_else(|| "The neighbour counts could not be read".to_owned()),
                    Err(error) => Err(error.to_string()),
                };
                let shown = this.update(cx, |this, cx| {
                    for id in &ids {
                        let count = match &counts {
                            Ok(counts) => Ok(counts.get(id).copied().unwrap_or(0)),
                            Err(error) => Err(error.clone()),
                        };
                        let relation = Relation {
                            count,
                            neighbours: Vec::new(),
                            whole: false,
                        };
                        this.relations.insert(id.clone(), key.clone(), relation);
                    }
                    if counts.is_err() {
                        this.relation_tasks.remove(&task_id);
                    }
                    cx.notify();
                });
                let (Ok(counts), Ok(())) = (counts, shown) else {
                    return;
                };
                // The rows that show chips: each whole list in page order while the budget
                // lasts, none of more than a hub's.
                let (sampled, _) = relation_sample(&ids, &counts);
                let output = match first {
                    Some(read) if !sampled.is_empty() => read.await.ok(),
                    _ => None,
                };
                // Neither read nor wanted: the counts show alone, as a failed read leaves them.
                let Some(output) = output else {
                    this.update(cx, |this, _| this.relation_tasks.remove(&task_id))
                        .ok();
                    return;
                };
                let (whole, rest) = NeighbourLists::read(&output.rows).split(sampled, &counts);
                let shown = this.update(cx, |this, cx| {
                    this.show_neighbours(&key, whole);
                    if rest.is_empty() {
                        this.relation_tasks.remove(&task_id);
                    }
                    cx.notify();
                });
                if rest.is_empty() || shown.is_err() {
                    return;
                }
                let (rest, total) = relation_sample(&rest, &counts);
                let read =
                    builder::page_neighbours(&schema, &type_name, &group, &rest, total as u32)
                        .ok()
                        .and_then(|query| {
                            this.update(cx, |this, cx| {
                                this.read_shown(&query, snapshot.clone(), cx)
                            })
                            .ok()
                        });
                let output = match read {
                    Some(read) => read.await.ok(),
                    None => None,
                };
                this.update(cx, |this, cx| {
                    this.relation_tasks.remove(&task_id);
                    // A row this read does not hold whole either shows its count alone.
                    if let Some(output) = output {
                        let (whole, _) = NeighbourLists::read(&output.rows).split(rest, &counts);
                        this.show_neighbours(&key, whole);
                        cx.notify();
                    }
                })
                .ok();
            });
            self.relation_tasks.insert(task_id, task);
        }
    }

    /// Shows the chips of rows whose whole lists of neighbours through relationship `key` were
    /// read; their counts show already.
    fn show_neighbours(&mut self, key: &str, whole: Vec<(String, NeighbourList)>) {
        for (id, neighbours) in whole {
            if let Some(relation) = self.relations.get_mut(&id, key) {
                relation.neighbours = neighbours;
                relation.whole = true;
            }
        }
    }

    /// Counts the rows the first read showed, by reads at the pin ([`Self::read_counts`]).
    /// When the session's snapshot counts the pin's commit ([`Self::pinned_snapshot_count`]),
    /// its count of every row is the count, and only the count through the filters is read.
    /// Otherwise, while the count of every row is out, the session's snapshot's shows in its
    /// place when it may be the pin's ([`Self::snapshot_count`]), so the rows are counted as the
    /// first page shows; the read replaces it as it lands, so a snapshot of another commit shows
    /// only until then, and never past a Refresh.
    fn load_counts(&mut self, cx: &mut Context<Self>) {
        if let Some(n) = self.pinned_snapshot_count(cx) {
            if self.filters.is_empty() {
                self.count = Loadable::Loaded(n);
                self.count_task = None;
            } else {
                self.total = Loadable::Loaded(n);
                self.total_task = None;
                let query = self.count_query(true);
                let snapshot = self.paging.pinned.clone();
                self.count_task = self.spawn_count(query, snapshot, true, cx);
            }
            return;
        }
        let provisional = self.snapshot_count(cx);
        self.read_counts(cx);
        let Some(n) = provisional else {
            return;
        };
        let shown = if self.filters.is_empty() {
            &mut self.count
        } else {
            &mut self.total
        };
        // Only over a count still being read: one already in is the pin's own.
        if matches!(shown, Loadable::Loading) {
            *shown = Loadable::Loaded(n);
        }
    }

    /// Reads the counts at the pin: through the filters, and of every row (the count itself
    /// while no filter is set, else the total shown beside it).
    fn read_counts(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.paging.pinned.clone();
        let filtered = !self.filters.is_empty();
        if filtered {
            let query = self.count_query(true);
            self.count_task = self.spawn_count(query, snapshot.clone(), true, cx);
        }
        let query = self.count_query(false);
        let task = self.spawn_count(query, snapshot, !filtered, cx);
        if filtered {
            self.total_task = task;
        } else {
            self.count_task = task;
        }
    }

    /// How many rows the session's snapshot of the branch counts for the type, when that may
    /// be the count at the pin: the snapshot counts the branch head, so only while the pin is
    /// the head the session holds. `None` for a table held at a commit, for a pin the
    /// session's head moved past, while the table's own writes past its pin are counted
    /// ([`Self::count_shift`]), and while the session holds no count for the type. Unless the
    /// session found the snapshot's commit to be the pin ([`Self::pinned_snapshot_count`]), a
    /// commit between the session's snapshot and its head read can make it another commit's:
    /// it only stands in until the read at the pin lands ([`Self::load_counts`]).
    fn snapshot_count(&self, cx: &App) -> Option<i64> {
        if self.at_commit.is_some() || self.count_shift.is_some() {
            return None;
        }
        let pin = self.paging.pinned.as_deref()?;
        let session = self.session.read(cx);
        if session.branch != self.branch || session.head(&self.branch) != Some(pin) {
            return None;
        }
        session.count_of(self.entity_kind(), &self.type_name)
    }

    /// How many rows the session's snapshot counts for the type when it counts the pin itself:
    /// the session found the commit at the snapshot's manifest version
    /// ([`GraphSession::counts_commit`]), and it is the pin, whatever branch or commit the table
    /// is held at. `None` while the table's own writes past its pin are counted
    /// ([`Self::count_shift`]) and while the session holds no count for the type.
    fn pinned_snapshot_count(&self, cx: &App) -> Option<i64> {
        if self.count_shift.is_some() {
            return None;
        }
        let pin = self.paging.pinned.as_deref()?;
        let session = self.session.read(cx);
        if session.counts_commit() != Some(pin) {
            return None;
        }
        session.count_of(self.entity_kind(), &self.type_name)
    }

    /// The snapshot's kind for the table's.
    fn entity_kind(&self) -> graph_store::EntityKind {
        match self.kind {
            TableKind::Node => graph_store::EntityKind::Node,
            TableKind::Edge => graph_store::EntityKind::Edge,
        }
    }

    fn spawn_count(
        &mut self,
        query: Result<Option<BuiltQuery>, BuildError>,
        snapshot: Option<String>,
        filtered: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<()>> {
        let query = match query {
            Ok(Some(query)) => query,
            result => {
                let value = match result {
                    Err(error) => Loadable::Failed(error.to_string()),
                    _ => Loadable::Loaded(0),
                };
                if filtered {
                    self.count = value;
                } else {
                    self.total = value;
                }
                return None;
            }
        };
        let at = snapshot.clone();
        let read = self.read_shown(&query, snapshot, cx);
        Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, cx| {
                let shift = this.count_shift_at(at.as_deref(), filtered);
                let value = match result {
                    Ok(output) => match count_of(&output.rows) {
                        Some(n) => Loadable::Loaded(n + shift),
                        None => Loadable::Failed("The count came back empty".into()),
                    },
                    Err(error) => Loadable::Failed(error.to_string()),
                };
                if filtered {
                    this.count = value;
                } else {
                    this.total = value;
                }
                cx.notify();
            })
            .ok();
        }))
    }

    /// What the table's own writes past its pin add to a count read at `at`: to the count
    /// through the filters (`filtered`), or to the total. One counted under other filters than
    /// the table's adds nothing to the count through them: it is not known there.
    fn count_shift_at(&self, at: Option<&str>, filtered: bool) -> i64 {
        let shift = self.count_shift.as_ref();
        let Some(shift) = shift.filter(|shift| Some(shift.pin.as_str()) == at) else {
            return 0;
        };
        if !filtered || self.filters.is_empty() {
            shift.rows
        } else if shift.filters == self.filters {
            shift.matching
        } else {
            0
        }
    }

    /// Offers Refresh when the branch head the session last saw is not the commit the table
    /// shows, unless nothing the table reads changed in between: then the pin moves to the head
    /// and nothing is read again. While the table's own writes are out it waits for them: each
    /// moves the pin itself.
    fn compare_head(&mut self, cx: &mut Context<Self>) {
        self.awaiting_changes = false;
        if self.saves_in_flight > 0 {
            self.head_moved_while_saving = true;
            return;
        }
        if self.newer || self.at_commit.is_some() {
            return;
        }
        let Some(pin) = self.paging.pinned.clone() else {
            return;
        };
        let Some(scope) = &self.scope else {
            return;
        };
        let session = self.session.read(cx);
        let Some(head) = session.head(&self.branch).filter(|head| *head != pin) else {
            return;
        };
        match session.scope_since(scope, &pin, head) {
            ScopeSince::Pending => self.awaiting_changes = true,
            ScopeSince::Unchanged => {
                self.paging.pinned = Some(head.to_owned());
                cx.notify();
            }
            ScopeSince::Changed => {
                self.newer = true;
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();
            }
        }
    }

    /// A write on the table's behalf is going out (a dialog's insert or delete).
    pub fn save_started(&mut self) {
        self.saves_in_flight += 1;
    }

    /// A write on the table's behalf came back and its outcome is applied.
    pub fn save_finished(&mut self, cx: &mut Context<Self>) {
        self.saves_in_flight = self.saves_in_flight.saturating_sub(1);
        self.saves_settled(cx);
    }

    /// Once every write is back, a head the session saw while they were out is compared.
    fn saves_settled(&mut self, cx: &mut Context<Self>) {
        if self.saves_in_flight == 0 && std::mem::take(&mut self.head_moved_while_saving) {
            self.compare_head(cx);
        }
    }

    /// A write bound to the pin was refused because the branch moved: offer Refresh.
    pub fn branch_moved(&mut self, cx: &mut Context<Self>) {
        if !self.newer {
            self.newer = true;
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
        }
    }

    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The newest is wanted, which the session may not hold yet: the head is asked after
        // first, and the table read at it once the session holds it, so the pin is a head the
        // session compares the next ones with.
        let head = self
            .session
            .update(cx, |session, cx| session.head_now(&self.branch, cx));
        self._refresh = Some(cx.spawn(async move |this, cx| {
            head.await;
            this.update(cx, |this, cx| {
                let schema_moved = match (&this.schema, this.session.read(cx).schema_model()) {
                    (Some(shown), Some(held)) => shown.source != held.source,
                    _ => false,
                };
                if schema_moved {
                    // Its columns and rows come from the new schema.
                    this.load_schema(cx);
                } else {
                    this.reload(false, cx);
                }
            })
            .ok();
        }));
        if self.peek.is_some() {
            self.close_peek(cx);
        }
        window.focus(&self.focus_handle, cx);
    }

    // ---- own writes from the dialogs ----

    /// A row this table inserted committed. It is shown where it sorts when that place is
    /// loaded; the pin follows when nothing else committed in between, and the counts follow
    /// it ([`Self::recount`]).
    pub fn row_inserted(
        &mut self,
        branch: &str,
        key: Option<String>,
        mut values: serde_json::Map<String, Value>,
        commit: &str,
        parent: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        // Inserted on the branch the table left: not one it shows.
        if branch != self.branch {
            return;
        }
        let behind = self.follow_own_commit(parent, commit);
        if behind {
            self.newer = true;
            cx.emit(ItemEvent::UpdateTab);
        }
        // A row added, with no relationship: nothing the peek shows changed.
        self.tell_peek_wrote(Vec::new(), None, commit, parent, cx);
        let filters = self.resolved_filters();
        let matching = i64::from(row_matches(&filters, &values));
        match key {
            Some(id) => {
                values.insert(crate::model::ID_COLUMN.into(), Value::String(id.clone()));
                // Grouped, a new row goes first in its open section; sorted, first in the list,
                // where it is seen. The next read places it.
                let section = self.grouping.as_ref().map(|grouping| {
                    let value = grouping.value_of_new_row(&values)?;
                    grouping.section_with(&value).filter(|ix| {
                        grouping
                            .sections()
                            .get(*ix)
                            .is_some_and(|section| section.expanded)
                    })
                });
                let position = match section {
                    Some(Some(ix)) => self
                        .grouping
                        .as_ref()
                        .and_then(|g| g.sections().get(ix))
                        .map(|section| section.range.start),
                    // A value with no open section, or not known yet, shows after a read.
                    Some(None) => {
                        self.newer = true;
                        cx.emit(ItemEvent::UpdateTab);
                        None
                    }
                    None if self.sort.is_some() => Some(0),
                    None => insert_position(&self.rows, &id, self.paging.exhausted),
                };
                if let (Some(Some(ix)), Some(grouping)) = (section, self.grouping.as_mut()) {
                    grouping.inserted(ix, 1);
                    if let Some(section) = grouping.section_mut(ix) {
                        section.count += 1;
                    }
                }
                if let Some(position) = position {
                    // Out of read order, sorted or in a section: a later page brings it again.
                    if self.sort.is_some() || section.is_some() {
                        self.placed_here.insert(id.clone());
                    }
                    let dimmed = !row_matches(&filters, &values);
                    self.rows.insert(position, Row { id, values, dimmed });
                    // The row a range starts from moves with the rows below it; the cursor goes
                    // to the new row.
                    self.rows_inserted(position, 1);
                    self.cursor = Some(Cursor {
                        row: position,
                        col: self.cursor.map_or(0, |c| c.col),
                    });
                    self.table_state
                        .read(cx)
                        .scroll_handle
                        .scroll_to_item(position, ScrollStrategy::Nearest);
                }
            }
            // Without a key the engine assigns the id; only a fresh read shows the row.
            None => {
                self.newer = true;
                cx.emit(ItemEvent::UpdateTab);
            }
        }
        self.recount(behind, 1, matching, cx);
        cx.notify();
    }

    /// Brings the counts up to the table's own insert or delete of `rows` rows, `matching` of
    /// them through its filters (negative for a delete). With the pin moved to the commit, they
    /// are read again there: the rows changed are counted as the commit counts them, those not
    /// loaded and those a filter on a relationship keeps included. Behind it, a read at the pin
    /// would not hold the change, so they are adjusted here until Refresh: those shown now, and
    /// each read at the pin as it lands, one out now included. The counts shown stay until
    /// the reads land, rather than the session's snapshot, read again for the commit.
    fn recount(&mut self, behind: bool, rows: i64, matching: i64, cx: &mut Context<Self>) {
        if !behind {
            if matches!(self.first_page, Loadable::Loaded(())) {
                self.read_counts(cx);
            }
            return;
        }
        if let Loadable::Loaded(n) = &mut self.count {
            *n += matching;
        }
        if let Loadable::Loaded(n) = &mut self.total {
            *n += rows;
        }
        let Some(pin) = self.paging.pinned.clone() else {
            return;
        };
        let mut shift = match self.count_shift.take() {
            Some(shift) if shift.pin == pin => shift,
            _ => CountShift {
                pin,
                filters: self.filters.clone(),
                rows: 0,
                matching: 0,
            },
        };
        // Those counted under other filters are not known under these, which the counts were
        // read with since.
        if shift.filters != self.filters {
            shift.filters = self.filters.clone();
            shift.matching = 0;
        }
        shift.rows += rows;
        shift.matching += matching;
        self.count_shift = Some(shift);
    }

    /// Rows the table's own delete removed in `commit`: they leave the list, with their drafts
    /// and the pick. The table follows the commit, which moves its pin once, and its counts
    /// follow it ([`Self::recount`]).
    pub fn rows_deleted(
        &mut self,
        branch: &str,
        ids: &[String],
        commit: &str,
        parent: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        // Deleted on the branch the table left: the rows it shows are still there.
        if branch != self.branch {
            return;
        }
        let behind = self.follow_own_commit(parent, commit);
        if behind {
            self.newer = true;
            cx.emit(ItemEvent::UpdateTab);
        }
        // Counted before they go, for counts adjusted here: each row once, however many places
        // it shows in.
        let (rows, matching) = {
            let going: HashSet<&str> = ids.iter().map(String::as_str).collect();
            let mut counted = HashSet::new();
            let filters = self.resolved_filters();
            self.rows
                .iter()
                .filter(|row| going.contains(row.id.as_str()) && counted.insert(row.id.as_str()))
                .fold((0, 0), |(rows, matching), row| {
                    let shown = row_matches(&filters, &row.values);
                    (rows + 1, matching + i64::from(shown))
                })
        };
        self.remove_rows(ids, cx);
        // A deleted node took its relationships with it: the rows still shown read their
        // relationship chips again at the new commit, rather than listing it.
        self.relations.clear();
        self.relation_tasks.clear();
        self.relations_asked.clear();
        self.relation_lines = None;
        self.load_shown_relations(cx);
        self.recount(behind, -rows, -matching, cx);
        cx.notify();
    }

    /// Takes rows out of the table with every copy of each (grouped by a relationship, a row
    /// related to two neighbours shows in both sections), in one pass: a batch of thousands
    /// costs one walk of the rows and one drafts sync, not one of each per row.
    fn remove_rows(&mut self, ids: &[String], cx: &mut Context<Self>) {
        let going: HashSet<&str> = ids.iter().map(String::as_str).collect();
        {
            // A row put back (an undone delete) shows its values afresh.
            let mut shown = self.cell_text.borrow_mut();
            for id in &going {
                shown.remove(*id);
            }
        }
        // From the last row up, so each index is the row's place once the rows after it went.
        let gone: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, row)| going.contains(row.id.as_str()))
            .map(|(index, _)| index)
            .collect();
        // A deleted row leaves the selection whether or not it is loaded.
        self.selection.retain(|id| !going.contains(id.as_str()));
        if gone.is_empty() {
            return;
        }
        for &index in &gone {
            if let Some(grouping) = &mut self.grouping {
                grouping.removed(index);
            }
            self.row_removed(index);
        }
        self.rows.retain(|row| !going.contains(row.id.as_str()));
        self.cells
            .retain(|(row_id, _), _| !going.contains(row_id.as_str()));
        self.drop_rows_drafts(&going, cx);
        // Moved up past each row that went (`row_removed`); on the last row, the one above.
        if self.rows.is_empty() {
            self.cursor = None;
        } else if let Some(cursor) = &mut self.cursor {
            cursor.row = cursor.row.min(self.rows.len() - 1);
        }
        if self
            .peek
            .as_ref()
            .is_some_and(|(peeked, _)| going.contains(peeked.as_str()))
        {
            self.close_peek(cx);
        }
    }

    /// Keeps the selection anchor and the cursor on their rows after the row at `index` went:
    /// the anchor goes with its own row, and a cursor on it stays at its place, on the next row.
    /// The rows drawn are asked for again: their places hold other rows now.
    fn row_removed(&mut self, index: usize) {
        self.selection_anchor = match self.selection_anchor {
            Some(anchor) if anchor == index => None,
            Some(anchor) if anchor > index => Some(anchor - 1),
            anchor => anchor,
        };
        if let Some(cursor) = &mut self.cursor
            && cursor.row > index
        {
            cursor.row -= 1;
        }
        self.lines_moved();
    }

    // ---- cursor ----

    fn row_index(&self, id: &str) -> Option<usize> {
        self.rows.iter().position(|row| row.id == id)
    }

    fn column_index(&self, key: &str) -> Option<usize> {
        self.columns.iter().position(|column| column.key == key)
    }

    fn move_cursor(&mut self, rows: isize, cols: isize, cx: &mut Context<Self>) {
        if self.rows.is_empty() || self.columns.is_empty() {
            return;
        }
        let col = self.cursor.map_or(0, |c| {
            (c.col as isize + cols).clamp(0, self.columns.len() as isize - 1) as usize
        });
        let row = match (&self.grouping, self.cursor) {
            // Grouped, the cursor moves among the rows shown: over headers, past closed groups.
            (Some(grouping), cursor) => {
                match grouping.step_row(
                    cursor.map(|c| c.row),
                    if cursor.is_some() { rows } else { 0 },
                ) {
                    Some(row) => row,
                    None => return,
                }
            }
            (None, None) => 0,
            (None, Some(c)) => {
                (c.row as isize + rows).clamp(0, self.rows.len() as isize - 1) as usize
            }
        };
        self.set_cursor(Cursor { row, col }, cx);
    }

    fn set_cursor(&mut self, cursor: Cursor, cx: &mut Context<Self>) {
        self.cursor = Some(cursor);
        let line = match &self.grouping {
            Some(grouping) => grouping.line_of_row(cursor.row),
            None => Some(cursor.row),
        };
        if let Some(line) = line {
            self.table_state
                .read(cx)
                .scroll_handle
                .scroll_to_item(line, ScrollStrategy::Nearest);
        }
        self.scroll_column_into_view(cursor.col, cx);
        if self
            .paging
            .wants_more(self.rows.len(), cursor.row + 1, self.loading_more)
        {
            self.load_more(cx);
        }
        // A peek follows the cursor from row to row.
        let peeked = self.peek.as_ref().map(|(id, _)| id.clone());
        if let Some(peeked) = peeked
            && self
                .rows
                .get(cursor.row)
                .is_some_and(|row| row.id != peeked)
        {
            self.show_peek(cursor.row, true, cx);
        }
        cx.notify();
    }

    /// Scrolls the unpinned columns so the cursor's column is in view, at the widths drawn
    /// (fitted, or dragged).
    fn scroll_column_into_view(&self, col: usize, cx: &App) {
        if col == 0 {
            return;
        }
        let total = self.width_from(1);
        let left = total - self.width_from(col);
        let right = total - self.width_from(col + 1);
        let handle = &self.table_state.read(cx).horizontal_scroll_handle;
        let viewport = handle.bounds().size.width;
        if viewport <= px(0.) {
            return;
        }
        let offset = handle.offset();
        let shown_left = -offset.x;
        if left < shown_left {
            handle.set_offset(point(-left, offset.y));
        } else if right > shown_left + viewport {
            handle.set_offset(point(-(right - viewport), offset.y));
        }
    }

    fn click_cell(
        &mut self,
        row: usize,
        col: usize,
        modifiers: gpui::Modifiers,
        clicks: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The columns can change between the frame clicked and the click: a column gone is no
        // cell.
        let Some(column) = self.columns.get(col) else {
            return;
        };
        let (role, editable) = (column.role.clone(), column.editable(self.kind));
        let relationship = column.relationship().cloned();
        let cursor = Cursor { row, col };
        // A double click on a node's key opens its page (spec 014); the first click peeked it.
        if clicks >= 2 && self.kind == TableKind::Node && matches!(role, ColumnRole::Key) {
            self.set_cursor(cursor, cx);
            self.open_row_page(row, window, cx);
            return;
        }
        // ⌘-click picks a row for a bulk set; ⇧-click picks the rows from the last one picked.
        if modifiers.secondary() || modifiers.shift {
            self.pick_rows(row, modifiers.shift, cx);
            self.set_cursor(cursor, cx);
            return;
        }
        if !self.selection.is_empty() {
            self.clear_selection(cx);
        }
        self.set_cursor(cursor, cx);
        // A relationship cell opens the related rows: the other type, filtered to those related
        // to this row through the same relationship seen from the other end.
        if let (Some(group), Some(id)) = (relationship, self.rows.get(row).map(|r| r.id.clone())) {
            let back = builder::reversed(&group, &self.type_name);
            window.dispatch_action(
                Box::new(studio_actions::graph::OpenType {
                    type_name: group.other_type.clone(),
                    at_commit: self.at_commit.clone(),
                    filter_id: Some(id),
                    via: Some(builder::relationship_key(&back)),
                    preview: false,
                }),
                cx,
            );
            return;
        }
        if editable {
            self.start_edit(cursor, window, cx);
        } else {
            self.show_peek(row, false, cx);
        }
    }

    // ---- peek ----

    fn show_peek(&mut self, row: usize, debounce: bool, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(row) else {
            return;
        };
        let subject = match self.kind {
            TableKind::Node => PeekSubject::Node {
                type_name: self.type_name.clone(),
                id: row.id.clone(),
            },
            TableKind::Edge => {
                let Some(edge) = self
                    .schema
                    .as_ref()
                    .and_then(|schema| schema.edge(&self.type_name))
                else {
                    return;
                };
                PeekSubject::Edge {
                    edge_type: edge.name.clone(),
                    id: row.id.clone(),
                    from_type: edge.from.clone(),
                    to_type: edge.to.clone(),
                }
            }
        };
        // What the row holds at the table's commit: a cell with a save out shows the value sent,
        // which is not stored until the save lands, and the peek shows it as saving.
        let mut values = row.values.clone();
        let saving = self.saves_of(&row.id);
        for (column, stored) in &saving {
            match stored {
                Value::Null => values.remove(column),
                stored => values.insert(column.clone(), stored.clone()),
            };
        }
        let saving: HashSet<String> = saving.into_iter().map(|(column, _)| column).collect();
        let id = row.id.clone();
        let session = self.session.clone();
        let snapshot = self.paging.pinned.clone();
        let holds_commit = self.at_commit.is_some();
        // The row's relationships as this table read them at the same commit.
        // Looked up by the row's own keys, not found among every row's: a peek follows each
        // arrow press. The neighbours were read without the edges relating them, which a peek
        // removing or replacing one names: only a peek held at the commit, which writes none,
        // is given them; another reads its own.
        let known: HashMap<String, KnownRelation> = match snapshot {
            Some(_) => self
                .columns
                .iter()
                .filter_map(ColumnSpec::relationship)
                .filter_map(|group| {
                    let key = builder::relationship_key(group);
                    let relation = self.relations.get(&id, &key)?;
                    let count = *relation.count.as_ref().ok()?;
                    let first = (holds_commit && relation.whole).then(|| {
                        relation
                            .neighbours
                            .iter()
                            .map(|(id, label)| Neighbour {
                                id: id.clone(),
                                label: label.clone(),
                                edge_id: None,
                            })
                            .collect()
                    });
                    Some((key.clone(), KnownRelation { count, first }))
                })
                .collect(),
            None => HashMap::new(),
        };
        // Its Blob values as the cells show them, where they hold: past the pin after the
        // table's own Blob write over another writer's commit.
        let blobs = self.shown_blobs(&id);
        let peek = cx.new(|cx| {
            let mut peek = NodePeek::from_table(
                session, subject, values, snapshot, known, blobs, debounce, cx,
            );
            if holds_commit {
                peek.hold_commit();
            }
            peek.table_saves(saving, cx);
            peek
        });
        if let Some(workspace) = self.workspace.upgrade() {
            let view: gpui::AnyView = peek.clone().into();
            let tab = cx.entity_id();
            workspace.update(cx, |workspace, cx| {
                workspace.set_inspector_of(tab, Some(view.clone()), cx);
                // The peek is a list that draws, and reads, only the relationship groups in
                // view.
                workspace.set_panel_scrolls_itself(&view, cx);
                // A peek asked for (Space, ↵, a click) shows the inspector even when its dock
                // is hidden; one that follows the cursor does not reopen it.
                if !debounce {
                    workspace.reveal_inspector(cx);
                }
            });
        }
        self.peek = Some((id, peek));
        cx.notify();
    }

    /// The cells of the row with `row_id` with a save out, each with the value the row holds
    /// at the table's commit.
    fn saves_of(&self, row_id: &str) -> Vec<(String, Value)> {
        self.cells
            .iter()
            .filter_map(|((row, column), state)| match state {
                CellState::Saving { stored } if row == row_id => {
                    Some((column.clone(), stored.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// Tells the peek of a row which of the row's cells have a save out: it shows them as
    /// saving, over the value the row holds, until they land.
    fn tell_peek_saves(&self, cx: &mut Context<Self>) {
        let Some((id, peek)) = &self.peek else {
            return;
        };
        let saving: HashSet<String> = self
            .saves_of(id)
            .into_iter()
            .map(|(column, _)| column)
            .collect();
        peek.update(cx, |peek, cx| peek.table_saves(saving, cx));
    }

    /// Tells the peek that one of the table's value writes landed at `commit`, made on `parent`:
    /// a saved cell, drafts saved together, a value set on many rows, an inserted row, and a
    /// Blob stored or cleared. The peek follows such a write as it follows its own writes,
    /// rather than offering Refresh for it. `written` is what it wrote, by row and property, and
    /// `blob` the Blob value it stored or cleared in one cell, by row; the peek takes those of
    /// its own row, and the other rows only by id, since it may show one of them among its
    /// neighbours. A write that only added rows (an insert) changed no row the peek shows.
    ///
    /// A delete ([`Self::rows_deleted`]) is not told, nor is Undo of one, which the workspace
    /// sends through the session: either changes relationships as well as rows (a deleted node
    /// takes its edges with it, and Undo puts them back), so the peek's groups of neighbours
    /// may have changed in ways `written` cannot carry. The peek sees that commit move the
    /// branch as another writer's does, and compares it with what it shows: unchanged, it moves
    /// to the head; changed, it offers Refresh. The peek of a deleted row closes.
    fn tell_peek_wrote(
        &self,
        written: Vec<(String, String, Value)>,
        blob: Option<(String, String, BlobMeta)>,
        commit: &str,
        parent: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let Some((id, peek)) = &self.peek else {
            return;
        };
        let mut touched = HashSet::new();
        let mut values = Vec::new();
        for (row, column, value) in written {
            if row == *id {
                values.push((column, value));
            } else {
                touched.insert(row);
            }
        }
        let blob = blob.and_then(|(row, property, meta)| {
            if row == *id {
                Some((property, meta))
            } else {
                touched.insert(row);
                None
            }
        });
        let wrote = TableWrote {
            commit: commit.to_owned(),
            parent: parent.map(str::to_owned),
            values,
            blob,
            type_name: self.type_name.clone(),
            touched,
        };
        peek.update(cx, |peek, cx| peek.table_wrote(wrote, cx));
    }

    /// Opens a node row's page, at the commit the table holds or the branch head.
    fn open_row_page(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.kind != TableKind::Node {
            return;
        }
        let Some(row) = self.rows.get(row) else {
            return;
        };
        window.dispatch_action(
            Box::new(studio_actions::graph::OpenNodePage {
                type_name: self.type_name.clone(),
                id: row.id.clone(),
                at_commit: self.at_commit.clone(),
            }),
            cx,
        );
    }

    fn on_open_row_page(&mut self, _: &OpenRowPage, window: &mut Window, cx: &mut Context<Self>) {
        let row = self.cursor.map_or(0, |cursor| cursor.row);
        self.open_row_page(row, window, cx);
    }

    fn close_peek(&mut self, cx: &mut Context<Self>) {
        if self.peek.take().is_some()
            && let Some(workspace) = self.workspace.upgrade()
        {
            let tab = cx.entity_id();
            workspace.update(cx, |workspace, cx| {
                workspace.set_inspector_of(tab, None, cx)
            });
        }
        cx.notify();
    }

    // ---- editing ----

    fn start_edit(&mut self, cursor: Cursor, window: &mut Window, cx: &mut Context<Self>) {
        let Some(column) = self.columns.get(cursor.col) else {
            return;
        };
        if !column.editable(self.kind)
            || self.at_commit.is_some()
            || self.change_refusal(cx).is_some()
        {
            self.show_peek(cursor.row, false, cx);
            return;
        }
        let Some(row) = self.rows.get(cursor.row) else {
            return;
        };
        let key = (row.id.clone(), column.key.clone());
        if matches!(self.cells.get(&key), Some(CellState::Saving { .. })) {
            return;
        }
        // An edit open is left as leaving its cell leaves it, before its editor goes: kept as
        // the cell's draft when it changed anything, which this edit then opens with.
        if let Some(editing) = self.editing.take() {
            self.keep_edit(editing, cx);
        }
        let (Some(row), Some(column)) = (self.rows.get(cursor.row), self.columns.get(cursor.col))
        else {
            return;
        };
        // A draft's save is bound to the value it was typed over, which a Refresh since may
        // have replaced in the cell: a change made elsewhere meanwhile then shows as a conflict
        // rather than being overwritten.
        let old = match self.cells.get(&key) {
            Some(CellState::Draft { .. }) => self.draft_old(&key.0, &key.1, cx),
            _ => None,
        }
        .unwrap_or_else(|| row.value(&column.key).clone());
        // Text with markup or more than one line opens in the editor with its preview (spec
        // 014): a one-line cell editor would flatten it. It reads the value at the table's
        // commit, so its save stays bound to what the cell shows.
        if let (Some(property), Some(text)) = (column.property(), old.as_str())
            && reads_as_markdown(property)
            && !studio_ui::text::is_plain(text)
            && !matches!(self.cells.get(&key), Some(CellState::Draft { .. }))
        {
            window.dispatch_action(
                Box::new(studio_actions::graph::OpenTextEditor {
                    type_name: self.type_name.clone(),
                    id: row.id.clone(),
                    property: property.name.clone(),
                    read_at: self.paging.pinned.clone(),
                    text: None,
                }),
                cx,
            );
            return;
        }
        // A draft stays the session's until it is saved, discarded or put back: opening it does
        // not take it, so Esc keeps it.
        let (text, notice) = match self.cells.get(&key).cloned() {
            Some(CellState::Draft { text, notice }) => (text, notice),
            _ => (edit_text(&old), None),
        };
        self.fresh_cell_editor(text, window, cx);
        self.editing = Some(Editing {
            row_id: key.0,
            column: key.1,
            old,
            notice,
        });
        window.focus(&self.cell_editor.focus_handle(cx), cx);
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// Gives the edit being opened an editor of its own, holding `text`, all of it selected,
    /// with nothing to undo: one editor kept from edit to edit would take ⌘Z back through the
    /// text typed in the cell edited before, and ↵ would write it here. The text put in is not
    /// an edit either, so ⌘Z does not empty the field.
    fn fresh_cell_editor(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            let put = editor.transact(window, cx, |editor, window, cx| {
                editor.set_text(text, window, cx)
            });
            if let Some(put) = put {
                editor
                    .buffer()
                    .update(cx, |buffer, cx| buffer.forget_transaction(put, cx));
            }
            editor.select_all(&editor::actions::SelectAll, window, cx);
            editor
        });
        self._cell_editor_events = cx.subscribe_in(&editor, window, Self::on_cell_editor_event);
        self.cell_editor = editor;
    }

    fn on_cell_editor_event(
        &mut self,
        _: &Entity<Editor>,
        event: &EditorEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            EditorEvent::Blurred => self.cell_editor_blurred(cx),
            // Only a cleared notice changes what the grid shows; the editor draws over the grid
            // (`edit_slot`), so typing leaves the grid as it was.
            EditorEvent::BufferEdited => {
                if let Some(editing) = &mut self.editing
                    && editing
                        .notice
                        .as_ref()
                        .is_some_and(|n| n.tone == Tone::Invalid)
                {
                    editing.notice = None;
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    /// The editor lost focus without Enter or Esc: an unchanged edit closes, a changed one is
    /// kept as a draft on the cell.
    fn cell_editor_blurred(&mut self, cx: &mut Context<Self>) {
        let Some(editing) = self.editing.take() else {
            return;
        };
        self.keep_edit(editing, cx);
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// An edit closed without being saved: its text is kept as the cell's draft when it
    /// changed anything, and the draft goes when it was typed back to what the cell holds. A
    /// change is one of value, as the peek and a save judge it: `1.50` typed over `1.5` asks
    /// for nothing; text that does not parse is kept, to be corrected.
    fn keep_edit(&mut self, editing: Editing, cx: &mut Context<Self>) {
        let text = self.cell_editor.read(cx).text(cx);
        if self.edit_keeps(&editing, &text) {
            self.put_draft(
                &editing.row_id,
                &editing.column,
                editing.old,
                text,
                editing.notice,
                cx,
            );
        } else {
            self.drop_draft(&editing.row_id, &editing.column, cx);
        }
    }

    /// Whether an edit closed with `text` is kept as its cell's draft ([`Self::keep_edit`]):
    /// it changes the value, or says why its last save did not land, or asks for the value it
    /// is bound to over another the row shows since, which is a change too.
    fn edit_keeps(&self, editing: &Editing, text: &str) -> bool {
        let changes = match self.property_of(&editing.column) {
            Some(property) => edited(&property, &editing.old, text).is_some(),
            None => text != edit_text(&editing.old),
        };
        changes
            || editing.notice.is_some()
            || !self.row_shows(&editing.row_id, &editing.column, &editing.old)
    }

    /// [`Self::keep_edit`] for an edit typed on `branch`, which the table may no longer show,
    /// or whose table is going: kept as that branch's draft, or its draft let go. A branch
    /// deleted meanwhile keeps none ([`GraphSession::settle_draft_on`]).
    fn keep_edit_on(&self, branch: &str, editing: Editing, cx: &mut App) {
        let text = self.cell_editor.read(cx).text(cx);
        let keeps = self.edit_keeps(&editing, &text);
        let key = self.draft_key(&editing.row_id, &editing.column);
        self.session.update(cx, |session, cx| {
            if keeps {
                let held = session.draft_on(branch, &key).cloned();
                let entry = DraftEntry {
                    old: editing.old,
                    text,
                };
                session.settle_draft_on(branch, key, held.as_ref(), Some(entry), cx);
            } else {
                session.remove_draft_on(branch, &key, cx);
            }
        });
    }

    fn finish_editing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = None;
        window.focus(&self.focus_handle, cx);
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
        // The footer's key hints read the bindings of the frame drawn before, which still had
        // the editor focused: the workspace, which draws the footer, draws once more so they
        // show the table's keys again. Not a refresh, which would draw every view in the
        // window again, skipping every cache (this table's grid, every pane).
        let workspace = self.workspace.clone();
        cx.on_next_frame(window, move |_, _, cx| {
            workspace.update(cx, |_, cx| cx.notify()).ok();
        });
    }

    /// Whether rows without a value of the sort's property can follow those with one: not when
    /// every row must hold it, which the schema says of the branch as it is; a table held at an
    /// older commit reads them anyway, as that commit's schema may have allowed them.
    fn sort_nulls_follow(&self) -> bool {
        self.at_commit.is_some()
            || self
                .sort
                .as_ref()
                .and_then(|sort| self.property_of(&sort.property))
                .is_none_or(|property| property.nullable)
    }

    fn property_of(&self, column: &str) -> Option<Property> {
        self.all_columns
            .iter()
            .find(|c| c.key == column)
            .and_then(|c| c.property().cloned())
    }

    fn confirm_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.words_editor.focus_handle(cx).is_focused(window) {
            self.run_words(cx);
            return;
        }
        if self.fill.is_some() && self.fill_editor.focus_handle(cx).is_focused(window) {
            self.run_fill(cx);
            return;
        }
        if self.filter_draft.is_some() && self.filter_editor.focus_handle(cx).is_focused(window) {
            // ↵ picks the highlighted offer, unless it is just what is typed. While the nodes
            // for what is typed are being read it waits: the ones offered were found for
            // earlier text.
            let text = self.filter_editor.read(cx).text(cx);
            if self
                .filter_draft
                .as_ref()
                .is_some_and(|draft| draft.answers.as_deref() != Some(text.as_str()))
            {
                return;
            }
            let pick = self.filter_draft.as_ref().and_then(|draft| {
                let index = draft.offers.highlighted()?;
                let suggestion = draft.offers.get(index)?;
                let list = draft.op == FilterOp::In;
                (!suggest::already_typed(&text, &suggestion.value, list)).then_some(index)
            });
            match pick {
                Some(index) => self.pick_suggestion(index, window, cx),
                None => self.apply_filter_draft(window, cx),
            }
            return;
        }
        // An edit open when the access route refused the branch (its save's 403 reopened it):
        // it is kept as the cell's draft and nothing is sent.
        if self.editing.is_some() && self.change_refusal(cx).is_some() {
            self.stage_edit(window, cx);
            return;
        }
        let Some(editing) = self.editing.as_ref() else {
            return;
        };
        let (row_id, column, old) = (
            editing.row_id.clone(),
            editing.column.clone(),
            editing.old.clone(),
        );
        let (Some(row_ix), Some(property), Some(schema)) = (
            self.row_index(&row_id),
            self.property_of(&column),
            self.schema.clone(),
        ) else {
            self.finish_editing(window, cx);
            return;
        };
        let text = self.cell_editor.read(cx).text(cx);
        let shown = self.rows[row_ix].value(&column).clone();
        let new = match edited(&property, &old, &text) {
            Some(Ok(value)) => value,
            Some(Err(message)) => {
                if let Some(editing) = &mut self.editing {
                    editing.notice = Some(Notice::new(Tone::Invalid, message));
                }
                cx.notify();
                return;
            }
            // Asks for the value the edit is bound to, which the row still shows: nothing to
            // save, and a draft the cell held goes.
            None if same_value(&old, &shown) => {
                self.drop_draft(&row_id, &column, cx);
                self.finish_editing(window, cx);
                return;
            }
            // The row shows another value since the draft was typed (a Refresh): the bound
            // write goes, and finds the change.
            None => old.clone(),
        };
        // Bound to the value the edit found; a list, which the write's `where` cannot compare,
        // is bound to the commit the table read it at while the row shows that value there.
        let read_at = self
            .paging
            .pinned
            .clone()
            .filter(|_| same_value(&old, &shown));
        let write = match write::write_properties(
            &self.session,
            &self.branch,
            &schema,
            &self.type_name,
            &row_id,
            &[(property.name.clone(), old.clone(), new.clone())],
            read_at,
            cx,
        ) {
            Ok(write) => write,
            Err(error) => {
                if let Some(editing) = &mut self.editing {
                    editing.notice = Some(Notice::new(Tone::Invalid, error));
                }
                cx.notify();
                return;
            }
        };
        // Show the new value with an amber dot while the save is out.
        self.set_cell(row_ix, &column, new.clone());
        let stored = shown.clone();
        self.cells.insert(
            (row_id.clone(), column.clone()),
            CellState::Saving { stored },
        );
        self.tell_peek_saves(cx);
        self.saves_in_flight += 1;
        self.finish_editing(window, cx);
        let (session, branch) = (self.session.clone(), self.branch.clone());
        let key = self.draft_key(&row_id, &column);
        let sent = session.read(cx).draft_on(&branch, &key).cloned();
        let entry = DraftEntry {
            old: old.clone(),
            text: text.clone(),
        };
        // Detached: a write is never cancelled by closing the tab.
        cx.spawn_in(window, async move |this, cx| {
            let result = write.await;
            let saved = matches!(result, SaveResult::Saved { .. });
            let held = sent.clone();
            let settled = this.update_in(cx, |this, window, cx| {
                let cell = SavedCell {
                    row_id,
                    column,
                    old,
                    new,
                    text,
                    shown,
                    sent: held,
                };
                this.finish_save(cell, &session, &branch, result, window, cx)
            });
            // The tab closed while the write was out: the cell's draft is still the session's
            // to settle, its text kept when it did not land.
            if settled.is_err() {
                let keep = (!saved).then_some(entry);
                session.update(cx, |session, cx| {
                    session.settle_draft_on(&branch, key, sent.as_ref(), keep, cx)
                });
            }
        })
        .detach();
    }

    fn finish_save(
        &mut self,
        cell: SavedCell,
        session: &Entity<GraphSession>,
        branch: &str,
        result: SaveResult,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.saves_in_flight = self.saves_in_flight.saturating_sub(1);
        // The table moved to another branch or graph meanwhile and shows what it read there;
        // the write's outcome is told by its toast. A saved cell's draft was the branch left's,
        // and the text of one that did not land stays a draft there.
        if !self.shows(session, branch) {
            let key = self.draft_key(&cell.row_id, &cell.column);
            let keep = (!matches!(result, SaveResult::Saved { .. })).then(|| DraftEntry {
                old: cell.old,
                text: cell.text,
            });
            session.update(cx, |session, cx| {
                session.settle_draft_on(branch, key, cell.sent.as_ref(), keep, cx)
            });
            self.saves_settled(cx);
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
            return;
        }
        match result {
            SaveResult::Saved { commit, parent } => {
                self.cells
                    .remove(&(cell.row_id.clone(), cell.column.clone()));
                self.settle_draft(&cell.row_id, &cell.column, cell.sent.as_ref(), None, cx);
                self.sync_drafts(cx);
                if self.follow_own_commit(parent.as_deref(), &commit) {
                    self.newer = true;
                }
                let written = vec![(cell.row_id.clone(), cell.column.clone(), cell.new.clone())];
                self.tell_peek_wrote(written, None, &commit, parent.as_deref(), cx);
                // Shown again: the rows may have been read again, at the commit before, while
                // the write was out.
                if let Some(ix) = self.row_index(&cell.row_id) {
                    self.set_cell(ix, &cell.column, cell.new);
                    self.set_dimmed(ix, !self.row_fits(ix));
                }
            }
            SaveResult::NoMatch => {
                // The bound value changed elsewhere. Read the row at the head to say what it is
                // now; the reopened save binds to that value.
                self.reread_conflict(cell, window, cx);
            }
            SaveResult::Refused(notice) | SaveResult::Unknown(notice) => {
                let places = self.places_of_id(&cell.row_id);
                self.put_back(&places, &cell.column, &cell.new, cell.shown);
                let edit = (cell.old, cell.text, notice);
                self.reopen(
                    cell.row_id,
                    cell.column,
                    edit,
                    cell.sent.as_ref(),
                    window,
                    cx,
                );
            }
        }
        self.tell_peek_saves(cx);
        self.saves_settled(cx);
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// ⌥↵: keeps the open edit as a draft on its cell, to save with the others in one commit.
    /// Returns false when the text does not parse, leaving the editor open with the reason.
    fn stage_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(editing) = self.editing.as_ref() else {
            return true;
        };
        let key = (editing.row_id.clone(), editing.column.clone());
        let old = editing.old.clone();
        let Some(property) = self.property_of(&key.1) else {
            self.finish_editing(window, cx);
            return true;
        };
        let text = self.cell_editor.read(cx).text(cx);
        // Text asking for the value the edit is bound to lets the draft go while the row shows
        // that value; over another, shown since, it stays, so its save finds the change.
        let shows_old = self.row_shows(&key.0, &key.1, &old);
        match edited(&property, &old, &text) {
            Some(Err(message)) => {
                if let Some(editing) = &mut self.editing {
                    editing.notice = Some(Notice::new(Tone::Invalid, message));
                }
                cx.notify();
                return false;
            }
            None if shows_old => {
                self.drop_draft(&key.0, &key.1, cx);
            }
            _ => {
                self.put_draft(&key.0, &key.1, old, text, None, cx);
                self.drafts.confirm_discard = false;
            }
        }
        self.finish_editing(window, cx);
        true
    }

    /// Whether the row with `row_id` shows `value` in `column`; a row not loaded is taken to.
    fn row_shows(&self, row_id: &str, column: &str, value: &Value) -> bool {
        self.row_index(row_id)
            .is_none_or(|ix| same_value(value, self.rows[ix].value(column)))
    }

    /// The value a cell's draft was typed over, which its save is bound to.
    fn draft_old(&self, row_id: &str, column: &str, cx: &App) -> Option<Value> {
        let key = self.draft_key(row_id, column);
        self.session
            .read(cx)
            .draft(&key)
            .map(|draft| draft.old.clone())
    }

    /// Whether the table still shows `session`'s `branch`: an answer read for another is not
    /// this table's.
    fn shows(&self, session: &Entity<GraphSession>, branch: &str) -> bool {
        &self.session == session && self.branch == branch
    }

    fn draft_key(&self, row_id: &str, column: &str) -> DraftKey {
        DraftKey {
            type_name: self.type_name.clone(),
            id: row_id.to_owned(),
            property: column.to_owned(),
        }
    }

    /// Rebuilds the cells' view of the session's drafts for this type, keeping the saves out
    /// (a cell with a save out shows that).
    fn sync_drafts(&mut self, cx: &App) {
        self.cells
            .retain(|_, state| matches!(state, CellState::Saving { .. }));
        if self.at_commit.is_some() {
            return;
        }
        let session = self.session.read(cx);
        for (key, entry) in session
            .drafts()
            .filter(|(key, _)| key.type_name == self.type_name)
        {
            let cell = (key.id.clone(), key.property.clone());
            if self.cells.contains_key(&cell) {
                continue;
            }
            let notice = self.draft_notices.get(&cell).cloned();
            self.cells.insert(
                cell,
                CellState::Draft {
                    text: entry.text.clone(),
                    notice,
                },
            );
        }
    }

    /// The session's drafts changed: the cells show them once the change is done. Drafts
    /// settled together announce themselves one by one, and are shown together.
    fn drafts_changed(&mut self, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.drafts_due, true) {
            return;
        }
        let table = cx.entity().downgrade();
        cx.defer(move |cx| {
            table
                .update(cx, |table, cx| {
                    table.drafts_due = false;
                    table.sync_drafts(cx);
                    cx.emit(ItemEvent::UpdateTab);
                    cx.notify();
                })
                .ok();
        });
    }

    /// Keeps a cell's unsaved text in the session's drafts, with why its last save did not land.
    fn put_draft(
        &mut self,
        row_id: &str,
        column: &str,
        old: Value,
        text: String,
        notice: Option<Notice>,
        cx: &mut Context<Self>,
    ) {
        let cell = (row_id.to_owned(), column.to_owned());
        match notice {
            Some(notice) => self.draft_notices.insert(cell, notice),
            None => self.draft_notices.remove(&cell),
        };
        let key = self.draft_key(row_id, column);
        self.session.update(cx, |session, cx| {
            session.set_draft(key, DraftEntry { old, text }, cx)
        });
        self.sync_drafts(cx);
    }

    /// Lets a cell's draft go: saved, or typed back to what the cell holds.
    fn drop_draft(&mut self, row_id: &str, column: &str, cx: &mut Context<Self>) {
        self.forget_draft(row_id, column, cx);
        self.sync_drafts(cx);
    }

    /// Settles a cell's draft once the write or read it waited on is in
    /// ([`GraphSession::settle_draft_on`]): `keep` it, with why its save did not land, or let it
    /// go, unless it is no longer `sent`, the draft as that went out. One retyped or let go
    /// elsewhere meanwhile (on a node's page, in the peek) stands, without the cell's notice,
    /// which spoke of the save sent. Whether it was settled; it shows with the next
    /// [`Self::sync_drafts`].
    fn settle_draft(
        &mut self,
        row_id: &str,
        column: &str,
        sent: Option<&DraftEntry>,
        keep: Option<(DraftEntry, Option<Notice>)>,
        cx: &mut Context<Self>,
    ) -> bool {
        let key = self.draft_key(row_id, column);
        let branch = self.branch.clone();
        let (entry, notice) = match keep {
            Some((entry, notice)) => (Some(entry), notice),
            None => (None, None),
        };
        let settled = self.session.update(cx, |session, cx| {
            session.settle_draft_on(&branch, key, sent, entry, cx)
        });
        let cell = (row_id.to_owned(), column.to_owned());
        match notice.filter(|_| settled) {
            Some(notice) => self.draft_notices.insert(cell, notice),
            None => self.draft_notices.remove(&cell),
        };
        settled
    }

    /// [`Self::drop_draft`] without showing it: drafts let go together are shown once, by one
    /// [`Self::sync_drafts`] after the last.
    fn forget_draft(&mut self, row_id: &str, column: &str, cx: &mut Context<Self>) {
        self.draft_notices
            .remove(&(row_id.to_owned(), column.to_owned()));
        let key = self.draft_key(row_id, column);
        self.session
            .update(cx, |session, cx| session.remove_draft(&key, cx));
    }

    /// A deleted row's drafts go with it.
    fn drop_rows_drafts(&mut self, ids: &HashSet<&str>, cx: &mut Context<Self>) {
        let keys: Vec<DraftKey> = self
            .session
            .read(cx)
            .drafts()
            .filter(|(key, _)| key.type_name == self.type_name && ids.contains(key.id.as_str()))
            .map(|(key, _)| key.clone())
            .collect();
        self.session.update(cx, |session, cx| {
            for key in &keys {
                session.remove_draft(key, cx);
            }
        });
        self.draft_notices
            .retain(|(row, _), _| !ids.contains(row.as_str()));
        self.sync_drafts(cx);
    }

    /// Peeks the row at `row`, as a click does, and hands back the peek.
    #[cfg(test)]
    pub(crate) fn peek_row(&mut self, row: usize, cx: &mut Context<Self>) -> Entity<NodePeek> {
        self.show_peek(row, false, cx);
        let (_, peek) = self.peek.as_ref().expect("a row to peek");
        peek.clone()
    }

    /// The draft text a cell shows, if any.
    #[cfg(test)]
    pub(crate) fn draft_text(&self, row_id: &str, column: &str) -> Option<String> {
        match self.cells.get(&(row_id.to_owned(), column.to_owned())) {
            Some(CellState::Draft { text, .. }) => Some(text.clone()),
            _ => None,
        }
    }

    /// The cells holding a draft, in table order: row, then column, a hidden column after the
    /// shown ones. `places` is where each row shows ([`Self::row_places`]).
    fn draft_keys(&self, places: &HashMap<String, Vec<usize>>) -> Vec<(String, String)> {
        let mut keys: Vec<(String, String)> = self
            .cells
            .iter()
            .filter(|(_, state)| matches!(state, CellState::Draft { .. }))
            .map(|(key, _)| key.clone())
            .collect();
        keys.sort_by_cached_key(|(row, column)| {
            (
                places.get(row).map_or(usize::MAX, |at| at[0]),
                self.column_index(column).unwrap_or(usize::MAX),
            )
        });
        keys
    }

    /// ⌘S: saves every draft as one mutation of one statement per row, each bound to the values
    /// the row shows, so one commit. A draft that does not parse opens instead, and nothing is
    /// sent.
    fn save_drafts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.drafts.saving
            || self.kind != TableKind::Node
            || self.at_commit.is_some()
            || self.change_refusal(cx).is_some()
        {
            return;
        }
        if self.editing.is_some() && !self.stage_edit(window, cx) {
            return;
        }
        let Some(schema) = self.schema.clone() else {
            return;
        };
        let places = self.row_places();
        let mut edits = Vec::new();
        let mut unseen = 0;
        // Typed back to what they were typed over: let go together, once the loop is done.
        let mut unchanged: Vec<(String, String)> = Vec::new();
        // Whether every list the drafts set was typed over the value the table read at its pin.
        let mut lists_shown = true;
        for key in self.draft_keys(&places) {
            let Some(CellState::Draft { text, .. }) = self.cells.get(&key).cloned() else {
                continue;
            };
            // Of every column: a draft on a hidden one is saved with the others.
            let (Some(row), Some(property)) =
                (places.get(&key.0).map(|at| at[0]), self.property_of(&key.1))
            else {
                unseen += 1;
                continue;
            };
            let shown = self.rows[row].value(&key.1).clone();
            let old = self
                .draft_old(&key.0, &key.1, cx)
                .unwrap_or_else(|| shown.clone());
            let new = match edited(&property, &old, &text) {
                Some(Err(message)) => {
                    for (row_id, column) in &unchanged {
                        self.forget_draft(row_id, column, cx);
                    }
                    let notice = Notice::new(Tone::Invalid, message.clone());
                    self.put_draft(&key.0, &key.1, old, text, Some(notice), cx);
                    match self.column_index(&key.1) {
                        Some(col) => {
                            let cursor = Cursor { row, col };
                            self.set_cursor(cursor, cx);
                            self.start_edit(cursor, window, cx);
                        }
                        // A hidden column's cell cannot open: the bar says which it is.
                        None => {
                            self.drafts.notice = Some(Notice::new(
                                Tone::Invalid,
                                format!(
                                    "Nothing was saved: the draft of {} on {} cannot be saved \
                                     as typed ({message}), and its column is hidden. Show the \
                                     column to change it.",
                                    key.1, key.0
                                ),
                            ));
                            cx.notify();
                        }
                    }
                    return;
                }
                Some(Ok(new)) => new,
                // Asks for the value it was typed over, which the row still shows.
                None if same_value(&old, &shown) => {
                    unchanged.push(key);
                    continue;
                }
                // Over another value, shown since: saved as typed, so the bound write finds the
                // change.
                None => old.clone(),
            };
            if property.list && !same_value(&old, &shown) {
                lists_shown = false;
            }
            edits.push(DraftEdit {
                row_id: key.0,
                column: key.1,
                old,
                new,
                text,
                shown,
            });
        }
        for (row_id, column) in &unchanged {
            self.forget_draft(row_id, column, cx);
        }
        self.sync_drafts(cx);
        self.drafts.confirm_discard = false;
        self.drafts.notice = (unseen > 0).then(|| {
            Notice::new(
                Tone::Conflict,
                format!(
                    "{} {} on rows the table no longer shows; {} {}.",
                    plural(unseen, "draft", "drafts"),
                    if unseen == 1 { "is" } else { "are" },
                    if unseen == 1 { "it" } else { "they" },
                    if unseen == 1 { "waits" } else { "wait" },
                ),
            )
        });
        if edits.is_empty() {
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
            return;
        }
        // A row's drafts are next to one another, in table order.
        let mut rows: Vec<builder::RowEdit> = Vec::new();
        for edit in &edits {
            let cell = (edit.column.clone(), edit.old.clone(), edit.new.clone());
            match rows.last_mut().filter(|row| row.id == edit.row_id) {
                Some(row) => row.cells.push(cell),
                None => rows.push(builder::RowEdit {
                    id: edit.row_id.clone(),
                    cells: vec![cell],
                }),
            }
        }
        let description = format!(
            "Save {} on {} {} {}",
            plural(edits.len(), "change", "changes"),
            rows.len(),
            self.type_name,
            if rows.len() == 1 { "row" } else { "rows" },
        );
        let write = match write::write_rows(
            &self.session,
            &self.branch,
            &schema,
            &self.type_name,
            &rows,
            self.paging.pinned.clone().filter(|_| lists_shown),
            description,
            cx,
        ) {
            Ok(write) => write,
            Err(error) => {
                self.drafts.notice = Some(Notice::new(Tone::Invalid, error));
                cx.notify();
                return;
            }
        };
        // Show the new values with an amber dot while the save is out.
        for edit in &edits {
            if let Some(at) = places.get(&edit.row_id) {
                self.set_cell_at(at, &edit.column, edit.new.clone());
            }
            self.cells.insert(
                (edit.row_id.clone(), edit.column.clone()),
                CellState::Saving {
                    stored: edit.shown.clone(),
                },
            );
        }
        self.drafts.saving = true;
        self.saves_in_flight += 1;
        self.tell_peek_saves(cx);
        cx.notify();
        let (session, branch) = (self.session.clone(), self.branch.clone());
        let row_count = rows.len();
        let sent: Vec<(DraftKey, DraftEntry)> = edits
            .iter()
            .map(|edit| (self.draft_key(&edit.row_id, &edit.column), edit.sent()))
            .collect();
        // Detached: a write is never cancelled by closing the tab.
        cx.spawn_in(window, async move |this, cx| {
            let bound = write.await;
            let landed = bound.landed(row_count);
            let shown = this.update_in(cx, |this, window, cx| {
                this.finish_drafts(bound, row_count, edits, &session, &branch, window, cx)
            });
            // The tab closed while the write was out: drafts that all landed are let go, as
            // the tab would have; the others stay for the next one.
            if shown.is_err() && landed {
                session.update(cx, |session, cx| {
                    for (key, sent) in sent {
                        session.settle_draft_on(&branch, key, Some(&sent), None, cx);
                    }
                });
            }
        })
        .detach();
    }

    #[expect(clippy::too_many_arguments)]
    fn finish_drafts(
        &mut self,
        bound: Bound,
        row_count: usize,
        edits: Vec<DraftEdit>,
        session: &Entity<GraphSession>,
        branch: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.saves_in_flight = self.saves_in_flight.saturating_sub(1);
        self.drafts.saving = false;
        if !self.shows(session, branch) {
            // As for one cell: the drafts saved were the branch left's, and the rest wait there.
            if bound.landed(row_count) {
                let sent: Vec<(DraftKey, DraftEntry)> = edits
                    .iter()
                    .map(|edit| (self.draft_key(&edit.row_id, &edit.column), edit.sent()))
                    .collect();
                session.update(cx, |session, cx| {
                    for (key, sent) in sent {
                        session.settle_draft_on(branch, key, Some(&sent), None, cx);
                    }
                });
            }
            self.saves_settled(cx);
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
            return;
        }
        match &bound {
            Bound::Changed => {
                // A list read at the head no longer held what its draft was typed over, and
                // nothing was written. Read the rows at the head to tell which drafts it was.
                self.drafts.notice = Some(Notice::new(
                    Tone::Conflict,
                    "A list changed elsewhere, so nothing was saved; the drafts are kept",
                ));
                self.reread_drafts(edits, window, cx);
            }
            // A commit on no branch, or of a kind not known, is told as the others are.
            Bound::Sent(graph_client::WriteOutcome::Committed(output))
                if published(output.commit.as_ref(), branch).is_ok() =>
            {
                if let Some(commit) = output.commit.as_ref()
                    && self.follow_own_commit(
                        commit.parent_commit_id.as_deref(),
                        &commit.graph_commit_id,
                    )
                {
                    self.newer = true;
                }
                let changed = output.affected_nodes as usize;
                if let Some(commit) = output.commit.as_ref().filter(|_| changed == row_count) {
                    let written = edits
                        .iter()
                        .map(|edit| (edit.row_id.clone(), edit.column.clone(), edit.new.clone()))
                        .collect();
                    let parent = commit.parent_commit_id.as_deref();
                    self.tell_peek_wrote(written, None, &commit.graph_commit_id, parent, cx);
                }
                if output.commit.is_some() && changed == row_count {
                    // Shown again, as one saved cell is: the rows may have been read again.
                    let places = self.row_places();
                    for edit in &edits {
                        if let Some(at) = places.get(&edit.row_id) {
                            self.set_cell_at(at, &edit.column, edit.new.clone());
                        }
                        self.cells
                            .remove(&(edit.row_id.clone(), edit.column.clone()));
                        let sent = edit.sent();
                        self.settle_draft(&edit.row_id, &edit.column, Some(&sent), None, cx);
                    }
                    self.sync_drafts(cx);
                    self.refresh_dimmed(&edits);
                } else {
                    // A row whose shown values changed elsewhere was left alone, whole. Read
                    // the rows at the head to tell which drafts landed.
                    let left = row_count.saturating_sub(changed);
                    self.drafts.notice = Some(Notice::new(
                        Tone::Conflict,
                        format!(
                            "{} changed elsewhere; {} kept",
                            plural(left, "row", "rows"),
                            if left == 1 {
                                "its drafts are"
                            } else {
                                "their drafts are"
                            },
                        ),
                    ));
                    self.reread_drafts(edits, window, cx);
                }
            }
            _ => {
                if let SaveResult::Refused(notice) | SaveResult::Unknown(notice) =
                    bound.result(branch)
                {
                    let places = self.row_places();
                    for edit in edits {
                        let sent = edit.sent();
                        if let Some(at) = places.get(&edit.row_id) {
                            self.put_back(at, &edit.column, &edit.new, edit.shown);
                        }
                        self.cells
                            .remove(&(edit.row_id.clone(), edit.column.clone()));
                        let keep = (sent.clone(), Some(notice.clone()));
                        self.settle_draft(&edit.row_id, &edit.column, Some(&sent), Some(keep), cx);
                    }
                    self.sync_drafts(cx);
                    self.drafts.notice = Some(notice);
                }
            }
        }
        self.tell_peek_saves(cx);
        self.saves_settled(cx);
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn refresh_dimmed(&mut self, edits: &[DraftEdit]) {
        let places = self.row_places();
        let filters = self.resolved_filters();
        for edit in edits {
            if let Some(at) = places.get(&edit.row_id) {
                let fits = self.row_fits_under(&filters, at[0]);
                self.set_dimmed_at(at, !fits);
            }
        }
    }

    /// Reads a batch's rows at the head, together in one read of the properties their drafts
    /// set: a draft whose value is there landed; any other is kept, with what the cell holds
    /// now, and why when that changed elsewhere.
    fn reread_drafts(
        &mut self,
        edits: Vec<DraftEdit>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(schema) = self.schema.clone() else {
            return;
        };
        let mut ids: Vec<String> = edits.iter().map(|edit| edit.row_id.clone()).collect();
        ids.sort();
        ids.dedup();
        let mut properties: Vec<String> = edits.iter().map(|edit| edit.column.clone()).collect();
        properties.sort();
        properties.dedup();
        let Ok(query) = builder::nodes_fields_by_id(&schema, &self.type_name, &ids, &properties)
        else {
            // Kept as they are, with why: whether they landed cannot be told.
            self.drafts_read(edits, None, cx);
            return;
        };
        let read = self
            .session
            .update(cx, |session, cx| session.read(&query, None, cx));
        self.saves_in_flight += 1;
        let (session, branch) = (self.session.clone(), self.branch.clone());
        let keys: Vec<DraftKey> = edits
            .iter()
            .map(|edit| self.draft_key(&edit.row_id, &edit.column))
            .collect();
        cx.spawn_in(window, async move |this, cx| {
            // The rows at the head, by id: `None` when they could not be read; a row missing
            // from them is gone. A draft retyped or let go elsewhere meanwhile stands, here
            // and in `drafts_read`.
            let now: Option<HashMap<String, Row>> = read.await.ok().map(|output| {
                field_rows(&output.rows)
                    .into_iter()
                    .map(|row| (row.id.clone(), row))
                    .collect()
            });
            let shown = this.update_in(cx, |this, _window, cx| {
                this.saves_in_flight = this.saves_in_flight.saturating_sub(1);
                if !this.shows(&session, &branch) {
                    this.saves_settled(cx);
                    return false;
                }
                this.drafts_read(edits.clone(), now.as_ref(), cx);
                this.saves_settled(cx);
                true
            });
            // The table moved on, or its tab closed: each draft is settled where it was typed,
            // and nothing here changes. One that did not land stays bound to the value it was
            // typed over: what the head holds was never shown, and why it changed went with the
            // table, so its next save finds the change again rather than writing over it.
            if !shown.unwrap_or(false) {
                session.update(cx, |session, cx| {
                    for (edit, key) in edits.into_iter().zip(keys) {
                        let landed = now
                            .as_ref()
                            .and_then(|rows| rows.get(&edit.row_id))
                            .is_some_and(|row| same_value(row.value(&edit.column), &edit.new));
                        let sent = edit.sent();
                        let keep = (!landed).then(|| sent.clone());
                        session.settle_draft_on(&branch, key, Some(&sent), keep, cx);
                    }
                });
            }
        })
        .detach();
    }

    /// Settles a batch's drafts read again at the head (`now`, the rows by id as
    /// [`Self::reread_drafts`] reads them): one whose value is there landed and goes; one whose
    /// row was left alone for another draft's sake stands as typed; the others are kept, bound
    /// to what the row holds now, with why. A draft retyped or let go elsewhere while the read
    /// was out (on a node's page, in the peek) stands as it is; its cell still shows what the
    /// row holds.
    fn drafts_read(
        &mut self,
        edits: Vec<DraftEdit>,
        now: Option<&HashMap<String, Row>>,
        cx: &mut Context<Self>,
    ) {
        let places = self.row_places();
        for edit in edits {
            let key = (edit.row_id.clone(), edit.column.clone());
            let property = self.property_of(&edit.column);
            let at = places
                .get(&edit.row_id)
                .map(Vec::as_slice)
                .unwrap_or_default();
            self.cells.remove(&key);
            let sent = edit.sent();
            let (old, notice) = match now.map(|rows| rows.get(&edit.row_id)) {
                Some(Some(row)) => {
                    let value = row.value(&edit.column).clone();
                    if same_value(&value, &edit.new) {
                        self.settle_draft(&edit.row_id, &edit.column, Some(&sent), None, cx);
                        continue;
                    }
                    self.set_cell_at(at, &edit.column, value.clone());
                    let notice = (!same_value(&value, &edit.old))
                        .then(|| changed_elsewhere(property.as_ref(), Some(&value)));
                    (value, notice)
                }
                Some(None) => {
                    self.put_back(at, &edit.column, &edit.new, edit.shown);
                    self.set_dimmed_at(at, true);
                    (edit.old, Some(changed_elsewhere(property.as_ref(), None)))
                }
                None => {
                    self.put_back(at, &edit.column, &edit.new, edit.shown);
                    let notice = Notice::new(
                        Tone::Unknown,
                        "The row could not be read to check this change. Check History \
                         before trying again; your change is kept.",
                    );
                    (edit.old, Some(notice))
                }
            };
            let keep = DraftEntry {
                old,
                text: edit.text,
            };
            self.settle_draft(
                &edit.row_id,
                &edit.column,
                Some(&sent),
                Some((keep, notice)),
                cx,
            );
        }
        self.sync_drafts(cx);
        self.tell_peek_saves(cx);
        // The table now shows values newer than its pin.
        self.newer = true;
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// Drops every draft the pill counts, on the second click of Discard. A cell whose save is
    /// out keeps its draft: the save settles it, letting it go when it lands and keeping the
    /// text, with why, when it does not.
    fn discard_drafts(&mut self, cx: &mut Context<Self>) {
        if !self.drafts.confirm_discard {
            self.drafts.confirm_discard = true;
            cx.notify();
            return;
        }
        let cells: Vec<(String, String)> = self
            .cells
            .iter()
            .filter(|(_, state)| matches!(state, CellState::Draft { .. }))
            .map(|(cell, _)| cell.clone())
            .collect();
        let keys: Vec<DraftKey> = cells
            .iter()
            .map(|(row, column)| self.draft_key(row, column))
            .collect();
        self.session.update(cx, |session, cx| {
            for key in &keys {
                session.remove_draft(key, cx);
            }
        });
        for cell in &cells {
            self.draft_notices.remove(cell);
        }
        self.sync_drafts(cx);
        self.drafts.notice = None;
        self.drafts.confirm_discard = false;
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// The pill floating over the bottom of the table while cells hold drafts: how many,
    /// Discard, and saving them all as one commit. It floats, so rows never move when it shows.
    fn render_drafts_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.kind != TableKind::Node || self.at_commit.is_some() {
            return None;
        }
        let count = self
            .cells
            .values()
            .filter(|state| matches!(state, CellState::Draft { .. }))
            .count();
        if count == 0 && !self.drafts.saving {
            return None;
        }
        let saving = self.drafts.saving;
        let focus = self.focus_handle.clone();
        let refused = self.change_refusal(cx);
        let save_tooltip = format!(
            "One commit to {}; each draft is bound to the value it was typed over",
            self.branch
        );
        // The canvas's floating bar: its text at a control's inset, its buttons the bar's.
        let pill = studio_ui::ds::float_bar("drafts-bar", cx)
            .occlude()
            .max_w(px(studio_ui::ds::size::READING_WIDTH))
            .pl(px(space::CONTROL_INSET))
            .gap(px(space::MD))
            .child(studio_ui::ds::dot(Color::Accent))
            .child(
                studio_ui::ds::text::body(if saving {
                    "Saving drafts…".to_owned()
                } else {
                    format!("{} not saved", plural(count, "draft", "drafts"))
                })
                .weight(studio_ui::ds::text::weight::HEADING),
            )
            .children(self.drafts.notice.clone().map(|notice| {
                div().min_w_0().flex_shrink(1.).child(
                    studio_ui::ds::text::body(notice.message)
                        .color(notice.tone.color())
                        .truncate(),
                )
            }))
            .when(count > 0 && !saving, |bar| {
                bar.child(
                    studio_ui::ds::bar_button(
                        "drafts-discard",
                        if self.drafts.confirm_discard {
                            format!("Discard {}", plural(count, "draft", "drafts"))
                        } else {
                            "Discard".to_owned()
                        },
                    )
                    .style(if self.drafts.confirm_discard {
                        ButtonStyle::Tinted(TintColor::Error)
                    } else {
                        ButtonStyle::Subtle
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.discard_drafts(cx))),
                )
                .child(
                    studio_ui::ds::bar_button("drafts-save", "Save as one commit")
                        .style(ButtonStyle::Tinted(TintColor::Accent))
                        .disabled(refused.is_some())
                        .tooltip(move |_, cx| match &refused {
                            Some(reason) => Tooltip::simple(reason.clone(), cx),
                            None => Tooltip::for_action_in(
                                save_tooltip.clone(),
                                &SaveDrafts,
                                &focus,
                                cx,
                            ),
                        })
                        .on_click(cx.listener(|this, _, window, cx| this.save_drafts(window, cx))),
                )
            });
        Some(
            h_flex()
                .absolute()
                .bottom(px(space::LG))
                .left_0()
                .right_0()
                .px(px(space::LG))
                .justify_center()
                .child(pill)
                .into_any_element(),
        )
    }

    /// A cell's save matched nothing: the value it was bound to changed elsewhere. Reads the row
    /// at the head to say what the cell holds now, and opens the edit again bound to that.
    fn reread_conflict(&mut self, cell: SavedCell, window: &mut Window, cx: &mut Context<Self>) {
        let SavedCell {
            row_id,
            column,
            old,
            new,
            text,
            shown,
            sent,
        } = cell;
        let Some(schema) = self.schema.clone() else {
            return;
        };
        let property = self.property_of(&column);
        let query = match builder::node_by_id(&schema, &self.type_name, &row_id) {
            Ok(query) => query,
            Err(error) => {
                let places = self.places_of_id(&row_id);
                self.put_back(&places, &column, &new, shown);
                let notice = Notice::new(Tone::Conflict, error.to_string());
                let edit = (old, text, notice);
                self.reopen(row_id, column, edit, sent.as_ref(), window, cx);
                return;
            }
        };
        self.cells.insert(
            (row_id.clone(), column.clone()),
            CellState::Saving {
                stored: shown.clone(),
            },
        );
        self.saves_in_flight += 1;
        let read = self
            .session
            .update(cx, |session, cx| session.read(&query, None, cx));
        let (session, branch) = (self.session.clone(), self.branch.clone());
        let key = self.draft_key(&row_id, &column);
        cx.spawn_in(window, async move |this, cx| {
            // The row at the head: `None` when it could not be read, `Some(None)` when it is
            // gone.
            let now = read
                .await
                .ok()
                .map(|output| node_rows(&output.rows).into_iter().next());
            let shown_here = this.update_in(cx, |this, window, cx| {
                this.saves_in_flight = this.saves_in_flight.saturating_sub(1);
                if !this.shows(&session, &branch) {
                    this.saves_settled(cx);
                    return false;
                }
                let places = this.places_of_id(&row_id);
                // The edit opens again with why, bound to the value read, which the cell now
                // shows; without one, still to the value it was bound to.
                let (bound, notice) = match &now {
                    Some(Some(row)) => {
                        let value = row.value(&column).clone();
                        this.set_cell_at(&places, &column, value.clone());
                        let notice = changed_elsewhere(property.as_ref(), Some(&value));
                        (value, notice)
                    }
                    Some(None) => {
                        this.put_back(&places, &column, &new, shown.clone());
                        this.set_dimmed_at(&places, true);
                        (old.clone(), changed_elsewhere(property.as_ref(), None))
                    }
                    None => {
                        this.put_back(&places, &column, &new, shown.clone());
                        let notice = Notice::new(
                            Tone::Conflict,
                            "The value changed elsewhere while you were editing. Nothing was written.",
                        );
                        (old.clone(), notice)
                    }
                };
                // The table now shows a value newer than its pin.
                this.newer = true;
                this.saves_settled(cx);
                cx.emit(ItemEvent::UpdateTab);
                let (row_id, column) = (row_id.clone(), column.clone());
                let edit = (bound, text.clone(), notice);
                this.reopen(row_id, column, edit, sent.as_ref(), window, cx);
                this.tell_peek_saves(cx);
                true
            });
            // The table moved on, or its tab closed: the text stays a draft where it was typed,
            // and nothing here changes. It stays bound to the value it was typed over: what
            // the head holds was never shown, and why it changed went with the table, so its
            // next save finds the change again rather than writing over it.
            if !shown_here.unwrap_or(false) {
                let keep = Some(DraftEntry { old, text });
                session.update(cx, |session, cx| {
                    session.settle_draft_on(&branch, key, sent.as_ref(), keep, cx)
                });
            }
        })
        .detach();
    }

    /// Puts an edit that did not land (`edit`: the value it is bound to now, its text, why)
    /// back in front of the person: the editor reopens on the cell when they are not busy
    /// elsewhere, otherwise the cell keeps the draft and the notice. A draft retyped or let go
    /// elsewhere while the save was out (no longer `sent`) stands, and nothing reopens.
    fn reopen(
        &mut self,
        row_id: String,
        column: String,
        edit: (Value, String, Notice),
        sent: Option<&DraftEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (old, text, notice) = edit;
        let key = (row_id.clone(), column.clone());
        let position = self.row_index(&row_id).zip(self.column_index(&column));
        let free = self.editing.is_none()
            && self.filter_draft.is_none()
            && self.focus_handle.contains_focused(window, cx);
        self.cells.remove(&key);
        let keep = (DraftEntry { old, text }, Some(notice));
        let settled = self.settle_draft(&row_id, &column, sent, Some(keep), cx);
        self.sync_drafts(cx);
        if !settled {
            cx.notify();
            return;
        }
        if let (true, Some((row, col))) = (free, position) {
            let cursor = Cursor { row, col };
            self.set_cursor(cursor, cx);
            self.start_edit(cursor, window, cx);
        }
        cx.notify();
    }

    // ---- actions ----

    fn on_cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        // Esc leaves the sentence field and clears what it said; a call under way is dropped,
        // and the filters it made stay as pills.
        if self.words_editor.focus_handle(cx).is_focused(window)
            || !matches!(self.words, WordsState::Typing(None))
        {
            self.words = WordsState::Typing(None);
            window.focus(&self.focus_handle, cx);
            cx.notify();
            return;
        }
        // Esc closes the fill; a fill under way stops, and the drafts it made stay.
        if self.fill.take().is_some() {
            window.focus(&self.focus_handle, cx);
            cx.notify();
            return;
        }
        if !self.selection.is_empty() && self.editing.is_none() {
            self.clear_selection(cx);
            window.focus(&self.focus_handle, cx);
            return;
        }
        if self.filter_draft.is_some() {
            self.cancel_filter_draft(window, cx);
            return;
        }
        // Esc ends the edit; a draft the cell held stays, as it was before the edit.
        if self.editing.take().is_some() {
            self.finish_editing(window, cx);
            return;
        }
        if self.peek.is_some() {
            self.close_peek(cx);
            return;
        }
        if self.cursor.take().is_some() {
            cx.notify();
        }
    }

    /// Tab: save the open edit, if any, and move to the next cell in the row.
    fn tab(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.filter_draft.is_some() && self.filter_editor.focus_handle(cx).is_focused(window) {
            return;
        }
        if self.editing.is_some() {
            self.confirm_edit(window, cx);
            if self.editing.is_some() {
                // The input did not parse; stay on the cell.
                return;
            }
        }
        self.move_cursor(0, step, cx);
    }

    fn on_edit(&mut self, _: &EditCell, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cursor) = self
            .cursor
            .or_else(|| (!self.rows.is_empty()).then_some(Cursor { row: 0, col: 0 }))
        else {
            return;
        };
        self.set_cursor(cursor, cx);
        self.start_edit(cursor, window, cx);
    }

    /// G: the row's node on the explorer, rooted there (spec 006: from a table into the graph).
    fn on_explore(&mut self, _: &ExploreRow, window: &mut Window, cx: &mut Context<Self>) {
        if self.kind != TableKind::Node {
            return;
        }
        let row = self.cursor.map_or(0, |c| c.row);
        let Some(id) = self.rows.get(row).map(|r| r.id.clone()) else {
            return;
        };
        window.dispatch_action(
            Box::new(studio_actions::graph::ShowNodeOnCanvas {
                type_name: self.type_name.clone(),
                id,
            }),
            cx,
        );
    }

    fn on_peek(&mut self, _: &PeekRow, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(cursor) = self
            .cursor
            .or_else(|| (!self.rows.is_empty()).then_some(Cursor { row: 0, col: 0 }))
        else {
            return;
        };
        let peeking_this = self.peek.as_ref().map(|(id, _)| id.as_str())
            == self.rows.get(cursor.row).map(|row| row.id.as_str());
        if peeking_this {
            self.close_peek(cx);
        } else {
            self.cursor = Some(cursor);
            self.show_peek(cursor.row, false, cx);
        }
    }

    fn on_copy(&mut self, _: &CopyCell, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(cursor) = self.cursor else { return };
        let (Some(row), Some(column)) = (self.rows.get(cursor.row), self.columns.get(cursor.col))
        else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(studio_ui::value_text(
            row.value(&column.key),
        )));
    }

    fn on_insert(&mut self, _: &InsertRow, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(reason) = self.write_refusal(cx) {
            self.toast(ToastKind::Info, &reason, cx);
            return;
        }
        let (Some(schema), Some(workspace)) = (self.schema.clone(), self.workspace.upgrade())
        else {
            return;
        };
        let table = cx.entity().downgrade();
        let session = self.session.clone();
        let type_name = self.type_name.clone();
        let snapshot = self.paging.pinned.clone();
        let edge = self.kind == TableKind::Edge;
        workspace.update(cx, |workspace, cx| {
            if edge {
                workspace.toggle_modal(window, cx, |window, cx| {
                    InsertEdgeModal::new(table, session, schema, type_name, snapshot, window, cx)
                })
            } else {
                workspace.toggle_modal(window, cx, |window, cx| {
                    InsertRowModal::new(table, session, schema, type_name, window, cx)
                })
            }
        });
    }

    fn on_delete(&mut self, _: &DeleteRow, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(reason) = self.write_refusal(cx) {
            self.toast(ToastKind::Info, &reason, cx);
            return;
        }
        let (Some(schema), Some(workspace)) = (self.schema.clone(), self.workspace.upgrade())
        else {
            return;
        };
        // The picked rows, in table order, or else the cursor's.
        let picked: Vec<&Row> = if self.selection.is_empty() {
            self.cursor
                .and_then(|cursor| self.rows.get(cursor.row))
                .into_iter()
                .collect()
        } else {
            let mut seen = HashSet::new();
            self.rows
                .iter()
                .filter(|row| self.selection.contains(&row.id) && seen.insert(row.id.clone()))
                .collect()
        };
        if picked.is_empty() {
            return;
        }
        // Of the type this table shows: a node type and an edge type may share a name.
        let edge = self.kind == TableKind::Edge;
        let label = schema
            .node(&self.type_name)
            .filter(|_| !edge)
            .and_then(|node| node.label_property().map(str::to_owned));
        let targets: Vec<DeleteTarget> = picked
            .into_iter()
            .map(|row| {
                let text = |column: &str| {
                    row.values
                        .get(column)
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                };
                DeleteTarget {
                    id: row.id.clone(),
                    label: label.as_deref().and_then(text),
                    endpoints: edge.then(|| {
                        (
                            text(crate::model::FROM_COLUMN).unwrap_or_default(),
                            text(crate::model::TO_COLUMN).unwrap_or_default(),
                        )
                    }),
                }
            })
            .collect();
        let table = cx.entity().downgrade();
        let session = self.session.clone();
        let (type_name, kind) = (self.type_name.clone(), self.kind);
        let if_commit = self.paging.pinned.clone();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                DeleteRowModal::new(
                    table, session, schema, type_name, kind, targets, if_commit, window, cx,
                )
            })
        });
    }

    fn toast(&self, kind: ToastKind, message: &str, cx: &mut Context<Self>) {
        if let Some(workspace) = self.workspace.upgrade() {
            let message = message.to_owned();
            workspace.update(cx, |workspace, cx| workspace.show_toast(kind, message, cx));
        }
    }

    // ---- filters ----

    /// What the table says when its cells cannot change: an edge table at the head still adds
    /// and deletes rows.
    fn read_only_label(&self) -> &'static str {
        if self.kind == TableKind::Edge && self.at_commit.is_none() {
            "Cells read only"
        } else {
            "Read only"
        }
    }

    /// Why nothing can be written here, when the table reads a past commit.
    fn pinned_reason(&self) -> Option<String> {
        self.at_commit.as_ref().map(|commit| {
            format!(
                "Read-only: pinned to commit {}. Open the table at the branch head to edit.",
                studio_ui::short_commit_id(commit)
            )
        })
    }

    /// Why the credential may not change the branch the table shows, once the access route
    /// said so (spec 002): every control that writes is disabled with it, and its action is
    /// not registered. A table at a commit has its own reason.
    fn change_refusal(&self, cx: &App) -> Option<String> {
        if self.at_commit.is_some() {
            return None;
        }
        write::change_refusal(&self.session, &self.branch, cx)
    }

    /// Why nothing can be written here: the commit the table reads, else the credential.
    fn write_refusal(&self, cx: &App) -> Option<String> {
        self.pinned_reason().or_else(|| self.change_refusal(cx))
    }

    fn filterable(&self) -> impl Iterator<Item = &Property> {
        self.all_columns
            .iter()
            .filter_map(ColumnSpec::property)
            .filter(|p| !FilterOp::for_property(p).is_empty())
    }

    /// Shows only the rows one filter keeps, replacing the others (a table opened from the
    /// relationship tree).
    pub fn show_only(&mut self, filter: Filter, cx: &mut Context<Self>) {
        // Opened with it just now (`TypeTable::opening`): its rows are being read already.
        if self.filter_draft.is_none() && self.filters == [filter.clone()] {
            return;
        }
        self.filter_draft = None;
        self.filters = vec![filter];
        if self.schema.is_some() {
            self.reload(true, cx);
        }
        cx.notify();
    }

    /// The icon of a filter key: the property's kind, or the relationship's direction.
    fn key_icon(&self, key: &str) -> IconName {
        if key == builder::WHERE_FILTER {
            return IconName::Code;
        }
        if let Some((direction, _)) =
            builder::parse_relationship_key(key.split('.').next().unwrap_or(key))
        {
            return direction_icon(direction);
        }
        self.filterable()
            .find(|p| p.name == key)
            .map_or(IconName::CaseSensitive, property_icon)
    }

    /// The property a filter key tests: the table's own, or a neighbour's through a
    /// relationship key (`out:Attended.name`). None for a relationship itself.
    fn property_for_key(&self, key: &str) -> Option<Property> {
        if let Some((relationship, property)) = key.split_once('.') {
            let (direction, edge) = builder::parse_relationship_key(relationship)?;
            let schema = self.schema.as_ref()?;
            let group = schema
                .relationships_of(&self.type_name)
                .iter()
                .find(|group| group.direction == direction && group.edge == edge)?;
            return schema
                .node(&group.other_type)?
                .properties
                .iter()
                .find(|p| p.name == property)
                .filter(|p| !FilterOp::for_property(p).is_empty())
                .cloned();
        }
        self.filterable().find(|p| p.name == key).cloned()
    }

    /// What a filter can test: each filterable property, then (in a node table) each
    /// relationship group, as `(key, label, icon)`.
    fn filter_choices(&self) -> Vec<(String, String, IconName)> {
        let mut choices: Vec<(String, String, IconName)> = self
            .filterable()
            .map(|p| (p.name.clone(), p.name.clone(), property_icon(p)))
            .collect();
        if self.kind == TableKind::Node
            && let Some(schema) = &self.schema
        {
            for group in schema.relationships_of(&self.type_name) {
                let key = builder::relationship_key(group);
                // The entry's icon shows the direction, so the label names the traversal only.
                let label = format!(
                    "{} {}",
                    gq_language::traversal_name(&group.edge),
                    group.other_type
                );
                choices.push((key.clone(), label.clone(), direction_icon(group.direction)));
                // Then the neighbour's own properties, a nested search each: "attended Person
                // · name".
                if let Some(other) = schema.node(&group.other_type) {
                    for property in &other.properties {
                        if property.is_key || FilterOp::for_property(property).is_empty() {
                            continue;
                        }
                        choices.push((
                            builder::neighbour_property_key(group, &property.name),
                            format!("{label} · {}", property.name),
                            direction_icon(group.direction),
                        ));
                    }
                }
            }
            // Anything the pills cannot say, as GQ on the row (spec 004, "Condition").
            choices.push((
                builder::WHERE_FILTER.to_owned(),
                "Condition (GQ)".to_owned(),
                IconName::Code,
            ));
        }
        choices
    }

    /// Judges a condition as the compiler would before a read: the page query with the
    /// condition in it type-checks against the schema, or the first diagnostic says why not.
    fn check_condition(&self, text: &str) -> Result<(), String> {
        let Some(schema) = &self.schema else {
            return Err("The schema is not loaded yet.".to_owned());
        };
        let filter = Filter {
            property: builder::WHERE_FILTER.to_owned(),
            op: FilterOp::Expr,
            value: Value::String(text.to_owned()),
        };
        let query = builder::node_page(
            schema,
            &self.type_name,
            &[filter],
            None,
            &PageStart::First,
            1,
        )
        .map_err(|error| error.to_string())?;
        let checked = gq_language::check(schema, &query.text);
        match checked.diagnostics.first() {
            None => Ok(()),
            Some(diagnostic) => Err(match &diagnostic.fix {
                Some(fix) => format!("{} ({fix})", diagnostic.message),
                None => diagnostic.message.clone(),
            }),
        }
    }

    /// The tests offered for a filter key.
    fn ops_for(&self, key: &str) -> Vec<FilterOp> {
        if key == builder::WHERE_FILTER {
            return vec![FilterOp::Expr];
        }
        if builder::parse_relationship_key(key).is_some() {
            return FilterOp::for_relationship();
        }
        self.property_for_key(key)
            .map(|property| FilterOp::for_property(&property))
            .unwrap_or_default()
    }

    fn open_filter_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let preferred = self
            .cursor
            .and_then(|c| self.columns.get(c.col))
            .and_then(|c| c.property())
            .filter(|p| self.filterable().any(|f| f.name == p.name))
            .or_else(|| self.filterable().next())
            .cloned();
        let Some(property) = preferred else {
            self.toast(ToastKind::Info, "This table has no filterable columns.", cx);
            return;
        };
        self.set_draft_property(property.name, window, cx);
    }

    fn set_draft_property(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(&op) = self.ops_for(&name).first() else {
            return;
        };
        self.filter_draft = Some(FilterDraft {
            property: name,
            op,
            error: None,
            replacing: None,
            offers: Offers::default(),
            // An empty field answers itself; text put in it is searched for.
            answers: Some(String::new()),
            suggest_generation: 0,
            _suggest_task: None,
        });
        self.filter_editor.update(cx, |editor, cx| {
            editor.set_text("", window, cx);
            editor.set_placeholder_text(filter_placeholder(op), window, cx);
        });
        window.focus(&self.filter_editor.focus_handle(cx), cx);
        self.refresh_suggestions(cx);
        cx.notify();
    }

    /// Where the value field of a filter on `key` with `op` finds what to offer: an enum's
    /// values, `true` and `false`, or for "related to" the other type's nodes.
    fn suggestion_source(&self, key: &str, op: FilterOp) -> Option<SuggestSource> {
        if !op.takes_value() {
            return None;
        }
        if op == FilterOp::RelatedTo {
            let group = self
                .schema
                .as_ref()?
                .relationships_of(&self.type_name)
                .iter()
                .find(|group| builder::relationship_key(group) == key)?;
            return Some(SuggestSource::Nodes(group.other_type.clone()));
        }
        let property = self.property_for_key(key)?;
        if let Some(values) = &property.enum_values {
            return Some(SuggestSource::Values(values.clone()));
        }
        (property.kind == ScalarKind::Bool)
            .then(|| SuggestSource::Values(vec!["true".to_owned(), "false".to_owned()]))
    }

    /// Updates what the value field offers for its text. Nodes are searched on the server,
    /// at the table's commit, once typing pauses.
    fn refresh_suggestions(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = &self.filter_draft else {
            return;
        };
        let text = self.filter_editor.read(cx).text(cx);
        let list = draft.op == FilterOp::In;
        let source = suggest::completes(&text, list)
            .then(|| self.suggestion_source(&draft.property, draft.op))
            .flatten();
        let typed = !suggest::needle(&text, list).is_empty();
        let schema = self.schema.clone();
        let snapshot = self.paging.pinned.clone();
        let session = self.session.clone();
        let Some(draft) = &mut self.filter_draft else {
            return;
        };
        draft.suggest_generation += 1;
        let generation = draft.suggest_generation;
        match source {
            None => {
                draft.offers.clear();
                draft.answers = Some(text);
                draft._suggest_task = None;
            }
            Some(SuggestSource::Values(values)) => {
                draft
                    .offers
                    .set(suggest::from_values(&values, &text, list), typed);
                draft.answers = Some(text);
                draft._suggest_task = None;
            }
            Some(SuggestSource::Nodes(type_name)) => {
                let Some(schema) = schema else {
                    draft.offers.clear();
                    draft.answers = Some(text);
                    return;
                };
                let needle = suggest::needle(&text, list);
                let search =
                    suggest::search_nodes(&session, &schema, &type_name, &needle, snapshot, cx);
                draft._suggest_task = Some(cx.spawn(async move |this, cx| {
                    let result = search.await;
                    this.update(cx, |this, cx| {
                        let Some(draft) = &mut this.filter_draft else {
                            return;
                        };
                        if draft.suggest_generation != generation {
                            return;
                        }
                        // A failed search offers nothing, not what an earlier one found.
                        match result {
                            Ok(rows) => draft.offers.set(suggest::from_node_rows(&rows), typed),
                            Err(_) => draft.offers.clear(),
                        }
                        draft.answers = Some(text);
                        this.redraw_offers(cx);
                    })
                    .ok();
                }));
            }
        }
        // No `notify`: it would draw the grid on every key. The callers that change the draft
        // notify; a keystroke draws the table again because its field changed.
    }

    /// ↑/↓ in the value field: highlights another offered value.
    fn move_suggestion(&mut self, step: isize, cx: &mut Context<Self>) {
        if let Some(draft) = &mut self.filter_draft
            && draft.offers.step(step)
        {
            self.redraw_offers(cx);
        }
    }

    /// Draws the offers again: what the value field offers, or the one highlighted, changed.
    /// They are drawn under the field, beside the grid, so the field is what is notified: that
    /// draws the views around it again, as a key typed in it does, and leaves the grid as it
    /// was. A notify of the table would draw the grid again too (`TableGrid`).
    fn redraw_offers(&self, cx: &mut Context<Self>) {
        self.filter_editor.update(cx, |_, cx| cx.notify());
    }

    /// Puts an offered value in the field. A single value adds the filter at once; a list
    /// waits for its next value.
    fn pick_suggestion(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = &self.filter_draft else {
            return;
        };
        let Some(suggestion) = draft.offers.get(index).cloned() else {
            return;
        };
        let list = draft.op == FilterOp::In;
        let text = self.filter_editor.read(cx).text(cx);
        let text = suggest::complete(&text, &suggestion.value, list);
        if let Some(label) = &suggestion.label {
            self.node_labels
                .insert(suggestion.value.clone(), label.clone());
        }
        self.filter_editor.update(cx, |editor, cx| {
            editor.set_text(text, window, cx);
            // Not `move_to_end`: in a single-line editor it only propagates, and the ↵ that
            // picked would then also be typed into the field.
            let end = editor.buffer().read(cx).read(cx).len();
            editor.change_selections(Default::default(), window, cx, |s| {
                s.select_ranges(vec![end..end])
            });
        });
        if list {
            self.refresh_suggestions(cx);
        } else {
            self.apply_filter_draft(window, cx);
        }
    }

    /// A click on a pill: the filter comes out of the bar into the draft, its key, test and
    /// value as they were, to change and apply again or put back with Esc.
    fn edit_filter(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.filters.len() {
            return;
        }
        // A draft already open goes back first, so nothing is lost.
        self.cancel_filter_draft(window, cx);
        let filter = self.filters.remove(index);
        self.set_draft_property(filter.property.clone(), window, cx);
        let Some(draft) = &mut self.filter_draft else {
            self.filters.insert(index, filter);
            return;
        };
        draft.op = filter.op;
        let text = if filter.op.takes_value() {
            edit_text(&filter.value)
        } else {
            String::new()
        };
        draft.replacing = Some(filter);
        let op = draft.op;
        self.filter_editor.update(cx, |editor, cx| {
            editor.set_text(text, window, cx);
            editor.set_placeholder_text(filter_placeholder(op), window, cx);
            let end = editor.buffer().read(cx).read(cx).len();
            editor.change_selections(Default::default(), window, cx, |s| {
                s.select_ranges(vec![end..end])
            });
        });
        self.refresh_suggestions(cx);
        cx.notify();
    }

    /// Closes the draft; a filter it was editing goes back where it was.
    fn cancel_filter_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(draft) = self.filter_draft.take() {
            if let Some(filter) = draft.replacing {
                self.filters.push(filter);
            }
            window.focus(&self.focus_handle, cx);
            cx.notify();
        }
    }

    fn set_draft_op(&mut self, op: FilterOp, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(draft) = &mut self.filter_draft {
            draft.op = op;
            draft.error = None;
        }
        self.filter_editor.update(cx, |editor, cx| {
            editor.set_placeholder_text(filter_placeholder(op), window, cx)
        });
        if op.takes_value() {
            window.focus(&self.filter_editor.focus_handle(cx), cx);
        }
        self.refresh_suggestions(cx);
        cx.notify();
    }

    fn apply_filter_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = &self.filter_draft else {
            return;
        };
        let key = draft.property.clone();
        let op = draft.op;
        let text = self.filter_editor.read(cx).text(cx);
        let parsed = if op == FilterOp::Expr {
            let text = text.trim();
            if text.is_empty() {
                Err("Enter a condition".to_owned())
            } else {
                self.check_condition(text)
                    .map(|()| Value::String(text.to_owned()))
            }
        } else if op.is_relationship() {
            match op {
                FilterOp::RelatedTo if text.trim().is_empty() => Err("Enter a node id".to_owned()),
                FilterOp::RelatedTo => Ok(Value::String(text.trim().to_owned())),
                _ => Ok(Value::Null),
            }
        } else {
            let Some(property) = self.property_for_key(&key) else {
                return;
            };
            if op == FilterOp::In {
                parse_filter_list(&property, &text)
            } else if op.takes_value() {
                parse_filter_value(&property, &text)
            } else {
                Ok(Value::Null)
            }
        };
        let value = {
            match parsed {
                Ok(value) => value,
                Err(error) => {
                    if let Some(draft) = &mut self.filter_draft {
                        draft.error = Some(error);
                    }
                    cx.notify();
                    return;
                }
            }
        };
        self.filter_draft = None;
        self.filters.push(Filter {
            property: key,
            op,
            value,
        });
        window.focus(&self.focus_handle, cx);
        self.reload(true, cx);
    }

    // ---- filter in words (spec 019) ----

    fn open_words(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.words_editor.focus_handle(cx), cx);
        cx.notify();
    }

    /// The keys the filter menu offers, with their operators and properties, for the model.
    fn words_choices(&self) -> Vec<words::Choice> {
        self.filter_choices()
            .into_iter()
            .map(|(key, label, _)| words::Choice {
                ops: self.ops_for(&key),
                property: self.property_for_key(&key),
                other_type: self.related_type(&key),
                traversal: builder::parse_relationship_key(&key)
                    .map(|(_, edge)| gq_language::traversal_name(edge)),
                key,
                label,
            })
            .collect()
    }

    /// The type at the other end of a relationship key.
    fn related_type(&self, key: &str) -> Option<String> {
        let (direction, edge) = builder::parse_relationship_key(key)?;
        self.schema
            .as_ref()?
            .relationships_of(&self.type_name)
            .iter()
            .find(|group| group.direction == direction && group.edge == edge)
            .map(|group| group.other_type.clone())
    }

    /// ↵ in the sentence field: the sentence goes to the model, once the task is allowed.
    fn run_words(&mut self, cx: &mut Context<Self>) {
        let sentence = self.words_editor.read(cx).text(cx).trim().to_owned();
        if sentence.is_empty() || matches!(self.words, WordsState::Running { .. }) {
            return;
        }
        let Some(models) = Models::try_global(cx) else {
            self.words = WordsState::Failed(ModelError::NoKey.to_string());
            cx.notify();
            return;
        };
        self.start_words(models, sentence, cx);
    }

    fn start_words(&mut self, models: Entity<Models>, sentence: String, cx: &mut Context<Self>) {
        let choices = self.words_choices();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let request = ModelRequest {
            task: &words::TASK,
            instructions: words::INSTRUCTIONS,
            input: words::input(&self.type_name, &choices, &today, &sentence),
            schema: words::schema(&choices),
        };
        let call = models.read(cx).call::<words::Answer>(request, cx);
        let task = cx.spawn(async move |this, cx| {
            let answer = match call.await {
                Ok(answer) => answer,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.words = WordsState::Failed(error.to_string());
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            let (mut filters, mut refused) = words::filters(answer.value, &choices);
            // A `related_to` names a record; the name becomes its id through the node search
            // the filter's own value field uses, the first match taken, and the pill shows it.
            let mut labels: Vec<(String, String)> = Vec::new();
            for filter in &mut filters {
                if filter.op != FilterOp::RelatedTo {
                    continue;
                }
                let Some(name) = filter.value.as_str().map(str::to_owned) else {
                    continue;
                };
                let other = choices
                    .iter()
                    .find(|choice| choice.key == filter.property)
                    .and_then(|choice| choice.other_type.clone());
                let search = this
                    .read_with(cx, |this, cx| {
                        let (other, schema) = (other.as_deref()?, this.schema.clone()?);
                        Some(suggest::search_nodes(
                            &this.session,
                            &schema,
                            other,
                            &name,
                            this.paging.pinned.clone(),
                            cx,
                        ))
                    })
                    .ok()
                    .flatten();
                let found = match search {
                    Some(search) => search
                        .await
                        .ok()
                        .and_then(|rows| suggest::from_node_rows(&rows).into_iter().next()),
                    None => None,
                };
                match found {
                    Some(node) => {
                        if let Some(label) = node.label {
                            labels.push((node.value.clone(), label));
                        }
                        filter.value = Value::String(node.value);
                    }
                    None => {
                        refused.push(format!(
                            "no {} matching \"{name}\"",
                            other.unwrap_or_default()
                        ));
                        filter.value = Value::Null;
                    }
                }
            }
            filters.retain(|filter| filter.op != FilterOp::RelatedTo || !filter.value.is_null());
            // A condition the model wrote is judged as a typed one is: by the compiler, before
            // any read; one it refuses is dropped and the diagnostic named.
            let mut judged = Vec::with_capacity(filters.len());
            for filter in filters {
                if filter.op == FilterOp::Expr {
                    let text = filter.value.as_str().unwrap_or_default().to_owned();
                    match this
                        .read_with(cx, |this, _| this.check_condition(&text))
                        .unwrap_or_else(|_| Err("The table was closed.".to_owned()))
                    {
                        Ok(()) => judged.push(filter),
                        Err(why) => refused.push(format!("condition: {why}")),
                    }
                } else {
                    judged.push(filter);
                }
            }
            let filters = judged;
            this.update(cx, |this, cx| {
                this.node_labels.extend(labels);
                // The last sentence's filters go; ones added by hand stay.
                let made = std::mem::take(&mut this.words_made);
                this.filters.retain(|filter| !made.contains(filter));
                this.words_made = filters.clone();
                this.filters.extend(filters);
                let note =
                    (!refused.is_empty()).then(|| format!("Could not use: {}", refused.join("; ")));
                this.words = WordsState::Typing(note);
                this.reload(true, cx);
                cx.notify();
            })
            .ok();
        });
        self.words = WordsState::Running { _call: task };
        cx.notify();
    }

    /// The toolbar's sentence field (spec 019): a search field with the model's mark, its
    /// spinner while the call is out, and the field's red line when the call failed; the
    /// answer's filters are the bar's pills. ↵ asks, Esc leaves.
    fn render_words_field(&self, cx: &mut Context<Self>) -> AnyElement {
        let running = matches!(self.words, WordsState::Running { .. });
        let failed = matches!(self.words, WordsState::Failed(_));
        let about = match &self.words {
            WordsState::Failed(why) => why.clone(),
            _ => format!(
                "Filter in words (↵): {} go to {}",
                words::TASK.sends,
                Models::model_for(words::TASK.tier, cx)
            ),
        };
        studio_ui::ds::pill_field("filter-words", failed, cx)
            .key_context("FilterWords")
            .w(px(studio_ui::ds::size::FIELD_WIDE))
            .tooltip(Tooltip::text(about))
            .child(studio_ui::ds::icon::meta(IconName::ZedAssistant))
            .child(studio_ui::ds::field_input(self.words_editor.clone()))
            .when(running, |field| field.child(studio_ui::ds::spinner()))
            .into_any_element()
    }

    fn filter_by_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cursor) = self.cursor else {
            return;
        };
        let (Some(row), Some(column)) = (self.rows.get(cursor.row), self.columns.get(cursor.col))
        else {
            return;
        };
        let Some(property) = column.property() else {
            return;
        };
        let value = row.value(&column.key).clone();
        let ops = FilterOp::for_property(property);
        let filter = if value.is_null() {
            ops.contains(&FilterOp::IsEmpty).then(|| Filter {
                property: property.name.clone(),
                op: FilterOp::IsEmpty,
                value: Value::Null,
            })
        } else if property.list {
            value
                .as_array()
                .and_then(|items| items.first())
                .map(|first| Filter {
                    property: property.name.clone(),
                    op: FilterOp::Contains,
                    value: first.clone(),
                })
        } else if ops.contains(&FilterOp::Eq) {
            Some(Filter {
                property: property.name.clone(),
                op: FilterOp::Eq,
                value,
            })
        } else {
            None
        };
        if let Some(filter) = filter {
            self.filters.push(filter);
            window.focus(&self.focus_handle, cx);
            self.reload(true, cx);
        }
    }

    fn remove_filter(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.filters.len() {
            self.filters.remove(index);
            self.reload(true, cx);
        }
    }

    fn clear_filters(&mut self, cx: &mut Context<Self>) {
        self.filter_draft = None;
        if !self.filters.is_empty() {
            self.filters.clear();
            self.reload(true, cx);
        }
        cx.notify();
    }

    // ---- rendering ----

    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let count = match (&self.count, &self.total, self.filters.is_empty()) {
            (Loadable::Loaded(n), _, true) => {
                format!(
                    "{} {}",
                    studio_ui::group_count(*n),
                    if *n == 1 { "row" } else { "rows" }
                )
            }
            (Loadable::Loaded(n), Loadable::Loaded(total), false) => {
                format!(
                    "{} of {}",
                    studio_ui::group_count(*n),
                    studio_ui::group_count(*total)
                )
            }
            (Loadable::Loaded(n), _, false) => format!("{} matching", studio_ui::group_count(*n)),
            (Loadable::Failed(_), _, _) => "count unavailable".into(),
            (Loadable::Loading, _, _) => "counting…".into(),
            _ => String::new(),
        };
        let count_error = match &self.count {
            Loadable::Failed(error) => Some(error.clone()),
            _ => None,
        };
        let read_only = self.kind == TableKind::Edge || self.at_commit.is_some();
        let pinned = self.at_commit.is_some();
        let refused = self.change_refusal(cx);
        let target_tooltip = if let Some(reason) = self.pinned_reason().or_else(|| refused.clone())
        {
            reason
        } else if read_only {
            format!(
                "Edges are added and deleted here, committing to {}; their cells cannot change: \
                 the engine has no edge update (T16).",
                self.branch
            )
        } else {
            format!("Edits, inserts and deletes here commit to {}", self.branch)
        };
        studio_ui::ds::toolbar(cx)
            .child(studio_ui::kind_swatch(
                &self.type_name,
                self.kind == TableKind::Edge,
                cx,
            ))
            .child(studio_ui::ds::text::title(self.type_name.clone()))
            .child(
                div()
                    .id("row-count")
                    .when_some(count_error, |d, error| d.tooltip(Tooltip::text(error)))
                    .child(studio_ui::ds::text::muted(count)),
            )
            // Where edits go is the branch the status bar names; the table says only when it
            // cannot edit, the credential's refusal included.
            .when(read_only || refused.is_some(), |d| {
                d.child(
                    div()
                        .id("write-target")
                        .tooltip(Tooltip::text(target_tooltip))
                        .child(match &self.at_commit {
                            Some(commit) => studio_ui::ds::meta_code(
                                IconName::GitCommit,
                                format!("Read only · {}", studio_ui::short_commit_id(commit)),
                                cx,
                            )
                            .into_any_element(),
                            None if refused.is_some() => {
                                studio_ui::ds::text::meta("Read only").into_any_element()
                            }
                            None => {
                                studio_ui::ds::text::meta(self.read_only_label()).into_any_element()
                            }
                        }),
                )
            })
            .when(self.newer, |d| {
                d.child(
                    studio_ui::ds::button("newer-data", "Newer data · Refresh")
                        .style(ButtonStyle::Tinted(TintColor::Accent))
                        .tooltip(|_, cx| {
                            Tooltip::for_action("Read the branch head again", &RefreshTable, cx)
                        })
                        .on_click(cx.listener(|this, _, window, cx| this.refresh(window, cx))),
                )
            })
            .child(div().flex_1())
            .child(self.render_words_field(cx))
            .child(
                // The controls as chips (spec 002, "Control chips"): each set one says what it
                // is set to and clears with its ×.
                studio_ui::ds::toolbar_actions()
                    .gap(px(space::SM))
                    .child(self.render_views_menu(cx))
                    .child(self.render_columns_menu(cx))
                    .child(self.render_sort_menu(cx))
                    .child(self.render_group_menu(cx))
                    .child(
                        studio_ui::ds::control_chip(
                            "filter-chip",
                            studio_ui::ds::chip_button(
                                "add-filter",
                                IconName::Filter,
                                "Filters",
                                (!self.filters.is_empty()).then(|| {
                                    studio_ui::group_count(self.filters.len() as i64).into()
                                }),
                            )
                            .tooltip(|_, cx| Tooltip::for_action("Add a filter", &AddFilter, cx))
                            .on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.open_filter_draft(window, cx)
                                }),
                            ),
                        )
                        .when(!self.filters.is_empty(), |chip| {
                            chip.on_clear(
                                "Clear the filters",
                                cx.listener(|this, _, _, cx| this.clear_filters(cx)),
                            )
                        }),
                    )
                    .child(studio_ui::ds::control_chip(
                        "insert-row-chip",
                        studio_ui::ds::chip_button("insert-row", IconName::Plus, "Row", None)
                            .disabled(pinned || refused.is_some())
                            .tooltip(move |_, cx| match &refused {
                                _ if pinned => Tooltip::simple("Read-only at an older commit", cx),
                                Some(reason) => Tooltip::simple(reason.clone(), cx),
                                None => Tooltip::for_action("Add a row", &InsertRow, cx),
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.on_insert(&InsertRow, window, cx)
                            })),
                    ))
                    .child(
                        studio_ui::ds::icon_button("refresh-table", IconName::RotateCw)
                            .tooltip(|_, cx| Tooltip::for_action("Refresh", &RefreshTable, cx))
                            .on_click(cx.listener(|this, _, window, cx| this.refresh(window, cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_filter_bar(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let words_note = match &self.words {
            WordsState::Typing(Some(note)) => Some(studio_ui::ds::text::muted(note.clone())),
            WordsState::Failed(why) => Some(studio_ui::ds::text::error(why.clone())),
            _ => None,
        };
        if self.filters.is_empty() && self.filter_draft.is_none() && words_note.is_none() {
            return None;
        }
        let editor_bg = studio_ui::ds::surface::document(cx);
        let mut bar = h_flex()
            .flex_none()
            .flex_wrap()
            .px(px(space::EDGE))
            .py(px(space::SM))
            .gap(px(space::SM))
            .items_center()
            .border_b_1()
            .border_color(studio_ui::ds::surface::rule(cx));
        for (index, filter) in self.filters.iter().enumerate() {
            let (field, test, mut value) = filter_parts(filter);
            if filter.op == FilterOp::RelatedTo
                && let Some(label) = value.as_ref().and_then(|id| self.node_labels.get(id))
            {
                value = Some(label.clone());
            }
            bar = bar.child(
                studio_ui::ds::filter_pill(
                    ("filter", index),
                    self.key_icon(&filter.property),
                    field,
                    test,
                    value.map(Into::into),
                    cx.listener(move |this, _, _, cx| this.remove_filter(index, cx)),
                    cx,
                )
                .cursor_pointer()
                .tooltip(Tooltip::text("Edit this filter"))
                .on_click(
                    cx.listener(move |this, _, window, cx| this.edit_filter(index, window, cx)),
                ),
            );
        }
        if let Some(draft) = &self.filter_draft {
            let table = cx.entity().downgrade();
            let choices = self.filter_choices();
            let chosen = choices
                .iter()
                .find(|(key, _, _)| *key == draft.property)
                .map(|(_, label, _)| label.clone())
                .unwrap_or_else(|| draft.property.clone());
            let ops = self.ops_for(&draft.property);
            let property_table = table.clone();
            let chosen_icon = self.key_icon(&draft.property);
            let chevron = || studio_ui::ds::icon::meta(IconName::ChevronDown);
            // The filter being written is the pill it will become, outlined while it is open:
            // a ring outside it as the pill's own edge is, so its buttons keep a button's height.
            let pill = h_flex()
                .h(px(studio_ui::ds::size::PILL))
                .flex_none()
                .rounded_full()
                .shadow(vec![gpui::BoxShadow {
                    color: if draft.error.is_some() {
                        studio_ui::ds::surface::error_border(cx)
                    } else {
                        studio_ui::ds::surface::focus(cx)
                    },
                    offset: gpui::point(px(0.), px(0.)),
                    blur_radius: px(0.),
                    spread_radius: px(studio_ui::ds::size::HAIRLINE),
                    inset: false,
                }])
                .bg(editor_bg)
                .overflow_hidden()
                .child(
                    PopoverMenu::new("filter-property")
                        .with_handle(self.filter_menus[0].clone())
                        .trigger(
                            Button::new("filter-property-button", chosen)
                                .style(ButtonStyle::Subtle)
                                .start_icon(Icon::new(chosen_icon))
                                .end_icon(chevron()),
                        )
                        .menu(move |window, cx| {
                            let table = property_table.clone();
                            let choices = choices.clone();
                            Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                let mut relationships = false;
                                for (key, label, icon) in choices {
                                    if !relationships
                                        && builder::parse_relationship_key(&key).is_some()
                                    {
                                        relationships = true;
                                        menu = menu.separator().header("Relationships");
                                    }
                                    let table = table.clone();
                                    let name = key.clone();
                                    menu = menu.item(
                                        ContextMenuEntry::new(label)
                                            .icon(icon)
                                            .icon_color(Color::Muted)
                                            .handler(move |window, cx| {
                                                table
                                                    .update(cx, |this, cx| {
                                                        this.set_draft_property(
                                                            name.clone(),
                                                            window,
                                                            cx,
                                                        )
                                                    })
                                                    .ok();
                                            }),
                                    );
                                }
                                menu
                            }))
                        }),
                )
                .child(studio_ui::ds::pill_rule(cx))
                .child(
                    PopoverMenu::new("filter-op")
                        .with_handle(self.filter_menus[1].clone())
                        .trigger(
                            Button::new("filter-op-button", draft.op.label())
                                .style(ButtonStyle::Subtle)
                                .color(Color::Muted)
                                .end_icon(chevron()),
                        )
                        .menu(move |window, cx| {
                            let table = table.clone();
                            let ops = ops.clone();
                            Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                for op in ops {
                                    let table = table.clone();
                                    menu = menu.entry(op.label(), None, move |window, cx| {
                                        table
                                            .update(cx, |this, cx| {
                                                this.set_draft_op(op, window, cx)
                                            })
                                            .ok();
                                    });
                                }
                                menu
                            }))
                        }),
                )
                .when(draft.op.takes_value(), |pill| {
                    let menu_open = self.filter_menus.iter().any(PopoverMenuHandle::is_deployed);
                    let offers = (self.filter_editor.focus_handle(cx).is_focused(window)
                        && !menu_open
                        && !draft.offers.is_empty())
                    .then(|| self.render_suggestions(draft, cx));
                    pill.child(studio_ui::ds::pill_rule(cx)).child(
                        div()
                            .key_context("FilterValue")
                            .on_action(cx.listener(|this, _: &SelectNextSuggestion, _, cx| {
                                this.move_suggestion(1, cx)
                            }))
                            .on_action(cx.listener(|this, _: &SelectPreviousSuggestion, _, cx| {
                                this.move_suggestion(-1, cx)
                            }))
                            .relative()
                            .w(px(studio_ui::ds::size::FIELD_NARROW))
                            .px(px(space::MD))
                            .child(self.filter_editor.clone())
                            .children(offers),
                    )
                })
                .child(studio_ui::ds::pill_rule(cx))
                .child(
                    h_flex()
                        .h_full()
                        .pl(px(space::XS))
                        .pr(px(studio_ui::ds::PILL_BUTTON_GAP))
                        .gap(px(space::XXS))
                        .items_center()
                        .child(
                            studio_ui::ds::pill_button(
                                "apply-filter",
                                IconName::Check,
                                Color::Accent,
                                cx,
                            )
                            .tooltip(Tooltip::text("Add this filter (↵)"))
                            .on_click(cx.listener(
                                |this, _, window, cx| this.apply_filter_draft(window, cx),
                            )),
                        )
                        .child(
                            studio_ui::ds::pill_button(
                                "cancel-filter",
                                IconName::Close,
                                Color::Muted,
                                cx,
                            )
                            .tooltip(Tooltip::text("Discard this filter (Esc)"))
                            .on_click(cx.listener(
                                |this, _, window, cx| this.cancel_filter_draft(window, cx),
                            )),
                        ),
                );
            bar = bar
                .child(pill)
                .children(draft.error.clone().map(studio_ui::ds::text::error));
        }
        bar = bar.children(words_note);
        if !self.filters.is_empty() {
            bar = bar.child(div().flex_1()).child(
                studio_ui::ds::button("clear-filters", "Clear all")
                    .style(ButtonStyle::Subtle)
                    .tooltip(|_, cx| Tooltip::for_action("Remove every filter", &ClearFilters, cx))
                    .on_click(cx.listener(|this, _, _, cx| this.clear_filters(cx))),
            );
        }
        Some(bar.into_any_element())
    }

    /// The values the filter's value field offers, under it: ↑/↓ highlight, ↵ or a click picks.
    fn render_suggestions(&self, draft: &FilterDraft, cx: &mut Context<Self>) -> AnyElement {
        let pick =
            cx.listener(|this, index: &usize, window, cx| this.pick_suggestion(*index, window, cx));
        let items = draft.offers.rows(
            "filter-suggestion",
            Rc::new(move |index, window, cx| pick(&index, window, cx)),
            cx,
        );
        // Under the value field, its list's rows on the field's text.
        floating_under(
            point(px(-space::XS), px(studio_ui::ds::size::CONTROL)),
            1,
            studio_ui::ds::popover_list(cx)
                .id("filter-suggestions")
                .occlude()
                .min_w(px(studio_ui::ds::size::POPOVER_MIN))
                .max_w(px(studio_ui::ds::size::POPOVER_MAX))
                .max_h(px(studio_ui::ds::size::LIST_MAX_HEIGHT))
                .overflow_y_scroll()
                .children(items),
        )
    }

    fn render_body(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        self.frame_now = chrono::Local::now();
        self.frame_minute = self.frame_now.timestamp().div_euclid(60);
        self.edit_slot.clear();
        if self.missing {
            return studio_ui::ds::empty_state(
                IconName::Warning,
                format!("No type {}", self.type_name),
            )
            .child(studio_ui::ds::state_detail(
                "It may have been renamed or dropped. Close this tab.",
            ))
            .into_any_element();
        }
        match &self.first_page {
            Loadable::Failed(error) => {
                return studio_ui::ds::error_state(
                    format!("{} could not be read", self.type_name),
                    error.clone(),
                )
                .child(
                    studio_ui::ds::state_action(
                        "retry-table",
                        "Refresh",
                        &RefreshTable,
                        &self.focus_handle,
                        cx,
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.refresh(window, cx))),
                )
                .into_any_element();
            }
            Loadable::Loading | Loadable::NotLoaded => {
                return studio_ui::ds::loading_state(format!("Reading {}…", self.type_name))
                    .into_any_element();
            }
            Loadable::Loaded(()) => {}
        }
        let empty = match &self.grouping {
            Some(grouping) => grouping.sections().is_empty(),
            None => self.rows.is_empty(),
        };
        if empty {
            // The branch is the status bar's to name. Nodes and edges both add rows at the
            // head; a past commit adds none.
            let state = if self.filters.is_empty() {
                studio_ui::ds::empty_state(IconName::Table, format!("No {} rows", self.type_name))
                    .when(self.at_commit.is_none(), |state| {
                        state.child(
                            studio_ui::ds::state_action(
                                "insert-first-row",
                                "Add Row",
                                &InsertRow,
                                &self.focus_handle,
                                cx,
                            )
                            .on_click(cx.listener(
                                |this, _, window, cx| this.on_insert(&InsertRow, window, cx),
                            )),
                        )
                    })
            } else {
                studio_ui::ds::empty_state(IconName::Filter, "No rows match").child(
                    studio_ui::ds::state_action(
                        "clear-filters-empty",
                        "Clear Filters",
                        &ClearFilters,
                        &self.focus_handle,
                        cx,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.clear_filters(cx))),
                )
            };
            return state.into_any_element();
        }
        let grid = self.render_grid(window, cx);
        // A column's edge dragged anywhere over the grid resizes it until released. The grid
        // sits in the document under the toolbar, from the document's edge, with no frame.
        div()
            .size_full()
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<ColumnDrag>, _, cx| {
                    this.drag_column(event, cx)
                }),
            )
            // Released, by a drop here or elsewhere, or a click that dragged nothing.
            .capture_any_mouse_up(cx.listener(|this, _, _, cx| this.end_resize(cx)))
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.end_resize(cx)),
            )
            .child(grid)
            .into_any_element()
    }

    fn render_grid(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let selected_bg = studio_ui::ds::surface::selected_document(cx);
        let hover_bg = studio_ui::ds::surface::hover(cx);
        let drop_bg = studio_ui::ds::surface::selected_document(cx);
        let menu_open = self.header_menu.is_some();
        // The header and every line draw the same places: the columns in view and gaps.
        let drawn = Rc::new(self.drawn_columns(window, cx));
        let column_widths: Vec<Pixels> = self
            .columns
            .iter()
            .map(|c| px(self.layout.width(c)))
            .collect();
        let places: Rc<[GridPlace]> = grid_places(&drawn, &column_widths).into();
        self.frame_places = places.clone();
        // The grid ends at its last column: a header and rows no wider than the columns, so no
        // fill or rule runs on past it when they are narrower than the document.
        let content_width = column_widths
            .iter()
            .fold(Pixels::ZERO, |sum, width| sum + *width);
        let last = self.columns.len().saturating_sub(1);
        // A rule on every header's right but the last column's: the grid's edge is its own.
        let ruled = move |header: Div, index: usize, cx: &App| {
            if index < last {
                studio_ui::ds::cell_rule(header, cx)
            } else {
                header
            }
        };
        let headers: Vec<AnyElement> = places
            .iter()
            .map(|place| {
                let index = match *place {
                    GridPlace::Column(index) => index,
                    GridPlace::Gap(_) => return div().into_any_element(),
                };
                let column = &self.columns[index];
                let sortable = column.property().filter(|p| Sort::allowed(p));
                let sorted = self
                    .sort
                    .as_ref()
                    .filter(|sort| sortable.is_some_and(|p| p.name == sort.property));
                let key = column.key.clone();
                let menu_key = column.key.clone();
                let resize = self.render_resize_handle(index, column, cx);
                let group = SharedString::from(format!("column-header-{index}"));
                let kind = Some(self.column_icon(column));
                // The key's header starts at the page's gutter, as its cells do.
                let header = ruled(
                    studio_ui::ds::header_cell(kind, column.title.clone(), sorted.is_some())
                        .when(index == 0, |header| header.pl(px(KEY_TEXT_INSET))),
                    index,
                    cx,
                )
                .id(("column-header", index))
                .group(group.clone())
                .relative()
                .w_full()
                .child(resize)
                .when(index == 0, |header| header.child(self.render_all_check(cx)))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                        this.open_header_menu(key.clone(), event.position, window, cx)
                    }),
                )
                .child(
                    div().min_w_0().flex_shrink(1000.).child(
                        studio_ui::ds::text::meta(match self.absent.get(&column.key) {
                            Some(then) => crate::peek::absence_text(then, false),
                            None => column.detail.clone(),
                        })
                        .color(Color::Disabled)
                        .single_line()
                        .truncate(),
                    ),
                )
                .children(sorted.map(|sort| {
                    studio_ui::ds::icon::meta(if sort.descending {
                        IconName::ArrowDown
                    } else {
                        IconName::ArrowUp
                    })
                    .color(Color::Accent)
                }))
                // A number's header sits at the cells' end, as its values do.
                .when(column.align_right, |header| header.justify_end())
                .child(studio_ui::ds::hover_slot(
                    group,
                    studio_ui::ds::header_fill_opaque(sortable.is_some(), cx),
                    studio_ui::ds::cell_action(("column-menu", index), IconName::Ellipsis)
                        .tooltip(Tooltip::text("Column menu"))
                        .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                            this.open_header_menu(menu_key.clone(), event.position(), window, cx)
                        })),
                ));
                // An unpinned header drags onto another to move its column there.
                let header = if index == 0 {
                    header
                } else {
                    let target = column.key.clone();
                    header
                        .on_drag(
                            DraggedColumn {
                                key: column.key.clone(),
                                title: column.title.clone().into(),
                            },
                            |dragged, _, _, cx| {
                                cx.stop_propagation();
                                cx.new(|_| dragged.clone())
                            },
                        )
                        .drag_over::<DraggedColumn>(move |style, _, _, _| style.bg(drop_bg))
                        .on_drop(cx.listener(move |this, dragged: &DraggedColumn, _, cx| {
                            let target = target.clone();
                            this.arrange_columns(cx, |layout, all| {
                                layout.move_onto(all, &dragged.key, &target)
                            })
                        }))
                };
                match sortable {
                    Some(property) => {
                        let property = property.name.clone();
                        let next = match sorted {
                            None => "Sort ascending",
                            Some(sort) if !sort.descending => "Sort descending",
                            Some(_) => "Back to key order",
                        };
                        header
                            .cursor_pointer()
                            .hover(move |style| style.bg(hover_bg))
                            // Not over the header's own menu, which opens under the pointer.
                            .when(!menu_open, |header| {
                                header.tooltip(Tooltip::text(format!(
                                    "{next} · rows without a value come last"
                                )))
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.cycle_sort(property.clone(), cx)
                            }))
                            .into_any_element()
                    }
                    None => header.into_any_element(),
                }
            })
            .collect();
        // Grouped, the grid draws lines (headers, rows, read-more lines); otherwise one line
        // per row. Per line: the row it shows, if any.
        // The lines are worked out once a frame, by section, and shared with the row callback,
        // which the list calls more than once a frame.
        let lines: Option<Rc<Lines>> = self
            .grouping
            .as_ref()
            .map(|grouping| Rc::new(grouping.line_map()));
        self.frame_lines = lines.clone();
        // How each drawn line looks (picked, dimmed, the cursor's, a group header) is worked
        // out by the row callback for the lines it draws, not for every loaded row.
        let looks: Rc<RefCell<HashMap<usize, RowLook>>> = Rc::default();
        self.frame_looks = looks.clone();
        let picked_bg = studio_ui::ds::surface::picked(cx);
        let header_bg = studio_ui::ds::surface::well(cx);
        let line_count = lines.as_ref().map_or(self.rows.len(), |lines| lines.len());
        // Widths fitted to each column's type unless dragged. Not Zed's resizable columns: they
        // draw a full-height rule between every column, which the visual language leaves out
        // (spec 002); the header's edges resize instead.
        let widths: Vec<Pixels> = places
            .iter()
            .map(|place| match *place {
                GridPlace::Column(col) => column_widths[col],
                GridPlace::Gap(width) => width,
            })
            .collect();
        let header = self.render_header_row(headers, &widths, content_width, cx);
        let table = Table::new(places.len())
            .interactable(&self.table_state)
            .width_config(ColumnWidthConfig::explicit(widths))
            .disable_base_style()
            .hide_row_borders()
            .pin_cols(1)
            .map_row(move |(ix, row), _window, cx| {
                let look = looks.borrow().get(&ix).copied().unwrap_or_default();
                studio_ui::ds::dotted(row.border_b_1(), cx)
                    .max_w(content_width)
                    .group(ROW_GROUP)
                    .when(look.header, |row| row.bg(header_bg))
                    .when(look.picked, |row| row.bg(picked_bg))
                    .when(look.cursor, |row| row.bg(selected_bg))
                    .when(look.dimmed, |row| {
                        row.opacity(studio_ui::ds::opacity::DIMMED)
                    })
                    .into_any_element()
            })
            .uniform_list(
                "type-table-rows",
                line_count,
                cx.processor(move |this, range: Range<usize>, window, cx| {
                    this.note_looks(range.clone());
                    this.want_relations(&drawn, window, cx);
                    this.note_blob_columns(&drawn);
                    // Their rows' relationship and Blob cells are asked for once the frame is
                    // out, for the rows these lines show, not every row between their first
                    // and last: grouped, a folded section's rows are on no line.
                    this.frame_line_range = Some(range.clone());
                    this.drawn(window, cx);
                    #[cfg(test)]
                    {
                        this.frame_drawn = drawn.to_vec();
                    }
                    let Some(lines) = this.frame_lines.clone() else {
                        if this.more_error.is_none()
                            && this
                                .paging
                                .wants_more(this.rows.len(), range.end, this.loading_more)
                        {
                            cx.defer_in(window, |this, _, cx| this.load_more(cx));
                        }
                        return range.map(|ix| this.render_row(ix, window, cx)).collect();
                    };
                    let shown: Vec<Line> = lines.slice(range).collect();
                    // An open section reads its first rows when its read line comes into view,
                    // and a read dropped while its line was away reads again as it returns.
                    for line in &shown {
                        if let Line::More(index) = *line
                            && let Some(rows) = this.section_read_owed(index)
                        {
                            cx.defer_in(window, move |this, _, cx| {
                                this.load_section(index, rows, cx)
                            });
                        }
                    }
                    shown
                        .iter()
                        .map(|line| match *line {
                            Line::Row(row) => this.render_row(row, window, cx),
                            Line::Header(index) => this.render_section_header(index, cx),
                            Line::More(index) => this.render_section_more(index, cx),
                            Line::OtherValues => this.render_other_values(),
                        })
                        .collect()
                }),
            );
        let state = self.table_state.read(cx);
        let wheel = sideways_scroll(
            state.horizontal_scroll_handle.clone(),
            self.wheel_gesture.clone(),
            self.edit_slot.clone(),
        );
        v_flex()
            .size_full()
            .child(header)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    // The rows' fills and rules under their text: one layer of the scene, so a
                    // frame orders them without a search per cell (the edit callout floats,
                    // deferred, over them).
                    .child(table)
                    .child(wheel),
            )
            .into_any_element()
    }

    /// The grid's header row, drawn here rather than by Zed's table, which pads its header row
    /// and cells beyond [`size::HEADER`]: the header's fill under a hairline, the pinned key
    /// column's header, then the others in a strip that scrolls sideways with the rows (the
    /// table's horizontal scroll handle), each place as wide as its cells. It is no wider than
    /// the columns (`content_width`), as the rows are, so it ends where they do.
    fn render_header_row(
        &self,
        headers: Vec<AnyElement>,
        widths: &[Pixels],
        content_width: Pixels,
        cx: &App,
    ) -> AnyElement {
        let handle = self.table_state.read(cx).horizontal_scroll_handle.clone();
        let mut places = headers.into_iter().zip(widths).map(|(header, width)| {
            div()
                .w(*width)
                .h_full()
                .flex_none()
                .overflow_hidden()
                .child(header)
        });
        let pinned: Vec<Div> = places.by_ref().take(1).collect();
        let scrolled: Vec<Div> = places.collect();
        h_flex()
            .w_full()
            .max_w(content_width)
            .h(px(studio_ui::HEADER_HEIGHT))
            .flex_none()
            .bg(studio_ui::ds::header_fill(cx))
            .border_b_1()
            .border_color(studio_ui::ds::surface::rule(cx))
            .child(h_flex().h_full().flex_shrink_0().children(pinned))
            .when(!scrolled.is_empty(), |row| {
                row.child(
                    div()
                        .id("table-header-scrollable")
                        .h_full()
                        .flex_grow(1.)
                        // A flex container, as Zed's row strips are: its child keeps its
                        // columns' width, so the strip measures what it can scroll. A block
                        // child stretched to the strip measured nothing, and its clamp put the
                        // shared offset, every row's, back to 0 on each draw.
                        .flex()
                        .overflow_x_scroll()
                        .restrict_scroll_to_axis()
                        .track_scroll(&handle)
                        .child(h_flex().h_full().children(scrolled)),
                )
            })
            .into_any_element()
    }

    /// What a column's header shows it holds: its property's kind, the key property's (text
    /// for an `@id`), or for an edge's end and a relationship the arrow the sidebar draws them
    /// with.
    fn column_icon(&self, column: &ColumnSpec) -> IconName {
        match &column.role {
            ColumnRole::Property(property) => property_icon(property),
            ColumnRole::Key => self
                .schema
                .as_ref()
                .and_then(|schema| schema.node(&self.type_name))
                .and_then(|node| node.property(&column.key))
                .map_or(IconName::CaseSensitive, property_icon),
            ColumnRole::Endpoint { .. } | ColumnRole::Relationship(_) => IconName::ArrowRight,
        }
    }

    /// Peeks at row `ix`, as Space does with the cursor on it: the cursor moves to its key, and
    /// a second press closes the peek.
    fn peek_at(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.cursor = Some(Cursor { row: ix, col: 0 });
        self.on_peek(&PeekRow, window, cx);
    }

    /// A grouped grid's header line: the value, how many rows hold it, and the disclosure.
    fn render_section_header(&self, index: usize, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(grouping) = &self.grouping else {
            return Vec::new();
        };
        let Some(section) = grouping.sections().get(index) else {
            return Vec::new();
        };
        let property = self.group_property(&grouping.property);
        let missing = section.value.is_null();
        // By a relationship: the neighbour's label (its id in the tooltip), or "No Company".
        let other_type = self
            .all_columns
            .iter()
            .find(|c| c.key == grouping.property)
            .and_then(ColumnSpec::relationship)
            .map(|group| group.other_type.clone());
        let text = match (&other_type, missing) {
            (Some(other), true) => format!("No {other}"),
            // By a neighbour's or an end's property, the property is not in the row: "No city".
            (None, true) if !grouping.by_own_property() => property
                .as_ref()
                .map_or_else(|| "No value".to_owned(), |p| format!("No {}", p.name)),
            (None, true) => "No value".to_owned(),
            (Some(_), false) => section
                .label
                .clone()
                .unwrap_or_else(|| display_value(None, &section.value)),
            (None, false) => display_value(property.as_ref(), &section.value),
        };
        let id = (other_type.is_some() && !missing && section.label.is_some())
            .then(|| display_value(None, &section.value));
        // Two neighbours with one label are told apart by their ids.
        let repeated = id.is_some()
            && grouping
                .sections()
                .iter()
                .filter(|other| other.label == section.label)
                .count()
                > 1;
        let first = h_flex()
            .id(("section-header", index))
            .h(px(ROW_HEIGHT))
            .w_full()
            .pl(px(KEY_INSET))
            .pr(px(space::LG))
            .gap(px(space::SM))
            .items_center()
            .overflow_hidden()
            .cursor_pointer()
            .child(studio_ui::ds::icon::meta(if section.expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            }))
            .child(
                div().min_w_0().flex_shrink(1.).child(
                    studio_ui::ds::text::body(text)
                        .weight(studio_ui::ds::text::weight::HEADING)
                        .color(if missing {
                            Color::Muted
                        } else {
                            Color::Default
                        })
                        .when(missing, |label| label.italic())
                        .truncate(),
                ),
            )
            // The id gives way first when the cell is narrow, keeping its end, where ids of
            // one type differ.
            .when_some(id.clone().filter(|_| repeated), |header, id| {
                header.child(
                    div()
                        .min_w_0()
                        .flex_shrink(1000.)
                        .child(studio_ui::ds::text::meta(id).truncate_start()),
                )
            })
            .child(
                studio_ui::ds::text::meta(studio_ui::group_count(section.count as i64))
                    .single_line(),
            )
            .when_some(id, |header, id| header.tooltip(Tooltip::text(id)))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_section(index, cx)))
            .into_any_element();
        self.line_cells(first)
    }

    /// The line under an open section's rows: Show more, Loading…, or what failed and Retry.
    fn render_section_more(&self, index: usize, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(section) = self.grouping.as_ref().and_then(|g| g.sections().get(index)) else {
            return Vec::new();
        };
        let page = self.page_size(cx);
        let content: AnyElement = if section.loading {
            studio_ui::ds::text::meta("Reading…").into_any_element()
        } else if let Some(error) = &section.error {
            h_flex()
                .gap(px(space::XS))
                .child(studio_ui::ds::icon::meta(IconName::Warning).color(Color::Error))
                .child(
                    div()
                        .id(("section-error", index))
                        .tooltip(Tooltip::text(error.clone()))
                        .child(studio_ui::ds::text::error_note("Not read")),
                )
                .child(
                    studio_ui::ds::row_button(("section-retry", index), "Retry")
                        .style(ButtonStyle::Subtle)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(section) =
                                this.grouping.as_mut().and_then(|g| g.section_mut(index))
                            {
                                section.error = None;
                            }
                            let size = this.page_size(cx);
                            this.load_section(index, size, cx);
                        })),
                )
                .into_any_element()
        } else {
            let remaining = section.count.saturating_sub(section.range.len() as u64);
            let next = remaining.min(u64::from(page));
            // "Show 50 more of 120", or "Show 25 more" when the page holds the rest.
            let label = if next < remaining {
                format!(
                    "Show {next} more of {}",
                    studio_ui::group_count(remaining as i64)
                )
            } else {
                format!("Show {} more", studio_ui::group_count(remaining as i64))
            };
            studio_ui::ds::row_button(("section-more", index), label)
                .style(ButtonStyle::Subtle)
                .color(Color::Accent)
                .on_click(cx.listener(move |this, _, _, cx| {
                    let size = this.page_size(cx);
                    this.load_section(index, size, cx);
                }))
                .into_any_element()
        };
        self.line_cells(
            h_flex()
                .h(px(ROW_HEIGHT))
                .w_full()
                .pl(px(KEY_INSET))
                .pr(px(space::LG))
                .items_center()
                .child(content)
                .into_any_element(),
        )
    }

    fn render_other_values(&self) -> Vec<AnyElement> {
        self.line_cells(
            h_flex()
                .h(px(ROW_HEIGHT))
                .w_full()
                .pl(px(KEY_INSET))
                .pr(px(space::LG))
                .items_center()
                .child(
                    studio_ui::ds::text::meta(format!(
                        "The first {GROUP_LIMIT} values are shown; filter to see the others"
                    ))
                    .single_line(),
                )
                .into_any_element(),
        )
    }

    /// A line's cells: its content in the first (pinned) column, the other places empty.
    fn line_cells(&self, first: AnyElement) -> Vec<AnyElement> {
        std::iter::once(first)
            .chain((1..self.frame_places.len()).map(|_| div().h(px(ROW_HEIGHT)).into_any_element()))
            .collect()
    }

    /// Works out how the lines `range` look, for the row styling of the lines being drawn.
    fn note_looks(&self, range: Range<usize>) {
        let lines = self.frame_lines.as_deref();
        let cursor = self.cursor.map(|c| c.row);
        let mut looks = self.frame_looks.borrow_mut();
        for ix in range {
            let row = match lines {
                Some(lines) => match lines.get(ix) {
                    Some(Line::Row(row)) => Some(row),
                    Some(Line::Header(_)) => {
                        looks.insert(
                            ix,
                            RowLook {
                                header: true,
                                ..RowLook::default()
                            },
                        );
                        continue;
                    }
                    _ => None,
                },
                None => Some(ix),
            };
            let Some((row, data)) = row.and_then(|row| Some((row, self.rows.get(row)?))) else {
                continue;
            };
            looks.insert(
                ix,
                RowLook {
                    picked: self.selection.contains(&data.id),
                    dimmed: data.dimmed,
                    cursor: cursor == Some(row),
                    header: false,
                },
            );
        }
    }

    /// Asks for the Blob cells of the rows lines `lines` show, in the Blob columns drawn, once
    /// scrolling has paused: a fast scroll reads nothing for the rows it passes, and reads
    /// still out for cells no longer drawn, scrolled away up, down or sideways, are dropped.
    fn want_blob_metas(&mut self, lines: Range<usize>, cx: &mut Context<Self>) {
        if self.blob_lines.as_ref() == Some(&lines) || self.blob_columns.is_empty() {
            return;
        }
        self.blob_lines = Some(lines.clone());
        self._blob_wait = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(BLOB_READ_AFTER).await;
            this.update(cx, |this, cx| {
                let rows = this.rows_of_lines(lines);
                let drawn: HashSet<&str> = rows
                    .iter()
                    .filter_map(|&ix| this.rows.get(ix))
                    .map(|row| row.id.as_str())
                    .collect();
                // A read for a cell scrolled away is dropped and asked again when it returns.
                let gone: Vec<(String, String)> = this
                    .blob_tasks
                    .keys()
                    .filter(|(id, property)| {
                        !drawn.contains(id.as_str()) || !this.blob_columns.contains(property)
                    })
                    .cloned()
                    .collect();
                let columns = &this.blob_columns;
                let waiting_gone: Vec<(String, String)> = this
                    .blob_queue
                    .iter()
                    .filter(|(id, property)| {
                        !drawn.contains(id.as_str()) || !columns.contains(property)
                    })
                    .cloned()
                    .collect();
                for key in gone {
                    this.blob_tasks.remove(&key);
                    this.blobs.remove(&key.0, &key.1);
                }
                // One pass over the queue, however many leave it.
                let leaving: HashSet<&(String, String)> = waiting_gone.iter().collect();
                this.blob_queue.retain(|waiting| !leaving.contains(waiting));
                for key in &waiting_gone {
                    if matches!(this.blobs.get(&key.0, &key.1), Some(Loadable::Loading)) {
                        this.blobs.remove(&key.0, &key.1);
                    }
                }
                this.load_blob_metas(rows, cx);
            })
            .ok();
        }));
    }

    /// Notes the Blob columns this frame draws; one come into view, or gone out of it, has the
    /// rows drawn asked for again.
    fn note_blob_columns(&mut self, drawn: &[bool]) {
        let columns = self
            .columns
            .iter()
            .zip(drawn)
            .filter(|(_, drawn)| **drawn)
            .filter_map(|(column, _)| column.property())
            .filter(|property| property.kind == ScalarKind::Blob)
            .map(|property| property.name.as_str());
        if !columns
            .clone()
            .eq(self.blob_columns.iter().map(String::as_str))
        {
            self.blob_columns = columns.map(str::to_owned).collect();
            self.blob_lines = None;
        }
    }

    /// Drops the section reads whose read line is more than a screen from the lines drawn
    /// (`lines`): reads share a connection's few slots, so one scrolled past would keep those of
    /// the sections in view waiting. Its section reads as many rows again when its line comes
    /// back ([`Self::dropped_sections`]), a "Show more" as a first read does.
    fn drop_section_reads_away_from(&mut self, lines: Range<usize>) {
        if self.section_tasks.is_empty() {
            return;
        }
        let Some(grouping) = self.grouping.as_mut() else {
            return;
        };
        let screen = lines.len().max(1);
        let near = lines.start.saturating_sub(screen)..lines.end + screen;
        let kept: HashSet<usize> = grouping
            .line_map()
            .slice(near)
            .filter_map(|line| match line {
                Line::More(index) => Some(index),
                _ => None,
            })
            .collect();
        let away: Vec<usize> = self
            .section_tasks
            .keys()
            .filter(|index| !kept.contains(index))
            .copied()
            .collect();
        for index in away {
            if let Some(read) = self.section_tasks.remove(&index) {
                self.dropped_sections.insert(index, read.rows);
            }
            if let Some(section) = grouping.section_mut(index) {
                section.loading = false;
            }
        }
    }

    /// How many rows section `index`'s read line asks for as it is drawn: a read dropped away
    /// from the view asks for as many as it did, and an open section with no rows read its
    /// first few. `None` while a read is out, after one failed (Retry asks), and for a section
    /// whose next rows are asked for by "Show more".
    fn section_read_owed(&self, index: usize) -> Option<u32> {
        let section = self.grouping.as_ref()?.sections().get(index)?;
        if section.loading || section.error.is_some() {
            return None;
        }
        match self.dropped_sections.get(&index) {
            Some(&rows) => Some(rows),
            None if section.range.is_empty() => Some(SECTION_FIRST_PAGE),
            None => None,
        }
    }

    /// The Blob values the row `row_id` shows, each cell read so far, and where they hold, for a
    /// peek opened from the row to show and bind to the same.
    fn shown_blobs(&self, row_id: &str) -> TableBlobs {
        let shown = self
            .columns
            .iter()
            .filter_map(|c| c.property())
            .filter(|p| p.kind == ScalarKind::Blob)
            .filter_map(|p| {
                let meta = self.blobs.get(row_id, &p.name)?.loaded()?;
                Some((p.name.clone(), meta.clone()))
            })
            .collect();
        TableBlobs {
            at: self.paging.blobs.clone(),
            shown,
        }
    }

    /// Reads what the Blob cells of the rows at `rows` hold in the Blob columns drawn, once
    /// each, at the page's commit. Only drawn cells are read: a Blob cannot be projected by the
    /// page query (spec 004), so each is a request of its own.
    fn load_blob_metas(&mut self, rows: impl IntoIterator<Item = usize>, cx: &mut Context<Self>) {
        let properties = self.blob_columns.clone();
        if properties.is_empty() {
            return;
        }
        let rows: Vec<String> = rows
            .into_iter()
            .filter_map(|ix| self.rows.get(ix))
            .map(|row| row.id.clone())
            .collect();
        for id in rows {
            for property in &properties {
                if self.blobs.contains(&id, property) {
                    continue;
                }
                self.blobs
                    .insert(id.clone(), property.clone(), Loadable::Loading);
                self.blob_queue.push_back((id.clone(), property.clone()));
            }
        }
        self.read_queued_blobs(cx);
    }

    /// Reads the queued Blob cells, the first drawn first, while fewer than
    /// [`BLOB_READS_AT_ONCE`] are out; each read that ends starts the next. A cell answered
    /// meanwhile (a write landed its value) or let go (a reload, a scroll away) is passed over.
    fn read_queued_blobs(&mut self, cx: &mut Context<Self>) {
        let edge = self.kind == TableKind::Edge;
        let snapshot = self.paging.pinned.clone();
        while self.blob_tasks.len() < BLOB_READS_AT_ONCE {
            let Some(key) = self.blob_queue.pop_front() else {
                break;
            };
            let waiting = matches!(self.blobs.get(&key.0, &key.1), Some(Loadable::Loading));
            if !waiting || self.blob_tasks.contains_key(&key) {
                continue;
            }
            let cell = BlobCell {
                edge,
                type_name: self.type_name.clone(),
                id: key.0.clone(),
                property: key.1.clone(),
            };
            let scope = self.scope.clone();
            let read = self.session.update(cx, |session, cx| match &scope {
                Some(scope) => session.blob_meta_in(scope, cell, snapshot.clone(), cx),
                None => session.blob_meta(cell, snapshot.clone(), cx),
            });
            let task_key = key.clone();
            let task = cx.spawn(async move |this, cx| {
                let result = read.await;
                this.update(cx, |this, cx| {
                    // A reload since this read began leaves its answer out.
                    if this.blob_tasks.remove(&task_key).is_some() {
                        let (id, property) = task_key;
                        this.blobs.insert(
                            id,
                            property,
                            match result {
                                Ok(meta) => Loadable::Loaded(meta),
                                Err(error) => Loadable::Failed(error.to_string()),
                            },
                        );
                        cx.notify();
                    }
                    this.read_queued_blobs(cx);
                })
                .ok();
            });
            self.blob_tasks.insert(key, task);
        }
    }

    /// The Blob cell of row `row_id`'s `property`.
    fn blob_cell(&self, row_id: &str, property: &str) -> BlobCell {
        BlobCell {
            edge: self.kind == TableKind::Edge,
            type_name: self.type_name.clone(),
            id: row_id.to_owned(),
            property: property.to_owned(),
        }
    }

    /// Whether a file dropped on row `row_id`'s Blob `property` is stored there: a node table at
    /// the head of a branch the credential may change, with no write of the cell out.
    fn takes_blob_drop(&self, row_id: &str, property: &str, cx: &App) -> bool {
        // Asked for each Blob cell drawn, so it checks cheaply what `blob_write::write_refusal`
        // checks: the column is a Blob property of the schema already. A write that refusal
        // would stop says why when it is sent.
        self.kind == TableKind::Node
            && self.at_commit.is_none()
            && self.change_refusal(cx).is_none()
            && (self.blob_edits.is_empty()
                || !self
                    .blob_edits
                    .get(&(row_id.to_owned(), property.to_owned()))
                    .is_some_and(BlobEdit::is_saving))
    }

    /// A drop from the file manager on a Blob cell: its one file stored there.
    fn drop_blob_files(
        &mut self,
        row_id: &str,
        property: &str,
        paths: &[std::path::PathBuf],
        cx: &mut Context<Self>,
    ) {
        match blob_write::dropped_file(paths) {
            Ok(path) => self.upload_blob(row_id, property, path, cx),
            Err(notice) => {
                let key = (row_id.to_owned(), property.to_owned());
                self.blob_edits.insert(
                    key,
                    BlobEdit::Failed {
                        notice,
                        again: None,
                    },
                );
                cx.notify();
            }
        }
    }

    /// The Blob cell under the cursor, as (row id, property), in a node table: what Choose File
    /// and Clear act on.
    fn blob_under_cursor(&self) -> Option<(String, String)> {
        let cursor = self.cursor?;
        let property = self.columns.get(cursor.col)?.property()?;
        (self.kind == TableKind::Node && property.kind == ScalarKind::Blob)
            .then(|| self.rows.get(cursor.row))
            .flatten()
            .map(|row| (row.id.clone(), property.name.clone()))
    }

    /// Choose File…: the system's picker, then the file chosen stored in the Blob cell under the
    /// cursor.
    fn on_choose_blob_file(&mut self, _: &ChooseFile, _: &mut Window, cx: &mut Context<Self>) {
        let Some((row_id, property)) = self.blob_under_cursor() else {
            return;
        };
        let chosen = blob_write::choose_file(cx);
        cx.spawn(async move |this, cx| {
            if let Some(path) = chosen.await {
                this.update(cx, |this, cx| {
                    this.upload_blob(&row_id, &property, path, cx)
                })
                .ok();
            }
        })
        .detach();
    }

    /// Clear: the Blob cell under the cursor set to null, bound to the value it shows.
    fn on_clear_blob(&mut self, _: &ClearValue, _: &mut Window, cx: &mut Context<Self>) {
        if let Some((row_id, property)) = self.blob_under_cursor() {
            self.write_blob(&row_id, &property, BlobChange::Clear, cx);
        }
    }

    /// Stores the file at `path` in row `row_id`'s Blob `property` on the table's branch.
    pub(crate) fn upload_blob(
        &mut self,
        row_id: &str,
        property: &str,
        path: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) {
        self.write_blob(row_id, property, BlobChange::Upload(path), cx);
    }

    /// Sends `change` to row `row_id`'s Blob `property` on the table's branch, bound to the value
    /// the cell shows at the table's commit. After a conflict, a change made again is bound to
    /// what the cell held at the head then, which the cell's notice named. The cell shows a dot
    /// while it is out; its commit moves the table there as a cell edit's does.
    fn write_blob(
        &mut self,
        row_id: &str,
        property: &str,
        change: BlobChange,
        cx: &mut Context<Self>,
    ) {
        let key = (row_id.to_owned(), property.to_owned());
        if self.blob_edits.get(&key).is_some_and(BlobEdit::is_saving) {
            return;
        }
        let cell = self.blob_cell(row_id, property);
        let refused = |reason: String| BlobEdit::Failed {
            notice: Notice::new(Tone::Invalid, reason),
            again: None,
        };
        let over = match self.blob_edits.get(&key) {
            Some(BlobEdit::Failed {
                again: Some((_, now)),
                ..
            }) => Some(now.clone()),
            _ => None,
        };
        let (schema, pinned) = (self.schema.as_deref(), self.at_commit.as_deref());
        let denied = self.change_refusal(cx);
        let refusal = match &change {
            BlobChange::Clear if over.is_none() => blob_write::clear_refusal(
                schema,
                &cell,
                pinned,
                denied.as_deref(),
                self.blobs.get(&key.0, &key.1).and_then(Loadable::loaded),
            ),
            _ => blob_write::write_refusal(schema, &cell, pinned, denied.as_deref()),
        };
        if let Some(reason) = refusal {
            self.blob_edits.insert(key, refused(reason));
            cx.notify();
            return;
        }
        let shown = over.clone().or_else(|| {
            let shown = self.blobs.get(&key.0, &key.1);
            shown.and_then(Loadable::loaded).cloned()
        });
        let Some(shown) = shown else {
            let reason =
                format!("Nothing was written: Studio has not read what {property} holds yet.");
            self.blob_edits.insert(key, refused(reason));
            cx.notify();
            return;
        };
        // Bound where the values the cells show hold: the pin, or past it the table's own
        // commit that holds them; where none is known (its own write made over another
        // writer's), to the validator the cell shows. A change elsewhere since is found either
        // way. After a conflict, the change made again is bound to what the head held then.
        let read_at = match &over {
            Some(_) => None,
            None => self.paging.blobs_at().map(str::to_owned),
        };
        // Where the write was bound, once it is: an upload's binding is worked out as it is
        // sent, after the file is read.
        let bound_at = Rc::new(RefCell::new(read_at.clone()));
        let (session, branch) = (self.session.clone(), self.branch.clone());
        let write = match &change {
            BlobChange::Upload(path) => {
                // One of the table's own writes landing while the file is read moves where the
                // values it shows hold, and every validator of the type with them: the write is
                // bound to what the cell shows as it goes. After a conflict it stays bound to
                // what the head held then; with the table gone or on another branch, to what
                // the cell showed when the file was chosen.
                let table = cx.entity().downgrade();
                let (key, chosen) = (key.clone(), bound_at.clone());
                let (session, branch, again) = (session.clone(), branch.clone(), over.is_some());
                let chose = Binding { shown, read_at };
                let binding = move |cx: &mut App| {
                    let now = table
                        .upgrade()
                        .filter(|_| !again)
                        .and_then(|table| table.read(cx).blob_binding(&key, &session, &branch));
                    let binding = now.unwrap_or(chose);
                    *chosen.borrow_mut() = binding.read_at.clone();
                    binding
                };
                blob_write::upload_bound(
                    &self.session,
                    &self.branch,
                    cell,
                    binding,
                    path.clone(),
                    cx,
                )
            }
            BlobChange::Clear => {
                blob_write::clear(&self.session, &self.branch, cell, shown, read_at, cx)
            }
        };
        self.blob_edits
            .insert(key.clone(), BlobEdit::Saving(change.clone()));
        self.saves_in_flight += 1;
        // Detached: a write is never cancelled by closing the tab.
        cx.spawn(async move |this, cx| {
            let written = write.await;
            this.update(cx, |this, cx| {
                let sent = BlobSent {
                    key,
                    change,
                    bound_at: bound_at.borrow().clone(),
                };
                this.blob_written(sent, &session, &branch, written, cx)
            })
            .ok();
        })
        .detach();
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// What a Blob write to cell `key`, sent on `session`'s `branch`, is bound to now: the value
    /// the cell shows, read where the values the cells show hold ([`Paging::blobs_at`]). `None`
    /// once the table shows another branch, or no longer holds what the cell held.
    fn blob_binding(
        &self,
        key: &(String, String),
        session: &Entity<GraphSession>,
        branch: &str,
    ) -> Option<Binding> {
        if !self.shows(session, branch) {
            return None;
        }
        let shown = self.blobs.get(&key.0, &key.1)?.loaded()?.clone();
        Some(Binding {
            shown,
            read_at: self.paging.blobs_at().map(str::to_owned),
        })
    }

    /// Lands a Blob write sent on `session`'s `branch`. A commit on the one the table shows
    /// moves it there, and the cell shows the new value; over another writer's, the table is
    /// behind and offers Refresh, and a write bound to a validator read where the values shown
    /// hold leaves them holding at its commit, which the next write is bound at. One that did
    /// not land says why on its cell.
    fn blob_written(
        &mut self,
        sent: BlobSent,
        session: &Entity<GraphSession>,
        branch: &str,
        written: BlobWritten,
        cx: &mut Context<Self>,
    ) {
        let BlobSent {
            key,
            change,
            bound_at,
        } = sent;
        self.saves_in_flight = self.saves_in_flight.saturating_sub(1);
        // The table moved to another branch or graph meanwhile; the outcome is told by its
        // toast.
        if !self.shows(session, branch) {
            self.blob_edits.remove(&key);
            self.saves_settled(cx);
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
            return;
        }
        if let BlobWritten::Saved {
            commit,
            parent,
            meta,
            bound,
        } = &written
        {
            if let Some(commit) = commit {
                let before = self.paging.blobs_at().map(str::to_owned);
                if self.follow_own_commit(parent.as_deref(), commit) {
                    self.newer = true;
                    if let Some(at) = bound_at.as_deref().filter(|_| *bound) {
                        self.paging
                            .blobs
                            .after_bound_write(at, before.as_deref(), commit);
                    }
                }
                let blob = Some((key.0.clone(), key.1.clone(), meta.clone()));
                self.tell_peek_wrote(Vec::new(), blob, commit, parent.as_deref(), cx);
            }
            // A read of the cell still out would land the value replaced; the next waiting
            // takes its place.
            self.blob_tasks.remove(&key);
            self.blobs
                .insert(key.0.clone(), key.1.clone(), Loadable::Loaded(meta.clone()));
            self.read_queued_blobs(cx);
        }
        match BlobEdit::ended(written, change, &key.1) {
            Some(edit) => self.blob_edits.insert(key, edit),
            None => self.blob_edits.remove(&key),
        };
        self.saves_settled(cx);
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// Column `col`'s relationship cell of `row`: the first neighbours as chips, then "+N"; "—"
    /// for none. Drawn for every row in view at every draw, so its id is the cell's place (`id`,
    /// as [`Self::render_cell`] names the cell) and its tooltip is made only when it shows.
    fn render_relation_cell(
        &self,
        id: u64,
        col: usize,
        row: &Row,
        available: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = self.columns[col].key.as_str();
        let Some(relation) = self.relations.get(&row.id, key) else {
            return studio_ui::cell_text("…", cx)
                .color(Color::Disabled.color(cx))
                .into_any_element();
        };
        let count = match &relation.count {
            Ok(count) => *count,
            Err(_) => {
                let (table, row_id, key) = (cx.weak_entity(), row.id.clone(), key.to_owned());
                return div()
                    .id(ElementId::NamedInteger("relation".into(), id))
                    .tooltip(move |_, cx| {
                        let error = table.upgrade().and_then(|table| {
                            let table = table.read(cx);
                            match &table.relations.get(&row_id, &key)?.count {
                                Err(error) => Some(error.clone()),
                                Ok(_) => None,
                            }
                        });
                        Tooltip::simple(error.unwrap_or_default(), cx)
                    })
                    .child(studio_ui::ds::text::body("?").color(Color::Warning))
                    .into_any_element();
            }
        };
        if count == 0 {
            return studio_ui::cell_text("—", cx)
                .color(Color::Disabled.color(cx))
                .into_any_element();
        }
        let labels: Vec<&str> = relation
            .neighbours
            .iter()
            .take(RELATION_CHIPS_SHOWN)
            .map(|(id, label)| label.as_deref().unwrap_or(id))
            .collect();
        let fit = chips_that_fit(&labels, count, available);
        let more = count - fit as i64;
        // The other type is the column's, looked up when the tooltip shows.
        let table = cx.weak_entity();
        // The count stays in view; the tags give way, whole.
        h_flex()
            .id(ElementId::NamedInteger("relation".into(), id))
            .w_full()
            .min_w_0()
            .gap(px(space::XS))
            .tooltip(move |_, cx| {
                let other = table.upgrade().and_then(|table| {
                    let group = table.read(cx).columns.get(col)?.relationship()?;
                    Some(group.other_type.clone())
                });
                Tooltip::simple(
                    format!(
                        "{} {} · click to open them",
                        studio_ui::group_count(count),
                        other.unwrap_or_default()
                    ),
                    cx,
                )
            })
            .child(
                h_flex()
                    .min_w_0()
                    .gap(px(space::XS))
                    .overflow_hidden()
                    .children(
                        labels[..fit]
                            .iter()
                            .map(|label| studio_ui::ds::tag(label.to_string(), cx)),
                    ),
            )
            .when(more > 0, |cell| {
                cell.child(
                    div().flex_none().child(
                        studio_ui::cell_text(
                            if fit == 0 {
                                studio_ui::group_count(count)
                            } else {
                                format!("+{}", studio_ui::group_count(more))
                            },
                            cx,
                        )
                        .color(Color::Muted.color(cx))
                        .hug(),
                    ),
                )
            })
            .into_any_element()
    }

    /// A Blob cell: "Blob" while it is read, then its size and "Preview" (a click opens the
    /// peek, which shows it), "—" for null, or the host of an external reference. As a
    /// relationship cell, its id is the cell's place (`id`) and its tooltip is made only when
    /// it shows.
    fn render_blob_cell(
        &self,
        id: u64,
        row: &Row,
        property: &Property,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = self.blobs.get(&row.id, &property.name);
        let (text, color, preview, tooltip) = match state {
            Some(Loadable::Loaded(meta)) => (
                crate::blob::cell_label(meta),
                if matches!(meta, BlobMeta::Null) {
                    Color::Disabled
                } else {
                    Color::Default
                },
                matches!(meta, BlobMeta::Managed { size, .. } if *size > 0),
                matches!(meta, BlobMeta::External { .. }),
            ),
            Some(Loadable::Failed(_)) => ("Blob · ?".into(), Color::Warning, false, true),
            _ => ("Blob".into(), Color::Disabled, false, false),
        };
        // What the cell holds when the tooltip shows: a reference's address, or why it could
        // not be read.
        let tooltip = tooltip.then(|| {
            let (table, row_id, name) = (cx.weak_entity(), row.id.clone(), property.name.clone());
            move |_: &mut Window, cx: &mut App| {
                let text = table.upgrade().and_then(|table| {
                    match table.read(cx).blobs.get(&row_id, &name)? {
                        Loadable::Loaded(BlobMeta::External { uri }) => {
                            Some(format!("{uri}\nStudio does not fetch external objects."))
                        }
                        Loadable::Failed(error) => Some(error.clone()),
                        _ => None,
                    }
                });
                Tooltip::simple(text.unwrap_or_default(), cx)
            }
        });
        h_flex()
            .id(ElementId::NamedInteger("blob".into(), id))
            .min_w_0()
            .gap(px(space::XS))
            .when_some(tooltip, |cell, tooltip| cell.tooltip(tooltip))
            .child(
                studio_ui::ds::text::body(text)
                    .color(color)
                    .single_line()
                    .truncate(),
            )
            .when(preview, |cell| {
                cell.child(
                    studio_ui::ds::text::body("· Preview")
                        .color(Color::Accent)
                        .single_line(),
                )
            })
            .into_any_element()
    }

    /// A row's cells at this frame's places; a gap is one empty cell.
    fn render_row(
        &self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(row) = self.rows.get(ix) else {
            return Vec::new();
        };
        self.frame_places
            .clone()
            .iter()
            .map(|place| match *place {
                // The key cell carries the row's checkbox, over its start.
                GridPlace::Column(0) => div()
                    .relative()
                    .w_full()
                    .child(self.render_cell(ix, 0, row, window, cx))
                    .child(self.render_row_check(ix, &row.id, cx))
                    .into_any_element(),
                GridPlace::Column(col) => self.render_cell(ix, col, row, window, cx),
                GridPlace::Gap(_) => div().into_any_element(),
            })
            .collect()
    }

    /// A row's checkbox, on the page's gutter in its key cell: shown under the pointer, and on
    /// every row while any is checked, so a list nobody is picking from stays plain. A click
    /// checks the row or takes it out, ⇧ checks the rows from the last one checked; it moves no
    /// cursor and opens nothing.
    fn render_row_check(&self, ix: usize, id: &str, cx: &mut Context<Self>) -> AnyElement {
        let picked = self.selection.contains(id);
        let shown = !self.selection.is_empty();
        studio_ui::ds::check_box(("row-check", ix), ToggleState::from(picked), cx)
            .debug_selector(move || format!("row-check-{ix}"))
            .absolute()
            .left(px(KEY_INSET))
            .top(px((ROW_HEIGHT - studio_ui::ds::size::CHECK) / 2.))
            .when(!shown, |check| {
                check
                    .invisible()
                    .group_hover(ROW_GROUP, |style| style.visible())
            })
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                // The keyboard stays where it was, and the cell under the box hears nothing.
                window.prevent_default();
                cx.stop_propagation();
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                cx.stop_propagation();
                this.pick_rows(ix, event.modifiers().shift, cx);
            }))
            .into_any_element()
    }

    /// The key column's select-all: checks every loaded row, or, when every one is, takes them
    /// all out; a dash while some are checked.
    fn render_all_check(&self, cx: &mut Context<Self>) -> AnyElement {
        let state = self.all_check_state();
        let tooltip = match state {
            ToggleState::Selected => "Clear the selection (Esc)",
            _ => "Select every loaded row",
        };
        studio_ui::ds::check_box("all-rows-check", state, cx)
            .debug_selector(|| "all-rows-check".into())
            .absolute()
            .left(px(KEY_INSET))
            .top(px(
                (studio_ui::HEADER_HEIGHT - studio_ui::ds::size::CHECK) / 2.
            ))
            .tooltip(Tooltip::text(tooltip))
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                this.toggle_all_rows(cx);
            }))
            .into_any_element()
    }

    /// The select-all's state: none checked, every loaded row checked, or some.
    fn all_check_state(&self) -> ToggleState {
        if self.selection.is_empty() {
            ToggleState::Unselected
        } else if self.rows.iter().all(|row| self.selection.contains(&row.id)) {
            ToggleState::Selected
        } else {
            ToggleState::Indeterminate
        }
    }

    /// Checks every loaded row, or takes them all out when every one is checked.
    fn toggle_all_rows(&mut self, cx: &mut Context<Self>) {
        if self.all_check_state() == ToggleState::Selected {
            self.clear_selection(cx);
            return;
        }
        self.selection
            .extend(self.rows.iter().map(|row| row.id.clone()));
        self.selection_anchor = None;
        self.bulk.notice = None;
        cx.notify();
    }

    /// The columns rows draw this frame: the pinned first, those in the horizontal view with
    /// one either side, the cursor's and the edited cell's. A row lays out each cell it draws,
    /// so a wide type would otherwise pay for every column on every scroll
    /// (`docs/drawing.md`). Worked out as the frame is drawn, so the header draws the same.
    fn drawn_columns(&self, window: &Window, cx: &App) -> Vec<bool> {
        if self.columns.is_empty() {
            return Vec::new();
        }
        let handle = &self.table_state.read(cx).horizontal_scroll_handle;
        // The scrolled part is never wider than the window, whose width is this frame's, also
        // in the frame that resizes it; the offset is this frame's too, as a scroll draws again.
        let scrolled: Vec<Pixels> = self
            .columns
            .iter()
            .skip(1)
            .map(|column| px(self.layout.width(column)))
            .collect();
        let width = window.viewport_size().width;
        let content = scrolled.iter().fold(Pixels::ZERO, |sum, w| sum + *w);
        let shown_left = studio_ui::scrolled_within(-handle.offset().x, content, width);
        let shown = shown_left..shown_left + width;
        let mut drawn = vec![true];
        drawn.extend(columns_in_view(&scrolled, shown));
        let edited = self.editing.as_ref().and_then(|editing| {
            self.columns
                .iter()
                .position(|column| column.key == editing.column)
        });
        for col in self.cursor.map(|c| c.col).into_iter().chain(edited) {
            if let Some(drawn) = drawn.get_mut(col) {
                *drawn = true;
            }
        }
        drawn
    }

    /// What a cell shows, and a date's or time's exact value, made once (see `cell_text`):
    /// looked up without allocating. A date or a time reads relative to now, and is made again
    /// in a later minute.
    fn cell_text_of(&self, row: &Row, column: &ColumnSpec, value: &Value) -> CellText {
        if let Some(made) = self
            .cell_text
            .borrow()
            .get(&row.id)
            .and_then(|cells| cells.get(&column.key))
            .filter(|made| made.minute.is_none_or(|minute| minute == self.frame_minute))
        {
            return made.clone();
        }
        let property = column.property();
        let temporal = property.is_some_and(|property| {
            !property.list && matches!(property.kind, ScalarKind::Date | ScalarKind::DateTime)
        });
        let made = match crate::model::relative_value(property, value, self.frame_now) {
            Some((shown, exact)) => CellText {
                shown: shown.into(),
                exact: Some(exact.into()),
                minute: Some(self.frame_minute),
            },
            None => CellText {
                shown: display_value(property, value).into(),
                exact: None,
                minute: temporal.then_some(self.frame_minute),
            },
        };
        self.cell_text
            .borrow_mut()
            .entry(row.id.clone())
            .or_default()
            .insert(column.key.clone(), made.clone());
        made
    }

    fn render_cell(
        &self,
        ix: usize,
        col: usize,
        row: &Row,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let column = &self.columns[col];
        let (focus_border, divider, editor_bg) = (
            studio_ui::ds::surface::focus(cx),
            studio_ui::ds::surface::rule(cx),
            studio_ui::ds::surface::document(cx),
        );
        let is_cursor = self.cursor == Some(Cursor { row: ix, col });
        let editing = self
            .editing
            .as_ref()
            .filter(|e| e.row_id == row.id && e.column == column.key);
        // Most tables hold no cell state; looking one up costs two keys' copies.
        let state = if self.cells.is_empty() {
            None
        } else {
            self.cells.get(&(row.id.clone(), column.key.clone()))
        };
        let value = row.value(&column.key);
        let mut cell = div()
            .id(ElementId::NamedInteger(
                "cell".into(),
                (ix * self.columns.len() + col) as u64,
            ))
            .relative()
            .h(px(ROW_HEIGHT))
            .w_full()
            .px(px(space::LG))
            .when(col == 0, |d| d.pl(px(KEY_TEXT_INSET)))
            .flex()
            .items_center()
            .gap(px(space::XS))
            .overflow_hidden()
            // A rule on every cell's right but the last column's: the grid's edge is its own.
            .when(col + 1 < self.columns.len(), |d| {
                d.border_r_1().border_color(divider)
            })
            .when(column.align_right, |d| d.justify_end());
        // The cursor and the editor are drawn as a ring over the cell, so no cell changes size.
        let ring = || {
            div()
                .absolute()
                .inset_0()
                .border_1()
                .border_color(focus_border)
        };
        if let Some(editing) = editing {
            let hint = column
                .property()
                .and_then(|p| p.enum_values.as_ref())
                .map(|values| format!("One of: {}", values.join(", ")));
            // The editor itself is drawn over this place by the table (`edit_slot`).
            return cell
                .bg(editor_bg)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .child(self.edit_slot.mark()),
                )
                .child(ring())
                .child(self.render_edit_callout(editing.notice.clone(), hint, window, cx))
                .into_any_element();
        }
        // Dates read relative; the exact value is the cell's tooltip.
        let CellText {
            shown,
            exact: relative,
            ..
        } = self.cell_text_of(row, column, value);
        let place = (ix * self.columns.len() + col) as u64;
        let text_color = if value.is_null() {
            Color::Disabled
        } else {
            Color::Default
        };
        let numeric = column.align_right;
        if column.relationship().is_some() {
            // The cell's padding on both sides.
            let available = self.layout.width(column) - 2. * space::LG;
            return cell
                .child(self.render_relation_cell(place, col, row, available, cx))
                .when(is_cursor, |cell| cell.child(ring()))
                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    this.click_cell(ix, col, event.modifiers(), event.click_count(), window, cx)
                }))
                .into_any_element();
        }
        // A draft shows what was typed, so a row of drafts reads as it will once saved.
        let draft = match state {
            Some(CellState::Draft { text, .. }) => Some(text.clone()),
            _ => None,
        };
        let content: AnyElement = match column.property() {
            _ if draft.is_some() => {
                let text = draft.unwrap_or_default();
                let empty = text.trim().is_empty();
                studio_ui::ds::text::body(if empty { "—".to_owned() } else { text })
                    .color(if empty {
                        Color::Disabled
                    } else {
                        Color::Default
                    })
                    .single_line()
                    .truncate()
                    .into_any_element()
            }
            Some(property) if property.kind == ScalarKind::Blob => {
                self.render_blob_cell(place, row, property, cx)
            }
            Some(property) if property.enum_values.is_some() && !value.is_null() => {
                studio_ui::ds::chip(shown, cx).into_any_element()
            }
            _ => {
                // One line in the cell's room, never measured: most cells are this.
                let mut text = studio_ui::cell_text(shown, cx).color(text_color.color(cx));
                if numeric {
                    text = text.buffer_font(cx).align_end();
                }
                // A key is read like any value; its column header says it is the key.
                if matches!(column.role, ColumnRole::Key) {
                    text = text.weight(studio_ui::ds::text::weight::BODY);
                }
                text.into_any_element()
            }
        };
        let (dot, tooltip): (_, Option<SharedString>) = match state {
            Some(CellState::Saving { .. }) => (
                Some(Color::Warning),
                Some(format!("Saving to {}…", self.branch).into()),
            ),
            Some(CellState::Draft { text, notice }) => {
                let color = match notice.as_ref().map(|n| n.tone) {
                    Some(Tone::Refused) | Some(Tone::Invalid) => Color::Error,
                    Some(Tone::Conflict) | Some(Tone::Unknown) => Color::Warning,
                    None => Color::Accent,
                };
                let message = notice
                    .as_ref()
                    .map(|n| format!("{}\n", n.message))
                    .unwrap_or_default();
                let held = if value.is_null() {
                    "The cell is empty.".to_owned()
                } else {
                    format!(
                        "The cell holds “{}”.",
                        display_value(column.property(), value)
                    )
                };
                (
                    Some(color),
                    Some(
                        format!(
                            "{message}Not saved: “{text}”. {held} \
                             Reopen it to edit; Save as one commit saves every draft together."
                        )
                        .into(),
                    ),
                )
            }
            None => (None, relative),
        };
        // A Blob cell takes a file dropped on it from the file manager, and shows the write's
        // dot while it is out and why it did not land after.
        let blob = column
            .property()
            .filter(|property| property.kind == ScalarKind::Blob)
            .map(|property| property.name.clone());
        // Most tables hold no Blob write; looking one up costs two keys' copies.
        let (dot, tooltip) = match blob
            .as_ref()
            .filter(|_| !self.blob_edits.is_empty())
            .and_then(|property| self.blob_edits.get(&(row.id.clone(), property.clone())))
        {
            Some(BlobEdit::Saving(_)) => (
                Some(Color::Warning),
                Some(format!("Saving to {}…", self.branch).into()),
            ),
            Some(BlobEdit::Failed { notice, again }) => (
                Some(notice.tone.color()),
                Some(match again {
                    Some(_) => format!(
                        "{} Drop the file again to write it over that.",
                        notice.message
                    )
                    .into(),
                    None => notice.message.clone().into(),
                }),
            ),
            None => (dot, tooltip),
        };
        if let Some(property) = blob.filter(|property| self.takes_blob_drop(&row.id, property, cx))
        {
            let (row_id, drop_bg) = (row.id.clone(), studio_ui::ds::surface::drop_target(cx));
            cell = cell
                .drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(drop_bg))
                .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                    this.drop_blob_files(&row_id, &property, paths.paths(), cx)
                }));
        }
        if is_cursor {
            cell = cell.child(ring());
        }
        if row.dimmed && col == 0 {
            cell = cell.tooltip(Tooltip::text(
                "No longer matches the filters or its group. It leaves when the list is read again.",
            ));
        } else if let Some(tooltip) = tooltip {
            cell = cell.tooltip(Tooltip::text(tooltip));
        }
        // The key's cell ends in the row's Peek while the row is hovered, as the reference
        // table's cells end in their action.
        let peek = (col == 0).then(|| {
            div().ml_auto().flex_none().child(
                studio_ui::ds::cell_action(("peek-row", ix), IconName::Eye)
                    .visible_on_hover(ROW_GROUP)
                    .tooltip({
                        let focus = self.focus_handle.clone();
                        move |_, cx| Tooltip::for_action_in("Peek", &PeekRow, &focus, cx)
                    })
                    .on_click(cx.listener(move |this, _, window, cx| this.peek_at(ix, window, cx))),
            )
        });
        cell.children(dot.map(studio_ui::ds::dot))
            .child(content)
            .children(peek)
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.click_cell(ix, col, event.modifiers(), event.click_count(), window, cx)
            }))
            .into_any_element()
    }

    fn render_edit_callout(
        &self,
        notice: Option<Notice>,
        hint: Option<String>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Under the cell, its edge on the cell's, a hairline's gap below it.
        floating_under(
            point(
                px(-studio_ui::ds::size::HAIRLINE),
                px(ROW_HEIGHT + space::XXS),
            ),
            2,
            v_flex()
                .min_w(px(studio_ui::ds::size::POPOVER_MIN))
                .max_w(px(studio_ui::ds::size::POPOVER_MAX))
                .px(px(space::MD))
                .py(px(space::SM))
                .gap(px(space::XS))
                .map(|callout| studio_ui::ds::menu_surface(callout, cx))
                .children(notice.map(|notice| {
                    let unknown = notice.tone == Tone::Unknown;
                    h_flex()
                        .gap(px(space::SM))
                        .child(studio_ui::ds::dot(notice.tone.color()))
                        .child(studio_ui::ds::text::body(notice.message).color(notice.tone.color()))
                        // Whether the save landed is in the branch's history.
                        .when(unknown, |row| {
                            row.child(
                                studio_ui::ds::button("open-history", "Open history")
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
                .child(studio_ui::ds::text::meta(format!(
                    "↵ save · esc cancel · write · {}",
                    self.branch
                ))),
        )
    }

    /// The table's part of the status bar ([`Item::status`]): a page being read, or a Retry
    /// when one could not be, and the keys that act on the cursor, which the bar's keyboard
    /// button lists.
    fn status_parts(&self, cx: &mut Context<Self>) -> ItemStatus {
        // The status bar names only what is happening: the row count is in the toolbar, and the
        // keys are behind its keyboard button.
        let mut start = Vec::new();
        if self.loading_more {
            start.push(studio_ui::ds::spinner().into_any_element());
        }
        if let Some(error) = self.more_error.clone() {
            start.push(
                studio_ui::ds::row_button("retry-more", "Retry")
                    .style(ButtonStyle::Subtle)
                    .start_icon(Icon::new(IconName::Warning))
                    .tooltip(Tooltip::text(format!(
                        "The next page could not be read: {error}"
                    )))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.more_error = None;
                        this.load_more(cx);
                    }))
                    .into_any_element(),
            );
        }
        let focus = self.focus_handle.clone();
        let key =
            |label: &str, action: &dyn gpui::Action| KeyHint::new(label.to_owned(), action, &focus);
        // The keys of actions the credential may not take on the branch are not listed, as
        // they are not registered.
        let refused = self.change_refusal(cx).is_some();
        let mut keys = match (self.editing.is_some(), self.kind) {
            (true, _) => vec![
                key("Save", &ConfirmEdit),
                key("Keep as draft", &StageEdit),
                key("Cancel", &Cancel),
            ],
            // A table pinned to a commit only reads.
            (false, kind) if self.at_commit.is_some() => {
                let mut keys = vec![key("Peek", &PeekRow), key("Copy", &CopyCell)];
                if kind == TableKind::Node {
                    keys.insert(1, key("Open page", &OpenRowPage));
                }
                keys
            }
            (false, TableKind::Node) => {
                let mut keys = vec![
                    key("Edit", &EditCell),
                    key("Peek", &PeekRow),
                    key("Open page", &OpenRowPage),
                    key("Explore", &ExploreRow),
                    key("Copy", &CopyCell),
                    key("Filter by this value", &FilterBySelection),
                    key("New row", &InsertRow),
                    key("Delete row", &DeleteRow),
                    key("Save drafts", &SaveDrafts),
                    key("Refresh", &RefreshTable),
                ];
                if self.blob_under_cursor().is_some() {
                    keys.insert(1, key("Choose file", &ChooseFile));
                    keys.insert(2, key("Clear value", &ClearValue));
                }
                keys
            }
            (false, TableKind::Edge) => vec![
                key("Peek", &PeekRow),
                key("Copy", &CopyCell),
                key("New row", &InsertRow),
                key("Delete row", &DeleteRow),
                key("Refresh", &RefreshTable),
            ],
        };
        if refused {
            keys.retain(|hint| !writes(hint.action.as_ref()));
        }
        ItemStatus {
            start,
            end: Vec::new(),
            keys,
        }
    }
}

/// Whether a table key writes to the branch, so goes when the credential may not change it.
fn writes(action: &dyn gpui::Action) -> bool {
    let writing: [&dyn gpui::Action; 7] = [
        &EditCell,
        &ConfirmEdit,
        &SaveDrafts,
        &InsertRow,
        &DeleteRow,
        &ChooseFile,
        &ClearValue,
    ];
    writing.iter().any(|key| key.partial_eq(action))
}

fn filter_placeholder(op: FilterOp) -> &'static str {
    match op {
        FilterOp::In => "Values, separated by commas",
        FilterOp::RelatedTo => "Name or id",
        FilterOp::StartsWith => "Start of the value",
        FilterOp::Search => "Words, in any order",
        FilterOp::Expr => "GQ on $n, for example $n.kind = \"dinner\" or $n.kind = \"party\"",
        _ => "Value",
    }
}

fn set_value(row: &mut Row, column: &str, value: Value) {
    if value.is_null() {
        row.values.remove(column);
    } else {
        row.values.insert(column.to_owned(), value);
    }
}

impl Render for TypeTable {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        if std::mem::take(&mut self.rows_owed) {
            cx.defer_in(window, |this, _, cx| this.read_rows(cx));
        }
        // An edit taken from the cell (a reload kept it as a draft) leaves the cell editor
        // focused but no longer drawn: the keys come back to the table.
        if self.editing.is_none() && self.cell_editor.focus_handle(cx).is_focused(window) {
            window.focus(&self.focus_handle, cx);
        }
        // Keys typed into a field are text, not the table's: the cell editor, the filter
        // value and the bulk value alike.
        let typing = self.editing.is_some()
            || self.filter_editor.focus_handle(cx).is_focused(window)
            || self.bulk_editor.focus_handle(cx).is_focused(window)
            || self.words_editor.focus_handle(cx).is_focused(window)
            || self.fill_editor.focus_handle(cx).is_focused(window);
        let mut context = KeyContext::new_with_defaults();
        context.add("TypeTable");
        if typing {
            context.add("editing");
        }
        let background = studio_ui::ds::surface::document(cx);
        let toolbar = self.render_toolbar(cx);
        let filter_bar = self.render_filter_bar(window, cx);
        let bulk_bar = self.render_bulk_bar(cx);
        let drafts_bar = self.render_drafts_bar(cx);
        let writable = self.change_refusal(cx).is_none();
        let grid = self.grid.clone();
        v_flex()
            .key_context(context)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(background)
            .on_action(cx.listener(|this, _: &MoveUp, _, cx| this.move_cursor(-1, 0, cx)))
            .on_action(cx.listener(|this, _: &MoveDown, _, cx| this.move_cursor(1, 0, cx)))
            .on_action(cx.listener(|this, _: &MoveLeft, _, cx| this.move_cursor(0, -1, cx)))
            .on_action(cx.listener(|this, _: &MoveRight, _, cx| this.move_cursor(0, 1, cx)))
            .on_action(
                cx.listener(|this, _: &menu::SelectNext, window, cx| this.tab(1, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &menu::SelectPrevious, window, cx| this.tab(-1, window, cx)),
            )
            // ↵ also confirms the filter being written, so it is always registered; an edit it
            // would save on a refused branch is kept as a draft instead.
            .on_action(
                cx.listener(|this, _: &ConfirmEdit, window, cx| this.confirm_edit(window, cx)),
            )
            .on_action(cx.listener(|this, _: &StageEdit, window, cx| {
                this.stage_edit(window, cx);
            }))
            // What the credential may not take on the branch is not registered, so the palette
            // and the keys offer only what it may (constitution VIII).
            .when(writable, |table| {
                table
                    .on_action(cx.listener(Self::on_edit))
                    .on_action(
                        cx.listener(|this, _: &SaveDrafts, window, cx| {
                            this.save_drafts(window, cx)
                        }),
                    )
                    .on_action(cx.listener(Self::on_insert))
                    .on_action(cx.listener(Self::on_delete))
            })
            .on_action(cx.listener(Self::on_cancel))
            .on_action(cx.listener(Self::on_peek))
            .on_action(cx.listener(Self::on_open_row_page))
            .on_action(cx.listener(Self::on_explore))
            .on_action(cx.listener(|this, _: &RefreshTable, window, cx| this.refresh(window, cx)))
            .on_action(cx.listener(Self::on_copy))
            .on_action(
                cx.listener(|this, _: &AddFilter, window, cx| this.open_filter_draft(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &FilterInWords, window, cx| this.open_words(window, cx)),
            )
            .on_action(cx.listener(Self::on_fill_column))
            .on_action(cx.listener(|this, _: &FilterBySelection, window, cx| {
                this.filter_by_selection(window, cx)
            }))
            .on_action(cx.listener(|this, _: &ClearFilters, _, cx| this.clear_filters(cx)))
            .when(
                self.at_commit.is_none() && writable && self.blob_under_cursor().is_some(),
                |table| {
                    table
                        .on_action(cx.listener(Self::on_choose_blob_file))
                        .on_action(cx.listener(Self::on_clear_blob))
                },
            )
            .child(toolbar)
            .children(filter_bar)
            .children(bulk_bar)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(AnyView::from(grid).cached(StyleRefinement::default().size_full()))
                    .when(self.editing.is_some(), |body| {
                        body.child(
                            self.edit_slot.overlay(
                                div()
                                    .size_full()
                                    .flex()
                                    .items_center()
                                    .child(self.cell_editor.clone()),
                            ),
                        )
                    })
                    .children(drafts_bar),
            )
            .children(
                self.header_menu
                    .as_ref()
                    .map(|(menu, position, _)| studio_ui::ds::menu_at(menu.clone(), *position)),
            )
    }
}

/// The table's rows and their headers, drawn as a view of their own. Typing in the filter or
/// bulk-edit field draws the table again, as GPUI draws every view around a changed field, and
/// a grid drawn inside it would be laid out again with it: about as much work as a scroll, on
/// every key. Held apart and drawn cached, the grid keeps its last frame until the table
/// notifies or the grid itself changes (a scroll, the cell being edited).
struct TableGrid {
    table: WeakEntity<TypeTable>,
    _table_changed: Subscription,
    /// How often the grid was drawn, for the test that typing leaves it alone.
    #[cfg(test)]
    renders: usize,
}

impl TableGrid {
    /// Made with the table, not on its first draw: an entity made while a frame is drawn counts
    /// as read before its own cached draw, so GPUI leaves it out of what that view depends on
    /// and stops redrawing it when it notifies.
    fn new(cx: &mut Context<TypeTable>) -> Entity<Self> {
        let table = cx.entity();
        cx.new(|cx| TableGrid {
            table: table.downgrade(),
            _table_changed: cx.observe(&table, |_, _, cx| cx.notify()),
            #[cfg(test)]
            renders: 0,
        })
    }
}

impl Render for TableGrid {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        self.table
            .update(cx, |table, cx| table.render_body(window, cx))
            .unwrap_or_else(|_| gpui::Empty.into_any_element())
    }
}

/// What a header menu entry does to its table.
type TableAction = dyn Fn(&mut TypeTable, &mut Window, &mut Context<TypeTable>);

/// A column header being dragged onto another.
#[derive(Clone)]
struct DraggedColumn {
    key: String,
    title: SharedString,
}

impl Render for DraggedColumn {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        studio_ui::ds::drag_preview(cx).child(studio_ui::ds::text::body(self.title.clone()))
    }
}

/// A scope for a table of `session`'s branch.
fn new_scope(session: &Entity<GraphSession>, cx: &mut App) -> ReadScope {
    session.update(cx, |session, _| {
        let branch = session.branch.clone();
        session.read_scope(&branch)
    })
}

/// Takes a wheel over the rows before Zed's row list sees it, so a gesture moves the rows or
/// the columns and not both. The list scrolls only vertically, holds no axis, and GPUI gives a
/// vertical-only scroll area a purely sideways delta as a vertical one; every scroll area
/// under the pointer applies a wheel, none stopping it (`paint_scroll_listener`). The columns'
/// areas hold their axis for a gesture (`restrict_scroll_to_axis`). So the gesture's axis is
/// held here too, in the capture phase, as GPUI holds it (`OngoingScroll`): a trackpad gesture
/// keeps the axis it started on until it turns clearly to the other, and a wheel that sends
/// lines is judged event by event.
///
/// A sideways one moves the shared horizontal handle (the header's and every row's) and goes
/// no further. A purely sideways drift of one held up and down goes no further either, since
/// the list would move the rows by it. Anything else passes through untouched. Over the open
/// cell editor the wheel is the editor's. The area is the pointer's by `should_handle_scroll`,
/// as for any scroll area: `is_hovered` is false after a key. A moved handle draws again the
/// view that drew the grid and the views holding it (`mark_view_dirty`), as GPUI's own scroll
/// areas do, not every view in the window, which a refresh would (`refreshing` passes over
/// every view's cache).
fn sideways_scroll(
    columns: gpui::ScrollHandle,
    gesture: Rc<RefCell<gpui::OngoingScroll>>,
    editor: studio_ui::OverlaySlot,
) -> impl IntoElement {
    gpui::canvas(
        |bounds, window, _| window.insert_hitbox(bounds, gpui::HitboxBehavior::Normal),
        move |_, hitbox, window, _| {
            let view = window.current_view();
            window.on_mouse_event(move |event: &gpui::ScrollEvent, phase, window, cx| {
                if phase != gpui::DispatchPhase::Capture
                    || !hitbox.should_handle_scroll(window)
                    || editor.contains(event.position)
                {
                    return;
                }
                let sent = event.delta.pixel_delta(window.line_height());
                let mut delta = sent;
                if event.delta.precise() {
                    gesture.borrow_mut().filter(&mut delta, event.touch_phase);
                }
                if delta.x.abs() > delta.y.abs() {
                    let offset = columns.offset();
                    let x = (offset.x + delta.x).clamp(-columns.max_offset().x, px(0.));
                    if x != offset.x {
                        columns.set_offset(point(x, offset.y));
                        cx.notify(view);
                    }
                } else if sent.x == px(0.) || sent.y != px(0.) {
                    return;
                }
                cx.stop_propagation();
            });
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// `content` floating under the element this is placed in, `offset` from its top-left corner:
/// drawn over the rest of the window, `priority` among other overlays, and kept inside it by a
/// margin.
fn floating_under(
    offset: gpui::Point<Pixels>,
    priority: usize,
    content: impl IntoElement,
) -> AnyElement {
    div()
        .absolute()
        .top_0()
        .left_0()
        .child(
            deferred(
                anchored()
                    .anchor(Anchor::TopLeft)
                    .offset(offset)
                    .snap_to_window_with_margin(px(space::MD))
                    .child(content),
            )
            .with_priority(priority),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};

    use crate::test_support::{PEOPLE, changed, session, session_followed};

    fn grid_renders(table: &Entity<TypeTable>, cx: &mut VisualTestContext) -> usize {
        table.read_with(cx, |table, cx| table.grid.read(cx).renders)
    }

    /// A table not drawn (restored with the workspace behind another tab) reads no rows: not
    /// when it is made, nor when the branch moves. Drawn, it reads them. Before, every such
    /// table read its first page at start and again at every branch move.
    #[gpui::test]
    async fn a_table_not_drawn_reads_no_rows(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let session = session(PEOPLE, cx);
        cx.run_until_parked();
        let (_blank, cx) = cx.add_window_view(|_, _| gpui::EmptyView);
        let hidden = cx.update(|window, cx| {
            let session = session.clone();
            cx.new(|cx| {
                TypeTable::new(
                    WeakEntity::new_invalid(),
                    session,
                    "Person".into(),
                    None,
                    window,
                    cx,
                )
            })
        });
        cx.run_until_parked();
        session.update(cx, |session, cx| session.set_branch("fix".into(), cx));
        cx.run_until_parked();
        hidden.read_with(cx, |table, _| {
            assert!(table.page_task.is_none(), "no page read");
            assert!(table.rows_owed, "it reads when drawn");
            assert!(table.rows.is_empty());
        });
        let (shown, cx) = cx.add_window_view(|window, cx| {
            TypeTable::new(
                WeakEntity::new_invalid(),
                session,
                "Person".into(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        shown.read_with(cx, |table, _| {
            assert!(!table.rows.is_empty(), "drawn, it reads")
        });
    }

    /// Another writer's commit that changed none of the types the table reads moves its pin to
    /// the head without reading again; one that wrote its type offers Refresh (spec 015).
    #[gpui::test]
    async fn a_table_moves_its_pin_past_commits_to_other_types(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let reads = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = reads.clone();
        let (session, writer) = session_followed(
            &format!("{PEOPLE}node Note {{\n  slug: String @key\n}}\n"),
            move |request| {
                let name = request["name"].as_str().unwrap_or_default();
                seen.lock().unwrap().push(name.to_owned());
                if name == "studio_head" {
                    return serde_json::json!([]);
                }
                serde_json::json!([
                    {"n": {"@id": "p1", "slug": "p1", "name": "Ada"}},
                    {"n": {"@id": "p2", "slug": "p2", "name": "Grace"}},
                ])
            },
            cx,
        );
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::new(
                WeakEntity::new_invalid(),
                session.clone(),
                "Person".into(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert_eq!(table.paging.pinned.as_deref(), Some("c1"))
        });
        let read_before = reads.lock().unwrap().len();

        writer.commit("c2", vec![changed("node", "Note", "n1", None)]);
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert_eq!(
                table.paging.pinned.as_deref(),
                Some("c2"),
                "moved to the head"
            );
            assert!(!table.newer);
        });
        assert_eq!(
            reads.lock().unwrap()[read_before..].to_vec(),
            ["studio_head"],
            "nothing read again"
        );

        writer.commit("c3", vec![changed("node", "Person", "p3", None)]);
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert_eq!(table.paging.pinned.as_deref(), Some("c2"));
            assert!(table.newer, "a person changed");
        });
    }

    /// Another writer's schema change, seen at a head move, leaves the table as it was read and
    /// offers Refresh; Refresh reads it under the new schema (constitution II).
    #[gpui::test]
    async fn a_schema_changed_elsewhere_waits_for_refresh(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, writer) = session_followed(
            PEOPLE,
            |request| {
                if request["name"] == "studio_head" {
                    return serde_json::json!([]);
                }
                serde_json::json!([
                    {"n": {"@id": "p1", "slug": "p1", "name": "Ada"}},
                    {"n": {"@id": "p2", "slug": "p2", "name": "Grace"}},
                ])
            },
            cx,
        );
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::new(
                WeakEntity::new_invalid(),
                session.clone(),
                "Person".into(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        let grown = format!("{PEOPLE}node Note {{\n  slug: String @key\n}}\n");
        writer.set_schema(&grown);
        writer.commit("c2", vec![changed("node", "Note", "n1", None)]);
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert!(table.newer, "Refresh is offered");
            assert_eq!(table.rows.len(), 2, "the rows stay");
            assert_eq!(
                table.paging.pinned.as_deref(),
                Some("c1"),
                "and their commit"
            );
            assert_eq!(&*table.schema.as_ref().unwrap().source, PEOPLE);
        });

        table.update_in(cx, |table, window, cx| table.refresh(window, cx));
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert!(!table.newer);
            assert_eq!(&*table.schema.as_ref().unwrap().source, grown.as_str());
            assert_eq!(table.rows.len(), 2);
        });
    }

    /// A section folded away takes the cursor out of it, so no key acts on a row not drawn;
    /// another section folding leaves it.
    #[gpui::test]
    async fn a_folded_section_takes_the_cursor_out_of_it(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let session = session(PEOPLE, cx);
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::new(
                WeakEntity::new_invalid(),
                session,
                "Person".into(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        table.update(cx, |table, cx| {
            let mut grouping = Grouping::new("name");
            grouping.restart();
            let counts: ReadOutput = serde_json::from_value(serde_json::json!({
                "query_name": "studio_value_counts",
                "target": {"branch": "main"},
                "row_count": 2,
                "columns": ["value", "n"],
                "rows": [{"value": "Ada", "n": 1}, {"value": "Grace", "n": 1}],
            }))
            .unwrap();
            grouping.accept_counts(&counts, GROUP_LIMIT);
            for (index, range) in [0..1, 1..2].into_iter().enumerate() {
                let section = grouping.section_mut(index).unwrap();
                section.range = range;
                section.expanded = true;
            }
            table.grouping = Some(grouping);
            table.cursor = Some(Cursor { row: 1, col: 0 });
            table.toggle_section(0, cx);
            assert_eq!(
                table.cursor,
                Some(Cursor { row: 1, col: 0 }),
                "another section"
            );
            table.toggle_section(1, cx);
            assert_eq!(table.cursor, None);
        });
    }

    /// Keys typed into the bulk value are its text: `j` there is a letter, not a step down.
    #[gpui::test]
    async fn keys_typed_in_the_bulk_value_are_its_text(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
            cx.bind_keys([gpui::KeyBinding::new(
                "j",
                MoveDown,
                Some("TypeTable && !editing"),
            )]);
        });
        let session = session(PEOPLE, cx);
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::new(
                WeakEntity::new_invalid(),
                session,
                "Person".into(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        table.update_in(cx, |table, window, cx| {
            let ids: Vec<String> = table.rows.iter().map(|row| row.id.clone()).collect();
            table.selection.extend(ids);
            table.cursor = Some(Cursor { row: 0, col: 0 });
            table.bulk.property = Some("name".into());
            window.focus(&table.bulk_editor.focus_handle(cx), cx);
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("j");
        cx.run_until_parked();
        table.read_with(cx, |table, cx| {
            assert_eq!(table.bulk_editor.read(cx).text(cx), "j");
            assert_eq!(table.cursor, Some(Cursor { row: 0, col: 0 }));
        });
    }

    /// Typing in the filter field or in a cell, or moving through the filter's offered values,
    /// draws the grid no more: the field and its offers draw over it or beside it. Anything the
    /// table announces draws it again.
    #[gpui::test]
    async fn typing_in_a_field_leaves_the_grid_as_it_was(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let schema = PEOPLE.replace("  name: String\n", "  name: String\n  active: Bool?\n");
        let session = session(&schema, cx);
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::new(
                WeakEntity::new_invalid(),
                session,
                "Person".into(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        table.read_with(cx, |table, _| assert_eq!(table.rows.len(), 2));

        // The grid follows the table, also when it was made with the table.
        let before = grid_renders(&table, cx);
        table.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!(grid_renders(&table, cx), before + 1);

        // The first key also switches GPUI to keyboard input, which draws the whole window
        // once; the keys after it are the ones counted.
        table.update_in(cx, |table, window, cx| table.open_filter_draft(window, cx));
        cx.simulate_input("A");
        cx.run_until_parked();
        let before = grid_renders(&table, cx);
        cx.simulate_input("d");
        cx.run_until_parked();
        table.read_with(cx, |table, cx| {
            assert_eq!(table.filter_editor.read(cx).text(cx), "Ad")
        });
        assert_eq!(
            grid_renders(&table, cx),
            before,
            "the filter field drew the grid"
        );

        // Moving through the offered values draws them, beside the grid, and not the grid.
        table.update_in(cx, |table, window, cx| {
            table.set_draft_property("active".into(), window, cx)
        });
        cx.run_until_parked();
        let (before, drew) = (
            grid_renders(&table, cx),
            table.read_with(cx, |table, _| table.renders),
        );
        table.update(cx, |table, cx| table.move_suggestion(1, cx));
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            let offers = &table.filter_draft.as_ref().unwrap().offers;
            assert_eq!(offers.len(), 2);
            assert_eq!(offers.highlighted(), Some(0));
            assert!(table.renders > drew, "the offers were drawn again");
        });
        assert_eq!(grid_renders(&table, cx), before, "the offers drew the grid");

        table.update_in(cx, |table, window, cx| {
            table.filter_draft = None;
            let col = table.columns.iter().position(|c| c.key == "name").unwrap();
            table.start_edit(Cursor { row: 0, col }, window, cx);
        });
        cx.run_until_parked();
        let before = grid_renders(&table, cx);
        cx.simulate_input("Ada L");
        cx.run_until_parked();
        table.read_with(cx, |table, cx| {
            assert_eq!(table.cell_editor.read(cx).text(cx), "Ada L")
        });
        assert_eq!(
            grid_renders(&table, cx),
            before,
            "the cell editor drew the grid"
        );
    }

    /// A text cell with more than one line or with markup is not edited in the one-line cell
    /// editor, which would flatten it: it opens in the text editor (spec 014). A plain value
    /// edits in the cell.
    #[gpui::test]
    async fn a_text_cell_with_lines_opens_in_the_editor_not_the_cell(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let rows = serde_json::json!([
            {"n": {"@id": "p1", "slug": "p1", "name": "Ada"}},
            {"n": {"@id": "p2", "slug": "p2", "name": "first line\nsecond line"}},
        ]);
        let session = crate::test_support::session_reading(PEOPLE, rows, cx);
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::new(
                WeakEntity::new_invalid(),
                session,
                "Person".into(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        table.update_in(cx, |table, window, cx| {
            let col = table.columns.iter().position(|c| c.key == "name").unwrap();
            table.start_edit(Cursor { row: 1, col }, window, cx);
            assert!(table.editing.is_none(), "two lines go to the text editor");
            table.start_edit(Cursor { row: 0, col }, window, cx);
            assert!(table.editing.is_some(), "a plain value edits in the cell");
        });
    }

    /// A node's page reads the node by id once it is shown and names its tab by the node's
    /// label; it is one tab per node and commit, and comes back after a restart as it was,
    /// named by the label it was saved with until it is shown and reads the node (spec 014).
    #[gpui::test]
    async fn a_nodes_page_reads_it_and_names_its_tab(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let session = session(PEOPLE, cx);
        cx.run_until_parked();
        let (page, cx) = cx.add_window_view({
            let session = session.clone();
            move |_, cx| crate::NodePage::new(session, "Person".into(), "p1".into(), None, None, cx)
        });
        cx.run_until_parked();
        let saved = page.read_with(cx, |page, cx| {
            assert_eq!(page.tab_title(cx).as_ref(), "Ada", "the label, once read");
            assert_eq!(page.dedupe_key(cx).as_deref(), Some("node:Person:p1"));
            let saved = page.serialize(cx).expect("a page is remembered");
            assert_eq!(saved.kind, "node");
            assert_eq!(saved.data["type_name"], "Person");
            assert_eq!(saved.data["id"], "p1");
            assert_eq!(saved.data["label"], "Ada");
            assert!(saved.data["at_commit"].is_null(), "it reads the head");
            saved
        });
        // Restored behind another tab: named as it was saved, though it has read nothing.
        let label = saved.data["label"].as_str().map(str::to_owned);
        let restored = cx.new(|cx| {
            crate::NodePage::new(
                session.clone(),
                "Person".into(),
                "p1".into(),
                None,
                label,
                cx,
            )
        });
        cx.run_until_parked();
        restored.read_with(cx, |page, cx| {
            assert_eq!(page.tab_title(cx).as_ref(), "Ada", "named before it reads");
        });
        let held = cx.new(|cx| {
            crate::NodePage::new(
                session,
                "Person".into(),
                "p1".into(),
                Some("c0".into()),
                None,
                cx,
            )
        });
        cx.run_until_parked();
        held.read_with(cx, |page, cx| {
            assert_eq!(page.dedupe_key(cx).as_deref(), Some("node:Person:p1@c0"));
            assert_eq!(page.serialize(cx).unwrap().data["at_commit"], "c0");
            assert!(!page.is_stale(cx), "a page held at a commit is never newer");
        });
    }

    /// People by their company's city, and an edge by a property of either end: the Group menu
    /// offers each, the section header reads the neighbour's property, and a cell edit or a new
    /// row is placed by what the row can know.
    #[gpui::test]
    async fn rows_group_by_a_neighbours_or_an_ends_property(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let schema = "node Person {\n  slug: String @key\n  name: String\n}\n\
            node Company {\n  slug: String @key\n  city: String?\n}\n\
            edge WorksAt: Person -> Company\n";
        let open = |type_name: &str, cx: &mut TestAppContext| {
            let session = session(schema, cx);
            let type_name = type_name.to_owned();
            let (table, cx) = cx.add_window_view(|window, cx| {
                TypeTable::new(
                    WeakEntity::new_invalid(),
                    session,
                    type_name,
                    None,
                    window,
                    cx,
                )
            });
            cx.run_until_parked();
            (table, cx.clone())
        };
        let (people, mut cx_people) = open("Person", cx);
        let cx = &mut cx_people;
        people.read_with(cx, |table, _| {
            let choices = table.group_choices();
            let keys: Vec<&str> = choices.iter().map(|(key, _)| key.as_str()).collect();
            // A key is not offered: every row holds a value of its own.
            assert_eq!(keys, ["name", "out:WorksAt", "out:WorksAt.city"]);
            assert_eq!(choices[2].1, "worksAt → Company.city");
            let menus = table.group_menus();
            assert_eq!(menus.len(), 1);
            assert_eq!(menus[0].label, "worksAt Company");
            let city = table.group_property("out:WorksAt.city").unwrap();
            assert_eq!(city.name, "city");
        });
        people.update(cx, |table, cx| {
            table.set_grouping(Some("out:WorksAt.city".into()), cx)
        });
        cx.run_until_parked();
        people.read_with(cx, |table, _| {
            let grouping = table.grouping.as_ref().expect("grouped");
            assert_eq!(grouping.property, "out:WorksAt.city");
            assert!(!grouping.by_own_property());
            assert!(table.can_group_by("out:WorksAt.city"));
            assert!(!table.can_group_by("out:WorksAt.missing"));
        });

        let (works_at, mut cx_edges) = open("WorksAt", cx);
        let cx = &mut cx_edges;
        works_at.read_with(cx, |table, _| {
            let choices = table.group_choices();
            let keys: Vec<&str> = choices.iter().map(|(key, _)| key.as_str()).collect();
            assert_eq!(keys, ["from.name", "to.city"]);
            assert_eq!(choices[1].1, "to Company.city");
            assert_eq!(
                table.group_property("from.name").map(|p| p.name),
                Some("name".to_owned())
            );
        });
    }

    /// A draft is saved bound to the value it was typed over, not the one a Refresh put in the
    /// cell since: a change made elsewhere meanwhile is a conflict, not overwritten. Its column
    /// hidden, it is saved all the same.
    #[gpui::test]
    async fn a_draft_is_saved_bound_to_the_value_it_was_typed_over(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, sent) = crate::test_support::session_recording_writes(PEOPLE, cx);
        let (table, cx) = cx.add_window_view({
            let session = session.clone();
            |window, cx| {
                TypeTable::new(
                    WeakEntity::new_invalid(),
                    session,
                    "Person".into(),
                    None,
                    window,
                    cx,
                )
            }
        });
        cx.run_until_parked();
        // Typed over "Ada B"; the server now holds "Ada".
        session.update(cx, |session, cx| {
            let key = DraftKey {
                type_name: "Person".into(),
                id: "p1".into(),
                property: "name".into(),
            };
            let draft = graph_store::DraftEntry {
                old: "Ada B".into(),
                text: "Ada King".into(),
            };
            session.set_draft(key, draft, cx);
        });
        cx.run_until_parked();
        table.update_in(cx, |table, window, cx| {
            table.arrange_columns(cx, |layout, all| layout.toggle(all, "name"));
            assert!(table.column_index("name").is_none(), "hidden");
            table.save_drafts(window, cx);
            assert!(table.drafts.notice.is_none(), "{:?}", table.drafts.notice);
        });
        cx.run_until_parked();
        let sent = sent.lock().unwrap();
        let params: Vec<&str> = sent[0]["params"]
            .as_object()
            .unwrap()
            .values()
            .filter_map(Value::as_str)
            .collect();
        assert!(
            params.contains(&"Ada B"),
            "bound to the draft's value: {params:?}"
        );
        assert!(!params.contains(&"Ada"), "not to the one shown: {params:?}");
    }

    /// A list cannot be compared in a write's `where`: a draft setting one is bound to a commit
    /// holding the value it was typed over, the table's own while it is the head, and one setting
    /// only scalars to their values alone. A table behind reads the row at the head first and
    /// binds to that, rather than to its pin, which the write would be refused for.
    #[gpui::test]
    async fn a_list_draft_is_bound_to_a_commit_holding_the_value_it_was_typed_over(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        const TAGGED: &str =
            "node Person {\n  slug: String @key\n  name: String\n  tags: [String]?\n}\n";
        let (session, sent) = crate::test_support::session_recording_writes(TAGGED, cx);
        let (table, cx) = cx.add_window_view({
            let session = session.clone();
            |window, cx| {
                TypeTable::new(
                    WeakEntity::new_invalid(),
                    session,
                    "Person".into(),
                    None,
                    window,
                    cx,
                )
            }
        });
        cx.run_until_parked();
        let draft = |property: &str, old: Value, text: &str, cx: &mut VisualTestContext| {
            session.update(cx, |session, cx| {
                let key = DraftKey {
                    type_name: "Person".into(),
                    id: "p1".into(),
                    property: property.into(),
                };
                let text = text.into();
                session.set_draft(key, graph_store::DraftEntry { old, text }, cx);
            });
            cx.run_until_parked();
            table.update_in(cx, |table, window, cx| table.save_drafts(window, cx));
            cx.run_until_parked();
        };
        draft("tags", Value::Null, "math, poetry", cx);
        draft("name", "Ada".into(), "Ada King", cx);
        // Typed over a list the row no longer holds: a conflict, not a write.
        draft("tags", serde_json::json!(["math"]), "math, art", cx);
        assert_eq!(sent.lock().unwrap().len(), 2, "{:?}", sent.lock().unwrap());
        assert_eq!(sent.lock().unwrap()[0]["if_graph_commit"], "c1");
        assert!(sent.lock().unwrap()[1].get("if_graph_commit").is_none());
        table.read_with(cx, |table, _| {
            let cell = table.cells.get(&("p1".to_owned(), "tags".to_owned()));
            assert!(
                matches!(
                    cell,
                    Some(CellState::Draft {
                        notice: Some(_),
                        ..
                    })
                ),
                "{cell:?}"
            );
        });
        // Read at a commit the head has moved past: the row is read at the head, where the list
        // is as the draft found it, and the write is bound there.
        table.update(cx, |table, _| table.paging.pinned = Some("c0".into()));
        draft("tags", Value::Null, "art", cx);
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 3, "{sent:?}");
        assert_eq!(sent[2]["if_graph_commit"], "c1", "{}", sent[2]);
    }

    /// A cell saved on one branch lands after the window moved to another: it lets go of the
    /// draft of the branch it was saved on, never that of the branch shown, which stays. A save
    /// that lands, or is refused, after its draft was retyped elsewhere (on a node's page)
    /// leaves that draft as it is, and opens nothing over it.
    #[gpui::test]
    async fn a_save_that_lands_after_a_branch_switch_keeps_the_new_branchs_draft(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let session = session(PEOPLE, cx);
        let key = DraftKey {
            type_name: "Person".into(),
            id: "p1".into(),
            property: "name".into(),
        };
        let (table, cx) = cx.add_window_view({
            let session = session.clone();
            |window, cx| {
                TypeTable::new(
                    WeakEntity::new_invalid(),
                    session,
                    "Person".into(),
                    None,
                    window,
                    cx,
                )
            }
        });
        session.update(cx, |session, cx| {
            session.set_branch("fix".into(), cx);
            let draft = graph_store::DraftEntry {
                old: "Ada".into(),
                text: "Ada King".into(),
            };
            session.set_draft(key.clone(), draft, cx);
            session.set_branch("main".into(), cx);
        });
        cx.run_until_parked();
        table.update_in(cx, |table, window, cx| {
            let col = table.columns.iter().position(|c| c.key == "name").unwrap();
            table.start_edit(Cursor { row: 0, col }, window, cx);
            table
                .cell_editor
                .update(cx, |editor, cx| editor.set_text("Ada Lovelace", window, cx));
            table.confirm_edit(window, cx);
        });
        session.update(cx, |session, cx| session.set_branch("fix".into(), cx));
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            assert_eq!(session.branch, "fix");
            assert_eq!(
                session.draft(&key).map(|draft| draft.text.as_str()),
                Some("Ada King")
            );
        });
        table.read_with(cx, |table, _| assert_eq!(table.saves_in_flight, 0));

        session.update(cx, |session, cx| session.set_branch("main".into(), cx));
        cx.run_until_parked();
        let typed = graph_store::DraftEntry {
            old: "Ada".into(),
            text: "Ada Lovelace".into(),
        };
        let retyped = graph_store::DraftEntry {
            old: "Ada".into(),
            text: "Ada K.".into(),
        };
        let results = [
            SaveResult::Saved {
                commit: "c9".into(),
                parent: Some("c8".into()),
            },
            SaveResult::Refused(Notice::new(Tone::Refused, "Nothing was written.")),
        ];
        for result in results {
            session.update(cx, |session, cx| {
                session.set_draft(key.clone(), retyped.clone(), cx)
            });
            table.update_in(cx, |table, window, cx| {
                let cell = SavedCell {
                    row_id: "p1".into(),
                    column: "name".into(),
                    old: "Ada".into(),
                    new: "Ada Lovelace".into(),
                    text: "Ada Lovelace".into(),
                    shown: "Ada".into(),
                    sent: Some(typed.clone()),
                };
                table.saves_in_flight += 1;
                let session = table.session.clone();
                table.finish_save(cell, &session, "main", result, window, cx);
                assert!(table.editing.is_none(), "nothing opens over it");
            });
            session.read_with(cx, |session, _| {
                assert_eq!(session.draft(&key), Some(&retyped), "retyped meanwhile")
            });
        }
    }

    /// A conflict's re-read that comes back after the table moved to another branch keeps the
    /// typed text as a draft on the branch it was typed on, still bound to the value it was
    /// typed over: the value read there was never shown, so the next save finds the change
    /// rather than writing over it. A batch's drafts read again alike; the new branch is left
    /// alone.
    #[gpui::test]
    async fn a_conflict_read_again_after_a_branch_switch_stays_on_its_branch(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let session = session(PEOPLE, cx);
        let key = |id: &str| DraftKey {
            type_name: "Person".into(),
            id: id.into(),
            property: "name".into(),
        };
        let (table, cx) = cx.add_window_view({
            let session = session.clone();
            |window, cx| {
                TypeTable::new(
                    WeakEntity::new_invalid(),
                    session,
                    "Person".into(),
                    None,
                    window,
                    cx,
                )
            }
        });
        cx.run_until_parked();
        // The batch's draft, as the session held it when the batch went out; the cell had none.
        session.update(cx, |session, cx| {
            let draft = graph_store::DraftEntry {
                old: "Grace B".into(),
                text: "Grace Hopper".into(),
            };
            session.set_draft(key("p2"), draft, cx);
        });
        // Each row is read as holding neither the value typed over nor the one typed.
        table.update_in(cx, |table, window, cx| {
            let cell = SavedCell {
                row_id: "p1".into(),
                column: "name".into(),
                old: "Ada B".into(),
                new: "Ada Lovelace".into(),
                text: "Ada Lovelace".into(),
                shown: "Ada".into(),
                sent: None,
            };
            table.reread_conflict(cell, window, cx);
            let edit = DraftEdit {
                row_id: "p2".into(),
                column: "name".into(),
                old: "Grace B".into(),
                new: "Grace Hopper".into(),
                text: "Grace Hopper".into(),
                shown: "Grace".into(),
            };
            table.reread_drafts(vec![edit], window, cx);
        });
        session.update(cx, |session, cx| session.set_branch("fix".into(), cx));
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            let on_main: Vec<_> = session.drafts_on("main").collect();
            let entry = |old: &str, text: &str| graph_store::DraftEntry {
                old: old.into(),
                text: text.into(),
            };
            assert_eq!(
                on_main,
                vec![
                    (&key("p1"), &entry("Ada B", "Ada Lovelace")),
                    (&key("p2"), &entry("Grace B", "Grace Hopper")),
                ],
                "bound to the values they were typed over"
            );
            assert!(session.draft(&key("p1")).is_none(), "nothing on fix");
        });
        table.read_with(cx, |table, _| assert_eq!(table.saves_in_flight, 0));
    }

    /// A batch reads its rows again at the head in one read: a draft whose value is there landed
    /// and goes; one whose row holds another value now is kept, bound to it, which the cell
    /// shows. A draft retyped elsewhere while the read was out stands as it is.
    #[gpui::test]
    async fn a_batch_reads_its_rows_again_in_one_read(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, names) = crate::test_support::session_routing(
            PEOPLE,
            |request| match request["name"].as_str() {
                Some("studio_nodes_fields") => serde_json::json!([
                    { "id": "p1", "v_slug": "p1", "v_name": "Ada Lovelace" },
                    { "id": "p2", "v_slug": "p2", "v_name": "Grace B" },
                ]),
                _ => serde_json::json!([
                    {"n": {"@id": "p1", "slug": "p1", "name": "Ada"}},
                    {"n": {"@id": "p2", "slug": "p2", "name": "Grace"}},
                ]),
            },
            cx,
        );
        let key = |id: &str| DraftKey {
            type_name: "Person".into(),
            id: id.into(),
            property: "name".into(),
        };
        let (table, cx) = cx.add_window_view({
            let session = session.clone();
            |window, cx| {
                TypeTable::new(
                    WeakEntity::new_invalid(),
                    session,
                    "Person".into(),
                    None,
                    window,
                    cx,
                )
            }
        });
        cx.run_until_parked();
        let edit = |id: &str, old: &str, new: &str| DraftEdit {
            row_id: id.into(),
            column: "name".into(),
            old: old.into(),
            new: new.into(),
            text: new.into(),
            shown: old.into(),
        };
        let set_draft = |cx: &mut VisualTestContext, id: &str, old: &str, text: &str| {
            session.update(cx, |session, cx| {
                let draft = DraftEntry {
                    old: old.into(),
                    text: text.into(),
                };
                session.set_draft(key(id), draft, cx);
            });
        };
        // The drafts as the batch sent them.
        set_draft(cx, "p1", "Ada", "Ada Lovelace");
        set_draft(cx, "p2", "Grace", "Grace Hopper");
        table.update_in(cx, |table, window, cx| {
            let edits = vec![
                edit("p1", "Ada", "Ada Lovelace"),
                edit("p2", "Grace", "Grace Hopper"),
            ];
            table.reread_drafts(edits, window, cx);
        });
        cx.run_until_parked();
        let rows_read = names
            .lock()
            .unwrap()
            .iter()
            .filter(|name| *name == "studio_nodes_fields")
            .count();
        assert_eq!(rows_read, 1, "one read for the batch");
        session.read_with(cx, |session, _| {
            assert!(session.draft(&key("p1")).is_none(), "landed");
            assert_eq!(
                session.draft(&key("p2")),
                Some(&graph_store::DraftEntry {
                    old: "Grace B".into(),
                    text: "Grace Hopper".into(),
                }),
                "bound to what the row holds now"
            );
        });
        table.read_with(cx, |table, _| {
            assert_eq!(table.rows[1].value("name"), "Grace B");
            assert!(
                table
                    .draft_notices
                    .contains_key(&("p2".to_owned(), "name".to_owned()))
            );
            assert_eq!(table.saves_in_flight, 0);
        });

        // Retyped elsewhere (on a node's page, in the peek) while the read was out, a draft
        // stands as it is, whether its row holds the value it was sent asking for or another.
        set_draft(cx, "p1", "Ada", "Ada King");
        set_draft(cx, "p2", "Grace B", "Grace H.");
        table.update_in(cx, |table, window, cx| {
            let edits = vec![
                edit("p1", "Ada", "Ada Lovelace"),
                edit("p2", "Grace B", "Grace Hopper"),
            ];
            table.reread_drafts(edits, window, cx);
        });
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            let text = |id: &str| session.draft(&key(id)).map(|draft| draft.text.clone());
            assert_eq!(text("p1").as_deref(), Some("Ada King"), "not let go");
            assert_eq!(text("p2").as_deref(), Some("Grace H."), "not typed over");
        });
        table.read_with(cx, |table, _| {
            assert_eq!(table.draft_text("p1", "name").as_deref(), Some("Ada King"));
            assert_eq!(table.draft_text("p2", "name").as_deref(), Some("Grace H."));
            assert_eq!(table.saves_in_flight, 0);
        });
    }

    /// A save that does not land puts back what the row showed, not the value a draft was typed
    /// over before a Refresh: shown as read at the pin, that value would bind a list's next save
    /// to the pin, over the newer one. The draft stays bound to the value it was typed over.
    #[gpui::test]
    async fn a_refused_save_puts_back_what_the_row_showed(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let refused = serde_json::json!({ "error": "policy denied", "code": "forbidden", "disposition": { "action": "refresh", "effects": "none" } });
        let session = crate::test_support::session_answering(PEOPLE, 403, refused, cx);
        let (table, cx) = cx.add_window_view({
            let session = session.clone();
            |window, cx| {
                TypeTable::new(
                    WeakEntity::new_invalid(),
                    session,
                    "Person".into(),
                    None,
                    window,
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
        // Typed over "Ada B"; the row shows "Ada", read since.
        session.update(cx, |session, cx| {
            let draft = DraftEntry {
                old: "Ada B".into(),
                text: "Ada King".into(),
            };
            session.set_draft(key.clone(), draft, cx);
        });
        cx.run_until_parked();
        let settled = |cx: &mut VisualTestContext| {
            table.read_with(cx, |table, _| {
                assert_eq!(table.rows[0].value("name"), "Ada", "what the row showed");
                assert_eq!(table.saves_in_flight, 0);
            });
            let old =
                session.read_with(cx, |session, _| session.draft(&key).map(|d| d.old.clone()));
            assert_eq!(
                old,
                Some("Ada B".into()),
                "still bound to what it was typed over"
            );
        };
        table.update_in(cx, |table, window, cx| {
            let col = table.column_index("name").unwrap();
            table.start_edit(Cursor { row: 0, col }, window, cx);
            table.confirm_edit(window, cx);
            assert_eq!(
                table.rows[0].value("name"),
                "Ada King",
                "while the save is out"
            );
        });
        cx.run_until_parked();
        settled(cx);
        // ⌘S alike.
        table.update_in(cx, |table, window, cx| table.save_drafts(window, cx));
        cx.run_until_parked();
        settled(cx);
    }

    /// Grouped by a relationship, a row shows under each of its neighbours; a bulk set writes
    /// it once, so the commit's count is the rows set, not a conflict.
    #[gpui::test]
    async fn a_bulk_set_writes_a_row_shown_twice_once(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let mut both = crate::test_support::committed("c2", "c1");
        both["affected_nodes"] = 2.into();
        let session = crate::test_support::session_writing(PEOPLE, vec![(200, both)], cx);
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::new(
                WeakEntity::new_invalid(),
                session,
                "Person".into(),
                None,
                window,
                cx,
            )
        });
        table.update_in(cx, |table, window, cx| {
            let copy = table.rows[0].clone();
            table.rows.push(copy);
            table.selection.extend(["p1".to_owned(), "p2".to_owned()]);
            table.bulk.property = Some("name".into());
            table
                .bulk_editor
                .update(cx, |editor, cx| editor.set_text("Ida", window, cx));
            table.submit_bulk(window, cx);
        });
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert!(table.bulk.notice.is_none(), "{:?}", table.bulk.notice);
            assert!(table.rows.iter().all(|row| row.value("name") == "Ida"));
        });
    }

    /// A row that goes leaves the selection, and the row a range starts from stays put.
    #[gpui::test]
    async fn a_removed_row_leaves_the_selection(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let session = session(PEOPLE, cx);
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::new(
                WeakEntity::new_invalid(),
                session,
                "Person".into(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        table.update(cx, |table, cx| {
            table.selection.extend(["p1".to_owned(), "p2".to_owned()]);
            table.selection_anchor = Some(1);
            table.remove_rows(&["p1".to_owned()], cx);
            assert_eq!(table.selection.iter().collect::<Vec<_>>(), ["p2"]);
            assert_eq!(table.selection_anchor, Some(0), "still on p2");
            table.remove_rows(&["p2".to_owned()], cx);
            assert!(table.selection.is_empty() && table.selection_anchor.is_none());
        });
    }

    /// Rows deleted together leave in one pass: the cursor stays on its row, and a deleted row
    /// the table never loaded still leaves the selection.
    #[gpui::test]
    async fn rows_deleted_together_leave_in_one_pass(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let session = session(PEOPLE, cx);
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::new(
                WeakEntity::new_invalid(),
                session,
                "Person".into(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        table.update(cx, |table, cx| {
            assert_eq!(table.rows.len(), 2);
            table.cursor = Some(Cursor { row: 1, col: 0 });
            table
                .selection
                .extend(["p2".to_owned(), "unloaded".to_owned()]);
            table.remove_rows(&["p1".to_owned(), "unloaded".to_owned()], cx);
            let ids: Vec<&str> = table.rows.iter().map(|row| row.id.as_str()).collect();
            assert_eq!(ids, ["p2"]);
            assert_eq!(
                table.cursor.map(|c| c.row),
                Some(0),
                "the cursor stays on p2"
            );
            assert_eq!(table.selection.iter().collect::<Vec<_>>(), ["p2"]);
            table.remove_rows(&["p2".to_owned()], cx);
            assert!(table.rows.is_empty() && table.cursor.is_none());
        });
    }

    /// A saved view brings its filters, order and columns; the widths the tab's columns were
    /// dragged to stay, being the tab's own.
    #[gpui::test]
    async fn a_saved_view_keeps_the_tabs_column_widths(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let session = session(PEOPLE, cx);
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::new(
                WeakEntity::new_invalid(),
                session,
                "Person".into(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        table.update(cx, |table, cx| {
            table.layout.set_width("name", Some(300.));
            let view = studio_settings::SavedView {
                name: "v".into(),
                type_name: "Person".into(),
                hidden: vec!["slug".into()],
                ..Default::default()
            };
            table.apply_view(&view, cx);
            assert_eq!(table.layout.hidden, ["slug"]);
            assert_eq!(table.layout.widths.get("name"), Some(&300));
        });
    }

    /// A restored tab sets its order and grouping before the schema is read: they wait for it,
    /// and stay where the schema allows them.
    #[gpui::test]
    async fn an_order_or_grouping_set_before_the_schema_waits_for_it(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let session = session(PEOPLE, cx);
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::new(
                WeakEntity::new_invalid(),
                session,
                "Person".into(),
                None,
                window,
                cx,
            )
        });
        table.update(cx, |table, cx| {
            // As before the session's schema was read.
            table.schema = None;
            table.all_columns.clear();
            table.grouping_choices = Default::default();
            table.set_sort(
                Some(Sort {
                    property: "name".into(),
                    descending: true,
                }),
                cx,
            );
            table.set_grouping(Some("gone".into()), cx);
            assert!(
                table.sort.is_some() && table.grouping.is_some(),
                "kept as asked"
            );
            table.load_schema(cx);
        });
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert_eq!(
                table
                    .sort
                    .as_ref()
                    .map(|sort| (sort.property.as_str(), sort.descending)),
                Some(("name", true))
            );
            assert!(table.grouping.is_none(), "the schema has no such property");
        });
    }

    /// At a commit made before a property was added, the rows are refused, naming it: the
    /// table reads them again without it, and its header says so.
    #[gpui::test]
    async fn a_table_at_a_commit_before_a_property_was_added_reads_the_rest(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let schema = PEOPLE.replace("  name: String\n", "  name: String\n  born: Date?\n");
        let (session, names) = crate::test_support::session_routing(
            &schema,
            |request| match request["name"].as_str() {
                Some("studio_page") => serde_json::json!({
                    "refuse": "property 'born' of node type 'Person' does not exist at this snapshot"
                }),
                Some("studio_page_fields") => serde_json::json!([
                    { "id": "p1", "v_slug": "p1", "v_name": "Ada" },
                    { "id": "p2", "v_slug": "p2", "v_name": "Grace" },
                ]),
                _ => serde_json::json!([]),
            },
            cx,
        );
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::new(
                WeakEntity::new_invalid(),
                session,
                "Person".into(),
                Some("c0".into()),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert_eq!(table.rows.len(), 2);
            assert_eq!(
                table.rows[0].values.get("name"),
                Some(&serde_json::json!("Ada"))
            );
            assert_eq!(table.absent.get("born"), Some(&PropertyThen::Absent));
            assert!(matches!(table.first_page, Loadable::Loaded(())));
        });
        let read = names.lock().unwrap().clone();
        assert_eq!(
            read.iter()
                .filter(|n| n.starts_with("studio_page"))
                .collect::<Vec<_>>(),
            ["studio_page", "studio_page_fields"]
        );

        // Sorted and filtered by it there, no row held it: the table answers its page and its
        // count of 0 without a read.
        let reads_before = read.len();
        table.update(cx, |table, cx| {
            table.sort = Some(builder::Sort {
                property: "born".into(),
                descending: false,
            });
            table.filters = vec![Filter {
                property: "born".into(),
                op: FilterOp::Gt,
                value: serde_json::json!("1815-12-10"),
            }];
            table.reload(true, cx);
        });
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert!(matches!(table.first_page, Loadable::Loaded(())));
            assert_eq!(table.count, Loadable::Loaded(0));
        });
        let names = names.lock().unwrap().clone();
        let after = &names[reads_before..];
        assert!(
            after.iter().all(|name| !name.starts_with("studio_page")),
            "{after:?}"
        );
        assert!(
            !after.iter().any(|name| name == "studio_count"),
            "{after:?}"
        );
    }

    /// Grouped at a commit made before a property was added, its sections are refused for it
    /// as the ungrouped rows are, and read again without it; filtered by it there, no row holds
    /// it, and the groups are counted without a read.
    #[gpui::test]
    async fn a_grouped_table_at_a_commit_before_a_property_was_added_reads_the_rest(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let schema = PEOPLE.replace("  name: String\n", "  name: String\n  born: Date?\n");
        let (session, names) = crate::test_support::session_routing(
            &schema,
            |request| match request["name"].as_str() {
                Some("studio_value_counts") => serde_json::json!([
                    {"value": "Ada", "n": 1},
                    {"value": "Grace", "n": 1},
                ]),
                Some("studio_page") => serde_json::json!({
                    "refuse": "property 'born' of node type 'Person' does not exist at this snapshot"
                }),
                Some("studio_page_fields") => serde_json::json!([
                    { "id": "p1", "v_slug": "p1", "v_name": "Ada" },
                ]),
                _ => serde_json::json!([]),
            },
            cx,
        );
        cx.run_until_parked();
        let opening = Opening {
            group_by: Some("name".into()),
            ..Opening::default()
        };
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::opening(
                WeakEntity::new_invalid(),
                session,
                "Person".into(),
                Some("c0".into()),
                opening,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert_eq!(table.absent.get("born"), Some(&PropertyThen::Absent));
            assert!(matches!(table.first_page, Loadable::Loaded(())));
            let sections = table.grouping.as_ref().unwrap().sections();
            assert!(!sections.is_empty());
            assert!(
                sections.iter().all(|section| section.error.is_none()),
                "no section shows the refusal"
            );
        });
        assert!(
            names
                .lock()
                .unwrap()
                .iter()
                .any(|name| name == "studio_page_fields"),
            "the sections were read without it"
        );

        let before = names.lock().unwrap().len();
        table.update(cx, |table, cx| {
            table.filters = vec![Filter {
                property: "born".into(),
                op: FilterOp::Gt,
                value: serde_json::json!("1815-12-10"),
            }];
            table.reload(true, cx);
        });
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert!(matches!(table.first_page, Loadable::Loaded(())));
            assert!(table.grouping.as_ref().unwrap().sections().is_empty());
            assert_eq!(table.count, Loadable::Loaded(0));
        });
        let names = names.lock().unwrap();
        assert!(
            !names[before..]
                .iter()
                .any(|name| name == "studio_value_counts"),
            "{names:?}"
        );
    }

    /// A sideways wheel over the rows moves the columns, not the rows, after a key as after the
    /// pointer, draws the grid again, and leaves the columns where it put them when the table
    /// draws again. A trackpad gesture keeps its axis while it drifts, and a wheel over the open
    /// cell editor scrolls the editor's text (spec 004).
    #[gpui::test]
    async fn a_sideways_wheel_scrolls_the_columns_not_the_rows(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let edges: String = (1..=20)
            .map(|i| format!("edge R{i:02}: Person -> Person\n"))
            .collect();
        let schema = format!("{PEOPLE}{edges}");
        let rows: Vec<serde_json::Value> = (0..60)
            .map(|i| {
                // The first name runs past its cell, so its editor has text to scroll to.
                let name = if i == 0 { "Person 0 ".repeat(40) } else { format!("Person {i}") };
                serde_json::json!({"n": {"@id": format!("p{i}"), "slug": format!("p{i}"), "name": name}})
            })
            .collect();
        let (session, _) = crate::test_support::session_routing(
            &schema,
            move |request| match request["name"].as_str() {
                Some("studio_page_degrees") => serde_json::json!([]),
                _ => serde_json::Value::Array(rows.clone()),
            },
            cx,
        );
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::new(
                WeakEntity::new_invalid(),
                session.clone(),
                "Person".into(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        let offsets = |cx: &mut gpui::VisualTestContext| {
            table.read_with(cx, |table, cx| {
                let state = table.table_state.read(cx);
                (
                    state.horizontal_scroll_handle.offset().x,
                    state.scroll_handle.0.borrow().base_handle.offset().y,
                )
            })
        };
        assert_eq!(offsets(cx), (px(0.), px(0.)));
        let wheel = |at: gpui::Point<Pixels>, (x, y): (f32, f32), touch_phase| {
            gpui::PlatformInput::Scroll(gpui::ScrollEvent {
                position: at,
                delta: gpui::ScrollDelta::Pixels(point(px(x), px(y))),
                modifiers: gpui::Modifiers::default(),
                touch_phase,
                source: gpui::ScrollSource::Pointer,
            })
        };
        let over_rows = point(px(400.), px(300.));
        // A key leaves the pointer's hover false; the wheel is still the rows' area's.
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        let drawn = grid_renders(&table, cx);
        cx.update(|window, cx| {
            window.dispatch_event(wheel(over_rows, (-120., 0.), gpui::TouchPhase::Moved), cx);
        });
        cx.run_until_parked();
        let (x, y) = offsets(cx);
        assert_eq!(y, px(0.), "the rows did not move");
        assert_eq!(x, px(-120.), "the columns moved by the wheel's width");
        assert!(grid_renders(&table, cx) > drawn, "the grid drew the move");
        // Drawn again, they stay.
        table.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!(
            offsets(cx).0,
            px(-120.),
            "a draw keeps the columns where they were"
        );

        // A gesture that starts sideways and drifts down moves only the columns; one that
        // starts down and drifts sideways only the rows. Each gesture's events are sent
        // together, as a trackpad sends them, within the gap after which GPUI starts a new
        // gesture.
        let gesture = |cx: &mut gpui::VisualTestContext,
                       events: [((f32, f32), gpui::TouchPhase); 2]| {
            cx.update(|window, cx| {
                for (delta, phase) in events {
                    window.dispatch_event(wheel(over_rows, delta, phase), cx);
                }
            });
            cx.run_until_parked();
        };
        use gpui::TouchPhase::{Moved, Started};
        gesture(cx, [((-30., 0.), Started), ((-10., -15.), Moved)]);
        assert_eq!(offsets(cx), (px(-160.), px(0.)), "held sideways");
        gesture(cx, [((0., -40.), Started), ((-30., -20.), Moved)]);
        assert_eq!(offsets(cx), (px(-160.), px(-60.)), "held up and down");
        // A sideways jitter too small to turn the gesture, which the list would take as a
        // move down.
        gesture(cx, [((0., -10.), Started), ((-4., 0.), Moved)]);
        assert_eq!(
            offsets(cx),
            (px(-160.), px(-70.)),
            "a small sideways drift moves nothing"
        );

        // Over the open cell editor the wheel scrolls the editor's text.
        table.update_in(cx, |table, window, cx| {
            let state = table.table_state.read(cx);
            state
                .horizontal_scroll_handle
                .set_offset(point(px(0.), px(0.)));
            state
                .scroll_handle
                .0
                .borrow()
                .base_handle
                .set_offset(point(px(0.), px(0.)));
            let col = table.columns.iter().position(|c| c.key == "name").unwrap();
            table.start_edit(Cursor { row: 0, col }, window, cx);
        });
        cx.run_until_parked();
        let editor = table
            .read_with(cx, |table, _| table.edit_slot.shown())
            .expect("the editor's cell is drawn");
        let editor_x = |cx: &mut gpui::VisualTestContext| {
            table.update(cx, |table, cx| {
                table
                    .cell_editor
                    .update(cx, |editor, cx| editor.scroll_position(cx).x)
            })
        };
        assert_eq!(editor_x(cx), 0.);
        cx.update(|window, cx| {
            window.dispatch_event(
                wheel(editor.center(), (-60., 0.), gpui::TouchPhase::Moved),
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(offsets(cx), (px(0.), px(0.)), "the columns did not move");
        assert!(editor_x(cx) > 0., "the editor scrolled");
    }

    /// A type with many relationships reads the neighbours of the relationship columns rows
    /// draw, and of the others as they scroll into view, once each (spec 015).
    #[gpui::test]
    async fn relationship_columns_read_while_drawn(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let edges: String = (1..=20)
            .map(|i| format!("edge R{i:02}: Person -> Person\n"))
            .collect();
        let schema = format!("{PEOPLE}{edges}");
        let (session, names) = crate::test_support::session_routing(
            &schema,
            |request| match request["name"].as_str() {
                Some("studio_page_degrees") => serde_json::json!([]),
                _ => serde_json::json!([
                    {"n": {"@id": "p1", "slug": "p1", "name": "Ada"}},
                    {"n": {"@id": "p2", "slug": "p2", "name": "Grace"}},
                ]),
            },
            cx,
        );
        let degree_reads = move || {
            names
                .lock()
                .unwrap()
                .iter()
                .filter(|name| *name == "studio_page_degrees")
                .count()
        };
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::new(
                WeakEntity::new_invalid(),
                session.clone(),
                "Person".into(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        let read_for = |cx: &mut gpui::VisualTestContext| {
            table.read_with(cx, |table, _| {
                let mut keys: Vec<String> = table
                    .relations_asked
                    .iter()
                    .map(|(_, key)| key.clone())
                    .collect();
                keys.sort();
                keys.dedup();
                keys
            })
        };
        let first = read_for(cx);
        assert!(!first.is_empty(), "the columns in view are read");
        assert!(first.len() < 20, "not every relationship: {first:?}");
        assert_eq!(degree_reads(), first.len(), "one read per column drawn");
        let last = table.read_with(cx, |table, _| table.columns.last().unwrap().key.clone());
        assert!(!first.contains(&last));

        // Scrolled to the far end: the last relationship is read, and none is read twice.
        table.update_in(cx, |table, _, cx| {
            let total = table.width_from(1);
            let handle = table.table_state.read(cx).horizontal_scroll_handle.clone();
            let viewport = handle.bounds().size.width;
            handle.set_offset(point(-(total - viewport), px(0.)));
            cx.notify();
        });
        cx.run_until_parked();
        let read = read_for(cx);
        assert!(read.contains(&last));
        assert_eq!(degree_reads(), read.len(), "none read twice");

        // Far down a long table, a column brought into view reads the rows near those drawn.
        table.update(cx, |table, cx| {
            table.rows = (0..1000)
                .map(|i| Row {
                    id: format!("q{i}"),
                    values: Default::default(),
                    dimmed: false,
                })
                .collect();
            table.lines_drawn = 500..520;
            table.relations_asked.clear();
            table.load_shown_relations(cx);
            let asked: HashSet<&str> = table
                .relations_asked
                .iter()
                .map(|(id, _)| id.as_str())
                .collect();
            let near = RELATION_READ_CHUNK;
            assert!(asked.contains(format!("q{}", 500 - near).as_str()));
            assert!(asked.contains(format!("q{}", 520 + near - 1).as_str()));
            assert!(!asked.contains(format!("q{}", 500 - near - 1).as_str()));
            assert!(!asked.contains("q0") && !asked.contains("q999"));
        });
    }

    /// People who work at companies: one relationship column, `WorksAt`, on Person.
    const WORKERS: &str = "node Person {\n  slug: String @key\n  name: String\n}\nnode Company {\n  slug: String @key\n  name: String\n}\nedge WorksAt: Person -> Company\n";

    /// What a fake server answers a page's relationship reads with: its two people, how many
    /// companies each works at (`degrees`), and those companies, `c0` on, for each row the read
    /// names, in its order. A read naming `cut` answers with `cut`'s first 2,000 companies
    /// alone, as the limit of a read shared with a row of more ends it.
    fn workers(degrees: &[(&str, u64)], cut: Option<&str>) -> impl Fn(&Value) -> Value + use<> {
        let degrees: Vec<(String, u64)> = degrees
            .iter()
            .map(|(id, n)| ((*id).to_owned(), *n))
            .collect();
        let cut = cut.map(str::to_owned);
        fn listed(id: &str, n: u64) -> Vec<Value> {
            (0..n)
                .map(|i| {
                    serde_json::json!({"id": id, "other": format!("c{i}"), "label": format!("Company {i}")})
                })
                .collect()
        }
        move |request| match request["name"].as_str() {
            Some("studio_page_degrees") => degrees
                .iter()
                .map(|(id, n)| serde_json::json!({"id": id, "n": n}))
                .collect(),
            Some("studio_page_neighbours") => {
                let asked: Vec<&str> = request["params"]["ids"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                match &cut {
                    Some(hub) if asked.contains(&hub.as_str()) => listed(hub, 2000).into(),
                    _ => degrees
                        .iter()
                        .filter(|(id, _)| asked.contains(&id.as_str()))
                        .flat_map(|(id, n)| listed(id, *n))
                        .collect(),
                }
            }
            _ => serde_json::json!([
                {"n": {"@id": "p1", "slug": "p1", "name": "Ada"}},
                {"n": {"@id": "p2", "slug": "p2", "name": "Grace"}},
            ]),
        }
    }

    /// The key of a table's first relationship column.
    fn relationship_column(table: &Entity<TypeTable>, cx: &mut VisualTestContext) -> String {
        table.read_with(cx, |table, _| {
            let column = table.columns.iter().find(|c| c.relationship().is_some());
            column.expect("a relationship column").key.clone()
        })
    }

    /// A page's relationship counts and its neighbours are asked for together: each row's
    /// count shows as soon as the counts are in, and its chips once the neighbours are, with no
    /// further read. Before, the neighbours were asked for only once the counts were in, and
    /// neither showed until both were.
    #[gpui::test]
    async fn relationship_counts_show_before_the_neighbours_asked_for_with_them(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let answer = workers(&[("p1", 2), ("p2", 1)], None);
        let (session, _, served) = crate::test_support::session_served(WORKERS, answer, cx);
        let degrees = served.hold(|read| read["name"] == "studio_page_degrees");
        let neighbours = served.hold(|read| read["name"] == "studio_page_neighbours");
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::new(
                WeakEntity::new_invalid(),
                session,
                "Person".into(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        let key = relationship_column(&table, cx);
        // Both are out, the neighbours of every row of the page, while no count is in.
        assert_eq!(served.sent("studio_page_degrees").len(), 1);
        let sent = served.sent("studio_page_neighbours");
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0]["params"]["ids"], serde_json::json!(["p1", "p2"]));
        table.read_with(cx, |table, _| {
            assert!(table.relations.get("p1", &key).is_none())
        });

        // The counts are in: they show, the chips still to come.
        degrees.send(()).unwrap();
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            let ada = table.relations.get("p1", &key).unwrap();
            assert_eq!(ada.count, Ok(2));
            assert!(ada.neighbours.is_empty() && !ada.whole);
            assert_eq!(table.relations.get("p2", &key).unwrap().count, Ok(1));
        });

        neighbours.send(()).unwrap();
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            let ada = table.relations.get("p1", &key).unwrap();
            let named: Vec<&str> = ada.neighbours.iter().map(|(id, _)| id.as_str()).collect();
            assert_eq!(named, ["c0", "c1"]);
            assert!(ada.whole);
            let grace = table.relations.get("p2", &key).unwrap();
            assert_eq!(grace.neighbours.len(), 1);
            assert!(grace.whole);
            assert!(table.relation_tasks.is_empty());
        });
        assert_eq!(
            served.sent("studio_page_neighbours").len(),
            1,
            "none read again"
        );
    }

    /// A row the first neighbour read cut short, after a hub that spent its limit, is read
    /// again alone, sized by its count, so it shows its chips; the hub shows its count alone.
    #[gpui::test]
    async fn a_row_a_hub_left_bare_is_read_again_sized_by_its_count(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let answer = workers(&[("p1", 5000), ("p2", 3)], Some("p1"));
        let (session, _, served) = crate::test_support::session_served(WORKERS, answer, cx);
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::new(
                WeakEntity::new_invalid(),
                session,
                "Person".into(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        let key = relationship_column(&table, cx);
        let sent = served.sent("studio_page_neighbours");
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert_eq!(sent[1]["params"]["ids"], serde_json::json!(["p2"]));
        let query = sent[1]["query"].as_str().unwrap_or_default();
        assert!(query.contains("limit 3\n"), "{query}");
        table.read_with(cx, |table, _| {
            let hub = table.relations.get("p1", &key).unwrap();
            assert_eq!(hub.count, Ok(5000));
            assert!(hub.neighbours.is_empty() && !hub.whole, "its count alone");
            let grace = table.relations.get("p2", &key).unwrap();
            assert_eq!(grace.neighbours.len(), 3);
            assert!(grace.whole);
            assert!(table.relation_tasks.is_empty());
        });
    }

    /// A row draws the columns in view, one either side, the pinned first and the cursor's;
    /// the rest are empty at their width.
    #[gpui::test]
    async fn rows_draw_only_the_columns_in_view(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let properties: String = (1..=20).map(|i| format!("  p{i:02}: String?\n")).collect();
        let schema = format!("node Person {{\n  slug: String @key\n{properties}}}\n");
        let session = session(&schema, cx);
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::new(
                WeakEntity::new_invalid(),
                session,
                "Person".into(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        let drawn = |cx: &mut gpui::VisualTestContext| {
            table.update_in(cx, |table, window, cx| table.drawn_columns(window, cx))
        };
        let shown = drawn(cx);
        assert_eq!(shown.len(), 21);
        assert!(
            shown[0] && shown[1],
            "the pinned column and the first in view"
        );
        assert!(!shown[20], "a column far right of the view: {shown:?}");

        // Scrolled to the far end, the last column shows and the first after the pinned
        // one does not.
        table.update_in(cx, |table, _, cx| {
            let total = table.width_from(1);
            let handle = table.table_state.read(cx).horizontal_scroll_handle.clone();
            let viewport = handle.bounds().size.width;
            handle.set_offset(point(-(total - viewport), px(0.)));
        });
        let shown = drawn(cx);
        assert!(shown[0] && shown[20], "{shown:?}");
        assert!(!shown[1], "{shown:?}");

        // The cursor's column is drawn wherever it is.
        table.update_in(cx, |table, _, _| {
            table.cursor = Some(Cursor { row: 0, col: 1 })
        });
        assert!(drawn(cx)[1]);

        // A wider grid draws the columns its new width shows in the frame that widens it: the
        // header learns the width before the rows are picked (`drawn_columns`).
        table.update_in(cx, |table, _, cx| {
            table.cursor = None;
            table
                .table_state
                .read(cx)
                .horizontal_scroll_handle
                .set_offset(point(px(0.), px(0.)));
            cx.notify();
        });
        cx.run_until_parked();
        let rows_drew = |cx: &mut gpui::VisualTestContext| {
            table.read_with(cx, |table, _| table.frame_drawn.clone())
        };
        assert!(!rows_drew(cx)[20], "{:?}", rows_drew(cx));
        // A line lays out the drawn columns and one gap for the rest, as wide as they are, so
        // the grid scrolls as far as before.
        table.read_with(cx, |table, _| {
            let places = &table.frame_places;
            assert!(places.len() < table.columns.len(), "{places:?}");
            assert!(
                matches!(places.last(), Some(GridPlace::Gap(_))),
                "{places:?}"
            );
            let width = |place: &GridPlace| match *place {
                GridPlace::Column(col) => table.layout.width(&table.columns[col]),
                GridPlace::Gap(width) => f32::from(width),
            };
            let drawn: f32 = places.iter().map(width).sum();
            let all: f32 = table.columns.iter().map(|c| table.layout.width(c)).sum();
            assert!((drawn - all).abs() < 0.5, "{drawn} of {all}");
        });
        cx.simulate_resize(gpui::size(px(6000.), px(800.)));
        assert!(
            rows_drew(cx).iter().all(|drawn| *drawn),
            "every column fits now: {:?}",
            rows_drew(cx)
        );
    }

    /// A row put in or taken out moves the rows after it: the places drawn hold other rows
    /// then, which read their relationship and Blob cells. A delete reads the relationships of
    /// the rows still shown again, and a new row reads its own.
    #[gpui::test]
    async fn rows_moved_by_an_insert_or_a_delete_read_their_cells(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let schema = PEOPLE.replace("  name: String\n", "  name: String\n  doc: Blob?\n");
        let schema = format!("{schema}edge Knows: Person -> Person\n");
        let people: Vec<Value> = (0..200)
            .map(|i| {
                let id = format!("p{i:03}");
                serde_json::json!({"n": {"@id": id, "slug": id, "name": "Ada"}})
            })
            .collect();
        let (session, _) = crate::test_support::session_routing(
            &schema,
            move |request| match request["name"].as_str() {
                Some("studio_page") => Value::from(people.clone()),
                _ => serde_json::json!([]),
            },
            cx,
        );
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::new(
                WeakEntity::new_invalid(),
                session,
                "Person".into(),
                None,
                window,
                cx,
            )
        });
        let settle = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            cx.executor().advance_clock(BLOB_READ_AFTER);
            cx.run_until_parked();
        };
        // Every row drawn has its relationship cells read, and its Blob cell asked for.
        let drawn_read = |cx: &mut VisualTestContext| {
            table.read_with(cx, |table, _| {
                let drawn = table
                    .rows
                    .get(table.lines_drawn.clone())
                    .unwrap_or_default();
                assert!(
                    !drawn.is_empty() && drawn.len() < table.rows.len(),
                    "{:?}",
                    table.lines_drawn
                );
                assert!(!table.relations_drawn.is_empty());
                drawn.iter().all(|row| {
                    let read = |key: &String| table.relations.contains(&row.id, key);
                    table.relations_drawn.iter().all(read) && table.blobs.contains(&row.id, "doc")
                })
            })
        };
        settle(cx);
        assert!(drawn_read(cx), "the rows drawn first");

        table.update(cx, |table, cx| {
            table.rows_deleted("main", &["p000".to_owned()], "c2", Some("c1"), cx)
        });
        settle(cx);
        assert!(
            drawn_read(cx),
            "the row that came into view, and the others again"
        );

        let values =
            serde_json::from_value(serde_json::json!({"slug": "p0005", "name": "Ida"})).unwrap();
        table.update(cx, |table, cx| {
            table.row_inserted("main", Some("p0005".into()), values, "c3", Some("c2"), cx)
        });
        settle(cx);
        table.read_with(cx, |table, _| assert_eq!(table.rows[0].id, "p0005"));
        assert!(drawn_read(cx), "the new row");
    }

    /// The table's own insert and delete read its counts again at their commit: a row the table
    /// does not show yet counts, as one deleted without being loaded does.
    #[gpui::test]
    async fn own_inserts_and_deletes_read_the_counts_again(cx: &mut TestAppContext) {
        use std::sync::atomic::{AtomicI64, Ordering};
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let count = Arc::new(AtomicI64::new(2));
        let counted = count.clone();
        let (session, _) = crate::test_support::session_routing(
            PEOPLE,
            move |request| match request["name"].as_str() {
                Some("studio_count") => serde_json::json!([{"n": counted.load(Ordering::SeqCst)}]),
                _ => serde_json::json!([
                    {"n": {"@id": "p1", "slug": "p1", "name": "Ada"}},
                    {"n": {"@id": "p2", "slug": "p2", "name": "Grace"}},
                ]),
            },
            cx,
        );
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::new(
                WeakEntity::new_invalid(),
                session,
                "Person".into(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        table.read_with(cx, |table, _| assert_eq!(table.count, Loadable::Loaded(2)));

        count.store(3, Ordering::SeqCst);
        let values = serde_json::from_value(serde_json::json!({"name": "Ida"})).unwrap();
        // The engine gives the row its id: only a read shows it.
        table.update(cx, |table, cx| {
            table.row_inserted("main", None, values, "c2", Some("c1"), cx)
        });
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert_eq!(table.rows.len(), 2, "not shown");
            assert_eq!(table.count, Loadable::Loaded(3), "counted");
        });

        count.store(1, Ordering::SeqCst);
        table.update(cx, |table, cx| {
            let ids = ["p1".to_owned(), "p9".to_owned()];
            table.rows_deleted("main", &ids, "c3", Some("c2"), cx)
        });
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert_eq!(table.paging.pinned.as_deref(), Some("c3"));
            assert_eq!(table.count, Loadable::Loaded(1));
        });

        // Made on a commit past the pin, a read at the pin would not hold them: the counts
        // follow them here rather than being read again there.
        count.store(99, Ordering::SeqCst);
        let values = serde_json::from_value(serde_json::json!({"name": "Ida"})).unwrap();
        table.update(cx, |table, cx| {
            table.row_inserted("main", None, values, "c5", Some("c4"), cx)
        });
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert_eq!(table.paging.pinned.as_deref(), Some("c3"), "behind");
            assert_eq!(table.count, Loadable::Loaded(2), "counted here");
        });
        table.update(cx, |table, cx| {
            table.rows_deleted("main", &["p2".to_owned()], "c6", Some("c5"), cx)
        });
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert!(table.newer);
            assert_eq!(table.count, Loadable::Loaded(1), "counted here");
        });

        // A count read at the pin does not hold them either, one out when another lands
        // included: they are added to it as it lands.
        count.store(1, Ordering::SeqCst);
        let values = serde_json::from_value(serde_json::json!({"name": "Ida"})).unwrap();
        table.update(cx, |table, cx| {
            table.load_counts(cx);
            table.row_inserted("main", None, values, "c7", Some("c6"), cx);
        });
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert_eq!(
                table.count,
                Loadable::Loaded(2),
                "the read out, with the insert"
            );
        });
        table.update(cx, |table, cx| table.load_counts(cx));
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert_eq!(
                table.count,
                Loadable::Loaded(2),
                "a read after, with all three"
            );
        });
    }

    /// A table at the head the session holds shows the session's snapshot count of every row
    /// with its first page, until the read at its pin lands and replaces it, when the session
    /// could not bind the snapshot to a commit. Here another writer's commit lands between the
    /// session's snapshot and its reads of the head, so the snapshot counts the commit before
    /// the pin and the newest commit is at another manifest version: the snapshot names no
    /// commit, the read at the pin corrects its count, and Refresh reads it there again.
    /// Filtered, the count through the filter is read too. A table held at a commit shows no
    /// snapshot count while its read is out.
    #[gpui::test]
    async fn a_tables_snapshot_count_stands_in_until_the_read_at_its_pin(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, writer, served) = crate::test_support::session_served(
            PEOPLE,
            |request| {
                let filtered = request["params"].as_object().is_some_and(|p| !p.is_empty());
                match request["name"].as_str() {
                    Some("studio_count") if filtered => serde_json::json!([{"n": 1}]),
                    // Every row: 70,702 at c1, and one more at c2.
                    Some("studio_count") if request["at"] == "c2" => {
                        serde_json::json!([{"n": 70_703}])
                    }
                    Some("studio_count") => serde_json::json!([{"n": 70_702}]),
                    _ => serde_json::json!([
                        {"n": {"@id": "p1", "slug": "p1", "name": "Ada"}},
                        {"n": {"@id": "p2", "slug": "p2", "name": "Grace"}},
                    ]),
                }
            },
            cx,
        );
        served.count_nodes("Person", 70_702);
        fn open<'a>(
            session: &Entity<GraphSession>,
            at_commit: Option<&str>,
            cx: &'a mut TestAppContext,
        ) -> (Entity<TypeTable>, &'a mut VisualTestContext) {
            let (session, at_commit) = (session.clone(), at_commit.map(str::to_owned));
            let (table, cx) = cx.add_window_view(|window, cx| {
                TypeTable::new(
                    WeakEntity::new_invalid(),
                    session,
                    "Person".into(),
                    at_commit,
                    window,
                    cx,
                )
            });
            cx.run_until_parked();
            (table, cx)
        }
        // The session's first head read, and its read of the newest commit after the
        // snapshot, wait while another writer adds a person: the snapshot, read meanwhile,
        // counts c1 at manifest version 1, and the head the session holds is c2, at 2.
        let head = served.hold(|read| read["name"] == "studio_head");
        let newest = served.hold(|read| read["route"] == "commits");
        cx.run_until_parked();
        writer.commit("c2", vec![changed("node", "Person", "p3", None)]);
        head.send(()).unwrap();
        newest.send(()).unwrap();
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            assert_eq!(session.head("main"), Some("c2"));
            let counted = session.count_of(graph_store::EntityKind::Node, "Person");
            assert_eq!(counted, Some(70_702), "the commit before");
            assert_eq!(session.counts_commit(), None, "the versions differ");
        });
        assert_eq!(served.commit_pages(), ["branch=main&limit=1"]);

        // The snapshot's count shows with the first page while the read at the pin is out.
        let count = served.hold(|read| read["name"] == "studio_count");
        let (table, table_cx) = open(&session, None, cx);
        table.read_with(table_cx, |table, _| {
            assert_eq!(table.paging.pinned.as_deref(), Some("c2"));
            assert_eq!(table.count, Loadable::Loaded(70_702), "standing in");
        });
        let counts = served.sent("studio_count");
        assert_eq!(counts.len(), 1, "{counts:?}");
        assert_eq!(counts[0]["at"], "c2");
        count.send(()).unwrap();
        table_cx.run_until_parked();
        table.read_with(table_cx, |table, _| {
            assert_eq!(table.count, Loadable::Loaded(70_703), "the pin's")
        });

        // Refresh reads at the head again, and its count there.
        table.update_in(table_cx, |table, window, cx| table.refresh(window, cx));
        table_cx.run_until_parked();
        table.read_with(table_cx, |table, _| {
            assert_eq!(table.paging.pinned.as_deref(), Some("c2"));
            assert_eq!(table.count, Loadable::Loaded(70_703));
        });

        // Filtered, both counts are read at the pin.
        let only_ada = Filter {
            property: "name".into(),
            op: FilterOp::Eq,
            value: "Ada".into(),
        };
        table.update(table_cx, |table, cx| table.show_only(only_ada, cx));
        table_cx.run_until_parked();
        table.read_with(table_cx, |table, _| {
            assert_eq!(table.count, Loadable::Loaded(1));
            assert_eq!(table.total, Loadable::Loaded(70_703));
        });

        // Held at c1, a table shows no count from the head's snapshot while its own is read.
        let held_count = served.hold(|read| read["name"] == "studio_count" && read["at"] == "c1");
        let (held, held_cx) = open(&session, Some("c1"), cx);
        held.read_with(held_cx, |held, _| assert_eq!(held.count, Loadable::Loading));
        held_count.send(()).unwrap();
        held_cx.run_until_parked();
        held.read_with(held_cx, |held, _| {
            assert_eq!(held.count, Loadable::Loaded(70_702))
        });
    }

    /// When the branch's newest commit, read after the session's snapshot, is at the snapshot's
    /// manifest version, the snapshot counts that commit: a table pinned there counts every row
    /// from it and sends no count read, at the head or held at that commit. The newest commit
    /// is read once, and shared with a list of branches that asks for it. After another
    /// writer's commit the snapshot and the newest commit are read again, and a Refresh at the
    /// new head reads no count either; filtered, only the count through the filter is read.
    #[gpui::test]
    async fn a_table_at_the_commit_the_snapshot_counts_reads_no_count(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, writer, served) = crate::test_support::session_served(
            PEOPLE,
            |request| {
                let filtered = request["params"].as_object().is_some_and(|p| !p.is_empty());
                match request["name"].as_str() {
                    Some("studio_count") if filtered => serde_json::json!([{"n": 1}]),
                    Some("studio_count") => serde_json::json!([{"n": 70_702}]),
                    _ => serde_json::json!([
                        {"n": {"@id": "p1", "slug": "p1", "name": "Ada"}},
                        {"n": {"@id": "p2", "slug": "p2", "name": "Grace"}},
                    ]),
                }
            },
            cx,
        );
        served.count_nodes("Person", 70_702);
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            assert_eq!(session.head("main"), Some("c1"));
            assert_eq!(session.counts_commit(), Some("c1"), "both at version 1");
        });
        assert_eq!(served.commit_pages(), ["branch=main&limit=1"]);

        // A list of branches showing main's newest commit takes the read the counts made.
        let newest = session.update(cx, |session, cx| session.head_commit("main", false, cx));
        cx.run_until_parked();
        let newest = futures::executor::block_on(newest).unwrap().unwrap();
        assert_eq!(newest.graph_commit_id, "c1");
        assert_eq!(served.commit_pages().len(), 1, "shared");

        let open = |at_commit: Option<&str>, cx: &mut TestAppContext| {
            let (session, at_commit) = (session.clone(), at_commit.map(str::to_owned));
            let (table, cx) = cx.add_window_view(|window, cx| {
                TypeTable::new(
                    WeakEntity::new_invalid(),
                    session,
                    "Person".into(),
                    at_commit,
                    window,
                    cx,
                )
            });
            cx.run_until_parked();
            (table, cx.clone())
        };
        let (table, mut table_cx) = open(None, cx);
        table.read_with(&table_cx, |table, _| {
            assert_eq!(table.paging.pinned.as_deref(), Some("c1"));
            assert_eq!(table.count, Loadable::Loaded(70_702));
        });
        assert!(served.sent("studio_count").is_empty(), "no count read");

        // Held at the commit the snapshot counts, a table reads no count either.
        let (held, held_cx) = open(Some("c1"), cx);
        held.read_with(&held_cx, |held, _| {
            assert_eq!(held.count, Loadable::Loaded(70_702))
        });
        assert!(served.sent("studio_count").is_empty(), "no count read");

        // Another writer adds a person: once the session sees it, the snapshot and the newest
        // commit are read again, both at version 2, and a Refresh counts the new head from it.
        writer.commit("c2", vec![changed("node", "Person", "p3", None)]);
        served.count_nodes("Person", 70_703);
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            assert_eq!(session.counts_commit(), Some("c2"));
        });
        assert_eq!(served.commit_pages().len(), 2);
        table.update_in(&mut table_cx, |table, window, cx| table.refresh(window, cx));
        table_cx.run_until_parked();
        table.read_with(&table_cx, |table, _| {
            assert_eq!(table.paging.pinned.as_deref(), Some("c2"));
            assert_eq!(table.count, Loadable::Loaded(70_703));
        });
        assert!(served.sent("studio_count").is_empty(), "no count read");

        // Filtered, only the count through the filter is read.
        let only_ada = Filter {
            property: "name".into(),
            op: FilterOp::Eq,
            value: "Ada".into(),
        };
        table.update(&mut table_cx, |table, cx| table.show_only(only_ada, cx));
        table_cx.run_until_parked();
        table.read_with(&table_cx, |table, _| {
            assert_eq!(table.count, Loadable::Loaded(1));
            assert_eq!(table.total, Loadable::Loaded(70_703));
        });
        let counts = served.sent("studio_count");
        assert_eq!(counts.len(), 1, "{counts:?}");
        let through_filter = counts[0]["params"].as_object();
        assert!(through_filter.is_some_and(|p| !p.is_empty()), "{counts:?}");
        assert_eq!(counts[0]["at"], "c2");
    }

    /// The first page asks for two pages; the next, twice as many, is asked for once the view
    /// comes within a page of the rows read, and not as the first lands. A read out when the
    /// order changes is dropped, and the rows are read from the start in the new order. Before,
    /// the second page was read at once, a cursor scan on every open, sort and filter.
    #[gpui::test]
    async fn the_next_page_is_read_a_page_ahead_and_dropped_when_the_order_changes(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        // A thousand people, as many a page as it asks for from the one after `after`: `p…` by
        // id, `s…` by name.
        let (session, _, served) = crate::test_support::session_served(
            PEOPLE,
            |request| {
                if request["name"] != "studio_page" {
                    return serde_json::json!([]);
                }
                let query = request["query"].as_str().unwrap_or_default();
                let prefix = if query.contains("$n.name") { "s" } else { "p" };
                let from = request["params"]["after"]
                    .as_str()
                    .and_then(|after| after.get(1..)?.parse::<usize>().ok())
                    .map_or(0, |after| after + 1);
                let limit = query
                    .split("limit ")
                    .nth(1)
                    .and_then(|rest| rest.split_whitespace().next()?.parse::<usize>().ok())
                    .unwrap_or(100);
                (from..(from + limit).min(1000))
                    .map(|i| {
                        let id = format!("{prefix}{i:03}");
                        serde_json::json!({"n": {"@id": id, "slug": id, "name": id}})
                    })
                    .collect()
            },
            cx,
        );
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::new(
                WeakEntity::new_invalid(),
                session.clone(),
                "Person".into(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        let after = |read: &Value| read["params"]["after"].as_str().map(str::to_owned);
        let pages: Vec<Option<String>> = served.sent("studio_page").iter().map(after).collect();
        assert_eq!(pages, [None], "two pages, and nothing more at once");
        table.read_with(cx, |table, _| {
            assert_eq!(table.rows.len(), 200);
            assert!(!table.loading_more);
            assert!(table.lines_drawn.end < 100, "{:?}", table.lines_drawn);
        });

        // Scrolled to within a page of the rows read, the next is asked for, twice as large.
        let third = served.hold(|read| read["params"]["after"] == "p199");
        table.update(cx, |table, cx| {
            let height = table.line_height(cx);
            {
                let state = table.table_state.read(cx).scroll_handle.0.borrow();
                state.base_handle.set_offset(point(px(0.), height * -110.));
            }
            cx.notify();
        });
        cx.run_until_parked();
        let sent = served.sent("studio_page");
        assert_eq!(sent.len(), 2);
        let query = sent[1]["query"].as_str().unwrap_or_default();
        assert!(query.contains("limit 400"), "{query}");
        table.read_with(cx, |table, _| {
            assert!(table.loading_more);
            assert_eq!(table.rows.len(), 200);
        });

        // Ordered by name before it lands: it is dropped, and the rows are read again in order,
        // a page ahead of the view at its top.
        table.update(cx, |table, cx| {
            let by_name = Sort {
                property: "name".into(),
                descending: false,
            };
            table.set_sort(Some(by_name), cx)
        });
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            assert_eq!(session.reads_in_flight, 0, "the read dropped stopped")
        });
        third.send(()).ok();
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert_eq!(table.rows.len(), 200);
            assert!(table.rows.iter().all(|row| row.id.starts_with('s')));
            assert!(!table.loading_more);
        });
        let sorted: Vec<Option<String>> =
            served.sent("studio_page")[2..].iter().map(after).collect();
        assert_eq!(sorted, [None]);
    }

    /// A head the session sees move while the table's first read is out is compared once the
    /// read pins the table: whichever lands first, another view's write to a person offers
    /// Refresh.
    #[gpui::test(iterations = 10)]
    async fn a_head_moved_during_the_first_read_is_compared_once_it_lands(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let session = session(PEOPLE, cx);
        let (table, cx) = cx.add_window_view({
            let session = session.clone();
            |window, cx| {
                TypeTable::new(
                    WeakEntity::new_invalid(),
                    session,
                    "Person".into(),
                    None,
                    window,
                    cx,
                )
            }
        });
        cx.run_until_parked();
        let schema = session.read_with(cx, |session, _| {
            assert_eq!(session.head("main"), Some("c1"));
            session.schema_model().unwrap()
        });
        let edit = builder::RowEdit {
            id: "p1".into(),
            cells: vec![("name".into(), "Ada".into(), "Ida".into())],
        };
        let query = builder::update_rows(&schema, "Person", &[edit]).unwrap();
        // Read again as a schema or a naming change reads it: from the head the session holds.
        table.update(cx, |table, cx| table.reload(false, cx));
        session
            .update(cx, |session, cx| {
                session.write_on("main", &query, None, "Set name of p1".into(), None, cx)
            })
            .detach();
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert_eq!(table.paging.pinned.as_deref(), Some("c1"));
            assert!(table.newer, "the head moved past the commit shown");
        });
    }

    /// A first read goes to the head the session holds, not to a newer one only the server has
    /// seen: the pin is a commit the session compares its next head with, so a write to a
    /// person the table shows offers Refresh once the session sees it.
    #[gpui::test]
    async fn a_first_read_pins_the_head_the_session_holds(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let pages = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let seen = pages.clone();
        let (session, writer) = session_followed(
            PEOPLE,
            move |request| {
                if request["name"] == "studio_head" {
                    return serde_json::json!([]);
                }
                if request["name"] == "studio_page" {
                    seen.lock().unwrap().push(request["snapshot"].clone());
                }
                serde_json::json!([
                    {"n": {"@id": "p1", "slug": "p1", "name": "Ada"}},
                    {"n": {"@id": "p2", "slug": "p2", "name": "Grace"}},
                ])
            },
            cx,
        );
        let (table, cx) = cx.add_window_view({
            let session = session.clone();
            |window, cx| {
                TypeTable::new(
                    WeakEntity::new_invalid(),
                    session,
                    "Person".into(),
                    None,
                    window,
                    cx,
                )
            }
        });
        cx.run_until_parked();
        let held = session.read_with(cx, |session, _| session.head("main").map(str::to_owned));
        assert_eq!(held.as_deref(), Some("c1"));
        // Another writer's commit, which only the server has seen.
        writer.commit("c2", vec![changed("node", "Person", "p1", None)]);
        table.update(cx, |table, cx| table.reload(false, cx));
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert_eq!(table.paging.pinned.as_deref(), Some("c1"));
            assert!(!table.newer);
        });
        assert_eq!(
            pages.lock().unwrap().last(),
            Some(&serde_json::json!("c1")),
            "read at the head the session holds"
        );
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert!(table.newer, "a person it shows changed")
        });
        // Refresh wants the newest, which the session may not hold yet: it asks after the
        // head, and reads at it.
        writer.commit("c3", vec![changed("node", "Person", "p2", None)]);
        table.update_in(cx, |table, window, cx| table.refresh(window, cx));
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert_eq!(table.paging.pinned.as_deref(), Some("c3"));
            assert!(!table.newer);
        });
        session.read_with(cx, |session, _| {
            assert_eq!(session.head("main"), Some("c3"))
        });
        // The session holds that head now, so the read goes as a read of the branch.
        assert_eq!(
            pages.lock().unwrap().last(),
            Some(&serde_json::Value::Null),
            "read at the head asked after"
        );
    }

    /// ↵ on a draft typed back to the value it was typed over lets the draft go, as leaving the
    /// cell does, while the row still shows that value.
    #[gpui::test]
    async fn a_draft_typed_back_and_confirmed_goes(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, sent) = crate::test_support::session_recording_writes(PEOPLE, cx);
        let (table, cx) = cx.add_window_view({
            let session = session.clone();
            |window, cx| {
                TypeTable::new(
                    WeakEntity::new_invalid(),
                    session,
                    "Person".into(),
                    None,
                    window,
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
        let typed_back = |old: &str, cx: &mut VisualTestContext| {
            session.update(cx, |session, cx| {
                let draft = DraftEntry {
                    old: old.into(),
                    text: "Ada King".into(),
                };
                session.set_draft(key.clone(), draft, cx);
            });
            cx.run_until_parked();
            let out = table.update_in(cx, |table, window, cx| {
                let col = table.column_index("name").unwrap();
                table.start_edit(Cursor { row: 0, col }, window, cx);
                assert_eq!(table.cell_editor.read(cx).text(cx), "Ada King");
                table
                    .cell_editor
                    .update(cx, |editor, cx| editor.set_text(old, window, cx));
                table.confirm_edit(window, cx);
                assert!(table.editing.is_none());
                table.saves_in_flight
            });
            cx.run_until_parked();
            out
        };
        assert_eq!(typed_back("Ada", cx), 0, "nothing to save");
        session.read_with(cx, |session, _| assert!(session.draft(&key).is_none()));
        table.read_with(cx, |table, _| {
            assert_eq!(table.draft_text("p1", "name"), None)
        });
        assert!(sent.lock().unwrap().is_empty());

        // Typed over a value the row no longer shows: asking for it back is a change, sent
        // bound to it, so the change made since is found rather than the draft going unseen.
        assert_eq!(typed_back("Ada B", cx), 1, "the bound write goes");
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "{sent:?}");
        let params: Vec<&str> = sent[0]["params"]
            .as_object()
            .unwrap()
            .values()
            .filter_map(Value::as_str)
            .collect();
        assert!(params.contains(&"Ada B"), "{params:?}");
    }

    /// A reload under an open edit keeps it as leaving the cell would: text asking back for the
    /// value its draft was typed over, which the row no longer shows, stays a draft. The keys
    /// come back to the table, whose cell editor is no longer drawn.
    #[gpui::test]
    async fn a_reload_keeps_an_open_edit_as_leaving_it_would(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let session = session(PEOPLE, cx);
        let (table, cx) = cx.add_window_view({
            let session = session.clone();
            |window, cx| {
                TypeTable::new(
                    WeakEntity::new_invalid(),
                    session,
                    "Person".into(),
                    None,
                    window,
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
        // Typed over "Ada B"; the row shows "Ada" since.
        session.update(cx, |session, cx| {
            let draft = DraftEntry {
                old: "Ada B".into(),
                text: "Ada King".into(),
            };
            session.set_draft(key.clone(), draft, cx);
        });
        cx.run_until_parked();
        table.update_in(cx, |table, window, cx| {
            let col = table.column_index("name").unwrap();
            table.start_edit(Cursor { row: 0, col }, window, cx);
            table
                .cell_editor
                .update(cx, |editor, cx| editor.set_text("Ada B", window, cx));
            table.reload(false, cx);
        });
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            let draft = session.draft(&key).expect("the draft stays");
            assert_eq!(draft.text, "Ada B");
        });
        table.update_in(cx, |table, window, _| {
            assert!(table.editing.is_none());
            assert!(
                table.focus_handle.is_focused(window),
                "the table has the keys"
            );
        });
    }

    /// A file dropped on a Blob cell is stored there bound to the ETag the cell read at the
    /// table's commit, and its commit moves the table there with the cell showing the new
    /// value. A drop of several files, and any drop on a table held at a commit, sends nothing.
    #[gpui::test]
    async fn a_file_dropped_on_a_blob_cell_is_stored_bound_to_the_value_read(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let schema = PEOPLE.replace("  name: String\n", "  name: String\n  doc: Blob?\n");
        let (session, blobs) = crate::test_support::session_with_blobs(&schema, cx);
        let read = blobs.set("Person/p1/doc", b"old");
        /// The table, drawn, with its Blob cells read.
        fn open<'a>(
            session: &Entity<GraphSession>,
            at_commit: Option<&str>,
            cx: &'a mut TestAppContext,
        ) -> (Entity<TypeTable>, &'a mut VisualTestContext) {
            let (session, at_commit) = (session.clone(), at_commit.map(str::to_owned));
            let (table, cx) = cx.add_window_view(|window, cx| {
                TypeTable::new(
                    WeakEntity::new_invalid(),
                    session,
                    "Person".into(),
                    at_commit,
                    window,
                    cx,
                )
            });
            cx.run_until_parked();
            cx.executor().advance_clock(BLOB_READ_AFTER);
            cx.run_until_parked();
            (table, cx)
        }
        let key = ("p1".to_owned(), "doc".to_owned());

        let (held, held_cx) = open(&session, Some("c1"), cx);
        let file = crate::test_support::upload_file("held.txt", b"held", None);
        held.update(held_cx, |table, cx| {
            assert!(!table.takes_blob_drop("p1", "doc", cx));
            table.upload_blob("p1", "doc", file, cx);
        });
        held_cx.run_until_parked();
        assert!(blobs.writes.lock().unwrap().is_empty());
        held.read_with(held_cx, |table, _| {
            assert!(matches!(
                &table.blob_edits[&key],
                BlobEdit::Failed { notice, .. } if notice.message.starts_with("Read-only")
            ));
        });

        let (table, cx) = open(&session, None, cx);
        table.update(cx, |table, cx| {
            assert!(table.takes_blob_drop("p1", "doc", cx));
            table.drop_blob_files("p1", "doc", &["a.txt".into(), "b.txt".into()], cx);
        });
        assert!(blobs.writes.lock().unwrap().is_empty());
        let file = crate::test_support::upload_file("scan.txt", b"a new scan", None);
        table.update(cx, |table, cx| {
            table.drop_blob_files("p1", "doc", &[file], cx);
            assert!(table.blob_edits[&key].is_saving(), "its dot shows");
            assert_eq!(table.saves_in_flight, 1);
        });
        cx.run_until_parked();
        let writes = blobs.writes.lock().unwrap().clone();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].if_match.as_deref(), Some(read.as_str()));
        let stored = blobs.get("Person/p1/doc").unwrap();
        assert_eq!(stored.0, b"a new scan");
        table.read_with(cx, |table, _| {
            assert_eq!(table.paging.pinned.as_deref(), Some("c2"), "its own commit");
            assert!(!table.newer && table.saves_in_flight == 0);
            assert!(!table.blob_edits.contains_key(&key));
            assert_eq!(
                table.blobs.get(&key.0, &key.1),
                Some(&Loadable::Loaded(BlobMeta::Managed {
                    size: 10,
                    etag: Some(stored.1.clone())
                }))
            );
        });
    }

    /// A file still being stored when the table moves to another branch goes on to the branch
    /// left: the new branch's cell does not show it as going and takes a file of its own, and
    /// the write's landing changes nothing there.
    #[gpui::test]
    async fn a_blob_write_out_stays_with_the_branch_left(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let schema = PEOPLE.replace("  name: String\n", "  name: String\n  doc: Blob?\n");
        let (session, blobs) = crate::test_support::session_with_blobs(&schema, cx);
        blobs.set("Person/p1/doc", b"old");
        let (table, cx) = cx.add_window_view({
            let session = session.clone();
            |window, cx| {
                TypeTable::new(
                    WeakEntity::new_invalid(),
                    session,
                    "Person".into(),
                    None,
                    window,
                    cx,
                )
            }
        });
        cx.run_until_parked();
        cx.executor().advance_clock(BLOB_READ_AFTER);
        cx.run_until_parked();
        let (open, gate) = futures::channel::oneshot::channel();
        *blobs.hold.lock().unwrap() = Some(gate);
        let key = ("p1".to_owned(), "doc".to_owned());
        let file = crate::test_support::upload_file("scan.txt", b"a new scan", None);
        table.update(cx, |table, cx| table.upload_blob("p1", "doc", file, cx));
        cx.run_until_parked();
        table.read_with(cx, |table, _| assert!(table.blob_edits[&key].is_saving()));

        session.update(cx, |session, cx| session.set_branch("fix".into(), cx));
        cx.run_until_parked();
        table.read_with(cx, |table, cx| {
            assert_eq!(table.branch, "fix");
            assert!(table.blob_edits.is_empty(), "not shown on fix");
            assert!(table.takes_blob_drop("p1", "doc", cx));
        });
        open.send(()).ok();
        cx.run_until_parked();
        let writes = blobs.writes.lock().unwrap().clone();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].branch, "main", "it went to the branch left");
        table.read_with(cx, |table, _| {
            assert!(table.blob_edits.is_empty());
            assert_eq!(table.saves_in_flight, 0);
        });
    }

    /// Clear is a key on the Blob cell under the cursor, and only there: it sets the cell to
    /// null bound to the ETag the cell read, and the status bar offers it with Choose File.
    #[gpui::test]
    async fn the_blob_cell_under_the_cursor_is_cleared_by_its_key(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
            cx.bind_keys([gpui::KeyBinding::new(
                "ctrl-cmd-backspace",
                ClearValue,
                Some("TypeTable && !editing"),
            )]);
        });
        let schema = PEOPLE.replace("  name: String\n", "  name: String\n  doc: Blob?\n");
        let (session, blobs) = crate::test_support::session_with_blobs(&schema, cx);
        let read = blobs.set("Person/p1/doc", b"old");
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::new(
                WeakEntity::new_invalid(),
                session,
                "Person".into(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        cx.executor().advance_clock(BLOB_READ_AFTER);
        cx.run_until_parked();
        // The keys a cell takes are the table's actions as it draws there.
        let at = |table: &mut TypeTable,
                  column: &str,
                  window: &mut Window,
                  cx: &mut Context<TypeTable>| {
            let col = table.column_index(column).unwrap();
            table.set_cursor(Cursor { row: 0, col }, cx);
            window.focus(&table.focus_handle, cx);
            cx.notify();
        };
        table.update_in(cx, |table, window, cx| at(table, "name", window, cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("ctrl-cmd-backspace");
        cx.run_until_parked();
        assert!(blobs.writes.lock().unwrap().is_empty(), "not a Blob cell");

        table.update_in(cx, |table, window, cx| {
            at(table, "doc", window, cx);
            let keys = table.status_parts(cx).keys;
            let labels: Vec<&str> = keys.iter().map(|key| &*key.label).collect();
            assert_eq!(labels[1..3], ["Choose file", "Clear value"]);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("ctrl-cmd-backspace");
        cx.run_until_parked();
        let writes = blobs.writes.lock().unwrap().clone();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].method, "DELETE");
        assert_eq!(writes[0].if_match.as_deref(), Some(read.as_str()));
        table.read_with(cx, |table, _| {
            let key = ("p1".to_owned(), "doc".to_owned());
            assert_eq!(
                table.blobs.get(&key.0, &key.1),
                Some(&Loadable::Loaded(BlobMeta::Null))
            );
        });
    }

    /// On a branch the access route says the credential may not change, the table offers no
    /// write: Edit, New row, Delete row, Save drafts, Choose File and Clear are neither
    /// registered nor listed as keys; a click on a value peeks it, and a draft's save and a
    /// file stored send nothing, the file saying why and the draft staying. While what the
    /// credential may do is not known, every one is offered.
    #[gpui::test]
    async fn a_branch_the_credential_may_not_change_offers_no_writes(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let schema = PEOPLE.replace("  name: String\n", "  name: String\n  doc: Blob?\n");
        /// The table, drawn with its Blob cells read, the keyboard on the first row's Blob cell.
        fn open(
            session: Entity<GraphSession>,
            cx: &mut TestAppContext,
        ) -> (Entity<TypeTable>, &mut VisualTestContext) {
            let (table, cx) = cx.add_window_view(|window, cx| {
                TypeTable::new(
                    WeakEntity::new_invalid(),
                    session,
                    "Person".into(),
                    None,
                    window,
                    cx,
                )
            });
            cx.run_until_parked();
            cx.executor().advance_clock(BLOB_READ_AFTER);
            cx.run_until_parked();
            table.update_in(cx, |table, window, cx| {
                let col = table.column_index("doc").unwrap();
                table.set_cursor(Cursor { row: 0, col }, cx);
                window.focus(&table.focus_handle, cx);
                cx.notify();
            });
            cx.run_until_parked();
            (table, cx)
        }
        /// Which writes the table offers where the keyboard is, and the keys it lists.
        fn offered(
            table: &Entity<TypeTable>,
            cx: &mut VisualTestContext,
        ) -> (Vec<bool>, Vec<String>) {
            let writes: [&dyn gpui::Action; 6] = [
                &EditCell,
                &InsertRow,
                &DeleteRow,
                &SaveDrafts,
                &ChooseFile,
                &ClearValue,
            ];
            let available = cx.update(|window, cx| {
                writes
                    .iter()
                    .map(|action| window.is_action_available(*action, cx))
                    .collect()
            });
            let keys = table.update(cx, |table, cx| {
                let keys = table.status_parts(cx).keys;
                keys.iter().map(|key| key.label.to_string()).collect()
            });
            (available, keys)
        }

        let (unknown, _) = crate::test_support::session_with_blobs(&schema, cx);
        let (table, unknown_cx) = open(unknown, cx);
        let (available, keys) = offered(&table, unknown_cx);
        assert_eq!(available, [true; 6], "offered while not known: {keys:?}");
        assert!(keys.iter().any(|key| key == "New row"), "{keys:?}");

        let (session, sent, blobs) = crate::test_support::session_with_access(
            &schema,
            crate::test_support::read_only_access(),
            cx,
        );
        blobs.set("Person/p1/doc", b"old");
        let (table, cx) = open(session.clone(), cx);
        session.read_with(cx, |session, _| {
            assert_eq!(
                session.allows(
                    graph_store::AccessAction::Change,
                    &graph_store::AccessScope::branch(&session.branch)
                ),
                graph_store::Allowed::No
            );
        });
        let (available, keys) = offered(&table, cx);
        assert_eq!(available, [false; 6]);
        assert_eq!(
            keys,
            [
                "Peek",
                "Open page",
                "Explore",
                "Copy",
                "Filter by this value",
                "Refresh"
            ]
        );
        let refused = "Your credential may not change main";
        table.update_in(cx, |table, window, cx| {
            assert_eq!(table.change_refusal(cx).as_deref(), Some(refused));
            let col = table.column_index("name").unwrap();
            table.start_edit(Cursor { row: 0, col }, window, cx);
            assert!(table.editing.is_none(), "peeked, not edited");
        });
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
        let file = crate::test_support::upload_file("refused.txt", b"new", None);
        table.update_in(cx, |table, window, cx| {
            table.save_drafts(window, cx);
            assert!(!table.takes_blob_drop("p1", "doc", cx));
            table.upload_blob("p1", "doc", file, cx);
        });
        cx.run_until_parked();
        assert!(sent.lock().unwrap().is_empty(), "nothing was sent");
        assert!(blobs.writes.lock().unwrap().is_empty());
        table.read_with(cx, |table, _| {
            let doc = ("p1".to_owned(), "doc".to_owned());
            assert!(matches!(
                &table.blob_edits[&doc],
                BlobEdit::Failed { notice, .. } if notice.message == refused
            ));
            let name = ("p1".to_owned(), "name".to_owned());
            assert!(
                matches!(table.cells.get(&name), Some(CellState::Draft { .. })),
                "the draft stays"
            );
        });
    }

    /// Naming a type's nodes otherwise keeps a table where it is, at its commit with a cell
    /// being edited, and reads again only what names neighbours.
    #[gpui::test]
    async fn naming_nodes_otherwise_keeps_the_table_and_its_edit(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
            cx.set_global(studio_settings::StudioSettings::default());
        });
        let session = session(PEOPLE, cx);
        let (table, cx) = cx.add_window_view({
            let session = session.clone();
            |window, cx| {
                TypeTable::new(
                    WeakEntity::new_invalid(),
                    session,
                    "Person".into(),
                    None,
                    window,
                    cx,
                )
            }
        });
        cx.run_until_parked();
        table.update_in(cx, |table, window, cx| {
            let col = table.column_index("name").unwrap();
            table.start_edit(Cursor { row: 0, col }, window, cx);
            table
                .cell_editor
                .update(cx, |editor, cx| editor.set_text("Ada L", window, cx));
        });
        let graph = session.read_with(cx, |session, _| session.overrides_key());
        cx.update(|_, cx| {
            cx.update_global::<studio_settings::StudioSettings, _>(|settings, _| {
                settings.set_label(&graph, "Person", Some("slug".into()))
            })
        });
        cx.run_until_parked();
        table.read_with(cx, |table, cx| {
            let person = table.schema.as_ref().unwrap().node("Person").unwrap();
            assert_eq!(
                person.label_property(),
                Some("slug"),
                "named by the slug now"
            );
            assert_eq!(table.paging.pinned.as_deref(), Some("c1"));
            assert_eq!(table.rows.len(), 2);
            let edited = table
                .editing
                .as_ref()
                .map(|editing| editing.column.as_str());
            assert_eq!(edited, Some("name"), "the edit stays open");
            assert_eq!(table.cell_editor.read(cx).text(cx), "Ada L");
        });
    }

    /// A table opened sorted and grouped, as a split or a restored tab opens, reads its groups
    /// once, in that order.
    #[gpui::test]
    async fn a_table_opened_sorted_and_grouped_reads_once(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, names) = crate::test_support::session_routing(
            PEOPLE,
            |request| match request["name"].as_str() {
                Some("studio_value_counts") => serde_json::json!([
                    {"value": "Ada", "n": 1},
                    {"value": "Grace", "n": 1},
                ]),
                _ => serde_json::json!([
                    {"n": {"@id": "p1", "slug": "p1", "name": "Ada"}},
                    {"n": {"@id": "p2", "slug": "p2", "name": "Grace"}},
                ]),
            },
            cx,
        );
        // The schema is in, as for a tab split from another.
        cx.run_until_parked();
        let opening = Opening {
            sort: Some(Sort {
                property: "name".into(),
                descending: true,
            }),
            group_by: Some("name".into()),
            ..Opening::default()
        };
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::opening(
                WeakEntity::new_invalid(),
                session,
                "Person".into(),
                None,
                opening,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert_eq!(table.sort.as_ref().map(|sort| sort.descending), Some(true));
            assert_eq!(
                table.grouping.as_ref().map(|g| g.property.as_str()),
                Some("name")
            );
        });
        let names = names.lock().unwrap();
        let groups = names
            .iter()
            .filter(|name| *name == "studio_value_counts")
            .count();
        assert_eq!(groups, 1, "{names:?}");
    }

    /// The key of p1's name, the cell these tests edit.
    fn name_key() -> DraftKey {
        DraftKey {
            type_name: "Person".into(),
            id: "p1".into(),
            property: "name".into(),
        }
    }

    /// The Person table on `session`, read.
    fn people_table<'a>(
        session: &Entity<GraphSession>,
        cx: &'a mut TestAppContext,
    ) -> (Entity<TypeTable>, &'a mut VisualTestContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let session = session.clone();
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::new(
                WeakEntity::new_invalid(),
                session,
                "Person".into(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        (table, cx)
    }

    /// Opens p1's name in the cell editor with `text` typed.
    fn type_in_name(table: &Entity<TypeTable>, text: &str, cx: &mut VisualTestContext) {
        table.update_in(cx, |table, window, cx| {
            let col = table.column_index("name").unwrap();
            table.start_edit(Cursor { row: 0, col }, window, cx);
            table
                .cell_editor
                .update(cx, |editor, cx| editor.set_text(text, window, cx));
        });
    }

    /// The table moved to another branch with an edit open keeps what was typed as a draft on
    /// the branch it was typed on, as leaving the cell would: a link or a toast's follow-up that
    /// moves the window loses nothing, and the new branch gets none of it.
    #[gpui::test]
    async fn a_branch_change_keeps_an_open_edit_on_the_branch_it_was_typed_on(
        cx: &mut TestAppContext,
    ) {
        let session = session(PEOPLE, cx);
        let (table, cx) = people_table(&session, cx);
        type_in_name(&table, "Ada King", cx);
        session.update(cx, |session, cx| session.set_branch("fix".into(), cx));
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            let typed = DraftEntry {
                old: "Ada".into(),
                text: "Ada King".into(),
            };
            let on_main: Vec<_> = session.drafts_on("main").collect();
            assert_eq!(on_main, vec![(&name_key(), &typed)]);
            assert!(session.draft(&name_key()).is_none(), "nothing on fix");
        });
        table.update_in(cx, |table, window, _| {
            assert!(table.editing.is_none());
            assert!(
                table.focus_handle.is_focused(window),
                "the table has the keys"
            );
        });
    }

    /// An edit open that changes its cell is unsaved work, so closing the tab asks first; let
    /// go (its tab closed), the table keeps what was typed
    /// as the cell's draft.
    #[gpui::test]
    async fn a_table_let_go_keeps_its_open_edit_as_a_draft(cx: &mut TestAppContext) {
        let session = session(PEOPLE, cx);
        let (table, cx) = people_table(&session, cx);
        type_in_name(&table, "Ada", cx);
        table.read_with(cx, |table, cx| {
            assert!(!table.is_dirty(cx), "typed back, it asks for nothing")
        });
        type_in_name(&table, "Ada King", cx);
        table.read_with(cx, |table, cx| assert!(table.is_dirty(cx)));
        drop(table);
        cx.update(|window, _| window.remove_window());
        cx.run_until_parked();
        let draft = session.read_with(cx, |session, _| session.draft(&name_key()).cloned());
        assert_eq!(
            draft,
            Some(DraftEntry {
                old: "Ada".into(),
                text: "Ada King".into(),
            })
        );
    }

    /// A date preset holds no value of its own: a row is judged against its days, as the engine
    /// is asked, so an edit that leaves a row born today under "born is today" keeps it plain,
    /// and a row inserted born today counts as matching.
    #[gpui::test]
    async fn a_date_preset_judges_edited_and_inserted_rows_by_its_days(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        const BORN: &str = "node Person {\n  slug: String @key\n  name: String\n  born: Date?\n}\n";
        let today = chrono::Local::now()
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();
        let rows = serde_json::json!([
            {"n": {"@id": "p1", "slug": "p1", "name": "Ada", "born": today}},
            {"n": {"@id": "p2", "slug": "p2", "name": "Grace", "born": today}},
        ]);
        let session = crate::test_support::session_reading(BORN, rows, cx);
        let opening = Opening {
            filters: vec![Filter {
                property: "born".into(),
                op: FilterOp::Today,
                value: Value::Null,
            }],
            ..Opening::default()
        };
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::opening(
                WeakEntity::new_invalid(),
                session,
                "Person".into(),
                None,
                opening,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        type_in_name(&table, "Ada Lovelace", cx);
        table.update_in(cx, |table, window, cx| table.confirm_edit(window, cx));
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert_eq!(table.rows[0].value("name"), "Ada Lovelace");
            assert!(!table.rows[0].dimmed, "born today, it still matches");
        });
        // Inserted behind the branch, so the counts are adjusted here.
        table.update(cx, |table, cx| {
            table.count = Loadable::Loaded(2);
            let mut values = serde_json::Map::new();
            values.insert("slug".into(), "p3".into());
            values.insert("name".into(), "Cy".into());
            values.insert("born".into(), today.clone().into());
            table.row_inserted("main", Some("p3".into()), values, "c9", Some("c8"), cx);
            assert_eq!(table.count, Loadable::Loaded(3), "it matches the preset");
            let p3 = table.row_index("p3").expect("placed");
            assert!(!table.rows[p3].dimmed);
        });
    }

    /// A page read after the table's own write is read at its commit, where a row it edited can
    /// sort again further on (a title edited from "B" to "Zed"): it shows once, where it was
    /// read (constitution V). Before any own write, every page is read at one commit.
    #[gpui::test]
    async fn a_row_the_tables_write_moved_further_on_shows_once(cx: &mut TestAppContext) {
        let session = session(PEOPLE, cx);
        let (table, cx) = people_table(&session, cx);
        let page = |rows: Value| -> Result<ReadOutput, ReadError> {
            Ok(serde_json::from_value(serde_json::json!({
                "query_name": "studio_page",
                "target": {"branch": "main"},
                "row_count": 2,
                "columns": ["n"],
                "rows": rows,
                "graph_commit_id": "c2",
            }))
            .unwrap())
        };
        type_in_name(&table, "Zed", cx);
        table.update_in(cx, |table, window, cx| table.confirm_edit(window, cx));
        cx.run_until_parked();
        // The next page at the table's commit holds Cy, then Ada, now Zed.
        let next = serde_json::json!([
            {"n": {"@id": "p3", "slug": "p3", "name": "Cy"}},
            {"n": {"@id": "p1", "slug": "p1", "name": "Zed"}},
        ]);
        table.update(cx, |table, cx| {
            assert_eq!(table.paging.pinned.as_deref(), Some("c2"), "its own commit");
            table.apply_page(page(next), None, 2, cx);
            let ids: Vec<&str> = table.rows.iter().map(|row| row.id.as_str()).collect();
            assert_eq!(ids, ["p1", "p2", "p3"]);
            assert_eq!(table.rows[0].value("name"), "Zed");
        });
    }

    /// Grouped by name, a row the table's write moved from Ada's section into Grace's stays in
    /// Ada's, and Grace's pages, read at the table's commit, leave it out: Grace's count, read
    /// before the write, is spent on the rows still unread there, so none stays unread.
    #[gpui::test]
    async fn a_row_moved_into_another_section_leaves_its_rows_to_read(cx: &mut TestAppContext) {
        let session = session(PEOPLE, cx);
        let (table, cx) = people_table(&session, cx);
        let page = |rows: Value| -> Result<ReadOutput, ReadError> {
            Ok(serde_json::from_value(serde_json::json!({
                "query_name": "studio_page",
                "target": {"branch": "main"},
                "row_count": 1,
                "columns": ["n"],
                "rows": rows,
                "graph_commit_id": "c2",
            }))
            .unwrap())
        };
        let generation = table.update(cx, |table, _| {
            let mut grouping = Grouping::new("name");
            let generation = grouping.restart();
            let counts: ReadOutput = serde_json::from_value(serde_json::json!({
                "query_name": "studio_value_counts",
                "target": {"branch": "main"},
                "row_count": 2,
                "columns": ["value", "n"],
                "rows": [{"value": "Ada", "n": 1}, {"value": "Grace", "n": 2}],
            }))
            .unwrap();
            grouping.accept_counts(&counts, GROUP_LIMIT);
            for (index, range) in [0..1, 1..2].into_iter().enumerate() {
                grouping.section_mut(index).unwrap().range = range;
            }
            table.grouping = Some(grouping);
            // Ada renamed Grace, committed on the table's commit.
            table.set_cell(0, "name", "Grace".into());
            assert!(!table.follow_own_commit(Some("c1"), "c2"));
            let fits = table.row_fits(0);
            table.set_dimmed(0, !fits);
            assert!(table.rows[0].dimmed, "out of its section, it stays");
            generation
        });
        let grace = |table: &TypeTable| table.grouping.as_ref().unwrap().sections()[1].clone();
        // Grace's next row at the new commit is Ada, already shown.
        let only_ada = serde_json::json!([{"n": {"@id": "p1", "slug": "p1", "name": "Grace"}}]);
        table.update(cx, |table, cx| {
            table.apply_section_page(generation, 1, page(only_ada), 1, cx);
            assert_eq!(table.rows.len(), 2, "shown once");
            assert!(grace(table).has_more(), "a Grace is still unread");
        });
        let grace_row = serde_json::json!([{"n": {"@id": "p3", "slug": "p3", "name": "Grace"}}]);
        table.update(cx, |table, cx| {
            table.apply_section_page(generation, 1, page(grace_row), 1, cx);
            let ids: Vec<&str> = table.rows.iter().map(|row| row.id.as_str()).collect();
            assert_eq!(ids, ["p1", "p2", "p3"]);
            assert!(!grace(table).has_more());
        });
    }

    /// A Blob value's validator names its type's table version, so a file stored in one cell
    /// moves every other cell's. A table behind its branch binds its next write at the commit
    /// its own write made, which the server applied only while the table was as it showed it:
    /// a second file is stored, not refused as changed elsewhere. A change made elsewhere
    /// after it is still refused.
    #[gpui::test]
    async fn a_tables_own_blob_write_leaves_its_next_one_bound_where_it_holds(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let schema = PEOPLE.replace("  name: String\n", "  name: String\n  doc: Blob?\n");
        let (session, blobs) = crate::test_support::session_with_blobs(&schema, cx);
        blobs
            .versioned
            .store(true, std::sync::atomic::Ordering::SeqCst);
        blobs.set("Person/p1/doc", b"one");
        blobs.set("Person/p2/doc", b"two");
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::new(
                WeakEntity::new_invalid(),
                session,
                "Person".into(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        cx.executor().advance_clock(BLOB_READ_AFTER);
        cx.run_until_parked();
        // Behind its branch: another writer's commit came after the table's read.
        table.update(cx, |table, _| {
            table.paging.pinned = Some("c0".into());
            table.newer = true;
        });
        let file = crate::test_support::upload_file("first.txt", b"first", None);
        table.update(cx, |table, cx| table.upload_blob("p1", "doc", file, cx));
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert!(!table.blob_edits.contains_key(&("p1".into(), "doc".into())));
            assert_eq!(table.paging.blobs_at(), Some("c2"), "its own commit");
        });
        let p2_now = blobs.get("Person/p2/doc").unwrap().1;
        let file = crate::test_support::upload_file("second.txt", b"second", None);
        table.update(cx, |table, cx| table.upload_blob("p2", "doc", file, cx));
        cx.run_until_parked();
        {
            let writes = blobs.writes.lock().unwrap();
            assert_eq!(writes.len(), 2);
            assert_eq!(writes[1].if_match.as_deref(), Some(p2_now.as_str()));
        }
        assert_eq!(blobs.get("Person/p2/doc").unwrap().0, b"second");
        let p2 = ("p2".to_owned(), "doc".to_owned());
        table.read_with(cx, |table, _| {
            assert!(!table.blob_edits.contains_key(&p2), "stored");
        });

        // Another writer's value of the same size after it: refused, never written over.
        blobs.set("Person/p2/doc", b"theirs");
        let file = crate::test_support::upload_file("third.txt", b"third", None);
        table.update(cx, |table, cx| table.upload_blob("p2", "doc", file, cx));
        cx.run_until_parked();
        assert_eq!(blobs.writes.lock().unwrap().len(), 3, "sent once");
        assert_eq!(blobs.get("Person/p2/doc").unwrap().0, b"theirs");
        table.read_with(cx, |table, _| {
            assert!(matches!(
                &table.blob_edits[&p2],
                BlobEdit::Failed { notice, .. } if notice.tone == Tone::Conflict
            ));
        });
    }

    /// A peek of a row whose cell save is out shows what the row holds at the table's commit,
    /// with the property saving, not the value sent as if stored. Refused, the peek keeps the
    /// stored value; saved, it follows the save to its commit, as it follows its own writes.
    #[gpui::test]
    async fn a_peek_of_a_row_being_saved_shows_what_is_stored(cx: &mut TestAppContext) {
        for (status, body, landed) in [
            (
                403,
                serde_json::json!({ "error": "policy denied", "code": "forbidden", "disposition": { "action": "refresh", "effects": "none" } }),
                false,
            ),
            (200, crate::test_support::committed("c2", "c1"), true),
        ] {
            let session = crate::test_support::session_answering(PEOPLE, status, body, cx);
            let (table, cx) = people_table(&session, cx);
            type_in_name(&table, "Ada Lovelace", cx);
            let peek = table.update_in(cx, |table, window, cx| {
                table.confirm_edit(window, cx);
                assert_eq!(table.rows[0].value("name"), "Ada Lovelace", "sent");
                table.peek_row(0, cx)
            });
            peek.read_with(cx, |peek, _| {
                assert_eq!(peek.shown("name"), ("Ada".into(), true), "stored, saving");
            });
            cx.run_until_parked();
            peek.read_with(cx, |peek, _| {
                if landed {
                    assert_eq!(peek.shown("name"), ("Ada Lovelace".into(), false));
                    assert_eq!(peek.reads_at(), Some("c2"), "the save's commit");
                    assert!(!peek.is_newer());
                } else {
                    let nothing_written = ("Ada".into(), false);
                    assert_eq!(peek.shown("name"), nothing_written);
                    assert_eq!(peek.reads_at(), Some("c1"));
                }
            });
        }
    }

    /// Every write the table makes reaches its peek, which follows it as it follows its own: a
    /// bulk set its row is in shows the value set at the set's commit, and a row inserted moves
    /// it on to the insert's commit, neither offering Refresh. A cell's save and a drafts' save
    /// did before; these did not.
    #[gpui::test]
    async fn a_peek_follows_every_write_of_its_table(cx: &mut TestAppContext) {
        let mut both = crate::test_support::committed("c2", "c1");
        both["affected_nodes"] = 2.into();
        let inserted = crate::test_support::committed("c3", "c2");
        let session =
            crate::test_support::session_writing(PEOPLE, vec![(200, both), (200, inserted)], cx);
        let (table, cx) = people_table(&session, cx);
        let peek = table.update(cx, |table, cx| table.peek_row(0, cx));
        cx.run_until_parked();
        table.update_in(cx, |table, window, cx| {
            table.selection.extend(["p1".to_owned(), "p2".to_owned()]);
            table.bulk.property = Some("name".into());
            table
                .bulk_editor
                .update(cx, |editor, cx| editor.set_text("Ida", window, cx));
            table.submit_bulk(window, cx);
        });
        cx.run_until_parked();
        peek.read_with(cx, |peek, _| {
            assert_eq!(peek.shown("name"), ("Ida".into(), false));
            assert_eq!(peek.reads_at(), Some("c2"), "the set's commit");
            assert!(!peek.is_newer());
        });

        // An insert, sent as the dialog sends it, then landed in the table.
        let values = vec![
            ("slug".to_owned(), serde_json::json!("p3")),
            ("name".to_owned(), serde_json::json!("Lin")),
        ];
        let write = session.update(cx, |session, cx| {
            let schema = session.schema_model().unwrap();
            let query = gq_language::builder::insert_node(&schema, "Person", &values).unwrap();
            session.write_on("main", &query, None, "Insert p3".into(), None, cx)
        });
        cx.run_until_parked();
        assert!(matches!(
            write.await,
            graph_client::WriteOutcome::Committed(_)
        ));
        table.update(cx, |table, cx| {
            let row = values.into_iter().collect();
            table.row_inserted("main", Some("p3".into()), row, "c3", Some("c2"), cx)
        });
        cx.run_until_parked();
        peek.read_with(cx, |peek, _| {
            assert_eq!(peek.reads_at(), Some("c3"), "the insert's commit");
            assert!(!peek.is_newer());
            assert_eq!(peek.shown("name"), ("Ida".into(), false));
        });
    }

    const DOCS: &str = "node Person {\n  slug: String @key\n  name: String\n  doc: Blob?\n}\n";

    /// A Blob the table stores in its peek's row reaches the peek: the slot shows the value
    /// stored, at the write's commit, rather than offering Refresh.
    #[gpui::test]
    async fn a_peek_shows_the_blobs_its_table_stores(cx: &mut TestAppContext) {
        let (session, blobs) = crate::test_support::session_with_blobs(DOCS, cx);
        blobs.set("Person/p1/doc", b"old");
        let (table, cx) = people_table(&session, cx);
        table.update(cx, |table, cx| table.load_blob_metas(0..2, cx));
        cx.run_until_parked();
        let peek = table.update(cx, |table, cx| table.peek_row(0, cx));
        cx.run_until_parked();
        peek.read_with(cx, |peek, _| assert_eq!(peek.blob_size("doc"), Some(3)));
        let file = crate::test_support::upload_file("new.txt", b"newer", None);
        table.update(cx, |table, cx| table.upload_blob("p1", "doc", file, cx));
        cx.run_until_parked();
        peek.read_with(cx, |peek, _| {
            assert_eq!(peek.reads_at(), Some("c2"), "the write's commit");
            assert!(!peek.is_newer());
            assert_eq!(peek.blob_size("doc"), Some(5), "the value stored");
        });
    }

    /// A table behind its branch (another writer committed after its read) that stores a Blob
    /// holds its cells at its own commit, past its pin: a peek opened from it then shows the
    /// value stored, and its slots hold there, so its own Blob write binds to that value and a
    /// tab it opens reads there. Given only the table's pin, it read the value replaced.
    #[gpui::test]
    async fn a_peek_opened_after_its_tables_blob_write_shows_it_where_it_holds(
        cx: &mut TestAppContext,
    ) {
        let (session, blobs) = crate::test_support::session_with_blobs(DOCS, cx);
        blobs.set("Person/p1/doc", b"old");
        blobs.set("Person/p2/doc", b"two");
        let (table, cx) = people_table(&session, cx);
        table.update(cx, |table, cx| table.load_blob_metas(0..2, cx));
        cx.run_until_parked();
        // Another writer's commit, c2, on the table's c1.
        let cell = BlobCell {
            edge: false,
            type_name: "Person".into(),
            id: "p2".into(),
            property: "doc".into(),
        };
        let theirs = session.update(cx, |session, cx| {
            session.blob_put_on("main", cell, None, b"theirs".to_vec(), "theirs".into(), cx)
        });
        cx.run_until_parked();
        theirs.await;
        let file = crate::test_support::upload_file("newest.txt", b"newest", None);
        table.update(cx, |table, cx| table.upload_blob("p1", "doc", file, cx));
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert_eq!(table.paging.pinned.as_deref(), Some("c1"), "behind");
            assert_eq!(table.paging.blobs, crate::model::BlobsAt::Own("c3".into()));
        });
        let peek = table.update(cx, |table, cx| table.peek_row(0, cx));
        cx.run_until_parked();
        peek.read_with(cx, |peek, _| {
            assert_eq!(peek.reads_at(), Some("c1"), "the table's pin");
            assert_eq!(peek.blob_size("doc"), Some(6), "not the value replaced");
            assert_eq!(peek.blobs_held_at(), Some("c3"));
        });
    }

    /// A date or a time reads relative to now: what its cell shows is made once a minute, kept
    /// with the cell's other text, not parsed and spelled again at every draw.
    #[gpui::test]
    async fn a_dates_relative_text_is_kept_for_its_minute(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        const SEEN: &str =
            "node Person {\n  slug: String @key\n  name: String\n  seen: DateTime?\n}\n";
        let rows = serde_json::json!([
            {"n": {"@id": "p1", "slug": "p1", "name": "Ada", "seen": "2020-01-02T03:04:05"}},
        ]);
        let session = crate::test_support::session_reading(SEEN, rows, cx);
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::new(
                WeakEntity::new_invalid(),
                session,
                "Person".into(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        table.update(cx, |table, cx| {
            let made = |table: &TypeTable| {
                let cells = table.cell_text.borrow();
                cells.get("p1").and_then(|cells| cells.get("seen")).cloned()
            };
            let made_then = made(table).expect("made as the row drew");
            assert_eq!(made_then.minute, Some(table.frame_minute));
            assert_eq!(made_then.shown.as_ref(), "2020-01-02");
            assert_eq!(
                made_then.exact.as_deref(),
                Some("2020-01-02 03:04:05.000 UTC")
            );
            // Made in an earlier minute, it is made again; within it, kept as made.
            if let Some(cell) = table
                .cell_text
                .borrow_mut()
                .get_mut("p1")
                .and_then(|cells| cells.get_mut("seen"))
            {
                cell.shown = "kept".into();
            }
            let (row, column) = (table.rows[0].clone(), table.columns[2].clone());
            assert_eq!(column.key, "seen");
            let value = row.value("seen").clone();
            assert_eq!(
                table.cell_text_of(&row, &column, &value).shown.as_ref(),
                "kept"
            );
            table.frame_minute += 1;
            let again = table.cell_text_of(&row, &column, &value);
            assert_eq!(again.shown.as_ref(), "2020-01-02");
            assert_eq!(again.minute, Some(table.frame_minute));
            cx.notify();
        });
    }

    /// Whether a read asks for the rows without the sort's value.
    fn reads_the_rows_without_a_value(request: &Value) -> bool {
        request["query"]
            .as_str()
            .is_some_and(|query| query.contains("is null"))
    }

    /// A page or a section read before the table's own write landed answers for the commit
    /// before it: a row deleted there would come back under the write's commit. It is read
    /// again at the pin the write moved the table to (constitution II).
    #[gpui::test]
    async fn a_read_sent_before_the_tables_own_write_landed_is_read_again_at_its_commit(
        cx: &mut TestAppContext,
    ) {
        let ada = serde_json::json!({"n": {"@id": "p1", "slug": "p1", "name": "Ada"}});
        let grace = serde_json::json!({"n": {"@id": "p2", "slug": "p2", "name": "Grace"}});
        let (session, _) = crate::test_support::session_routing(
            PEOPLE,
            move |request| {
                if request["name"] == "studio_value_counts" {
                    return serde_json::json!([{"value": "Grace", "n": 1}]);
                }
                if reads_the_rows_without_a_value(request) {
                    return serde_json::json!([]);
                }
                // p1 is deleted at c2, and p2 at c3.
                match request["at"].as_str() {
                    Some("c2") => serde_json::json!([grace.clone()]),
                    Some("c3") => serde_json::json!([]),
                    _ => serde_json::json!([ada.clone(), grace.clone()]),
                }
            },
            cx,
        );
        let (table, cx) = people_table(&session, cx);
        let ids = |table: &TypeTable| -> Vec<String> {
            table.rows.iter().map(|row| row.id.clone()).collect()
        };
        // A header clicked reads the first page at c1; the table's delete of p1 lands first.
        table.update(cx, |table, cx| {
            table.cycle_sort("name".into(), cx);
            table.rows_deleted("main", &["p1".to_owned()], "c2", Some("c1"), cx);
        });
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert_eq!(table.paging.pinned.as_deref(), Some("c2"));
            assert_eq!(table.reads_sent_again, 1, "read again at c2");
            assert_eq!(ids(table), ["p2"], "the deleted row does not come back");
        });

        // Grouped, a section's page read at c2 lands after the delete of p2 at c3.
        table.update(cx, |table, cx| table.set_grouping(Some("name".into()), cx));
        cx.run_until_parked();
        table.update(cx, |table, cx| {
            table.section_tasks.clear();
            table.rows.clear();
            if let Some(section) = table.grouping.as_mut().and_then(|g| g.section_mut(0)) {
                section.loading = false;
                section.range = 0..0;
                section.paging = crate::model::Paging::default();
            }
            table.load_section(0, SECTION_FIRST_PAGE, cx);
            table.rows_deleted("main", &["p2".to_owned()], "c3", Some("c2"), cx);
        });
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert_eq!(table.paging.pinned.as_deref(), Some("c3"));
            assert!(ids(table).is_empty(), "{:?}", ids(table));
        });
    }

    /// Sorted by a property no row holds, the first page of rows with a value is empty: the
    /// rows without one are read at once, not left for a scroll that cannot happen.
    #[gpui::test]
    async fn a_sorted_table_whose_rows_hold_no_value_reads_them_at_once(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        const MAILED: &str =
            "node Person {\n  slug: String @key\n  name: String\n  email: String?\n}\n";
        let (session, _) = crate::test_support::session_routing(
            MAILED,
            |request| {
                let query = request["query"].as_str().unwrap_or_default();
                if query.contains("is not null") {
                    return serde_json::json!([]);
                }
                serde_json::json!([
                    {"n": {"@id": "p1", "slug": "p1", "name": "Ada"}},
                    {"n": {"@id": "p2", "slug": "p2", "name": "Grace"}},
                ])
            },
            cx,
        );
        let opening = Opening {
            filters: vec![Filter {
                property: "email".into(),
                op: FilterOp::IsEmpty,
                value: Value::Null,
            }],
            sort: Some(Sort {
                property: "email".into(),
                descending: false,
            }),
            ..Opening::default()
        };
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::opening(
                WeakEntity::new_invalid(),
                session,
                "Person".into(),
                None,
                opening,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            let ids: Vec<&str> = table.rows.iter().map(|row| row.id.as_str()).collect();
            assert_eq!(ids, ["p1", "p2"]);
            assert!(table.paging.exhausted);
        });
    }

    /// Every edit has an editor of its own: ⌘Z in the next cell takes nothing back from the
    /// cell edited before, and ↵ then writes nothing there.
    #[gpui::test]
    async fn undo_in_a_cell_never_brings_back_another_cells_text(cx: &mut TestAppContext) {
        let (session, sent) = crate::test_support::session_recording_writes(PEOPLE, cx);
        let (table, cx) = people_table(&session, cx);
        table.update_in(cx, |table, window, cx| {
            let col = table.column_index("name").unwrap();
            table.start_edit(Cursor { row: 0, col }, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input("Ada L");
        cx.run_until_parked();
        table.update_in(cx, |table, window, cx| {
            assert_eq!(table.cell_editor.read(cx).text(cx), "Ada L");
            table.confirm_edit(window, cx);
            let col = table.column_index("name").unwrap();
            table.start_edit(Cursor { row: 1, col }, window, cx);
            table.cell_editor.update(cx, |editor, cx| {
                editor.undo(&editor::actions::Undo, window, cx)
            });
            assert_eq!(table.cell_editor.read(cx).text(cx), "Grace");
            table.confirm_edit(window, cx);
        });
        cx.run_until_parked();
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "only p1's name: {sent:?}");
        table.read_with(cx, |table, _| {
            assert_eq!(table.rows[1].value("name"), "Grace");
            assert!(table.draft_text("p2", "name").is_none());
        });
    }

    /// Discard drops the drafts the pill counts; one whose save is out stays the session's
    /// until the save settles it, so a refused save puts its text back.
    #[gpui::test]
    async fn discard_leaves_a_draft_whose_save_is_out(cx: &mut TestAppContext) {
        let refused = serde_json::json!({ "error": "policy denied", "code": "forbidden", "disposition": { "action": "refresh", "effects": "none" } });
        let session = crate::test_support::session_answering(PEOPLE, 403, refused, cx);
        let (table, cx) = people_table(&session, cx);
        let key = |id: &str| DraftKey {
            type_name: "Person".into(),
            id: id.into(),
            property: "name".into(),
        };
        session.update(cx, |session, cx| {
            for (id, old, text) in [("p1", "Ada", "Ada King"), ("p2", "Grace", "Grace H.")] {
                let draft = DraftEntry {
                    old: old.into(),
                    text: text.into(),
                };
                session.set_draft(key(id), draft, cx);
            }
        });
        cx.run_until_parked();
        table.update_in(cx, |table, window, cx| {
            let col = table.column_index("name").unwrap();
            table.start_edit(Cursor { row: 0, col }, window, cx);
            table.confirm_edit(window, cx);
            table.discard_drafts(cx);
            table.discard_drafts(cx);
            assert!(matches!(
                table.cells.get(&("p1".to_owned(), "name".to_owned())),
                Some(CellState::Saving { .. })
            ));
            assert!(table.draft_text("p2", "name").is_none());
        });
        session.read_with(cx, |session, _| {
            assert!(session.draft(&key("p1")).is_some(), "its save is out");
            assert!(session.draft(&key("p2")).is_none(), "discarded");
        });
        cx.run_until_parked();
        let kept = session.read_with(cx, |session, _| session.draft(&key("p1")).cloned());
        assert_eq!(
            kept.map(|draft| draft.text),
            Some("Ada King".to_owned()),
            "refused, the text is kept"
        );
    }

    /// Esc while a bulk set is out leaves it out: Set stays disabled, and a pick made meanwhile
    /// is neither told the outcome nor cleared by it.
    #[gpui::test]
    async fn esc_while_a_bulk_set_is_out_leaves_it_out(cx: &mut TestAppContext) {
        let mut both = crate::test_support::committed("c2", "c1");
        both["affected_nodes"] = 2.into();
        let session = crate::test_support::session_writing(PEOPLE, vec![(200, both)], cx);
        let (table, cx) = people_table(&session, cx);
        table.update_in(cx, |table, window, cx| {
            table.pick_rows(0, false, cx);
            table.pick_rows(1, false, cx);
            table.bulk.property = Some("name".into());
            table
                .bulk_editor
                .update(cx, |editor, cx| editor.set_text("Same", window, cx));
            table.submit_bulk(window, cx);
            assert!(table.bulk.saving());
            table.on_cancel(&Cancel, window, cx);
            assert!(table.selection.is_empty());
            assert!(table.bulk.saving(), "Esc leaves the set out");
            // Picked again meanwhile: Set does nothing while one is out.
            table.pick_rows(1, false, cx);
            table.pick_rows(0, false, cx);
            table.submit_bulk(window, cx);
            assert_eq!(table.saves_in_flight, 1, "sent once");
            table.pick_rows(0, false, cx);
        });
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert!(!table.bulk.saving());
            let picked: Vec<&str> = table.selection.iter().map(String::as_str).collect();
            assert_eq!(picked, ["p2"], "the pick made meanwhile stays");
            assert!(table.bulk.notice.is_none());
            assert_eq!(table.rows[0].value("name"), "Same");
        });
    }

    /// A row's checkbox checks it, or takes it out, without moving the cursor or opening
    /// anything; the key column's select-all checks every loaded row, shows a dash while some
    /// are, and takes them all out once all are; one checked row is enough for the bulk bar.
    #[gpui::test]
    async fn checkboxes_pick_rows_and_the_header_selects_all(cx: &mut TestAppContext) {
        let session = session(PEOPLE, cx);
        let (table, cx) = people_table(&session, cx);
        cx.run_until_parked();
        let click = |cx: &mut VisualTestContext, selector: &'static str| {
            let bounds = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} was not drawn"));
            cx.simulate_click(bounds.center(), gpui::Modifiers::none());
            cx.run_until_parked();
        };
        let picked = |cx: &mut VisualTestContext| {
            table.read_with(cx, |table, _| {
                table.selection.iter().cloned().collect::<Vec<String>>()
            })
        };
        let state = |cx: &mut VisualTestContext| table.read_with(cx, |t, _| t.all_check_state());
        let cursor = table.read_with(cx, |table, _| table.cursor);
        assert_eq!(state(cx), ToggleState::Unselected);
        click(cx, "all-rows-check");
        assert_eq!(picked(cx), ["p1", "p2"]);
        assert_eq!(state(cx), ToggleState::Selected);
        // With a row checked every row's box shows, so the first row's takes it out.
        click(cx, "row-check-0");
        assert_eq!(picked(cx), ["p2"]);
        assert_eq!(state(cx), ToggleState::Indeterminate);
        table.update(cx, |table, cx| {
            assert_eq!(table.cursor, cursor, "a checkbox moves no cursor");
            assert!(
                table.render_bulk_bar(cx).is_some(),
                "one checked row has the bar"
            );
        });
        click(cx, "all-rows-check");
        assert_eq!(picked(cx), ["p2", "p1"]);
        click(cx, "all-rows-check");
        assert!(
            picked(cx).is_empty(),
            "all checked, the select-all takes them out"
        );
        table.update(cx, |table, cx| assert!(table.render_bulk_bar(cx).is_none()));
    }

    /// The time of a frame of a full table while its grid replays from its cache, as in every
    /// frame an animation, a hover or a keystroke elsewhere in the window draws: GPUI builds the
    /// whole scene each frame, and a cached view's quads and lines go back into its draw order.
    #[gpui::test]
    #[ignore = "instrument: prints the time of a frame of a 200-row, 20-column table, its grid cached"]
    async fn instrument_a_frame_of_a_full_table(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        const COLUMNS: usize = 20;
        let properties: String = (0..COLUMNS)
            .map(|c| format!("  p{c:02}: String\n"))
            .collect();
        let schema = format!("node Wide {{\n  slug: String @key\n{properties}}}\n");
        let rows: Vec<Value> = (0..200)
            .map(|i| {
                let mut node = serde_json::Map::new();
                node.insert("@id".into(), format!("w{i}").into());
                node.insert("slug".into(), format!("wide-{i:06}").into());
                for c in 0..COLUMNS {
                    node.insert(format!("p{c:02}"), format!("value {i}·{c}").into());
                }
                serde_json::json!({ "n": node })
            })
            .collect();
        let session = crate::test_support::session_reading(&schema, Value::Array(rows), cx);
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::new(
                WeakEntity::new_invalid(),
                session,
                "Wide".into(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        let frame = |cx: &mut VisualTestContext| {
            table.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
        };
        for _ in 0..30 {
            frame(cx);
        }
        let frames = 600;
        let started = std::time::Instant::now();
        for _ in 0..frames {
            frame(cx);
        }
        let drawn = table.read_with(cx, |table, _| table.rows.len());
        eprintln!(
            "{drawn} rows loaded: {:?} a frame over {frames}",
            started.elapsed() / frames
        );
    }

    /// A grouped frame reads the Blob and relationship cells of the rows its lines show: a
    /// folded section between two open ones reads nothing, though its rows are loaded.
    #[gpui::test]
    async fn a_grouped_frame_reads_only_the_rows_its_lines_show(cx: &mut TestAppContext) {
        let schema = PEOPLE.replace("  name: String\n", "  name: String\n  doc: Blob?\n");
        let schema = format!("{schema}edge Knows: Person -> Person\n");
        let (session, _) = crate::test_support::session_routing(
            &schema,
            |request| match request["name"].as_str() {
                Some("studio_page_degrees") => serde_json::json!([]),
                _ => serde_json::json!([
                    {"n": {"@id": "p1", "slug": "p1", "name": "Ada"}},
                    {"n": {"@id": "p2", "slug": "p2", "name": "Grace"}},
                ]),
            },
            cx,
        );
        let (table, cx) = people_table(&session, cx);
        cx.executor().advance_clock(BLOB_READ_AFTER);
        cx.run_until_parked();
        // Three sections, the middle one folded: rows 0–2, 3–52 and 53–55.
        let ranges = [0..3, 3..53, 53..56];
        table.update(cx, |table, cx| {
            table.rows = (0..56)
                .map(|i| Row {
                    id: format!("q{i:02}"),
                    values: Default::default(),
                    dimmed: false,
                })
                .collect();
            let mut grouping = Grouping::new("name");
            grouping.restart();
            let counts: ReadOutput = serde_json::from_value(serde_json::json!({
                "query_name": "studio_value_counts",
                "target": {"branch": "main"},
                "row_count": 3,
                "columns": ["value", "n"],
                "rows": [
                    {"value": "A", "n": 3}, {"value": "B", "n": 50}, {"value": "C", "n": 3},
                ],
            }))
            .unwrap();
            grouping.accept_counts(&counts, GROUP_LIMIT);
            for (index, range) in ranges.iter().enumerate() {
                let section = grouping.section_mut(index).unwrap();
                section.range = range.clone();
                section.expanded = index != 1;
            }
            table.grouping = Some(grouping);
            table.blobs.clear();
            table.blob_tasks.clear();
            table.relations.clear();
            table.relation_tasks.clear();
            table.relations_asked.clear();
            table.relation_lines = None;
            table.blob_lines = None;
            cx.notify();
        });
        cx.run_until_parked();
        cx.executor().advance_clock(BLOB_READ_AFTER);
        cx.run_until_parked();
        let shown: HashSet<String> = [0, 1, 2, 53, 54, 55]
            .map(|i| format!("q{i:02}"))
            .into_iter()
            .collect();
        table.read_with(cx, |table, _| {
            assert!(!table.relations_drawn.is_empty());
            // One HEAD per Blob cell read, one id per relationship read.
            let blob_reads: HashSet<String> = table.blobs.0.keys().cloned().collect();
            assert_eq!(blob_reads, shown, "Blob cells read");
            let related: HashSet<String> = table
                .relations_asked
                .iter()
                .map(|(id, _)| id.clone())
                .collect();
            assert_eq!(related, shown, "relationships read");
            let asked = table.relations_asked.len();
            assert_eq!(asked, shown.len() * table.relations_drawn.len());
        });
    }

    /// A Blob column scrolled off sideways reads nothing; scrolled into view, its cells are
    /// read for the rows drawn.
    #[gpui::test]
    async fn blob_cells_are_read_in_the_columns_drawn(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let properties: String = (1..=20).map(|i| format!("  p{i:02}: String?\n")).collect();
        let schema = format!("node Person {{\n  slug: String @key\n{properties}  doc: Blob?\n}}\n");
        let (session, blobs) = crate::test_support::session_with_blobs(&schema, cx);
        blobs.set("Person/p1/doc", b"held");
        let (table, cx) = cx.add_window_view(|window, cx| {
            TypeTable::new(
                WeakEntity::new_invalid(),
                session,
                "Person".into(),
                None,
                window,
                cx,
            )
        });
        let settle = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            cx.executor().advance_clock(BLOB_READ_AFTER);
            cx.run_until_parked();
        };
        settle(cx);
        table.read_with(cx, |table, _| {
            assert!(table.blob_columns.is_empty(), "{:?}", table.blob_columns);
            assert!(table.blobs.0.is_empty(), "nothing read off to the right");
        });
        // Scrolled to the far end, the Blob column is drawn, and its cells are read.
        table.update_in(cx, |table, window, cx| {
            let total = table.width_from(1);
            let handle = table.table_state.read(cx).horizontal_scroll_handle.clone();
            let viewport = handle.bounds().size.width;
            handle.set_offset(point(-(total - viewport), px(0.)));
            let drawn = table.drawn_columns(window, cx);
            table.note_blob_columns(&drawn);
            assert_eq!(table.blob_columns, ["doc"]);
            table.load_blob_metas(0..2, cx);
        });
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert!(matches!(
                table.blobs.get("p1", "doc"),
                Some(Loadable::Loaded(BlobMeta::Managed { size: 4, .. }))
            ));
            assert!(table.blobs.contains("p2", "doc"));
        });
        // Many cells asked for at once are read four at a time, the first drawn first, each
        // read that ends starting the next.
        table.update(cx, |table, cx| {
            table.rows = (0..10)
                .map(|i| Row {
                    id: format!("r{i}"),
                    values: Default::default(),
                    dimmed: false,
                })
                .collect();
            table.load_blob_metas(0..10, cx);
            assert_eq!(table.blob_tasks.len(), BLOB_READS_AT_ONCE);
            assert_eq!(table.blob_queue.len(), 10 - BLOB_READS_AT_ONCE);
            assert_eq!(
                table.blob_queue.front().map(|(id, _)| id.as_str()),
                Some("r4")
            );
        });
        cx.run_until_parked();
        table.read_with(cx, |table, _| {
            assert!(table.blob_tasks.is_empty() && table.blob_queue.is_empty());
            for i in 0..10 {
                let cell = table.blobs.get(&format!("r{i}"), "doc");
                assert!(
                    cell.is_some_and(|cell| !matches!(cell, Loadable::Loading)),
                    "r{i}"
                );
            }
        });
    }

    /// Grouped, a section's read whose line is more than a screen away is dropped, freeing
    /// its slot for the lines in view, and reads again when its line comes back; rows a read
    /// puts in above the lines in view leave those lines where they are (constitution V).
    #[gpui::test]
    async fn section_reads_away_from_the_view_are_dropped_and_rows_above_it_move_nothing(
        cx: &mut TestAppContext,
    ) {
        let session = session(PEOPLE, cx);
        let (table, cx) = people_table(&session, cx);
        let page = |rows: Value| -> Result<ReadOutput, ReadError> {
            Ok(serde_json::from_value(serde_json::json!({
                "query_name": "studio_page",
                "target": {"branch": "main"},
                "row_count": 2,
                "columns": ["n"],
                "rows": rows,
                "graph_commit_id": "c1",
            }))
            .unwrap())
        };
        table.update(cx, |table, cx| {
            table.rows.clear();
            let mut grouping = Grouping::new("name");
            let generation = grouping.restart();
            let values: Vec<Value> = (0..40)
                .map(|i| serde_json::json!({"value": format!("v{i:02}"), "n": 2}))
                .collect();
            let counts: ReadOutput = serde_json::from_value(serde_json::json!({
                "query_name": "studio_value_counts",
                "target": {"branch": "main"},
                "row_count": 40,
                "columns": ["value", "n"],
                "rows": values,
            }))
            .unwrap();
            grouping.accept_counts(&counts, GROUP_LIMIT);
            table.grouping = Some(grouping);
            // Each section is a header and its read line: line 40 is section 20's header.
            let height = table.line_height(cx);
            {
                let state = table.table_state.read(cx).scroll_handle.0.borrow();
                state.base_handle.set_offset(point(px(0.), height * -40.));
            }
            assert_eq!(
                table.first_line_in_view(cx).map(|(line, _)| line),
                Some(Line::Header(20))
            );

            // Section 0's read is far above the lines drawn; section 21's is in view.
            for index in [0, 21] {
                let read = SectionRead {
                    rows: SECTION_FIRST_PAGE,
                    _task: Task::ready(()),
                };
                table.section_tasks.insert(index, read);
                table
                    .grouping
                    .as_mut()
                    .unwrap()
                    .section_mut(index)
                    .unwrap()
                    .loading = true;
            }
            table.drop_section_reads_away_from(40..60);
            let loading = |table: &TypeTable, index: usize| {
                table.grouping.as_ref().unwrap().sections()[index].loading
            };
            assert!(!table.section_tasks.contains_key(&0) && !loading(table, 0));
            assert!(table.section_tasks.contains_key(&21) && loading(table, 21));

            // Section 0's rows land above the view: the scroll moves with them.
            let rows = serde_json::json!([
                {"n": {"@id": "p1", "slug": "p1", "name": "v00"}},
                {"n": {"@id": "p2", "slug": "p2", "name": "v00"}},
            ]);
            table.apply_section_page(generation, 0, page(rows), 10, cx);
            assert_eq!(table.rows.len(), 2);
            assert_eq!(
                table.first_line_in_view(cx).map(|(line, _)| line),
                Some(Line::Header(20)),
                "the same line is first in view"
            );
        });
    }

    /// A table's upload is bound as it is sent: where the values it shows hold then, past its
    /// pin once its own write moved it, and on the branch it was sent to only. The table's
    /// insert of a row with a value in its Blob lands on its pin while the file is read: the
    /// type's table moved, and every ETag of it with it, so the upload carries the ETag read at
    /// the insert's commit and is stored. Moved to another branch meanwhile, the table no longer
    /// shows the cell: the upload goes to the branch it was sent on, bound to what the cell
    /// showed when the file was chosen.
    #[gpui::test]
    async fn a_tables_upload_is_bound_where_its_values_hold_as_it_goes(cx: &mut TestAppContext) {
        for moves_branch in [false, true] {
            let (session, blobs) = crate::test_support::session_with_blobs(DOCS, cx);
            blobs
                .versioned
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let chosen = blobs.set("Person/p1/doc", b"one");
            let (table, cx) = people_table(&session, cx);
            cx.executor().advance_clock(BLOB_READ_AFTER);
            cx.run_until_parked();
            table.read_with(cx, |table, _| {
                assert!(matches!(
                    table.blobs.get("p1", "doc"),
                    Some(Loadable::Loaded(BlobMeta::Managed { etag: Some(etag), .. }))
                        if *etag == chosen
                ));
            });
            let file = crate::test_support::upload_file("bound-in-table.txt", b"bound", None);
            table.update(cx, |table, cx| {
                table.upload_blob("p1", "doc", file, cx);
                if moves_branch {
                    session.update(cx, |session, cx| session.set_branch("fix".into(), cx));
                } else {
                    // While the file is read, the insert's commit gives p3 a value, and every
                    // cell of the type a new ETag; the insert lands on the table's pin.
                    blobs.set("Person/p3/doc", b"three");
                    let values = [("slug", "p3"), ("name", "Lin")]
                        .into_iter()
                        .map(|(key, value)| (key.to_owned(), Value::String(value.into())))
                        .collect();
                    table.row_inserted("main", Some("p3".into()), values, "c2", Some("c1"), cx);
                    assert_eq!(table.paging.blobs_at(), Some("c2"));
                }
            });
            let now = blobs.get("Person/p1/doc").unwrap().1;
            if !moves_branch {
                assert_ne!(now, chosen, "the insert moved p1's ETag");
            }
            cx.run_until_parked();
            let writes = blobs.writes.lock().unwrap().clone();
            assert_eq!(writes.len(), 1, "{writes:?}");
            assert_eq!(
                writes[0].branch, "main",
                "sent on the branch it was chosen on"
            );
            let bound = if moves_branch { &chosen } else { &now };
            assert_eq!(writes[0].if_match.as_deref(), Some(bound.as_str()));
            assert_eq!(blobs.get("Person/p1/doc").unwrap().0, b"bound", "stored");
            // Not refused: what its toast tells is a commit on main, which a table moved to
            // fix shows nothing of; a table still on main shows no notice on the cell either.
            session.read_with(cx, |session, _| {
                let told = session
                    .outcomes
                    .last()
                    .expect("the upload's outcome is logged");
                assert_eq!(told.branch, "main");
                assert!(
                    matches!(told.outcome, graph_store::OutcomeKind::Committed { .. }),
                    "{:?}",
                    told.outcome
                );
            });
            table.read_with(cx, |table, _| {
                let key = ("p1".to_owned(), "doc".to_owned());
                assert!(
                    !table.blob_edits.contains_key(&key),
                    "no notice on the cell"
                );
            });
        }
    }

    /// Folding a grouped section moves the rows of the next one up into the lines drawn, the
    /// same lines as before: their Blob and relationship cells are read, as rows put in or taken
    /// out have theirs read. Before, the frame drew the same lines, asked for nothing, and the
    /// moved rows showed a grey "Blob" whose Choose File refused until a scroll.
    #[gpui::test]
    async fn folding_a_section_reads_the_cells_of_the_rows_it_brings_into_view(
        cx: &mut TestAppContext,
    ) {
        let schema = format!("{DOCS}edge Knows: Person -> Person\n");
        let (session, _) = crate::test_support::session_routing(
            &schema,
            |request| match request["name"].as_str() {
                Some("studio_page_degrees") => serde_json::json!([]),
                _ => serde_json::json!([
                    {"n": {"@id": "p1", "slug": "p1", "name": "Ada"}},
                    {"n": {"@id": "p2", "slug": "p2", "name": "Grace"}},
                ]),
            },
            cx,
        );
        let (table, cx) = people_table(&session, cx);
        let settle = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            cx.executor().advance_clock(BLOB_READ_AFTER);
            cx.run_until_parked();
        };
        settle(cx);
        // Two open sections of 300 rows each: more than the view holds either way, and than
        // the relationship reads ahead of it.
        table.update(cx, |table, cx| {
            table.rows = (0..600)
                .map(|i| Row {
                    id: format!("q{i:03}"),
                    values: Default::default(),
                    dimmed: false,
                })
                .collect();
            let mut grouping = Grouping::new("name");
            grouping.restart();
            let counts: ReadOutput = serde_json::from_value(serde_json::json!({
                "query_name": "studio_value_counts",
                "target": {"branch": "main"},
                "row_count": 2,
                "columns": ["value", "n"],
                "rows": [{"value": "A", "n": 300}, {"value": "B", "n": 300}],
            }))
            .unwrap();
            grouping.accept_counts(&counts, GROUP_LIMIT);
            for (index, range) in [0..300, 300..600].into_iter().enumerate() {
                grouping.section_mut(index).unwrap().range = range;
            }
            table.grouping = Some(grouping);
            table.blobs.clear();
            table.blob_tasks.clear();
            table.relations.clear();
            table.relation_tasks.clear();
            table.relations_asked.clear();
            table.lines_moved();
            cx.notify();
        });
        settle(cx);
        let read = |table: &TypeTable, id: &str| {
            let related = table.relations_asked.iter().any(|(row, _)| row == id);
            (table.blobs.contains(id, "doc"), related)
        };
        let drawn = table.read_with(cx, |table, _| {
            assert!(!table.relations_drawn.is_empty());
            assert_eq!(read(table, "q000"), (true, true), "A's first row is drawn");
            assert_eq!(read(table, "q300"), (false, false), "B's is far below");
            table.lines_drawn.clone()
        });

        table.update(cx, |table, cx| table.toggle_section(0, cx));
        settle(cx);
        table.read_with(cx, |table, _| {
            assert_eq!(table.lines_drawn, drawn, "the same lines are drawn");
            assert_eq!(
                read(table, "q300"),
                (true, true),
                "B's rows moved up are read"
            );
        });
    }

    /// Another writer's commit to another type moves the table's pin while a read is out: the
    /// read answers for what the head holds as well, so it lands, and is not sent again. Before,
    /// every pin move sent it again: the session answers that from the first read's answer when
    /// it still keeps it and knows every commit since, and otherwise asks the server again, so
    /// under a writer committing every few seconds a slow read could keep missing its pin. A
    /// read out when the table's own write moved the pin is still read again
    /// (`a_read_sent_before_the_tables_own_write_landed_is_read_again_at_its_commit`).
    #[gpui::test]
    async fn a_read_out_when_a_commit_to_another_type_moves_the_pin_lands(cx: &mut TestAppContext) {
        let pages = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = pages.clone();
        let (session, writer) = session_followed(
            &format!("{PEOPLE}node Note {{\n  slug: String @key\n}}\n"),
            move |request| {
                if request["name"] == "studio_head" {
                    return serde_json::json!([]);
                }
                if request["name"] == "studio_page" {
                    let at = request["at"].as_str().unwrap_or_default().to_owned();
                    seen.lock().unwrap().push(at);
                }
                serde_json::json!([
                    {"n": {"@id": "p1", "slug": "p1", "name": "Ada"}},
                    {"n": {"@id": "p2", "slug": "p2", "name": "Grace"}},
                ])
            },
            cx,
        );
        let (table, cx) = people_table(&session, cx);
        table.read_with(cx, |table, _| {
            assert_eq!(table.paging.pinned.as_deref(), Some("c1"))
        });
        // A write of the table's is out as the other writer's c2 lands, so the table compares
        // the head once it settles; meanwhile the session reads what c2 changed: a Note.
        table.update(cx, |table, _| table.save_started());
        writer.commit("c2", vec![changed("node", "Note", "n1", None)]);
        session.update(cx, |session, cx| session.check_heads(cx));
        cx.run_until_parked();
        pages.lock().unwrap().clear();

        table.update(cx, |table, cx| {
            // A new order or filter reads the first page again at c1, and while it is out the
            // write settles and the pin moves to c2: nothing the table reads changed there.
            table.reload(true, cx);
            table.save_finished(cx);
            assert_eq!(table.paging.pinned.as_deref(), Some("c2"));
            assert!(!table.newer);
        });
        cx.run_until_parked();
        assert_eq!(*pages.lock().unwrap(), ["c1"], "one request");
        table.read_with(cx, |table, _| {
            assert_eq!(table.reads_sent_again, 0, "the read landed as it was");
            let ids: Vec<&str> = table.rows.iter().map(|row| row.id.as_str()).collect();
            assert_eq!(ids, ["p1", "p2"]);
            assert_eq!(table.paging.pinned.as_deref(), Some("c2"));
        });
    }

    /// A section's "Show more" read dropped while its line was a screen away is asked for again
    /// when its line is drawn, for as many rows as it asked for, as a section's first read is.
    /// Before, only a section with no rows read again, and the dropped page never came. The
    /// read is the one Show more sends, and the view moves away before it is answered.
    #[gpui::test]
    async fn a_show_more_dropped_away_from_the_view_reads_again_when_its_line_returns(
        cx: &mut TestAppContext,
    ) {
        let pages = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let seen = pages.clone();
        let (session, _) = crate::test_support::session_routing(
            PEOPLE,
            move |request| {
                if request["name"] == "studio_page" {
                    seen.lock().unwrap().push(request.clone());
                }
                serde_json::json!([])
            },
            cx,
        );
        let (table, cx) = people_table(&session, cx);
        let page = table.read_with(cx, |table, cx| table.page_size(cx));
        // Section 0 is open with two of its twelve rows; 150 folded sections follow it.
        table.update(cx, |table, cx| {
            table.rows = ["p1", "p2"]
                .map(|id| Row {
                    id: id.into(),
                    values: Default::default(),
                    dimmed: false,
                })
                .into();
            let mut grouping = Grouping::new("name");
            grouping.restart();
            let values: Vec<Value> = (0..151)
                .map(|i| serde_json::json!({"value": format!("v{i:03}"), "n": 12}))
                .collect();
            let counts: ReadOutput = serde_json::from_value(serde_json::json!({
                "query_name": "studio_value_counts",
                "target": {"branch": "main"},
                "row_count": values.len(),
                "columns": ["value", "n"],
                "rows": values,
            }))
            .unwrap();
            grouping.accept_counts(&counts, GROUP_LIMIT);
            for index in 1..151 {
                grouping.section_mut(index).unwrap().expanded = false;
            }
            grouping.section_mut(0).unwrap().range = 0..2;
            table.grouping = Some(grouping);
            table.lines_moved();
            cx.notify();
        });
        cx.run_until_parked();
        pages.lock().unwrap().clear();
        let scroll = |line: f32, table: &mut TypeTable, cx: &mut Context<TypeTable>| {
            let height = table.line_height(cx);
            {
                let state = table.table_state.read(cx).scroll_handle.0.borrow();
                state.base_handle.set_offset(point(px(0.), height * -line));
            }
            cx.notify();
        };

        // Show more asks for a page, as its button does; before it is answered the view
        // scrolls to the end, where the read line (line 3) is far above the lines drawn. The
        // frame drawn as the update ends drops the read.
        table.update(cx, |table, cx| {
            table.load_section(0, page, cx);
            assert!(table.section_tasks.contains_key(&0), "the read is out");
            assert!(table.grouping.as_ref().unwrap().sections()[0].loading);
            scroll(150., table, cx);
        });
        table.read_with(cx, |table, _| {
            let drawn = &table.lines_drawn;
            assert!(
                drawn.start > 3 + drawn.len(),
                "a screen from line 3: {drawn:?}"
            );
            assert!(!table.section_tasks.contains_key(&0));
            assert_eq!(table.dropped_sections.get(&0), Some(&page));
            assert!(!table.grouping.as_ref().unwrap().sections()[0].loading);
        });
        // The dropped read may have reached the server before it stopped; a frame away from
        // the line asks for nothing more.
        cx.run_until_parked();
        pages.lock().unwrap().clear();
        table.update(cx, |table, cx| scroll(149., table, cx));
        cx.run_until_parked();
        assert!(pages.lock().unwrap().is_empty());
        table.read_with(cx, |table, _| {
            assert_eq!(table.dropped_sections.get(&0), Some(&page))
        });

        // Scrolled back, its line is drawn and the page is asked for again.
        table.update(cx, |table, cx| scroll(0., table, cx));
        cx.run_until_parked();
        let pages = pages.lock().unwrap().clone();
        assert_eq!(pages.len(), 1, "{pages:?}");
        let query = pages[0]["query"].as_str().unwrap_or_default();
        assert!(query.contains(&format!("limit {page}")), "{query}");
        assert!(pages[0]["params"].to_string().contains("v000"), "{pages:?}");
        table.read_with(cx, |table, _| {
            assert!(table.dropped_sections.is_empty());
        });
    }
}

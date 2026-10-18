//! One type's table (spec 004): keyset pages pinned to one commit, engine-side filters and
//! counts, a cell cursor, value-bound cell edits with their three outcomes, and a peek.
//!
//! The grid is Zed's `ui::Table` (`crates/ui/src/components/data_table.rs` at v1.23.2) in the
//! configuration Zed's `tabular_data_preview` uses: resizable columns, the first column pinned,
//! rows virtualized by `uniform_list`.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use collections::IndexSet;
use editor::{Editor, EditorEvent};
use gpui::{
    Anchor, AnyView, App, ClickEvent, ClipboardItem, Context, DragMoveEvent, ElementId, Entity,
    EventEmitter, FocusHandle, Focusable, KeyContext, MouseButton, MouseDownEvent, Pixels, Render,
    ScrollStrategy, SharedString, StyleRefinement, Subscription, Task, WeakEntity, Window,
    anchored, deferred, point,
};
use gq_language::{
    BuildError, BuiltQuery, Filter, FilterOp, PageStart, Property, RelationshipGroup, ScalarKind,
    SchemaModel, Sort, builder,
};
use graph_client::api::ReadOutput;
use graph_client::{BlobCell, BlobMeta, HistoricalGap, PropertyThen, ReadError};
use graph_store::carry::{Since, TypeRef, edge_reads, node_reads};
use graph_store::{DraftEntry, DraftKey, GraphSession, Loadable, ReadWatch, SessionEvent};
use serde_json::Value;
use studio_actions::table::{
    AddFilter, Cancel, ClearFilters, ConfirmEdit, CopyCell, DeleteRow, EditCell, ExploreRow,
    FilterBySelection, InsertRow, MoveDown, MoveLeft, MoveRight, MoveUp, OpenRowPage, PeekRow,
    RefreshTable, SaveDrafts, SelectNextSuggestion, SelectPreviousSuggestion, StageEdit,
};
use studio_ui::{ColumnDrag, ColumnResize, ROW_HEIGHT};
use studio_workspace::{
    Item, ItemEvent, ItemFooter, KeyHint, SavedItem, ToastKind, Workspace, WorkspaceEvent,
};
use ui::prelude::*;
use ui::{
    ColumnWidthConfig, ContextMenu, ContextMenuEntry, Indicator, PopoverMenu, PopoverMenuHandle,
    ResizableColumnsState, Table, TableInteractionState, TableResizeBehavior, TintColor, Tooltip,
};

use crate::dialogs::{
    DeleteRowModal, DeleteTarget, InsertEdgeModal, InsertRowModal, SaveViewModal,
};
use crate::grouping::{GROUP_LIMIT, Grouping, Line, SECTION_FIRST_PAGE};
use crate::model::{
    ColumnLayout, ColumnRole, ColumnSpec, Notice, Paging, RELATION_CHIPS_SHOWN, Row, SaveResult,
    TableKind, Tone, changed_elsewhere, chips_that_fit, copy_text, count_of, display_value,
    edge_columns, edge_rows, edit_text, field_rows, filter_parts, format_count, insert_position,
    node_columns, node_rows, parse_filter_list, parse_filter_value, parse_input, preset_start,
    reads_as_markdown, relation_sample, row_matches, same_value, save_result, short_commit,
};
use crate::offers::Offers;
use crate::peek::{KnownRelation, NEIGHBOUR_LIMIT, Neighbour, NodePeek, PeekSubject};
use crate::suggest::{self, NODE_SUGGESTIONS};

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
    /// The save is out; the cell shows an amber dot.
    Saving,
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
    /// The value the cell showed: the save is bound to it.
    old: Value,
    notice: Option<Notice>,
}

struct FilterDraft {
    property: String,
    op: FilterOp,
    error: Option<String>,
    /// What the value field offers for its text, the one ↵ picks, and the node search under way.
    offers: Offers,
    suggest_generation: u64,
    _suggest_task: Option<Task<()>>,
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

/// One draft being saved: the cell, the value it showed (the save is bound to it), the new
/// value, and the text to put back if the save does not land.
#[derive(Debug, Clone)]
struct DraftEdit {
    row_id: String,
    column: String,
    old: Value,
    new: Value,
    text: String,
}

/// A bulk set being written: the property, and what came of the last try.
#[derive(Default)]
struct BulkDraft {
    property: Option<String>,
    notice: Option<Notice>,
    saving: bool,
}

/// A property's icon, by what it holds: text, a number, a time, a choice, a file.
fn property_icon(property: &Property) -> IconName {
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

/// One row's neighbours through one relationship: how many, and the first few by id (as many
/// as the peek shows; the cell shows `RELATION_CHIPS_SHOWN`). `whole` when the row's whole list
/// was read, so these are its first ones and the peek needs no read.
struct Relation {
    count: Result<i64, String>,
    neighbours: Vec<(String, Option<String>)>,
    whole: bool,
}

/// `id → n` from a `page_degrees` result.
fn relation_counts(rows: &Value) -> HashMap<String, i64> {
    rows.as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| Some((row.get("id")?.as_str()?.to_owned(), row.get("n")?.as_i64()?)))
        .collect()
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
    /// Every column the schema gives this type, in schema order.
    all_columns: Vec<ColumnSpec>,
    /// Which of them show, and in what order.
    layout: ColumnLayout,
    /// The columns shown, in order: what the grid, the cursor and editing address.
    columns: Vec<ColumnSpec>,
    /// A column header's right-click menu, where it was opened.
    header_menu: Option<(Entity<ContextMenu>, gpui::Point<Pixels>, Subscription)>,
    rows: Vec<Row>,
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
    frame_lines: Option<Rc<Vec<Line>>>,
    /// How the lines drawn this frame look, filled by the row callback for the row styling.
    frame_looks: Rc<RefCell<HashMap<usize, RowLook>>>,
    /// The columns the last drawn rows drew.
    #[cfg(test)]
    frame_drawn: Vec<bool>,
    /// A column being dragged wider or narrower: its key, and where the pointer and the column's
    /// width were when the drag began.
    resizing: Option<ColumnResize<String>>,
    /// The time relative dates are read against, taken once a frame rather than once a cell
    /// (reading the local time zone is not free).
    frame_now: chrono::DateTime<chrono::Local>,
    /// The grid as a view of its own, drawn again whenever the table notifies.
    grid: Entity<TableGrid>,
    /// Where the grid drew the cell being edited; the cell's editor is drawn over it from
    /// outside the grid, so a keystroke does not draw the grid again.
    edit_slot: studio_ui::OverlaySlot,
    /// The rows whose Blob cells were last asked for, and the pause before they are read.
    blob_rows: Option<Range<usize>>,
    _blob_wait: Option<Task<()>>,
    /// Labels of nodes picked for "related to" filters, so their pills read by name.
    node_labels: HashMap<String, String>,
    /// The filter being written's property and operator menus; the offered values hide while
    /// either is open.
    filter_menus: [PopoverMenuHandle<ContextMenu>; 2],
    /// The toolbar's Views, Columns and Group menus, whose buttons keep their tooltips quiet
    /// while open.
    toolbar_menus: [PopoverMenuHandle<ContextMenu>; 3],
    filter_draft: Option<FilterDraft>,
    filter_editor: Entity<Editor>,
    /// Rows picked for a bulk set (⌘-click, ⇧-click), by id, and the row ⇧ extends from.
    /// The picked rows' ids, in the order picked; checked for every row drawn.
    selection: IndexSet<String>,
    selection_anchor: Option<usize>,
    bulk: BulkDraft,
    bulk_editor: Entity<Editor>,
    drafts: DraftsBar,
    cursor: Option<Cursor>,
    editing: Option<Editing>,
    cell_editor: Entity<Editor>,
    /// Cells with a save out, and the session's drafts for this type as of its last change:
    /// a view rebuilt from the session ([`TypeTable::sync_drafts`]), never written on its own.
    cells: HashMap<(String, String), CellState>,
    /// Why a draft's last save did not land, by cell.
    draft_notices: HashMap<(String, String), Notice>,
    /// What each Blob cell drawn so far holds, by row id and property, read at the page's
    /// commit; and the reads under way.
    blobs: HashMap<(String, String), Loadable<BlobMeta>>,
    blob_tasks: HashMap<(String, String), Task<()>>,
    /// Each row's neighbours in the relationship columns, by row id and relationship key;
    /// read for each page at the page's commit.
    relations: HashMap<(String, String), Relation>,
    relation_tasks: Vec<Task<()>>,
    /// The relationship columns rows drew in the last frame, by key. Only these read their
    /// neighbours: a type with dozens of relationships reads the few on screen (spec 015).
    relations_drawn: HashSet<String>,
    /// Rows and relationship keys whose read is out or done, so a column scrolled back into
    /// view is not asked again.
    relations_asked: HashSet<(String, String)>,
    /// Properties the commit shown does not hold under their current name, and what they were
    /// then. Rows are read without them.
    absent: HashMap<String, PropertyThen>,
    peek: Option<(String, Entity<NodePeek>)>,
    newer: bool,
    /// Writes out for this table: its own saves and the dialogs' inserts and deletes. Each moves
    /// the pin itself when it lands, so a head the session sees meanwhile waits for them.
    saves_in_flight: usize,
    /// The session saw the head move while a save was out; compared once they are all back.
    head_moved_while_saving: bool,
    /// What the table reads, told to the session for its branch: a head move that changed none
    /// of it moves the pin without reading again (spec 015).
    reads: BTreeSet<TypeRef>,
    _reads: Option<ReadWatch>,
    /// The head moved and the session is reading what changed.
    awaiting_changes: bool,
    focus_handle: FocusHandle,
    table_state: Entity<TableInteractionState>,
    widths: Entity<ResizableColumnsState>,
    page_task: Option<Task<()>>,
    count_task: Option<Task<()>>,
    total_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ItemEvent> for TypeTable {}

impl Focusable for TypeTable {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for TypeTable {
    /// The grid is drawn cached on its own (`TableGrid`).
    fn caches_its_parts(&self) -> bool {
        true
    }

    fn tab_title(&self, _cx: &App) -> SharedString {
        match &self.at_commit {
            Some(commit) => format!("{} @ {}", self.type_name, short_commit(commit)).into(),
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

    fn is_dirty(&self, _cx: &App) -> bool {
        self.saves_in_flight > 0
            || self
                .cells
                .values()
                .any(|state| matches!(state, CellState::Draft { .. }))
    }

    fn is_stale(&self, _cx: &App) -> bool {
        self.newer
    }

    fn footer(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Option<ItemFooter> {
        Some(self.footer_parts(cx))
    }
}

impl TypeTable {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        session: Entity<GraphSession>,
        type_name: String,
        at_commit: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let cell_editor = cx.new(|cx| Editor::single_line(window, cx));
        let filter_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Value", window, cx);
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
        let mut subscriptions = vec![
            cx.subscribe_in(&session, window, Self::on_session_event),
            cx.subscribe_in(
                &cell_editor,
                window,
                |this, _, event: &EditorEvent, _window, cx| match event {
                    EditorEvent::Blurred => this.cell_editor_blurred(cx),
                    // Only a cleared notice changes what the grid shows; the editor draws
                    // over the grid (`edit_slot`), so typing leaves the grid as it was.
                    EditorEvent::BufferEdited => {
                        if let Some(editing) = &mut this.editing
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
                },
            ),
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
                    EditorEvent::Focused | EditorEvent::Blurred => cx.notify(),
                    _ => {}
                },
            ),
        ];
        if let Some(workspace) = workspace.upgrade() {
            subscriptions.push(cx.subscribe_in(
                &workspace,
                window,
                |this, workspace, event: &WorkspaceEvent, window, cx| match event {
                    WorkspaceEvent::SessionChanged => {
                        let Some(session) = workspace.read(cx).session().cloned() else {
                            return;
                        };
                        if session != this.session {
                            this.switch_session(session, window, cx);
                        }
                    }
                },
            ));
        }
        let branch = session.read(cx).branch.clone();
        let table_state = cx.new(|cx| TableInteractionState::new(cx));
        let widths = cx.new(|_| {
            ResizableColumnsState::new(1, vec![px(160.)], vec![TableResizeBehavior::Resizable])
        });
        let mut this = Self {
            workspace,
            session,
            type_name,
            branch,
            at_commit,
            kind: TableKind::Node,
            schema: None,
            missing: false,
            all_columns: Vec::new(),
            layout: ColumnLayout::default(),
            columns: Vec::new(),
            header_menu: None,
            rows: Vec::new(),
            paging: Paging::default(),
            first_page: Loadable::NotLoaded,
            loading_more: false,
            more_error: None,
            count: Loadable::NotLoaded,
            total: Loadable::NotLoaded,
            filters: Vec::new(),
            sort: None,
            grouping: None,
            frame_lines: None,
            frame_looks: Rc::default(),
            #[cfg(test)]
            frame_drawn: Vec::new(),
            frame_now: chrono::Local::now(),
            grid,
            edit_slot: Default::default(),
            resizing: None,
            blob_rows: None,
            _blob_wait: None,
            node_labels: HashMap::new(),
            filter_menus: Default::default(),
            toolbar_menus: Default::default(),
            filter_draft: None,
            filter_editor,
            selection: IndexSet::default(),
            selection_anchor: None,
            bulk: BulkDraft::default(),
            drafts: DraftsBar::default(),
            bulk_editor,
            cursor: None,
            editing: None,
            cell_editor,
            cells: HashMap::new(),
            draft_notices: HashMap::new(),
            blobs: HashMap::new(),
            blob_tasks: HashMap::new(),
            relations: HashMap::new(),
            relation_tasks: Vec::new(),
            relations_drawn: HashSet::new(),
            relations_asked: HashSet::new(),
            absent: HashMap::new(),
            peek: None,
            newer: false,
            saves_in_flight: 0,
            head_moved_while_saving: false,
            reads: BTreeSet::new(),
            _reads: None,
            awaiting_changes: false,
            focus_handle: cx.focus_handle(),
            table_state,
            widths,
            page_task: None,
            count_task: None,
            total_task: None,
            _subscriptions: subscriptions,
        };
        // Drafts already kept for this type (on a node's page, in another table) show at once.
        this.sync_drafts(cx);
        this.load_schema(cx);
        this
    }

    pub fn type_name(&self) -> &str {
        &self.type_name
    }

    // ---- session ----

    fn on_session_event(
        &mut self,
        _session: &Entity<GraphSession>,
        event: &SessionEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SessionEvent::SchemaChanged => self.load_schema(cx),
            // A table pinned to a commit reads that commit whatever the branch does.
            SessionEvent::BranchChanged | SessionEvent::HeadMoved { .. }
                if self.at_commit.is_some() => {}
            SessionEvent::BranchChanged => {
                self.branch = self.session.read(cx).branch.clone();
                self.watch_reads(cx);
                // The branch's own drafts show; the other's wait in the session.
                self.cells.clear();
                self.draft_notices.clear();
                self.editing = None;
                self.sync_drafts(cx);
                self.reload(false, cx);
            }
            SessionEvent::DraftsChanged => {
                self.sync_drafts(cx);
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();
            }
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

    fn switch_session(
        &mut self,
        session: Entity<GraphSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._subscriptions[0] = cx.subscribe_in(&session, window, Self::on_session_event);
        self.branch = session.read(cx).branch.clone();
        self.session = session;
        self.schema = None;
        self.cells.clear();
        self.draft_notices.clear();
        self.editing = None;
        self.sync_drafts(cx);
        self.load_schema(cx);
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
            .is_some_and(|current| Arc::ptr_eq(current, &schema) || current.source == schema.source)
        {
            return;
        }
        let columns = if let Some(node) = schema.node(&self.type_name) {
            self.kind = TableKind::Node;
            node_columns(node, &schema.relationships_of(&self.type_name))
        } else if let Some(edge) = schema.edge(&self.type_name) {
            self.kind = TableKind::Edge;
            edge_columns(edge)
        } else {
            self.missing = true;
            self.schema = Some(schema);
            self.all_columns.clear();
            self.columns.clear();
            self.rows.clear();
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
        self.watch_reads(cx);
        self.reload(false, cx);
    }

    /// Tells the session what the table reads on its branch: its type, and for a node type each
    /// relationship's edge and other type, and theirs one hop further, which grouping a
    /// relationship column by its neighbours' relationships reads.
    fn watch_reads(&mut self, cx: &mut Context<Self>) {
        let Some(schema) = self.schema.clone() else {
            return;
        };
        if self.missing || self.at_commit.is_some() {
            self._reads = None;
            return;
        }
        let reads = match self.kind {
            TableKind::Node => {
                let mut reads = node_reads(&schema, &self.type_name);
                for group in schema.relationships_of(&self.type_name) {
                    reads.extend(node_reads(&schema, &group.other_type));
                }
                reads
            }
            TableKind::Edge => edge_reads(&schema, &self.type_name),
        };
        let branch = self.branch.clone();
        let types = reads.clone();
        self._reads = Some(
            self.session
                .update(cx, |session, _| session.watch_reads(&branch, types)),
        );
        self.reads = reads;
    }

    /// Shows the columns the layout picks, keeping the cursor on its column when it still shows.
    fn show_columns(&mut self, cx: &mut Context<Self>) {
        let cursor_key = self
            .cursor
            .and_then(|c| self.columns.get(c.col))
            .map(|column| column.key.clone());
        self.set_columns(self.layout.shown(&self.all_columns), cx);
        if let Some(cursor) = &mut self.cursor {
            cursor.col = cursor_key
                .and_then(|key| self.columns.iter().position(|c| c.key == key))
                .unwrap_or(0);
        }
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// Shows the table with a remembered set and order of columns.
    pub fn set_layout(&mut self, layout: ColumnLayout, cx: &mut Context<Self>) {
        self.layout = layout;
        self.show_columns(cx);
        self.load_shown_relations(cx);
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
        let groups: Vec<(RelationshipGroup, Vec<String>)> = self
            .drawn_relationships()
            .into_iter()
            .map(|group| {
                let key = builder::relationship_key(&group);
                let missing = self
                    .rows
                    .iter()
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
        studio_ui::column_resize_handle(index, active, on_start, on_reset, cx).into_any_element()
    }

    /// Follows a column drag: the column's new width is where the pointer is now, relative to
    /// where the drag began.
    fn drag_column(&mut self, event: &DragMoveEvent<ColumnDrag>, cx: &mut Context<Self>) {
        let Some(resizing) = &self.resizing else {
            return;
        };
        let (key, width) = (resizing.column.clone(), resizing.width(event));
        self.resize_column(&key, Some(width), cx);
    }

    fn resize_column(&mut self, key: &str, width: Option<f32>, cx: &mut Context<Self>) {
        let before = self.layout.widths.get(key).copied();
        self.layout.set_width(key, width);
        if self.layout.widths.get(key).copied() == before {
            return;
        }
        let widths: Vec<_> = self
            .columns
            .iter()
            .map(|c| px(self.layout.width(c)))
            .collect();
        let behaviors = vec![TableResizeBehavior::MinSize(3.); self.columns.len()];
        let count = self.columns.len();
        self.widths.update(cx, |state, _| {
            *state = ResizableColumnsState::new(count, widths, behaviors);
        });
        // Saved with the tab, like the columns' order.
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn set_columns(&mut self, columns: Vec<ColumnSpec>, cx: &mut Context<Self>) {
        let widths: Vec<_> = columns.iter().map(|c| px(self.layout.width(c))).collect();
        let behaviors = vec![TableResizeBehavior::MinSize(3.); columns.len()];
        let count = columns.len();
        self.widths.update(cx, |state, _| {
            *state = ResizableColumnsState::new(count, widths, behaviors);
        });
        self.columns = columns;
    }

    // ---- reading ----

    fn page_query(
        &self,
        start: &PageStart,
        limit: u32,
        section: Option<&Filter>,
    ) -> Result<BuiltQuery, BuildError> {
        let schema = self.schema.as_ref().expect("schema loaded before reading");
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
        let graph = self.session.read(cx).settings_key(cx);
        cx.try_global::<studio_settings::StudioSettings>()
            .map(|settings| settings.views_for(&graph, &self.type_name))
            .unwrap_or_default()
    }

    /// The saved view the table shows now, if any.
    fn active_view(&self, cx: &App) -> Option<String> {
        let current = self.current_view();
        self.saved_views(cx)
            .into_iter()
            .find(|view| {
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
            .map(|view| view.name)
    }

    /// Shows a saved view: its filters and order, read again at the same commit. Filters on
    /// properties or relationships the schema no longer has are left out.
    fn apply_view(&mut self, view: &studio_settings::SavedView, cx: &mut Context<Self>) {
        let known: HashSet<String> = self
            .filter_choices()
            .into_iter()
            .map(|(key, _)| key)
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
            .filter(|sort| {
                self.all_columns
                    .iter()
                    .filter_map(ColumnSpec::property)
                    .any(|p| p.name == sort.property && Sort::allowed(p))
            });
        self.grouping = view
            .group_by
            .as_ref()
            .filter(|key| self.can_group_by(key))
            .map(Grouping::new);
        self.layout = ColumnLayout::new(view.hidden.clone(), view.order.clone());
        self.show_columns(cx);
        self.filter_draft = None;
        self.reload(true, cx);
    }

    fn open_save_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let graph = self.session.read(cx).settings_key(cx);
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

    fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.selection.clear();
        self.selection_anchor = None;
        self.bulk = BulkDraft::default();
        cx.notify();
    }

    /// Sets the chosen property on every selected row in one mutation, each row bound to the
    /// value it shows (spec 004, bulk set). Rows already holding the value are left out.
    fn submit_bulk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        let rows: Vec<(String, Value)> = self
            .rows
            .iter()
            .filter(|row| self.selection.contains(&row.id))
            .map(|row| (row.id.clone(), row.value(&property.name).clone()))
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
        let query =
            match builder::update_cells(&schema, &self.type_name, &property.name, &rows, &new) {
                Ok(query) => query,
                Err(error) => {
                    self.bulk.notice = Some(Notice::new(Tone::Invalid, error.to_string()));
                    cx.notify();
                    return;
                }
            };
        let count = rows.len();
        let description = format!("Set {} on {count} {} rows", property.name, self.type_name);
        let write = self.session.update(cx, |session, cx| {
            session.write(&query, None, description, cx)
        });
        self.bulk.saving = true;
        self.bulk.notice = None;
        self.saves_in_flight += 1;
        cx.notify();
        let branch = self.branch.clone();
        let column = property.name.clone();
        // Detached: a write is never cancelled by closing the tab.
        cx.spawn_in(window, async move |this, cx| {
            let outcome = write.await;
            this.update_in(cx, |this, _window, cx| {
                this.finish_bulk(outcome, rows, column, new, &branch, cx)
            })
            .ok();
        })
        .detach();
    }

    fn finish_bulk(
        &mut self,
        outcome: graph_client::WriteOutcome<graph_client::api::ChangeOutput>,
        rows: Vec<(String, Value)>,
        column: String,
        new: Value,
        branch: &str,
        cx: &mut Context<Self>,
    ) {
        self.saves_in_flight = self.saves_in_flight.saturating_sub(1);
        self.bulk.saving = false;
        match &outcome {
            graph_client::WriteOutcome::Committed(output) => {
                let changed = output.affected_nodes as usize;
                match output.commit.as_ref() {
                    Some(commit) if changed == rows.len() => {
                        for (id, _) in &rows {
                            if let Some(ix) = self.row_index(id) {
                                self.set_cell(ix, &column, new.clone());
                                self.set_dimmed(ix, !self.row_fits(ix));
                            }
                        }
                        if self.paging.advance_for_own_commit(
                            commit.parent_commit_id.as_deref(),
                            &commit.graph_commit_id,
                        ) {
                            self.newer = true;
                        }
                        self.clear_selection(cx);
                    }
                    _ => {
                        // Some rows held another value than the one shown: they were left
                        // alone. Read the table at the head so every row shows what it holds.
                        let left = rows.len().saturating_sub(changed);
                        self.bulk.notice = Some(Notice::new(
                            Tone::Conflict,
                            format!(
                                "{left} of {} rows had changed elsewhere and were left alone; \
                                 the table now shows {branch}'s head.",
                                rows.len()
                            ),
                        ));
                        self.reload(false, cx);
                    }
                }
            }
            _ => {
                if let SaveResult::Refused(notice) | SaveResult::Unknown(notice) =
                    save_result(&outcome, branch)
                {
                    self.bulk.notice = Some(notice);
                }
            }
        }
        self.saves_settled(cx);
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// The bar over the table while rows are picked: how many, and setting one property on
    /// all of them.
    fn render_bulk_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.selection.len() < 2 {
            return None;
        }
        let border = cx.theme().colors().border_variant;
        let count = self.selection.len();
        let read_only = self.kind == TableKind::Edge || self.at_commit.is_some();
        let editable: Vec<String> = self
            .columns
            .iter()
            .filter(|c| c.editable(self.kind))
            .filter_map(|c| c.property().map(|p| p.name.clone()))
            .collect();
        let chosen = self.bulk.property.clone();
        let table = cx.entity().downgrade();
        let saving = self.bulk.saving;
        let mut bar = h_flex()
            .flex_none()
            .px_3()
            .py_1p5()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(border)
            .child(
                Label::new(format!("{count} rows selected"))
                    .size(LabelSize::Small)
                    .weight(gpui::FontWeight::MEDIUM),
            );
        if read_only {
            bar = bar.child(studio_ui::ds::text::muted(self.read_only_label()));
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
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small),
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
                            .w(px(220.))
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
                        .label_size(LabelSize::Small)
                        .disabled(saving)
                        .tooltip(Tooltip::text(format!(
                            "One commit to {}; each row is bound to the value it shows",
                            self.branch
                        )))
                        .on_click(cx.listener(|this, _, window, cx| this.submit_bulk(window, cx))),
                    )
                });
        }
        Some(
            bar.children(self.bulk.notice.clone().map(|notice| {
                Label::new(notice.message)
                    .size(LabelSize::Small)
                    .color(match notice.tone {
                        Tone::Refused | Tone::Invalid => Color::Error,
                        Tone::Conflict | Tone::Unknown => Color::Warning,
                    })
            }))
            .child(div().flex_1())
            .child(
                studio_ui::ds::button("bulk-clear", "Clear")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .tooltip(Tooltip::text("Clear the selection (Esc)"))
                    .on_click(cx.listener(|this, _, _, cx| this.clear_selection(cx))),
            )
            .into_any_element(),
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
        let hidden = self
            .column_choices()
            .iter()
            .filter(|(_, _, shown)| !shown)
            .count();
        let table = cx.entity().downgrade();
        let open = self.toolbar_menus[1].is_deployed();
        PopoverMenu::new("columns-menu")
            .anchor(gpui::Anchor::TopRight)
            .with_handle(self.toolbar_menus[1].clone())
            .trigger(
                studio_ui::ds::button(
                    "columns",
                    match hidden {
                        0 => "Columns".to_owned(),
                        n => format!("{n} hidden"),
                    },
                )
                .style(ButtonStyle::Subtle)
                .label_size(LabelSize::Small)
                .start_icon(Icon::new(IconName::Eye).size(IconSize::Small))
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
        PopoverMenu::new("group-menu")
            .anchor(gpui::Anchor::TopRight)
            .with_handle(self.toolbar_menus[2].clone())
            .trigger(
                studio_ui::ds::button(
                    "group",
                    match &current_label {
                        Some(label) => format!("Group: {label}"),
                        None => "Group".to_owned(),
                    },
                )
                .style(ButtonStyle::Subtle)
                .label_size(LabelSize::Small)
                .start_icon(Icon::new(IconName::ListTree).size(IconSize::Small))
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
                    for (key, label) in &choices {
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
                    for group in &menus {
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
            })
    }

    /// The toolbar's Views menu: this type's saved views, then save and delete.
    fn render_views_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let views = self.saved_views(cx);
        let active = self.active_view(cx);
        let can_save = !self.filters.is_empty()
            || self.sort.is_some()
            || self.grouping.is_some()
            || self.layout != ColumnLayout::default();
        let table = cx.entity().downgrade();
        let graph = self.session.read(cx).settings_key(cx);
        let type_name = self.type_name.clone();
        let open = self.toolbar_menus[0].is_deployed();
        PopoverMenu::new("views-menu")
            .anchor(gpui::Anchor::TopRight)
            .with_handle(self.toolbar_menus[0].clone())
            .trigger(
                studio_ui::ds::button("views", active.clone().unwrap_or_else(|| "Views".into()))
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(IconName::Star).size(IconSize::Small))
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
                        ContextMenuEntry::new("Save View…")
                            .disabled(!can_save)
                            .handler(move |window, cx| {
                                save_table
                                    .update(cx, |table, cx| table.open_save_view(window, cx))
                                    .ok();
                            }),
                    );
                    if let Some(name) = active.clone() {
                        let graph = graph.clone();
                        let type_name = type_name.clone();
                        menu = menu.entry(format!("Delete “{name}”"), None, move |_, cx| {
                            let (graph, type_name, name) =
                                (graph.clone(), type_name.clone(), name.clone());
                            studio_settings::update(cx, move |settings| {
                                settings.delete_view(&graph, &type_name, &name)
                            });
                        });
                    }
                    menu
                }))
            })
    }

    /// Restores a remembered order, if the property can still order the table.
    pub fn set_sort(&mut self, sort: Option<Sort>, cx: &mut Context<Self>) {
        let allowed = sort.as_ref().is_none_or(|sort| {
            self.all_columns
                .iter()
                .filter_map(ColumnSpec::property)
                .any(|p| p.name == sort.property && Sort::allowed(p))
        });
        if allowed && sort != self.sort {
            self.sort = sort;
            self.reload(true, cx);
        }
    }

    /// The filters with each date preset's start computed now, in local time.
    fn resolved_filters(&self) -> Vec<Filter> {
        let now = chrono::Local::now();
        self.filters
            .iter()
            .map(|filter| {
                if !filter.op.is_date_preset() {
                    return filter.clone();
                }
                let kind = self
                    .columns
                    .iter()
                    .filter_map(ColumnSpec::property)
                    .find(|p| p.name == filter.property)
                    .map(|p| p.kind);
                Filter {
                    value: preset_start(filter.op, kind, now),
                    ..filter.clone()
                }
            })
            .collect()
    }

    fn count_query(&self, filters: &[Filter]) -> Result<BuiltQuery, BuildError> {
        let schema = self.schema.as_ref().expect("schema loaded before reading");
        let filters = if filters.is_empty() {
            Vec::new()
        } else {
            self.resolved_filters()
        };
        let filters = filters.as_slice();
        match self.kind {
            TableKind::Node => builder::node_count(schema, &self.type_name, filters),
            TableKind::Edge => builder::edge_count(schema, &self.type_name, filters),
        }
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
        if self.schema.is_none() || self.missing {
            return;
        }
        if keep_pin {
            self.paging.restart();
        } else {
            // Another commit, or another schema: what it holds is found out again.
            self.absent.clear();
            self.paging.reset();
            // A table opened at a commit stays pinned to it (constitution II).
            self.paging.pinned = self.at_commit.clone();
            self.newer = false;
            cx.emit(ItemEvent::UpdateTab);
        }
        self.rows.clear();
        // At the same commit a row's relationships and Blob cells cannot have changed: a new
        // order or filter keeps what was read and reads only rows not seen yet.
        if !keep_pin {
            self.blobs.clear();
            self.blob_tasks.clear();
            self.relations.clear();
            self.relation_tasks.clear();
            self.relations_asked.clear();
        }
        self.blob_rows = None;
        self._blob_wait = None;
        self.cursor = None;
        self.editing = None;
        self.loading_more = false;
        self.more_error = None;
        self.first_page = Loadable::Loading;
        self.count = Loadable::Loading;
        self.total = if self.filters.is_empty() {
            Loadable::NotLoaded
        } else {
            Loadable::Loading
        };
        self.count_task = None;
        self.total_task = None;
        self.table_state
            .read(cx)
            .scroll_handle
            .scroll_to_item(0, ScrollStrategy::Top);
        let size = self.page_size(cx);
        if self.grouping.is_some() {
            self.read_groups(cx);
            cx.notify();
            return;
        }
        let query = match self.page_query(&PageStart::First, size, None) {
            Ok(query) => query,
            Err(error) => {
                self.first_page = Loadable::Failed(error.to_string());
                cx.notify();
                return;
            }
        };
        let snapshot = self.paging.pinned.clone();
        let read = self
            .session
            .update(cx, |session, cx| session.read(&query, snapshot, cx));
        self.page_task = Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, cx| this.apply_page(result, true, size, cx))
                .ok();
        }));
        cx.notify();
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
        let size = self.page_size(cx);
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
        let snapshot = self.paging.pinned.clone();
        let read = self
            .session
            .update(cx, |session, cx| session.read(&query, snapshot, cx));
        self.page_task = Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, cx| this.apply_page(result, false, size, cx))
                .ok();
        }));
        cx.notify();
    }

    /// What the rows can be grouped by: each groupable property, then what each submenu of
    /// [`Self::group_menus`] offers. Key and how the choice reads.
    fn group_choices(&self) -> Vec<(String, String)> {
        let mut choices: Vec<(String, String)> = self
            .all_columns
            .iter()
            .filter_map(ColumnSpec::property)
            .filter(|p| builder::GroupBy::allowed(p))
            .map(|p| (p.name.clone(), p.name.clone()))
            .collect();
        for menu in self.group_menus() {
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
    fn group_menus(&self) -> Vec<GroupMenu> {
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
                    .into_iter()
                    .find(|group| group.direction == direction && group.edge == edge)?;
                (group.other_type, property)
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
    /// the same commit.
    pub fn set_grouping(&mut self, property: Option<String>, cx: &mut Context<Self>) {
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
        let filters = self.resolved_filters();
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
        let read = self
            .session
            .update(cx, |session, cx| session.read(&query, snapshot.clone(), cx));
        self.page_task = Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            // The rows with none are counted at the commit the groups were read at.
            let none = match (none, &result) {
                (Some(none), Ok(output)) => {
                    let commit = snapshot.or_else(|| output.graph_commit_id.clone());
                    let Ok(read) = this.update(cx, |this, cx| {
                        this.session
                            .update(cx, |session, cx| session.read(&none, commit, cx))
                    }) else {
                        return;
                    };
                    Some(read.await)
                }
                _ => None,
            };
            this.update(cx, |this, cx| {
                this.apply_groups(generation, result, none, cx)
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
        let Some(grouping) = self
            .grouping
            .as_mut()
            .filter(|g| g.generation == generation)
        else {
            return;
        };
        let none =
            none.map(|read| read.map(|output| count_of(&output.rows).unwrap_or(0).max(0) as u64));
        let result = match (result, none) {
            (Ok(output), Some(Ok(none))) => Ok((output, Some(none))),
            (Ok(output), None) => Ok((output, None)),
            (Err(error), _) | (_, Some(Err(error))) => Err(error),
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
    fn load_section(&mut self, index: usize, size: u32, cx: &mut Context<Self>) {
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
        let snapshot = self.paging.pinned.clone();
        let read = self
            .session
            .update(cx, |session, cx| session.read(&query, snapshot, cx));
        cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, cx| {
                this.apply_section_page(generation, index, result, size, cx)
            })
            .ok();
        })
        .detach();
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
        let page = match result {
            Ok(output) => {
                let page = self.rows_from(&output.rows);
                let sort_column = self.sort.as_ref().map(|sort| sort.property.clone());
                if let Some(section) = self.grouping.as_mut().and_then(|g| g.section_mut(index)) {
                    section.loading = false;
                    section.paging.accept(
                        output.graph_commit_id.as_deref(),
                        &page,
                        size,
                        sort_column.as_deref(),
                    );
                }
                page
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
        self.load_relations(ids, cx);
        cx.notify();
    }

    /// Keeps the cursor and the selection anchor on their rows after `count` rows went in at
    /// `at`.
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
    }

    fn toggle_section(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(section) = self.grouping.as_mut().and_then(|g| g.section_mut(index)) {
            section.expanded = !section.expanded;
            cx.notify();
        }
    }

    /// Sets a cell of the row at `ix`, and of every other copy of that row: grouped by a
    /// relationship, a row related to two neighbours shows in both sections.
    fn set_cell(&mut self, ix: usize, column: &str, value: Value) {
        let Some(id) = self.rows.get(ix).map(|row| row.id.clone()) else {
            return;
        };
        for row in self.rows.iter_mut().filter(|row| row.id == id) {
            set_value(row, column, value.clone());
        }
    }

    /// Dims the row at `ix` and its copies, or shows them plainly again.
    fn set_dimmed(&mut self, ix: usize, dimmed: bool) {
        let Some(id) = self.rows.get(ix).map(|row| row.id.clone()) else {
            return;
        };
        for row in self.rows.iter_mut().filter(|row| row.id == id) {
            row.dimmed = dimmed;
        }
    }

    /// Whether a row still belongs where it is shown: it matches the filters and, grouped, holds
    /// its section's value.
    fn row_fits(&self, ix: usize) -> bool {
        let Some(row) = self.rows.get(ix) else {
            return true;
        };
        if !row_matches(&self.filters, &row.values) {
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

    fn apply_page(
        &mut self,
        result: Result<ReadOutput, ReadError>,
        first: bool,
        size: u32,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(output) => {
                let page = self.rows_from(&output.rows);
                let sort_column = self.sort.as_ref().map(|sort| sort.property.as_str());
                self.paging
                    .accept(output.graph_commit_id.as_deref(), &page, size, sort_column);
                let ids: Vec<String> = page.iter().map(|row| row.id.clone()).collect();
                self.rows.extend(page);
                self.load_relations(ids, cx);
                if first {
                    self.first_page = Loadable::Loaded(());
                    self.load_counts(cx);
                } else {
                    self.loading_more = false;
                }
            }
            Err(ReadError::Refused(refusal)) if self.read_without(refusal.historical_gap()) => {
                self.reload(true, cx);
                return;
            }
            Err(error) => {
                if first {
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
                    let asked = ((*id).clone(), key.clone());
                    !self.relations.contains_key(&asked) && !self.relations_asked.contains(&asked)
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
            let degrees = self.session.update(cx, |session, cx| {
                session.read(&degrees, snapshot.clone(), cx)
            });
            let ids = ids.clone();
            let schema = schema.clone();
            let type_name = self.type_name.clone();
            let snapshot = snapshot.clone();
            // The degrees first: they size the neighbour read, so every row gets its chips.
            self.relation_tasks.push(cx.spawn(async move |this, cx| {
                let degrees = degrees.await;
                let counts: HashMap<String, i64> = match &degrees {
                    Ok(output) => relation_counts(&output.rows),
                    Err(_) => HashMap::new(),
                };
                let (sampled, total) = relation_sample(&ids, &counts);
                let neighbours = if sampled.is_empty() {
                    None
                } else {
                    let query = builder::page_neighbours(
                        &schema,
                        &type_name,
                        &group,
                        &sampled,
                        total as u32,
                    );
                    match query {
                        Ok(query) => this
                            .update(cx, |this, cx| {
                                this.session.update(cx, |session, cx| {
                                    session.read(&query, snapshot.clone(), cx)
                                })
                            })
                            .ok(),
                        Err(_) => None,
                    }
                };
                let neighbours = match neighbours {
                    Some(read) => Some(read.await),
                    None => None,
                };
                this.update(cx, |this, cx| {
                    let mut shown: HashMap<String, Vec<(String, Option<String>)>> = HashMap::new();
                    let read_whole = matches!(&neighbours, Some(Ok(_)));
                    if let Some(Ok(output)) = &neighbours {
                        for (id, other, label) in relation_samples(&output.rows) {
                            let list = shown.entry(id).or_default();
                            if list.len() < NEIGHBOUR_LIMIT as usize {
                                list.push((other, label));
                            }
                        }
                    }
                    let failed = degrees.err().map(|error| error.to_string());
                    for id in ids {
                        let relation = Relation {
                            count: match &failed {
                                Some(error) => Err(error.clone()),
                                None => Ok(counts.get(&id).copied().unwrap_or(0)),
                            },
                            whole: read_whole && sampled.contains(&id),
                            neighbours: shown.remove(&id).unwrap_or_default(),
                        };
                        this.relations.insert((id, key.clone()), relation);
                    }
                    cx.notify();
                })
                .ok();
            }));
        }
    }

    fn load_counts(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.paging.pinned.clone();
        let filtered = self.count_query(&self.filters);
        self.count_task = self.spawn_count(filtered, snapshot.clone(), true, cx);
        if !self.filters.is_empty() {
            let total = self.count_query(&[]);
            self.total_task = self.spawn_count(total, snapshot, false, cx);
        }
    }

    fn spawn_count(
        &mut self,
        query: Result<BuiltQuery, BuildError>,
        snapshot: Option<String>,
        filtered: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<()>> {
        let query = match query {
            Ok(query) => query,
            Err(error) => {
                let failed = Loadable::Failed(error.to_string());
                if filtered {
                    self.count = failed;
                } else {
                    self.total = failed;
                }
                return None;
            }
        };
        let read = self
            .session
            .update(cx, |session, cx| session.read(&query, snapshot, cx));
        Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, cx| {
                let value = match result {
                    Ok(output) => match count_of(&output.rows) {
                        Some(n) => Loadable::Loaded(n),
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
        let session = self.session.read(cx);
        let Some(head) = session.head(&self.branch).filter(|head| *head != pin) else {
            return;
        };
        match session.changes_since(&self.branch, &pin, head) {
            Since::Pending => self.awaiting_changes = true,
            Since::Known(since) if !since.any_changed(&self.reads) => {
                self.paging.pinned = Some(head.to_owned());
                cx.notify();
            }
            Since::Known(_) | Since::Unknown => {
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
        self.reload(false, cx);
        if self.peek.is_some() {
            self.close_peek(cx);
        }
        window.focus(&self.focus_handle, cx);
    }

    // ---- own writes from the dialogs ----

    /// A row this table inserted committed. It is shown where it sorts when that place is
    /// loaded; the pin follows when nothing else committed in between.
    pub fn row_inserted(
        &mut self,
        key: Option<String>,
        mut values: serde_json::Map<String, Value>,
        commit: &str,
        parent: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        if self.paging.advance_for_own_commit(parent, commit) {
            self.newer = true;
            cx.emit(ItemEvent::UpdateTab);
        }
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
                    let dimmed = !row_matches(&self.filters, &values);
                    self.rows.insert(position, Row { id, values, dimmed });
                    if let Some(cursor) = &mut self.cursor
                        && cursor.row >= position
                    {
                        cursor.row += 1;
                    }
                    self.cursor = Some(Cursor {
                        row: position,
                        col: self.cursor.map_or(0, |c| c.col),
                    });
                    self.table_state
                        .read(cx)
                        .scroll_handle
                        .scroll_to_item(position, ScrollStrategy::Nearest);
                    if !dimmed && let Loadable::Loaded(n) = &mut self.count {
                        *n += 1;
                    }
                    if let Loadable::Loaded(n) = &mut self.total {
                        *n += 1;
                    }
                }
            }
            // Without a key the engine assigns the id; only a fresh read shows the row.
            None => {
                self.newer = true;
                cx.emit(ItemEvent::UpdateTab);
            }
        }
        cx.notify();
    }

    /// Rows the table's own delete removed in `commit`: they leave the list, with their drafts
    /// and the pick. The table follows the commit, which moves its pin once.
    pub fn rows_deleted(
        &mut self,
        ids: &[String],
        commit: &str,
        parent: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        if self.paging.advance_for_own_commit(parent, commit) {
            self.newer = true;
            cx.emit(ItemEvent::UpdateTab);
        }
        for id in ids {
            self.remove_row(id, cx);
            self.selection.shift_remove(id);
        }
        cx.notify();
    }

    fn remove_row(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(index) = self.rows.iter().position(|row| row.id == id) {
            let row = self.rows.remove(index);
            if let Some(grouping) = &mut self.grouping {
                grouping.removed(index);
            }
            // Grouped by a relationship, the row's other copies go too.
            while let Some(copy) = self.rows.iter().position(|row| row.id == id) {
                self.rows.remove(copy);
                if let Some(grouping) = &mut self.grouping {
                    grouping.removed(copy);
                }
            }
            if !row.dimmed
                && let Loadable::Loaded(n) = &mut self.count
            {
                *n -= 1;
            }
            if let Loadable::Loaded(n) = &mut self.total {
                *n -= 1;
            }
            self.cells.retain(|(row_id, _), _| row_id != id);
            self.drop_row_drafts(id, cx);
            if let Some(cursor) = &mut self.cursor {
                if self.rows.is_empty() {
                    self.cursor = None;
                } else if cursor.row >= self.rows.len() || cursor.row > index {
                    cursor.row = cursor.row.saturating_sub(1).min(self.rows.len() - 1);
                }
            }
            if self.peek.as_ref().is_some_and(|(peeked, _)| peeked == id) {
                self.close_peek(cx);
            }
        }
    }

    // ---- cursor ----

    fn row_index(&self, id: &str) -> Option<usize> {
        self.rows.iter().position(|row| row.id == id)
    }

    fn column_index(&self, key: &str) -> Option<usize> {
        self.columns.iter().position(|column| column.key == key)
    }

    fn move_cursor(
        &mut self,
        rows: isize,
        cols: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        self.set_cursor(Cursor { row, col }, window, cx);
    }

    fn set_cursor(&mut self, cursor: Cursor, window: &mut Window, cx: &mut Context<Self>) {
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
        self.scroll_column_into_view(cursor.col, window, cx);
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

    /// Scrolls the unpinned columns so the cursor's column is in view. Column widths come from
    /// the resizable state, which may have been dragged.
    fn scroll_column_into_view(&self, col: usize, window: &Window, cx: &App) {
        if col == 0 {
            return;
        }
        let rem = window.rem_size();
        let widths = self.widths.read(cx);
        let total = widths.scrollable_width(1, rem);
        let left = total - widths.scrollable_width(col, rem);
        let right = total - widths.scrollable_width(col + 1, rem);
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
        let cursor = Cursor { row, col };
        // A double click on a node's key opens its page (spec 014); the first click peeked it.
        if clicks >= 2
            && self.kind == TableKind::Node
            && matches!(self.columns[col].role, ColumnRole::Key)
        {
            self.set_cursor(cursor, window, cx);
            self.open_row_page(row, window, cx);
            return;
        }
        // ⌘-click picks a row for a bulk set; ⇧-click picks the rows from the last one picked.
        if modifiers.secondary() || modifiers.shift {
            self.pick_rows(row, modifiers.shift, cx);
            self.set_cursor(cursor, window, cx);
            return;
        }
        if !self.selection.is_empty() {
            self.clear_selection(cx);
        }
        self.set_cursor(cursor, window, cx);
        // A relationship cell opens the related rows: the other type, filtered to those related
        // to this row through the same relationship seen from the other end.
        if let (Some(group), Some(id)) = (
            self.columns[col].relationship(),
            self.rows.get(row).map(|r| r.id.clone()),
        ) {
            let back = builder::reversed(group, &self.type_name);
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
        if self.columns[col].editable(self.kind) {
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
        let values = row.values.clone();
        let id = row.id.clone();
        let session = self.session.clone();
        let snapshot = self.paging.pinned.clone();
        // The row's relationships as this table read them at the same commit.
        let known: HashMap<String, KnownRelation> = match snapshot {
            Some(_) => self
                .relations
                .iter()
                .filter(|((row_id, _), _)| *row_id == id)
                .filter_map(|((_, key), relation)| {
                    let count = *relation.count.as_ref().ok()?;
                    let first = relation.whole.then(|| {
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
        let holds_commit = self.at_commit.is_some();
        let peek = cx.new(|cx| {
            let mut peek = NodePeek::new(
                session,
                subject,
                Some(values),
                snapshot,
                known,
                debounce,
                cx,
            );
            if holds_commit {
                peek.hold_commit();
            }
            peek
        });
        if let Some(workspace) = self.workspace.upgrade() {
            let view = peek.clone();
            workspace.update(cx, |workspace, cx| {
                workspace.set_inspector(Some(view.into()), cx);
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
            workspace.update(cx, |workspace, cx| workspace.set_inspector(None, cx));
        }
        cx.notify();
    }

    // ---- editing ----

    fn start_edit(&mut self, cursor: Cursor, window: &mut Window, cx: &mut Context<Self>) {
        let Some(column) = self.columns.get(cursor.col) else {
            return;
        };
        if !column.editable(self.kind) || self.at_commit.is_some() {
            self.show_peek(cursor.row, false, cx);
            return;
        }
        let Some(row) = self.rows.get(cursor.row) else {
            return;
        };
        let key = (row.id.clone(), column.key.clone());
        if matches!(self.cells.get(&key), Some(CellState::Saving)) {
            return;
        }
        let old = row.value(&column.key).clone();
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
        self.cell_editor.update(cx, |editor, cx| {
            editor.set_text(text, window, cx);
            editor.select_all(&editor::actions::SelectAll, window, cx);
        });
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

    /// The editor lost focus without Enter or Esc: an unchanged edit closes, a changed one is
    /// kept as a draft on the cell.
    fn cell_editor_blurred(&mut self, cx: &mut Context<Self>) {
        let Some(editing) = self.editing.take() else {
            return;
        };
        let text = self.cell_editor.read(cx).text(cx);
        if text != edit_text(&editing.old) || editing.notice.is_some() {
            self.put_draft(
                &editing.row_id,
                &editing.column,
                editing.old,
                text,
                editing.notice,
                cx,
            );
        } else {
            // Typed back to what the cell holds: no draft.
            self.drop_draft(&editing.row_id, &editing.column, cx);
        }
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn finish_editing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = None;
        window.focus(&self.focus_handle, cx);
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
        // The footer's key hints read the bindings of the frame drawn before, which still had
        // the editor focused; draw once more so they show the table's keys again.
        cx.on_next_frame(window, |_, window, _| window.refresh());
    }

    fn property_of(&self, column: &str) -> Option<Property> {
        self.all_columns
            .iter()
            .find(|c| c.key == column)
            .and_then(|c| c.property().cloned())
    }

    fn confirm_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.filter_draft.is_some() && self.filter_editor.focus_handle(cx).is_focused(window) {
            // ↵ picks the highlighted offer, unless it is just what is typed.
            let text = self.filter_editor.read(cx).text(cx);
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
        let new = match parse_input(&property, &text) {
            Ok(value) => value,
            Err(message) => {
                if let Some(editing) = &mut self.editing {
                    editing.notice = Some(Notice::new(Tone::Invalid, message));
                }
                cx.notify();
                return;
            }
        };
        if same_value(&new, &old) {
            self.finish_editing(window, cx);
            return;
        }
        let query = match builder::update_cell(
            &schema,
            &self.type_name,
            &property.name,
            &row_id,
            &old,
            &new,
        ) {
            Ok(query) => query,
            Err(error) => {
                if let Some(editing) = &mut self.editing {
                    editing.notice = Some(Notice::new(Tone::Invalid, error.to_string()));
                }
                cx.notify();
                return;
            }
        };
        // Show the new value with an amber dot while the save is out.
        self.set_cell(row_ix, &column, new.clone());
        self.cells
            .insert((row_id.clone(), column.clone()), CellState::Saving);
        self.saves_in_flight += 1;
        self.finish_editing(window, cx);
        let description = format!("Set {} of {row_id}", property.name);
        let write = self.session.update(cx, |session, cx| {
            session.write(&query, None, description, cx)
        });
        let branch = self.branch.clone();
        // Detached: a write is never cancelled by closing the tab.
        cx.spawn_in(window, async move |this, cx| {
            let outcome = write.await;
            let result = save_result(&outcome, &branch);
            this.update_in(cx, |this, window, cx| {
                this.finish_save(row_id, column, old, new, text, result, window, cx)
            })
            .ok();
        })
        .detach();
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_save(
        &mut self,
        row_id: String,
        column: String,
        old: Value,
        _new: Value,
        text: String,
        result: SaveResult,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.saves_in_flight = self.saves_in_flight.saturating_sub(1);
        let key = (row_id.clone(), column.clone());
        match result {
            SaveResult::Saved { commit, parent } => {
                self.cells.remove(&key);
                self.drop_draft(&row_id, &column, cx);
                if self
                    .paging
                    .advance_for_own_commit(parent.as_deref(), &commit)
                {
                    self.newer = true;
                }
                if let Some(ix) = self.row_index(&row_id) {
                    self.set_dimmed(ix, !self.row_fits(ix));
                }
            }
            SaveResult::NoMatch => {
                // The bound value changed elsewhere. Read the row at the head to say what it is
                // now; the reopened save binds to that value.
                self.reread_conflict(row_id, column, old, text, window, cx);
            }
            SaveResult::Refused(notice) | SaveResult::Unknown(notice) => {
                if let Some(ix) = self.row_index(&row_id) {
                    self.set_cell(ix, &column, old);
                }
                self.reopen(row_id, column, text, notice, window, cx);
            }
        }
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
        match parse_input(&property, &text) {
            Err(message) => {
                if let Some(editing) = &mut self.editing {
                    editing.notice = Some(Notice::new(Tone::Invalid, message));
                }
                cx.notify();
                return false;
            }
            Ok(new) if same_value(&new, &old) => {
                self.drop_draft(&key.0, &key.1, cx);
            }
            Ok(_) => {
                self.put_draft(&key.0, &key.1, old, text, None, cx);
                self.drafts.confirm_discard = false;
            }
        }
        self.finish_editing(window, cx);
        true
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
            .retain(|_, state| matches!(state, CellState::Saving));
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
        self.draft_notices
            .remove(&(row_id.to_owned(), column.to_owned()));
        let key = self.draft_key(row_id, column);
        self.session
            .update(cx, |session, cx| session.remove_draft(&key, cx));
        self.sync_drafts(cx);
    }

    /// A deleted row's drafts go with it.
    fn drop_row_drafts(&mut self, id: &str, cx: &mut Context<Self>) {
        let keys: Vec<DraftKey> = self
            .session
            .read(cx)
            .drafts()
            .filter(|(key, _)| key.type_name == self.type_name && key.id == id)
            .map(|(key, _)| key.clone())
            .collect();
        self.session.update(cx, |session, cx| {
            for key in &keys {
                session.remove_draft(key, cx);
            }
        });
        self.draft_notices.retain(|(row, _), _| row != id);
        self.sync_drafts(cx);
    }

    /// The draft text a cell shows, if any.
    #[cfg(test)]
    pub(crate) fn draft_text(&self, row_id: &str, column: &str) -> Option<String> {
        match self.cells.get(&(row_id.to_owned(), column.to_owned())) {
            Some(CellState::Draft { text, .. }) => Some(text.clone()),
            _ => None,
        }
    }

    /// The cells holding a draft, in table order: row, then column.
    fn draft_keys(&self) -> Vec<(String, String)> {
        let mut keys: Vec<(String, String)> = self
            .cells
            .iter()
            .filter(|(_, state)| matches!(state, CellState::Draft { .. }))
            .map(|(key, _)| key.clone())
            .collect();
        keys.sort_by_key(|(row, column)| {
            (
                self.row_index(row).unwrap_or(usize::MAX),
                self.column_index(column).unwrap_or(usize::MAX),
            )
        });
        keys
    }

    /// ⌘S: saves every draft as one mutation of one statement per row, each bound to the values
    /// the row shows, so one commit. A draft that does not parse opens instead, and nothing is
    /// sent.
    fn save_drafts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.drafts.saving || self.kind != TableKind::Node || self.at_commit.is_some() {
            return;
        }
        if self.editing.is_some() && !self.stage_edit(window, cx) {
            return;
        }
        let Some(schema) = self.schema.clone() else {
            return;
        };
        let mut edits = Vec::new();
        let mut unseen = 0;
        for key in self.draft_keys() {
            let Some(CellState::Draft { text, .. }) = self.cells.get(&key).cloned() else {
                continue;
            };
            let (Some(row), Some(col), Some(property)) = (
                self.row_index(&key.0),
                self.column_index(&key.1),
                self.property_of(&key.1),
            ) else {
                unseen += 1;
                continue;
            };
            let old = self.rows[row].value(&key.1).clone();
            match parse_input(&property, &text) {
                Err(message) => {
                    let notice = Notice::new(Tone::Invalid, message);
                    self.put_draft(&key.0, &key.1, old, text, Some(notice), cx);
                    let cursor = Cursor { row, col };
                    self.set_cursor(cursor, window, cx);
                    self.start_edit(cursor, window, cx);
                    return;
                }
                Ok(new) if same_value(&new, &old) => {
                    self.drop_draft(&key.0, &key.1, cx);
                }
                Ok(new) => edits.push(DraftEdit {
                    row_id: key.0,
                    column: key.1,
                    old,
                    new,
                    text,
                }),
            }
        }
        self.drafts.confirm_discard = false;
        self.drafts.notice = (unseen > 0).then(|| {
            Notice::new(
                Tone::Conflict,
                format!("{unseen} drafts are on rows the table no longer shows; they wait."),
            )
        });
        if edits.is_empty() {
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
            return;
        }
        let mut rows: Vec<builder::RowEdit> = Vec::new();
        for edit in &edits {
            let cell = (edit.column.clone(), edit.old.clone(), edit.new.clone());
            match rows.iter_mut().find(|row| row.id == edit.row_id) {
                Some(row) => row.cells.push(cell),
                None => rows.push(builder::RowEdit {
                    id: edit.row_id.clone(),
                    cells: vec![cell],
                }),
            }
        }
        let query = match builder::update_rows(&schema, &self.type_name, &rows) {
            Ok(query) => query,
            Err(error) => {
                self.drafts.notice = Some(Notice::new(Tone::Invalid, error.to_string()));
                cx.notify();
                return;
            }
        };
        // Show the new values with an amber dot while the save is out.
        for edit in &edits {
            if let Some(ix) = self.row_index(&edit.row_id) {
                self.set_cell(ix, &edit.column, edit.new.clone());
            }
            self.cells.insert(
                (edit.row_id.clone(), edit.column.clone()),
                CellState::Saving,
            );
        }
        let description = format!(
            "Save {} on {} {} {}",
            plural(edits.len(), "change", "changes"),
            rows.len(),
            self.type_name,
            if rows.len() == 1 { "row" } else { "rows" },
        );
        let write = self.session.update(cx, |session, cx| {
            session.write(&query, None, description, cx)
        });
        self.drafts.saving = true;
        self.saves_in_flight += 1;
        cx.notify();
        let branch = self.branch.clone();
        let row_count = rows.len();
        // Detached: a write is never cancelled by closing the tab.
        cx.spawn_in(window, async move |this, cx| {
            let outcome = write.await;
            this.update_in(cx, |this, window, cx| {
                this.finish_drafts(outcome, row_count, edits, &branch, window, cx)
            })
            .ok();
        })
        .detach();
    }

    fn finish_drafts(
        &mut self,
        outcome: graph_client::WriteOutcome<graph_client::api::ChangeOutput>,
        row_count: usize,
        edits: Vec<DraftEdit>,
        branch: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.saves_in_flight = self.saves_in_flight.saturating_sub(1);
        self.drafts.saving = false;
        match &outcome {
            graph_client::WriteOutcome::Committed(output) => {
                if let Some(commit) = output.commit.as_ref()
                    && self.paging.advance_for_own_commit(
                        commit.parent_commit_id.as_deref(),
                        &commit.graph_commit_id,
                    )
                {
                    self.newer = true;
                }
                let changed = output.affected_nodes as usize;
                if output.commit.is_some() && changed == row_count {
                    for edit in &edits {
                        self.cells
                            .remove(&(edit.row_id.clone(), edit.column.clone()));
                        self.drop_draft(&edit.row_id, &edit.column, cx);
                    }
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
                    save_result(&outcome, branch)
                {
                    for edit in edits {
                        if let Some(ix) = self.row_index(&edit.row_id) {
                            self.set_cell(ix, &edit.column, edit.old.clone());
                        }
                        self.cells
                            .remove(&(edit.row_id.clone(), edit.column.clone()));
                        self.put_draft(
                            &edit.row_id,
                            &edit.column,
                            edit.old,
                            edit.text,
                            Some(notice.clone()),
                            cx,
                        );
                    }
                    self.drafts.notice = Some(notice);
                }
            }
        }
        self.saves_settled(cx);
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn refresh_dimmed(&mut self, edits: &[DraftEdit]) {
        for edit in edits {
            if let Some(ix) = self.row_index(&edit.row_id) {
                self.set_dimmed(ix, !self.row_fits(ix));
            }
        }
    }

    /// Reads each row of a short batch at the head: a draft whose value is there landed; any
    /// other is kept, with what the cell holds now.
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
        ids.dedup();
        for id in ids {
            let row_edits: Vec<DraftEdit> = edits
                .iter()
                .filter(|edit| edit.row_id == id)
                .cloned()
                .collect();
            let Ok(query) = builder::node_by_id(&schema, &self.type_name, &id) else {
                continue;
            };
            let read = self
                .session
                .update(cx, |session, cx| session.read(&query, None, cx));
            self.saves_in_flight += 1;
            cx.spawn_in(window, async move |this, cx| {
                let result = read.await;
                this.update_in(cx, |this, _window, cx| {
                    this.saves_in_flight = this.saves_in_flight.saturating_sub(1);
                    let now = result
                        .ok()
                        .map(|output| node_rows(&output.rows).into_iter().next());
                    for edit in row_edits {
                        let key = (edit.row_id.clone(), edit.column.clone());
                        let property = this.property_of(&edit.column);
                        let ix = this.row_index(&edit.row_id);
                        let notice = match &now {
                            Some(Some(row)) => {
                                let value = row.value(&edit.column).clone();
                                if same_value(&value, &edit.new) {
                                    this.cells.remove(&key);
                                    this.drop_draft(&edit.row_id, &edit.column, cx);
                                    continue;
                                }
                                if let Some(ix) = ix {
                                    this.set_cell(ix, &edit.column, value.clone());
                                }
                                changed_elsewhere(property.as_ref(), Some(&value))
                            }
                            Some(None) => {
                                if let Some(ix) = ix {
                                    this.set_cell(ix, &edit.column, edit.old.clone());
                                    this.set_dimmed(ix, true);
                                }
                                changed_elsewhere(property.as_ref(), None)
                            }
                            None => {
                                if let Some(ix) = ix {
                                    this.set_cell(ix, &edit.column, edit.old.clone());
                                }
                                Notice::new(
                                    Tone::Unknown,
                                    "The row could not be read to check this change. \
                                     Check History before trying again; your change is kept.",
                                )
                            }
                        };
                        this.cells.remove(&key);
                        let old = ix
                            .map(|ix| this.rows[ix].value(&edit.column).clone())
                            .unwrap_or(edit.old);
                        this.put_draft(
                            &edit.row_id,
                            &edit.column,
                            old,
                            edit.text,
                            Some(notice),
                            cx,
                        );
                    }
                    // The table now shows values newer than its pin.
                    this.newer = true;
                    this.saves_settled(cx);
                    cx.emit(ItemEvent::UpdateTab);
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
    }

    /// Drops every draft, on the second click of Discard.
    fn discard_drafts(&mut self, cx: &mut Context<Self>) {
        if !self.drafts.confirm_discard {
            self.drafts.confirm_discard = true;
            cx.notify();
            return;
        }
        let keys: Vec<DraftKey> = self
            .session
            .read(cx)
            .drafts()
            .filter(|(key, _)| key.type_name == self.type_name)
            .map(|(key, _)| key.clone())
            .collect();
        self.session.update(cx, |session, cx| {
            for key in &keys {
                session.remove_draft(key, cx);
            }
        });
        self.draft_notices.clear();
        self.sync_drafts(cx);
        self.drafts = DraftsBar::default();
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
        let colors = cx.theme().colors();
        let (border, fill) = (
            colors.border_variant,
            if cx.theme().appearance.is_light() {
                gpui::white()
            } else {
                colors.elevated_surface_background
            },
        );
        let saving = self.drafts.saving;
        let focus = self.focus_handle.clone();
        let save_tooltip = format!(
            "One commit to {}; each row is bound to the values it shows",
            self.branch
        );
        let pill = h_flex()
            .id("drafts-bar")
            .occlude()
            .max_w(px(720.))
            .pl_3()
            .pr_1p5()
            .py_1()
            .gap_2()
            .items_center()
            .rounded(px(studio_ui::ds::radius::LG))
            .bg(fill)
            .border_1()
            .border_color(border)
            .shadow(studio_ui::ds::elevation::float())
            .child(Indicator::dot().color(Color::Accent))
            .child(
                Label::new(if saving {
                    "Saving drafts…".to_owned()
                } else {
                    format!("{} not saved", plural(count, "draft", "drafts"))
                })
                .size(LabelSize::Small)
                .weight(gpui::FontWeight::MEDIUM),
            )
            .children(self.drafts.notice.clone().map(|notice| {
                div().min_w_0().flex_shrink(1.).child(
                    Label::new(notice.message)
                        .size(LabelSize::Small)
                        .color(match notice.tone {
                            Tone::Refused | Tone::Invalid => Color::Error,
                            Tone::Conflict | Tone::Unknown => Color::Warning,
                        })
                        .truncate(),
                )
            }))
            .when(count > 0 && !saving, |bar| {
                bar.child(
                    studio_ui::ds::button(
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
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, _, _, cx| this.discard_drafts(cx))),
                )
                .child(
                    studio_ui::ds::button("drafts-save", "Save as one commit")
                        .style(ButtonStyle::Tinted(TintColor::Accent))
                        .label_size(LabelSize::Small)
                        .tooltip(move |_, cx| {
                            Tooltip::for_action_in(save_tooltip.clone(), &SaveDrafts, &focus, cx)
                        })
                        .on_click(cx.listener(|this, _, window, cx| this.save_drafts(window, cx))),
                )
            });
        Some(
            h_flex()
                .absolute()
                .bottom_3()
                .left_0()
                .right_0()
                .px_3()
                .justify_center()
                .child(pill)
                .into_any_element(),
        )
    }

    fn reread_conflict(
        &mut self,
        row_id: String,
        column: String,
        old: Value,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(schema) = self.schema.clone() else {
            return;
        };
        let property = self.property_of(&column);
        let query = match builder::node_by_id(&schema, &self.type_name, &row_id) {
            Ok(query) => query,
            Err(error) => {
                let notice = Notice::new(Tone::Conflict, error.to_string());
                self.reopen(row_id, column, text, notice, window, cx);
                return;
            }
        };
        self.cells
            .insert((row_id.clone(), column.clone()), CellState::Saving);
        self.saves_in_flight += 1;
        let read = self
            .session
            .update(cx, |session, cx| session.read(&query, None, cx));
        cx.spawn_in(window, async move |this, cx| {
            let result = read.await;
            this.update_in(cx, |this, window, cx| {
                this.saves_in_flight = this.saves_in_flight.saturating_sub(1);
                let notice = match result {
                    Ok(output) => {
                        let now = node_rows(&output.rows).into_iter().next();
                        let ix = this.row_index(&row_id);
                        match now {
                            Some(row) => {
                                let value = row.value(&column).clone();
                                if let Some(ix) = ix {
                                    this.set_cell(ix, &column, value.clone());
                                }
                                changed_elsewhere(property.as_ref(), Some(&value))
                            }
                            None => {
                                if let Some(ix) = ix {
                                    this.set_cell(ix, &column, old);
                                    this.set_dimmed(ix, true);
                                }
                                changed_elsewhere(property.as_ref(), None)
                            }
                        }
                    }
                    Err(_) => {
                        if let Some(ix) = this.row_index(&row_id) {
                            this.set_cell(ix, &column, old);
                        }
                        Notice::new(
                            Tone::Conflict,
                            "The value changed elsewhere while you were editing. Nothing was written.",
                        )
                    }
                };
                // The table now shows a value newer than its pin.
                this.newer = true;
                this.saves_settled(cx);
                cx.emit(ItemEvent::UpdateTab);
                this.reopen(row_id, column, text, notice, window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Puts an edit that did not land back in front of the person: the editor reopens on the
    /// cell when they are not busy elsewhere, otherwise the cell keeps the draft and the notice.
    fn reopen(
        &mut self,
        row_id: String,
        column: String,
        text: String,
        notice: Notice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = (row_id.clone(), column.clone());
        let position = self.row_index(&row_id).zip(self.column_index(&column));
        let free = self.editing.is_none()
            && self.filter_draft.is_none()
            && self.focus_handle.contains_focused(window, cx);
        let old = self
            .row_index(&row_id)
            .map(|ix| self.rows[ix].value(&column).clone())
            .unwrap_or(Value::Null);
        self.cells.remove(&key);
        self.put_draft(&row_id, &column, old, text, Some(notice), cx);
        if let (true, Some((row, col))) = (free, position) {
            let cursor = Cursor { row, col };
            self.set_cursor(cursor, window, cx);
            self.start_edit(cursor, window, cx);
        }
        cx.notify();
    }

    // ---- actions ----

    fn on_cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selection.is_empty() && self.editing.is_none() {
            self.clear_selection(cx);
            window.focus(&self.focus_handle, cx);
            return;
        }
        if self.filter_draft.is_some() {
            self.filter_draft = None;
            window.focus(&self.focus_handle, cx);
            cx.notify();
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
        self.move_cursor(0, step, window, cx);
    }

    fn on_edit(&mut self, _: &EditCell, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cursor) = self
            .cursor
            .or_else(|| (!self.rows.is_empty()).then_some(Cursor { row: 0, col: 0 }))
        else {
            return;
        };
        self.set_cursor(cursor, window, cx);
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
        cx.write_to_clipboard(ClipboardItem::new_string(copy_text(row.value(&column.key))));
    }

    fn on_insert(&mut self, _: &InsertRow, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(reason) = self.pinned_reason() {
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
        if let Some(reason) = self.pinned_reason() {
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
        let label = schema
            .node(&self.type_name)
            .and_then(|node| node.label_property().map(str::to_owned));
        let edge = self.kind == TableKind::Edge;
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
        let type_name = self.type_name.clone();
        let if_commit = self.paging.pinned.clone();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                DeleteRowModal::new(
                    table, session, schema, type_name, targets, if_commit, window, cx,
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
                short_commit(commit)
            )
        })
    }

    fn filterable(&self) -> Vec<Property> {
        self.all_columns
            .iter()
            .filter_map(ColumnSpec::property)
            .filter(|p| !FilterOp::for_property(p).is_empty())
            .cloned()
            .collect()
    }

    /// Shows only the rows one filter keeps, replacing the others (a table opened from the
    /// relationship tree).
    pub fn show_only(&mut self, filter: Filter, cx: &mut Context<Self>) {
        self.filter_draft = None;
        self.filters = vec![filter];
        if self.schema.is_some() {
            self.reload(true, cx);
        }
        cx.notify();
    }

    /// What a filter can test: each filterable property, then (in a node table) each
    /// relationship group, as `(key, label)`.
    /// The icon of a filter key: the property's kind, or the relationship's direction.
    fn key_icon(&self, key: &str) -> IconName {
        if let Some((direction, _)) = builder::parse_relationship_key(key) {
            return direction_icon(direction);
        }
        self.filterable()
            .iter()
            .find(|p| p.name == key)
            .map_or(IconName::CaseSensitive, property_icon)
    }

    fn filter_choices(&self) -> Vec<(String, String)> {
        let mut choices: Vec<(String, String)> = self
            .filterable()
            .into_iter()
            .map(|p| (p.name.clone(), p.name))
            .collect();
        if self.kind == TableKind::Node
            && let Some(schema) = &self.schema
        {
            for group in schema.relationships_of(&self.type_name) {
                let key = builder::relationship_key(&group);
                // The entry's icon shows the direction, so the label names the traversal only.
                let label = format!(
                    "{} {}",
                    gq_language::traversal_name(&group.edge),
                    group.other_type
                );
                choices.push((key, label));
            }
        }
        choices
    }

    /// The tests offered for a filter key.
    fn ops_for(&self, key: &str) -> Vec<FilterOp> {
        if builder::parse_relationship_key(key).is_some() {
            return FilterOp::for_relationship();
        }
        self.filterable()
            .iter()
            .find(|p| p.name == key)
            .map(FilterOp::for_property)
            .unwrap_or_default()
    }

    fn open_filter_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let filterable = self.filterable();
        let preferred = self
            .cursor
            .and_then(|c| self.columns.get(c.col))
            .and_then(|c| c.property())
            .filter(|p| filterable.iter().any(|f| f.name == p.name))
            .cloned()
            .or_else(|| filterable.first().cloned());
        let Some(property) = preferred else {
            self.toast(ToastKind::Info, "This table has no filterable columns.", cx);
            return;
        };
        self.set_draft_property(property.name.clone(), window, cx);
    }

    fn set_draft_property(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(&op) = self.ops_for(&name).first() else {
            return;
        };
        self.filter_draft = Some(FilterDraft {
            property: name,
            op,
            error: None,
            offers: Offers::default(),
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
                .into_iter()
                .find(|group| builder::relationship_key(group) == key)?;
            return Some(SuggestSource::Nodes(group.other_type));
        }
        let property = self.filterable().into_iter().find(|p| p.name == key)?;
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
        let source = self.suggestion_source(&draft.property, draft.op);
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
                draft._suggest_task = None;
            }
            Some(SuggestSource::Values(values)) => {
                draft
                    .offers
                    .set(suggest::from_values(&values, &text, list), typed);
                draft._suggest_task = None;
            }
            Some(SuggestSource::Nodes(type_name)) => {
                let Some(schema) = schema else {
                    return;
                };
                let needle = suggest::needle(&text, list).to_owned();
                draft._suggest_task = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(Duration::from_millis(120))
                        .await;
                    let Ok(query) =
                        builder::node_search(&schema, &type_name, &needle, NODE_SUGGESTIONS)
                    else {
                        return;
                    };
                    let read = cx.update(|cx| {
                        session.update(cx, |session, cx| session.read(&query, snapshot, cx))
                    });
                    let Ok(output) = read.await else { return };
                    this.update(cx, |this, cx| {
                        let Some(draft) = &mut this.filter_draft else {
                            return;
                        };
                        if draft.suggest_generation != generation {
                            return;
                        }
                        draft
                            .offers
                            .set(suggest::from_node_rows(&output.rows), typed);
                        cx.notify();
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
            cx.notify();
        }
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
        let parsed = if op.is_relationship() {
            match op {
                FilterOp::RelatedTo if text.trim().is_empty() => Err("Enter a node id".to_owned()),
                FilterOp::RelatedTo => Ok(Value::String(text.trim().to_owned())),
                _ => Ok(Value::Null),
            }
        } else {
            let Some(property) = self.filterable().into_iter().find(|p| p.name == key) else {
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
                    format_count(*n),
                    if *n == 1 { "row" } else { "rows" }
                )
            }
            (Loadable::Loaded(n), Loadable::Loaded(total), false) => {
                format!("{} of {}", format_count(*n), format_count(*total))
            }
            (Loadable::Loaded(n), _, false) => format!("{} matching", format_count(*n)),
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
        let target_tooltip = if let Some(reason) = self.pinned_reason() {
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
            ))
            .child(
                Label::new(self.type_name.clone())
                    .size(LabelSize::Default)
                    .weight(gpui::FontWeight::MEDIUM),
            )
            .child(
                div()
                    .id("row-count")
                    .when_some(count_error, |d, error| d.tooltip(Tooltip::text(error)))
                    .child(Label::new(count).size(LabelSize::Small).color(Color::Muted)),
            )
            // Where edits go is the branch the status bar names; the table says only when it
            // cannot edit.
            .when(read_only, |d| {
                d.child(
                    div()
                        .id("write-target")
                        .tooltip(Tooltip::text(target_tooltip))
                        .child(match &self.at_commit {
                            Some(commit) => studio_ui::ds::meta_code(
                                IconName::GitCommit,
                                format!("Read only · {}", short_commit(commit)),
                                cx,
                            )
                            .into_any_element(),
                            None => Label::new(self.read_only_label())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .into_any_element(),
                        }),
                )
            })
            .when(self.newer, |d| {
                d.child(
                    studio_ui::ds::button("newer-data", "Newer data · Refresh")
                        .style(ButtonStyle::Tinted(TintColor::Accent))
                        .label_size(LabelSize::Small)
                        .tooltip(|_, cx| {
                            Tooltip::for_action("Read the branch head again", &RefreshTable, cx)
                        })
                        .on_click(cx.listener(|this, _, window, cx| this.refresh(window, cx))),
                )
            })
            .child(div().flex_1())
            .child(self.render_views_menu(cx))
            .child(self.render_columns_menu(cx))
            .child(self.render_group_menu(cx))
            .child(
                studio_ui::ds::button("add-filter", "Filter")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(IconName::Filter).size(IconSize::Small))
                    .tooltip(|_, cx| Tooltip::for_action("Add a filter", &AddFilter, cx))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.open_filter_draft(window, cx)),
                    ),
            )
            .child(
                studio_ui::ds::button("insert-row", "Row")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(IconName::Plus).size(IconSize::Small))
                    .disabled(pinned)
                    .tooltip(move |_, cx| {
                        if pinned {
                            Tooltip::simple("Read-only at an older commit", cx)
                        } else {
                            Tooltip::for_action("Add a row", &InsertRow, cx)
                        }
                    })
                    .on_click(
                        cx.listener(|this, _, window, cx| this.on_insert(&InsertRow, window, cx)),
                    ),
            )
            .child(
                studio_ui::ds::icon_button("refresh-table", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .tooltip(|_, cx| Tooltip::for_action("Refresh", &RefreshTable, cx))
                    .on_click(cx.listener(|this, _, window, cx| this.refresh(window, cx))),
            )
            .into_any_element()
    }

    fn render_filter_bar(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.filters.is_empty() && self.filter_draft.is_none() {
            return None;
        }
        let editor_bg = cx.theme().colors().editor_background;
        let mut bar = h_flex()
            .flex_none()
            .flex_wrap()
            .px(px(studio_ui::ds::space::EDGE))
            .py(px(studio_ui::ds::space::SM))
            .gap(px(studio_ui::ds::space::SM))
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
            bar = bar.child(studio_ui::ds::filter_pill(
                ("filter", index),
                self.key_icon(&filter.property),
                field,
                test,
                value.map(Into::into),
                cx.listener(move |this, _, _, cx| this.remove_filter(index, cx)),
                cx,
            ));
        }
        if let Some(draft) = &self.filter_draft {
            let table = cx.entity().downgrade();
            let choices: Vec<(String, String, IconName)> = self
                .filter_choices()
                .into_iter()
                .map(|(key, label)| {
                    let icon = self.key_icon(&key);
                    (key, label, icon)
                })
                .collect();
            let chosen = choices
                .iter()
                .find(|(key, _, _)| *key == draft.property)
                .map(|(_, label, _)| label.clone())
                .unwrap_or_else(|| draft.property.clone());
            let ops = self.ops_for(&draft.property);
            let property_table = table.clone();
            let chosen_icon = self.key_icon(&draft.property);
            let rule = studio_ui::ds::surface::rule(cx);
            let divider = move || div().w(px(1.)).h_full().flex_none().bg(rule);
            let chevron = || {
                Icon::new(IconName::ChevronDown)
                    .size(IconSize::XSmall)
                    .color(Color::Muted)
            };
            // The filter being written is the pill it will become, outlined while it is open.
            let pill = h_flex()
                .h(px(studio_ui::ds::size::PILL))
                .flex_none()
                .rounded_full()
                .border_1()
                .border_color(if draft.error.is_some() {
                    cx.theme().status().error_border
                } else {
                    cx.theme().colors().border_focused
                })
                .bg(editor_bg)
                .overflow_hidden()
                .child(
                    PopoverMenu::new("filter-property")
                        .with_handle(self.filter_menus[0].clone())
                        .trigger(
                            Button::new("filter-property-button", chosen)
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .start_icon(
                                    Icon::new(chosen_icon)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
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
                .child(divider())
                .child(
                    PopoverMenu::new("filter-op")
                        .with_handle(self.filter_menus[1].clone())
                        .trigger(
                            Button::new("filter-op-button", draft.op.label())
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
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
                    pill.child(divider()).child(
                        div()
                            .key_context("FilterValue")
                            .on_action(cx.listener(|this, _: &SelectNextSuggestion, _, cx| {
                                this.move_suggestion(1, cx)
                            }))
                            .on_action(cx.listener(|this, _: &SelectPreviousSuggestion, _, cx| {
                                this.move_suggestion(-1, cx)
                            }))
                            .relative()
                            .w(px(180.))
                            .px(px(studio_ui::ds::space::MD))
                            .child(self.filter_editor.clone())
                            .children(offers),
                    )
                })
                .child(divider())
                .child(
                    h_flex()
                        .h_full()
                        .pl(px(studio_ui::ds::space::XS))
                        // The pill's border is the rest of the gap.
                        .pr(px(studio_ui::ds::PILL_BUTTON_GAP - 1.))
                        .gap(px(studio_ui::ds::space::XXS))
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
                                |this, _, window, cx| {
                                    this.filter_draft = None;
                                    window.focus(&this.focus_handle, cx);
                                    cx.notify();
                                },
                            )),
                        ),
                );
            bar = bar.child(pill).children(
                draft
                    .error
                    .clone()
                    .map(|error| Label::new(error).size(LabelSize::Small).color(Color::Error)),
            );
        }
        if !self.filters.is_empty() {
            bar = bar.child(div().flex_1()).child(
                studio_ui::ds::button("clear-filters", "Clear all")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .tooltip(|_, cx| Tooltip::for_action("Remove every filter", &ClearFilters, cx))
                    .on_click(cx.listener(|this, _, _, cx| this.clear_filters(cx))),
            );
        }
        Some(bar.into_any_element())
    }

    /// The values the filter's value field offers, under it: ↑/↓ highlight, ↵ or a click picks.
    fn render_suggestions(&self, draft: &FilterDraft, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let (bg, border) = (colors.elevated_surface_background, colors.border_variant);
        let pick =
            cx.listener(|this, index: &usize, window, cx| this.pick_suggestion(*index, window, cx));
        let items = draft.offers.rows(
            "filter-suggestion",
            Rc::new(move |index, window, cx| pick(&index, window, cx)),
            cx,
        );
        div()
            .absolute()
            .top_0()
            .left_0()
            .child(
                deferred(
                    anchored()
                        .anchor(Anchor::TopLeft)
                        .offset(point(px(-4.), px(28.)))
                        .snap_to_window_with_margin(px(8.))
                        .child(
                            studio_ui::ds::row_list()
                                .id("filter-suggestions")
                                .occlude()
                                .min_w(px(220.))
                                .max_w(px(360.))
                                .max_h(px(260.))
                                .overflow_y_scroll()
                                .border_1()
                                .border_color(border)
                                .bg(bg)
                                .shadow(studio_ui::ds::elevation::overlay())
                                .children(items),
                        ),
                )
                .with_priority(1),
            )
            .into_any_element()
    }

    fn render_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.frame_now = chrono::Local::now();
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
        let grid = self.render_grid(cx);
        // A column's edge dragged anywhere over the grid resizes it until released.
        div()
            .size_full()
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<ColumnDrag>, _, cx| {
                    this.drag_column(event, cx)
                }),
            )
            .on_drop(cx.listener(|this, _: &ColumnDrag, _, cx| {
                this.resizing = None;
                cx.notify();
            }))
            .child(grid)
            .into_any_element()
    }

    fn render_grid(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let selected_bg = studio_ui::ds::surface::selected_document(cx);
        let hover_bg = studio_ui::ds::surface::hover(cx);
        let drop_bg = studio_ui::ds::surface::selected_document(cx);
        let menu_open = self.header_menu.is_some();
        let headers: Vec<AnyElement> = self
            .columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                let sortable = column.property().filter(|p| Sort::allowed(p));
                let sorted = self
                    .sort
                    .as_ref()
                    .filter(|sort| sortable.is_some_and(|p| p.name == sort.property));
                let key = column.key.clone();
                let resize = self.render_resize_handle(index, column, cx);
                let header = h_flex()
                    .id(("column-header", index))
                    .relative()
                    .child(resize)
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            this.open_header_menu(key.clone(), event.position, window, cx)
                        }),
                    )
                    .h(px(studio_ui::HEADER_HEIGHT))
                    .px_4()
                    .gap_1p5()
                    .w_full()
                    .overflow_hidden()
                    .when(column.align_right, |d| d.justify_end())
                    .child(
                        Label::new(column.title.clone())
                            .size(LabelSize::Small)
                            .color(if sorted.is_some() {
                                Color::Default
                            } else {
                                Color::Muted
                            })
                            .single_line(),
                    )
                    .child(
                        Label::new(match self.absent.get(&column.key) {
                            Some(then) => crate::peek::absence_text(then, false),
                            None => column.detail.clone(),
                        })
                        .size(LabelSize::XSmall)
                        .color(Color::Disabled)
                        .single_line()
                        .truncate(),
                    )
                    .children(sorted.map(|sort| {
                        Icon::new(if sort.descending {
                            IconName::ArrowDown
                        } else {
                            IconName::ArrowUp
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Accent)
                    }));
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
        // The lines are worked out once a frame and shared with the row callback, which the
        // list calls more than once a frame.
        let lines: Option<Rc<Vec<Line>>> = self
            .grouping
            .as_ref()
            .map(|grouping| Rc::new(grouping.lines()));
        self.frame_lines = lines.clone();
        // How each drawn line looks (picked, dimmed, the cursor's, a group header) is worked
        // out by the row callback for the lines it draws, not for every loaded row.
        let looks: Rc<RefCell<HashMap<usize, RowLook>>> = Rc::default();
        self.frame_looks = looks.clone();
        let picked_bg = studio_ui::ds::surface::selected_document(cx).opacity(0.6);
        let header_bg = studio_ui::ds::surface::well(cx);
        let line_count = lines.as_ref().map_or(self.rows.len(), |lines| lines.len());
        let hairline = studio_ui::ds::surface::rule(cx);
        let glass = studio_ui::ds::is_glass(cx);
        // Widths fitted to each column's type unless dragged. Not Zed's resizable columns: they
        // draw a full-height rule between every column, which the visual language leaves out
        // (spec 002); the header's edges resize instead.
        let widths: Vec<_> = self
            .columns
            .iter()
            .map(|c| px(self.layout.width(c)))
            .collect();
        Table::new(self.columns.len())
            .interactable(&self.table_state)
            .width_config(ColumnWidthConfig::explicit(widths))
            .header(headers)
            .disable_base_style()
            .hide_row_borders()
            .pin_cols(1)
            .map_row(move |(ix, row), _window, _cx| {
                let look = looks.borrow().get(&ix).copied().unwrap_or_default();
                row.border_b_1()
                    .border_color(hairline)
                    .when(glass, |row| row.border_dashed())
                    .when(look.header, |row| row.bg(header_bg))
                    .when(look.picked, |row| row.bg(picked_bg))
                    .when(look.cursor, |row| row.bg(selected_bg))
                    .when(look.dimmed, |row| row.opacity(0.45))
                    .into_any_element()
            })
            .uniform_list(
                "type-table-rows",
                line_count,
                cx.processor(|this, range: Range<usize>, window, cx| {
                    this.note_looks(range.clone());
                    let drawn = this.drawn_columns(window, cx);
                    this.want_relations(&drawn, window, cx);
                    #[cfg(test)]
                    {
                        this.frame_drawn = drawn.clone();
                    }
                    let Some(lines) = this.frame_lines.clone() else {
                        if this.more_error.is_none()
                            && this
                                .paging
                                .wants_more(this.rows.len(), range.end, this.loading_more)
                        {
                            cx.defer_in(window, |this, _, cx| this.load_more(cx));
                        }
                        this.want_blob_metas(range.clone(), cx);
                        return range
                            .map(|ix| this.render_row(ix, &drawn, window, cx))
                            .collect();
                    };
                    let range = range.start.min(lines.len())..range.end.min(lines.len());
                    let shown = &lines[range.clone()];
                    // An open section reads its first rows when its read line comes into view.
                    for line in shown {
                        if let Line::More(index) = *line
                            && this
                                .grouping
                                .as_ref()
                                .and_then(|g| g.sections().get(index))
                                .is_some_and(|section| {
                                    section.range.is_empty()
                                        && !section.loading
                                        && section.error.is_none()
                                })
                        {
                            cx.defer_in(window, move |this, _, cx| {
                                this.load_section(index, SECTION_FIRST_PAGE, cx)
                            });
                        }
                    }
                    let rows: Vec<usize> = shown
                        .iter()
                        .filter_map(|line| match line {
                            Line::Row(row) => Some(*row),
                            _ => None,
                        })
                        .collect();
                    if let (Some(first), Some(last)) = (rows.iter().min(), rows.iter().max()) {
                        this.want_blob_metas(*first..*last + 1, cx);
                    }
                    shown
                        .iter()
                        .map(|line| match *line {
                            Line::Row(row) => this.render_row(row, &drawn, window, cx),
                            Line::Header(index) => this.render_section_header(index, cx),
                            Line::More(index) => this.render_section_more(index, cx),
                            Line::OtherValues => this.render_other_values(),
                        })
                        .collect()
                }),
            )
            .into_any_element()
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
            .px_3()
            .gap_1p5()
            .items_center()
            .overflow_hidden()
            .cursor_pointer()
            .child(
                Icon::new(if section.expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(IconSize::XSmall)
                .color(Color::Muted),
            )
            .child(
                div().min_w_0().flex_shrink(1.).child(
                    Label::new(text)
                        .size(LabelSize::Small)
                        .weight(gpui::FontWeight::MEDIUM)
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
                    div().min_w_0().flex_shrink(1000.).child(
                        Label::new(id)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate_start(),
                    ),
                )
            })
            .child(
                Label::new(format_count(section.count as i64))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
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
            Label::new("Loading…")
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .into_any_element()
        } else if let Some(error) = &section.error {
            h_flex()
                .gap_1()
                .child(
                    Icon::new(IconName::Warning)
                        .size(IconSize::XSmall)
                        .color(Color::Error),
                )
                .child(
                    div()
                        .id(("section-error", index))
                        .tooltip(Tooltip::text(error.clone()))
                        .child(
                            Label::new("Not read")
                                .size(LabelSize::XSmall)
                                .color(Color::Error),
                        ),
                )
                .child(
                    studio_ui::ds::button(("section-retry", index), "Retry")
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::XSmall)
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
                format!("Show {next} more of {}", format_count(remaining as i64))
            } else {
                format!("Show {} more", format_count(remaining as i64))
            };
            studio_ui::ds::button(("section-more", index), label)
                .style(ButtonStyle::Subtle)
                .label_size(LabelSize::XSmall)
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
                .px_4()
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
                .px_3()
                .items_center()
                .child(
                    Label::new(format!(
                        "The first {GROUP_LIMIT} values are shown; filter to see the others"
                    ))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .single_line(),
                )
                .into_any_element(),
        )
    }

    /// A line's cells: its content in the first (pinned) column, the others empty.
    fn line_cells(&self, first: AnyElement) -> Vec<AnyElement> {
        std::iter::once(first)
            .chain((1..self.columns.len()).map(|_| div().h(px(ROW_HEIGHT)).into_any_element()))
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
                    Some(Line::Row(row)) => Some(*row),
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

    /// Asks for the Blob cells of the rows drawn, once scrolling has paused: a fast scroll
    /// reads nothing for the rows it passes, and reads still out for rows no longer drawn are
    /// dropped.
    fn want_blob_metas(&mut self, rows: Range<usize>, cx: &mut Context<Self>) {
        if self.blob_rows.as_ref() == Some(&rows) || !self.has_blob_columns() {
            return;
        }
        self.blob_rows = Some(rows.clone());
        self._blob_wait = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(BLOB_READ_AFTER).await;
            this.update(cx, |this, cx| {
                let drawn: HashSet<String> = this
                    .rows
                    .get(rows.start..rows.end.min(this.rows.len()))
                    .unwrap_or_default()
                    .iter()
                    .map(|row| row.id.clone())
                    .collect();
                // A read for a row scrolled away is dropped and asked again when it returns.
                let gone: Vec<(String, String)> = this
                    .blob_tasks
                    .keys()
                    .filter(|(id, _)| !drawn.contains(id))
                    .cloned()
                    .collect();
                for key in gone {
                    this.blob_tasks.remove(&key);
                    this.blobs.remove(&key);
                }
                this.load_blob_metas(rows, cx);
            })
            .ok();
        }));
    }

    fn has_blob_columns(&self) -> bool {
        self.columns
            .iter()
            .filter_map(ColumnSpec::property)
            .any(|p| p.kind == ScalarKind::Blob)
    }

    /// Reads what the Blob cells of rows `range` hold, once each, at the page's commit. Only
    /// drawn rows are read: a Blob cannot be projected by the page query (spec 004).
    fn load_blob_metas(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        let properties: Vec<String> = self
            .columns
            .iter()
            .filter_map(|c| c.property())
            .filter(|p| p.kind == ScalarKind::Blob)
            .map(|p| p.name.clone())
            .collect();
        if properties.is_empty() {
            return;
        }
        let edge = self.kind == TableKind::Edge;
        let snapshot = self.paging.pinned.clone();
        let rows: Vec<String> = self
            .rows
            .get(range.start..range.end.min(self.rows.len()))
            .unwrap_or_default()
            .iter()
            .map(|row| row.id.clone())
            .collect();
        for id in rows {
            for property in &properties {
                let key = (id.clone(), property.clone());
                if self.blobs.contains_key(&key) {
                    continue;
                }
                self.blobs.insert(key.clone(), Loadable::Loading);
                let cell = BlobCell {
                    edge,
                    type_name: self.type_name.clone(),
                    id: id.clone(),
                    property: property.clone(),
                };
                let read = self.session.update(cx, |session, cx| {
                    session.blob_meta(cell, snapshot.clone(), cx)
                });
                let task_key = key.clone();
                let task = cx.spawn(async move |this, cx| {
                    let result = read.await;
                    this.update(cx, |this, cx| {
                        // A reload since this read began leaves its answer out.
                        if this.blob_tasks.remove(&task_key).is_some() {
                            this.blobs.insert(
                                task_key,
                                match result {
                                    Ok(meta) => Loadable::Loaded(meta),
                                    Err(error) => Loadable::Failed(error.to_string()),
                                },
                            );
                            cx.notify();
                        }
                    })
                    .ok();
                });
                self.blob_tasks.insert(key, task);
            }
        }
    }

    /// A relationship cell: the first neighbours as chips, then "+N"; "—" for none.
    fn render_relation_cell(
        &self,
        row: &Row,
        key: &str,
        group: &RelationshipGroup,
        available: f32,
        cx: &App,
    ) -> AnyElement {
        let Some(relation) = self.relations.get(&(row.id.clone(), key.to_owned())) else {
            return Label::new("…")
                .size(LabelSize::Small)
                .color(Color::Disabled)
                .into_any_element();
        };
        let count = match &relation.count {
            Ok(count) => *count,
            Err(error) => {
                return div()
                    .id(SharedString::from(format!("relation-{}-{key}", row.id)))
                    .tooltip(Tooltip::text(error.clone()))
                    .child(Label::new("?").size(LabelSize::Small).color(Color::Warning))
                    .into_any_element();
            }
        };
        if count == 0 {
            return Label::new("—")
                .size(LabelSize::Small)
                .color(Color::Disabled)
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
        // The count stays in view; the tags give way, whole.
        h_flex()
            .id(SharedString::from(format!("relation-{}-{key}", row.id)))
            .w_full()
            .min_w_0()
            .gap_1()
            .tooltip(Tooltip::text(format!(
                "{} {} · click to open them",
                format_count(count),
                group.other_type
            )))
            .child(
                h_flex().min_w_0().gap_1().overflow_hidden().children(
                    labels[..fit]
                        .iter()
                        .map(|label| studio_ui::ds::tag(label.to_string(), cx)),
                ),
            )
            .when(more > 0, |cell| {
                cell.child(
                    div().flex_none().child(
                        Label::new(if fit == 0 {
                            format_count(count)
                        } else {
                            format!("+{}", format_count(more))
                        })
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .single_line(),
                    ),
                )
            })
            .into_any_element()
    }

    /// A Blob cell: "Blob" while it is read, then its size and "Preview" (a click opens the
    /// peek, which shows it), "—" for null, or the host of an external reference.
    fn render_blob_cell(&self, row: &Row, property: &Property) -> AnyElement {
        let state = self.blobs.get(&(row.id.clone(), property.name.clone()));
        let (text, color, preview, tooltip) = match state {
            Some(Loadable::Loaded(meta)) => (
                crate::blob::cell_label(meta),
                if matches!(meta, BlobMeta::Null) {
                    Color::Disabled
                } else {
                    Color::Default
                },
                matches!(meta, BlobMeta::Managed { size, .. } if *size > 0),
                match meta {
                    BlobMeta::External { uri } => {
                        Some(format!("{uri}\nStudio does not fetch external objects."))
                    }
                    _ => None,
                },
            ),
            Some(Loadable::Failed(error)) => (
                "Blob · ?".into(),
                Color::Warning,
                false,
                Some(error.clone()),
            ),
            _ => ("Blob".into(), Color::Disabled, false, None),
        };
        h_flex()
            .id(SharedString::from(format!(
                "blob-{}-{}",
                row.id, property.name
            )))
            .min_w_0()
            .gap_1()
            .when_some(tooltip, |cell, tooltip| {
                cell.tooltip(Tooltip::text(tooltip))
            })
            .child(
                Label::new(text)
                    .size(LabelSize::Small)
                    .color(color)
                    .single_line()
                    .truncate(),
            )
            .when(preview, |cell| {
                cell.child(
                    Label::new("· Preview")
                        .size(LabelSize::Small)
                        .color(Color::Accent)
                        .single_line(),
                )
            })
            .into_any_element()
    }

    /// A row's cells; a column `drawn` leaves out is an empty cell at its width.
    fn render_row(
        &self,
        ix: usize,
        drawn: &[bool],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(row) = self.rows.get(ix) else {
            return Vec::new();
        };
        (0..self.columns.len())
            .map(|col| {
                if drawn.get(col).copied().unwrap_or(true) {
                    self.render_cell(ix, col, row, window, cx)
                } else {
                    div().into_any_element()
                }
            })
            .collect()
    }

    /// The columns rows draw this frame: the pinned first, those in the horizontal view with
    /// one either side so a quick scroll shows no empty column, the cursor's and the edited
    /// cell's. A row lays out each cell it draws, so a wide type would otherwise pay for every
    /// column on every scroll (`docs/drawing.md`).
    fn drawn_columns(&self, window: &Window, cx: &App) -> Vec<bool> {
        let count = self.columns.len();
        let handle = &self.table_state.read(cx).horizontal_scroll_handle;
        // The header's scrolled part tracks the same handle and is laid out before the list
        // draws its rows, so this is this frame's width, also right after a resize.
        let viewport = handle.bounds().size.width;
        if viewport <= px(0.) {
            return vec![true; count];
        }
        let rem = window.rem_size();
        let widths = self.widths.read(cx);
        let total = widths.scrollable_width(1, rem);
        let shown_left = -handle.offset().x;
        let shown_right = shown_left + viewport;
        let in_view: Vec<bool> = (0..count)
            .map(|col| {
                col == 0 || {
                    let left = total - widths.scrollable_width(col, rem);
                    let right = total - widths.scrollable_width(col + 1, rem);
                    right > shown_left && left < shown_right
                }
            })
            .collect();
        let mut drawn: Vec<bool> = (0..count)
            .map(|col| {
                in_view[col]
                    || in_view.get(col + 1).copied().unwrap_or(false)
                    || (col > 1 && in_view[col - 1])
            })
            .collect();
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

    fn render_cell(
        &self,
        ix: usize,
        col: usize,
        row: &Row,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let column = &self.columns[col];
        let colors = cx.theme().colors();
        let (focus_border, divider, editor_bg) = (
            colors.border_focused,
            colors.border_variant,
            colors.editor_background,
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
            .px_4()
            .flex()
            .items_center()
            .gap_1()
            .overflow_hidden()
            .when(col == 0, |d| {
                d.border_r_1().border_color(divider.opacity(0.6))
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
        let relative = crate::model::relative_value(column.property(), value, self.frame_now);
        let shown = match &relative {
            Some((shown, _)) => shown.clone(),
            None => display_value(column.property(), value),
        };
        let text_color = if value.is_null() {
            Color::Disabled
        } else {
            Color::Default
        };
        let numeric = column.align_right;
        if let Some(group) = column.relationship() {
            let rem = window.rem_size();
            let widths = self.widths.read(cx);
            let width = widths.scrollable_width(col, rem) - widths.scrollable_width(col + 1, rem);
            // The cell's padding (`px_4`) on both sides.
            let available = f32::from(width) - 32.;
            return cell
                .child(self.render_relation_cell(row, &column.key, group, available, cx))
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
                let text = draft.clone().unwrap_or_default();
                let empty = text.trim().is_empty();
                Label::new(if empty { "—".to_owned() } else { text })
                    .size(LabelSize::Small)
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
                self.render_blob_cell(row, property)
            }
            Some(property) if property.enum_values.is_some() && !value.is_null() => {
                ui::Chip::new(shown).into_any_element()
            }
            _ => {
                let mut label = Label::new(shown)
                    .size(LabelSize::Small)
                    .color(text_color)
                    .single_line()
                    .truncate();
                if numeric {
                    label = label.buffer_font(cx);
                }
                if matches!(column.role, ColumnRole::Key) {
                    label = label.weight(gpui::FontWeight::MEDIUM);
                }
                label.into_any_element()
            }
        };
        let (dot, tooltip) = match state {
            Some(CellState::Saving) => (
                Some(Color::Warning),
                Some(format!("Saving to {}…", self.branch)),
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
                    Some(format!(
                        "{message}Not saved: “{text}”. {held} \
                         Reopen it to edit; Save as one commit saves every draft together."
                    )),
                )
            }
            None => (None, relative.map(|(_, exact)| exact)),
        };
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
        cell.children(dot.map(|color| Indicator::dot().color(color)))
            .child(content)
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
        let colors = cx.theme().colors();
        let (bg, border) = (colors.elevated_surface_background, colors.border);
        let notice_color = |tone: Tone| match tone {
            Tone::Refused | Tone::Invalid => Color::Error,
            Tone::Conflict | Tone::Unknown => Color::Warning,
        };
        div()
            .absolute()
            .top_0()
            .left_0()
            .child(
                deferred(
                    anchored()
                        .anchor(Anchor::TopLeft)
                        .offset(point(px(-1.), px(ROW_HEIGHT + 2.)))
                        .snap_to_window_with_margin(px(8.))
                        .child(
                            v_flex()
                                .min_w(px(280.))
                                .max_w(px(460.))
                                .px_2()
                                .py_1p5()
                                .gap_1()
                                .rounded_md()
                                .border_1()
                                .border_color(border)
                                .bg(bg)
                                .shadow_md()
                                .children(notice.map(|notice| {
                                    let unknown = notice.tone == Tone::Unknown;
                                    h_flex()
                                        .gap_1p5()
                                        .child(Indicator::dot().color(notice_color(notice.tone)))
                                        .child(
                                            Label::new(notice.message)
                                                .size(LabelSize::Small)
                                                .color(notice_color(notice.tone)),
                                        )
                                        // Whether the save landed is in the branch's history.
                                        .when(unknown, |row| {
                                            row.child(
                                                studio_ui::ds::button(
                                                    "open-history",
                                                    "Open history",
                                                )
                                                .style(ButtonStyle::Subtle)
                                                .label_size(LabelSize::Small)
                                                .on_click(|_, window, cx| {
                                                    window.dispatch_action(
                                                        Box::new(
                                                            studio_actions::graph::OpenHistory,
                                                        ),
                                                        cx,
                                                    )
                                                }),
                                            )
                                        })
                                }))
                                .children(hint.map(|hint| {
                                    Label::new(hint).size(LabelSize::XSmall).color(Color::Muted)
                                }))
                                .child(
                                    Label::new(format!(
                                        "↵ save · esc cancel · write · {}",
                                        self.branch
                                    ))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                                ),
                        ),
                )
                .with_priority(2),
            )
            .into_any_element()
    }

    /// The table's part of the footer strip: how much is loaded and the commit every page is
    /// read at, then the keys that act on the cursor, as key caps (spec 002: layout, not
    /// sentences).
    fn footer_parts(&self, cx: &mut Context<Self>) -> ItemFooter {
        // The status bar names only what is happening: the row count is in the toolbar, and the
        // keys are behind its keyboard button.
        let mut start = Vec::new();
        if self.loading_more {
            start.push(studio_ui::ds::spinner().into_any_element());
        }
        if let Some(error) = self.more_error.clone() {
            start.push(
                studio_ui::ds::button("retry-more", "Retry")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::XSmall)
                    .start_icon(
                        Icon::new(IconName::Warning)
                            .size(IconSize::XSmall)
                            .color(Color::Error),
                    )
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
        let keys = match (self.editing.is_some(), self.kind) {
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
            (false, TableKind::Node) => vec![
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
            ],
            (false, TableKind::Edge) => vec![
                key("Peek", &PeekRow),
                key("Copy", &CopyCell),
                key("New row", &InsertRow),
                key("Delete row", &DeleteRow),
                key("Refresh", &RefreshTable),
            ],
        };
        ItemFooter {
            start,
            end: Vec::new(),
            keys,
        }
    }
}

fn filter_placeholder(op: FilterOp) -> &'static str {
    match op {
        FilterOp::In => "Values, separated by commas",
        FilterOp::RelatedTo => "Name or id",
        _ => "Value",
    }
}

/// "1 draft", "3 drafts".
fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
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
        let typing =
            self.editing.is_some() || self.filter_editor.focus_handle(cx).is_focused(window);
        let mut context = KeyContext::new_with_defaults();
        context.add("TypeTable");
        if typing {
            context.add("editing");
        }
        let background = cx.theme().colors().editor_background;
        let toolbar = self.render_toolbar(cx);
        let filter_bar = self.render_filter_bar(window, cx);
        let bulk_bar = self.render_bulk_bar(cx);
        let drafts_bar = self.render_drafts_bar(cx);
        let grid = self.grid.clone();
        v_flex()
            .key_context(context)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(background)
            .on_action(
                cx.listener(|this, _: &MoveUp, window, cx| this.move_cursor(-1, 0, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &MoveDown, window, cx| this.move_cursor(1, 0, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &MoveLeft, window, cx| this.move_cursor(0, -1, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &MoveRight, window, cx| this.move_cursor(0, 1, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &menu::SelectNext, window, cx| this.tab(1, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &menu::SelectPrevious, window, cx| this.tab(-1, window, cx)),
            )
            .on_action(cx.listener(Self::on_edit))
            .on_action(
                cx.listener(|this, _: &ConfirmEdit, window, cx| this.confirm_edit(window, cx)),
            )
            .on_action(cx.listener(|this, _: &StageEdit, window, cx| {
                this.stage_edit(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SaveDrafts, window, cx| this.save_drafts(window, cx)))
            .on_action(cx.listener(Self::on_cancel))
            .on_action(cx.listener(Self::on_peek))
            .on_action(cx.listener(Self::on_open_row_page))
            .on_action(cx.listener(Self::on_explore))
            .on_action(cx.listener(|this, _: &RefreshTable, window, cx| this.refresh(window, cx)))
            .on_action(cx.listener(Self::on_insert))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_copy))
            .on_action(
                cx.listener(|this, _: &AddFilter, window, cx| this.open_filter_draft(window, cx)),
            )
            .on_action(cx.listener(|this, _: &FilterBySelection, window, cx| {
                this.filter_by_selection(window, cx)
            }))
            .on_action(cx.listener(|this, _: &ClearFilters, _, cx| this.clear_filters(cx)))
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
            .children(self.header_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(Anchor::TopLeft)
                        .snap_to_window_with_margin(px(8.))
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        self.table
            .update(cx, |table, cx| table.render_body(cx))
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
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .shadow_md()
            .child(Label::new(self.title.clone()).size(LabelSize::Small))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};

    use crate::test_support::{PEOPLE, changed, session, session_followed};

    fn grid_renders(table: &Entity<TypeTable>, cx: &mut VisualTestContext) -> usize {
        table.read_with(cx, |table, cx| table.grid.read(cx).renders)
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

    /// Typing in the filter field or in a cell draws the grid no more: the field draws over
    /// it or beside it. Anything the table announces draws it again.
    #[gpui::test]
    async fn typing_in_a_field_leaves_the_grid_as_it_was(cx: &mut TestAppContext) {
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

    /// A node's page reads the node by id and names its tab by the node's label; it is one tab
    /// per node and commit, and comes back after a restart as it was (spec 014).
    #[gpui::test]
    async fn a_nodes_page_reads_it_and_names_its_tab(cx: &mut TestAppContext) {
        cx.update(studio_theme::init_test);
        let session = session(PEOPLE, cx);
        cx.run_until_parked();
        let page = cx.new(|cx| {
            crate::NodePage::new(session.clone(), "Person".into(), "p1".into(), None, cx)
        });
        cx.run_until_parked();
        page.read_with(cx, |page, cx| {
            assert_eq!(page.tab_title(cx).as_ref(), "Ada", "the label, once read");
            assert_eq!(page.dedupe_key(cx).as_deref(), Some("node:Person:p1"));
            let saved = page.serialize(cx).expect("a page is remembered");
            assert_eq!(saved.kind, "node");
            assert_eq!(saved.data["type_name"], "Person");
            assert_eq!(saved.data["id"], "p1");
            assert!(saved.data["at_commit"].is_null(), "it reads the head");
        });
        let held = cx.new(|cx| {
            crate::NodePage::new(session, "Person".into(), "p1".into(), Some("c0".into()), cx)
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
        let names = names.lock().unwrap().clone();
        assert_eq!(
            names
                .iter()
                .filter(|n| n.starts_with("studio_page"))
                .collect::<Vec<_>>(),
            ["studio_page", "studio_page_fields"]
        );
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
        table.update_in(cx, |table, window, cx| {
            let rem = window.rem_size();
            let total = table.widths.read(cx).scrollable_width(1, rem);
            let handle = table.table_state.read(cx).horizontal_scroll_handle.clone();
            let viewport = handle.bounds().size.width;
            handle.set_offset(point(-(total - viewport), px(0.)));
            cx.notify();
        });
        cx.run_until_parked();
        let read = read_for(cx);
        assert!(read.contains(&last));
        assert_eq!(degree_reads(), read.len(), "none read twice");
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
        table.update_in(cx, |table, window, cx| {
            let rem = window.rem_size();
            let total = table.widths.read(cx).scrollable_width(1, rem);
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
        cx.simulate_resize(gpui::size(px(6000.), px(800.)));
        assert!(
            rows_drew(cx).iter().all(|drawn| *drawn),
            "every column fits now: {:?}",
            rows_drew(cx)
        );
    }
}

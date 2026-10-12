//! One type's table (spec 004): keyset pages pinned to one commit, engine-side filters and
//! counts, a cell cursor, value-bound cell edits with their three outcomes, and a peek.
//!
//! The grid is Zed's `ui::Table` (`crates/ui/src/components/data_table.rs` at v1.23.2) in the
//! configuration Zed's `tabular_data_preview` uses: resizable columns, the first column pinned,
//! rows virtualized by `uniform_list`.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use editor::{Editor, EditorEvent};
use gpui::{
    Anchor, App, ClipboardItem, Context, ElementId, Entity, EventEmitter, FocusHandle, Focusable,
    KeyContext, Render, ScrollStrategy, SharedString, Subscription, Task, WeakEntity, Window,
    anchored, deferred, point,
};
use gq_language::{
    BuildError, BuiltQuery, Filter, FilterOp, PageStart, Property, RelationshipGroup, ScalarKind,
    SchemaModel, Sort, builder,
};
use graph_client::api::ReadOutput;
use graph_client::{BlobCell, BlobMeta, ReadError};
use graph_store::{GraphSession, Loadable, SessionEvent};
use serde_json::Value;
use studio_actions::table::{
    AddFilter, Cancel, ClearFilters, ConfirmEdit, CopyCell, DeleteRow, EditCell, FilterBySelection,
    InsertRow, MoveDown, MoveLeft, MoveRight, MoveUp, PeekRow, RefreshTable,
};
use studio_ui::ROW_HEIGHT;
use studio_workspace::{
    Item, ItemEvent, ItemFooter, SavedItem, ToastKind, Workspace, WorkspaceEvent,
};
use ui::prelude::*;
use ui::{
    ColumnWidthConfig, ContextMenu, Indicator, PopoverMenu, ResizableColumnsState, Table,
    TableInteractionState, TableResizeBehavior, TintColor, Tooltip,
};

use crate::dialogs::{DeleteRowModal, InsertRowModal};
use crate::model::{
    ColumnRole, ColumnSpec, Notice, Paging, Row, SaveResult, TableKind, Tone, changed_elsewhere,
    copy_text, count_of, display_value, edge_columns, edge_rows, edit_text, filter_label,
    format_count, insert_position, node_columns, node_rows, parse_filter_list, parse_filter_value,
    parse_input, preset_start, row_matches, same_value, save_result, short_commit,
};
use crate::peek::{NodePeek, PeekSubject};

/// How often an open, visible table asks whether its branch moved (constitution II: offer
/// Refresh, never change under the reader).
const HEAD_POLL: Duration = Duration::from_secs(30);

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
    /// An edit that did not land, or that was left by clicking away. Reopening the cell restores
    /// the text and the notice.
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
}

/// How many neighbours a relationship cell shows as chips.
const RELATION_CHIPS: usize = 3;

/// One row's neighbours through one relationship: how many, and the first few.
struct Relation {
    count: Result<i64, String>,
    neighbours: Vec<(String, Option<String>)>,
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
    columns: Vec<ColumnSpec>,
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
    filter_draft: Option<FilterDraft>,
    filter_editor: Entity<Editor>,
    cursor: Option<Cursor>,
    editing: Option<Editing>,
    cell_editor: Entity<Editor>,
    cells: HashMap<(String, String), CellState>,
    /// What each Blob cell drawn so far holds, by row id and property, read at the page's
    /// commit; and the reads under way.
    blobs: HashMap<(String, String), Loadable<BlobMeta>>,
    blob_tasks: HashMap<(String, String), Task<()>>,
    /// Each row's neighbours in the relationship columns, by row id and relationship key;
    /// read for each page at the page's commit.
    relations: HashMap<(String, String), Relation>,
    relation_tasks: Vec<Task<()>>,
    peek: Option<(String, Entity<NodePeek>)>,
    newer: bool,
    saves_in_flight: usize,
    focus_handle: FocusHandle,
    table_state: Entity<TableInteractionState>,
    widths: Entity<ResizableColumnsState>,
    page_task: Option<Task<()>>,
    count_task: Option<Task<()>>,
    total_task: Option<Task<()>>,
    head_task: Option<Task<()>>,
    _poll_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ItemEvent> for TypeTable {}

impl Focusable for TypeTable {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for TypeTable {
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
            editor
        });
        let mut subscriptions = vec![
            cx.subscribe_in(&session, window, Self::on_session_event),
            cx.subscribe_in(
                &cell_editor,
                window,
                |this, _, event: &EditorEvent, _window, cx| match event {
                    EditorEvent::Blurred => this.cell_editor_blurred(cx),
                    EditorEvent::BufferEdited => {
                        if let Some(editing) = &mut this.editing
                            && editing
                                .notice
                                .as_ref()
                                .is_some_and(|n| n.tone == Tone::Invalid)
                        {
                            editing.notice = None;
                        }
                        cx.notify();
                    }
                    _ => {}
                },
            ),
            cx.subscribe(&filter_editor, |this, _, event: &EditorEvent, cx| {
                if let EditorEvent::BufferEdited = event {
                    if let Some(draft) = &mut this.filter_draft {
                        draft.error = None;
                    }
                    cx.notify();
                }
            }),
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
        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(HEAD_POLL).await;
                let alive = this
                    .update(cx, |this, cx| {
                        if this.is_active(cx) {
                            this.check_head(cx);
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
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
            columns: Vec::new(),
            rows: Vec::new(),
            paging: Paging::default(),
            first_page: Loadable::NotLoaded,
            loading_more: false,
            more_error: None,
            count: Loadable::NotLoaded,
            total: Loadable::NotLoaded,
            filters: Vec::new(),
            sort: None,
            filter_draft: None,
            filter_editor,
            cursor: None,
            editing: None,
            cell_editor,
            cells: HashMap::new(),
            blobs: HashMap::new(),
            blob_tasks: HashMap::new(),
            relations: HashMap::new(),
            relation_tasks: Vec::new(),
            peek: None,
            newer: false,
            saves_in_flight: 0,
            focus_handle: cx.focus_handle(),
            table_state,
            widths,
            page_task: None,
            count_task: None,
            total_task: None,
            head_task: None,
            _poll_task: Some(poll),
            _subscriptions: subscriptions,
        };
        this.load_schema(cx);
        this
    }

    pub fn type_name(&self) -> &str {
        &self.type_name
    }

    fn is_active(&self, cx: &Context<Self>) -> bool {
        let Some(workspace) = self.workspace.upgrade() else {
            return false;
        };
        workspace
            .read(cx)
            .active_item()
            .is_some_and(|item| item.item_id() == cx.entity_id())
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
            SessionEvent::BranchChanged | SessionEvent::Committed { .. }
                if self.at_commit.is_some() => {}
            SessionEvent::BranchChanged => {
                self.branch = self.session.read(cx).branch.clone();
                self.cells.clear();
                self.editing = None;
                self.reload(false, cx);
            }
            SessionEvent::Committed { branch }
                // Our own saves settle the pin themselves; anything else may have moved the head.
                if *branch == self.branch && self.saves_in_flight == 0 => {
                    self.check_head(cx);
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
        self.editing = None;
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
        if self
            .schema
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &schema))
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
        self.set_columns(columns, cx);
        self.schema = Some(schema);
        self.reload(false, cx);
    }

    fn set_columns(&mut self, columns: Vec<ColumnSpec>, cx: &mut Context<Self>) {
        let widths: Vec<_> = columns.iter().map(|c| px(c.width)).collect();
        let behaviors = vec![TableResizeBehavior::MinSize(3.); columns.len()];
        let count = columns.len();
        self.widths.update(cx, |state, _| {
            *state = ResizableColumnsState::new(count, widths, behaviors);
        });
        self.columns = columns;
    }

    // ---- reading ----

    fn page_query(&self, start: &PageStart, limit: u32) -> Result<BuiltQuery, BuildError> {
        let schema = self.schema.as_ref().expect("schema loaded before reading");
        let filters = self.resolved_filters();
        let sort = self.sort.as_ref();
        match self.kind {
            TableKind::Node => {
                builder::node_page(schema, &self.type_name, &filters, sort, start, limit)
            }
            TableKind::Edge => {
                builder::edge_page(schema, &self.type_name, &filters, sort, start, limit)
            }
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

    /// Restores a remembered order, if the property can still order the table.
    pub fn set_sort(&mut self, sort: Option<Sort>, cx: &mut Context<Self>) {
        let allowed = sort.as_ref().is_none_or(|sort| {
            self.columns
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
            self.paging.reset();
            // A table opened at a commit stays pinned to it (constitution II).
            self.paging.pinned = self.at_commit.clone();
            self.newer = false;
            cx.emit(ItemEvent::UpdateTab);
        }
        self.rows.clear();
        self.blobs.clear();
        self.blob_tasks.clear();
        self.relations.clear();
        self.relation_tasks.clear();
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
        let query = match self.page_query(&PageStart::First, size) {
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
        if self.loading_more
            || self.paging.exhausted
            || !matches!(self.first_page, Loadable::Loaded(()))
        {
            return;
        }
        let size = self.page_size(cx);
        let start = self.paging.start.clone();
        let query = match self.page_query(&start, size) {
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
            Err(error) => {
                if first {
                    self.first_page = Loadable::Failed(error.to_string());
                    self.count = Loadable::NotLoaded;
                } else {
                    self.loading_more = false;
                    self.more_error = Some(error.to_string());
                }
            }
        }
        cx.notify();
    }

    /// Reads the relationship columns for one page's rows: per relationship, how many
    /// neighbours each row has, and the first few for the chips. Pinned to the page's commit.
    fn load_relations(&mut self, ids: Vec<String>, cx: &mut Context<Self>) {
        let Some(schema) = self.schema.clone() else {
            return;
        };
        if ids.is_empty() || self.kind != TableKind::Node {
            return;
        }
        let groups: Vec<_> = self
            .columns
            .iter()
            .filter_map(ColumnSpec::relationship)
            .cloned()
            .collect();
        let snapshot = self.paging.pinned.clone();
        // Enough for three chips a row on most pages; a row past it shows its count alone.
        let sample = (ids.len() as u32 * 3).clamp(30, 1000);
        for group in groups {
            let key = builder::relationship_key(&group);
            let queries = (
                builder::page_degrees(&schema, &self.type_name, &group, &ids),
                builder::page_neighbours(&schema, &self.type_name, &group, &ids, sample),
            );
            let (Ok(degrees), Ok(neighbours)) = queries else {
                continue;
            };
            let degrees = self.session.update(cx, |session, cx| {
                session.read(&degrees, snapshot.clone(), cx)
            });
            let neighbours = self.session.update(cx, |session, cx| {
                session.read(&neighbours, snapshot.clone(), cx)
            });
            let ids = ids.clone();
            self.relation_tasks.push(cx.spawn(async move |this, cx| {
                let (degrees, neighbours) = (degrees.await, neighbours.await);
                this.update(cx, |this, cx| {
                    let counts: HashMap<String, i64> = match &degrees {
                        Ok(output) => relation_counts(&output.rows),
                        Err(_) => HashMap::new(),
                    };
                    let mut shown: HashMap<String, Vec<(String, Option<String>)>> = HashMap::new();
                    if let Ok(output) = &neighbours {
                        for (id, other, label) in relation_samples(&output.rows) {
                            let list = shown.entry(id).or_default();
                            if list.len() < RELATION_CHIPS {
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

    /// Asks the branch head for its commit with the cheapest read there is (one row), and offers
    /// Refresh when it moved past the pin.
    fn check_head(&mut self, cx: &mut Context<Self>) {
        if self.newer
            || self.at_commit.is_some()
            || self.paging.pinned.is_none()
            || self.schema.is_none()
            || self.missing
        {
            return;
        }
        // One row of the plain key order is enough to learn the head's commit.
        let saved_filters = std::mem::take(&mut self.filters);
        let saved_sort = self.sort.take();
        let query = self.page_query(&PageStart::First, 1);
        self.filters = saved_filters;
        self.sort = saved_sort;
        let Ok(query) = query else { return };
        let read = self
            .session
            .update(cx, |session, cx| session.read(&query, None, cx));
        self.head_task = Some(cx.spawn(async move |this, cx| {
            let Ok(output) = read.await else { return };
            this.update(cx, |this, cx| {
                let Some(head) = output.graph_commit_id else {
                    return;
                };
                if this.saves_in_flight == 0
                    && this.paging.pinned.as_deref().is_some_and(|pin| pin != head)
                {
                    this.newer = true;
                    cx.emit(ItemEvent::UpdateTab);
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// A write bound to the pin was refused because the branch moved: offer Refresh.
    pub fn branch_moved(&mut self, cx: &mut Context<Self>) {
        if !self.newer {
            self.newer = true;
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
        }
    }

    pub fn pinned_commit(&self) -> Option<&str> {
        self.paging.pinned.as_deref()
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
                // Sorted, a new row goes first, where it is seen; the next read places it.
                let position = if self.sort.is_some() {
                    Some(0)
                } else {
                    insert_position(&self.rows, &id, self.paging.exhausted)
                };
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

    pub fn row_deleted(
        &mut self,
        id: &str,
        commit: &str,
        parent: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        if self.paging.advance_for_own_commit(parent, commit) {
            self.newer = true;
            cx.emit(ItemEvent::UpdateTab);
        }
        if let Some(index) = self.rows.iter().position(|row| row.id == id) {
            let row = self.rows.remove(index);
            if !row.dimmed
                && let Loadable::Loaded(n) = &mut self.count
            {
                *n -= 1;
            }
            if let Loadable::Loaded(n) = &mut self.total {
                *n -= 1;
            }
            self.cells.retain(|(row_id, _), _| row_id != id);
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
        cx.notify();
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
        let cursor = match self.cursor {
            None => Cursor { row: 0, col: 0 },
            Some(c) => Cursor {
                row: (c.row as isize + rows).clamp(0, self.rows.len() as isize - 1) as usize,
                col: (c.col as isize + cols).clamp(0, self.columns.len() as isize - 1) as usize,
            },
        };
        self.set_cursor(cursor, window, cx);
    }

    fn set_cursor(&mut self, cursor: Cursor, window: &mut Window, cx: &mut Context<Self>) {
        self.cursor = Some(cursor);
        self.table_state
            .read(cx)
            .scroll_handle
            .scroll_to_item(cursor.row, ScrollStrategy::Nearest);
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

    fn click_cell(&mut self, row: usize, col: usize, window: &mut Window, cx: &mut Context<Self>) {
        let cursor = Cursor { row, col };
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
        let peek =
            cx.new(|cx| NodePeek::new(session, subject, Some(values), snapshot, debounce, cx));
        if let Some(workspace) = self.workspace.upgrade() {
            let view = peek.clone();
            workspace.update(cx, |workspace, cx| {
                workspace.set_inspector(Some(view.into()), cx)
            });
        }
        self.peek = Some((id, peek));
        cx.notify();
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
        let (text, notice) = match self.cells.remove(&key) {
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
            self.cells.insert(
                (editing.row_id, editing.column),
                CellState::Draft {
                    text,
                    notice: editing.notice,
                },
            );
        }
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn finish_editing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = None;
        window.focus(&self.focus_handle, cx);
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn property_of(&self, column: &str) -> Option<Property> {
        self.columns
            .iter()
            .find(|c| c.key == column)
            .and_then(|c| c.property().cloned())
    }

    fn confirm_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.filter_draft.is_some() && self.filter_editor.focus_handle(cx).is_focused(window) {
            self.apply_filter_draft(window, cx);
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
        set_value(&mut self.rows[row_ix], &column, new.clone());
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
                if self
                    .paging
                    .advance_for_own_commit(parent.as_deref(), &commit)
                {
                    self.newer = true;
                }
                if let Some(ix) = self.row_index(&row_id) {
                    let matches = row_matches(&self.filters, &self.rows[ix].values);
                    self.rows[ix].dimmed = !matches;
                }
            }
            SaveResult::NoMatch => {
                // The bound value changed elsewhere. Read the row at the head to say what it is
                // now; the reopened save binds to that value.
                self.reread_conflict(row_id, column, old, text, window, cx);
            }
            SaveResult::Refused(notice) | SaveResult::Unknown(notice) => {
                if let Some(ix) = self.row_index(&row_id) {
                    set_value(&mut self.rows[ix], &column, old);
                }
                self.reopen(row_id, column, text, notice, window, cx);
            }
        }
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
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
                                    set_value(&mut this.rows[ix], &column, value.clone());
                                }
                                changed_elsewhere(property.as_ref(), Some(&value))
                            }
                            None => {
                                if let Some(ix) = ix {
                                    set_value(&mut this.rows[ix], &column, old);
                                    this.rows[ix].dimmed = true;
                                }
                                changed_elsewhere(property.as_ref(), None)
                            }
                        }
                    }
                    Err(_) => {
                        if let Some(ix) = this.row_index(&row_id) {
                            set_value(&mut this.rows[ix], &column, old);
                        }
                        Notice::new(
                            Tone::Conflict,
                            "The value changed elsewhere while you were editing. Nothing was written.",
                        )
                    }
                };
                // The table now shows a value newer than its pin.
                this.newer = true;
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
        self.cells.insert(
            key,
            CellState::Draft {
                text,
                notice: Some(notice),
            },
        );
        if let (true, Some((row, col))) = (free, position) {
            let cursor = Cursor { row, col };
            self.set_cursor(cursor, window, cx);
            self.start_edit(cursor, window, cx);
        }
        cx.notify();
    }

    // ---- actions ----

    fn on_cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if self.filter_draft.is_some() {
            self.filter_draft = None;
            window.focus(&self.focus_handle, cx);
            cx.notify();
            return;
        }
        if let Some(editing) = self.editing.take() {
            self.cells.remove(&(editing.row_id, editing.column));
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
        if self.kind != TableKind::Node {
            self.toast(ToastKind::Info, "Edges are added with a query for now.", cx);
            return;
        }
        let (Some(schema), Some(workspace)) = (self.schema.clone(), self.workspace.upgrade())
        else {
            return;
        };
        let table = cx.entity().downgrade();
        let session = self.session.clone();
        let type_name = self.type_name.clone();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                InsertRowModal::new(table, session, schema, type_name, window, cx)
            })
        });
    }

    fn on_delete(&mut self, _: &DeleteRow, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(reason) = self.pinned_reason() {
            self.toast(ToastKind::Info, &reason, cx);
            return;
        }
        if self.kind != TableKind::Node {
            self.toast(
                ToastKind::Info,
                "Edges are deleted with a query for now.",
                cx,
            );
            return;
        }
        let Some(cursor) = self.cursor else { return };
        let (Some(row), Some(schema), Some(workspace)) = (
            self.rows.get(cursor.row),
            self.schema.clone(),
            self.workspace.upgrade(),
        ) else {
            return;
        };
        let table = cx.entity().downgrade();
        let session = self.session.clone();
        let type_name = self.type_name.clone();
        let id = row.id.clone();
        let if_commit = self.paging.pinned.clone();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                DeleteRowModal::new(table, session, schema, type_name, id, if_commit, window, cx)
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
        self.columns
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
                let label = format!(
                    "{} {}",
                    builder::relationship_label(&key).unwrap_or_default(),
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
        });
        self.filter_editor.update(cx, |editor, cx| {
            editor.set_text("", window, cx);
            editor.set_placeholder_text(filter_placeholder(op), window, cx);
        });
        window.focus(&self.filter_editor.focus_handle(cx), cx);
        cx.notify();
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
        let target_tooltip = if let Some(reason) = self.pinned_reason() {
            reason
        } else if read_only {
            "Edges cannot be changed in a cell: the engine has no edge update (T16).".to_owned()
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
            // Where edits go is in the title bar; the table says only when it cannot edit.
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
                            None => Label::new("Read only")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .into_any_element(),
                        }),
                )
            })
            .when(self.newer, |d| {
                d.child(
                    Button::new("newer-data", "Newer data · Refresh")
                        .style(ButtonStyle::Tinted(TintColor::Accent))
                        .label_size(LabelSize::Small)
                        .tooltip(|_, cx| {
                            Tooltip::for_action("Read the branch head again", &RefreshTable, cx)
                        })
                        .on_click(cx.listener(|this, _, window, cx| this.refresh(window, cx))),
                )
            })
            .child(div().flex_1())
            .child(
                Button::new("add-filter", "Filter")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(IconName::Filter).size(IconSize::Small))
                    .tooltip(|_, cx| Tooltip::for_action("Add a filter", &AddFilter, cx))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.open_filter_draft(window, cx)),
                    ),
            )
            .child(
                Button::new("insert-row", "Row")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(IconName::Plus).size(IconSize::Small))
                    .disabled(read_only)
                    .tooltip(move |_, cx| {
                        if read_only {
                            Tooltip::simple("Edges are added with a query for now", cx)
                        } else {
                            Tooltip::for_action("Add a row", &InsertRow, cx)
                        }
                    })
                    .on_click(
                        cx.listener(|this, _, window, cx| this.on_insert(&InsertRow, window, cx)),
                    ),
            )
            .child(
                IconButton::new("refresh-table", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .tooltip(|_, cx| Tooltip::for_action("Refresh", &RefreshTable, cx))
                    .on_click(cx.listener(|this, _, window, cx| this.refresh(window, cx))),
            )
            .into_any_element()
    }

    fn render_filter_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.filters.is_empty() && self.filter_draft.is_none() {
            return None;
        }
        let colors = cx.theme().colors();
        let (border, chip_bg, editor_bg) = (
            colors.border_variant,
            colors.element_background,
            colors.editor_background,
        );
        let mut bar = h_flex()
            .flex_none()
            .flex_wrap()
            .px_3()
            .py_1p5()
            .gap_1p5()
            .items_center()
            .border_b_1()
            .border_color(border);
        for (index, filter) in self.filters.iter().enumerate() {
            bar = bar.child(
                h_flex()
                    .gap_0p5()
                    .pl_2()
                    .pr_0p5()
                    .rounded_md()
                    .border_1()
                    .border_color(border)
                    .bg(chip_bg)
                    .child(Label::new(filter_label(filter)).size(LabelSize::Small))
                    .child(
                        IconButton::new(("remove-filter", index), IconName::Close)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Remove this filter"))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.remove_filter(index, cx)),
                            ),
                    ),
            );
        }
        if let Some(draft) = &self.filter_draft {
            let table = cx.entity().downgrade();
            let choices = self.filter_choices();
            let chosen = choices
                .iter()
                .find(|(key, _)| *key == draft.property)
                .map(|(_, label)| label.clone())
                .unwrap_or_else(|| draft.property.clone());
            let ops = self.ops_for(&draft.property);
            let property_table = table.clone();
            bar = bar
                .child(
                    PopoverMenu::new("filter-property")
                        .trigger(
                            Button::new("filter-property-button", format!("{chosen} ▾"))
                                .style(ButtonStyle::Outlined)
                                .label_size(LabelSize::Small),
                        )
                        .menu(move |window, cx| {
                            let table = property_table.clone();
                            let choices = choices.clone();
                            Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                let mut relationships = false;
                                for (key, label) in choices {
                                    if !relationships
                                        && builder::parse_relationship_key(&key).is_some()
                                    {
                                        relationships = true;
                                        menu = menu.separator().header("Relationships");
                                    }
                                    let table = table.clone();
                                    let name = key.clone();
                                    menu = menu.entry(label, None, move |window, cx| {
                                        table
                                            .update(cx, |this, cx| {
                                                this.set_draft_property(name.clone(), window, cx)
                                            })
                                            .ok();
                                    });
                                }
                                menu
                            }))
                        }),
                )
                .child(
                    PopoverMenu::new("filter-op")
                        .trigger(
                            Button::new("filter-op-button", format!("{} ▾", draft.op.label()))
                                .style(ButtonStyle::Outlined)
                                .label_size(LabelSize::Small),
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
                .when(draft.op.takes_value(), |bar| {
                    bar.child(
                        div()
                            .w(px(200.))
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .border_1()
                            .border_color(if draft.error.is_some() {
                                cx.theme().status().error_border
                            } else {
                                border
                            })
                            .bg(editor_bg)
                            .child(self.filter_editor.clone()),
                    )
                })
                .child(
                    Button::new("apply-filter", "Add")
                        .style(ButtonStyle::Filled)
                        .label_size(LabelSize::Small)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.apply_filter_draft(window, cx)),
                        ),
                )
                .child(
                    IconButton::new("cancel-filter", IconName::Close)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Discard this filter"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.filter_draft = None;
                            window.focus(&this.focus_handle, cx);
                            cx.notify();
                        })),
                )
                .children(
                    draft
                        .error
                        .clone()
                        .map(|error| Label::new(error).size(LabelSize::Small).color(Color::Error)),
                );
        }
        if !self.filters.is_empty() {
            bar = bar.child(div().flex_1()).child(
                Button::new("clear-filters", "Clear all")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .tooltip(|_, cx| Tooltip::for_action("Remove every filter", &ClearFilters, cx))
                    .on_click(cx.listener(|this, _, _, cx| this.clear_filters(cx))),
            );
        }
        Some(bar.into_any_element())
    }

    fn render_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if self.missing {
            return studio_ui::empty_state(
                format!("The schema has no type {}", self.type_name),
                Some("It may have been renamed or dropped. Close this tab.".into()),
            )
            .into_any_element();
        }
        match &self.first_page {
            Loadable::Failed(error) => {
                let error = error.clone();
                return v_flex()
                    .size_full()
                    .child(studio_ui::empty_state(
                        format!("Studio could not read {}", self.type_name),
                        Some(error.into()),
                    ))
                    .child(
                        h_flex().justify_center().pb_6().child(
                            Button::new("retry-table", "Refresh")
                                .style(ButtonStyle::Filled)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.refresh(window, cx)),
                                ),
                        ),
                    )
                    .into_any_element();
            }
            Loadable::Loading | Loadable::NotLoaded => {
                return studio_ui::empty_state(format!("Reading {}…", self.type_name), None)
                    .into_any_element();
            }
            Loadable::Loaded(()) => {}
        }
        if self.rows.is_empty() {
            let (title, detail) = if self.filters.is_empty() {
                (
                    format!("No {} rows on {}", self.type_name, self.branch),
                    match self.kind {
                        TableKind::Node => Some("Add one with Row (⌘N).".into()),
                        TableKind::Edge => None,
                    },
                )
            } else {
                (
                    "No rows match these filters".to_owned(),
                    Some("Remove a filter or clear them all.".into()),
                )
            };
            return studio_ui::empty_state(title, detail).into_any_element();
        }
        self.render_grid(cx)
    }

    fn render_grid(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let selected_bg = studio_ui::ds::surface::selected_document(cx);
        let hover_bg = studio_ui::ds::surface::hover(cx);
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
                let header = h_flex()
                    .id(("column-header", index))
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
                        Label::new(column.detail.clone())
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
                            .tooltip(Tooltip::text(format!(
                                "{next} · rows without a value come last"
                            )))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.cycle_sort(property.clone(), cx)
                            }))
                            .into_any_element()
                    }
                    None => header.into_any_element(),
                }
            })
            .collect();
        let cursor_row = self.cursor.map(|c| c.row);
        let dimmed: HashSet<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.dimmed)
            .map(|(ix, _)| ix)
            .collect();
        let hairline = studio_ui::ds::surface::rule(cx);
        let glass = studio_ui::ds::is_glass(cx);
        // Fixed fitted widths: Zed's resizable columns draw a full-height rule between every
        // column, which the visual language leaves out (spec 002).
        let widths: Vec<_> = self.columns.iter().map(|c| px(c.width)).collect();
        Table::new(self.columns.len())
            .interactable(&self.table_state)
            .width_config(ColumnWidthConfig::explicit(widths))
            .header(headers)
            .disable_base_style()
            .hide_row_borders()
            .pin_cols(1)
            .map_row(move |(ix, row), _window, _cx| {
                row.border_b_1()
                    .border_color(hairline)
                    .when(glass, |row| row.border_dashed())
                    .when(cursor_row == Some(ix), |row| row.bg(selected_bg))
                    .when(dimmed.contains(&ix), |row| row.opacity(0.45))
                    .into_any_element()
            })
            .uniform_list(
                "type-table-rows",
                self.rows.len(),
                cx.processor(|this, range: Range<usize>, window, cx| {
                    if this.more_error.is_none()
                        && this
                            .paging
                            .wants_more(this.rows.len(), range.end, this.loading_more)
                    {
                        cx.defer_in(window, |this, _, cx| this.load_more(cx));
                    }
                    let shown = range.clone();
                    cx.defer_in(window, move |this, _, cx| this.load_blob_metas(shown, cx));
                    range.map(|ix| this.render_row(ix, window, cx)).collect()
                }),
            )
            .into_any_element()
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
        let more = count - relation.neighbours.len() as i64;
        // The count stays in view; the tags give way.
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
            .child(h_flex().min_w_0().gap_1().overflow_hidden().children(
                relation.neighbours.iter().map(|(id, label)| {
                    studio_ui::ds::tag(label.clone().unwrap_or_else(|| id.clone()), cx)
                }),
            ))
            .when(more > 0, |cell| {
                cell.child(
                    div().flex_none().child(
                        Label::new(if relation.neighbours.is_empty() {
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

    fn render_row(
        &self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(row) = self.rows.get(ix) else {
            return Vec::new();
        };
        (0..self.columns.len())
            .map(|col| self.render_cell(ix, col, row, window, cx))
            .collect()
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
        let state = self.cells.get(&(row.id.clone(), column.key.clone()));
        let value = row.value(&column.key);
        let mut cell = div()
            .id(ElementId::NamedInteger(
                SharedString::from(format!("cell-{col}")),
                ix as u64,
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
            return cell
                .bg(editor_bg)
                .child(div().flex_1().min_w_0().child(self.cell_editor.clone()))
                .child(ring())
                .child(self.render_edit_callout(editing.notice.clone(), hint, window, cx))
                .into_any_element();
        }
        // Dates read relative; the exact value is the cell's tooltip.
        let relative = crate::model::relative_value(column.property(), value, chrono::Local::now());
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
            return cell
                .child(self.render_relation_cell(row, &column.key, group, cx))
                .when(is_cursor, |cell| cell.child(ring()))
                .on_click(
                    cx.listener(move |this, _, window, cx| this.click_cell(ix, col, window, cx)),
                )
                .into_any_element();
        }
        let content: AnyElement = match column.property() {
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
                (
                    Some(color),
                    Some(format!(
                        "{message}Your edit “{text}” is kept. Press ↵ to reopen it."
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
                "No longer matches the filters. It leaves when the list is read again.",
            ));
        } else if let Some(tooltip) = tooltip {
            cell = cell.tooltip(Tooltip::text(tooltip));
        }
        cell.children(dot.map(|color| Indicator::dot().color(color)))
            .child(content)
            .on_click(cx.listener(move |this, _, window, cx| this.click_cell(ix, col, window, cx)))
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
                                                Button::new("open-history", "Open history")
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
        let loaded = match (&self.count, self.rows.len()) {
            (_, 0) => None,
            (Loadable::Loaded(total), n) if self.filters.is_empty() => Some(format!(
                "{} of {}",
                format_count(n as i64),
                format_count(*total)
            )),
            (_, n) => Some(format_count(n as i64)),
        };
        let mut start = Vec::new();
        if let Some(loaded) = loaded {
            start.push(
                div()
                    .id("loaded-rows")
                    .tooltip(Tooltip::text(if self.paging.exhausted {
                        "Every row is loaded"
                    } else {
                        "Rows loaded so far; more load as you scroll"
                    }))
                    .child(studio_ui::ds::meta(IconName::Hash, loaded))
                    .into_any_element(),
            );
        }
        if let Some(commit) = self.paging.pinned.clone() {
            start.push(
                div()
                    .id("pinned-commit")
                    .tooltip(Tooltip::text(format!(
                        "Every page of this table is read at commit {commit}"
                    )))
                    .child(studio_ui::ds::meta_code(
                        IconName::GitCommit,
                        short_commit(&commit),
                        cx,
                    ))
                    .into_any_element(),
            );
        }
        if self.loading_more {
            start.push(
                Icon::new(IconName::ArrowCircle)
                    .size(IconSize::XSmall)
                    .color(Color::Muted)
                    .into_any_element(),
            );
        }
        if let Some(error) = self.more_error.clone() {
            start.push(
                Button::new("retry-more", "Retry")
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
        let mut hints = h_flex().gap_4().items_center();
        hints = match (self.editing.is_some(), self.kind) {
            // A table pinned to a commit only reads.
            (false, _) if self.at_commit.is_some() => {
                hints.child(studio_ui::ds::key_hint(&PeekRow, "Peek", &focus, cx))
            }
            (true, _) => hints
                .child(studio_ui::ds::key_hint(&ConfirmEdit, "Save", &focus, cx))
                .child(studio_ui::ds::key_hint(&Cancel, "Cancel", &focus, cx)),
            (false, TableKind::Node) => hints
                .child(studio_ui::ds::key_hint(&EditCell, "Edit", &focus, cx))
                .child(studio_ui::ds::key_hint(&PeekRow, "Peek", &focus, cx))
                .child(studio_ui::ds::key_hint(&InsertRow, "Row", &focus, cx))
                .child(studio_ui::ds::key_hint(&DeleteRow, "Delete", &focus, cx)),
            (false, TableKind::Edge) => {
                hints.child(studio_ui::ds::key_hint(&PeekRow, "Peek", &focus, cx))
            }
        };
        ItemFooter {
            start,
            end: vec![hints.into_any_element()],
        }
    }
}

/// What the value field asks for.
fn filter_placeholder(op: FilterOp) -> &'static str {
    match op {
        FilterOp::In => "Values, separated by commas",
        FilterOp::RelatedTo => "Node id, for example company-00002",
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
        let typing =
            self.editing.is_some() || self.filter_editor.focus_handle(cx).is_focused(window);
        let mut context = KeyContext::new_with_defaults();
        context.add("TypeTable");
        if typing {
            context.add("editing");
        }
        let background = cx.theme().colors().editor_background;
        let toolbar = self.render_toolbar(cx);
        let filter_bar = self.render_filter_bar(cx);
        let body = self.render_body(cx);
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
            .on_action(cx.listener(Self::on_cancel))
            .on_action(cx.listener(Self::on_peek))
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
            .child(div().flex_1().min_h_0().child(body))
    }
}

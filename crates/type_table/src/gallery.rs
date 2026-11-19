//! A type's rows as tiles of one of their files (spec 035): a masonry of columns of one width,
//! each tile as tall as its file's shape needs, placed in row order into the shortest column and
//! never moved after, one tab per set, read a page at a time at one commit. Each tile's file is
//! read on its own through the Blob route once scrolling pauses, at most four at once: its first
//! bytes give its shape, for the rows about a screen below too, and the rest of a picture is
//! read for the tiles shown and drawn from a preview decoded and scaled off the main thread; the
//! previews held are capped.

use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use collections::IndexSet;
use gpui::{
    AnyView, App, ClickEvent, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable,
    KeyContext, Render, ScrollHandle, SharedString, StyleRefinement, Subscription, Task,
    WeakEntity, Window, point,
};
use gq_language::{BuiltQuery, Filter, ScalarKind, SchemaModel, Sort, builder};
use graph_client::api::ReadOutput;
use graph_client::{BlobCell, BlobMeta, HistoricalGap, PropertyThen, ReadError};
use graph_store::scope::ReadScope;
use graph_store::{GraphSession, HeadWatch, Loadable, NetChange, SessionEvent};
use serde_json::Value;
use studio_actions::gallery::{
    ClearPicks, CopyIds, LargerTiles, MoveDown, MoveLeft, MoveRight, MoveToFirst, MoveToLast,
    MoveUp, OpenFile, OpenRowPage, PickAll, Refresh, ShowNextFile, SmallerTiles, TogglePick,
};
use studio_actions::graph::{
    OpenBlob, OpenNodePage, OpenType, ResultNode, ShowNodeOnCanvas, ShowSetInGallery, ShowSetInTree,
};
use studio_actions::workspace::ToggleInspector;
use studio_actions::{ViewKind, ViewLink};
use studio_ui::ds::{Tipped, space};
use studio_ui::menu::ContextMenu;
use studio_ui::{ChangeKind, plural};
use studio_workspace::{Inspected, Item, ItemEvent, ItemStatus, KeyHint, SavedItem, Workspace};
use ui::PopoverMenu;
use ui::prelude::*;

use crate::branch_diff::{BranchDiff, TARGET};
use crate::masonry::{Masonry, Place};
use crate::model::{Paging, Row, field_rows, node_rows};
use crate::peek::{NodePeek, PeekSubject};
use crate::tile::{self, FIRST_READ, Facts, Look};

/// How long scrolling rests before the tiles drawn are read, as the table's Blob cells wait.
const READ_AFTER: Duration = Duration::from_millis(120);
/// How many tiles' files are read at once: half the connection's slots
/// (`graph_client::READ_CONCURRENCY`), so pages, counts and the inspector never wait behind them.
pub(crate) const READS_AT_ONCE: usize = 4;
/// The most the previews held may take; past it the tiles farthest from the lines drawn let
/// theirs go, and read it again if they come back.
pub(crate) const PREVIEWS_HELD: usize = 64 * 1024 * 1024;
/// How long after a tile's read lands the grid draws it, taking every other landing meanwhile
/// in the same draw (the command palette's landing delay): tiles land in bursts as a screen is
/// read, and one draw each would draw the grid dozens of times.
pub(crate) const LANDING_DELAY: Duration = Duration::from_millis(30);

/// How wide a gallery's columns are drawn at least; the columns grow from it to fill the width.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TileSize {
    Small,
    #[default]
    Medium,
    Large,
}

impl TileSize {
    const ALL: [TileSize; 3] = [TileSize::Small, TileSize::Medium, TileSize::Large];

    fn edge(self) -> f32 {
        match self {
            TileSize::Small => studio_ui::ds::size::TILE_SMALL,
            TileSize::Medium => studio_ui::ds::size::TILE,
            TileSize::Large => studio_ui::ds::size::TILE_LARGE,
        }
    }

    fn label(self) -> &'static str {
        match self {
            TileSize::Small => "S",
            TileSize::Medium => "M",
            TileSize::Large => "L",
        }
    }

    fn larger(self) -> Self {
        match self {
            TileSize::Small => TileSize::Medium,
            _ => TileSize::Large,
        }
    }

    fn smaller(self) -> Self {
        match self {
            TileSize::Large => TileSize::Medium,
            _ => TileSize::Small,
        }
    }
}

/// How many columns of at least `edge` fit `width`, and how wide each is drawn so they fill it:
/// the gallery's edges keep [`space::EDGE`], and columns and tiles are [`space::LG`] apart.
pub(crate) fn column_layout(width: f32, edge: f32) -> (usize, f32) {
    let inner = (width - 2. * space::EDGE).max(edge);
    let columns = ((inner + space::LG) / (edge + space::LG)).floor().max(1.) as usize;
    let tile = (inner - space::LG * (columns as f32 - 1.)) / columns as f32;
    (columns, tile.max(edge.min(inner)))
}

/// What a tile adds to its preview's height: the ring and inset around the preview, and the
/// caption under it.
fn tile_chrome() -> f32 {
    2. * tile_inset() + studio_ui::ds::size::TILE_CAPTION
}

/// How far a tile's preview sits inside its edge: the cursor's ring and [`space::XS`].
fn tile_inset() -> f32 {
    studio_ui::ds::size::ACCENT_LINE + space::XS
}

/// What is known of one tile's file.
#[derive(Debug)]
struct Tile {
    meta: Loadable<BlobMeta>,
    /// What its first bytes said.
    facts: Option<Facts>,
    /// Its shape, height over width, once known: kept for the gallery's life, so the columns
    /// are laid out again without a read.
    shape: Option<f32>,
    /// The first bytes of a file measured ahead of the view, kept for its preview's read to go
    /// on from.
    head: Option<Vec<u8>>,
    look: Loadable<Look>,
}

impl Default for Tile {
    fn default() -> Self {
        Self {
            meta: Loadable::NotLoaded,
            facts: None,
            shape: None,
            head: None,
            look: Loadable::NotLoaded,
        }
    }
}

impl Tile {
    /// What it holds against [`PREVIEWS_HELD`].
    fn held(&self) -> usize {
        let look = match &self.look {
            Loadable::Loaded(look) => look.held(),
            _ => 0,
        };
        look + self.head.as_ref().map_or(0, Vec::len)
    }
}

/// What a tile's read is for: its shape alone (a row below the view), or its preview too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Want {
    Shape,
    Preview,
}

/// A tile and the property it shows, by row id.
type TileKey = (String, String);

pub struct TypeGallery {
    workspace: WeakEntity<Workspace>,
    session: Entity<GraphSession>,
    type_name: String,
    /// The set in words, as the tab names it.
    title: String,
    branch: String,
    /// The commit the gallery is held at, read-only; `None` follows its branch.
    at_commit: Option<String>,
    /// The commit the table it came from read, which its first page is pinned to.
    opened_at: Option<String>,
    filters: Vec<Filter>,
    sort: Option<Sort>,
    /// The rows it holds, when listed rather than read; how many of them were read.
    ids: Option<Vec<String>>,
    listed_asked: usize,
    /// Properties the commit read does not hold under their current names (a commit made
    /// before they were added or renamed): rows are read as columns without them, as the
    /// table reads them (spec 004).
    absent: HashMap<String, PropertyThen>,
    /// The commit the first page was asked at, so a read again after a refusal asks there.
    first_snapshot: Option<String>,
    /// The cursor's row is to be scrolled to once it is placed.
    scroll_owed: bool,
    /// The Blob property the tiles show, and the type's Blob properties.
    property: Option<String>,
    files: Vec<String>,
    schema: Option<Arc<SchemaModel>>,
    rows: Vec<Row>,
    paging: Paging,
    first_page: Loadable<()>,
    loading_more: bool,
    more_error: Option<String>,
    _page: Option<Task<()>>,
    count: Loadable<i64>,
    _count: Option<Task<()>>,
    /// Bumped by each read from the start: an answer to an older one is let go.
    generation: u64,
    cursor: Option<usize>,
    select_on_load: Option<String>,
    picks: IndexSet<String>,
    pick_anchor: Option<usize>,
    size: TileSize,
    tiles: HashMap<TileKey, Tile>,
    tile_tasks: HashMap<TileKey, (Want, Task<()>)>,
    tile_queue: VecDeque<(TileKey, Want)>,
    /// What the previews and first bytes held take, against [`PREVIEWS_HELD`].
    held: usize,
    /// Where the tiles are; laid out again from the shapes kept when the width or tile size
    /// changes.
    masonry: Masonry,
    /// The span of the columns the grid showed last, top to bottom, and the one whose tiles
    /// were asked for.
    span: Range<f32>,
    span_read: Option<(i32, i32)>,
    _read_wait: Option<Task<()>>,
    /// The branch's diff is being read: nothing is placed until it is in, since the rows the
    /// branch removed go first.
    diff_pending: bool,
    /// A draw owed for tiles that landed, [`LANDING_DELAY`] after the first of them.
    _landing: Option<Task<()>>,
    /// The grid's width as last laid out, and the line layout it gives.
    width: Rc<Cell<f32>>,
    diff: Option<Arc<BranchDiff>>,
    /// The merge base the branch's diff is read from: a removed row's file is read there.
    diff_base: Option<String>,
    _diff: Option<Task<()>>,
    newer: bool,
    scope: Option<ReadScope>,
    _head_watch: Option<HeadWatch>,
    peek: Option<(String, Entity<NodePeek>)>,
    inspected_row: Option<String>,
    grid: Entity<GalleryGrid>,
    scroll: ScrollHandle,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
    #[cfg(test)]
    pub(crate) renders: usize,
}

impl EventEmitter<ItemEvent> for TypeGallery {}

impl Focusable for TypeGallery {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TypeGallery {
    pub(crate) fn new(
        workspace: WeakEntity<Workspace>,
        session: Entity<GraphSession>,
        opening: ShowSetInGallery,
        size: TileSize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let grid = GalleryGrid::new(cx);
        let branch = session.read(cx).branch.clone();
        let filters: Vec<Filter> = opening
            .filters
            .clone()
            .and_then(|filters| serde_json::from_value(filters).ok())
            .unwrap_or_default();
        let sort = opening.sort.as_ref().and_then(|sort| {
            Some(Sort {
                property: sort.get("property")?.as_str()?.to_owned(),
                descending: sort.get("descending")?.as_bool()?,
            })
        });
        let scope = opening.at_commit.is_none().then(|| {
            session.update(cx, |session, _| {
                let branch = session.branch.clone();
                session.read_scope(&branch, graph_store::ledger::ReadView::Gallery)
            })
        });
        let head_watch = opening.at_commit.is_none().then(|| {
            let branch = branch.clone();
            session.update(cx, |session, cx| session.watch_head(&branch, cx))
        });
        let subscriptions = vec![
            cx.subscribe_in(&session, window, Self::on_session_event),
            // The inspector shows the tile under the cursor (spec 020).
            cx.observe_self(|this, cx| this.report_selection(cx)),
            cx.on_release(|this: &mut Self, cx| this.release_all(cx)),
        ];
        let mut this = Self {
            workspace,
            session,
            type_name: opening.type_name,
            title: opening.title,
            branch,
            at_commit: opening.at_commit,
            opened_at: opening.snapshot,
            filters,
            sort,
            ids: opening.ids,
            listed_asked: 0,
            absent: HashMap::new(),
            first_snapshot: None,
            scroll_owed: false,
            property: opening.property,
            files: Vec::new(),
            schema: None,
            rows: Vec::new(),
            paging: Paging::default(),
            first_page: Loadable::NotLoaded,
            loading_more: false,
            more_error: None,
            _page: None,
            count: Loadable::NotLoaded,
            _count: None,
            generation: 0,
            cursor: None,
            select_on_load: opening.select,
            picks: IndexSet::default(),
            pick_anchor: None,
            size,
            tiles: HashMap::new(),
            tile_tasks: HashMap::new(),
            tile_queue: VecDeque::new(),
            held: 0,
            masonry: Masonry::new(1, size.edge(), space::LG, tile_chrome()),
            span: 0.0..0.0,
            span_read: None,
            _read_wait: None,
            diff_pending: false,
            _landing: None,
            width: Rc::new(Cell::new(0.)),
            diff: None,
            diff_base: None,
            _diff: None,
            newer: false,
            scope,
            _head_watch: head_watch,
            peek: None,
            inspected_row: None,
            grid,
            scroll: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
            #[cfg(test)]
            renders: 0,
        };
        this.read_from_start(cx);
        this
    }

    /// What reopens this gallery, as an action: the set it holds, where it reads, and the file
    /// its tiles show.
    pub(crate) fn opening(&self) -> ShowSetInGallery {
        ShowSetInGallery {
            title: self.title.clone(),
            type_name: self.type_name.clone(),
            filters: (!self.filters.is_empty())
                .then(|| serde_json::to_value(&self.filters).ok())
                .flatten(),
            sort: self.sort.as_ref().map(|sort| {
                serde_json::json!({"property": sort.property, "descending": sort.descending})
            }),
            ids: self.ids.clone(),
            property: self.property.clone(),
            at_commit: self.at_commit.clone(),
            snapshot: None,
            select: None,
            preview: false,
        }
    }

    // ---- reading ----

    /// Reads the set from its first page: at the held commit, at the commit the gallery was
    /// opened at (its source's, once), or at the head. Every preview is let go; the cursor's
    /// row is found again.
    fn read_from_start(&mut self, cx: &mut Context<Self>) {
        self.absent.clear();
        self.start_reading(cx);
    }

    fn start_reading(&mut self, cx: &mut Context<Self>) {
        if self.select_on_load.is_none() {
            self.select_on_load = self.cursor_row().map(|row| row.id.clone());
        }
        self.cursor = None;
        self.generation += 1;
        self.release_all(cx);
        self.rows.clear();
        self.paging = Paging::default();
        self.listed_asked = 0;
        self.first_page = Loadable::Loading;
        self.loading_more = false;
        self.more_error = None;
        self.count = Loadable::NotLoaded;
        self.newer = false;
        self.diff = None;
        self.diff_base = None;
        self.diff_pending = false;
        self.span_read = None;
        self.relayout(true);
        self.scroll.set_offset(point(px(0.), px(0.)));
        self.schema = self.session.read(cx).schema_model();
        self.files = self
            .schema
            .as_ref()
            .and_then(|schema| schema.node(&self.type_name))
            .map(|node| {
                node.properties
                    .iter()
                    .filter(|p| p.kind == ScalarKind::Blob)
                    .map(|p| p.name.clone())
                    .collect()
            })
            .unwrap_or_default();
        if self
            .property
            .as_ref()
            .is_none_or(|property| !self.files.contains(property))
        {
            self.property = self.files.first().cloned();
        }
        if self.schema.is_none() {
            // The schema is read with the session; its event reads the set then.
            cx.notify();
            return;
        }
        let snapshot = self.at_commit.clone().or_else(|| self.opened_at.take());
        self.first_snapshot = snapshot.clone();
        self.read_page(snapshot, true, cx);
        self.read_diff(cx);
        cx.notify();
    }

    /// The graph's page size (`studio.type_table_page_size`, or the graph's own).
    fn page_size(&self, cx: &App) -> u32 {
        self.session
            .read(cx)
            .options(cx)
            .type_table_page_size
            .max(1)
    }

    /// The next page's query, and for listed rows how far into the list it reads; the gallery
    /// moves on only once the page is read, so a page that failed is asked again.
    fn page_query(&self, size: u32) -> Option<(BuiltQuery, Option<usize>)> {
        let schema = self.schema.clone()?;
        let without: Vec<String> = self.absent.keys().cloned().collect();
        match &self.ids {
            Some(ids) => {
                let from = self.listed_asked.min(ids.len());
                let to = (from + size as usize).min(ids.len());
                let listed = &ids[from..to];
                let query = if without.is_empty() {
                    builder::nodes_by_ids(&schema, &self.type_name, listed)
                } else {
                    let properties: Vec<String> = schema
                        .node(&self.type_name)?
                        .properties
                        .iter()
                        .filter(|p| p.projectable() && !without.contains(&p.name))
                        .map(|p| p.name.clone())
                        .collect();
                    builder::nodes_fields_by_id(&schema, &self.type_name, listed, &properties)
                };
                query.ok().map(|query| (query, Some(to)))
            }
            None if without.is_empty() => builder::node_page(
                &schema,
                &self.type_name,
                &self.filters,
                self.sort.as_ref(),
                &self.paging.start,
                size,
            )
            .ok()
            .map(|query| (query, None)),
            None => builder::node_page_fields(
                &schema,
                &self.type_name,
                &self.filters,
                self.sort.as_ref(),
                &self.paging.start,
                size,
                &without,
            )
            .ok()
            .map(|query| (query, None)),
        }
    }

    /// Reads the next page, at `snapshot` (the pin once the first page set it).
    fn read_page(&mut self, snapshot: Option<String>, first: bool, cx: &mut Context<Self>) {
        let base = self.page_size(cx);
        let size = self.paging.next_page_size(base);
        let Some((query, listed_to)) = self.page_query(size) else {
            self.first_page = Loadable::Failed(format!(
                "The schema has no node type {} to read.",
                self.type_name
            ));
            return;
        };
        let generation = self.generation;
        let read = self.read_shown(&query, snapshot, cx);
        if !first {
            self.loading_more = true;
        }
        self._page = Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, cx| {
                if this.generation == generation {
                    this.take_page(result, first, size, base, listed_to, cx);
                }
            })
            .ok();
        }));
    }

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

    fn take_page(
        &mut self,
        result: Result<ReadOutput, ReadError>,
        first: bool,
        size: u32,
        base: u32,
        listed_to: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        self.loading_more = false;
        let output = match result {
            Ok(output) => output,
            // A commit made before a property was added: read again without it.
            Err(ReadError::Refused(refusal)) if self.read_without(refusal.historical_gap()) => {
                self.opened_at = self.first_snapshot.clone();
                self.start_reading(cx);
                return;
            }
            Err(error) => {
                if first {
                    self.first_page = Loadable::Failed(error.to_string());
                } else {
                    self.more_error = Some(error.to_string());
                }
                cx.notify();
                return;
            }
        };
        let mut page = if self.absent.is_empty() {
            node_rows(&output.rows)
        } else {
            field_rows(&output.rows)
        };
        if let Some(to) = listed_to {
            self.listed_asked = to;
        }
        match &self.ids {
            Some(ids) => {
                // Listed rows keep the order they were listed in.
                let place: HashMap<&str, usize> = ids
                    .iter()
                    .enumerate()
                    .map(|(ix, id)| (id.as_str(), ix))
                    .collect();
                page.sort_by_key(|row| place.get(row.id.as_str()).copied().unwrap_or(usize::MAX));
                if self.paging.pinned.is_none() {
                    self.paging.pinned = output.graph_commit_id.clone();
                }
                self.paging.exhausted = self.listed_asked >= ids.len();
                self.paging.page_size = size;
                self.paging.ahead = base;
            }
            None => {
                let sort_column = self.sort.as_ref().map(|sort| sort.property.clone());
                let nulls_follow = self.sort_nulls_follow();
                self.paging.accept(
                    output.graph_commit_id.as_deref(),
                    &page,
                    size,
                    base,
                    sort_column.as_deref(),
                    nulls_follow,
                );
            }
        }
        // A row the branch removed is shown first already; it is not read twice.
        let shown: HashSet<String> = self.rows.iter().map(|row| row.id.clone()).collect();
        let before = self.rows.len();
        self.rows
            .extend(page.into_iter().filter(|row| !shown.contains(&row.id)));
        let added = self.rows.len() > before;
        if first {
            self.first_page = Loadable::Loaded(());
            self.compare_head(cx);
            self.read_count(cx);
            self.place_removed();
            cx.emit(ItemEvent::UpdateTab);
        }
        self.count_listed();
        self.find_cursor_row();
        self.place_ready(cx);
        // A page that showed no row and is not the last reads on at once: sorted, the rows with
        // a value ran out and those without one are next; listed, its rows are gone. The rows
        // drawn ask for nothing while there are none.
        if !added && !self.paging.exhausted {
            let pin = self.paging.pinned.clone();
            self.read_page(pin, false, cx);
        } else {
            self.want_more(cx);
        }
        self.read_tiles(cx);
        cx.notify();
    }

    /// A listed set read whole counts the rows it shows: those gone at this commit are not in
    /// it, and those the branch removed are.
    fn count_listed(&mut self) {
        if self.ids.is_some() && self.paging.exhausted {
            self.count = Loadable::Loaded(self.rows.len() as i64);
        }
    }

    /// Reads the page that failed again, at the pin: the same rows, listed or keyset.
    pub(crate) fn retry_page(&mut self, cx: &mut Context<Self>) {
        self.more_error = None;
        let pin = self.paging.pinned.clone();
        self.read_page(pin, false, cx);
        cx.notify();
    }

    /// Puts the cursor on the row it was asked to open at (the table's cursor row, or the
    /// cursor's before a read again) once that row is read; meanwhile there is none, and pages
    /// are read on to find it. Once the set is read whole without it, the first tile.
    fn find_cursor_row(&mut self) {
        let Some(id) = self.select_on_load.clone() else {
            if self.cursor.is_none() && !self.rows.is_empty() {
                self.cursor = Some(0);
            }
            return;
        };
        if let Some(row) = self.rows.iter().position(|row| row.id == id) {
            self.cursor = Some(row);
            self.select_on_load = None;
            self.scroll_owed = true;
        } else if self.paging.exhausted && !self.diff_pending {
            self.select_on_load = None;
            self.cursor = (!self.rows.is_empty()).then_some(0);
        }
    }

    /// Notes a property the commit read does not hold under its current name, so the rows are
    /// read again without it; false when the refusal names no new property of the type. Once
    /// per property, so the reads end.
    fn read_without(&mut self, gap: Option<HistoricalGap>) -> bool {
        let Some(HistoricalGap::Property {
            owner,
            property,
            then,
        }) = gap
        else {
            return false;
        };
        let known = self
            .schema
            .as_ref()
            .and_then(|schema| schema.node(&self.type_name))
            .is_some_and(|node| node.property(&property).is_some());
        if owner != self.type_name || !known || self.absent.contains_key(&property) {
            return false;
        }
        self.absent.insert(property, then);
        true
    }

    fn sort_nulls_follow(&self) -> bool {
        self.at_commit.is_some()
            || self
                .sort
                .as_ref()
                .and_then(|sort| {
                    self.schema
                        .as_ref()?
                        .node(&self.type_name)?
                        .property(&sort.property)
                        .cloned()
                })
                .is_none_or(|property| property.nullable)
    }

    /// Reads the next page once the rows a screen below the view would need come within a page
    /// of the last row read.
    fn want_more(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.first_page, Loadable::Loaded(())) || self.more_error.is_some() {
            return;
        }
        let wanted = self.masonry.placed() + self.ahead_rows();
        // The row the cursor opens at is not read yet: the pages are read on to it.
        let finding = self.select_on_load.is_some() && !self.loading_more && !self.paging.exhausted;
        if finding
            || self
                .paging
                .wants_more(self.rows.len(), wanted, self.loading_more)
        {
            let pin = self.paging.pinned.clone();
            self.read_page(pin, false, cx);
        }
    }

    /// The count of the set at the pin: what the session counted, or one read; a listed set is
    /// as long as its list.
    fn read_count(&mut self, cx: &mut Context<Self>) {
        if let Some(ids) = &self.ids {
            self.count = Loadable::Loaded(ids.len() as i64);
            return;
        }
        if self.paging.exhausted {
            self.count = Loadable::Loaded(self.rows.len() as i64);
            return;
        }
        let Some(query) = self
            .schema
            .as_ref()
            .and_then(|schema| builder::node_count(schema, &self.type_name, &self.filters).ok())
        else {
            return;
        };
        let generation = self.generation;
        self.count = Loadable::Loading;
        let read = self.read_shown(&query, self.paging.pinned.clone(), cx);
        self._count = Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.count = match result {
                    Ok(output) => builder::count_of(&output.rows)
                        .map_or_else(|| Loadable::Failed("No count".into()), Loadable::Loaded),
                    Err(error) => Loadable::Failed(error.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// What the branch changed in the type, against its target (spec 033), read as the table
    /// reads it: on a branch other than the target, following it.
    fn read_diff(&mut self, cx: &mut Context<Self>) {
        self._diff = None;
        if self.branch == TARGET || self.at_commit.is_some() {
            return;
        }
        let (session, branch, type_name) = (
            self.session.clone(),
            self.branch.clone(),
            self.type_name.clone(),
        );
        let generation = self.generation;
        self.diff_pending = true;
        self._diff = Some(cx.spawn(async move |this, cx| {
            let Some((base, folded)) =
                crate::table::read_branch_diff_from(&session, &branch, cx).await
            else {
                // Unread, the tiles are placed unmarked, as a table shows its rows unmarked.
                this.update(cx, |this, cx| {
                    if this.generation == generation {
                        this.diff_pending = false;
                        this.find_cursor_row();
                        this.place_ready(cx);
                        this.read_tiles(cx);
                        cx.notify();
                    }
                })
                .ok();
                return;
            };
            let diff =
                cx.background_executor()
                    .spawn(async move {
                        BranchDiff::of(&folded, graph_store::EntityKind::Node, &type_name)
                    })
                    .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.diff = Some(Arc::new(diff));
                this.diff_base = Some(base);
                this.diff_pending = false;
                this.place_removed();
                this.count_listed();
                this.find_cursor_row();
                this.place_ready(cx);
                this.read_tiles(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Puts the rows the branch removed first, once the first page and the diff are both in:
    /// before any tile is placed ([`Self::diff_pending`]).
    fn place_removed(&mut self) {
        let (Some(diff), Loadable::Loaded(())) = (self.diff.clone(), &self.first_page) else {
            return;
        };
        let shown: HashSet<&str> = self.rows.iter().map(|row| row.id.as_str()).collect();
        // A gallery of listed rows shows the listed ones the branch removed, in their order.
        let listed: Option<HashMap<&str, usize>> = self.ids.as_ref().map(|ids| {
            ids.iter()
                .enumerate()
                .map(|(ix, id)| (id.as_str(), ix))
                .collect()
        });
        let mut placed: Vec<Row> = diff
            .removed()
            .iter()
            .filter(|row| !shown.contains(row.id.as_str()))
            .filter(|row| {
                listed
                    .as_ref()
                    .is_none_or(|ids| ids.contains_key(row.id.as_str()))
            })
            .cloned()
            .collect();
        if let Some(listed) = &listed {
            placed.sort_by_key(|row| listed.get(row.id.as_str()).copied());
        }
        if placed.is_empty() {
            return;
        }
        let moved = placed.len();
        self.rows.splice(0..0, placed);
        // The cursor stays on its tile, which moves on by the tiles put before it.
        self.cursor = self.cursor.map(|cursor| cursor + moved);
        self.pick_anchor = self.pick_anchor.map(|anchor| anchor + moved);
    }

    fn change_of(&self, id: &str) -> Option<NetChange> {
        self.diff.as_ref()?.mark(id).map(|mark| mark.net)
    }

    fn is_removed(&self, id: &str) -> bool {
        self.change_of(id) == Some(NetChange::Removed)
    }

    /// Whether the branch changed the file the tiles show in row `id`.
    fn file_changed(&self, id: &str) -> bool {
        let Some(property) = &self.property else {
            return false;
        };
        self.diff
            .as_ref()
            .and_then(|diff| diff.mark(id))
            .is_some_and(|mark| mark.changed.contains(property))
    }

    /// Offers Refresh once the branch's head is past the pin.
    fn compare_head(&mut self, cx: &App) {
        if self.at_commit.is_some() {
            return;
        }
        let head = self.session.read(cx).head(&self.branch).map(str::to_owned);
        self.newer = head.is_some() && head != self.paging.pinned;
    }

    fn on_session_event(
        &mut self,
        _: &Entity<GraphSession>,
        event: &SessionEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SessionEvent::SchemaChanged if self.schema.is_none() => self.read_from_start(cx),
            SessionEvent::SchemaChanged | SessionEvent::SchemaMoved => {
                self.newer = true;
                cx.notify();
            }
            // The window moved to another branch: a gallery that follows its branch reads it.
            SessionEvent::BranchChanged if self.at_commit.is_none() => {
                self.branch = self.session.read(cx).branch.clone();
                let branch = self.branch.clone();
                self.scope = Some(self.session.update(cx, |session, _| {
                    session.read_scope(&branch, graph_store::ledger::ReadView::Gallery)
                }));
                self._head_watch = Some(
                    self.session
                        .update(cx, |session, cx| session.watch_head(&branch, cx)),
                );
                self.read_from_start(cx);
            }
            SessionEvent::HeadMoved { branch, head }
                if *branch == self.branch && self.at_commit.is_none() =>
            {
                let newer = self.paging.pinned.as_deref().is_some_and(|pin| pin != head);
                if newer != self.newer {
                    self.newer = newer;
                    cx.notify();
                }
            }
            // A new token may see less: what was read under the old one is read again.
            SessionEvent::CredentialChanged => {
                self.opened_at = self.paging.pinned.clone();
                self.read_from_start(cx);
            }
            _ => {}
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.read_from_start(cx);
    }

    // ---- tiles and their places ----

    /// The columns for the width last laid out.
    fn column_layout(&self) -> (usize, f32) {
        let width = self.width.get();
        let width = if width > 0. {
            width
        } else {
            studio_ui::ds::size::WINDOW_WIDTH - studio_ui::ds::size::SIDEBAR_WIDTH
        };
        column_layout(width, self.size.edge())
    }

    /// Lays the columns out again for the width and tile size, from the shapes kept: no read.
    /// `always` empties them even when they fit (a read from the start, another file shown).
    fn relayout(&mut self, always: bool) {
        let (columns, width) = self.column_layout();
        if !always && self.masonry.fits(columns, width) {
            return;
        }
        self.masonry = Masonry::new(columns, width, space::LG, tile_chrome());
        self.place_known();
        // Other tiles meet the same span now: they are asked for at the next draw.
        self.span_read = None;
    }

    /// The shape of row `row`'s tile, once known.
    fn shape_of(&self, row: usize) -> Option<f32> {
        let property = self.property.as_ref()?;
        let id = &self.rows.get(row)?.id;
        self.tiles
            .get(&(id.clone(), property.clone()))
            .and_then(|tile| tile.shape)
    }

    /// Places the rows whose shapes are known, in row order, up to the first that is not;
    /// nothing while the branch's diff is out, since the rows it removed go first.
    fn place_known(&mut self) -> usize {
        if self.diff_pending || !matches!(self.first_page, Loadable::Loaded(())) {
            return 0;
        }
        let from = self.masonry.placed();
        let shapes: Vec<Option<f32>> = (from..self.rows.len())
            .map(|row| self.shape_of(row))
            .collect();
        let placed = self.masonry.place_known(self.rows.len(), |row| {
            shapes.get(row - from).copied().flatten()
        });
        // The row the gallery opened at, once placed, is scrolled to.
        if self.scroll_owed
            && let Some(cursor) = self.cursor.filter(|&row| row < self.masonry.placed())
        {
            self.scroll_owed = false;
            self.scroll_to_tile(cursor);
        }
        placed
    }

    /// Places what is ready; when a tile was placed, the rows after it are measured on.
    fn place_ready(&mut self, cx: &mut Context<Self>) {
        if self.place_known() > 0 {
            self.want_more(cx);
        }
    }

    /// How many rows below the last placed one are measured ahead: a screen's worth of square
    /// tiles and a line more, while the shortest column ends within a screen of the view's
    /// bottom.
    fn ahead_rows(&self) -> usize {
        let screen = self.screen_height();
        if self.masonry.placed() > 0 && self.masonry.shortest() > self.span.end + screen {
            return 0;
        }
        let columns = self.masonry.columns();
        let lines = (screen / self.masonry.width().max(1.)).ceil() as usize;
        columns * (lines + 1)
    }

    /// The height of the view, as last drawn.
    fn screen_height(&self) -> f32 {
        let height = f32::from(self.scroll.bounds().size.height);
        if height > 0. {
            height
        } else {
            studio_ui::ds::size::WINDOW_HEIGHT
        }
    }

    /// The commit a row's file is read at: where it last was for a row the branch removed, else
    /// the pin.
    fn commit_of(&self, id: &str) -> Option<String> {
        if self.is_removed(id) {
            return self.diff_base.clone();
        }
        self.paging.pinned.clone()
    }

    /// The grid showed `span` of its columns: the next page is read when the rows needed below
    /// near the end of those read, and the tiles' files once scrolling rests.
    fn saw(&mut self, span: Range<f32>, cx: &mut Context<Self>) {
        self.span = span.clone();
        self.want_more(cx);
        let key = (span.start.round() as i32, span.end.round() as i32);
        if self.span_read == Some(key) || self.property.is_none() {
            return;
        }
        self.span_read = Some(key);
        self._read_wait = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(READ_AFTER).await;
            this.update(cx, |this, cx| this.read_tiles(cx)).ok();
        }));
    }

    /// What the view wants read: the files of the tiles it shows, then the shapes of the rows
    /// a screen below the last placed one, each in row order.
    fn wanted(&self) -> Vec<(TileKey, Want)> {
        let Some(property) = &self.property else {
            return Vec::new();
        };
        let key = |row: usize| (self.rows[row].id.clone(), property.clone());
        let mut wanted: Vec<(TileKey, Want)> = self
            .masonry
            .meeting(self.span.clone())
            .into_iter()
            .map(|row| (key(row), Want::Preview))
            .collect();
        let from = self.masonry.placed();
        // The cursor's row, not placed yet (the row the gallery opened at), is measured to as
        // well: every row before it is placed first.
        let to_cursor = self
            .cursor
            .filter(|_| self.scroll_owed)
            .map_or(0, |cursor| cursor + 1);
        let to = (from + self.ahead_rows())
            .max(to_cursor)
            .min(self.rows.len());
        wanted.extend((from..to).map(|row| (key(row), Want::Shape)));
        wanted
    }

    /// Whether a tile still needs what `want` asks.
    fn needs(&self, key: &TileKey, want: Want) -> bool {
        match self.tiles.get(key) {
            None => true,
            Some(tile) if matches!(tile.meta, Loadable::Failed(_)) => false,
            Some(tile) => match want {
                Want::Shape => tile.shape.is_none(),
                Want::Preview => !matches!(tile.look, Loadable::Loaded(_) | Loadable::Failed(_)),
            },
        }
    }

    /// Reads what the view wants, the tiles shown first; a preview's read out for a tile no
    /// longer shown is dropped, which stops its request, and asked again when it is. A read out
    /// for a tile's shape finishes, and its preview is asked after when it is shown.
    pub(crate) fn read_tiles(&mut self, cx: &mut Context<Self>) {
        let wanted: Vec<(TileKey, Want)> = self
            .wanted()
            .into_iter()
            .filter(|(key, want)| self.needs(key, *want))
            .collect();
        let wants: HashSet<&TileKey> = wanted.iter().map(|(key, _)| key).collect();
        // A shape's read out is let finish: one bounded read, wanted again as soon as the
        // columns reach its row, and dropped it would be sent twice.
        let gone: Vec<TileKey> = self
            .tile_tasks
            .iter()
            .filter(|(key, (want, _))| *want == Want::Preview && !wants.contains(key))
            .map(|(key, _)| key.clone())
            .collect();
        for key in gone {
            self.tile_tasks.remove(&key);
            if let Some(tile) = self.tiles.get_mut(&key) {
                if matches!(tile.meta, Loadable::Loading) {
                    tile.meta = Loadable::NotLoaded;
                }
                if matches!(tile.look, Loadable::Loading) {
                    tile.look = Loadable::NotLoaded;
                }
            }
        }
        self.tile_queue = wanted.into_iter().collect();
        self.read_queued(cx);
    }

    fn read_queued(&mut self, cx: &mut Context<Self>) {
        while self.tile_tasks.len() < READS_AT_ONCE {
            let Some((key, want)) = self.tile_queue.pop_front() else {
                break;
            };
            if self.tile_tasks.contains_key(&key) || !self.needs(&key, want) {
                continue;
            }
            let tile = self.tiles.entry(key.clone()).or_default();
            if want == Want::Preview {
                tile.look = Loadable::Loading;
            }
            let task = self.read_tile(key.clone(), want, cx);
            self.tile_tasks.insert(key, (want, task));
        }
    }

    /// One tile's read: what the cell holds; its first bytes, which give its shape; and, for a
    /// tile shown, the rest of a file drawn whole, then its preview made off the main thread. A
    /// file whose first bytes are all of it has its preview made at once, wherever it is.
    fn read_tile(&mut self, key: TileKey, want: Want, cx: &mut Context<Self>) -> Task<()> {
        let cell = BlobCell {
            edge: false,
            type_name: self.type_name.clone(),
            id: key.0.clone(),
            property: key.1.clone(),
        };
        let snapshot = self.commit_of(&key.0);
        // A removed row's file is read where it last was, which is no view's commit.
        let scope = (!self.is_removed(&key.0))
            .then(|| self.scope.clone())
            .flatten();
        let (known, head) = match self.tiles.get_mut(&key) {
            Some(tile) => (tile.meta.loaded().cloned(), tile.head.take()),
            None => (None, None),
        };
        if let Some(head) = &head {
            self.held = self.held.saturating_sub(head.len());
        }
        let session = self.session.downgrade();
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let meta = match known {
                Some(meta) => Ok(meta),
                None => {
                    let Ok(read) = session.update(cx, |session, cx| match &scope {
                        Some(scope) => {
                            session.blob_meta_in(scope, cell.clone(), snapshot.clone(), cx)
                        }
                        None => session.blob_meta(cell.clone(), snapshot.clone(), cx),
                    }) else {
                        return;
                    };
                    read.await.map_err(|error| error.to_string())
                }
            };
            let size = match &meta {
                Ok(BlobMeta::Managed { size, .. }) if *size > 0 => Some(*size),
                _ => None,
            };
            let go_on = this
                .update(cx, |this, cx| {
                    if this.generation != generation {
                        return false;
                    }
                    let tile = this.tiles.entry(key.clone()).or_default();
                    match meta {
                        Ok(meta) => {
                            if size.is_none() {
                                // No file, an empty one or a reference: a short band, at once.
                                tile.shape = Some(tile::shape::NO_FILE);
                                tile.look = Loadable::Loaded(match meta {
                                    BlobMeta::Managed { .. } => Look::Empty,
                                    _ => Look::Icon,
                                });
                            }
                            tile.meta = Loadable::Loaded(meta);
                        }
                        Err(error) => {
                            tile.shape = Some(tile::shape::NO_FILE);
                            tile.meta = Loadable::Failed(error.clone());
                            tile.look = Loadable::Failed(error);
                        }
                    }
                    if size.is_none() {
                        this.tile_landed(&key, cx);
                    }
                    true
                })
                .unwrap_or(false);
            let (Some(size), true) = (size, go_on) else {
                return;
            };
            let first = match head {
                Some(head) => Ok(head),
                None => {
                    let to = size.min(FIRST_READ);
                    read_bytes(&session, &cell, &snapshot, scope.as_ref(), 0, to, cx).await
                }
            };
            let first = match first {
                Ok(first) => first,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        if let Some(tile) = this.tiles.get_mut(&key) {
                            tile.shape.get_or_insert(tile::shape::NO_FILE);
                            tile.look = Loadable::Failed(error);
                        }
                        this.tile_landed(&key, cx);
                    })
                    .ok();
                    return;
                }
            };
            let facts = Facts::of(&first);
            let whole = first.len() as u64 >= size;
            // Measured ahead and not whole: its first bytes wait for its preview's read.
            let measured_only = want == Want::Shape && !whole;
            let stop = this
                .update(cx, |this, cx| {
                    if this.generation != generation {
                        return true;
                    }
                    let tile = this.tiles.entry(key.clone()).or_default();
                    tile.facts = Some(facts);
                    tile.shape = Some(facts.shape());
                    if measured_only {
                        tile.head = Some(first.clone());
                        if matches!(tile.look, Loadable::Loading) {
                            tile.look = Loadable::NotLoaded;
                        }
                        this.held += first.len();
                        this.tile_landed(&key, cx);
                    }
                    measured_only
                })
                .unwrap_or(true);
            if stop {
                return;
            }
            let bytes = if !whole && tile::draws_whole(&facts, size) {
                let from = first.len() as u64;
                read_bytes(&session, &cell, &snapshot, scope.as_ref(), from, size, cx)
                    .await
                    .map(|rest| {
                        let mut bytes = first;
                        bytes.extend(rest);
                        bytes
                    })
            } else {
                Ok(first)
            };
            let look = match bytes {
                Ok(bytes) => {
                    let svg = cx.update(|cx| cx.svg_renderer());
                    Ok(cx
                        .background_spawn(async move { tile::look(bytes, size, &svg) })
                        .await)
                }
                Err(error) => Err(error),
            };
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    if let Ok(Look::Image { image, .. }) = &look {
                        image.release(cx);
                    }
                    return;
                }
                if let Some(tile) = this.tiles.get_mut(&key) {
                    tile.look = match look {
                        Ok(look) => {
                            this.held += look.held();
                            Loadable::Loaded(look)
                        }
                        Err(error) => Loadable::Failed(error),
                    };
                }
                this.tile_landed(&key, cx);
                this.let_go_past_cap(cx);
            })
            .ok();
        })
    }

    /// A tile's read ended: what it made known is placed, the next read starts, and the grid
    /// draws it [`LANDING_DELAY`] from the first landing not yet drawn, with every other that
    /// lands meanwhile.
    fn tile_landed(&mut self, key: &TileKey, cx: &mut Context<Self>) {
        self.tile_tasks.remove(key);
        if self.place_known() > 0 {
            // The columns grew: rows further below, or tiles now shown, may be wanted.
            self.want_more(cx);
            self.read_tiles(cx);
        } else {
            self.read_queued(cx);
        }
        if self._landing.is_some() {
            return;
        }
        self._landing = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LANDING_DELAY).await;
            this.update(cx, |this, cx| {
                this._landing = None;
                cx.notify();
            })
            .ok();
        }));
    }

    /// Frees the previews and first bytes of the tiles farthest from the span shown while
    /// they hold more than [`PREVIEWS_HELD`]; each is read again if it comes back into view.
    /// The tiles shown keep theirs, so a screen that alone holds more is not read again and
    /// again; every shape is kept.
    fn let_go_past_cap(&mut self, cx: &mut App) {
        if self.held <= PREVIEWS_HELD {
            return;
        }
        let shown: HashSet<usize> = self
            .masonry
            .meeting(self.span.clone())
            .into_iter()
            .collect();
        let middle = (self.span.start + self.span.end) / 2.;
        let place: HashMap<&str, usize> = self
            .rows
            .iter()
            .enumerate()
            .map(|(ix, row)| (row.id.as_str(), ix))
            .collect();
        let mut held: Vec<(f32, TileKey)> = self
            .tiles
            .iter()
            .filter(|(_, tile)| tile.held() > 0)
            .filter_map(|(key, _)| {
                let row = place.get(key.0.as_str()).copied()?;
                if shown.contains(&row) {
                    return None;
                }
                // A row not placed yet is below every placed one.
                let distance = self
                    .masonry
                    .place_of(row)
                    .map_or(f32::MAX, |place| (place.middle() - middle).abs());
                Some((distance, key.clone()))
            })
            .collect();
        held.sort_by(|a, b| b.0.total_cmp(&a.0));
        for (_, key) in held {
            if self.held <= PREVIEWS_HELD {
                break;
            }
            let Some(tile) = self.tiles.get_mut(&key) else {
                continue;
            };
            if let Some(head) = tile.head.take() {
                self.held = self.held.saturating_sub(head.len());
            }
            if let Loadable::Loaded(look) = std::mem::replace(&mut tile.look, Loadable::NotLoaded) {
                self.held = self.held.saturating_sub(look.held());
                if let Look::Image { image, .. } = look {
                    image.release(cx);
                }
            }
        }
    }

    /// Frees every preview and drops every read: a read from the start, or the tab closed.
    fn release_all(&mut self, cx: &mut App) {
        for tile in self.tiles.values() {
            if let Loadable::Loaded(Look::Image { image, .. }) = &tile.look {
                image.release(cx);
            }
        }
        self.tiles.clear();
        self.tile_tasks.clear();
        self.tile_queue.clear();
        self.held = 0;
    }

    // ---- the cursor and picks ----

    fn cursor_row(&self) -> Option<&Row> {
        self.rows.get(self.cursor?)
    }

    fn selected_row_id(&self) -> Option<&str> {
        self.cursor_row()
            .filter(|row| !self.is_removed(&row.id))
            .map(|row| row.id.as_str())
    }

    fn report_selection(&mut self, cx: &mut Context<Self>) {
        let selected = self.selected_row_id().map(str::to_owned);
        if selected != self.inspected_row {
            self.inspected_row = selected;
            cx.emit(ItemEvent::Selected);
        }
    }

    /// Scrolls the least that shows row `row`'s tile whole, once it is placed.
    fn scroll_to_tile(&self, row: usize) {
        let Some(place) = self.masonry.place_of(row) else {
            return;
        };
        let height = self.screen_height();
        let top = -f32::from(self.scroll.offset().y);
        // The columns start [`space::EDGE`] down the scroll area.
        let (tile_top, tile_bottom) = (place.top, place.bottom() + 2. * space::EDGE);
        let to = if tile_top < top {
            tile_top
        } else if tile_bottom > top + height {
            tile_bottom - height
        } else {
            return;
        };
        self.scroll.set_offset(point(px(0.), px(-to.max(0.))));
    }

    /// Moves the cursor: within its column by `by` tiles, or with `across` to the tile in the
    /// next column (`by` −1 or 1) whose middle is nearest. A cursor on a row not placed yet
    /// goes to the first tile.
    pub(crate) fn move_cursor(&mut self, by: isize, across: bool, cx: &mut Context<Self>) {
        if self.masonry.placed() == 0 {
            return;
        }
        let next = match self.cursor.filter(|&row| row < self.masonry.placed()) {
            None => Some(0),
            Some(row) if across => self.masonry.across(row, by),
            Some(row) => self.masonry.vertical(row, by),
        };
        if let Some(next) = next {
            self.set_cursor(next, cx);
        }
    }

    fn set_cursor(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.cursor == Some(ix) {
            return;
        }
        self.cursor = Some(ix);
        self.scroll_to_tile(ix);
        cx.notify();
    }

    fn move_to_end(&mut self, cx: &mut Context<Self>) {
        if let Some(last) = self.masonry.placed().checked_sub(1) {
            self.set_cursor(last, cx);
        }
    }

    /// Picks the tile at `ix`, or takes it out; with `range`, picks from the last tile picked.
    fn pick(&mut self, ix: usize, range: bool, cx: &mut Context<Self>) {
        let Some(id) = self.rows.get(ix).map(|row| row.id.clone()) else {
            return;
        };
        if self.is_removed(&id) {
            return;
        }
        match self.pick_anchor.filter(|_| range) {
            Some(anchor) => {
                for ix in anchor.min(ix)..=anchor.max(ix).min(self.rows.len() - 1) {
                    let id = self.rows[ix].id.clone();
                    if !self.is_removed(&id) {
                        self.picks.insert(id);
                    }
                }
            }
            None => {
                if !self.picks.shift_remove(&id) {
                    self.picks.insert(id);
                }
                self.pick_anchor = Some(ix);
            }
        }
        cx.notify();
    }

    fn pick_all(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<String> = self
            .rows
            .iter()
            .filter(|row| !self.is_removed(&row.id))
            .map(|row| row.id.clone())
            .collect();
        self.picks.extend(ids);
        cx.notify();
    }

    fn clear_picks(&mut self, cx: &mut Context<Self>) {
        if self.picks.is_empty() {
            return;
        }
        self.picks.clear();
        self.pick_anchor = None;
        cx.notify();
    }

    fn copy_ids(&self, cx: &mut App) {
        let ids: Vec<&str> = if self.picks.is_empty() {
            self.selected_row_id().into_iter().collect()
        } else {
            self.picks.iter().map(String::as_str).collect()
        };
        if !ids.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(ids.join("\n")));
        }
    }

    fn click_tile(
        &mut self,
        ix: usize,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        let modifiers = event.modifiers();
        if modifiers.secondary(cx) || modifiers.shift {
            self.pick(ix, modifiers.shift, cx);
            self.set_cursor(ix, cx);
            return;
        }
        self.set_cursor(ix, cx);
        if event.click_count() >= 2 {
            self.open_file(window, cx);
        }
    }

    fn open_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(row), Some(property)) = (self.cursor_row(), self.property.clone()) else {
            return;
        };
        let removed = self.is_removed(&row.id);
        let at_commit = self.commit_of(&row.id);
        window.dispatch_action(
            Box::new(OpenBlob {
                type_name: self.type_name.clone(),
                id: row.id.clone(),
                property,
                edge: false,
                // Held where it was read when the gallery holds its commit or the row is gone
                // from the branch; else it writes on the branch, bound to the value read here.
                held: self.at_commit.is_some() || removed,
                at_commit,
            }),
            cx,
        );
    }

    fn open_row_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected_row_id().map(str::to_owned) else {
            return;
        };
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| workspace.keep_active_tab(cx));
        }
        window.dispatch_action(
            Box::new(OpenNodePage {
                type_name: self.type_name.clone(),
                id,
                at_commit: self.at_commit.clone(),
                preview: false,
            }),
            cx,
        );
    }

    fn show_file(&mut self, property: String, cx: &mut Context<Self>) {
        if self.property.as_ref() == Some(&property) {
            return;
        }
        self.property = Some(property);
        // Another file, other shapes: the columns are placed again from the first tile.
        self.relayout(true);
        self.span_read = None;
        self.read_tiles(cx);
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn show_next_file(&mut self, cx: &mut Context<Self>) {
        let next = match &self.property {
            Some(shown) => self
                .files
                .iter()
                .position(|file| file == shown)
                .map(|ix| self.files[(ix + 1) % self.files.len()].clone()),
            None => self.files.first().cloned(),
        };
        if let Some(next) = next {
            self.show_file(next, cx);
        }
    }

    fn set_size(&mut self, size: TileSize, cx: &mut Context<Self>) {
        if self.size == size {
            return;
        }
        self.size = size;
        // The columns flow again from the shapes kept, with no read.
        self.relayout(false);
        self.span_read = None;
        if let Some(cursor) = self.cursor {
            self.scroll_to_tile(cursor);
        }
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    // ---- what it opens ----

    fn row_label(&self, row: &Row) -> Option<String> {
        let schema = self.schema.as_ref()?;
        let label = schema.node(&self.type_name)?.label_property()?;
        row.values
            .get(label)
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    fn picked_nodes(&self) -> Option<Vec<ResultNode>> {
        (!self.picks.is_empty()).then(|| {
            self.rows
                .iter()
                .filter(|row| self.picks.contains(&row.id))
                .map(|row| ResultNode {
                    type_name: self.type_name.clone(),
                    id: row.id.clone(),
                    label: self.row_label(row),
                })
                .collect()
        })
    }

    fn filters_value(&self) -> Option<Value> {
        (!self.filters.is_empty())
            .then(|| serde_json::to_value(&self.filters).ok())
            .flatten()
    }

    // ---- drawing ----

    fn count_text(&self) -> String {
        let total = match &self.count {
            Loadable::Loaded(n) => Some(*n),
            _ => None,
        };
        match total {
            Some(total) if self.paging.exhausted || self.rows.len() as i64 >= total => {
                plural(total.max(0) as usize, "row", "rows")
            }
            Some(total) => format!(
                "{} of {} read",
                studio_ui::group_count(self.rows.len() as i64),
                studio_ui::group_count(total)
            ),
            None if matches!(self.count, Loadable::Loading) => "counting…".into(),
            None => plural(self.rows.len(), "row", "rows"),
        }
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        studio_ui::ds::toolbar(cx)
            .children(studio_workspace::view_strip(Item::views(self, cx), cx))
            .child(studio_ui::kind_swatch(&self.type_name, false, cx))
            .child(studio_ui::ds::text::title(self.type_name.clone()))
            .child(studio_ui::ds::text::muted(self.count_text()))
            .children(self.render_diff_counts())
            .when_some(self.at_commit.as_ref(), |bar, commit| {
                bar.child(studio_ui::ds::meta_code(
                    IconName::GitCommit,
                    format!("Read only · {}", studio_ui::short_commit_id(commit)),
                    cx,
                ))
            })
            .when(self.newer, |bar| {
                bar.child(
                    studio_ui::ds::button("gallery-newer", "Newer data · Refresh")
                        .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                        .tip(studio_ui::ds::tooltip_for_action(
                            "Read the branch head again",
                            &Refresh,
                        ))
                        .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                )
            })
            .child(div().flex_1())
            .child(
                studio_ui::ds::toolbar_actions()
                    .gap(px(space::SM))
                    .child(self.render_file_menu(cx))
                    .child(self.render_sizes(cx)),
            )
    }

    fn render_diff_counts(&self) -> Option<AnyElement> {
        let (added, changed, removed) = self.diff.as_ref()?.counts();
        if added + changed + removed == 0 {
            return None;
        }
        let tooltip = format!(
            "On {} against {TARGET}: {} added, {} changed, {} removed",
            self.branch,
            plural(added, "row", "rows"),
            plural(changed, "row", "rows"),
            plural(removed, "row", "rows"),
        );
        Some(
            div()
                .id("gallery-diff-counts")
                .tip(studio_ui::ds::tooltip(tooltip))
                .child(studio_ui::change_counts(added, changed, removed))
                .into_any_element(),
        )
    }

    /// The File chip: which Blob property the tiles show.
    fn render_file_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let gallery = cx.entity().downgrade();
        let (files, shown) = (self.files.clone(), self.property.clone());
        let menu = PopoverMenu::new("gallery-file-menu")
            .anchor(gpui::Anchor::TopRight)
            .trigger(
                studio_ui::ds::chip_button(
                    "gallery-file",
                    IconName::Image,
                    "File",
                    self.property.clone().map(SharedString::from),
                )
                .disabled(self.files.len() < 2)
                .tip(if self.files.len() < 2 {
                    studio_ui::ds::tooltip(format!(
                        "The tiles show {}, the type's one file",
                        self.property.as_deref().unwrap_or("no file")
                    ))
                } else {
                    studio_ui::ds::tooltip_for_action("Which file the tiles show", &ShowNextFile)
                }),
            )
            .menu(move |window, cx| {
                let (gallery, files, shown) = (gallery.clone(), files.clone(), shown.clone());
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for file in &files {
                        let (gallery, chosen) = (gallery.clone(), file.clone());
                        menu = menu.toggleable_entry(
                            file.clone(),
                            shown.as_ref() == Some(file),
                            ui::IconPosition::Start,
                            None,
                            move |_, cx| {
                                gallery
                                    .update(cx, |gallery, cx| gallery.show_file(chosen.clone(), cx))
                                    .ok();
                            },
                        );
                    }
                    menu
                }))
            });
        studio_ui::ds::control_chip("gallery-file-chip", menu)
    }

    /// The tile size, as a segmented control; `=` and `−` step it.
    fn render_sizes(&self, cx: &mut Context<Self>) -> impl IntoElement {
        studio_ui::ds::segmented_track(cx).children(TileSize::ALL.into_iter().map(|size| {
            let current = size == self.size;
            studio_ui::ds::segment(("gallery-size", size as usize), size.label(), current, cx)
                .tip(studio_ui::ds::tooltip(match size {
                    TileSize::Small => "Small tiles · −",
                    TileSize::Medium => "Medium tiles",
                    TileSize::Large => "Large tiles · =",
                }))
                .on_click(cx.listener(move |this, _, _, cx| this.set_size(size, cx)))
        }))
    }

    /// The grid, or the state that stands in for it.
    fn render_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let focus = self.focus_handle.clone();
        match &self.first_page {
            Loadable::NotLoaded | Loadable::Loading => {
                return studio_ui::ds::loading_state("Reading…").into_any_element();
            }
            Loadable::Failed(reason) => {
                return studio_ui::ds::error_state(
                    format!("Could not read {}", self.type_name),
                    reason.clone(),
                )
                .child(
                    studio_ui::ds::state_action("gallery-retry", "Retry", &Refresh, &focus, cx)
                        .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                )
                .into_any_element();
            }
            Loadable::Loaded(()) => {}
        }
        if self.property.is_none() {
            return studio_ui::ds::empty_state(
                IconName::Image,
                format!("{} has no files now", self.type_name),
            )
            .into_any_element();
        }
        // Pages that showed no row read on: the set is empty only once read whole.
        if self.rows.is_empty() && (!self.paging.exhausted || self.diff_pending) {
            if let Some(error) = &self.more_error {
                return studio_ui::ds::error_state(
                    format!("Could not read {}", self.type_name),
                    error.clone(),
                )
                .child(
                    studio_ui::ds::state_action("gallery-retry", "Retry", &Refresh, &focus, cx)
                        .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                )
                .into_any_element();
            }
            return studio_ui::ds::loading_state("Reading…").into_any_element();
        }
        if self.rows.is_empty() {
            let title = if self.filters.is_empty() && self.ids.is_none() {
                "No rows"
            } else {
                "No rows match"
            };
            return studio_ui::ds::empty_state(IconName::Image, title).into_any_element();
        }
        let width = self.width.clone();
        let gallery = cx.entity_id();
        let measure = gpui::canvas(
            move |bounds, _, cx| {
                let now = f32::from(bounds.size.width);
                if (width.get() - now).abs() > 0.5 {
                    width.set(now);
                    cx.defer(move |cx| cx.notify(gallery));
                }
            },
            |_, (), _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .w_full()
        .h(px(0.));
        // The columns flow again from the shapes kept when the width changed.
        self.relayout(false);
        let inset = space::EDGE;
        let top = -f32::from(self.scroll.offset().y);
        let screen = {
            let height = f32::from(self.scroll.bounds().size.height);
            if height > 0. {
                height
            } else {
                f32::from(window.viewport_size().height)
            }
        };
        // The span of the columns in view; tiles half a screen either side are laid out too,
        // so a scroll shows them drawn.
        let span = (top - inset)..(top - inset + screen);
        self.saw(span.clone(), cx);
        let drawn = self
            .masonry
            .meeting((span.start - screen / 2.)..(span.end + screen / 2.));
        let waiting = self.masonry.placed() < self.rows.len() || !self.paging.exhausted;
        let band = if waiting {
            space::LG + studio_ui::ds::size::TILE_BAND
        } else {
            0.
        };
        let height = 2. * inset + self.masonry.height() + band;
        let tiles: Vec<AnyElement> = drawn
            .into_iter()
            .filter_map(|row| self.render_tile(row, cx))
            .collect();
        let bands = waiting.then(|| self.render_bands(cx));
        div()
            .relative()
            .size_full()
            .child(measure)
            .child(
                div()
                    .id("gallery-columns")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(
                        div()
                            .relative()
                            .w_full()
                            .h(px(height))
                            .children(tiles)
                            .children(bands.into_iter().flatten()),
                    ),
            )
            .into_any_element()
    }

    /// Under each column, while rows wait for their shapes or their page: a quiet band where
    /// the next tiles will go, never a tile's placeholder. The shortest column's says why, and
    /// offers Retry when the next page failed.
    fn render_bands(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let bottoms = self.masonry.bottoms().to_vec();
        let shortest = (0..bottoms.len())
            .min_by(|a, b| bottoms[*a].total_cmp(&bottoms[*b]))
            .unwrap_or(0);
        let radius = studio_ui::ds::radii(cx).md;
        bottoms
            .iter()
            .enumerate()
            .map(|(column, bottom)| {
                let says: Option<AnyElement> =
                    (column == shortest).then(|| match &self.more_error {
                        Some(error) => h_flex()
                            .gap(px(space::SM))
                            .child(
                                div()
                                    .id("gallery-more-error")
                                    .tip(studio_ui::ds::tooltip(format!(
                                        "The next page could not be read: {error}"
                                    )))
                                    .child(
                                        studio_ui::ds::text::meta("Not read").color(Color::Warning),
                                    ),
                            )
                            .child(
                                studio_ui::ds::row_button("gallery-retry-more", "Retry")
                                    .on_click(cx.listener(|this, _, _, cx| this.retry_page(cx))),
                            )
                            .into_any_element(),
                        None => studio_ui::ds::text::meta("Reading…").into_any_element(),
                    });
                div()
                    .absolute()
                    .left(px(self.masonry.left(column, space::EDGE)))
                    .top(px(space::EDGE + bottom))
                    .w(px(self.masonry.width()))
                    .h(px(studio_ui::ds::size::TILE_BAND))
                    .rounded(px(radius))
                    .bg(studio_ui::ds::surface::highlight(cx))
                    .flex()
                    .items_center()
                    .justify_center()
                    .children(says)
                    .into_any_element()
            })
            .collect()
    }

    /// One placed tile: its preview, as tall as its file's shape needs, then the row's name and
    /// what the file is. The cursor's tile is ringed in the focus colour, a picked one checked,
    /// and a branch's change marked as every change is (spec 033).
    fn render_tile(&self, ix: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let place: Place = *self.masonry.place_of(ix)?;
        let row = self.rows.get(ix)?;
        let width = self.masonry.width();
        let radii = studio_ui::ds::radii(cx);
        let property = self.property.clone().unwrap_or_default();
        let tile = self.tiles.get(&(row.id.clone(), property.clone()));
        let cursor = self.cursor == Some(ix);
        let picked = self.picks.contains(&row.id);
        let change = self.change_of(&row.id);
        let removed = change == Some(NetChange::Removed);
        let name = self.row_label(row).unwrap_or_else(|| row.id.clone());
        let (meta, meta_color) = self.meta_of(&row.id, tile, &property);
        // The ring and the fill sit [`space::XS`] outside the preview, inside a ring as wide as
        // the accent line: the preview's corner is concentric within the tile's.
        let outer = radii.md;
        let well_radius = studio_ui::ds::radius::within(outer, tile_inset());
        let inner = width - 2. * tile_inset();
        let preview_height = place.height - tile_chrome();
        let fill = if cursor {
            Some(studio_ui::ds::surface::selected_document(cx))
        } else if picked {
            Some(studio_ui::ds::surface::picked(cx))
        } else {
            None
        };
        let preview = self.render_preview(tile, inner, preview_height, well_radius, cx);
        let kind = change.map(|net| match net {
            NetChange::Added => ChangeKind::Added,
            NetChange::Changed => ChangeKind::Changed,
            NetChange::Removed => ChangeKind::Removed,
        });
        let tip = format!("{name}\n{meta}");
        Some(
            v_flex()
                .id(("gallery-tile", ix))
                .debug_selector(move || format!("gallery-tile-{ix}"))
                .absolute()
                .left(px(self.masonry.left(place.column, space::EDGE)))
                .top(px(space::EDGE + place.top))
                .w(px(width))
                .h(px(place.height))
                .p(px(space::XS))
                .rounded(px(outer))
                .border(px(studio_ui::ds::size::ACCENT_LINE))
                .border_color(if cursor {
                    studio_ui::ds::surface::focus(cx)
                } else {
                    gpui::transparent_black()
                })
                .when_some(fill, |tile, fill| tile.bg(fill))
                .when(removed, |tile| tile.opacity(studio_ui::ds::opacity::DIMMED))
                .cursor_pointer()
                .tip(studio_ui::ds::tooltip(tip))
                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    this.click_tile(ix, event, window, cx)
                }))
                .child(
                    div()
                        .relative()
                        .w(px(inner))
                        .h(px(preview_height))
                        .rounded(px(well_radius))
                        .overflow_hidden()
                        .bg(studio_ui::ds::surface::well(cx))
                        .child(preview)
                        // A bar of the change's colour along the preview's top edge.
                        .when_some(
                            kind.filter(|kind| *kind != ChangeKind::Removed),
                            |well, kind| {
                                well.child(
                                    div()
                                        .absolute()
                                        .top_0()
                                        .left_0()
                                        .w_full()
                                        .h(px(studio_ui::ds::size::ACCENT_LINE))
                                        .bg(kind.color().color(cx)),
                                )
                            },
                        )
                        .when(picked, |well| {
                            well.child(
                                div()
                                    .absolute()
                                    .top(px(space::SM))
                                    .left(px(space::SM))
                                    .child(studio_ui::ds::check_box(
                                        ("gallery-pick", ix),
                                        ui::ToggleState::Selected,
                                        cx,
                                    )),
                            )
                        }),
                )
                .child(
                    v_flex()
                        .h(px(studio_ui::ds::size::TILE_CAPTION))
                        .pt(px(space::SM))
                        .px(px(space::XXS))
                        .gap(px(space::XXS))
                        .when(kind == Some(ChangeKind::Added), |caption| {
                            caption
                                .rounded(px(well_radius))
                                .bg(studio_ui::ds::surface::wash(Color::Created, cx))
                        })
                        .child(
                            h_flex()
                                .min_w_0()
                                .gap(px(space::XS))
                                .when_some(kind, |line, kind| {
                                    line.child(
                                        studio_ui::ds::text::body(kind.symbol())
                                            .color(kind.color()),
                                    )
                                })
                                .child(
                                    div().min_w_0().flex_1().child(
                                        studio_ui::ds::text::body(name)
                                            .single_line()
                                            .truncate()
                                            .when(removed, |label| label.strikethrough()),
                                    ),
                                ),
                        )
                        .child(
                            studio_ui::ds::text::meta(meta)
                                .color(meta_color)
                                .single_line()
                                .truncate(),
                        ),
                )
                .into_any_element(),
        )
    }

    /// A tile's meta line and its colour: what the file is, or why there is none.
    fn meta_of(&self, id: &str, tile: Option<&Tile>, property: &str) -> (String, Color) {
        if self.file_changed(id) {
            return ("File changed".into(), Color::Modified);
        }
        match tile.map(|tile| (&tile.meta, tile.facts.as_ref())) {
            Some((Loadable::Loaded(BlobMeta::Managed { size, .. }), facts)) => {
                (tile::meta_line(facts, *size), Color::Muted)
            }
            Some((Loadable::Loaded(BlobMeta::Null), _)) => {
                (format!("No {property}"), Color::Disabled)
            }
            Some((Loadable::Loaded(BlobMeta::External { uri }), _)) => (
                format!("External · {}", crate::blob::host(uri)),
                Color::Muted,
            ),
            Some((Loadable::Failed(_), _)) => ("Could not read".into(), Color::Warning),
            _ => (String::new(), Color::Muted),
        }
    }

    /// What a tile's preview well shows: its picture filling it edge to edge (a shape held to
    /// the masonry's bounds, or a small picture never drawn past twice its pixels, sits
    /// centered in it), the first lines of a text, or its kind's icon.
    fn render_preview(
        &self,
        tile: Option<&Tile>,
        width: f32,
        height: f32,
        radius: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let centered = || div().size_full().flex().items_center().justify_center();
        let Some(tile) = tile else {
            return centered().into_any_element();
        };
        let icon = |name: IconName, color: Color| {
            centered()
                .child(studio_ui::ds::icon::tile(name).color(color))
                .into_any_element()
        };
        match (&tile.meta, &tile.look) {
            (Loadable::Loaded(BlobMeta::Null), _) => icon(IconName::Dash, Color::Disabled),
            (Loadable::Loaded(BlobMeta::External { .. }), _) => icon(IconName::Link, Color::Muted),
            (_, Loadable::Failed(_)) | (Loadable::Failed(_), _) => {
                icon(IconName::Warning, Color::Warning)
            }
            (_, Loadable::Loaded(Look::Image { image, .. })) => {
                let (w, h) = image_size(image, tile.facts.as_ref());
                let scale = (width / w).min(height / h).min(2.);
                let (w, h) = (w * scale, h * scale);
                // Edge to edge it takes the well's corner; inside it, a picture's square one.
                let fills = width - w < 1. && height - h < 1.;
                let corner = if fills { radius } else { 0. };
                centered()
                    .child(
                        div()
                            .relative()
                            .w(px(w))
                            .h(px(h))
                            .child(
                                gpui::img(image.source())
                                    .size_full()
                                    .rounded(px(corner))
                                    .object_fit(gpui::ObjectFit::Contain),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .left_0()
                                    .size_full()
                                    .rounded(px(corner))
                                    .border(px(studio_ui::ds::size::HAIRLINE))
                                    .border_color(studio_ui::ds::surface::image_outline(cx)),
                            ),
                    )
                    .into_any_element()
            }
            (_, Loadable::Loaded(Look::Text(text))) => {
                studio_ui::ds::preview_text(text.clone(), cx).into_any_element()
            }
            (_, Loadable::Loaded(Look::Empty)) => centered()
                .child(studio_ui::ds::text::meta("Empty"))
                .into_any_element(),
            (_, Loadable::Loaded(Look::Icon)) => {
                let kind = tile.facts.map(|facts| facts.kind);
                icon(kind_icon(kind), Color::Muted)
            }
            _ => match tile.facts {
                Some(facts) => icon(kind_icon(Some(facts.kind)), Color::Disabled),
                None => centered().into_any_element(),
            },
        }
    }
}

/// The size a preview is laid out at, in pixels: an image's own, a drawing's shape at a
/// preview's size, else a square.
fn image_size(image: &crate::blob::ShownImage, facts: Option<&Facts>) -> (f32, f32) {
    match image {
        crate::blob::ShownImage::Raster(render) => {
            let size = render.size(0);
            let (w, h) = (size.width.0 as f32, size.height.0 as f32);
            // A thumbnail stands for the image's own size, which may be larger.
            match facts.and_then(|facts| facts.pixels) {
                Some((pw, ph)) if pw as f32 >= w => (pw as f32, ph as f32),
                _ => (w.max(1.), h.max(1.)),
            }
        }
        // A drawing has no pixels of its own: its shape, as large as a preview is held.
        crate::blob::ShownImage::Svg(_) => {
            let edge = tile::THUMB_EDGE as f32;
            match facts.and_then(|facts| facts.drawing) {
                Some((w, h)) if w > 0. && h > 0. => {
                    let scale = edge / w.max(h);
                    (w * scale, h * scale)
                }
                _ => (edge, edge),
            }
        }
    }
}

/// The icon a file of `kind` stands as when it is not drawn.
fn kind_icon(kind: Option<crate::blob::ValueKind>) -> IconName {
    use crate::blob::ValueKind;
    match kind {
        Some(ValueKind::Image(_)) | Some(ValueKind::Svg) => IconName::Image,
        Some(ValueKind::Pdf) => IconName::FileDoc,
        Some(ValueKind::Text) => IconName::FileTextOutlined,
        Some(ValueKind::Binary) | None => IconName::FileGeneric,
    }
}

/// Bytes `from..to` of a file read at `snapshot`, in as many reads as the server takes to give
/// them (it answers up to 4 MiB a range).
async fn read_bytes(
    session: &WeakEntity<GraphSession>,
    cell: &BlobCell,
    snapshot: &Option<String>,
    scope: Option<&ReadScope>,
    from: u64,
    to: u64,
    cx: &mut gpui::AsyncApp,
) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    while from + (bytes.len() as u64) < to {
        let at = from + bytes.len() as u64;
        let read = session
            .update(cx, |session, cx| match scope {
                Some(scope) => {
                    session.blob_bytes_in(scope, cell.clone(), snapshot.clone(), at..to, cx)
                }
                None => session.blob_bytes(cell.clone(), snapshot.clone(), at..to, cx),
            })
            .map_err(|_| "The graph closed.".to_owned())?;
        let chunk = read.await.map_err(|error| error.to_string())?;
        if chunk.is_empty() {
            break;
        }
        bytes.extend(chunk);
    }
    Ok(bytes)
}

impl Item for TypeGallery {
    fn thing(&self, _cx: &App) -> Option<studio_actions::Thing> {
        Some(studio_actions::Thing::Type(self.type_name.clone()))
    }

    fn view_kind(&self, _cx: &App) -> Option<ViewKind> {
        Some(ViewKind::Gallery)
    }

    fn held_at(&self, _cx: &App) -> Option<String> {
        self.at_commit.clone()
    }

    /// The strip a node table has, Gallery current (spec 020).
    fn views(&self, _cx: &App) -> Vec<ViewLink> {
        let tree = if self.rows.is_empty() {
            ViewLink::disabled_because(ViewKind::Tree, "No rows to show")
        } else if self.picks.is_empty() {
            ViewLink::to(ViewKind::Tree)
                .saying("Show these rows, each opening to its relationships")
        } else {
            ViewLink::to(ViewKind::Tree).saying(format!(
                "Show the {}, each opening to its relationships",
                plural(self.picks.len(), "picked row", "picked rows")
            ))
        };
        let canvas = match self.selected_row_id() {
            Some(id) => {
                let name = self
                    .cursor_row()
                    .and_then(|row| self.row_label(row))
                    .unwrap_or_else(|| id.to_owned());
                ViewLink::to(ViewKind::Canvas).saying(format!("Walk from {name}"))
            }
            None => ViewLink::disabled_because(ViewKind::Canvas, "Select a tile to walk from it"),
        };
        vec![
            ViewLink::to(ViewKind::Table).saying(format!("The {} table", self.type_name)),
            tree,
            canvas,
            ViewLink::current(ViewKind::Gallery),
        ]
    }

    fn view_action(&self, kind: ViewKind, _cx: &App) -> Option<Box<dyn gpui::Action>> {
        match kind {
            ViewKind::Table => Some(Box::new(OpenType {
                type_name: self.type_name.clone(),
                at_commit: self.at_commit.clone(),
                filter_id: None,
                via: None,
                preview: true,
                filters: self.filters_value(),
                separate: false,
            })),
            ViewKind::Tree => Some(Box::new(ShowSetInTree {
                title: self.title.clone(),
                type_name: Some(self.type_name.clone()),
                filters: self.filters_value(),
                nodes: self.picked_nodes().or_else(|| {
                    self.ids.as_ref().map(|ids| {
                        ids.iter()
                            .map(|id| ResultNode {
                                type_name: self.type_name.clone(),
                                id: id.clone(),
                                label: None,
                            })
                            .collect()
                    })
                }),
                total: match &self.count {
                    Loadable::Loaded(n) if self.picks.is_empty() => Some((*n).max(0) as u64),
                    _ => None,
                },
                at_commit: self.at_commit.clone(),
                select: self.selected_row_id().map(str::to_owned),
                preview: true,
            })),
            ViewKind::Canvas => Some(Box::new(ShowNodeOnCanvas {
                type_name: self.type_name.clone(),
                id: self.selected_row_id()?.to_owned(),
                at_commit: self.at_commit.clone(),
                preview: true,
            })),
            _ => None,
        }
    }

    fn selected_ids(&self, _cx: &App) -> Option<studio_workspace::SelectedIds> {
        let ids: Vec<String> = if self.picks.is_empty() {
            self.selected_row_id()
                .map(str::to_owned)
                .into_iter()
                .collect()
        } else {
            self.picks.iter().cloned().collect()
        };
        (!ids.is_empty()).then(|| studio_workspace::SelectedIds {
            type_name: Some(self.type_name.clone()),
            ids,
            root: None,
        })
    }

    /// The tile under the cursor's row, with the values the gallery read of it, as the table's
    /// peek shows a row.
    fn inspected(&mut self, cx: &mut Context<Self>) -> Option<Inspected> {
        let Some(id) = self.selected_row_id().map(str::to_owned) else {
            self.peek = None;
            return None;
        };
        if let Some((shown, peek)) = &self.peek
            && *shown == id
        {
            return Some(Inspected::View {
                view: peek.clone().into(),
                scrolls_itself: true,
            });
        }
        let values = self.cursor_row()?.values.clone();
        let (session, type_name, snapshot) = (
            self.session.clone(),
            self.type_name.clone(),
            self.paging.pinned.clone(),
        );
        let held = self.at_commit.is_some();
        let peek = cx.new(|cx| {
            let mut peek = NodePeek::new(
                session,
                PeekSubject::Node {
                    type_name,
                    id: id.clone(),
                },
                Some(values),
                snapshot,
                HashMap::new(),
                true,
                cx,
            );
            if held {
                peek.hold_commit();
            }
            peek
        });
        self.peek = Some((id, peek.clone()));
        Some(Inspected::View {
            view: peek.into(),
            scrolls_itself: true,
        })
    }

    fn caches_its_parts(&self) -> bool {
        true
    }

    fn tab_title(&self, _cx: &App) -> SharedString {
        let title = format!("Gallery: {}", self.title);
        match &self.at_commit {
            Some(commit) => format!("{title} @ {}", studio_ui::short_commit_id(commit)).into(),
            None => title.into(),
        }
    }

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(IconName::Image)
    }

    fn dedupe_key(&self, _cx: &App) -> Option<SharedString> {
        Some(crate::gallery_key(&self.opening()).into())
    }

    fn serialize(&self, _cx: &App) -> Option<SavedItem> {
        let mut data = serde_json::to_value(self.opening()).ok()?;
        if let Some(map) = data.as_object_mut() {
            map.insert(
                "size".into(),
                serde_json::to_value(self.size).unwrap_or(Value::Null),
            );
        }
        Some(SavedItem {
            kind: "gallery".into(),
            data,
        })
    }

    fn is_stale(&self, _cx: &App) -> bool {
        self.newer
    }

    fn status(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> Option<ItemStatus> {
        let mut start = Vec::new();
        if self.loading_more || !self.tile_tasks.is_empty() {
            start.push(studio_ui::ds::spinner().into_any_element());
        }
        if !self.picks.is_empty() {
            start.push(
                studio_ui::ds::text::meta(plural(self.picks.len(), "picked", "picked"))
                    .into_any_element(),
            );
        }
        let focus = self.focus_handle.clone();
        let key =
            |label: &str, action: &dyn gpui::Action| KeyHint::new(label.to_owned(), action, &focus);
        let keys = vec![
            key("Move", &MoveRight),
            key("Peek", &ToggleInspector),
            key("Open the file", &OpenFile),
            key("Open the row", &OpenRowPage),
            key("Pick", &TogglePick),
            key("Pick all", &PickAll),
            key("Copy ids", &CopyIds),
            key("Next file", &ShowNextFile),
            key("Larger tiles", &LargerTiles),
            key("Smaller tiles", &SmallerTiles),
            key("Refresh", &Refresh),
        ];
        Some(ItemStatus {
            start,
            end: Vec::new(),
            keys,
        })
    }
}

impl Render for TypeGallery {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        let mut context = KeyContext::new_with_defaults(cx.platform_info().os);
        context.add("Gallery");
        let grid = self.grid.clone();
        v_flex()
            .key_context(context)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(studio_ui::ds::surface::document(cx))
            .on_action(cx.listener(|this, _: &MoveLeft, _, cx| this.move_cursor(-1, true, cx)))
            .on_action(cx.listener(|this, _: &MoveRight, _, cx| this.move_cursor(1, true, cx)))
            .on_action(cx.listener(|this, _: &MoveUp, _, cx| this.move_cursor(-1, false, cx)))
            .on_action(cx.listener(|this, _: &MoveDown, _, cx| this.move_cursor(1, false, cx)))
            .on_action(cx.listener(|this, _: &MoveToFirst, _, cx| this.set_cursor(0, cx)))
            .on_action(cx.listener(|this, _: &MoveToLast, _, cx| this.move_to_end(cx)))
            .on_action(cx.listener(|this, _: &OpenFile, window, cx| this.open_file(window, cx)))
            .on_action(
                cx.listener(|this, _: &OpenRowPage, window, cx| this.open_row_page(window, cx)),
            )
            .on_action(cx.listener(|this, _: &TogglePick, _, cx| {
                if let Some(cursor) = this.cursor {
                    this.pick(cursor, false, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &PickAll, _, cx| this.pick_all(cx)))
            .on_action(cx.listener(|this, _: &ClearPicks, _, cx| this.clear_picks(cx)))
            .on_action(cx.listener(|this, _: &CopyIds, _, cx| this.copy_ids(cx)))
            .when(self.files.len() > 1, |gallery| {
                gallery
                    .on_action(cx.listener(|this, _: &ShowNextFile, _, cx| this.show_next_file(cx)))
            })
            .on_action(cx.listener(|this, _: &LargerTiles, _, cx| {
                let size = this.size.larger();
                this.set_size(size, cx)
            }))
            .on_action(cx.listener(|this, _: &SmallerTiles, _, cx| {
                let size = this.size.smaller();
                this.set_size(size, cx)
            }))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.refresh(cx)))
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(AnyView::from(grid).cached(StyleRefinement::default().size_full())),
            )
    }
}

/// The gallery's tiles, drawn as a view of their own and cached (`docs/drawing.md`): the
/// toolbar's menus and the status bar draw without them.
pub(crate) struct GalleryGrid {
    gallery: WeakEntity<TypeGallery>,
    _gallery_changed: Subscription,
    #[cfg(test)]
    pub(crate) renders: usize,
}

impl GalleryGrid {
    /// Made with the gallery, not on its first draw, so GPUI counts it among what the gallery's
    /// draw depends on (`docs/drawing.md`).
    fn new(cx: &mut Context<TypeGallery>) -> Entity<Self> {
        let gallery = cx.entity();
        cx.new(|cx| GalleryGrid {
            gallery: gallery.downgrade(),
            _gallery_changed: cx.observe(&gallery, |_, _, cx| cx.notify()),
            #[cfg(test)]
            renders: 0,
        })
    }
}

impl Render for GalleryGrid {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        self.gallery
            .update(cx, |gallery, cx| gallery.render_body(window, cx))
            .unwrap_or_else(|_| gpui::Empty.into_any_element())
    }
}

#[cfg(test)]
mod tests;

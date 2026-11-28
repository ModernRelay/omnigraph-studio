//! A type's nodes as cards (spec 048): a grid of columns of one width and cards of one size,
//! each its cover (one of the node's files, fitted inside a box of one shape) and its title,
//! placed in row order and never moved, one tab per set, read a page at a time at one commit.
//! Each card's cover is read on its own through the Blob route once scrolling pauses, at most
//! four at once, only for the cards a frame lays out, and drawn from a preview decoded and
//! scaled off the main thread; the previews held are capped. It grew out of spec 035's gallery.

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
use studio_actions::cards::{
    Cancel, CopyIds, LargerCards, MoveDown, MoveLeft, MoveRight, MoveToFirst, MoveToLast, MoveUp,
    OpenCover, OpenPage, PickAll, Refresh, ShowNextCover, ShowOnCanvas, SmallerCards, TogglePick,
};
use studio_actions::graph::{
    OpenBlob, OpenNodePage, OpenType, ResultNode, ShowNodeOnCanvas, ShowSetInCards, ShowSetInTree,
};
use studio_actions::workspace::ToggleInspector;
use studio_actions::{ViewKind, ViewLink};
use studio_ui::ds::menu::ContextMenu;
use studio_ui::ds::{Tipped, space};
use studio_ui::{ChangeKind, plural};
use studio_workspace::{Inspected, Item, ItemEvent, ItemStatus, KeyHint, SavedItem, Workspace};
use ui::PopoverMenu;
use ui::prelude::*;

use crate::branch_diff::{BranchDiff, TARGET};
use crate::card_grid::{COVER_RATIO, CardGrid, Step};
use crate::model::{Paging, Row, field_rows, node_rows};
use crate::peek::{NodePeek, PeekSubject};
use crate::tile::{self, FIRST_READ, Facts, Look};

/// How long scrolling rests before the covers of the cards laid out are read, as the table's
/// Blob cells wait.
const READ_AFTER: Duration = Duration::from_millis(120);
/// How many covers are read at once: half the connection's slots
/// (`graph_client::READ_CONCURRENCY`), so pages, counts and the inspector never wait behind them.
pub(crate) const READS_AT_ONCE: usize = 4;
/// The most the previews held may take; past it the cards farthest from the lines drawn let
/// theirs go, and read it again if they come back.
pub(crate) const PREVIEWS_HELD: usize = 64 * 1024 * 1024;
/// How long after a cover's read lands the grid draws it, taking every other landing meanwhile
/// in the same draw (the command palette's landing delay): covers land in bursts as a screen is
/// read, and one draw each would draw the grid dozens of times.
pub(crate) const LANDING_DELAY: Duration = Duration::from_millis(30);

/// How wide a Cards tab's columns are drawn at least; the columns grow from it to fill the
/// width.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CardSize {
    Small,
    #[default]
    Medium,
    Large,
}

impl CardSize {
    const ALL: [CardSize; 3] = [CardSize::Small, CardSize::Medium, CardSize::Large];

    pub(crate) fn edge(self) -> f32 {
        match self {
            CardSize::Small => studio_ui::ds::size::CARD_SMALL,
            CardSize::Medium => studio_ui::ds::size::CARD,
            CardSize::Large => studio_ui::ds::size::CARD_LARGE,
        }
    }

    fn label(self) -> &'static str {
        match self {
            CardSize::Small => "S",
            CardSize::Medium => "M",
            CardSize::Large => "L",
        }
    }

    fn larger(self) -> Self {
        match self {
            CardSize::Small => CardSize::Medium,
            _ => CardSize::Large,
        }
    }

    fn smaller(self) -> Self {
        match self {
            CardSize::Large => CardSize::Medium,
            _ => CardSize::Small,
        }
    }
}

/// The grid for a tab `width` wide and cards of at least `edge`: the tab's edges keep
/// [`space::EDGE`], and columns and lines are [`space::LG`] apart.
pub(crate) fn grid_for(width: f32, edge: f32) -> CardGrid {
    CardGrid::fit(width - 2. * space::EDGE, edge, space::LG, card_height)
}

/// How far a card's cover sits inside its edge: the cursor's ring and [`space::XS`].
fn card_inset() -> f32 {
    studio_ui::ds::size::ACCENT_LINE + space::XS
}

/// How tall the cover of a card `width` wide is: a box 4 wide by 3 high across the card, on
/// whole pixels.
fn cover_height(width: f32) -> f32 {
    ((width - 2. * card_inset()) * COVER_RATIO).round()
}

/// How tall a card `width` wide is, whatever it holds: its cover and its title line, inside
/// the ring.
pub(crate) fn card_height(width: f32) -> f32 {
    2. * card_inset() + cover_height(width) + studio_ui::ds::size::CARD_TITLE
}

/// What is known of one card's cover.
#[derive(Debug)]
struct Cover {
    meta: Loadable<BlobMeta>,
    /// What its first bytes said.
    facts: Option<Facts>,
    look: Loadable<Look>,
}

impl Default for Cover {
    fn default() -> Self {
        Self {
            meta: Loadable::NotLoaded,
            facts: None,
            look: Loadable::NotLoaded,
        }
    }
}

impl Cover {
    /// What it holds against [`PREVIEWS_HELD`].
    fn held(&self) -> usize {
        match &self.look {
            Loadable::Loaded(look) => look.held(),
            _ => 0,
        }
    }
}

/// A cover: a row, by id, and the property shown of it.
type CoverKey = (String, String);

pub struct TypeCards {
    workspace: WeakEntity<Workspace>,
    session: Entity<GraphSession>,
    type_name: String,
    /// The set in words, as the tab names it.
    title: String,
    branch: String,
    /// The commit the cards are held at, read-only; `None` follows the branch.
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
    /// The cursor's card is to be scrolled to once it is placed and the grid measured.
    scroll_owed: bool,
    /// The Blob property the cards show as their cover, and the type's Blob properties.
    cover: Option<String>,
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
    size: CardSize,
    covers: HashMap<CoverKey, Cover>,
    cover_tasks: HashMap<CoverKey, Task<()>>,
    cover_queue: VecDeque<CoverKey>,
    /// What the previews held take, against [`PREVIEWS_HELD`].
    held: usize,
    /// The span of the grid the view showed last, top to bottom from the first line's top
    /// (empty until it is drawn), and the one whose covers were asked for, with the width it
    /// was laid out at: at another width other cards meet the same span.
    span: Range<f32>,
    span_read: Option<(i32, i32, i32)>,
    _read_wait: Option<Task<()>>,
    /// The branch's diff is being read: nothing is placed until it is in, since the rows the
    /// branch removed go first.
    diff_pending: bool,
    /// A draw owed for covers that landed, [`LANDING_DELAY`] after the first of them.
    _landing: Option<Task<()>>,
    /// The grid's width as last laid out; 0 until it is.
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
    grid: Entity<CardsGrid>,
    scroll: ScrollHandle,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
    #[cfg(test)]
    pub(crate) renders: usize,
    /// How many cards the last frame laid out.
    #[cfg(test)]
    pub(crate) laid_out_last: usize,
}

impl EventEmitter<ItemEvent> for TypeCards {}

impl Focusable for TypeCards {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TypeCards {
    pub(crate) fn new(
        workspace: WeakEntity<Workspace>,
        session: Entity<GraphSession>,
        opening: ShowSetInCards,
        size: CardSize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let grid = CardsGrid::new(cx);
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
                session.read_scope(&branch, graph_store::ledger::ReadView::Cards)
            })
        });
        let head_watch = opening.at_commit.is_none().then(|| {
            let branch = branch.clone();
            session.update(cx, |session, cx| session.watch_head(&branch, cx))
        });
        let subscriptions = vec![
            cx.subscribe_in(&session, window, Self::on_session_event),
            // The inspector shows the card under the cursor (spec 020).
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
            cover: opening.cover,
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
            covers: HashMap::new(),
            cover_tasks: HashMap::new(),
            cover_queue: VecDeque::new(),
            held: 0,
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
            #[cfg(test)]
            laid_out_last: 0,
        };
        this.read_from_start(cx);
        this
    }

    /// What reopens these cards, as an action: the set they hold, where it is read, and the
    /// file that is their cover.
    pub(crate) fn opening(&self) -> ShowSetInCards {
        ShowSetInCards {
            title: self.title.clone(),
            type_name: self.type_name.clone(),
            filters: (!self.filters.is_empty())
                .then(|| serde_json::to_value(&self.filters).ok())
                .flatten(),
            sort: self.sort.as_ref().map(|sort| {
                serde_json::json!({"property": sort.property, "descending": sort.descending})
            }),
            ids: self.ids.clone(),
            cover: self.cover.clone(),
            at_commit: self.at_commit.clone(),
            snapshot: None,
            select: None,
            preview: false,
        }
    }

    // ---- reading ----

    /// Reads the set from its first page: at the held commit, at the commit the tab was
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
            .cover
            .as_ref()
            .is_none_or(|property| !self.files.contains(property))
        {
            self.cover = self.files.first().cloned();
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

    /// The next page's query, and for listed rows how far into the list it reads; the list
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
            self.put_removed_first();
            cx.emit(ItemEvent::UpdateTab);
        }
        self.count_listed();
        self.find_cursor_row();
        // A page that showed no row and is not the last reads on at once: sorted, the rows with
        // a value ran out and those without one are next; listed, its rows are gone. The cards
        // drawn ask for nothing while there are none.
        if !added && !self.paging.exhausted {
            let pin = self.paging.pinned.clone();
            self.read_page(pin, false, cx);
        } else {
            self.want_more(cx);
        }
        self.read_covers(cx);
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
    /// are read on to find it. Once the set is read whole without it, the first card.
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

    /// Reads the next page once the cards laid out come within a page of the last row read.
    fn want_more(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.first_page, Loadable::Loaded(())) || self.more_error.is_some() {
            return;
        }
        let wanted = self.laid_out().end;
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
                // Unread, the cards are placed unmarked, as a table shows its rows unmarked.
                this.update(cx, |this, cx| {
                    if this.generation == generation {
                        this.diff_pending = false;
                        this.find_cursor_row();
                        this.want_more(cx);
                        this.read_covers(cx);
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
                this.put_removed_first();
                this.count_listed();
                this.find_cursor_row();
                this.want_more(cx);
                this.read_covers(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Puts the rows the branch removed first, once the first page and the diff are both in:
    /// before any card is placed ([`Self::diff_pending`]), so none moves for them.
    fn put_removed_first(&mut self) {
        let (Some(diff), Loadable::Loaded(())) = (self.diff.clone(), &self.first_page) else {
            return;
        };
        let shown: HashSet<&str> = self.rows.iter().map(|row| row.id.as_str()).collect();
        // Cards of listed rows show the listed ones the branch removed, in their order.
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
        // The cursor stays on its row, which moves on by the rows put before it.
        self.cursor = self.cursor.map(|cursor| cursor + moved);
        self.pick_anchor = self.pick_anchor.map(|anchor| anchor + moved);
    }

    fn change_of(&self, id: &str) -> Option<NetChange> {
        self.diff.as_ref()?.mark(id).map(|mark| mark.net)
    }

    fn is_removed(&self, id: &str) -> bool {
        self.change_of(id) == Some(NetChange::Removed)
    }

    /// Whether the branch changed the file the cards show as their cover in row `id`.
    fn file_changed(&self, id: &str) -> bool {
        let Some(property) = &self.cover else {
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
            // The window moved to another branch: cards that follow their branch read it.
            SessionEvent::BranchChanged if self.at_commit.is_none() => {
                self.branch = self.session.read(cx).branch.clone();
                let branch = self.branch.clone();
                self.scope = Some(self.session.update(cx, |session, _| {
                    session.read_scope(&branch, graph_store::ledger::ReadView::Cards)
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

    // ---- cards and their covers ----

    /// The grid for the width last laid out and the card size: where every card is, from its
    /// index alone.
    fn card_grid(&self) -> CardGrid {
        let width = self.width.get();
        let width = if width > 0. {
            width
        } else {
            studio_ui::ds::size::WINDOW_WIDTH - studio_ui::ds::size::SIDEBAR_WIDTH
        };
        grid_for(width, self.size.edge())
    }

    /// How many rows are placed as cards: every row read, once the first page is in, and on a
    /// branch its diff with it, since the rows it removed go first.
    fn placed(&self) -> usize {
        if self.diff_pending || !matches!(self.first_page, Loadable::Loaded(())) {
            0
        } else {
            self.rows.len()
        }
    }

    /// The cards a frame lays out: those of the lines that meet the view and half a screen
    /// either side, so a scroll shows them drawn. None before the view is drawn.
    fn laid_out(&self) -> Range<usize> {
        let screen = self.span.end - self.span.start;
        if screen <= 0. {
            return 0..0;
        }
        self.card_grid().meeting(
            (self.span.start - screen / 2.)..(self.span.end + screen / 2.),
            self.placed(),
        )
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

    /// The view showed `span` of the grid: the next page is read when the cards laid out near
    /// the end of the rows read, and their covers once scrolling rests.
    fn saw(&mut self, span: Range<f32>, cx: &mut Context<Self>) {
        self.span = span.clone();
        self.want_more(cx);
        let key = (
            span.start.round() as i32,
            span.end.round() as i32,
            self.width.get().round() as i32,
        );
        if self.span_read == Some(key) || self.cover.is_none() {
            return;
        }
        self.span_read = Some(key);
        self._read_wait = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(READ_AFTER).await;
            this.update(cx, |this, cx| this.read_covers(cx)).ok();
        }));
    }

    /// The covers the view wants read: those of the cards it shows, in row order, then those
    /// of the cards laid out below it and above it. None while there is no cover, or before the
    /// view is drawn.
    fn wanted(&self) -> Vec<CoverKey> {
        let Some(property) = &self.cover else {
            return Vec::new();
        };
        let near = self.laid_out();
        if near.is_empty() {
            return Vec::new();
        }
        // What is laid out holds what is shown: its span is the view's and half a screen more.
        let shown = self.card_grid().meeting(self.span.clone(), self.placed());
        let shown = shown.start.clamp(near.start, near.end)..shown.end.clamp(near.start, near.end);
        shown
            .clone()
            .chain(shown.end..near.end)
            .chain(near.start..shown.start)
            .map(|row| (self.rows[row].id.clone(), property.clone()))
            .collect()
    }

    /// Whether a cover is still to be read.
    fn needs(&self, key: &CoverKey) -> bool {
        match self.covers.get(key) {
            None => true,
            Some(cover) if matches!(cover.meta, Loadable::Failed(_)) => false,
            Some(cover) => !matches!(cover.look, Loadable::Loaded(_) | Loadable::Failed(_)),
        }
    }

    /// Reads the covers the view wants, those shown first. A read out for a card no longer laid
    /// out is dropped, which stops its request, and asked again when the card comes back.
    pub(crate) fn read_covers(&mut self, cx: &mut Context<Self>) {
        let wanted: Vec<CoverKey> = self
            .wanted()
            .into_iter()
            .filter(|key| self.needs(key))
            .collect();
        let wants: HashSet<&CoverKey> = wanted.iter().collect();
        let gone: Vec<CoverKey> = self
            .cover_tasks
            .keys()
            .filter(|key| !wants.contains(key))
            .cloned()
            .collect();
        for key in gone {
            self.cover_tasks.remove(&key);
            if let Some(cover) = self.covers.get_mut(&key)
                && matches!(cover.look, Loadable::Loading)
            {
                cover.look = Loadable::NotLoaded;
            }
        }
        self.cover_queue = wanted.into_iter().collect();
        self.read_queued(cx);
    }

    fn read_queued(&mut self, cx: &mut Context<Self>) {
        while self.cover_tasks.len() < READS_AT_ONCE {
            let Some(key) = self.cover_queue.pop_front() else {
                break;
            };
            if self.cover_tasks.contains_key(&key) || !self.needs(&key) {
                continue;
            }
            self.covers.entry(key.clone()).or_default().look = Loadable::Loading;
            let task = self.read_cover(key.clone(), cx);
            self.cover_tasks.insert(key, task);
        }
    }

    /// One cover's read: what the cell holds; its first bytes, which say what the file is; the
    /// rest of a file drawn whole; then its preview, made off the main thread.
    fn read_cover(&mut self, key: CoverKey, cx: &mut Context<Self>) -> Task<()> {
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
        let known = self
            .covers
            .get(&key)
            .and_then(|cover| cover.meta.loaded().cloned());
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
                    let cover = this.covers.entry(key.clone()).or_default();
                    match meta {
                        Ok(meta) => {
                            if size.is_none() {
                                // No file, an empty one or a reference: said at once.
                                cover.look = Loadable::Loaded(match meta {
                                    BlobMeta::Managed { .. } => Look::Empty,
                                    _ => Look::Icon,
                                });
                            }
                            cover.meta = Loadable::Loaded(meta);
                        }
                        Err(error) => {
                            cover.meta = Loadable::Failed(error.clone());
                            cover.look = Loadable::Failed(error);
                        }
                    }
                    if size.is_none() {
                        this.cover_landed(&key, cx);
                    }
                    true
                })
                .unwrap_or(false);
            let (Some(size), true) = (size, go_on) else {
                return;
            };
            let to = size.min(FIRST_READ);
            let first =
                match read_bytes(&session, &cell, &snapshot, scope.as_ref(), 0, to, cx).await {
                    Ok(first) => first,
                    Err(error) => {
                        this.update(cx, |this, cx| {
                            if this.generation != generation {
                                return;
                            }
                            if let Some(cover) = this.covers.get_mut(&key) {
                                cover.look = Loadable::Failed(error);
                            }
                            this.cover_landed(&key, cx);
                        })
                        .ok();
                        return;
                    }
                };
            let facts = Facts::of(&first);
            let whole = first.len() as u64 >= size;
            let stop = this
                .update(cx, |this, _| {
                    if this.generation != generation {
                        return true;
                    }
                    this.covers.entry(key.clone()).or_default().facts = Some(facts);
                    false
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
                if let Some(cover) = this.covers.get_mut(&key) {
                    cover.look = match look {
                        Ok(look) => {
                            this.held += look.held();
                            Loadable::Loaded(look)
                        }
                        Err(error) => Loadable::Failed(error),
                    };
                }
                this.cover_landed(&key, cx);
                this.let_go_past_cap(cx);
            })
            .ok();
        })
    }

    /// A cover's read ended: the next read starts, and the grid draws what landed
    /// [`LANDING_DELAY`] from the first landing not yet drawn, with every other that lands
    /// meanwhile.
    fn cover_landed(&mut self, key: &CoverKey, cx: &mut Context<Self>) {
        self.cover_tasks.remove(key);
        self.read_queued(cx);
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

    /// Frees the previews of the cards farthest from the span shown while they hold more than
    /// [`PREVIEWS_HELD`]; each is read again if its card comes back into view. The cards laid
    /// out keep theirs, so a screen that alone holds more is not read again and again. A
    /// preview of a file that is no longer the cover goes first.
    fn let_go_past_cap(&mut self, cx: &mut App) {
        if self.held <= PREVIEWS_HELD {
            return;
        }
        let shown = self.laid_out();
        let grid = self.card_grid();
        let middle = (self.span.start + self.span.end) / 2.;
        let place: HashMap<&str, usize> = self
            .rows
            .iter()
            .enumerate()
            .map(|(ix, row)| (row.id.as_str(), ix))
            .collect();
        let mut held: Vec<(f32, CoverKey)> = self
            .covers
            .iter()
            .filter(|(_, cover)| cover.held() > 0)
            .filter_map(|(key, _)| {
                if self.cover.as_ref() != Some(&key.1) {
                    return Some((f32::MAX, key.clone()));
                }
                let row = place.get(key.0.as_str()).copied()?;
                if shown.contains(&row) {
                    return None;
                }
                Some(((grid.middle_of(row) - middle).abs(), key.clone()))
            })
            .collect();
        held.sort_by(|a, b| b.0.total_cmp(&a.0));
        for (_, key) in held {
            if self.held <= PREVIEWS_HELD {
                break;
            }
            let Some(cover) = self.covers.get_mut(&key) else {
                continue;
            };
            if let Loadable::Loaded(look) = std::mem::replace(&mut cover.look, Loadable::NotLoaded)
            {
                self.held = self.held.saturating_sub(look.held());
                if let Look::Image { image, .. } = look {
                    image.release(cx);
                }
            }
        }
    }

    /// Frees every preview and drops every read: a read from the start, or the tab closed.
    fn release_all(&mut self, cx: &mut App) {
        for cover in self.covers.values() {
            if let Loadable::Loaded(Look::Image { image, .. }) = &cover.look {
                image.release(cx);
            }
        }
        self.covers.clear();
        self.cover_tasks.clear();
        self.cover_queue.clear();
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

    /// Scrolls the least that shows card `row` whole, once it is placed; whether it scrolled.
    fn scroll_to_card(&self, row: usize) -> bool {
        if row >= self.placed() {
            return false;
        }
        let grid = self.card_grid();
        let height = self.screen_height();
        let top = -f32::from(self.scroll.offset().y);
        // The grid starts [`space::EDGE`] down the scroll area, and ends as far above its end.
        let card_top = grid.place_of(row).1;
        let card_bottom = card_top + grid.card_height() + 2. * space::EDGE;
        let to = if card_top < top {
            card_top
        } else if card_bottom > top + height {
            card_bottom - height
        } else {
            return false;
        };
        self.scroll.set_offset(point(px(0.), px(-to.max(0.))));
        true
    }

    /// Moves the cursor by an arrow: to the card before or after in row order, or a line up or
    /// down ([`CardGrid::step`]). A cursor on a row not placed goes to the first card.
    pub(crate) fn move_cursor(&mut self, step: Step, cx: &mut Context<Self>) {
        let placed = self.placed();
        if placed == 0 {
            return;
        }
        let next = match self.cursor.filter(|&row| row < placed) {
            None => 0,
            Some(row) => self.card_grid().step(row, step, placed),
        };
        self.set_cursor(next, cx);
    }

    /// Puts the cursor on card `ix` and shows it whole: a key that leaves the cursor where it
    /// is (Home on the first card) still brings its card back into view.
    fn set_cursor(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.placed() {
            return;
        }
        let moved = self.cursor != Some(ix);
        self.cursor = Some(ix);
        self.scroll_owed = false;
        if self.scroll_to_card(ix) || moved {
            cx.notify();
        }
    }

    fn move_to_end(&mut self, cx: &mut Context<Self>) {
        if let Some(last) = self.placed().checked_sub(1) {
            self.set_cursor(last, cx);
        }
    }

    /// Picks the card at `ix`, or takes it out; with `range`, picks from the last card picked.
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

    /// Esc: takes every pick out.
    fn cancel(&mut self, cx: &mut Context<Self>) {
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

    /// A click puts the cursor on a card, and a second opens its page, as ↵; with ⌘ it picks
    /// the card, and with ⇧ from the last card picked to it.
    fn click_card(
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
            self.open_page(window, cx);
        }
    }

    /// ↵: the page of the node under the cursor.
    fn open_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected_row_id().map(str::to_owned) else {
            return;
        };
        self.keep_tab(cx);
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

    /// ⌥↵: the cover's file of the card under the cursor, in its Blob tab (spec 004).
    fn open_cover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(row), Some(property)) = (self.cursor_row(), self.cover.clone()) else {
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
                // Held where it was read when the cards hold their commit or the row is gone
                // from the branch; else it writes on the branch, bound to the value read here.
                held: self.at_commit.is_some() || removed,
                at_commit,
            }),
            cx,
        );
    }

    /// G: a canvas walking from the node under the cursor, the one the strip's Canvas opens.
    fn show_on_canvas(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(action) = Item::view_action(self, ViewKind::Canvas, cx) else {
            return;
        };
        self.keep_tab(cx);
        window.dispatch_action(action, cx);
    }

    /// Keeps this tab when it is its pane's preview: a view opened from it must not take its
    /// place (spec 020).
    fn keep_tab(&self, cx: &mut Context<Self>) {
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| workspace.keep_active_tab(cx));
        }
    }

    /// Shows another of the type's files as the cover. Every card keeps its place and its
    /// size; the covers of the cards laid out are read.
    fn show_cover(&mut self, property: String, cx: &mut Context<Self>) {
        if self.cover.as_ref() == Some(&property) {
            return;
        }
        self.cover = Some(property);
        self.span_read = None;
        self.read_covers(cx);
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn show_next_cover(&mut self, cx: &mut Context<Self>) {
        let next = match &self.cover {
            Some(shown) => self
                .files
                .iter()
                .position(|file| file == shown)
                .map(|ix| self.files[(ix + 1) % self.files.len()].clone()),
            None => self.files.first().cloned(),
        };
        if let Some(next) = next {
            self.show_cover(next, cx);
        }
    }

    /// Another card size: the columns flow again, the previews held are kept, and the covers
    /// of the cards that now meet the view are asked for at the next draw.
    fn set_size(&mut self, size: CardSize, cx: &mut Context<Self>) {
        if self.size == size {
            return;
        }
        self.size = size;
        self.span_read = None;
        if let Some(cursor) = self.cursor {
            self.scroll_to_card(cursor);
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
        let focus = self.focus_handle.clone();
        studio_ui::ds::toolbar(cx)
            .children(studio_workspace::view_strip(Item::views(self, cx), cx))
            .child(studio_ui::kind_swatch(
                &self.type_name,
                false,
                self.session.read(cx).type_tints(),
                cx,
            ))
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
                    studio_ui::ds::button("cards-newer", "Newer data · Refresh")
                        .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                        .tip(studio_ui::ds::tooltip_for_action_in(
                            "Read the branch head again",
                            &Refresh,
                            &focus,
                        ))
                        .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                )
            })
            .child(div().flex_1())
            .child(
                studio_ui::ds::toolbar_actions()
                    .gap(px(space::SM))
                    .child(self.render_cover_menu(cx))
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
                .id("cards-diff-counts")
                .tip(studio_ui::ds::tooltip(tooltip))
                .child(studio_ui::change_counts(added, changed, removed))
                .into_any_element(),
        )
    }

    /// The Cover chip: which of the type's files the cards show as their cover.
    fn render_cover_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let cards = cx.entity().downgrade();
        let (files, shown) = (self.files.clone(), self.cover.clone());
        let menu = PopoverMenu::new("cards-cover-menu")
            .anchor(gpui::Anchor::TopRight)
            .trigger(
                studio_ui::ds::chip_button(
                    "cards-cover",
                    IconName::Image,
                    "Cover",
                    self.cover.clone().map(SharedString::from),
                )
                .disabled(self.files.len() < 2)
                .tip(if self.files.len() < 2 {
                    studio_ui::ds::tooltip(format!(
                        "The cover is {}, the type's one file",
                        self.cover.as_deref().unwrap_or("no file")
                    ))
                } else {
                    studio_ui::ds::tooltip_for_action_in(
                        "Which file is the cover",
                        &ShowNextCover,
                        &self.focus_handle,
                    )
                }),
            )
            .menu(move |window, cx| {
                let (cards, files, shown) = (cards.clone(), files.clone(), shown.clone());
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for file in &files {
                        let (cards, chosen) = (cards.clone(), file.clone());
                        menu = menu.toggleable_entry(
                            file.clone(),
                            shown.as_ref() == Some(file),
                            ui::IconPosition::Start,
                            None,
                            move |_, cx| {
                                cards
                                    .update(cx, |cards, cx| cards.show_cover(chosen.clone(), cx))
                                    .ok();
                            },
                        );
                    }
                    menu
                }))
            });
        studio_ui::ds::control_chip("cards-cover-chip", menu)
    }

    /// The card size, as a segmented control; the keys that step it are in its tooltips.
    fn render_sizes(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self.focus_handle.clone();
        studio_ui::ds::segmented_track(cx).children(CardSize::ALL.into_iter().map(|size| {
            let current = size == self.size;
            studio_ui::ds::segment(("cards-size", size as usize), size.label(), current, cx)
                .tip(match size {
                    CardSize::Small => {
                        studio_ui::ds::tooltip_for_action_in("Small cards", &SmallerCards, &focus)
                    }
                    CardSize::Medium => studio_ui::ds::tooltip("Medium cards"),
                    CardSize::Large => {
                        studio_ui::ds::tooltip_for_action_in("Large cards", &LargerCards, &focus)
                    }
                })
                .on_click(cx.listener(move |this, _, _, cx| this.set_size(size, cx)))
        }))
    }

    /// The grid, or the state that stands in for it.
    fn render_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let focus = self.focus_handle.clone();
        #[cfg(test)]
        {
            self.laid_out_last = 0;
        }
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
                    studio_ui::ds::state_action("cards-retry", "Retry", &Refresh, &focus, cx)
                        .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                )
                .into_any_element();
            }
            Loadable::Loaded(()) => {}
        }
        if self.cover.is_none() {
            return studio_ui::ds::empty_state(
                IconName::Image,
                format!("{} has no files now", self.type_name),
            )
            .into_any_element();
        }
        // Pages that showed no row read on: the set is empty only once read whole.
        if self.rows.is_empty() && !self.paging.exhausted {
            if let Some(error) = &self.more_error {
                return studio_ui::ds::error_state(
                    format!("Could not read {}", self.type_name),
                    error.clone(),
                )
                .child(
                    // The page that failed, again at the pin: a read from the start would move
                    // to the branch head and forget the properties the commit lacks.
                    div().debug_selector(|| "cards-retry-page".into()).child(
                        studio_ui::ds::state_action("cards-retry", "Retry", &Refresh, &focus, cx)
                            .on_click(cx.listener(|this, _, _, cx| this.retry_page(cx))),
                    ),
                )
                .into_any_element();
            }
            return studio_ui::ds::loading_state("Reading…").into_any_element();
        }
        // Nothing is placed until the branch's diff is read: the rows it removed go first.
        if self.diff_pending {
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
        let cards = cx.entity_id();
        let measure = gpui::canvas(
            move |bounds, _, cx| {
                let now = f32::from(bounds.size.width);
                if (width.get() - now).abs() > 0.5 {
                    width.set(now);
                    cx.defer(move |cx| cx.notify(cards));
                }
            },
            |_, (), _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .w_full()
        .h(px(0.));
        let inset = space::EDGE;
        let grid = self.card_grid();
        let placed = self.placed();
        // The card the tab opened at, once the grid is measured and the view has a height.
        if self.scroll_owed
            && self.width.get() > 0.
            && let Some(cursor) = self.cursor.filter(|&row| row < placed)
        {
            self.scroll_owed = false;
            self.scroll_to_card(cursor);
        }
        let screen = {
            let height = f32::from(self.scroll.bounds().size.height);
            if height > 0. {
                height
            } else {
                f32::from(window.viewport_size().height)
            }
        };
        let height = 2. * inset + grid.height(placed);
        // Where the scroll will be held: GPUI holds the offset within the range only as it lays
        // the frame out, so cards that now take fewer lines (a wider window, a smaller size)
        // would be chosen from an offset past their end, and none drawn.
        let top = f32::from(studio_ui::scrolled_within(
            -self.scroll.offset().y,
            px(height),
            px(screen),
        ));
        // The span of the grid in view; the cards of the lines half a screen either side are
        // laid out too, so a scroll shows them drawn. A frame lays out those and no others,
        // whatever is read.
        self.saw((top - inset)..(top - inset + screen), cx);
        let drawn = self.laid_out();
        #[cfg(test)]
        {
            self.laid_out_last = drawn.len();
        }
        let cards: Vec<AnyElement> = drawn
            .filter_map(|row| self.render_card(row, &grid, cx))
            .collect();
        div()
            .relative()
            .size_full()
            .child(measure)
            .child(
                div()
                    .id("cards-grid")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(div().relative().w_full().h(px(height)).children(cards)),
            )
            .into_any_element()
    }

    /// One card: its cover, a box of one shape across it, then the node's name on one line.
    /// The cursor's card is ringed in the focus colour, a picked one filled and checked, and a
    /// branch's change marked as every change is (spec 033). One element id and one click
    /// handler a card.
    fn render_card(
        &self,
        ix: usize,
        grid: &CardGrid,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let row = self.rows.get(ix)?;
        let (left, top) = grid.place_of(ix);
        let width = grid.width();
        let radii = studio_ui::ds::radii(cx);
        let property = self.cover.clone().unwrap_or_default();
        let cover = self.covers.get(&(row.id.clone(), property.clone()));
        let cursor = self.cursor == Some(ix);
        let picked = self.picks.contains(&row.id);
        let change = self.change_of(&row.id);
        let removed = change == Some(NetChange::Removed);
        let name = self.row_label(row).unwrap_or_else(|| row.id.clone());
        // The ring and the fill sit [`space::XS`] outside the cover, inside a ring as wide as
        // the accent line: the cover's corner is concentric within the card's.
        let outer = radii.md;
        let well_radius = studio_ui::ds::radius::within(outer, card_inset());
        let inner = width - 2. * card_inset();
        let cover_height = cover_height(width);
        let fill = if cursor {
            Some(studio_ui::ds::surface::selected_document(cx))
        } else if picked {
            Some(studio_ui::ds::surface::picked(cx))
        } else {
            None
        };
        let preview = self.render_cover(cover, inner, cover_height, well_radius, cx);
        let kind = change.map(|net| match net {
            NetChange::Added => ChangeKind::Added,
            NetChange::Changed => ChangeKind::Changed,
            NetChange::Removed => ChangeKind::Removed,
        });
        let tip = match self.cover_line(&row.id, cover, &property) {
            Some(line) => studio_ui::ds::tooltip(name.clone()).meta(line),
            None => studio_ui::ds::tooltip(name.clone()),
        };
        let title = studio_ui::cell_text(name, cx).weight(studio_ui::ds::text::weight::HEADING);
        Some(
            v_flex()
                .id(("card", ix))
                .debug_selector(move || format!("card-{ix}"))
                .absolute()
                .left(px(space::EDGE + left))
                .top(px(space::EDGE + top))
                .w(px(width))
                .h(px(grid.card_height()))
                .p(px(space::XS))
                .rounded(px(outer))
                .border(px(studio_ui::ds::size::ACCENT_LINE))
                .border_color(if cursor {
                    studio_ui::ds::surface::focus(cx)
                } else {
                    gpui::transparent_black()
                })
                .when_some(fill, |card, fill| card.bg(fill))
                .when(removed, |card| card.opacity(studio_ui::ds::opacity::DIMMED))
                .cursor_pointer()
                .tip(tip)
                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    this.click_card(ix, event, window, cx)
                }))
                .child(
                    div()
                        .relative()
                        .flex_none()
                        .w(px(inner))
                        .h(px(cover_height))
                        .rounded(px(well_radius))
                        .overflow_hidden()
                        .bg(studio_ui::ds::surface::well(cx))
                        .child(preview)
                        // A bar of the change's colour along the cover's top edge.
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
                                    .child(studio_ui::ds::picked_mark(cx)),
                            )
                        }),
                )
                .child(
                    h_flex()
                        .flex_none()
                        .h(px(studio_ui::ds::size::CARD_TITLE))
                        .px(px(space::XS))
                        .gap(px(space::XS))
                        .items_center()
                        .when(kind == Some(ChangeKind::Added), |line| {
                            line.rounded(px(well_radius))
                                .bg(studio_ui::ds::surface::wash(Color::Created, cx))
                        })
                        .when_some(kind, |line, kind| {
                            line.child(studio_ui::ds::text::body(kind.symbol()).color(kind.color()))
                        })
                        .child(if removed { title.struck() } else { title }),
                )
                .into_any_element(),
        )
    }

    /// What a card's tooltip says of its cover, under the node's name: what the file is, why
    /// there is none, or that the branch changed it. Nothing before it is read.
    fn cover_line(&self, id: &str, cover: Option<&Cover>, property: &str) -> Option<String> {
        if self.file_changed(id) {
            return Some(format!("{property} changed on {}", self.branch));
        }
        match cover.map(|cover| (&cover.meta, cover.facts.as_ref())) {
            Some((Loadable::Loaded(BlobMeta::Managed { size, .. }), facts)) => {
                Some(tile::meta_line(facts, *size))
            }
            Some((Loadable::Loaded(BlobMeta::Null), _)) => Some(format!("No {property}")),
            Some((Loadable::Loaded(BlobMeta::External { uri }), _)) => {
                Some(format!("External · {}", crate::blob::host(uri)))
            }
            Some((Loadable::Failed(reason), _)) => {
                Some(format!("Could not read {property}: {reason}"))
            }
            _ => None,
        }
    }

    /// What a card's cover shows, in its box: a picture fitted inside it and centered, never
    /// drawn past twice its pixels (one that fills the box takes its corner), the first lines
    /// of a text, or its kind's icon.
    fn render_cover(
        &self,
        cover: Option<&Cover>,
        width: f32,
        height: f32,
        radius: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let centered = || div().size_full().flex().items_center().justify_center();
        let Some(cover) = cover else {
            return centered().into_any_element();
        };
        let icon = |name: IconName, color: Color| {
            centered()
                .child(studio_ui::ds::icon::cover(name).color(color))
                .into_any_element()
        };
        match (&cover.meta, &cover.look) {
            (Loadable::Loaded(BlobMeta::Null), _) => icon(IconName::Dash, Color::Disabled),
            (Loadable::Loaded(BlobMeta::External { .. }), _) => icon(IconName::Link, Color::Muted),
            (_, Loadable::Failed(_)) | (Loadable::Failed(_), _) => {
                icon(IconName::Warning, Color::Warning)
            }
            (_, Loadable::Loaded(Look::Image { image, .. })) => {
                let (w, h) = image_size(image, cover.facts.as_ref());
                let scale = (width / w).min(height / h).min(2.);
                let (w, h) = (w * scale, h * scale);
                // Edge to edge it takes the box's corner; inside it, a picture's square one.
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
                let kind = cover.facts.map(|facts| facts.kind);
                icon(kind_icon(kind), Color::Muted)
            }
            _ => match cover.facts {
                Some(facts) => icon(kind_icon(Some(facts.kind)), Color::Disabled),
                None => centered().into_any_element(),
            },
        }
    }
}

/// The size a preview is fitted from, in pixels: an image's own, a drawing's shape at a
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

impl Item for TypeCards {
    fn thing(&self, _cx: &App) -> Option<studio_actions::Thing> {
        Some(studio_actions::Thing::Type(self.type_name.clone()))
    }

    fn view_kind(&self, _cx: &App) -> Option<ViewKind> {
        Some(ViewKind::Cards)
    }

    fn held_at(&self, _cx: &App) -> Option<String> {
        self.at_commit.clone()
    }

    /// The strip a node table has, Cards current (spec 020).
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
            None => ViewLink::disabled_because(ViewKind::Canvas, "Select a card to walk from it"),
        };
        vec![
            ViewLink::to(ViewKind::Table).saying(format!("The {} table", self.type_name)),
            tree,
            canvas,
            ViewLink::current(ViewKind::Cards),
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

    /// The node of the card under the cursor, whole, with the values the cards read of it, as
    /// the table's peek shows a row.
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
        let title = format!("Cards: {}", self.title);
        match &self.at_commit {
            Some(commit) => format!("{title} @ {}", studio_ui::short_commit_id(commit)).into(),
            None => title.into(),
        }
    }

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(IconName::Image)
    }

    fn dedupe_key(&self, _cx: &App) -> Option<SharedString> {
        Some(crate::cards_key(&self.opening()).into())
    }

    /// Saved as kind `cards`: the set, the file that is the cover, and the card size. A tab
    /// the gallery saved (kind `gallery`, its cover as `property`) is read the same way
    /// ([`crate::saved_cards`]).
    fn serialize(&self, _cx: &App) -> Option<SavedItem> {
        let mut data = serde_json::to_value(self.opening()).ok()?;
        if let Some(map) = data.as_object_mut() {
            map.insert(
                "size".into(),
                serde_json::to_value(self.size).unwrap_or(Value::Null),
            );
        }
        Some(SavedItem {
            kind: crate::CARDS_KIND.into(),
            data,
        })
    }

    fn is_stale(&self, _cx: &App) -> bool {
        self.newer
    }

    /// The cards' part of the status bar: a page or covers being read, a Retry when the next
    /// page could not be, how many cards are picked, and the keys, each from its action.
    fn status(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Option<ItemStatus> {
        let mut start = Vec::new();
        if self.loading_more || !self.cover_tasks.is_empty() {
            start.push(studio_ui::ds::spinner().into_any_element());
        }
        // With no row shown the grid's place says so, and offers the same Retry.
        if let Some(error) = self.more_error.clone().filter(|_| !self.rows.is_empty()) {
            start.push(
                studio_ui::ds::row_button("cards-retry-more", "Retry")
                    .style(ButtonStyle::Subtle)
                    .start_icon(Icon::new(IconName::Warning))
                    .tip(studio_ui::ds::tooltip(format!(
                        "The next page could not be read: {error}"
                    )))
                    .on_click(cx.listener(|this, _, _, cx| this.retry_page(cx)))
                    .into_any_element(),
            );
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
        let mut keys = vec![
            key("Move", &MoveRight),
            key("Inspector", &ToggleInspector),
            key("Open page", &OpenPage),
            key("Open the cover", &OpenCover),
            key("Canvas", &ShowOnCanvas),
            key("Pick", &TogglePick),
            key("Pick all", &PickAll),
            key("Clear picks", &Cancel),
            key("Copy ids", &CopyIds),
        ];
        // Bound only while the type has another file to show.
        if self.files.len() > 1 {
            keys.push(key("Next cover", &ShowNextCover));
        }
        keys.extend([
            key("Larger cards", &LargerCards),
            key("Smaller cards", &SmallerCards),
            key("Refresh", &Refresh),
        ]);
        Some(ItemStatus {
            start,
            end: Vec::new(),
            keys,
        })
    }
}

impl Render for TypeCards {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        let mut context = KeyContext::new_with_defaults(cx.platform_info().os);
        context.add("Cards");
        // The gallery's context too (spec 035), so the bindings a person wrote for it still act.
        context.add("Gallery");
        let grid = self.grid.clone();
        v_flex()
            .key_context(context)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(studio_ui::ds::surface::document(cx))
            .on_action(cx.listener(|this, _: &MoveLeft, _, cx| this.move_cursor(Step::Left, cx)))
            .on_action(cx.listener(|this, _: &MoveRight, _, cx| this.move_cursor(Step::Right, cx)))
            .on_action(cx.listener(|this, _: &MoveUp, _, cx| this.move_cursor(Step::Up, cx)))
            .on_action(cx.listener(|this, _: &MoveDown, _, cx| this.move_cursor(Step::Down, cx)))
            .on_action(cx.listener(|this, _: &MoveToFirst, _, cx| this.set_cursor(0, cx)))
            .on_action(cx.listener(|this, _: &MoveToLast, _, cx| this.move_to_end(cx)))
            .on_action(cx.listener(|this, _: &OpenPage, window, cx| this.open_page(window, cx)))
            .on_action(cx.listener(|this, _: &OpenCover, window, cx| this.open_cover(window, cx)))
            .on_action(
                cx.listener(|this, _: &ShowOnCanvas, window, cx| this.show_on_canvas(window, cx)),
            )
            .on_action(cx.listener(|this, _: &TogglePick, _, cx| {
                if let Some(cursor) = this.cursor {
                    this.pick(cursor, false, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &PickAll, _, cx| this.pick_all(cx)))
            .on_action(cx.listener(|this, _: &Cancel, _, cx| this.cancel(cx)))
            .on_action(cx.listener(|this, _: &CopyIds, _, cx| this.copy_ids(cx)))
            .when(self.files.len() > 1, |cards| {
                cards.on_action(
                    cx.listener(|this, _: &ShowNextCover, _, cx| this.show_next_cover(cx)),
                )
            })
            .on_action(cx.listener(|this, _: &LargerCards, _, cx| {
                let size = this.size.larger();
                this.set_size(size, cx)
            }))
            .on_action(cx.listener(|this, _: &SmallerCards, _, cx| {
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

/// The cards, drawn as a view of their own and cached (`docs/drawing.md`): the toolbar's menus
/// and the status bar draw without them.
pub(crate) struct CardsGrid {
    cards: WeakEntity<TypeCards>,
    _cards_changed: Subscription,
    #[cfg(test)]
    pub(crate) renders: usize,
}

impl CardsGrid {
    /// Made with the tab, not on its first draw, so GPUI counts it among what the tab's draw
    /// depends on (`docs/drawing.md`).
    fn new(cx: &mut Context<TypeCards>) -> Entity<Self> {
        let cards = cx.entity();
        cx.new(|cx| CardsGrid {
            cards: cards.downgrade(),
            _cards_changed: cx.observe(&cards, |_, _, cx| cx.notify()),
            #[cfg(test)]
            renders: 0,
        })
    }
}

impl Render for CardsGrid {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        self.cards
            .update(cx, |cards, cx| cards.render_body(window, cx))
            .unwrap_or_else(|_| gpui::Empty.into_any_element())
    }
}

#[cfg(test)]
mod tests;

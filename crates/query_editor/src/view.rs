//! The query editor tab (spec 007): Zed's editor over `.gq` text with highlighting, checks as
//! you type and completion, then a run loop with parameters, a confirm step for writes,
//! results, plans and outcomes under it.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use editor::display_map::{
    BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId,
};
use editor::scroll::Autoscroll;
use editor::{
    Anchor, Bias, Editor, EditorEvent, HighlightKey, MultiBufferOffset, MultiBufferSnapshot,
    SelectionEffects, ToOffset as _,
};
use gpui::{
    AnyView, App, ClickEvent, ClipboardItem, Context, DragMoveEvent, Entity, EventEmitter,
    FocusHandle, Focusable, HighlightStyle, MouseButton, MouseDownEvent, Pixels, Render,
    ScrollHandle, ScrollStrategy, SharedString, StyleRefinement, Subscription, Task,
    UnderlineStyle, UniformListScrollHandle, WeakEntity, Window, uniform_list,
};
use gq_language::check::{Destroys, Publication};
use gq_language::{QueryInfo, ResultColumn, SchemaModel};
use graph_client::api::ErrorCode;
use graph_client::api::{
    ChangeRequest, ParamDescriptor, ParamKind, QueryCatalogEntry, QueryRequest, ReadOutput,
    ReadPageOutput,
};
use graph_client::{ReadError, WriteOutcome};
use graph_store::{GraphSession, SessionEvent};
use serde_json::{Map, Value};
use studio_actions::query::{
    ApplyFix, CopyCell, CopyRow, Explain, NextPage, NextProblem, PreviousProblem, Run, Save,
    SaveAs, SelectNextColumn, SelectPreviousColumn, ToggleResultsFocus,
};
use studio_actions::workspace::Refresh;
use studio_ui::ds::{Dotted as _, space};
use studio_ui::menu::{ContextMenu, ContextMenuEntry};
use studio_ui::{
    ColumnDrag, ColumnResize, GridColumn, GridPlace, ROW_HEIGHT, columns_in_view, format_value,
    grid_cell, grid_header, grid_places, is_numeric, resizable_grid_header, short_commit_id,
    value_text,
};
use studio_workspace::{Item, ItemEvent, ItemStatus, KeyHint, SavedItem};
use ui::prelude::*;
use ui::{KeyBinding, PopoverMenu, Tooltip};

use crate::analysis::Analysis;
use crate::completion::GqCompletionProvider;
use crate::results::{
    PlanItem, ResultGraph, ResultTable, ServerDiagnostic, SourceMap, WriteEffect, diff_page_text,
    format_elapsed, is_branch_diff, is_plan, plan_items, rows_label, server_diagnostic, type_label,
};

/// How long typing pauses before the source is checked again.
const CHECK_DELAY: Duration = Duration::from_millis(150);
/// The width of the row-number column in the result grid.
const ROW_NUMBER_WIDTH: f32 = 48.;
const PLAN_ROW_HEIGHT: f32 = studio_ui::ds::size::SIDEBAR_ROW;

/// Highlight keys: Zed's editor has a closed set; `ConsoleAnsiHighlight(n)` is the one keyed by
/// a number. Studio's diagnostics take numbers no Zed console uses.
const DIAGNOSTIC_KEY: usize = 0x6751_0100;
/// The parts of the query the hovered plan operator stands for.
const PLAN_KEY: usize = 0x6751_0200;

/// A run that is ready to send.
#[derive(Clone, Default)]
struct Prepared {
    text: String,
    name: Option<String>,
    /// The query's name, or the statement, for status lines.
    label: String,
    write: bool,
    statement: bool,
    params: Option<Value>,
    columns: Option<Vec<ResultColumn>>,
    map: SourceMap,
    /// The editor's text as it was sent, which `map` maps into: a place the answer names is
    /// anchored in it, so the anchor follows every edit made since.
    sent: Option<MultiBufferSnapshot>,
    /// The stored query to invoke by name instead of sending `text` (spec 007).
    stored: Option<String>,
    /// What the write destroys that Studio cannot put back: it is confirmed by typing.
    destroys: Option<Destroys>,
    /// Where the write's result goes: the branch, or held off it (`as commit`, `as unattached
    /// branch`), which the confirm bar says. A stored query's text is the server's, so it is
    /// taken to publish.
    publication: Publication,
    /// Which page of a paged read (`branch diff`) this asks for, from 1; 0 for any other run.
    page: usize,
}

/// Where a read's rows came from, as its answer says.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ReadAt {
    Branch(String),
    Commit(String),
    /// A statement, which names its own branches: no branch or commit of the tab's. A diff's
    /// page names the commits it compared.
    Statement,
}

impl ReadAt {
    /// ` on main`, ` on commit 01JABC`, or nothing for a statement, which names its own.
    fn suffix(&self) -> String {
        match self {
            ReadAt::Branch(branch) => format!(" on {branch}"),
            ReadAt::Commit(commit) => format!(" on commit {}", short_commit_id(commit)),
            ReadAt::Statement => String::new(),
        }
    }
}

/// The stored query a tab runs: the server serves it by name and never sends its text, so the
/// tab shows its description and parameters where a query tab shows its text.
struct Stored {
    entry: QueryCatalogEntry,
    /// The graph's catalog no longer lists it.
    gone: bool,
    /// Values for parameters the tab has no input for yet: it opened before the catalog was
    /// read. Each fills its input when the input is made.
    pending: Map<String, Value>,
}

/// A write waiting for the person to confirm it (constitution III, VII).
struct PendingWrite {
    prepared: Prepared,
    branch: String,
    /// A write Studio cannot put back is confirmed by typing what it destroys (IV).
    typed: Option<TypedConfirm>,
}

/// The words a destructive write is confirmed with, and the field they are typed in.
struct TypedConfirm {
    /// What goes, in a sentence.
    what: String,
    phrase: String,
    field: Entity<Editor>,
    /// A confirmation was asked for with other words in the field.
    refused: bool,
}

enum Output {
    /// Nothing has run yet.
    Empty,
    Running {
        label: SharedString,
    },
    /// Boxed: its table and its next page's run make it many times the others' size.
    Rows(Box<RowsOutput>),
    Plan(PlanOutput),
    Written(WriteResult),
    Failed(Failure),
}

struct RowsOutput {
    label: String,
    table: ResultTable,
    /// The result's nodes and links for the canvas, built once when the result arrived.
    graph: Option<Arc<ResultGraph>>,
    widths: Vec<f32>,
    right_aligned: Vec<bool>,
    /// What each row's cells show, made the first time the row is drawn rather than every
    /// frame it is.
    shown: Vec<std::cell::OnceCell<Box<[SharedString]>>>,
    row_count: i64,
    elapsed: Duration,
    read_at: ReadAt,
    commit: Option<String>,
    /// A paged read's page (`branch diff`): the commits it compares and its continuation.
    page: Option<ReadPageOutput>,
    /// Which page this is, from 1; 0 for a read that is not paged.
    page_number: usize,
    /// The run that reads the next page, when the server says there is one.
    next: Option<Prepared>,
    /// The read that made these rows, which Refresh sends again; none for a statement, which
    /// names its own branches, or a read at a pinned commit, which never moves.
    rerun: Option<Prepared>,
    /// The branch the rows were read at moved past the commit they were read at
    /// (constitution II): while it is the session's branch, the tab offers Refresh, and it
    /// never reads again by itself. Kept when the session moves to another branch and back.
    newer: bool,
    /// The head of that branch the session knew when the rows were shown, which may lag the
    /// commit they were read at: a later head other than both is a newer one.
    head_seen: Option<String>,
}

/// A read's rows made ready to show: the table, each column's width from its content, which
/// columns align right, the graph the node columns make, and a place for each row's cells.
struct BuiltRows {
    table: ResultTable,
    widths: Vec<f32>,
    right_aligned: Vec<bool>,
    graph: Option<Arc<ResultGraph>>,
    shown: Vec<std::cell::OnceCell<Box<[SharedString]>>>,
}

/// A read's answer as a run shows it, made off the main thread: a plan, rows built into the
/// table, or the refusal.
enum Answer {
    Plan(ReadOutput),
    Rows(ReadOutput, BuiltRows),
    Refused(ReadError),
}

impl Answer {
    fn of(
        result: Result<ReadOutput, ReadError>,
        columns: Option<&[ResultColumn]>,
        schema: Option<Arc<SchemaModel>>,
    ) -> Self {
        match result {
            Ok(output) if is_plan(&output.columns) => Answer::Plan(output),
            Ok(mut output) => {
                let built = BuiltRows::new(&mut output, columns, schema);
                Answer::Rows(output, built)
            }
            Err(error) => Answer::Refused(error),
        }
    }
}

impl BuiltRows {
    /// Takes the answer's rows into the table.
    fn new(
        output: &mut ReadOutput,
        columns: Option<&[ResultColumn]>,
        schema: Option<Arc<SchemaModel>>,
    ) -> Self {
        let rows = Arc::unwrap_or_clone(std::mem::take(&mut output.rows));
        let table = ResultTable::new(output.columns.clone(), rows, columns);
        let widths = (0..table.columns.len())
            .map(|i| table.column_width(i))
            .collect();
        let right_aligned = (0..table.columns.len())
            .map(|i| {
                table
                    .rows
                    .iter()
                    .find(|row| !row[i].is_null())
                    .is_some_and(|row| is_numeric(&row[i]))
            })
            .collect();
        let graph = schema.and_then(|schema| table.graph(&schema)).map(Arc::new);
        let shown = table.rows.iter().map(|_| Default::default()).collect();
        Self {
            table,
            widths,
            right_aligned,
            graph,
            shown,
        }
    }
}

struct PlanOutput {
    label: String,
    /// The planned declaration's name, to find it again in the text as it is now.
    name: Option<String>,
    items: Vec<PlanItem>,
    elapsed: Duration,
    commit: Option<String>,
}

enum WriteResult {
    /// The server committed the write: what it did ([`WriteEffect`]), and a mutation's or a
    /// load's counts (`None` for a branch or commit statement, which reports none).
    Done {
        effect: WriteEffect,
        counts: Option<(i64, i64)>,
        elapsed: Duration,
    },
    Refused {
        status: u16,
        message: String,
        diagnostic: Option<ServerDiagnostic>,
    },
    Unknown {
        reason: String,
    },
}

struct Failure {
    title: String,
    hint: Option<String>,
    diagnostic: Option<ServerDiagnostic>,
}

impl Failure {
    fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            hint: None,
            diagnostic: None,
        }
    }

    /// A failed read; a refusal's diagnostic is placed in the editor through `map`, from the
    /// text `sent`, which its position and edits count in.
    fn from_read(error: &ReadError, map: SourceMap, sent: &str) -> Self {
        let (title, hint) = match error {
            ReadError::Refused(summary) => {
                let hint = match summary.code {
                    Some(ErrorCode::Unauthorized) => {
                        "The server refused the token. Reconnect with a valid token."
                    }
                    Some(ErrorCode::Forbidden) => "Your token may not read this graph or branch.",
                    Some(ErrorCode::BadRequest) => {
                        "Fix the query (the problems list shows what the compiler sees) and run it again."
                    }
                    Some(ErrorCode::GraphUnavailable | ErrorCode::ServiceUnavailable) => {
                        "The graph is not serving right now. Try again in a moment."
                    }
                    Some(ErrorCode::TooManyRequests) => {
                        "The server is busy. Try again in a moment."
                    }
                    _ => "Check the query and the server, then run it again.",
                };
                (
                    format!(
                        "The server refused the read ({}): {}",
                        summary.status, summary.message
                    ),
                    hint,
                )
            }
            ReadError::Transport(_) => (
                error.to_string(),
                "Check the connection in the status bar, then run it again.",
            ),
            ReadError::ContractMismatch { .. } | ReadError::Decode(_) => (
                error.to_string(),
                "This server speaks a different OmniGraph contract than Studio.",
            ),
            // Nothing of it is shown: a part would read as the whole answer.
            ReadError::TooLarge { .. } => (
                format!("{error}."),
                "Add a `limit` to the query, or return fewer properties, then run it again.",
            ),
        };
        let diagnostic = match error {
            ReadError::Refused(summary) => summary
                .diagnostic
                .as_ref()
                .and_then(|d| server_diagnostic(d, sent, map)),
            _ => None,
        };
        Self {
            title,
            hint: Some(hint.into()),
            diagnostic,
        }
    }
}

/// A file tab's text as a remembered tab brings it back, and a hash of the file's text it was
/// typed over ([`content_hash`]): `None` when the tab was remembered by a build that kept no
/// hash, so whether the file changed since is not known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub text: String,
    pub based_on: Option<String>,
    /// The text was the file's when the tab was remembered: the file is shown when it can be
    /// read, and this only when it cannot (deleted, or gone with a `git switch`), so the text
    /// is not lost with it.
    pub saved: bool,
}

/// The longest text a saved file tab remembers for when its file is gone; a longer one comes
/// back only from its file. An unsaved draft is remembered whole.
const REMEMBERED_SAVED_TEXT: usize = 256 * 1024;

/// A file tab whose file is being read off the main thread: the tab shows it is reading, runs
/// and saves nothing, and keeps what it remembered until the file is in.
struct Loading {
    draft: Option<Draft>,
}

/// A file tab's file that could not be read when the tab came back (deleted, or gone with a
/// `git switch`).
struct GoneFile {
    /// The hash of the file's text the tab's text was typed over ([`Draft::based_on`]), kept
    /// while the file is gone: a file that comes back holding that text, or the tab's own, is
    /// the file as the tab knew it, not one changed elsewhere ([`QueryEditor::file_back`]).
    based_on: Option<String>,
}

/// The file changed on disk after the draft was typed over it (another editor, a `git pull`):
/// the draft is kept and the file is not written over until the person says so.
struct DiskChange {
    /// The hash of the text the draft was typed over, which a remembered tab keeps.
    based_on: Option<String>,
    /// ⌘S was pressed: the bar asks to save over the file or take its text.
    asked: bool,
}

/// Where a save goes.
enum SaveTarget {
    /// The file `path`, written only while it holds what is `expected` of it.
    File {
        path: std::path::PathBuf,
        expected: Expected,
    },
    /// A new file in `dir`, named after `name`, over none already there.
    New {
        dir: std::path::PathBuf,
        name: String,
    },
}

/// What a file must hold for a save to write over it.
enum Expected {
    /// Whatever it holds: a file chosen in the save dialog, or one the person said to save
    /// over.
    Anything,
    /// The text Studio last read or wrote there.
    Text(String),
    /// No file, as it was gone when the tab last read it. One back is compared with what the
    /// tab knew of it before anything is written ([`QueryEditor::file_back`]), even empty: an
    /// empty file is not the tab's text.
    NoFile,
}

/// What a save did, off the main thread.
enum Saved {
    Written(std::path::PathBuf),
    /// The file no longer holds the text Studio last read or wrote: this is what it holds.
    Changed(String),
}

/// A stable hash of a file's text, kept with a remembered draft to tell, after a restart,
/// whether the file changed since the draft was typed over it: FNV-1a over its bytes, with
/// their count, so the same text gives the same hash in every build.
pub fn content_hash(text: &str) -> String {
    let hash = text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("fnv1a64:{hash:016x}:{}", text.len())
}

/// One of the server's suggested edits, held in the editor. It is checked against its context,
/// its place with a little text either side, because an insertion's place alone holds no text
/// to check: text typed there, or the whole text replaced (a clean tab taking its file's),
/// leaves an empty place empty.
struct HeldEdit {
    /// The context, anchored in the text as it was run; text typed just outside it stays out.
    context: std::ops::Range<Anchor>,
    /// The context's text as it was run: the edit holds while the context still reads so.
    original: String,
    /// Where the edit is in the context, in bytes.
    within: std::ops::Range<usize>,
    replacement: String,
}

/// How many bytes either side of a held edit its context takes.
const EDIT_CONTEXT: usize = 16;

/// How many of the branch's latest commits the snapshot picker offers.
const PICKER_COMMITS: usize = 15;

static NEXT_NUMBER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

/// The next tab number: "Query 1", "Query 2", ….
pub fn next_number() -> usize {
    NEXT_NUMBER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub struct QueryEditor {
    number: usize,
    session: Entity<GraphSession>,
    /// The `.gq` file this tab edits, and its text as last read or written (spec 007).
    path: Option<std::path::PathBuf>,
    saved_text: Option<String>,
    /// The text differs from `saved_text`: found once per edit, not on every draw of the tab.
    unsaved: bool,
    file_error: Option<String>,
    /// The file's text is being read: the tab says so until it is in.
    loading: Option<Loading>,
    /// The file could not be read when the tab came back: the tab holds the text it
    /// remembered, and a bar says the file is gone and offers Save As.
    file_gone: Option<GoneFile>,
    /// The file changed on disk after the draft was typed over it, and `saved_text` is what
    /// it holds now: the draft stays, and is written over the file only when the person says.
    disk_change: Option<DiskChange>,
    /// A read of the file to see whether it changed elsewhere, made when the editor takes the
    /// keyboard; a save drops it, since it would answer for the text before the save.
    disk_task: Option<Task<()>>,
    /// The bar that says the file changed on disk, which ⌘S moves the keyboard to.
    file_focus: FocusHandle,
    /// The file is being written. A tab writes one at a time, so a second ⌘S neither names
    /// another new file nor lands before the first.
    saving: bool,
    /// Asked for while saving, and done once the write ends: ⌘S (`None`), or a file chosen to
    /// save as.
    save_next: Option<Option<std::path::PathBuf>>,
    /// The run line above each declaration (spec 007): the blocks, the declarations they were
    /// made for, and each declaration's last outcome by name.
    run_lines: Vec<CustomBlockId>,
    run_line_names: Vec<String>,
    /// Where each run line's declaration starts, kept by the text as it is edited.
    run_line_starts: Vec<Anchor>,
    /// Where each problem of the last check is, kept by the text as it is edited: a jump made
    /// before the next check lands where the problem is now.
    problem_places: Vec<Option<std::ops::Range<Anchor>>>,
    /// The problem the keyboard went to ([`NextProblem`]) or that was clicked, which
    /// [`ApplyFix`] fixes; a cursor moved off its place lets it go.
    selected_problem: Option<usize>,
    /// The problems list, scrolled to the problem gone to.
    problems_scroll: UniformListScrollHandle,
    /// The problem ⌘. fixes as the problems list last showed it: the list is drawn again when
    /// the cursor moves into another one's place, not at every key.
    fix_shown: Option<usize>,
    /// The problems list as a view of its own, drawn again only when what it shows changes.
    problems: Entity<QueryProblems>,
    /// Where the server's diagnostic shown points in the text, followed through later edits.
    diagnostic_at: Option<Anchor>,
    /// The server's suggested edits for that diagnostic, anchored in the text as it was run,
    /// so they land where they were meant after edits elsewhere.
    diagnostic_edits: Vec<HeldEdit>,
    /// Every one of `diagnostic_edits` still finds the text it was made for, so Apply is
    /// offered; found again at each edit, and the results drawn again when it changes.
    server_fix_holds: bool,
    last_runs: HashMap<String, RunNote>,
    /// The branch's latest commits, for the snapshot picker: id, Unix micros, actor. Read when
    /// the picker opens, and kept until a commit lands on the branch.
    commits: Vec<(String, i64, Option<String>)>,
    /// `commits` is the branch's newest page as of the last commit seen.
    commits_fresh: bool,
    /// The read of `commits` failed: the picker says so.
    commits_failed: bool,
    commits_task: Option<Task<()>>,
    /// The snapshot picker's menu while it is open: it shows the commits when they come.
    picker_menu: Option<WeakEntity<ContextMenu>>,
    /// Reads run at this commit and writes are refused (spec 009); `None` uses the branch.
    at_commit: Option<String>,
    editor: Entity<Editor>,
    /// The editor's focus, held so a part drawn on its own (the problems list) names it
    /// without reading the editor, which would make that part draw again at every key.
    editor_focus: FocusHandle,
    /// The buffer parses as GQ (`syntax`): colours, brackets, indents and folds follow the
    /// syntax tree. Until then the text is plain.
    parsed: bool,
    /// The latest check; it lags the text by `CHECK_DELAY`.
    analysis: Option<Arc<Analysis>>,
    cursor: usize,
    params: HashMap<String, Entity<Editor>>,
    output: Output,
    /// The plan row under the pointer; its parts of the query are lit.
    plan_hovered: Option<usize>,
    pending_write: Option<PendingWrite>,
    writing: bool,
    selected: Option<(usize, usize)>,
    /// Result columns dragged to a width, by name: they keep it in later results and come back
    /// with the tab.
    column_widths: BTreeMap<String, f32>,
    /// The result column being dragged, by position.
    resizing: Option<ColumnResize<usize>>,
    /// The tab was filled with an example read once; emptied, it stays empty.
    filled_example: bool,
    results_scroll: UniformListScrollHandle,
    /// How far the results are scrolled sideways, for the columns a frame draws.
    results_hscroll: ScrollHandle,
    results_focus: FocusHandle,
    confirm_focus: FocusHandle,
    run_task: Option<Task<()>>,
    check_task: Option<Task<()>>,
    /// A stored query's tab (spec 007); `None` for a query written in the editor.
    stored: Option<Stored>,
    /// The result area as a view of its own, drawn again only when what it shows changes
    /// ([`Self::redraw_results`]).
    results: Entity<QueryResults>,
    _subscriptions: Vec<Subscription>,
}

/// The tab's result area, drawn as a view of its own, so a keystroke in the query or a check
/// of it draws the toolbar and the text again but leaves the rows as they were
/// (`docs/drawing.md`). The tab tells it when what it shows changed; it does not follow every
/// notify of the tab.
struct QueryResults {
    query: WeakEntity<QueryEditor>,
    /// How often the results were drawn, for the test that typing leaves them alone.
    #[cfg(test)]
    renders: usize,
}

impl QueryResults {
    /// Made with the tab, not on its first draw (`docs/drawing.md`).
    fn new(cx: &mut Context<QueryEditor>) -> Entity<Self> {
        let query = cx.entity().downgrade();
        cx.new(|_| QueryResults {
            query,
            #[cfg(test)]
            renders: 0,
        })
    }
}

impl Render for QueryResults {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        let results = self
            .query
            .update(cx, |query, cx| query.render_results(window, cx))
            .unwrap_or_else(|_| gpui::Empty.into_any_element());
        v_flex().size_full().child(results)
    }
}

/// The problems list under the query, drawn as a view of its own, as the result area is: the
/// tab draws again at every key, the list only when its problems, the one gone to or the one
/// ⌘. fixes change ([`QueryEditor::redraw_problems`]). Its rows are a virtualized list.
struct QueryProblems {
    query: WeakEntity<QueryEditor>,
    /// How often the list was drawn, for the test that typing leaves it alone.
    #[cfg(test)]
    renders: usize,
}

impl QueryProblems {
    /// Made with the tab, not on its first draw (`docs/drawing.md`).
    fn new(cx: &mut Context<QueryEditor>) -> Entity<Self> {
        let query = cx.entity().downgrade();
        cx.new(|_| QueryProblems {
            query,
            #[cfg(test)]
            renders: 0,
        })
    }
}

impl Render for QueryProblems {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        self.query
            .update(cx, |query, cx| query.render_problems(cx))
            .ok()
            .flatten()
            .unwrap_or_else(|| gpui::Empty.into_any_element())
    }
}

/// How many problem rows show before the list scrolls.
const PROBLEM_ROWS: usize = 4;

impl EventEmitter<ItemEvent> for QueryEditor {}

impl Focusable for QueryEditor {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.main_focus(cx)
    }
}

impl Item for QueryEditor {
    /// The result area is drawn cached on its own (`QueryResults`).
    fn caches_its_parts(&self) -> bool {
        true
    }

    fn tab_title(&self, _cx: &App) -> SharedString {
        let name = match (&self.stored, &self.path) {
            (Some(stored), _) => stored.entry.name.clone(),
            (None, path) => match path {
                Some(path) => path
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_else(|| format!("Query {}", self.number)),
                None => format!("Query {}", self.number),
            },
        };
        match &self.at_commit {
            Some(commit) => format!("{name} @ {}", short_commit_id(commit)).into(),
            None => name.into(),
        }
    }

    /// A file-backed query with changes since it was saved.
    fn is_dirty(&self, _cx: &App) -> bool {
        self.writing || self.stored.is_none() && self.unsaved
    }

    /// The rows shown were read at a commit their branch, the one shown, has moved past.
    fn is_stale(&self, cx: &App) -> bool {
        let branch = &self.session.read(cx).branch;
        matches!(&self.output, Output::Rows(rows)
            if rows.newer && matches!(&rows.read_at, ReadAt::Branch(read) if read == branch))
    }

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(if self.stored.is_some() {
            IconName::Server
        } else {
            IconName::FileCode
        })
    }

    fn dedupe_key(&self, _cx: &App) -> Option<SharedString> {
        if let Some(stored) = &self.stored {
            return Some(stored_key(&stored.entry.name).into());
        }
        Some(match &self.path {
            Some(path) => file_key(path).into(),
            None => format!("query:{}", self.number).into(),
        })
    }

    /// The tab's part of the status bar, as the table's: the result's facts on the leading
    /// side, what the keyboard does with it on the trailing side.
    fn status(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Option<ItemStatus> {
        Some(self.status_parts(cx))
    }

    /// The draft comes back after a restart (spec 007: the text is never cleared). It is
    /// written to the workspace file, never to a log. A file tab keeps its text only while it
    /// is unsaved, with the hash of the file's text it was typed over, so a file changed since
    /// is told from the draft; a saved one comes back as the file is then.
    fn serialize(&self, cx: &App) -> Option<SavedItem> {
        if let Some(stored) = &self.stored {
            // The values typed come back with the tab, as a query's text does, and so do
            // those that came before the catalog and wait for their inputs.
            let mut values = stored.pending.clone();
            values.extend(
                self.params
                    .iter()
                    .map(|(name, input)| (name.clone(), Value::String(input.read(cx).text(cx)))),
            );
            return Some(SavedItem {
                kind: "stored_query".into(),
                data: serde_json::json!({
                    "name": stored.entry.name,
                    "at_commit": self.at_commit,
                    "values": values,
                    "widths": self.column_widths,
                }),
            });
        }
        let mut data = serde_json::json!({
            "at_commit": self.at_commit,
            "path": self.path.as_ref().map(|path| path.to_string_lossy().into_owned()),
            "widths": self.column_widths,
        });
        let draft = match (&self.loading, &self.path) {
            // Still being read: what was remembered, as it was.
            (Some(loading), _) => loading.draft.clone(),
            // Saved: the file's text, for the restart that finds the file gone.
            (None, Some(_)) if !self.unsaved => {
                let text = self.editor.read(cx).text(cx);
                (text.len() <= REMEMBERED_SAVED_TEXT).then(|| Draft {
                    based_on: Some(content_hash(&text)),
                    text,
                    saved: true,
                })
            }
            (None, Some(_)) => Some(Draft {
                text: self.editor.read(cx).text(cx),
                // While the file is gone `saved_text` is empty, which the text was not typed
                // over: the gone file's record keeps what it was.
                based_on: match (&self.disk_change, &self.file_gone) {
                    (Some(change), _) => change.based_on.clone(),
                    (None, Some(gone)) => gone.based_on.clone(),
                    (None, None) => self.saved_text.as_deref().map(content_hash),
                },
                saved: false,
            }),
            (None, None) => Some(Draft {
                text: self.editor.read(cx).text(cx),
                based_on: None,
                saved: false,
            }),
        };
        if let Some(draft) = draft {
            data["text"] = Value::String(draft.text);
            if let Some(based_on) = draft.based_on {
                data["based_on"] = Value::String(based_on);
            }
            if draft.saved {
                data["saved"] = Value::Bool(true);
            }
        }
        Some(SavedItem {
            kind: "query".into(),
            data,
        })
    }
}

impl QueryEditor {
    /// A new query, filled with an example read once the schema is in.
    pub fn new(
        number: usize,
        session: Entity<GraphSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(number, session, None, None, window, cx)
    }

    /// A query with `text` given by whoever opens it (a remembered tab, a link, the agent):
    /// kept as it is, even empty, never replaced by the example.
    pub fn with_text(
        number: usize,
        session: Entity<GraphSession>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(number, session, None, Some(text), window, cx)
    }

    /// A tab for the file `path`, whose text is being read: it shows it is reading until
    /// [`Self::file_read`] gives it the text, and keeps `draft`, a remembered tab's unsaved
    /// text, until then.
    pub fn reading(
        number: usize,
        session: Entity<GraphSession>,
        path: std::path::PathBuf,
        draft: Option<Draft>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self::build(number, session, None, Some(""), window, cx);
        this.editor
            .update(cx, |editor, _| editor.set_read_only(true));
        this.path = Some(path);
        this.loading = Some(Loading { draft });
        this
    }

    /// A tab for the stored query `entry` (spec 007), with an input for each parameter.
    pub fn new_stored(
        number: usize,
        session: Entity<GraphSession>,
        entry: QueryCatalogEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let stored = Stored {
            entry,
            gone: false,
            pending: Map::new(),
        };
        let mut this = Self::build(number, session, Some(stored), None, window, cx);
        this.sync_stored_params(window, cx);
        // Opened before the catalog was read, or after the query left it.
        this.catalog_changed(window, cx);
        this
    }

    fn build(
        number: usize,
        session: Entity<GraphSession>,
        stored: Option<Stored>,
        text: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            let placeholder = match studio_ui::shortcut_text(&Run, cx) {
                Some(keys) => format!("Write a query. {keys} runs the query under the cursor."),
                None => "Write a query, then run the query under the cursor.".to_owned(),
            };
            editor.set_placeholder_text(&placeholder, window, cx);
            editor.set_completion_provider(Some(std::rc::Rc::new(GqCompletionProvider::new(
                session.downgrade(),
            ))));
            editor.set_semantics_provider(Some(std::rc::Rc::new(crate::hover::GqSemantics::new(
                session.downgrade(),
            ))));
            editor
        });
        let editor_focus = editor.focus_handle(cx);
        let subscriptions = vec![
            cx.subscribe_in(
                &editor,
                window,
                |this, _, event: &EditorEvent, window, cx| match event {
                    EditorEvent::BufferEdited => this.edited(window, cx),
                    EditorEvent::SelectionsChanged { .. } => this.cursor_moved(window, cx),
                    _ => {}
                },
            ),
            // Coming back to a file tab, its file may have changed elsewhere.
            cx.on_focus_in(&editor_focus, window, |this, window, cx| {
                this.check_disk(window, cx)
            }),
            cx.subscribe_in(
                &session,
                window,
                |this, _, event: &SessionEvent, window, cx| match event {
                    // Another writer's schema too: the text is checked against the schema the
                    // session reads under now.
                    SessionEvent::SchemaChanged | SessionEvent::SchemaMoved => {
                        this.set_example(window, cx);
                        this.schedule_check(window, cx);
                    }
                    SessionEvent::BranchChanged => {
                        // A write waiting to be confirmed was for the branch shown when it
                        // was asked for. So are the commits offered to pin to: none until the
                        // new branch's are read, when the picker opens.
                        this.pending_write = None;
                        this.commits.clear();
                        this.commits_stale(window, cx);
                        // Rows of the branch left name it in the footer, and offer Refresh
                        // only on their own branch, which Run reads: back on it, they are
                        // compared with the head the session knows for it now.
                        this.compare_rows_with_head(cx);
                        cx.emit(ItemEvent::UpdateTab);
                        // A write still to run says which branch it asks before writing to.
                        this.redraw_expected(cx);
                        cx.notify()
                    }
                    SessionEvent::BranchesChanged => cx.notify(),
                    SessionEvent::Committed { branch }
                        if *branch == this.session.read(cx).branch =>
                    {
                        this.commits_stale(window, cx)
                    }
                    SessionEvent::HeadMoved { branch, head } => this.head_moved(branch, head, cx),
                    SessionEvent::StoredQueriesChanged => this.catalog_changed(window, cx),
                    _ => {}
                },
            ),
        ];
        let mut this = Self {
            number,
            session,
            path: None,
            saved_text: None,
            unsaved: false,
            file_error: None,
            loading: None,
            file_gone: None,
            disk_change: None,
            disk_task: None,
            file_focus: cx.focus_handle(),
            saving: false,
            save_next: None,
            commits: Vec::new(),
            commits_fresh: false,
            commits_failed: false,
            commits_task: None,
            picker_menu: None,
            run_lines: Vec::new(),
            run_line_names: Vec::new(),
            run_line_starts: Vec::new(),
            problem_places: Vec::new(),
            selected_problem: None,
            problems_scroll: UniformListScrollHandle::new(),
            fix_shown: None,
            problems: QueryProblems::new(cx),
            diagnostic_at: None,
            diagnostic_edits: Vec::new(),
            server_fix_holds: false,
            last_runs: HashMap::new(),
            at_commit: None,
            editor,
            editor_focus,
            parsed: false,
            analysis: None,
            cursor: 0,
            params: HashMap::new(),
            output: Output::Empty,
            plan_hovered: None,
            pending_write: None,
            writing: false,
            selected: None,
            column_widths: BTreeMap::new(),
            resizing: None,
            filled_example: false,
            results_scroll: UniformListScrollHandle::new(),
            results_hscroll: ScrollHandle::new(),
            results_focus: cx.focus_handle(),
            confirm_focus: cx.focus_handle(),
            run_task: None,
            check_task: None,
            stored,
            results: QueryResults::new(cx),
            _subscriptions: subscriptions,
        };
        this.parse_as_gq(cx);
        match text {
            // Text given, even empty, is the tab's: the example never replaces it.
            Some(text) => this.set_text(text, window, cx),
            None => this.set_example(window, cx),
        }
        this
    }

    /// Gives the editor's buffer the GQ language, so it is parsed with the tree-sitter grammar.
    fn parse_as_gq(&mut self, cx: &mut Context<Self>) {
        let Some(language) = crate::syntax::language(crate::syntax::Syntax::Gq, cx) else {
            return;
        };
        let buffer = self.editor.read(cx).buffer().read(cx).as_singleton();
        if let Some(buffer) = buffer {
            buffer.update(cx, |buffer, cx| buffer.set_language(Some(language), cx));
            self.parsed = true;
        }
    }

    /// The branch's latest commits for the snapshot picker: one page of its history, which
    /// costs only the commits it holds, where the whole list costs every commit the branch has.
    /// An open picker shows them when they come.
    fn load_commits(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let read = self.session.update(cx, |session, cx| {
            let branch = session.branch.clone();
            session.commit_page(&branch, PICKER_COMMITS as u32, None, cx)
        });
        self.commits_failed = false;
        self.commits_task = Some(cx.spawn_in(window, async move |this, cx| {
            let page = read.await;
            let Ok(menu) = this.update(cx, |this, cx| {
                this.commits_task = None;
                match page {
                    Ok(page) => {
                        this.commits = page
                            .commits
                            .iter()
                            .map(|c| (c.graph_commit_id.clone(), c.created_at, c.actor_id.clone()))
                            .collect();
                        this.commits_fresh = true;
                    }
                    Err(_) => this.commits_failed = true,
                }
                cx.notify();
                this.picker_menu.as_ref().and_then(WeakEntity::upgrade)
            }) else {
                return;
            };
            // The menu is built from the tab's state, so outside the tab's update.
            if let Some(menu) = menu {
                cx.update(|window, cx| menu.update(cx, |menu, cx| menu.rebuild(window, cx)))
                    .ok();
            }
        }));
    }

    /// A commit landed on the branch, or the branch changed: the commits held are not its
    /// newest any more. An open picker reads them again; a closed one does when it opens.
    fn commits_stale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.commits_fresh = false;
        // A read under way answers for what was before.
        self.commits_task = None;
        if self.picker_open() {
            self.load_commits(window, cx);
        }
    }

    fn picker_open(&self) -> bool {
        self.picker_menu
            .as_ref()
            .is_some_and(|menu| menu.upgrade().is_some())
    }

    /// The snapshot picker opened with `menu`: the branch's commits are read unless those held
    /// are its newest.
    fn picker_opened(
        &mut self,
        menu: WeakEntity<ContextMenu>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.picker_menu = Some(menu);
        if !self.commits_fresh && self.commits_task.is_none() {
            self.load_commits(window, cx);
        }
    }

    /// The snapshot picker's menu, built from what the tab holds whenever it is built again.
    fn picker_menu(
        this: WeakEntity<Self>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<ContextMenu> {
        ContextMenu::build_persistent(window, cx, move |menu, _, cx| {
            let menu = menu.keep_open_on_confirm(false);
            match this.upgrade() {
                Some(editor) => editor.read(cx).picker_entries(menu, this.clone(), cx),
                None => menu,
            }
        })
    }

    /// The head of the branch, then its latest commits as they were read.
    fn picker_entries(
        &self,
        mut menu: ContextMenu,
        editor: WeakEntity<Self>,
        cx: &App,
    ) -> ContextMenu {
        let branch = self.branch(cx);
        let pinned = self.at_commit.clone();
        let head_editor = editor.clone();
        menu = menu.toggleable_entry(
            format!("Head of {branch}"),
            pinned.is_none(),
            IconPosition::Start,
            None,
            move |_, cx| {
                head_editor.update(cx, |editor, cx| editor.unpin(cx)).ok();
            },
        );
        let waiting =
            (!self.commits_fresh && self.commits.is_empty()).then_some(if self.commits_failed {
                "The latest commits could not be read"
            } else {
                "Reading the latest commits…"
            });
        if !self.commits.is_empty() || waiting.is_some() {
            menu = menu.separator().header("Read at a commit");
        }
        if let Some(waiting) = waiting {
            menu = menu.item(ContextMenuEntry::new(waiting).disabled(true));
        }
        let now = studio_ui::now_micros();
        for (id, at, actor) in self.commits.iter().cloned() {
            let editor = editor.clone();
            let label = format!(
                "{} · {}{}",
                short_commit_id(&id),
                studio_ui::relative_time(at, now),
                actor.map(|a| format!(" · {a}")).unwrap_or_default()
            );
            let checked = pinned.as_deref() == Some(id.as_str());
            menu =
                menu.toggleable_entry(label, checked, IconPosition::Start, None, move |_, cx| {
                    let id = id.clone();
                    editor.update(cx, |editor, cx| editor.pin_to(id, cx)).ok();
                });
        }
        menu
    }

    /// Reads at the branch head again, and writes are allowed again.
    fn unpin(&mut self, cx: &mut Context<Self>) {
        if self.at_commit.take().is_some() {
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
        }
    }

    /// The toolbar's snapshot picker: the branch head, or one of its latest commits, read
    /// only (spec 007).
    fn render_snapshot_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let branch = self.branch(cx);
        let pinned = self.at_commit.clone();
        let editor = cx.entity().downgrade();
        let label = match &pinned {
            Some(commit) => format!("Read only · {}", short_commit_id(commit)),
            None => format!("Head of {branch}"),
        };
        PopoverMenu::new("snapshot-picker")
            .trigger(
                studio_ui::ds::toolbar_button("snapshot-picker-button", label)
                    .style(ButtonStyle::Subtle)
                    .start_icon(Icon::new(if pinned.is_some() {
                        IconName::GitCommit
                    } else {
                        IconName::GitBranch
                    }))
                    .end_icon(Icon::new(IconName::ChevronDown))
                    .tooltip(Tooltip::text(match &pinned {
                        Some(commit) => {
                            format!("Reads run at commit {commit}; writes are refused")
                        }
                        None => format!("Reads run at the head of {branch}"),
                    })),
            )
            .menu(move |window, cx| {
                // The commits are read when the picker opens, not when the tab does.
                let menu = Self::picker_menu(editor.clone(), window, cx);
                let opened = menu.downgrade();
                editor
                    .update(cx, |editor, cx| editor.picker_opened(opened, window, cx))
                    .ok();
                Some(menu)
            })
    }

    /// Result columns' dragged widths, by name, from a remembered tab.
    pub fn set_column_widths(&mut self, widths: BTreeMap<String, f32>) {
        self.column_widths = widths
            .into_iter()
            .map(|(name, width)| (name, studio_ui::clamp_width(width)))
            .collect();
    }

    /// Pins reads to a commit and refuses writes, for a query opened from History.
    pub fn pin_to(&mut self, commit: String, cx: &mut Context<Self>) {
        self.at_commit = Some(commit);
        // A pinned tab only reads.
        self.pending_write = None;
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// Makes this tab edit `path`, whose text on disk is `saved` (`None` when it could not be
    /// read, so any text counts as changed), as [`Self::file_read`] does for a file read.
    #[cfg(test)]
    pub fn set_file(
        &mut self,
        path: std::path::PathBuf,
        saved: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.path = Some(path);
        self.saved_text = Some(saved.unwrap_or_default());
        self.sync_unsaved(cx);
        self.file_error = None;
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// The file this tab was [`Self::reading`] was read: `disk` is its text, `None` when it
    /// could not be read. A tab that was saved, or remembered nothing, shows the file; a draft
    /// typed over the text the file still holds is kept; a draft over a file that changed
    /// since is kept too, with the bar saying so, and ⌘S asks before writing over the file. A
    /// file that cannot be read leaves the tab with the text it remembered, saved or not, and
    /// a bar saying the file is gone. Returns false when there is nothing to show: nothing
    /// remembered, and no file.
    pub fn file_read(
        &mut self,
        disk: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(Loading { draft }) = self.loading.take() else {
            return true;
        };
        self.editor
            .update(cx, |editor, _| editor.set_read_only(false));
        match (draft, disk) {
            (None, None) => return false,
            (None, Some(disk)) | (Some(Draft { saved: true, .. }), Some(disk)) => {
                self.saved_text = Some(disk.clone());
                self.set_text(&disk, window, cx);
            }
            (Some(draft), None) => {
                // Nothing is on disk: the text is the tab's alone, and closing it asks. What it
                // was typed over is kept, for the file coming back.
                self.saved_text = Some(String::new());
                self.file_gone = Some(GoneFile {
                    based_on: draft.based_on,
                });
                self.set_text(&draft.text, window, cx);
            }
            (Some(draft), Some(disk)) => {
                let unchanged = draft.text == disk
                    || draft.based_on.as_deref() == Some(content_hash(&disk).as_str());
                if !unchanged {
                    self.disk_change = Some(DiskChange {
                        based_on: draft.based_on,
                        asked: false,
                    });
                }
                self.saved_text = Some(disk);
                self.set_text(&draft.text, window, cx);
            }
        }
        self.sync_unsaved(cx);
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
        true
    }

    /// Reads the file again, off the main thread, to see whether it changed elsewhere since
    /// Studio last read or wrote it: when the editor takes the keyboard.
    fn check_disk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.path.clone() else {
            return;
        };
        if self.loading.is_some() || self.saving || self.disk_task.is_some() {
            return;
        }
        let read = cx.background_spawn(async move { std::fs::read_to_string(&path).ok() });
        self.disk_task = Some(cx.spawn_in(window, async move |this, cx| {
            let disk = read.await;
            this.update_in(cx, |this, window, cx| {
                this.disk_task = None;
                this.disk_read(disk, window, cx);
            })
            .ok();
        }));
    }

    /// The file holds `disk` now. A tab with nothing unsaved shows it; one with a draft keeps
    /// the draft and says the file changed. A file gone says nothing until a save writes it,
    /// and one back is compared with what the tab knew of it ([`Self::file_back`]).
    fn disk_read(&mut self, disk: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(disk) = disk else {
            return;
        };
        if self.saving || self.loading.is_some() {
            return;
        }
        if let Some(gone) = self.file_gone.take() {
            self.file_back(gone, disk, cx);
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
            return;
        }
        let Some(saved) = &self.saved_text else {
            return;
        };
        if disk == *saved {
            return;
        }
        if self.unsaved || self.disk_change.is_some() {
            if self.disk_change.is_none() {
                self.disk_change = Some(DiskChange {
                    based_on: Some(content_hash(saved)),
                    asked: false,
                });
            }
            self.saved_text = Some(disk);
            self.sync_unsaved(cx);
        } else {
            self.saved_text = Some(disk.clone());
            self.set_text(&disk, window, cx);
        }
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// The tab's file was gone and holds `disk` again (a `git switch` back), seen by a read or
    /// by a save. While it was gone the tab compared its text with nothing, so `disk` is
    /// compared with what the tab knew of the file instead: the tab's own text, or the text it
    /// was typed over (`gone.based_on`), is the file as the tab knew it and is taken as the
    /// file's text with no bar, the tab's text unsaved only when it differs from it. Other text
    /// was changed elsewhere, and the bar says so as for any change.
    fn file_back(&mut self, gone: GoneFile, disk: String, cx: &mut Context<Self>) {
        let the_tabs = {
            let buffer = self.editor.read(cx).buffer().read(cx).read(cx);
            let end = buffer.len();
            studio_ui::text::pieces_are(
                end.0,
                buffer.text_for_range(MultiBufferOffset(0)..end),
                &disk,
            )
        };
        let typed_over = gone.based_on.as_deref() == Some(content_hash(&disk).as_str());
        if !the_tabs && !typed_over {
            self.disk_change = Some(DiskChange {
                based_on: gone.based_on,
                asked: false,
            });
        }
        self.saved_text = Some(disk);
        self.sync_unsaved(cx);
    }

    /// The bar's Save over it: the draft is written over the file's newer text.
    fn save_over(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.path.clone() else {
            return;
        };
        if self.saving || self.disk_change.is_none() {
            return;
        }
        self.write_to(
            SaveTarget::File {
                path,
                expected: Expected::Anything,
            },
            window,
            cx,
        );
        window.focus(&self.editor_focus, cx);
    }

    /// The bar's Use the file's text: the draft is replaced by what the file holds now, an
    /// edit ⌘Z takes back.
    fn take_disk_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(disk) = self.saved_text.clone() else {
            return;
        };
        if self.disk_change.take().is_none() {
            return;
        }
        self.set_text(&disk, window, cx);
        self.sync_unsaved(cx);
        window.focus(&self.editor_focus, cx);
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// Whether the text differs from the file's, compared where it lies: a keystroke copies
    /// none of it.
    fn sync_unsaved(&mut self, cx: &App) {
        let Some(saved) = &self.saved_text else {
            self.unsaved = false;
            return;
        };
        let buffer = self.editor.read(cx).buffer().read(cx).read(cx);
        let end = buffer.len();
        self.unsaved = !studio_ui::text::pieces_are(
            end.0,
            buffer.text_for_range(MultiBufferOffset(0)..end),
            saved,
        );
    }

    /// ⌘S: writes the file this tab edits; a new query is saved in this graph's queries folder,
    /// named after its first declaration.
    fn save(
        &mut self,
        _: &studio_actions::query::Save,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save_now(window, cx);
    }

    fn save_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A stored query lives on the server; there is no text here to save. A file still
        // being read has none yet.
        if self.stored.is_some() || self.loading.is_some() {
            return;
        }
        if self.saving {
            // A file chosen to save as meanwhile is where this goes too.
            self.save_next.get_or_insert(None);
            return;
        }
        // The file changed on disk since the draft was typed over it: nothing is written over
        // it until the person says so, in the bar ⌘S moves the keyboard to.
        if let Some(change) = &mut self.disk_change {
            change.asked = true;
            window.focus(&self.file_focus, cx);
            cx.notify();
            return;
        }
        let target = match &self.path {
            Some(path) => SaveTarget::File {
                path: path.clone(),
                expected: match (&self.file_gone, &self.saved_text) {
                    (Some(_), _) => Expected::NoFile,
                    (None, Some(saved)) => Expected::Text(saved.clone()),
                    (None, None) => Expected::Anything,
                },
            },
            None => {
                let graph = self.session.read(cx).overrides_key(cx);
                let name = gq_language::outline(&self.editor.read(cx).text(cx))
                    .declarations
                    .first()
                    .map(|d| d.name.clone())
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| format!("query-{}", self.number));
                SaveTarget::New {
                    dir: studio_settings::queries_dir(&graph),
                    name,
                }
            }
        };
        self.write_to(target, window, cx);
    }

    /// ⌘⇧S: writes the query to a file chosen in the system's save dialog.
    fn save_as(
        &mut self,
        _: &studio_actions::query::SaveAs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.stored.is_some() || self.loading.is_some() {
            return;
        }
        let graph = self.session.read(cx).overrides_key(cx);
        let directory = self
            .path
            .as_ref()
            .and_then(|path| path.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| studio_settings::queries_dir(&graph));
        let suggested = self
            .path
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "query.gq".to_owned());
        // The dialog opens in the graph's folder, made off the main thread first.
        let made = {
            let directory = directory.clone();
            cx.background_spawn(async move { std::fs::create_dir_all(&directory).ok() })
        };
        cx.spawn_in(window, async move |this, cx| {
            made.await;
            let Ok(chosen) = this.update(cx, |_, cx| {
                cx.prompt_for_new_path(&directory, Some(&suggested))
            }) else {
                return;
            };
            let Ok(Ok(Some(path))) = chosen.await else {
                return;
            };
            // The dialog asked before replacing a file: whatever it holds is written over.
            let target = SaveTarget::File {
                path,
                expected: Expected::Anything,
            };
            this.update_in(cx, |this, window, cx| this.write_to(target, window, cx))
                .ok();
        })
        .detach();
    }

    /// Writes the text to `target` off the main thread, one write at a time. A file that no
    /// longer holds the text Studio last read or wrote is left as it is, and the bar says it
    /// changed.
    fn write_to(&mut self, target: SaveTarget, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading.is_some() {
            return;
        }
        if self.saving {
            match target {
                SaveTarget::File {
                    path,
                    expected: Expected::Anything,
                } => self.save_next = Some(Some(path)),
                _ => {
                    self.save_next.get_or_insert(None);
                }
            }
            return;
        }
        self.saving = true;
        // A read made to see whether the file changed would answer for the text this replaces.
        self.disk_task = None;
        let text = self.editor.read(cx).text(cx);
        let written = text.clone();
        let write = cx.background_spawn(async move {
            let path = match target {
                SaveTarget::File { path, expected } => {
                    // A file gone since is written again; one holding other text is not
                    // written over, nor one back where there was none, whatever it holds.
                    let now = match expected {
                        Expected::Anything => None,
                        Expected::Text(_) | Expected::NoFile => std::fs::read_to_string(&path).ok(),
                    };
                    match (expected, now) {
                        (Expected::Text(expected), Some(now)) if now != expected => {
                            return Ok(Saved::Changed(now));
                        }
                        (Expected::NoFile, Some(now)) => return Ok(Saved::Changed(now)),
                        _ => {}
                    }
                    path
                }
                SaveTarget::New { dir, name } => unused_path(&dir, &name),
            };
            studio_settings::write_atomic(&path, written.as_bytes(), studio_settings::Access::Keep)
                .map(|()| Saved::Written(path))
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = write.await;
            this.update_in(cx, |this, window, cx| {
                this.saving = false;
                match result {
                    Ok(Saved::Written(path)) => {
                        this.path = Some(path);
                        this.saved_text = Some(text);
                        this.disk_change = None;
                        this.file_gone = None;
                        // Typing while it was written stays unsaved.
                        this.sync_unsaved(cx);
                        this.file_error = None;
                        crate::saved_queries_changed(cx);
                    }
                    Ok(Saved::Changed(now)) => match this.file_gone.take() {
                        // The save expected no file and found it back, so nothing was written.
                        // Back as the tab knew it, the save goes on over it; changed elsewhere,
                        // the bar asks.
                        Some(gone) => {
                            this.file_back(gone, now, cx);
                            if let Some(change) = &mut this.disk_change {
                                change.asked = true;
                                this.save_next = None;
                                window.focus(&this.file_focus, cx);
                            } else if this.unsaved {
                                this.save_next.get_or_insert(None);
                            }
                        }
                        None => {
                            // Nothing was written: the draft stays, and the bar asks.
                            this.disk_change = Some(DiskChange {
                                based_on: this.saved_text.as_deref().map(content_hash),
                                asked: true,
                            });
                            this.saved_text = Some(now);
                            this.sync_unsaved(cx);
                            this.save_next = None;
                            window.focus(&this.file_focus, cx);
                        }
                    },
                    Err(error) => {
                        this.file_error = Some(format!("Could not save: {error}"));
                    }
                }
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();
                match this.save_next.take() {
                    Some(Some(path)) => this.write_to(
                        SaveTarget::File {
                            path,
                            expected: Expected::Anything,
                        },
                        window,
                        cx,
                    ),
                    Some(None) => this.save_now(window, cx),
                    None => {}
                }
            })
            .ok();
        })
        .detach();
    }

    /// Replaces the draft, as when a remembered tab comes back. Text given is the tab's, even
    /// empty: the example read never replaces it.
    pub fn set_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.filled_example = true;
        self.editor
            .update(cx, |editor, cx| editor.set_text(text, window, cx));
    }

    fn schema(&self, cx: &App) -> Option<Arc<SchemaModel>> {
        self.session.read(cx).schema_model()
    }

    fn branch(&self, cx: &App) -> String {
        self.session.read(cx).branch.clone()
    }

    /// Fills an untouched, empty editor with a read over the schema's first node type.
    fn set_example(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stored.is_some() || self.filled_example || !self.editor.read(cx).is_empty(cx) {
            return;
        }
        let Some(text) = self.schema(cx).and_then(|s| gq_language::example_read(&s)) else {
            return;
        };
        self.filled_example = true;
        self.editor.update(cx, |editor, cx| {
            editor.set_text(text, window, cx);
            // Put the cursor inside the query, so ⌘↵ runs it straight away.
            let at = gq_language::outline(&editor.text(cx))
                .declarations
                .first()
                .map_or(0, |declaration| declaration.range.start);
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
                s.select_ranges([MultiBufferOffset(at)..MultiBufferOffset(at)])
            });
        });
    }

    // ---- editing ----

    fn edited(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The write waiting to be confirmed is the text as it was; an edit asks again.
        if self.pending_write.take().is_some() {
            cx.notify();
        }
        if self.saved_text.is_some() {
            // The tab's changed-since-saved dot follows the text: the tab and the window
            // around it draw again when it comes or goes, not at every key.
            let was = self.unsaved;
            self.sync_unsaved(cx);
            if self.unsaved != was {
                cx.emit(ItemEvent::UpdateTab);
            }
        }
        // An edit where the server's suggestion goes takes back its Apply.
        self.sync_server_fix(cx);
        self.schedule_check(window, cx);
        self.cursor_moved(window, cx);
    }

    fn cursor_moved(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let before = self.current_place();
        let editor = self.editor.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        self.cursor = editor
            .selections
            .newest_anchor()
            .head()
            .to_offset(&snapshot)
            .0;
        self.sync_params(window, cx);
        // A problem chosen by the keyboard or a click is let go once the cursor leaves it, or
        // moves at all when the problem has no place in the text.
        if let Some(index) = self.selected_problem
            && !self.problem_holds(index, self.cursor, cx)
        {
            self.selected_problem = None;
            self.redraw_problems(cx);
            cx.notify();
        }
        // The problem ⌘. fixes shows the key on its Fix button.
        self.sync_fix_shown(cx);
        // The toolbar and the status bar name the declaration under the cursor, and before
        // a run the result area shows its columns. Only a new declaration notifies: the tab
        // draws again anyway because its editor changed, and a notify on every key would draw
        // the rows again too. Two may share a name, so it is told by its place.
        if self.current_place() != before {
            self.redraw_expected(cx);
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
        }
    }

    /// The declaration under the cursor as the last check placed it: its name and start.
    fn current_place(&self) -> Option<(String, usize)> {
        let (declaration, _) = self.analysis.as_ref()?.query_at(self.cursor)?;
        Some((declaration.name.clone(), declaration.range.start))
    }

    /// Draws the problems list again: what it shows changed.
    fn redraw_problems(&self, cx: &mut Context<Self>) {
        self.problems.update(cx, |_, cx| cx.notify());
    }

    /// Notes the problem ⌘. fixes now, and draws the list again when it is another one.
    fn sync_fix_shown(&mut self, cx: &mut Context<Self>) {
        let now = self.fix_target(cx);
        if now != self.fix_shown {
            self.fix_shown = now;
            self.redraw_problems(cx);
        }
    }

    /// Where problem `index` is in the text now, when it has a place.
    fn problem_place(&self, index: usize, cx: &App) -> Option<std::ops::Range<usize>> {
        let place = self.problem_places.get(index)?.as_ref()?;
        let snapshot = self.snapshot(cx);
        Some(place.start.to_offset(&snapshot).0..place.end.to_offset(&snapshot).0)
    }

    /// Whether `offset` is in problem `index`'s place, its end included.
    fn problem_holds(&self, index: usize, offset: usize, cx: &App) -> bool {
        self.problem_place(index, cx)
            .is_some_and(|place| place.start <= offset && offset <= place.end)
    }

    /// Draws the result area again: what it shows changed. A notify of the tab alone leaves it
    /// as it was (`QueryResults`).
    fn redraw_results(&self, cx: &mut Context<Self>) {
        self.results.update(cx, |_, cx| cx.notify());
    }

    /// Draws the result area again when it shows what the query under the cursor will return,
    /// which follows the cursor, the check and the branch.
    fn redraw_expected(&self, cx: &mut Context<Self>) {
        if matches!(self.output, Output::Empty) {
            self.redraw_results(cx);
        }
    }

    /// Checks the text after typing pauses, off the main thread.
    fn schedule_check(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A check under a schema being read again would be replaced when it is in.
        let Some(schema) = self.schema(cx).filter(|_| self.stored.is_none()) else {
            self.check_task = None;
            return;
        };
        self.check_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(CHECK_DELAY).await;
            let Ok(text) = this.update(cx, |this, cx| this.editor.read(cx).text(cx)) else {
                return;
            };
            let checked = schema.clone();
            let analysis = cx
                .background_spawn(async move { Analysis::new(&checked, text) })
                .await;
            this.update_in(cx, |this, window, cx| {
                // Under another schema now: the check that schema scheduled is coming.
                if this
                    .schema(cx)
                    .is_some_and(|now| Arc::ptr_eq(&now, &schema) || now.source == schema.source)
                {
                    this.apply_analysis(Arc::new(analysis), window, cx);
                }
            })
            .ok();
        }));
    }

    fn apply_analysis(
        &mut self,
        analysis: Arc<Analysis>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A check of older text would underline the wrong places and offer the wrong
        // parameters; a newer one is coming.
        if self.editor.read(cx).text(cx) != analysis.text {
            return;
        }
        let error = Color::Error.color(cx);
        let key = HighlightKey::ConsoleAnsiHighlight(DIAGNOSTIC_KEY);
        self.problem_places = self.editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let places: Vec<Option<std::ops::Range<Anchor>>> = analysis
                .problems
                .iter()
                .map(|(_, range)| {
                    let range = range.as_ref()?;
                    Some(
                        snapshot.anchor_after(MultiBufferOffset(range.start))
                            ..snapshot.anchor_before(MultiBufferOffset(range.end)),
                    )
                })
                .collect();
            let ranges: Vec<_> = places.iter().flatten().cloned().collect();
            if ranges.is_empty() {
                editor.clear_highlights(key, cx);
            } else {
                editor.highlight_text(
                    key,
                    ranges,
                    HighlightStyle {
                        underline: Some(UnderlineStyle {
                            thickness: px(1.),
                            color: Some(error),
                            wavy: true,
                        }),
                        ..Default::default()
                    },
                    cx,
                );
            }
            places
        });
        // The problems are this check's: one chosen from the last is another's now. The list
        // is drawn again only when they are not the last check's, so typing that leaves them
        // as they were leaves the list too.
        let chosen = self.selected_problem.take().is_some();
        let same_problems = self
            .analysis
            .as_ref()
            .is_some_and(|before| before.problems == analysis.problems);
        let expected_before = self
            .current_query()
            .map(|(name, info)| (name.to_owned(), info.cloned()));
        self.analysis = Some(analysis);
        if chosen || !same_problems {
            self.redraw_problems(cx);
        }
        self.sync_fix_shown(cx);
        self.drop_undeclared_params();
        self.sync_params(window, cx);
        self.sync_run_lines(cx);
        // Before a run the result area shows what the declaration under the cursor returns;
        // otherwise a check leaves it alone.
        let expected_now = self
            .current_query()
            .map(|(name, info)| (name.to_owned(), info.cloned()));
        if expected_before != expected_now {
            self.redraw_expected(cx);
        }
        // What the declaration returns, in the status bar, follows the check.
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// Lets go of the input of a parameter no declaration declares any more, once a check read
    /// every declaration: one that does not parse may still declare it, and its value stays.
    fn drop_undeclared_params(&mut self) {
        let Some(analysis) = &self.analysis else {
            return;
        };
        if self.stored.is_some() || !analysis.read_every_declaration() {
            return;
        }
        let declared: collections::HashSet<&str> = analysis
            .queries
            .iter()
            .flatten()
            .flat_map(|info| info.params.iter().map(|(name, ..)| name.as_str()))
            .collect();
        self.params
            .retain(|name, _| declared.contains(name.as_str()));
    }

    /// Puts a run line above each declaration, when the declarations changed since the lines
    /// were placed; anchored to the text, the lines move with edits in between. Lines whose
    /// anchors no longer sit where their declarations start are placed again: text pasted over
    /// them all collapses their anchors to one place, the names unchanged.
    fn sync_run_lines(&mut self, cx: &mut Context<Self>) {
        let Some(analysis) = self.analysis.clone() else {
            return;
        };
        let names: Vec<String> = analysis
            .outline
            .declarations
            .iter()
            .map(|d| d.name.clone())
            .collect();
        // The check is of the text as it is now (`apply_analysis`), so its offsets are this
        // text's.
        let in_place = {
            let snapshot = self.editor.read(cx).buffer().read(cx).snapshot(cx);
            self.run_line_starts
                .iter()
                .map(|at| at.to_offset(&snapshot).0)
                .eq(analysis.outline.declarations.iter().map(|d| d.range.start))
        };
        if names == self.run_line_names && in_place {
            return;
        }
        let this = cx.entity().downgrade();
        let old: collections::HashSet<CustomBlockId> = self.run_lines.drain(..).collect();
        let (lines, starts) = self.editor.update(cx, |editor, cx| {
            editor.remove_blocks(old, None, cx);
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let starts: Vec<Anchor> = analysis
                .outline
                .declarations
                .iter()
                // After: text typed just before a declaration stays above its run line.
                .map(|declaration| {
                    snapshot.anchor_after(MultiBufferOffset(declaration.range.start))
                })
                .collect();
            let blocks: Vec<BlockProperties<editor::Anchor>> = analysis
                .outline
                .declarations
                .iter()
                .zip(&starts)
                .enumerate()
                .map(|(index, (declaration, at))| {
                    let this = this.clone();
                    let name = declaration.name.clone();
                    let at = *at;
                    BlockProperties {
                        placement: BlockPlacement::Above(at),
                        height: Some(1),
                        style: BlockStyle::Flex,
                        priority: 0,
                        render: std::sync::Arc::new(move |block: &mut BlockContext| {
                            run_line(&this, &name, index, block)
                        }),
                    }
                })
                .collect();
            (editor.insert_blocks(blocks, None, cx), starts)
        });
        self.run_lines = lines;
        self.run_line_starts = starts;
        self.run_line_names = names;
    }

    /// The run line's click: run the declaration it sits above, where it is now.
    fn run_declaration(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(start) = self
            .run_line_starts
            .get(index)
            .map(|at| self.offset_of(at, cx))
        else {
            return;
        };
        self.jump_to(start, window, cx);
        // The editor reports the moved cursor later; run what the line sits above now.
        self.cursor = start;
        self.run(&Run, window, cx);
    }

    fn note_run(&mut self, name: Option<&str>, note: RunNote) {
        if let Some(name) = name {
            self.last_runs.insert(name.to_owned(), note);
        }
    }

    // ---- stored queries (spec 007) ----

    /// The stored query this tab runs, when it is a stored query's tab.
    pub fn stored_name(&self) -> Option<&str> {
        self.stored
            .as_ref()
            .map(|stored| stored.entry.name.as_str())
    }

    /// Where the keyboard goes in this tab: the editor; in a stored query's tab its first
    /// parameter, else the results.
    fn main_focus(&self, cx: &App) -> FocusHandle {
        match &self.stored {
            None => self.editor.focus_handle(cx),
            Some(stored) => stored
                .entry
                .params
                .iter()
                .find_map(|param| self.params.get(&param.name))
                .map(|input| input.focus_handle(cx))
                .unwrap_or_else(|| self.results_focus.clone()),
        }
    }

    /// An input for each of the stored query's parameters, kept by name like a query's.
    fn sync_stored_params(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(stored) = &mut self.stored else {
            return;
        };
        for param in stored.entry.params.clone() {
            if self.params.contains_key(&param.name) {
                continue;
            }
            let placeholder = if param.nullable {
                "empty leaves it out"
            } else {
                "value"
            };
            let text = stored
                .pending
                .remove(&param.name)
                .map(|value| gq_language::input::edit_text(&value));
            let input = param_input(placeholder, text, window, cx);
            self.params.insert(param.name, input);
        }
    }

    /// Fills parameters from `values`, a remembered tab's. An input shows its value as the text
    /// a field edits it with (a string as it is), which the input reads back. A parameter with
    /// no input yet, as before the catalog is read, takes its value when its input is made.
    pub fn fill_params(
        &mut self,
        values: &Map<String, Value>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for (name, value) in values {
            match self.params.get(name) {
                Some(input) => {
                    let text = gq_language::input::edit_text(value);
                    input.update(cx, |input, cx| input.set_text(text, window, cx));
                }
                None => {
                    if let Some(stored) = &mut self.stored {
                        stored.pending.insert(name.clone(), value.clone());
                    }
                }
            }
        }
        cx.notify();
    }

    /// The graph's catalog was read again: the tab takes its stored query's parameters as they
    /// are now, or says the graph no longer serves it.
    fn catalog_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.stored_name().map(str::to_owned) else {
            return;
        };
        let session = self.session.read(cx);
        if session.stored_queries.loaded().is_none() {
            return;
        }
        let entry = session.stored_query(&name);
        if let Some(stored) = &mut self.stored {
            stored.gone = entry.is_none();
            if let Some(entry) = entry {
                stored.entry = entry;
            }
        }
        self.sync_stored_params(window, cx);
        // Before a run, the result area says whether the query writes.
        self.redraw_expected(cx);
        cx.notify();
    }

    fn prepare_stored(&self, stored: &Stored, explain: bool, cx: &App) -> Result<Prepared, String> {
        let name = &stored.entry.name;
        if explain {
            return Err(format!(
                "Explain needs a query's text, and the server keeps {name}'s to itself."
            ));
        }
        if stored.gone {
            return Err(format!(
                "This graph no longer serves the stored query {name}."
            ));
        }
        let mut values = Map::new();
        for param in &stored.entry.params {
            let text = self
                .params
                .get(&param.name)
                .map(|input| input.read(cx).text(cx))
                .unwrap_or_default();
            // An empty optional parameter is left out, which the catalog says it may be.
            match gq_language::param_value(&param.name, &param_type(param), param.nullable, &text) {
                Ok(Some(value)) => {
                    values.insert(param.name.clone(), value);
                }
                Ok(None) => {}
                Err(error) => {
                    return Err(format!(
                        "{error}. Fill in the parameters above, then run again."
                    ));
                }
            }
        }
        Ok(Prepared {
            text: String::new(),
            name: Some(name.clone()),
            label: name.clone(),
            write: stored.entry.mutation,
            statement: false,
            params: (!values.is_empty()).then_some(Value::Object(values)),
            columns: None,
            map: SourceMap::default(),
            sent: None,
            stored: Some(name.clone()),
            // The server keeps its text: what it deletes, or whether it is held, is not known
            // here.
            destroys: None,
            publication: Publication::Branch,
            page: 0,
        })
    }

    /// The parameters the form shows, `(name, type, nullable)`: the stored query's, else the
    /// declaration's under the cursor.
    fn shown_params(&self) -> Option<Vec<(String, String, bool)>> {
        match &self.stored {
            Some(stored) => Some(
                stored
                    .entry
                    .params
                    .iter()
                    .map(|param| (param.name.clone(), param_type(param), param.nullable))
                    .collect(),
            ),
            None => {
                let (_, Some(info)) = self.current_query()? else {
                    return None;
                };
                Some(info.params.clone())
            }
        }
    }

    /// What the catalog says about a stored query, under the toolbar that names it: its
    /// description, its instruction, and whether the graph still serves it. Nothing when it
    /// says nothing.
    fn render_stored_about(&self, stored: &Stored, cx: &mut Context<Self>) -> Option<AnyElement> {
        use studio_ui::ds::{self, space};
        let entry = &stored.entry;
        let mut facts = Vec::new();
        if stored.gone {
            facts.push(
                h_flex()
                    .gap(px(space::XS))
                    .child(studio_ui::ds::icon::meta(IconName::Warning).color(Color::Warning))
                    .child(ds::text::meta("This graph no longer serves it"))
                    .into_any_element(),
            );
        }
        if entry.description.is_none() && entry.instruction.is_none() && facts.is_empty() {
            return None;
        }
        Some(
            v_flex()
                .flex_none()
                .px(px(space::EDGE))
                .py(px(space::LG))
                .gap(px(space::XS))
                .border_b_1()
                .border_color(ds::surface::rule(cx))
                .children(entry.description.clone().map(ds::text::body))
                .children(entry.instruction.clone().map(ds::text::muted))
                .when(!facts.is_empty(), |column| {
                    column.child(ds::meta_row(facts))
                })
                .into_any_element(),
        )
    }

    /// The declaration under the cursor and what the last check said about it.
    fn current_query(&self) -> Option<(&str, Option<&QueryInfo>)> {
        let analysis = self.analysis.as_ref()?;
        let (declaration, info) = analysis.query_at(self.cursor)?;
        Some((declaration.name.as_str(), info))
    }

    /// Makes an input for each parameter of the query under the cursor. Inputs are kept by
    /// name, so a value survives edits and moving between queries.
    fn sync_params(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, Some(info))) = self.current_query() else {
            return;
        };
        let missing: Vec<(String, bool)> = info
            .params
            .iter()
            .filter(|(name, ..)| !self.params.contains_key(name))
            .map(|(name, _, nullable)| (name.clone(), *nullable))
            .collect();
        for (name, nullable) in missing {
            let placeholder = if nullable { "empty is null" } else { "value" };
            let input = param_input(placeholder, None, window, cx);
            self.params.insert(name, input);
        }
    }

    fn snapshot(&self, cx: &App) -> MultiBufferSnapshot {
        self.editor.read(cx).buffer().read(cx).snapshot(cx)
    }

    /// Holds where the diagnostic of the output shown points, and its suggested edits,
    /// anchored in the text the run sent (`sent`, else the text now), so a jump to it lands
    /// there, and its edits where they were meant, after any edit.
    fn hold_diagnostic(&mut self, sent: Option<&MultiBufferSnapshot>, cx: &App) {
        let diagnostic = match &self.output {
            Output::Failed(Failure {
                diagnostic: Some(diagnostic),
                ..
            })
            | Output::Written(WriteResult::Refused {
                diagnostic: Some(diagnostic),
                ..
            }) => Some(diagnostic),
            _ => None,
        };
        let byte = diagnostic.and_then(|diagnostic| diagnostic.byte);
        let edits = diagnostic.map_or(&[][..], |diagnostic| diagnostic.edits.as_slice());
        let now = sent.is_none().then(|| self.snapshot(cx));
        let Some(snapshot) = sent.or(now.as_ref()) else {
            return;
        };
        let clip = |byte: usize, bias: Bias| {
            snapshot.clip_offset(MultiBufferOffset(byte.min(snapshot.len().0)), bias)
        };
        self.diagnostic_at = byte.map(|byte| snapshot.anchor_before(clip(byte, Bias::Left)));
        self.diagnostic_edits = edits
            .iter()
            .map(|(range, replacement)| {
                let (start, end) = (clip(range.start, Bias::Left), clip(range.end, Bias::Right));
                let around = clip(start.0.saturating_sub(EDIT_CONTEXT), Bias::Left)
                    ..clip(end.0 + EDIT_CONTEXT, Bias::Right);
                HeldEdit {
                    context: snapshot.anchor_after(around.start)
                        ..snapshot.anchor_before(around.end),
                    original: snapshot.text_for_range(around.clone()).collect(),
                    within: start.0 - around.start.0..end.0 - around.start.0,
                    replacement: replacement.clone(),
                }
            })
            .collect();
        self.server_fix_holds = self.server_fix_place(cx).is_some();
    }

    /// Where the server's suggested edits go in the text now: `None` when there are none, or
    /// when one's context ([`HeldEdit`]) no longer reads as it did, so `()` typed where the
    /// server would insert `()` is not doubled. The compiler's fixes ask the same of the whole
    /// text ([`Self::apply_fix`]); these are held across edits elsewhere, so each asks it of
    /// the text around its own place.
    fn server_fix_place(
        &self,
        cx: &App,
    ) -> Option<Vec<(std::ops::Range<MultiBufferOffset>, String)>> {
        if self.diagnostic_edits.is_empty() || self.stored.is_some() {
            return None;
        }
        let snapshot = self.snapshot(cx);
        self.diagnostic_edits
            .iter()
            .map(|edit| {
                let start = edit.context.start.to_offset(&snapshot);
                let end = edit.context.end.to_offset(&snapshot);
                let holds = start <= end
                    && studio_ui::text::pieces_are(
                        end.0 - start.0,
                        snapshot.text_for_range(start..end),
                        &edit.original,
                    );
                holds.then(|| {
                    (
                        MultiBufferOffset(start.0 + edit.within.start)
                            ..MultiBufferOffset(start.0 + edit.within.end),
                        edit.replacement.clone(),
                    )
                })
            })
            .collect()
    }

    /// Whether Apply is offered changed with an edit: the results say so.
    fn sync_server_fix(&mut self, cx: &mut Context<Self>) {
        if self.diagnostic_edits.is_empty() && !self.server_fix_holds {
            return;
        }
        let holds = self.server_fix_place(cx).is_some();
        if holds != self.server_fix_holds {
            self.server_fix_holds = holds;
            self.redraw_results(cx);
        }
    }

    /// Applies the server's suggested edits, where their anchors are now, while their places
    /// hold the text they were made for.
    fn apply_server_fix(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut placed) = self.server_fix_place(cx) else {
            return;
        };
        self.diagnostic_edits.clear();
        self.server_fix_holds = false;
        placed.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
        self.editor.update(cx, |editor, cx| editor.edit(placed, cx));
        window.focus(&self.editor_focus, cx);
        self.redraw_results(cx);
        cx.notify();
    }

    fn offset_of(&self, anchor: &Anchor, cx: &App) -> usize {
        let snapshot = self.editor.read(cx).buffer().read(cx).snapshot(cx);
        anchor.to_offset(&snapshot).0
    }

    /// Puts the cursor at `offset`, kept within the text and on a character's boundary.
    fn jump_to(&mut self, offset: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let offset = MultiBufferOffset(offset.min(snapshot.len().0));
            let offset = snapshot.clip_offset(offset, Bias::Left).0;
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::center()),
                window,
                cx,
                |s| s.select_ranges([MultiBufferOffset(offset)..MultiBufferOffset(offset)]),
            );
        });
        let focus = self.editor.focus_handle(cx);
        window.focus(&focus, cx);
    }

    // ---- running ----

    fn prepare(&self, explain: bool, cx: &App) -> Result<Prepared, String> {
        if let Some(stored) = &self.stored {
            return self.prepare_stored(stored, explain, cx);
        }
        let source = self.editor.read(cx).text(cx);
        let outline = gq_language::outline(&source);
        if let Some(statement) = outline.statement {
            if explain {
                return Err("Explain shows the plan of a query, not of a statement.".into());
            }
            if statement.targets_branch()
                && let Some(commit) = &self.at_commit
            {
                return Err(format!(
                    "A load goes to a branch head, and this tab reads commit {}. Open a new \
                     query to load into a branch.",
                    short_commit_id(commit)
                ));
            }
            // The whole statement, on one line: its target can be on a line of its own, and the
            // confirm bar names what runs.
            let label = one_line_label(&source[outline.prefix.end..]);
            let write = statement.writes();
            return Ok(Prepared {
                // Parsing a statement needs no schema, and a statement is a line or two.
                destroys: write
                    .then(|| gq_language::check::statement_destroys(&source))
                    .flatten(),
                publication: gq_language::check::statement_publication(&source),
                // A diff answers a page at a time; this asks for its first.
                page: usize::from(is_branch_diff(&source)),
                text: source.clone(),
                name: None,
                label,
                write,
                // A load, and the plan of one, go to the request's branch, as a mutation does;
                // the others name their own targets.
                statement: !statement.targets_branch(),
                params: None,
                columns: None,
                // Past the end, so the end itself is a place in the statement.
                map: SourceMap {
                    prefix_len: source.len() + 1,
                    ..Default::default()
                },
                sent: Some(self.snapshot(cx)),
                stored: None,
            });
        }
        let declaration = outline
            .declaration_at(self.cursor)
            .ok_or("There is no query to run. Write one, for example `query q() { … }`.")?;
        let explained = explain || declaration.explained;
        let text = if explained {
            outline.explain_text(&source, declaration)
        } else {
            outline.runnable_text(&source, declaration)
        };
        // The check held answers for this declaration when it is of this text and schema; else
        // it is checked now (typed within the last `CHECK_DELAY`, or a schema read since).
        let info = self.schema(cx).and_then(|schema| {
            match self
                .analysis
                .as_ref()
                .filter(|analysis| analysis.is_of(&source, &schema))
            {
                Some(analysis) => analysis.query_of(declaration).cloned(),
                None => gq_language::check(&schema, &text)
                    .queries
                    .into_iter()
                    .find(|q| q.name == declaration.name),
            }
        });
        let mutation = info
            .as_ref()
            .map(|q| q.is_mutation)
            .unwrap_or_else(|| gq_language::looks_like_mutation(&text));
        if explained && mutation {
            return Err("Explain shows the plan of a read; this query changes data.".into());
        }
        let params = match &info {
            Some(info) => self.param_values(info, cx).map_err(|error| {
                format!("{error}. Fill in the parameters above, then run again.")
            })?,
            None => None,
        };
        let label = if declaration.name.is_empty() {
            "query".to_owned()
        } else {
            declaration.name.clone()
        };
        let (columns, destroys, publication) = match info {
            Some(info) => (Some(info.columns), info.destroys, info.publication),
            // Unchecked (no schema yet): where it publishes and what it destroys are in its
            // parse alone, so a destructive write still waits for its name typed.
            None => {
                let declared = gq_language::check::declared_write(&text, &declaration.name);
                (None, declared.destroys, declared.publication)
            }
        };
        Ok(Prepared {
            text,
            name: (!declaration.name.is_empty()).then(|| declaration.name.clone()),
            label,
            write: mutation,
            statement: false,
            params,
            columns,
            map: SourceMap {
                prefix_len: outline.prefix.end,
                inserted: if explained { "explain ".len() } else { 0 },
                start: declaration.range.start,
            },
            sent: Some(self.snapshot(cx)),
            stored: None,
            destroys,
            publication,
            page: 0,
        })
    }

    fn param_values(&self, info: &QueryInfo, cx: &App) -> Result<Option<Value>, String> {
        if info.params.is_empty() {
            return Ok(None);
        }
        let mut values = Map::new();
        for (name, ty, nullable) in &info.params {
            let text = self
                .params
                .get(name)
                .map(|input| input.read(cx).text(cx))
                .unwrap_or_default();
            let value = gq_language::param_value(name, ty, *nullable, &text)?;
            values.insert(name.clone(), value.unwrap_or(Value::Null));
        }
        Ok(Some(Value::Object(values)))
    }

    fn run(&mut self, _: &Run, window: &mut Window, cx: &mut Context<Self>) {
        if self.writing {
            return;
        }
        // A read still out from an earlier run would land over what this one shows; a read
        // this one sends replaces it.
        self.run_task = None;
        match self.prepare(false, cx) {
            Err(message) => self.replace_output(Output::Failed(Failure::new(message)), cx),
            Ok(prepared) if prepared.write && self.at_commit.is_some() => {
                let commit = self
                    .at_commit
                    .as_deref()
                    .map(short_commit_id)
                    .unwrap_or_default();
                self.replace_output(
                    Output::Failed(Failure::new(format!(
                        "Read-only: this query reads commit {commit}. Open a new query to write to \
                         a branch."
                    ))),
                    cx,
                );
            }
            Ok(prepared) if prepared.write => {
                // A read stopped for it says nothing more.
                if matches!(self.output, Output::Running { .. }) {
                    self.replace_output(Output::Empty, cx);
                }
                // Never on ⌘↵ alone: the person confirms the branch first, and what it
                // destroys by typing its name when Studio cannot put it back.
                let branch = self.branch(cx);
                let typed = prepared
                    .destroys
                    .as_ref()
                    .map(|destroys| self.typed_confirm(destroys, &prepared, &branch, window, cx));
                match &typed {
                    Some(typed) => {
                        let focus = typed.field.focus_handle(cx);
                        window.focus(&focus, cx);
                    }
                    None => window.focus(&self.confirm_focus, cx),
                }
                self.pending_write = Some(PendingWrite {
                    prepared,
                    branch,
                    typed,
                });
            }
            Ok(prepared) => {
                self.pending_write = None;
                self.send_read(prepared, cx);
            }
        }
        // The status bar shows the run.
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// What a destructive write destroys, said in full, and the words typed to confirm it: the
    /// name or key of what goes (constitution IV, as a delete Undo cannot restore is confirmed
    /// in a table).
    fn typed_confirm(
        &self,
        destroys: &Destroys,
        prepared: &Prepared,
        branch: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TypedConfirm {
        let schema = self.schema(cx);
        let is_node = |type_name: &str| {
            schema
                .as_ref()
                .is_none_or(|schema| schema.node(type_name).is_some())
        };
        let (what, phrase) = match destroys {
            Destroys::Branch(name) => (
                format!("Deletes branch {name}, with its commits no other branch holds."),
                name.clone(),
            ),
            Destroys::Commit(id) => (
                format!(
                    "Drops the unpublished commit {}, which was never on a branch.",
                    short_commit_id(id)
                ),
                short_commit_id(id).to_string(),
            ),
            Destroys::Overwrite { type_name, edge } => (
                format!(
                    "Replaces every {type_name} {} on {branch} with the file's.",
                    if *edge { "relationship" } else { "node" }
                ),
                type_name.clone(),
            ),
            Destroys::Rows(types) => {
                let names = match types.split_last() {
                    Some((last, [])) => last.clone(),
                    Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
                    None => String::new(),
                };
                let relationships = if types.iter().any(|t| is_node(t)) {
                    ", with their relationships"
                } else {
                    ""
                };
                let phrase = match types.as_slice() {
                    [one] => one.clone(),
                    _ => prepared.label.clone(),
                };
                (
                    format!(
                        "Deletes what its `where` matches of {names} from {branch}{relationships}."
                    ),
                    phrase,
                )
            }
        };
        let field = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(&phrase, window, cx);
            editor.set_text_style_refinement(studio_ui::ds::field_text(cx));
            editor
        });
        TypedConfirm {
            what: format!("{what} Studio cannot put it back. Type {phrase} to confirm."),
            phrase,
            field,
            refused: false,
        }
    }

    fn explain(&mut self, _: &Explain, _window: &mut Window, cx: &mut Context<Self>) {
        // A write's outcome stays on screen until it is in, as Run waits for it.
        if self.writing {
            return;
        }
        self.run_task = None;
        match self.prepare(true, cx) {
            Err(message) => self.replace_output(Output::Failed(Failure::new(message)), cx),
            Ok(prepared) => self.send_read(prepared, cx),
        }
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn send_read(&mut self, prepared: Prepared, cx: &mut Context<Self>) {
        let branch = self.branch(cx);
        let request = QueryRequest {
            query: prepared.text.clone(),
            name: prepared.name.clone(),
            params: prepared.params.clone(),
            // A statement names its own branches; the server refuses a target with it. A query
            // pinned to a commit reads that snapshot instead of the branch.
            branch: (!prepared.statement && self.at_commit.is_none()).then(|| branch.clone()),
            snapshot: self.at_commit.clone().filter(|_| !prepared.statement),
        };
        // Named by what is sent: a statement goes without the tab's branch or pinned commit,
        // and reads what it names itself.
        let read_at = match (&self.at_commit, prepared.statement) {
            (_, true) => ReadAt::Statement,
            (Some(commit), false) => ReadAt::Commit(commit.clone()),
            (None, false) => ReadAt::Branch(branch),
        };
        let started = Instant::now();
        let task = match &prepared.stored {
            Some(name) => {
                let (params, snapshot) = (prepared.params.clone(), self.at_commit.clone());
                self.session.update(cx, |session, cx| {
                    session.invoke_stored_read(name, params, snapshot, cx)
                })
            }
            None => self
                .session
                .update(cx, |session, cx| session.read_request(request, cx)),
        };
        let label = match prepared.page {
            page if page > 1 => format!("Reading page {page} of {}…", prepared.label),
            _ => format!("Running {}{}…", prepared.label, read_at.suffix()),
        };
        self.replace_output(
            Output::Running {
                label: label.into(),
            },
            cx,
        );
        self.selected = None;
        let schema = self.session.read(cx).schema_model();
        // The head the session knew when the read went: one it learns while the read is out is
        // compared with the commit the answer was read at.
        let head_before = self
            .session
            .read(cx)
            .head(&self.session.read(cx).branch)
            .map(str::to_owned);
        // Replacing the task cancels an earlier read still in flight.
        self.run_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let elapsed = started.elapsed();
            // The rows are built off the main thread: a large answer would hold up frames.
            let columns = prepared.columns.clone();
            let answer = cx
                .background_spawn(async move { Answer::of(result, columns.as_deref(), schema) })
                .await;
            this.update(cx, |this, cx| {
                // A new output replaces the plan whose operator lit the text.
                this.light_plan_parts(None, cx);
                let sent = prepared.sent.clone();
                this.show_read(prepared, read_at, answer, elapsed);
                this.hold_diagnostic(sent.as_ref(), cx);
                this.rows_against_head(head_before, cx);
                cx.emit(ItemEvent::UpdateTab);
                this.run_task = None;
                this.redraw_results(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// A read's answer built and shown in one step, as a run does across threads.
    #[cfg(test)]
    fn finish_read(
        &mut self,
        prepared: Prepared,
        read_at: ReadAt,
        result: Result<ReadOutput, ReadError>,
        elapsed: Duration,
        schema: Option<Arc<SchemaModel>>,
    ) {
        let answer = Answer::of(result, prepared.columns.as_deref(), schema);
        self.show_read(prepared, read_at, answer, elapsed);
    }

    /// Shows a read's answer, its rows already built ([`BuiltRows`]).
    fn show_read(&mut self, prepared: Prepared, sent: ReadAt, answer: Answer, elapsed: Duration) {
        let note = match &answer {
            Answer::Plan(output) | Answer::Rows(output, _) => RunNote {
                text: format!(
                    "{} · {}",
                    rows_label(output.row_count),
                    format_elapsed(elapsed)
                ),
                failed: false,
            },
            Answer::Refused(ReadError::TooLarge { .. }) => RunNote {
                text: "Too large · add a limit".into(),
                failed: true,
            },
            Answer::Refused(_) => RunNote {
                text: "Refused".into(),
                failed: true,
            },
        };
        self.note_run(prepared.name.as_deref(), note);
        self.output = match answer {
            Answer::Plan(output) => Output::Plan(PlanOutput {
                name: prepared.name.clone(),
                label: prepared.label,
                items: plan_items(&output.rows),
                elapsed,
                commit: output.graph_commit_id,
            }),
            Answer::Rows(output, built) => {
                let BuiltRows {
                    table,
                    widths,
                    right_aligned,
                    graph,
                    shown,
                } = built;
                // A width the person dragged a column to wins over the one from its content.
                let widths = widths
                    .into_iter()
                    .zip(&table.columns)
                    .map(|(width, column)| self.column_widths.get(column).copied().unwrap_or(width))
                    .collect();
                let to_branch = matches!(sent, ReadAt::Branch(_));
                // A statement's answer names no branch or snapshot of the tab's; a read's names
                // where it read, else it read where it was sent.
                let read_at = match sent {
                    ReadAt::Statement => ReadAt::Statement,
                    sent => match (output.target.branch, output.target.snapshot) {
                        (Some(branch), _) => ReadAt::Branch(branch),
                        (None, Some(commit)) => ReadAt::Commit(commit),
                        (None, None) => sent,
                    },
                };
                // The same statement, continued where the server says this page stops.
                let next = output
                    .page
                    .as_ref()
                    .and_then(|page| page.next_page_token.as_deref())
                    .filter(|_| prepared.page > 0)
                    .and_then(|token| {
                        Some(Prepared {
                            text: diff_page_text(&prepared.text, token)?,
                            page: prepared.page + 1,
                            ..prepared.clone()
                        })
                    });
                // A read of the branch head is read again by Refresh once the branch moves;
                // a statement names its own branches, and a commit never moves.
                let rerun = (to_branch && matches!(read_at, ReadAt::Branch(_)) && !prepared.write)
                    .then(|| prepared.clone());
                Output::Rows(Box::new(RowsOutput {
                    label: prepared.label,
                    graph,
                    table,
                    widths,
                    right_aligned,
                    shown,
                    row_count: output.row_count,
                    elapsed,
                    read_at,
                    commit: output.graph_commit_id,
                    page: output.page,
                    page_number: prepared.page,
                    next,
                    rerun,
                    newer: false,
                    head_seen: None,
                }))
            }
            Answer::Refused(error) => {
                Output::Failed(Failure::from_read(&error, prepared.map, &prepared.text))
            }
        };
        self.results_scroll.scroll_to_item(0, ScrollStrategy::Top);
    }

    /// The rows just shown against the head of their branch: a head the session learned while
    /// the read was out, other than the commit the answer was read at, is a newer one.
    fn rows_against_head(&mut self, head_before: Option<String>, cx: &mut Context<Self>) {
        let session = self.session.read(cx);
        let Output::Rows(rows) = &mut self.output else {
            return;
        };
        let ReadAt::Branch(branch) = &rows.read_at else {
            return;
        };
        let now = session.head(branch).map(str::to_owned);
        rows.head_seen = now.clone();
        if let (Some(now), Some(commit)) = (now, rows.commit.clone())
            && Some(&now) != head_before.as_ref()
            && now != commit
        {
            let branch = branch.clone();
            self.head_moved(&branch, &now, cx);
        }
    }

    /// The rows against the head the session knows for their branch now, when the session
    /// moves to a branch: a head that is neither the commit they were read at nor the one the
    /// session knew when they were shown is newer. One the session learns later comes as
    /// [`SessionEvent::HeadMoved`].
    fn compare_rows_with_head(&mut self, cx: &mut Context<Self>) {
        let Output::Rows(rows) = &self.output else {
            return;
        };
        let (ReadAt::Branch(branch), Some(commit)) = (&rows.read_at, &rows.commit) else {
            return;
        };
        let Some(head) = self.session.read(cx).head(branch).map(str::to_owned) else {
            return;
        };
        if head != *commit && Some(&head) != rows.head_seen.as_ref() {
            let branch = branch.clone();
            self.head_moved(&branch, &head, cx);
        }
    }

    /// `branch`'s head is now `head`: rows read at the branch at another commit have a newer
    /// one, which the tab offers to read while the branch is the session's (constitution II).
    /// Nothing is read again until the person asks, and a write's outcome is never run again.
    fn head_moved(&mut self, branch: &str, head: &str, cx: &mut Context<Self>) {
        let Output::Rows(rows) = &mut self.output else {
            return;
        };
        let read_here = matches!(&rows.read_at, ReadAt::Branch(read) if read == branch);
        if rows.newer || rows.rerun.is_none() || !read_here || rows.commit.as_deref() == Some(head)
        {
            return;
        }
        rows.newer = true;
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// ⌘R, or Newer data · Refresh: reads the rows shown again at the branch head, as they
    /// were read, and goes on to the workspace, which reads the graph's schema, branches and
    /// counts again. A write is never sent again.
    fn refresh(&mut self, _: &Refresh, _window: &mut Window, cx: &mut Context<Self>) {
        cx.propagate();
        self.refresh_rows(cx);
    }

    fn refresh_rows(&mut self, cx: &mut Context<Self>) {
        if self.writing {
            return;
        }
        // Sent to the session's branch, as Run is: rows of another branch are read again
        // there, once the session is back on it.
        let branch = self.session.read(cx).branch.clone();
        let Some(rerun) = (match &self.output {
            Output::Rows(rows) if matches!(&rows.read_at, ReadAt::Branch(read) if *read == branch) => {
                rows.rerun.clone()
            }
            _ => None,
        }) else {
            return;
        };
        self.run_task = None;
        self.send_read(rerun, cx);
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// Reads the next page of a paged result (`branch diff`): the same statement again, from
    /// where the server said this page stops.
    fn next_page(&mut self, _: &NextPage, _window: &mut Window, cx: &mut Context<Self>) {
        // A write's outcome stays on screen until it is in, as Run waits for it.
        if self.writing {
            return;
        }
        let Some(next) = (match &self.output {
            Output::Rows(rows) => rows.next.clone(),
            _ => None,
        }) else {
            return;
        };
        self.run_task = None;
        self.send_read(next, cx);
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn confirm_write(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        // A write Studio cannot put back goes only with its name typed: anything else keeps it
        // waiting, and says what to type.
        if let Some(typed) = self
            .pending_write
            .as_mut()
            .and_then(|pending| pending.typed.as_mut())
            && typed.field.read(cx).text(cx).trim() != typed.phrase
        {
            typed.refused = true;
            cx.notify();
            return;
        }
        let Some(PendingWrite {
            prepared, branch, ..
        }) = self.pending_write.take()
        else {
            return;
        };
        // What would be sent now must be what the bar asked about: a parameter typed, a branch
        // switched or a pin since then sends nothing, and Run asks again.
        let now = self.prepare(false, cx).ok();
        let same = now.as_ref().is_some_and(|now| {
            now.text == prepared.text
                && now.params == prepared.params
                && now.stored == prepared.stored
        }) && self.at_commit.is_none()
            && self.branch(cx) == branch;
        if !same {
            self.replace_output(
                Output::Failed(Failure::new(
                    "The query, its parameters or its branch changed after Run; nothing was \
                     written. Run it again to confirm the write as it is now."
                        .to_owned(),
                )),
                cx,
            );
            let focus = self.main_focus(cx);
            window.focus(&focus, cx);
            cx.notify();
            return;
        }
        let request = ChangeRequest {
            query: prepared.text.clone(),
            name: prepared.name.clone(),
            params: prepared.params.clone(),
            branch: (!prepared.statement).then(|| branch.clone()),
        };
        let description = match &prepared.stored {
            Some(name) => format!("Run stored query {name}"),
            None => format!("Run {}", prepared.label),
        };
        let started = Instant::now();
        // No read precondition: the person confirmed this write against the branch head.
        let task = match &prepared.stored {
            Some(name) => {
                let params = prepared.params.clone();
                let branch = branch.clone();
                self.session.update(cx, |session, cx| {
                    session.invoke_stored_write(name, params, branch, None, description, cx)
                })
            }
            None => self.session.update(cx, |session, cx| {
                session.write_request(request, None, description, cx)
            }),
        };
        self.writing = true;
        // A read still out would land over the write's outcome.
        self.run_task = None;
        self.replace_output(
            Output::Running {
                label: format!("Writing {} to {branch}…", prepared.label).into(),
            },
            cx,
        );
        let (map, sent) = (prepared.map, prepared.sent.clone());
        let sent_text = prepared.text.clone();
        let name = prepared.name.clone();
        // Detached, not stored: closing the tab must not cancel a write in flight, and the
        // session logs its outcome either way (constitution III).
        cx.spawn(async move |this, cx| {
            let outcome = task.await;
            let elapsed = started.elapsed();
            this.update(cx, |this, cx| {
                this.writing = false;
                let note = match &outcome {
                    WriteOutcome::Committed(output) => RunNote {
                        text: WriteEffect::of(output)
                            .note(output.affected_nodes, output.affected_edges),
                        failed: false,
                    },
                    WriteOutcome::Refused(_) => RunNote {
                        text: "Refused · nothing written".into(),
                        failed: true,
                    },
                    WriteOutcome::Unknown { .. } => RunNote {
                        text: "Outcome unknown · check History".into(),
                        failed: true,
                    },
                };
                this.note_run(name.as_deref(), note);
                cx.emit(ItemEvent::UpdateTab);
                let written = Output::Written(match outcome {
                    WriteOutcome::Committed(output) => WriteResult::Done {
                        effect: WriteEffect::of(&output),
                        // A branch or commit statement moves refs and reports no counts.
                        counts: output
                            .outcome
                            .is_none()
                            .then_some((output.affected_nodes, output.affected_edges)),
                        elapsed,
                    },
                    WriteOutcome::Refused(error) => WriteResult::Refused {
                        status: error.status,
                        diagnostic: error
                            .body
                            .diagnostic
                            .as_ref()
                            .and_then(|d| server_diagnostic(d, &sent_text, map)),
                        message: error.body.error,
                    },
                    WriteOutcome::Unknown { reason } => WriteResult::Unknown { reason },
                });
                this.replace_output(written, cx);
                this.hold_diagnostic(sent.as_ref(), cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
        let focus = self.main_focus(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn cancel_write(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_write = None;
        let focus = self.main_focus(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    // ---- results ----

    fn select_cell(
        &mut self,
        row: usize,
        column: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let first = self.selected.is_none();
        self.selected = Some((row, column));
        window.focus(&self.results_focus, cx);
        if first {
            // The status bar offers copying once a cell is selected.
            cx.emit(ItemEvent::UpdateTab);
        }
        self.redraw_results(cx);
        cx.notify();
    }

    /// Moves the selected cell `rows` rows down and `columns` columns right, within the
    /// result; with nothing selected yet, the first step lands on the first cell.
    fn move_selection(&mut self, rows: isize, columns: isize, cx: &mut Context<Self>) {
        let Output::Rows(output) = &self.output else {
            return;
        };
        let (count, width) = (output.table.rows.len(), output.table.columns.len());
        if count == 0 || width == 0 {
            return;
        }
        let first = self.selected.is_none();
        let (row, column) = match self.selected {
            None => (0, 0),
            Some((row, column)) => (
                (row as isize + rows).clamp(0, count as isize - 1) as usize,
                (column as isize + columns).clamp(0, width as isize - 1) as usize,
            ),
        };
        self.selected = Some((row, column));
        self.results_scroll
            .scroll_to_item(row, ScrollStrategy::Nearest);
        self.scroll_column_into_view(column);
        if first {
            // The status bar offers copying once a cell is selected.
            cx.emit(ItemEvent::UpdateTab);
        }
        self.redraw_results(cx);
        cx.notify();
    }

    /// Scrolls the results sideways so result column `column` is in view, at the widths drawn
    /// (fitted, or dragged); the row numbers scroll with the columns.
    fn scroll_column_into_view(&self, column: usize) {
        let Output::Rows(rows) = &self.output else {
            return;
        };
        let left = px(ROW_NUMBER_WIDTH + rows.widths[..column].iter().sum::<f32>());
        let right = left + px(rows.widths[column]);
        let handle = &self.results_hscroll;
        let view = handle.bounds().size.width;
        if view <= px(0.) {
            return;
        }
        let offset = handle.offset();
        let shown = -offset.x;
        // The first column brings the row numbers back with it.
        let left = if column == 0 { px(0.) } else { left };
        if left < shown {
            handle.set_offset(gpui::point(-left, offset.y));
        } else if right > shown + view {
            handle.set_offset(gpui::point(-(right - view), offset.y));
        }
    }

    /// Moves the keyboard to the results, or back to the query from them.
    fn toggle_results_focus(
        &mut self,
        _: &ToggleResultsFocus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.results_focus.is_focused(window) {
            let focus = self.main_focus(cx);
            window.focus(&focus, cx);
        } else if matches!(self.output, Output::Rows(_)) {
            window.focus(&self.results_focus, cx);
            // The first cell is selected, so the keys that copy and move have one to act on.
            if self.selected.is_none() {
                self.move_selection(0, 0, cx);
            }
        }
    }

    fn copy_cell(&mut self, _: &CopyCell, _: &mut Window, cx: &mut Context<Self>) {
        if let (Output::Rows(rows), Some((row, column))) = (&self.output, self.selected)
            && let Some(value) = rows.table.rows.get(row).and_then(|r| r.get(column))
        {
            cx.write_to_clipboard(ClipboardItem::new_string(value_text(value)));
        }
    }

    fn copy_row(&mut self, _: &CopyRow, _: &mut Window, cx: &mut Context<Self>) {
        if let (Output::Rows(rows), Some((row, _))) = (&self.output, self.selected)
            && let Some(json) = rows.table.row_json(row)
        {
            cx.write_to_clipboard(ClipboardItem::new_string(json));
        }
    }

    // ---- rendering ----

    fn status_parts(&self, cx: &mut Context<Self>) -> ItemStatus {
        use studio_ui::ds;
        let mut start = Vec::new();
        let mut end = Vec::new();
        // The editor's keys, then the result's: the status bar lists them behind its keyboard
        // button.
        let editor_focus = self.editor.focus_handle(cx);
        let mut keys = vec![
            KeyHint::new("Run the query under the cursor", &Run, &editor_focus),
            KeyHint::new("Show its plan", &Explain, &editor_focus),
            KeyHint::new("Save", &Save, &editor_focus),
            KeyHint::new("Save as…", &SaveAs, &editor_focus),
        ];
        if self.stored.is_none()
            && self
                .analysis
                .as_ref()
                .is_some_and(|analysis| !analysis.problems.is_empty())
        {
            keys.push(KeyHint::new(
                "Go to the next problem",
                &NextProblem,
                &editor_focus,
            ));
            keys.push(KeyHint::new(
                "Go to the previous problem",
                &PreviousProblem,
                &editor_focus,
            ));
            if self.fix_target(cx).is_some() {
                keys.push(KeyHint::new(
                    "Apply the suggested fix",
                    &ApplyFix,
                    &editor_focus,
                ));
            }
        }
        match &self.output {
            Output::Rows(rows) => {
                // The commit it was read at is the count's tooltip, not a fact of its own.
                start.push(
                    div()
                        .id("read-rows")
                        .when_some(rows.commit.clone(), |fact, commit| {
                            fact.tooltip(Tooltip::text(format!("Read at graph commit {commit}")))
                        })
                        .child(ds::meta(IconName::Hash, rows_label(rows.row_count)))
                        .into_any_element(),
                );
                start.push(
                    ds::meta(IconName::Clock, format_elapsed(rows.elapsed)).into_any_element(),
                );
                // Where the rows were read, when it is not the branch the status bar shows: the
                // branch changed since, and a commit pin is named by the commit itself. A
                // statement read no branch of the tab's; a diff says which commits it compared.
                match &rows.read_at {
                    ReadAt::Branch(branch) if *branch != self.branch(cx) => {
                        start.push(ds::meta(IconName::GitBranch, branch.clone()).into_any_element())
                    }
                    ReadAt::Statement => {
                        if let Some(sides) = rows.page.as_ref().and_then(diff_sides) {
                            start.push(sides);
                        }
                    }
                    _ => {}
                }
                // A paged read says which page it shows and whether more follow.
                if rows.page_number > 0 && (rows.next.is_some() || rows.page_number > 1) {
                    let page = match &rows.next {
                        Some(_) => format!("Page {} · more follow", rows.page_number),
                        None => format!("Page {} · the last", rows.page_number),
                    };
                    start.push(ds::meta(IconName::ListTree, page).into_any_element());
                }
                if rows.next.is_some() {
                    let focus = self.main_focus(cx);
                    end.push(
                        ds::toolbar_button("result-next-page", "Next page")
                            .style(ButtonStyle::Subtle)
                            .start_icon(Icon::new(IconName::ArrowRight))
                            .key_binding(KeyBinding::for_action_in(&NextPage, &focus, cx))
                            .disabled(self.writing)
                            .tooltip(move |_window, cx| {
                                Tooltip::for_action_in(
                                    "Read the changes after this page",
                                    &NextPage,
                                    &focus,
                                    cx,
                                )
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.next_page(&NextPage, window, cx)
                            }))
                            .into_any_element(),
                    );
                    keys.push(KeyHint::new(
                        "Read the next page",
                        &NextPage,
                        &self.main_focus(cx),
                    ));
                }
                keys.push(KeyHint::new(
                    "Move between the query and its results",
                    &ToggleResultsFocus,
                    &editor_focus,
                ));

                if let Some(graph) = rows.graph.clone() {
                    let title = format!("{} · result", rows.label);
                    end.push(
                        ds::toolbar_button("result-canvas", "Canvas")
                            .style(ButtonStyle::Subtle)
                            .start_icon(Icon::new(IconName::GitGraph))
                            .tooltip(Tooltip::text(format!(
                                "Draw the result's {} nodes, linking those in the same row",
                                graph.0.len()
                            )))
                            .on_click(move |_, window, cx| {
                                window.dispatch_action(
                                    Box::new(studio_actions::graph::ShowResultsOnCanvas {
                                        title: title.clone(),
                                        nodes: graph.0.clone(),
                                        links: graph.1.clone(),
                                    }),
                                    cx,
                                )
                            })
                            .into_any_element(),
                    );
                }
                if self.selected.is_some() {
                    let focus = &self.results_focus;
                    keys.push(KeyHint::new("Copy cell", &CopyCell, focus));
                    keys.push(KeyHint::new("Copy row", &CopyRow, focus));
                    keys.push(KeyHint::new(
                        "Select the cell to the left",
                        &SelectPreviousColumn,
                        focus,
                    ));
                    keys.push(KeyHint::new(
                        "Select the cell to the right",
                        &SelectNextColumn,
                        focus,
                    ));
                }
            }
            Output::Plan(plan) => {
                start.push(
                    div()
                        .id("plan-of")
                        .when_some(plan.commit.clone(), |fact, commit| {
                            fact.tooltip(Tooltip::text(format!("Planned at graph commit {commit}")))
                        })
                        .child(ds::meta(
                            IconName::ListTree,
                            format!("Plan of {}", plan.label),
                        ))
                        .into_any_element(),
                );
                start.push(
                    ds::meta(IconName::Clock, format_elapsed(plan.elapsed)).into_any_element(),
                );
            }
            Output::Running { label } => {
                start.push(
                    h_flex()
                        .gap(px(ds::space::XS))
                        .items_center()
                        .child(ds::spinner())
                        .child(ds::text::meta(label.clone()))
                        .into_any_element(),
                );
            }
            Output::Empty | Output::Written(_) | Output::Failed(_) => {
                if let (None, Some((name, Some(info)))) = (&self.stored, self.current_query())
                    && !info.is_mutation
                    && !info.columns.is_empty()
                {
                    start.push(
                        ds::meta(
                            IconName::ListTree,
                            match info.columns.len() {
                                1 => format!("{name} returns 1 column"),
                                n => format!("{name} returns {n} columns"),
                            },
                        )
                        .into_any_element(),
                    );
                }
                let focus = self.main_focus(cx);
                end.push(ds::key_hint(&Run, "Run", &focus, cx).into_any_element());
                if self.stored.is_none() {
                    end.push(ds::key_hint(&Explain, "Explain", &focus, cx).into_any_element());
                }
            }
        }
        ItemStatus { start, end, keys }
    }

    /// The page's toolbar, as the table's: what the page runs on the leading side (the query
    /// under the cursor, or the stored query), the commit it reads and its actions on the
    /// trailing side, Run last.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self.main_focus(cx);
        let (icon, title, kind): (IconName, Option<String>, Option<&str>) = match &self.stored {
            Some(stored) => (
                IconName::Server,
                Some(stored.entry.name.clone()),
                Some(if stored.entry.mutation {
                    "writes"
                } else {
                    "reads"
                }),
            ),
            None => match self.current_query() {
                Some((name, Some(info))) if !name.is_empty() => (
                    IconName::FileCode,
                    Some(name.to_owned()),
                    Some(if info.is_mutation { "writes" } else { "reads" }),
                ),
                Some((name, None)) if !name.is_empty() => {
                    (IconName::FileCode, Some(name.to_owned()), None)
                }
                _ => (IconName::FileCode, None, None),
            },
        };
        let tip = match &self.stored {
            Some(_) => "A stored query: the server runs it by name, and its text stays there",
            None => "The query under the cursor",
        };
        let busy = self.writing;
        let run_tip = match &self.stored {
            Some(stored) => format!("Run {} with the values below", stored.entry.name),
            None => "Run the query under the cursor".to_owned(),
        };
        studio_ui::ds::toolbar(cx)
            .children(title.map(|title| {
                h_flex()
                    .id("query-title")
                    .min_w_0()
                    .gap(px(space::INLINE))
                    .items_center()
                    .tooltip(Tooltip::text(tip))
                    .child(studio_ui::ds::icon::lead(icon))
                    .child(studio_ui::ds::text::title(title).single_line().truncate())
                    .children(kind.map(studio_ui::ds::text::muted))
            }))
            .children(
                self.file_error
                    .clone()
                    .map(|error| studio_ui::ds::text::error(error).single_line().truncate()),
            )
            // The rows shown were read at a commit the branch has moved past.
            .when(self.is_stale(cx), |toolbar| {
                let focus = focus.clone();
                toolbar.child(
                    studio_ui::ds::toolbar_button("newer-rows", "Newer data · Refresh")
                        .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                        .disabled(busy)
                        .tooltip(move |_window, cx| {
                            Tooltip::for_action_in(
                                "The branch moved since these rows were read; read them again",
                                &Refresh,
                                &focus,
                                cx,
                            )
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.refresh_rows(cx))),
                )
            })
            .child(div().flex_1())
            .child(self.render_snapshot_picker(cx))
            // A stored query's plan needs its text, which the server does not send.
            .when(self.stored.is_none(), |toolbar| {
                toolbar.child(
                    studio_ui::ds::toolbar_button("explain", "Explain")
                        .start_icon(Icon::new(IconName::ListTree))
                        .key_binding(KeyBinding::for_action_in(&Explain, &focus, cx))
                        .tooltip({
                            let focus = focus.clone();
                            move |_window, cx| {
                                Tooltip::for_action_in(
                                    "Show the plan of the query under the cursor",
                                    &Explain,
                                    &focus,
                                    cx,
                                )
                            }
                        })
                        .on_click(
                            cx.listener(|this, _, window, cx| this.explain(&Explain, window, cx)),
                        ),
                )
            })
            .child(
                studio_ui::ds::toolbar_button("run", "Run")
                    .style(ButtonStyle::Filled)
                    .start_icon(Icon::new(IconName::PlayFilled))
                    .key_binding(KeyBinding::for_action_in(&Run, &focus, cx))
                    .disabled(busy)
                    .tooltip({
                        let focus = focus.clone();
                        move |_window, cx| {
                            if busy {
                                Tooltip::simple("A write is in flight; wait for its outcome.", cx)
                            } else {
                                Tooltip::for_action_in(run_tip.clone(), &Run, &focus, cx)
                            }
                        }
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.run(&Run, window, cx))),
            )
    }

    /// The height the problems list takes: a row each, up to [`PROBLEM_ROWS`], then it scrolls;
    /// `None` when there are none to list. Its view is drawn cached, so it is laid out at this
    /// size rather than measured.
    fn problems_height(&self) -> Option<f32> {
        if self.stored.is_some() {
            return None;
        }
        let count = self.analysis.as_ref()?.problems.len();
        (count > 0).then(|| {
            count.min(PROBLEM_ROWS) as f32 * studio_ui::ds::size::ROW + 2. * space::XS + 1.
        })
    }

    /// The problems list, drawn by its own view ([`QueryProblems`]). It reads only what the tab
    /// holds (the check, the problem gone to, the one ⌘. fixes), never the editor, so a key in
    /// the query leaves it as it was.
    fn render_problems(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.problems_height()?;
        let count = self.analysis.as_ref()?.problems.len();
        Some(
            v_flex()
                .size_full()
                .py(px(space::XS))
                .border_t_1()
                .border_color(studio_ui::ds::surface::rule(cx))
                .child(
                    uniform_list(
                        "query-problems",
                        count,
                        cx.processor(|this, range, _window, cx| {
                            this.render_problem_rows(range, cx)
                        }),
                    )
                    .track_scroll(&self.problems_scroll)
                    .flex_1(),
                )
                .into_any_element(),
        )
    }

    fn render_problem_rows(
        &mut self,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(analysis) = self.analysis.clone() else {
            return Vec::new();
        };
        let (hover, selected_bg) = (
            studio_ui::ds::surface::hover(cx),
            studio_ui::ds::surface::selected_document(cx),
        );
        // The problem ⌘. fixes shows the key on its Fix button.
        let fixed_by_key = self.fix_shown;
        let editor_focus = self.editor_focus.clone();
        range
            .filter_map(|index| {
                let (problem, _) = analysis.problems.get(index)?;
                let position = match (problem.line, problem.column) {
                    (Some(line), Some(column)) => format!("{line}:{column}"),
                    _ => String::new(),
                };
                let tip = match &problem.fix {
                    Some(fix) => format!("{} · Fix: {fix}", problem.message),
                    None => problem.message.clone(),
                };
                let row = h_flex()
                    .id(("problem", index))
                    .h(px(studio_ui::ds::size::ROW))
                    .px(px(space::MD))
                    .gap(px(space::MD))
                    .items_center()
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .when(self.selected_problem == Some(index), |row| {
                        row.bg(selected_bg)
                    })
                    .tooltip(Tooltip::text(tip))
                    .child(studio_ui::ds::icon::meta(IconName::XCircle).color(Color::Error))
                    .child(
                        div()
                            .flex_none()
                            .child(studio_ui::ds::text::meta(problem.code.clone()).single_line()),
                    )
                    .child(
                        h_flex()
                            .min_w_0()
                            .flex_1()
                            .gap(px(space::MD))
                            .overflow_hidden()
                            .child(
                                studio_ui::ds::text::body(problem.message.clone())
                                    .single_line()
                                    .truncate(),
                            )
                            .children(problem.fix.clone().map(|fix| {
                                studio_ui::ds::text::muted(format!("Fix: {fix}"))
                                    .single_line()
                                    .truncate()
                            })),
                    )
                    .when(!problem.edits.is_empty(), |row| {
                        let focus = editor_focus.clone();
                        row.child(
                            studio_ui::ds::row_button(("apply-fix", index), "Fix")
                                .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                                .when(fixed_by_key == Some(index), |button| {
                                    button.key_binding(KeyBinding::for_action_in(
                                        &ApplyFix, &focus, cx,
                                    ))
                                })
                                .tooltip(move |_window, cx| {
                                    Tooltip::for_action_in(
                                        "Apply the compiler's suggested edit",
                                        &ApplyFix,
                                        &focus,
                                        cx,
                                    )
                                })
                                // The check held when clicked: the list is not drawn again for
                                // a check that found the same problems in edited text.
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    let Some(checked) = this.analysis.clone() else {
                                        return;
                                    };
                                    if let Some((problem, _)) = checked.problems.get(index) {
                                        this.apply_fix(&checked.text, &problem.edits, window, cx)
                                    }
                                })),
                        )
                    })
                    .child(
                        div()
                            .flex_none()
                            .child(studio_ui::ds::text::meta(position).single_line()),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_problem(index, window, cx)
                    }));
                Some(row.into_any_element())
            })
            .collect()
    }

    /// Goes to problem `index`: it is the one ⌘. fixes, its row is shown in the list and the
    /// cursor goes to its place when it has one.
    fn select_problem(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(count) = self.analysis.as_ref().map(|a| a.problems.len()) else {
            return;
        };
        if index >= count {
            return;
        }
        self.selected_problem = Some(index);
        self.problems_scroll
            .scroll_to_item(index, ScrollStrategy::Nearest);
        self.redraw_problems(cx);
        match self.problem_place(index, cx) {
            Some(place) => self.jump_to(place.start, window, cx),
            None => window.focus(&self.editor_focus, cx),
        }
        cx.notify();
    }

    /// F8: the next problem after the cursor, or after the one gone to, round to the first.
    fn next_problem(&mut self, _: &NextProblem, window: &mut Window, cx: &mut Context<Self>) {
        self.step_problem(true, window, cx);
    }

    fn previous_problem(
        &mut self,
        _: &PreviousProblem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_problem(false, window, cx);
    }

    fn step_problem(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let count = match &self.analysis {
            Some(analysis) if self.stored.is_none() && !analysis.problems.is_empty() => {
                analysis.problems.len()
            }
            _ => return,
        };
        let index = match self.selected_problem {
            Some(index) if forward => (index + 1) % count,
            Some(index) => (index + count - 1) % count,
            // From the cursor: problems are in the order of the text, those with no place last.
            None => {
                let starts: Vec<Option<usize>> = (0..count)
                    .map(|index| self.problem_place(index, cx).map(|place| place.start))
                    .collect();
                let cursor = self.cursor;
                if forward {
                    starts
                        .iter()
                        .position(|start| start.is_some_and(|start| start > cursor))
                        .unwrap_or(0)
                } else {
                    starts
                        .iter()
                        .rposition(|start| start.is_some_and(|start| start < cursor))
                        .unwrap_or(count - 1)
                }
            }
        };
        self.select_problem(index, window, cx);
    }

    /// The problem ⌘. fixes: the one gone to, else the one under the cursor, when it has a fix.
    fn fix_target(&self, cx: &App) -> Option<usize> {
        let analysis = self.analysis.as_ref()?;
        let has_fix = |index: usize| {
            analysis
                .problems
                .get(index)
                .is_some_and(|(problem, _)| !problem.edits.is_empty())
        };
        self.selected_problem
            .filter(|&index| has_fix(index))
            .or_else(|| {
                (0..analysis.problems.len())
                    .find(|&index| has_fix(index) && self.problem_holds(index, self.cursor, cx))
            })
    }

    /// ⌘.: applies the compiler's suggested edit for the problem gone to or under the cursor.
    fn apply_fix_action(&mut self, _: &ApplyFix, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(index), Some(analysis)) = (self.fix_target(cx), self.analysis.clone()) else {
            return;
        };
        if let Some((problem, _)) = analysis.problems.get(index) {
            self.apply_fix(&analysis.text, &problem.edits, window, cx);
        }
    }

    /// Applies a suggestion's edits, if the text is still the text they were made for.
    fn apply_fix(
        &mut self,
        checked: &str,
        edits: &[(std::ops::Range<usize>, String)],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editor.read(cx).text(cx) != checked {
            return;
        }
        self.editor.update(cx, |editor, cx| {
            editor.edit(
                edits.iter().rev().map(|(range, text)| {
                    (
                        MultiBufferOffset(range.start)..MultiBufferOffset(range.end),
                        text.clone(),
                    )
                }),
                cx,
            )
        });
        let focus = self.editor.focus_handle(cx);
        window.focus(&focus, cx);
    }

    /// The parameters as a field grid: each name, its input and its type.
    fn render_params(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        use studio_ui::ds::{self, size, space};
        let params = self.shown_params()?;
        if params.is_empty() {
            return None;
        }
        Some(
            v_flex()
                .flex_none()
                .px(px(space::EDGE))
                .py(px(space::LG))
                .border_b_1()
                .border_color(ds::surface::rule(cx))
                .child(
                    ds::fields().children(params.iter().filter_map(|(name, ty, nullable)| {
                        let input = self.params.get(name)?.clone();
                        let ty = format!("{ty}{}", if *nullable { "?" } else { "" });
                        Some(ds::field(
                            format!("${name}"),
                            h_flex()
                                .gap(px(space::MD))
                                .items_center()
                                .child(
                                    ds::field_frame(false, cx)
                                        .flex_1()
                                        .max_w(px(size::INSPECTOR_WIDTH))
                                        .child(ds::field_input(input)),
                                )
                                .child(ds::text::meta(ty).single_line()),
                        ))
                    })),
                )
                .into_any_element(),
        )
    }

    fn render_confirm(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        use studio_ui::ds::{self, size, space};
        let pending = self.pending_write.as_ref()?;
        let (background, border) = (ds::surface::warning(cx), ds::surface::warning_border(cx));
        let (said, button) = confirm_words(&pending.prepared, &pending.branch);
        let what = match &pending.typed {
            Some(typed) => typed.what.clone(),
            None => said,
        };
        // A write Studio cannot put back takes its name typed, in a field before the buttons.
        let typed = pending.typed.as_ref().map(|typed| {
            ds::field_frame(typed.refused, cx)
                .flex_none()
                .w(px(size::FIELD_LABEL_WIDE))
                .child(ds::field_input(typed.field.clone()))
        });
        let confirm_focus = match &pending.typed {
            Some(typed) => typed.field.focus_handle(cx),
            None => self.confirm_focus.clone(),
        };
        Some(
            h_flex()
                .key_context("QueryConfirm")
                .track_focus(&self.confirm_focus)
                .on_action(cx.listener(Self::confirm_write))
                .on_action(cx.listener(Self::cancel_write))
                .flex_none()
                .min_h(px(size::STRIP))
                .px(px(space::EDGE))
                .gap(px(space::INLINE))
                .items_center()
                .bg(background)
                .border_b_1()
                .border_color(border)
                .child(studio_ui::ds::icon::lead(IconName::Warning).color(Color::Warning))
                .child(div().flex_1().min_w_0().child(ds::text::body(what)))
                .children(typed)
                .child(
                    h_flex()
                        .gap(px(space::XS))
                        .child(
                            ds::button("cancel-write", "Cancel")
                                .key_binding(KeyBinding::for_action_in(
                                    &menu::Cancel,
                                    &confirm_focus,
                                    cx,
                                ))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.cancel_write(&menu::Cancel, window, cx)
                                })),
                        )
                        .child(
                            ds::button("confirm-write", button)
                                .style(ButtonStyle::Filled)
                                .key_binding(KeyBinding::for_action_in(
                                    &menu::Confirm,
                                    &confirm_focus,
                                    cx,
                                ))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.confirm_write(&menu::Confirm, window, cx)
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    /// The bar that says the file changed on disk after the draft was typed over it: the draft
    /// is kept, and the person saves it over the file (↵ once ⌘S asked) or takes the file's
    /// text. Nothing is written over the file without it.
    fn render_disk_change(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        use studio_ui::ds::{self, size, space};
        let change = self.disk_change.as_ref()?;
        let name = self.path.as_deref().map(file_name).unwrap_or_default();
        let said = if change.asked {
            format!(
                "{name} changed on disk after this draft was made. Save writes the draft over it; \
                 the file's text is gone then."
            )
        } else {
            format!(
                "{name} changed on disk after this draft was made. The draft is kept; nothing is \
                 written over the file until you choose."
            )
        };
        let focus = self.file_focus.clone();
        Some(
            h_flex()
                .key_context("QueryConfirm")
                .track_focus(&self.file_focus)
                .on_action(
                    cx.listener(|this, _: &menu::Confirm, window, cx| this.save_over(window, cx)),
                )
                .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| {
                    window.focus(&this.editor_focus, cx);
                    cx.notify();
                }))
                .flex_none()
                .min_h(px(size::STRIP))
                .px(px(space::EDGE))
                .gap(px(space::INLINE))
                .items_center()
                .bg(ds::surface::warning(cx))
                .border_b_1()
                .border_color(ds::surface::warning_border(cx))
                .child(studio_ui::ds::icon::lead(IconName::Warning).color(Color::Warning))
                .child(div().flex_1().min_w_0().child(ds::text::body(said)))
                .child(
                    h_flex()
                        .gap(px(space::XS))
                        .child(
                            ds::button("take-disk-text", "Use the file's text")
                                .tooltip(Tooltip::text(
                                    "Replace the draft with what the file holds now (⌘Z brings \
                                     the draft back)",
                                ))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.take_disk_text(window, cx)
                                })),
                        )
                        .child(
                            ds::button("save-over", "Save over it")
                                .style(ButtonStyle::Filled)
                                .disabled(self.saving)
                                .key_binding(KeyBinding::for_action_in(&menu::Confirm, &focus, cx))
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.save_over(window, cx)),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// The bar that says the tab's file is gone, so the text it holds is on no disk: Save As
    /// keeps it elsewhere, and Save writes it back where it was.
    fn render_file_gone(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        use studio_ui::ds::{self, size, space};
        self.file_gone.as_ref()?;
        let name = self.path.as_deref().map(file_name).unwrap_or_default();
        let focus = self.editor_focus.clone();
        Some(
            h_flex()
                .flex_none()
                .min_h(px(size::STRIP))
                .px(px(space::EDGE))
                .gap(px(space::INLINE))
                .items_center()
                .bg(ds::surface::warning(cx))
                .border_b_1()
                .border_color(ds::surface::warning_border(cx))
                .child(studio_ui::ds::icon::lead(IconName::Warning).color(Color::Warning))
                .child(div().flex_1().min_w_0().child(ds::text::body(format!(
                    "{name} is not on disk any more: it was moved or deleted. This is the text \
                     the tab last held; Save As keeps it, and Save writes it back where it was."
                ))))
                .child(
                    ds::button("file-gone-save-as", "Save As…")
                        .style(ButtonStyle::Filled)
                        .disabled(self.saving)
                        .key_binding(KeyBinding::for_action_in(&SaveAs, &focus, cx))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.save_as(&SaveAs, window, cx)),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_results(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match &self.output {
            Output::Empty => self.render_expected(cx),
            Output::Running { .. } => v_flex().flex_1().min_h_0().into_any_element(),
            Output::Rows(rows) => self.render_rows(rows, window, cx),
            Output::Plan(plan) => self.render_plan(plan, cx),
            Output::Written(result) => self.render_written(result, cx),
            Output::Failed(failure) => self.render_failure(failure, cx),
        }
    }

    /// Before the first run: the columns the query under the cursor will return, or what Run
    /// will do.
    fn render_expected(&self, cx: &mut Context<Self>) -> AnyElement {
        use studio_ui::ds;
        let focus = self.main_focus(cx);
        let run_hint = ds::key_hint(&Run, "Run", &focus, cx);
        let writes = |name: &str, branch: String| {
            ds::empty_state(IconName::Warning, format!("{name} changes data")).child(
                ds::text::meta(format!("Run asks before it writes to {branch}")),
            )
        };
        if let Some(stored) = &self.stored {
            if stored.entry.mutation {
                return v_flex()
                    .flex_1()
                    .min_h_0()
                    .child(writes(&stored.entry.name, self.branch(cx)))
                    .into_any_element();
            }
            return v_flex()
                .flex_1()
                .min_h_0()
                .child(ds::empty_state(IconName::PlayFilled, "Results appear here").child(run_hint))
                .into_any_element();
        }
        match self.current_query() {
            Some((_, Some(info))) if !info.is_mutation && !info.columns.is_empty() => {
                let columns: Vec<GridColumn> = info
                    .columns
                    .iter()
                    // Named as the answer will name them, so a run leaves the headers as
                    // they were.
                    .map(|c| {
                        GridColumn::new(c.column.clone(), c.column.clone())
                            .detail(type_label(c))
                            .width(px(crate::results::header_width(
                                &c.column,
                                Some(&type_label(c)),
                            )))
                    })
                    .collect();
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("expected-columns")
                            .overflow_x_scroll()
                            .child(grid_header(&columns, px(ROW_NUMBER_WIDTH), cx)),
                    )
                    .into_any_element()
            }
            Some((name, Some(info))) if info.is_mutation => v_flex()
                .flex_1()
                .min_h_0()
                .child(writes(name, self.branch(cx)))
                .into_any_element(),
            _ => v_flex()
                .flex_1()
                .min_h_0()
                .child(ds::empty_state(IconName::PlayFilled, "Results appear here").child(run_hint))
                .into_any_element(),
        }
    }

    fn render_rows(
        &self,
        rows: &RowsOutput,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let places = self.result_places(rows, window);
        let table = &rows.table;
        let columns: Vec<GridColumn> = table
            .columns
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let mut column = GridColumn::new(name.clone(), name.clone())
                    .width(px(rows.widths[i]))
                    .align_right(rows.right_aligned[i]);
                if let Some(Some(ty)) = table.types.get(i) {
                    column = column.detail(ty.clone());
                }
                column
            })
            .collect();
        let total: f32 = ROW_NUMBER_WIDTH + rows.widths.iter().sum::<f32>();
        let header = || {
            resizable_grid_header(
                &columns,
                Some(&places),
                px(ROW_NUMBER_WIDTH),
                |index| Some(self.render_resize_handle(index, rows.widths[index], cx)),
                cx,
            )
        };
        let body = if table.rows.is_empty() {
            v_flex()
                .flex_1()
                .min_h_0()
                .child(header())
                .child(studio_ui::ds::empty_state(IconName::Hash, "No rows"))
                .into_any_element()
        } else {
            div()
                .id("query-results-scroll")
                .flex_1()
                .min_h_0()
                .overflow_x_scroll()
                .track_scroll(&self.results_hscroll)
                .child(
                    v_flex().h_full().w(px(total)).child(header()).child(
                        uniform_list(
                            "query-results",
                            table.rows.len(),
                            cx.processor(|this, range, window, cx| {
                                this.render_grid_rows(range, window, cx)
                            }),
                        )
                        .track_scroll(&self.results_scroll)
                        .flex_1(),
                    ),
                )
                .into_any_element()
        };
        v_flex()
            .key_context("QueryResults")
            .track_focus(&self.results_focus)
            .on_action(cx.listener(Self::copy_cell))
            .on_action(cx.listener(Self::copy_row))
            .on_action(
                cx.listener(|this, _: &menu::SelectNext, _, cx| this.move_selection(1, 0, cx)),
            )
            .on_action(
                cx.listener(|this, _: &menu::SelectPrevious, _, cx| this.move_selection(-1, 0, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectNextColumn, _, cx| this.move_selection(0, 1, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectPreviousColumn, _, cx| this.move_selection(0, -1, cx)),
            )
            // A column's edge dragged anywhere over the results resizes it until released.
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<ColumnDrag>, _, cx| {
                    this.drag_column(event, cx)
                }),
            )
            // Released, by a drop here or elsewhere, or a click that dragged nothing.
            .capture_any_mouse_up(cx.listener(|this, _, _, cx| {
                if this.resizing.take().is_some() {
                    this.redraw_results(cx);
                }
            }))
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.resizing.take().is_some() {
                        this.redraw_results(cx);
                    }
                }),
            )
            .flex_1()
            .min_h_0()
            .child(body)
            .into_any_element()
    }

    /// The right edge of result column `index`'s header: drag to make the column wider or
    /// narrower, click twice to fit it to its values again.
    fn render_resize_handle(&self, index: usize, width: f32, cx: &Context<Self>) -> AnyElement {
        let active = self
            .resizing
            .as_ref()
            .is_some_and(|resizing| resizing.column == index);
        let on_start = cx.listener(move |this, event: &MouseDownEvent, _, cx| {
            this.resizing = Some(ColumnResize::new(index, event, width));
            this.redraw_results(cx);
        });
        let on_reset = cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.resizing = None;
            this.resize_column(index, None, cx);
        });
        studio_ui::column_resize_handle(cx.entity_id(), index, active, on_start, on_reset, cx)
            .into_any_element()
    }

    fn drag_column(&mut self, event: &DragMoveEvent<ColumnDrag>, cx: &mut Context<Self>) {
        let Some(resizing) = &self.resizing else {
            return;
        };
        if !ColumnDrag::of(event, cx.entity_id(), cx) {
            return;
        }
        let (index, width) = (resizing.column, resizing.width(event));
        self.resize_column(index, Some(width), cx);
    }

    /// Sets result column `index` to `width`, or fits it to its values again with `None`. The
    /// width holds for that column name in later results.
    fn resize_column(&mut self, index: usize, width: Option<f32>, cx: &mut Context<Self>) {
        let Output::Rows(rows) = &mut self.output else {
            return;
        };
        let Some(name) = rows.table.columns.get(index) else {
            return;
        };
        let width = match width {
            Some(width) => {
                self.column_widths.insert(name.clone(), width);
                width
            }
            None => {
                self.column_widths.remove(name);
                rows.table.column_width(index)
            }
        };
        if rows.widths[index] == width {
            return;
        }
        rows.widths[index] = width;
        cx.emit(ItemEvent::UpdateTab);
        self.redraw_results(cx);
        cx.notify();
    }

    /// The result columns this frame draws: those in the sideways view, one either side, and
    /// the selected cell's (`studio_ui::grid_places`).
    fn result_places(&self, rows: &RowsOutput, window: &Window) -> Vec<GridPlace> {
        let widths: Vec<Pixels> = rows.widths.iter().map(|&width| px(width)).collect();
        // The row numbers scroll with the columns. The view is never wider than the window,
        // whose width is this frame's.
        let width = window.viewport_size().width;
        let content = widths.iter().fold(px(ROW_NUMBER_WIDTH), |sum, w| sum + *w);
        let scrolled = studio_ui::scrolled_within(-self.results_hscroll.offset().x, content, width);
        let start = scrolled - px(ROW_NUMBER_WIDTH);
        let mut drawn = columns_in_view(&widths, start..start + width);
        if let Some(drawn) = self.selected.and_then(|(_, column)| drawn.get_mut(column)) {
            *drawn = true;
        }
        grid_places(&drawn, &widths)
    }

    fn render_grid_rows(
        &mut self,
        range: std::ops::Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Output::Rows(rows) = &self.output else {
            return Vec::new();
        };
        let places = self.result_places(rows, window);
        let (selected_bg, row_selected_bg) = (
            studio_ui::ds::surface::selected_document(cx),
            studio_ui::ds::surface::hover(cx),
        );
        let (muted, disabled, default) = (
            Color::Muted.color(cx),
            Color::Disabled.color(cx),
            Color::Default.color(cx),
        );
        let selected = self.selected;
        range
            .filter_map(|ix| {
                let values = rows.table.rows.get(ix)?;
                let shown = rows.shown.get(ix)?.get_or_init(|| {
                    values
                        .iter()
                        .map(|value| SharedString::from(format_value(value)))
                        .collect()
                });
                let row_selected = selected.is_some_and(|(row, _)| row == ix);
                Some(
                    h_flex()
                        .h(px(ROW_HEIGHT))
                        .border_b_1()
                        .dotted(cx)
                        .when(row_selected, |d| d.bg(row_selected_bg))
                        .child(
                            h_flex()
                                .w(px(ROW_NUMBER_WIDTH))
                                .flex_none()
                                .px(px(space::MD))
                                .child(
                                    studio_ui::cell_text((ix + 1).to_string(), cx)
                                        .meta(cx)
                                        .color(muted),
                                ),
                        )
                        .children(places.iter().map(|place| {
                            let column = match *place {
                                GridPlace::Column(column) => column,
                                GridPlace::Gap(width) => {
                                    return div().w(width).flex_none().into_any_element();
                                }
                            };
                            let cell_selected = selected == Some((ix, column));
                            let right = rows.right_aligned[column];
                            let text = studio_ui::cell_text(shown[column].clone(), cx).color(
                                if values[column].is_null() {
                                    disabled
                                } else {
                                    default
                                },
                            );
                            grid_cell(px(rows.widths[column]), right)
                                .when(cell_selected, |d| d.bg(selected_bg))
                                .child(if right { text.align_end() } else { text })
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, window, cx| {
                                        this.select_cell(ix, column, window, cx)
                                    }),
                                )
                                .into_any_element()
                        }))
                        .into_any_element(),
                )
            })
            .collect()
    }

    fn render_plan(&self, plan: &PlanOutput, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .flex_1()
            .min_h_0()
            .child(
                uniform_list(
                    "query-plan",
                    plan.items.len(),
                    cx.processor(|this, range, _window, cx| this.render_plan_rows(range, cx)),
                )
                .flex_1()
                .py(px(space::XS)),
            )
            .into_any_element()
    }

    /// Shows `output` in place of the output shown. A plan's operator lit in the text goes out
    /// with it, and the rows it replaces are dropped off the main thread, where a large
    /// answer's would hold up a frame.
    fn replace_output(&mut self, output: Output, cx: &mut Context<Self>) {
        self.unlight_plan(cx);
        let replaced = std::mem::replace(&mut self.output, output);
        if matches!(replaced, Output::Rows(_)) {
            cx.background_spawn(async move { drop(replaced) }).detach();
        }
        self.redraw_results(cx);
    }

    /// Clears the text a plan's operator lit, before the output with the plan is replaced: the
    /// row the pointer was on goes, and with it the hover that would clear it.
    fn unlight_plan(&mut self, cx: &mut Context<Self>) {
        if self.plan_hovered.is_some() {
            self.light_plan_parts(None, cx);
        }
    }

    /// Lights the parts of the query that the plan operator at `ix` stands for, or none
    /// (spec 007). The declaration is found again by name, so edits since the plan was read
    /// move the light with the text.
    fn light_plan_parts(&mut self, ix: Option<usize>, cx: &mut Context<Self>) {
        let key = HighlightKey::ConsoleAnsiHighlight(PLAN_KEY);
        self.plan_hovered = ix;
        let ranges = match (ix, &self.output) {
            (Some(ix), Output::Plan(plan)) => match plan.items.get(ix) {
                Some(PlanItem::Operator { node, detail, .. }) => {
                    let text = self.editor.read(cx).text(cx);
                    let outline = gq_language::outline(&text);
                    plan.name
                        .as_deref()
                        .and_then(|name| outline.declarations.iter().find(|d| d.name == name))
                        .or_else(|| outline.declaration_at(self.cursor))
                        .map(|declaration| {
                            let parts = gq_language::query_parts(&text, declaration.range.clone());
                            gq_language::operator_parts(&parts, node, detail)
                        })
                        .unwrap_or_default()
                }
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        let fill = studio_ui::ds::surface::text_highlight(cx);
        self.editor.update(cx, |editor, cx| {
            if ranges.is_empty() {
                editor.clear_background_highlights(key, cx);
                return;
            }
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let anchors: Vec<_> = ranges
                .iter()
                .map(|range| {
                    snapshot.anchor_after(MultiBufferOffset(range.start))
                        ..snapshot.anchor_before(MultiBufferOffset(range.end))
                })
                .collect();
            editor.highlight_background(key, &anchors, move |_, _| fill, cx);
        });
        // The plan row under the pointer is drawn lit.
        self.redraw_results(cx);
        cx.notify();
    }

    fn render_plan_rows(
        &mut self,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let hover_bg = studio_ui::ds::surface::hover(cx);
        let lit_bg = studio_ui::ds::surface::selected_document(cx);
        let hovered = self.plan_hovered;
        let Output::Plan(plan) = &self.output else {
            return Vec::new();
        };
        range
            .filter_map(|ix| {
                let element = match plan.items.get(ix)? {
                    PlanItem::Header(title) => h_flex()
                        .h(px(PLAN_ROW_HEIGHT))
                        .px(px(space::MD))
                        .child(studio_ui::ds::text::meta(title.clone()))
                        .into_any_element(),
                    PlanItem::Operator {
                        depth,
                        node,
                        facts,
                        detail: _,
                    } => h_flex()
                        .id(("plan-row", ix))
                        .w_full()
                        .h(px(PLAN_ROW_HEIGHT))
                        .pl(px(12. + *depth as f32 * 16.))
                        .pr(px(space::MD))
                        .gap(px(space::MD))
                        .hover(|row| row.bg(hover_bg))
                        .when(hovered == Some(ix), |row| row.bg(lit_bg))
                        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                            if *hovered {
                                this.light_plan_parts(Some(ix), cx);
                            } else if this.plan_hovered == Some(ix) {
                                this.light_plan_parts(None, cx);
                            }
                        }))
                        .child(studio_ui::ds::icon::meta(if *depth == 0 {
                            IconName::ChevronDown
                        } else {
                            IconName::ArrowRight
                        }))
                        .child(studio_ui::ds::text::body(node.clone()))
                        .child(render_facts(facts))
                        .when(!facts.is_empty(), |d| {
                            d.tooltip(Tooltip::text(facts_tooltip(facts)))
                        })
                        .into_any_element(),
                    PlanItem::Note { name, value } => h_flex()
                        .id(("plan-note", ix))
                        .h(px(PLAN_ROW_HEIGHT))
                        .pl(px(space::LG))
                        .pr(px(space::MD))
                        .gap(px(space::MD))
                        .child(studio_ui::ds::text::meta(name.clone()))
                        .child(
                            studio_ui::ds::text::meta(value.clone())
                                .color(Color::Default)
                                .single_line()
                                .truncate(),
                        )
                        .tooltip(Tooltip::text(value.clone()))
                        .into_any_element(),
                };
                Some(element)
            })
            .collect()
    }

    fn render_written(&self, result: &WriteResult, cx: &mut Context<Self>) -> AnyElement {
        use studio_ui::ds;
        match result {
            WriteResult::Done {
                effect,
                counts,
                elapsed,
            } => {
                let count =
                    |n: i64, one: &str| format!("{n} {one}{}", if n == 1 { "" } else { "s" });
                let mut facts = Vec::new();
                if let Some((nodes, edges)) = counts {
                    facts.push(
                        ds::meta(
                            IconName::Hash,
                            format!("{}, {}", count(*nodes, "node"), count(*edges, "edge")),
                        )
                        .into_any_element(),
                    );
                }
                facts.push(ds::meta(IconName::Clock, format_elapsed(*elapsed)).into_any_element());
                if let WriteEffect::BranchCreated { from, .. } = effect
                    && !from.is_empty()
                {
                    facts.push(
                        ds::meta(IconName::GitBranch, format!("from {from}")).into_any_element(),
                    );
                }
                if let Some((commit, what)) = effect.commit() {
                    facts.push(
                        div()
                            .id("write-commit")
                            .child(ds::meta_code(
                                IconName::GitCommit,
                                short_commit_id(commit),
                                cx,
                            ))
                            .tooltip(Tooltip::text(format!("{what} {commit}")))
                            .into_any_element(),
                    );
                }
                self.render_message(
                    IconName::Check,
                    Color::Success,
                    effect.title(),
                    effect.detail(),
                    facts,
                    None,
                    cx,
                )
            }
            WriteResult::Refused {
                status,
                message,
                diagnostic,
            } => self.render_message(
                IconName::XCircle,
                Color::Error,
                format!("Refused ({status}): {message}"),
                Some("Nothing was written. Your query is unchanged; fix it and run again.".into()),
                Vec::new(),
                diagnostic.clone(),
                cx,
            ),
            WriteResult::Unknown { reason } => self.render_message(
                IconName::Warning,
                Color::Warning,
                format!("The outcome is unknown: {reason}"),
                Some(
                    "The write may or may not have landed. Check History before trying again; \
                     Studio does not retry."
                        .into(),
                ),
                Vec::new(),
                None,
                cx,
            ),
        }
    }

    fn render_failure(&self, failure: &Failure, cx: &mut Context<Self>) -> AnyElement {
        self.render_message(
            IconName::XCircle,
            Color::Error,
            failure.title.clone(),
            failure.hint.clone(),
            Vec::new(),
            failure.diagnostic.clone(),
            cx,
        )
    }

    /// An outcome or a failure: its icon and title, then what to do, its facts and the place
    /// in the query the server pointed at.
    #[allow(clippy::too_many_arguments)]
    fn render_message(
        &self,
        icon: IconName,
        color: Color,
        title: String,
        detail: Option<String>,
        facts: Vec<AnyElement>,
        diagnostic: Option<ServerDiagnostic>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use studio_ui::ds::{self, space};
        let fix = diagnostic.as_ref().and_then(|d| d.fix.clone());
        // The server's suggested edit, said in words, and applied here while the tab holds
        // the text it was run with: a stored query's is the server's.
        let suggestion = diagnostic
            .as_ref()
            .filter(|_| self.stored.is_none())
            .and_then(|d| Some((d.suggestion.clone()?, d.review)));
        let (can_apply, applied) = (self.server_fix_holds, self.diagnostic_edits.is_empty());
        v_flex()
            .flex_1()
            .min_h_0()
            .px(px(space::EDGE))
            .py(px(space::LG))
            .child(
                h_flex()
                    .items_start()
                    .gap(px(space::INLINE))
                    .child(
                        div()
                            .pt(px(space::XXS))
                            .child(studio_ui::ds::icon::lead(icon).color(color)),
                    )
                    .child(
                        v_flex()
                            .min_w_0()
                            .gap(px(space::XS))
                            .child(ds::text::body(title))
                            .children(detail.map(ds::text::muted))
                            .when(!facts.is_empty(), |column| {
                                column.child(ds::meta_row(facts))
                            })
                            .children(diagnostic.map(|diagnostic| {
                                // A stored query's text is the server's: no place in this tab.
                                let at = self
                                    .diagnostic_at
                                    .filter(|_| diagnostic.byte.is_some() && self.stored.is_none());
                                let tip = match (&at, &diagnostic.stage) {
                                    (Some(_), _) => "Go to where the server found it".to_owned(),
                                    (None, Some(stage)) => {
                                        format!("Found at the {stage} stage, after parsing")
                                    }
                                    (None, None) => "The server names no place in the text".into(),
                                };
                                ds::button(
                                    "server-diagnostic",
                                    format!("{} · {}", diagnostic.code, diagnostic.message),
                                )
                                .start_icon(Icon::new(IconName::ArrowRight))
                                .disabled(at.is_none())
                                .tooltip(Tooltip::text(tip))
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        if let Some(at) = at {
                                            let at = this.offset_of(&at, cx);
                                            this.jump_to(at, window, cx);
                                        }
                                    },
                                ))
                            }))
                            .children(fix.map(|fix| ds::text::muted(format!("Fix: {fix}"))))
                            .children(suggestion.map(|(said, review)| {
                                h_flex()
                                    .gap(px(space::MD))
                                    .items_center()
                                    .child(ds::text::muted(format!("Suggested edit: {said}")))
                                    .child(
                                        studio_ui::ds::row_button("apply-server-fix", "Apply")
                                            .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                                            .disabled(!can_apply)
                                            .tooltip(Tooltip::text(match (can_apply, review) {
                                                (false, _) if applied => {
                                                    "Applied; run the query again"
                                                }
                                                (false, _) => {
                                                    "The query changed where this edit goes; run \
                                                     it again for the server's suggestion now"
                                                }
                                                (true, true) => {
                                                    "Apply the server's suggested edit; it asks \
                                                     for a look before you run again"
                                                }
                                                (true, false) => {
                                                    "Apply the server's suggested edit to the query"
                                                }
                                            }))
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.apply_server_fix(window, cx)
                                            })),
                                    )
                            })),
                    ),
            )
            .into_any_element()
    }
}

impl Render for QueryEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Recolours the language when the theme has changed since it last coloured it.
        if self.parsed {
            crate::syntax::language(crate::syntax::Syntax::Gq, cx);
        }
        let toolbar = self.render_toolbar(cx).into_any_element();
        // At most one: a file that changed on disk is one that can be read.
        let disk_change = self
            .render_disk_change(cx)
            .or_else(|| self.render_file_gone(cx));
        // Cached, as the results are: a key in the query draws the tab again, and the list
        // only when its problems change. A cached view is laid out at its style's size.
        let problems = self.problems_height().map(|height| {
            AnyView::from(self.problems.clone())
                .cached(StyleRefinement::default().flex_none().h(px(height)))
        });
        let params = self.render_params(cx);
        let confirm = self.render_confirm(cx);
        let results = AnyView::from(self.results.clone())
            .cached(StyleRefinement::default().flex_1().min_h_0());
        let source = match &self.stored {
            Some(stored) => self.render_stored_about(stored, cx),
            None => Some(
                div()
                    .h(relative(0.42))
                    .min_h(px(studio_ui::ds::size::EDITOR_MIN_HEIGHT))
                    .flex_none()
                    .pt(px(space::XS))
                    .border_b_1()
                    .border_color(studio_ui::ds::surface::rule(cx))
                    .map(|area| match (&self.loading, &self.path) {
                        // Its file's text is not in yet: the tab says so, and shows no empty
                        // editor that reads as an empty file.
                        (Some(_), Some(path)) => area.child(studio_ui::ds::loading_state(format!(
                            "Reading {}…",
                            file_name(path)
                        ))),
                        _ => area.child(self.editor.clone()),
                    })
                    .into_any_element(),
            ),
        };
        v_flex()
            .key_context("QueryEditor")
            .on_action(cx.listener(Self::run))
            .on_action(cx.listener(Self::explain))
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::save_as))
            .on_action(cx.listener(Self::next_page))
            .on_action(cx.listener(Self::toggle_results_focus))
            .on_action(cx.listener(Self::next_problem))
            .on_action(cx.listener(Self::previous_problem))
            .on_action(cx.listener(Self::apply_fix_action))
            .on_action(cx.listener(Self::refresh))
            .size_full()
            .child(toolbar)
            .children(disk_change)
            .children(source)
            .children(problems)
            .children(params)
            .children(confirm)
            .child(results)
    }
}

/// A diff's two sides, as the commits its page compared (`01JA → 01JB`), with the merge base in
/// the tooltip for a branch's diff since it.
fn diff_sides(page: &ReadPageOutput) -> Option<AnyElement> {
    let (from, to) = (page.from_commit_id.as_ref()?, page.to_commit_id.as_ref()?);
    let mut tip = format!("Compared commit {from} with commit {to}");
    if let Some(base) = &page.base_commit_id {
        tip.push_str(&format!(", since their merge base {base}"));
    }
    Some(
        div()
            .id("diff-sides")
            .child(studio_ui::ds::meta(
                IconName::GitCommit,
                format!("{} → {}", short_commit_id(from), short_commit_id(to)),
            ))
            .tooltip(Tooltip::text(tip))
            .into_any_element(),
    )
}

/// The dedupe key of a tab editing `path`, so a file opens once.
pub fn file_key(path: &std::path::Path) -> String {
    format!("query-file:{}", path.display())
}

/// A file's name as the tab and its bars say it: `people.gq`.
fn file_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// A parameter's input: one line, its text at the size of the labels beside it.
fn param_input(
    placeholder: &str,
    text: Option<String>,
    window: &mut Window,
    cx: &mut Context<QueryEditor>,
) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text(placeholder, window, cx);
        editor.set_text_style_refinement(studio_ui::ds::field_text(cx));
        if let Some(text) = text {
            editor.set_text(text, window, cx);
        }
        editor
    })
}

/// The dedupe key of the tab running the stored query `name`, so it opens once.
pub fn stored_key(name: &str) -> String {
    format!("stored-query:{name}")
}

/// The type a stored-query parameter is read as, in the spelling `gq_language::param_value`
/// takes. A 64-bit integer is sent as the text typed (`BigInt` is no spelling it converts), so
/// a value past 2^53, or above the signed maximum, reaches the server exactly; the server reads
/// either form. A kind Studio does not know is sent as text too.
fn param_type(param: &ParamDescriptor) -> String {
    fn scalar(kind: ParamKind) -> &'static str {
        match kind {
            ParamKind::String => "String",
            ParamKind::Bool => "Bool",
            ParamKind::Int => "I64",
            ParamKind::BigInt => "BigInt",
            ParamKind::Float => "F64",
            ParamKind::Date => "Date",
            ParamKind::DateTime => "DateTime",
            ParamKind::Blob => "Blob",
            ParamKind::Vector | ParamKind::List | ParamKind::Other => "Text",
        }
    }
    match param.kind {
        ParamKind::Vector => format!("Vector({})", param.vector_dim.unwrap_or_default()),
        ParamKind::List => format!("[{}]", scalar(param.item_kind.unwrap_or(ParamKind::Other))),
        kind => scalar(kind).to_owned(),
    }
}

/// `dir/name.gq`, or `dir/name-2.gq` and so on when that is taken.
fn unused_path(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut candidate = dir.join(format!("{safe}.gq"));
    let mut n = 2;
    while candidate.exists() {
        candidate = dir.join(format!("{safe}-{n}.gq"));
        n += 1;
    }
    candidate
}

/// A declaration's last outcome, shown on its run line.
#[derive(Debug, Clone)]
struct RunNote {
    text: String,
    failed: bool,
}

/// The line above a declaration: Run, and how its last run went.
fn run_line(
    editor: &gpui::WeakEntity<QueryEditor>,
    name: &str,
    index: usize,
    block: &mut BlockContext,
) -> AnyElement {
    let note = editor
        .upgrade()
        .and_then(|editor| editor.read(block.app).last_runs.get(name).cloned());
    let editor = editor.clone();
    h_flex()
        .id(("run-line", index))
        .w_full()
        .pl(block.margins.gutter.full_width())
        .gap(px(space::MD))
        .items_center()
        .child(
            studio_ui::ds::row_button(("run-line-run", index), "Run")
                .style(ButtonStyle::Transparent)
                .color(Color::Muted)
                .start_icon(Icon::new(IconName::PlayFilled))
                .on_click(move |_, window, cx| {
                    editor
                        .update(cx, |editor, cx| editor.run_declaration(index, window, cx))
                        .ok();
                }),
        )
        .children(note.map(|note| {
            studio_ui::ds::text::meta(note.text).color(if note.failed {
                Color::Error
            } else {
                Color::Muted
            })
        }))
        .into_any_element()
}

/// An operator's facts on one line: each name muted, then its value, the line clipped at the
/// row's end.
fn render_facts(facts: &[(String, String)]) -> impl IntoElement {
    const SHOWN: usize = 48;
    h_flex()
        .min_w_0()
        .flex_1()
        .gap(px(space::LG))
        .overflow_hidden()
        .children(facts.iter().map(|(name, value)| {
            let value = if value.chars().count() > SHOWN {
                format!("{}…", value.chars().take(SHOWN - 1).collect::<String>())
            } else {
                value.clone()
            };
            h_flex()
                .flex_none()
                .gap(px(space::XS))
                .when(!name.is_empty(), |fact| {
                    fact.child(studio_ui::ds::text::meta(name.clone()))
                })
                .child(
                    studio_ui::ds::text::meta(value)
                        .color(Color::Default)
                        .single_line(),
                )
        }))
}

/// An operator's facts in full, one a line.
fn facts_tooltip(facts: &[(String, String)]) -> String {
    facts
        .iter()
        .map(|(name, value)| {
            let value: String = value.chars().take(240).collect();
            if name.is_empty() {
                value
            } else {
                format!("{name}  {value}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A statement's words on one line, at most [`LABEL_CHARS`] characters.
fn one_line_label(statement: &str) -> String {
    const LABEL_CHARS: usize = 200;
    let words = statement.split_whitespace().collect::<Vec<_>>().join(" ");
    if words.chars().count() <= LABEL_CHARS {
        return words;
    }
    let kept: String = words.chars().take(LABEL_CHARS - 1).collect();
    format!("{}…", kept.trim_end())
}

/// What the confirm bar says a write does, and its button: by where its result goes. A write
/// `as commit` is held as a commit made on `branch`'s head and moves nothing until it is
/// published; one `as unattached branch` never publishes; any other changes what it names.
fn confirm_words(prepared: &Prepared, branch: &str) -> (String, String) {
    let label = &prepared.label;
    match (prepared.publication, prepared.statement) {
        (Publication::Held, true) => (
            format!(
                "`{label}` holds the merge as a commit to publish or drop later; no branch changes."
            ),
            "Hold as commit".into(),
        ),
        (Publication::Held, false) => (
            format!(
                "{label} is held as a commit to publish or drop later; {branch} does not change."
            ),
            format!("Hold for {branch}"),
        ),
        (Publication::Unattached, _) => (
            format!(
                "{label} writes an unattached snapshot, which never publishes; {branch} does not change."
            ),
            "Write snapshot".into(),
        ),
        (Publication::Branch, true) => (
            format!("`{label}` changes branches."),
            "Run statement".into(),
        ),
        (Publication::Branch, false) => (
            format!("{label} changes data on {branch}."),
            format!("Run on {branch}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The confirm bar names the whole statement, its target included when it is on a line of
    /// its own.
    #[test]
    fn a_statement_is_named_whole_on_one_line() {
        assert_eq!(
            one_line_label("branch merge feature\n    into release\n"),
            "branch merge feature into release"
        );
        let long = one_line_label(&"word ".repeat(100));
        assert!(long.chars().count() <= 200 && long.ends_with('…'), "{long}");
    }

    fn param(kind: ParamKind) -> ParamDescriptor {
        ParamDescriptor {
            name: "p".into(),
            kind,
            item_kind: None,
            vector_dim: None,
            nullable: false,
        }
    }

    fn sent(param: &ParamDescriptor, text: &str) -> Result<Option<Value>, String> {
        gq_language::param_value(&param.name, &param_type(param), param.nullable, text)
    }

    /// Each catalog kind reads the text typed as the server takes it: a 64-bit integer as the
    /// text itself, so a value past the signed maximum stays exact, and an unknown kind as text.
    #[test]
    fn stored_parameters_are_read_by_their_catalog_kind() {
        assert_eq!(sent(&param(ParamKind::Int), " 42 "), Ok(Some(json!(42))));
        assert!(sent(&param(ParamKind::Int), "forty").is_err());
        assert_eq!(
            sent(&param(ParamKind::BigInt), "18446744073709551615"),
            Ok(Some(json!("18446744073709551615")))
        );
        assert_eq!(sent(&param(ParamKind::Float), "1.5"), Ok(Some(json!(1.5))));
        assert_eq!(sent(&param(ParamKind::Bool), "yes"), Ok(Some(json!(true))));
        assert_eq!(
            sent(&param(ParamKind::Date), "2026-10-14"),
            Ok(Some(json!("2026-10-14")))
        );
        assert_eq!(
            sent(&param(ParamKind::Other), "P1D"),
            Ok(Some(json!("P1D")))
        );

        let list = ParamDescriptor {
            item_kind: Some(ParamKind::Int),
            ..param(ParamKind::List)
        };
        assert_eq!(param_type(&list), "[I64]");
        assert_eq!(sent(&list, "1, 2"), Ok(Some(json!([1, 2]))));
        assert_eq!(sent(&list, "[3]"), Ok(Some(json!([3]))));

        let vector = ParamDescriptor {
            vector_dim: Some(2),
            ..param(ParamKind::Vector)
        };
        assert_eq!(param_type(&vector), "Vector(2)");
        assert_eq!(sent(&vector, "0.5, 1"), Ok(Some(json!([0.5, 1.0]))));

        // Empty is no value: refused when the parameter is required, left out when it may be.
        assert!(sent(&param(ParamKind::String), "").is_err());
        let optional = ParamDescriptor {
            nullable: true,
            ..param(ParamKind::Int)
        };
        assert_eq!(sent(&optional, "  "), Ok(None));
    }

    /// What an input shows for a value from a remembered tab, and reads back.
    #[test]
    fn values_show_as_text_the_inputs_read_back() {
        let shown = gq_language::input::edit_text;
        assert_eq!(shown(&json!("Ada")), "Ada");
        assert_eq!(shown(&json!(5)), "5");
        assert_eq!(shown(&json!(["a", "b"])), "a, b");
        assert_eq!(shown(&Value::Null), "");
        let list = ParamDescriptor {
            item_kind: Some(ParamKind::String),
            ..param(ParamKind::List)
        };
        for value in [json!(["a", "b"]), json!(["Smith, John", "x"]), json!([])] {
            assert_eq!(sent(&list, &shown(&value)), Ok(Some(value.clone())));
        }
    }

    /// The confirm bar says where a write's result goes: a declaration `as commit` is held and
    /// one `as unattached branch` is a snapshot, neither moving the branch; a merge `as commit`
    /// is held too; any other changes what it names.
    #[test]
    fn the_confirm_bar_says_where_a_write_goes() {
        let words = |label: &str, statement: bool, publication: Publication| {
            let prepared = Prepared {
                label: label.into(),
                write: true,
                statement,
                publication,
                ..Default::default()
            };
            confirm_words(&prepared, "main")
        };
        let said = |text: &str, button: &str| (text.to_owned(), button.to_owned());
        assert_eq!(
            words("add", false, Publication::Held),
            said(
                "add is held as a commit to publish or drop later; main does not change.",
                "Hold for main"
            )
        );
        assert_eq!(
            words("snap", false, Publication::Unattached),
            said(
                "snap writes an unattached snapshot, which never publishes; main does not change.",
                "Write snapshot"
            )
        );
        assert_eq!(
            words(
                "branch merge f into main as commit",
                true,
                Publication::Held
            ),
            said(
                "`branch merge f into main as commit` holds the merge as a commit to publish or \
                 drop later; no branch changes.",
                "Hold as commit"
            )
        );
        assert_eq!(
            words("add", false, Publication::Branch),
            said("add changes data on main.", "Run on main")
        );
        assert_eq!(
            words("branch delete f", true, Publication::Branch),
            said("`branch delete f` changes branches.", "Run statement")
        );
    }

    /// Headless tests of the tab in a window.
    mod views {
        use gpui::TestAppContext;
        use graph_store::{Connection, ServerEntry, ServerSource};
        use http_client::{AsyncBody, FakeHttpClient, Response};

        use super::*;

        fn session(cx: &mut TestAppContext) -> Entity<GraphSession> {
            served(cx, Vec::new()).0
        }

        const PEOPLE: &str = "node Person {\n  slug: String @key\n  name: String\n}\n";

        /// A session on a fake server: each route is a path's end and the body it answers
        /// with (`/schema` with a `schema_source`, `/commits`, `/query`, `/mutate`); every
        /// other request is answered with the branch list. What was asked for is kept as
        /// `METHOD path`.
        fn served(
            cx: &mut TestAppContext,
            routes: Vec<(&'static str, String)>,
        ) -> (Entity<GraphSession>, Arc<std::sync::Mutex<Vec<String>>>) {
            let asked: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
            let seen = asked.clone();
            let http = FakeHttpClient::create(move |request| {
                let path = request.uri().path().to_owned();
                seen.lock()
                    .unwrap()
                    .push(format!("{} {path}", request.method()));
                let body = routes
                    .iter()
                    .find(|(end, _)| path.ends_with(end))
                    .map(|(_, body)| body.clone())
                    .unwrap_or_else(|| r#"{"branches":["main"]}"#.to_owned());
                async move {
                    Ok(Response::builder()
                        .status(200)
                        .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                        .body(AsyncBody::from(body))
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
            (session, asked)
        }

        /// The `/schema` route serving `source`.
        fn schema_route(source: &str) -> (&'static str, String) {
            ("/schema", json!({ "schema_source": source }).to_string())
        }

        fn init(cx: &mut TestAppContext) {
            cx.update(|cx| {
                studio_theme::init_test(cx);
                editor::init(cx);
            });
        }

        /// A read of `columns` run as the query `people`.
        fn people() -> Prepared {
            Prepared {
                name: Some("people".into()),
                label: "people".into(),
                ..Default::default()
            }
        }

        /// A result with `columns`, one row whose values are the column names.
        fn finish(editor: &mut QueryEditor, columns: &[&str]) {
            let row: Map<String, Value> = columns
                .iter()
                .map(|c| ((*c).to_owned(), json!(c)))
                .collect();
            let output: ReadOutput = serde_json::from_value(json!({
                "query_name": "people",
                "target": {"branch": "main"},
                "row_count": 1,
                "columns": columns,
                "rows": [row],
            }))
            .unwrap();
            let at = ReadAt::Branch("main".into());
            editor.finish_read(people(), at, Ok(output), Duration::ZERO, None);
        }

        fn widths(editor: &QueryEditor) -> Vec<f32> {
            match &editor.output {
                Output::Rows(rows) => rows.widths.clone(),
                _ => panic!("no rows"),
            }
        }

        /// A result column dragged to a width keeps it in the next result that has a column of
        /// that name, wherever it is, and in the saved tab; two clicks fit it again.
        #[gpui::test]
        async fn a_dragged_result_column_keeps_its_width_by_name(cx: &mut TestAppContext) {
            cx.update(|cx| {
                studio_theme::init_test(cx);
                editor::init(cx);
            });
            let session = session(cx);
            let (editor, cx) =
                cx.add_window_view(|window, cx| QueryEditor::new(1, session, window, cx));
            editor.update(cx, |editor, cx| {
                finish(editor, &["name", "age"]);
                let fitted = widths(editor);
                editor.resize_column(0, Some(studio_ui::clamp_width(250.4)), cx);
                assert_eq!(widths(editor), vec![250., fitted[1]]);

                finish(editor, &["age", "name"]);
                assert_eq!(widths(editor), vec![fitted[1], 250.]);
                let saved = editor.serialize(cx).unwrap();
                assert_eq!(saved.data["widths"], json!({"name": 250.0}));

                editor.resize_column(1, None, cx);
                assert_eq!(widths(editor), vec![fitted[1], fitted[0]]);
                assert!(editor.column_widths.is_empty());

                // With nothing selected, ↓ lands on the first row, not the second.
                let output: ReadOutput = serde_json::from_value(json!({
                    "query_name": "people",
                    "target": {"branch": "main"},
                    "row_count": 2,
                    "columns": ["name"],
                    "rows": [{"name": "Ada"}, {"name": "Grace"}],
                }))
                .unwrap();
                let at = ReadAt::Branch("main".into());
                editor.finish_read(people(), at, Ok(output), Duration::ZERO, None);
                editor.selected = None;
                editor.move_selection(1, 0, cx);
                assert_eq!(editor.selected, Some((0, 0)));
            });
        }

        /// The confirm bar asks about one write: an edit or a pin since ⌘↵ dismisses it, and a
        /// confirmation of text that no longer matches sends nothing.
        #[gpui::test]
        async fn a_confirmed_write_is_the_one_the_bar_asked_about(cx: &mut TestAppContext) {
            cx.update(|cx| {
                studio_theme::init_test(cx);
                editor::init(cx);
            });
            let session = session(cx);
            let (editor, cx) =
                cx.add_window_view(|window, cx| QueryEditor::new(1, session, window, cx));
            let pending = |text: &str| PendingWrite {
                prepared: Prepared {
                    text: text.into(),
                    name: Some("w".into()),
                    label: "w".into(),
                    write: true,
                    ..Default::default()
                },
                branch: "main".into(),
                typed: None,
            };
            let (a, b) = (
                "query w() { insert Person { slug: \"a\" } }",
                "query w() { insert Person { slug: \"b\" } }",
            );
            editor.update_in(cx, |editor, window, cx| {
                editor.pending_write = Some(pending(a));
                editor.edited(window, cx);
                assert!(editor.pending_write.is_none(), "an edit asks again");

                // The text changed without the bar learning of it: confirming sends nothing.
                editor.set_text(b, window, cx);
                editor.pending_write = Some(pending(a));
                editor.confirm_write(&menu::Confirm, window, cx);
                assert!(!editor.writing, "nothing was sent");
                assert!(matches!(editor.output, Output::Failed(_)));

                editor.pending_write = Some(pending(b));
                editor.pin_to("c1".into(), cx);
                assert!(editor.pending_write.is_none(), "a pinned tab only reads");
            });
        }

        /// The tab's unsaved dot follows the text against the file as last read or written.
        #[gpui::test]
        async fn a_file_tab_is_unsaved_while_its_text_differs(cx: &mut TestAppContext) {
            cx.update(|cx| {
                studio_theme::init_test(cx);
                editor::init(cx);
            });
            let session = session(cx);
            let (editor, cx) =
                cx.add_window_view(|window, cx| QueryEditor::new(1, session, window, cx));
            // Each edit is heard once the update that made it ends, before the tab draws.
            let type_text = |text: &str, cx: &mut gpui::VisualTestContext| {
                editor.update_in(cx, |editor, window, cx| editor.set_text(text, window, cx));
                editor.read_with(cx, |editor, cx| editor.is_dirty(cx))
            };
            assert!(!type_text("query a() {}", cx), "no file yet");
            editor.update(cx, |editor, cx| {
                editor.set_file("a.gq".into(), Some("query a() {}".into()), cx);
                assert!(!editor.is_dirty(cx));
            });
            assert!(type_text("query b() {}", cx));
            assert!(!type_text("query a() {}", cx), "back to the saved text");
        }

        /// A problem and a run line keep their place in the text edited before the next check.
        #[gpui::test]
        async fn problems_and_run_lines_follow_edits_before_the_next_check(
            cx: &mut TestAppContext,
        ) {
            cx.update(|cx| {
                studio_theme::init_test(cx);
                editor::init(cx);
            });
            let session = session(cx);
            let (editor, cx) =
                cx.add_window_view(|window, cx| QueryEditor::new(1, session, window, cx));
            let schema = SchemaModel::parse("node Person {\n  slug: String @key\n}\n").unwrap();
            editor.update_in(cx, |editor, window, cx| {
                let text = "query a() { match { $p: Nope } return { $p.slug } }";
                editor.set_text(text, window, cx);
                let analysis = Arc::new(Analysis::new(&schema, text.to_owned()));
                let problem = analysis.problems[0].1.clone().expect("a position").start;
                editor.apply_analysis(analysis, window, cx);
                let note = "// note\n";
                editor.editor.update(cx, |editor, cx| {
                    editor.edit([(MultiBufferOffset(0)..MultiBufferOffset(0), note)], cx)
                });
                let at = editor.problem_place(0, cx).expect("anchored").start;
                assert_eq!(at, problem + note.len());
                assert_eq!(editor.offset_of(&editor.run_line_starts[0], cx), note.len());
            });
        }

        /// Text pasted over every declaration, the names kept, gets its run lines placed again
        /// at the new declarations: the old lines' anchors all collapsed into the paste.
        #[gpui::test]
        async fn run_lines_are_placed_again_when_pasted_over(cx: &mut TestAppContext) {
            cx.update(|cx| {
                studio_theme::init_test(cx);
                editor::init(cx);
            });
            let session = session(cx);
            let (editor, cx) =
                cx.add_window_view(|window, cx| QueryEditor::new(1, session, window, cx));
            let schema = SchemaModel::parse("node Person {\n  slug: String @key\n}\n").unwrap();
            editor.update_in(cx, |editor, window, cx| {
                let first = "query a() { match { $p: Person } return { $p.slug } }\n\
                             query b() { match { $p: Person } return { $p.slug } }";
                editor.set_text(first, window, cx);
                editor.apply_analysis(
                    Arc::new(Analysis::new(&schema, first.to_owned())),
                    window,
                    cx,
                );
                let revised = "// people\nquery a() { match { $p: Person } return { $p } }\n\n\
                               query b() { match { $p: Person } return { $p.slug } }";
                let end = editor.editor.read(cx).buffer().read(cx).len(cx);
                editor.editor.update(cx, |editor, cx| {
                    editor.edit([(MultiBufferOffset(0)..end, revised)], cx)
                });
                let analysis = Arc::new(Analysis::new(&schema, revised.to_owned()));
                let wanted: Vec<usize> = analysis
                    .outline
                    .declarations
                    .iter()
                    .map(|d| d.range.start)
                    .collect();
                editor.apply_analysis(analysis, window, cx);
                let placed: Vec<usize> = editor
                    .run_line_starts
                    .iter()
                    .map(|at| editor.offset_of(at, cx))
                    .collect();
                assert_eq!(placed, wanted);
            });
        }

        /// A check of text edited since is dropped whole: its underlines, parameters and run
        /// lines would be the old text's.
        #[gpui::test]
        async fn a_check_of_older_text_is_not_kept(cx: &mut TestAppContext) {
            cx.update(|cx| {
                studio_theme::init_test(cx);
                editor::init(cx);
            });
            let session = session(cx);
            let (editor, cx) =
                cx.add_window_view(|window, cx| QueryEditor::new(1, session, window, cx));
            let schema = SchemaModel::parse("node Person {\n  slug: String @key\n}\n").unwrap();
            editor.update_in(cx, |editor, window, cx| {
                let old = "query a($x: String) { match { $p: Person } return { $p.slug } }";
                editor.set_text(old, window, cx);
                let analysis = Arc::new(Analysis::new(&schema, old.to_owned()));
                editor.set_text(
                    "query b() { match { $p: Person } return { $p.slug } }",
                    window,
                    cx,
                );
                editor.apply_analysis(analysis, window, cx);
                assert!(
                    editor.analysis.as_ref().is_none_or(|kept| kept.text != old),
                    "the older text's check was kept"
                );
            });
        }

        /// A branch read runs at once and names its own branches; the plan of a load is a read
        /// of the tab's branch head, refused on a tab pinned to a commit.
        #[gpui::test]
        async fn statements_are_sent_as_the_server_reads_them(cx: &mut TestAppContext) {
            cx.update(|cx| {
                studio_theme::init_test(cx);
                editor::init(cx);
            });
            let session = session(cx);
            let (editor, cx) =
                cx.add_window_view(|window, cx| QueryEditor::new(1, session, window, cx));
            fn kind(
                text: &str,
                editor: &mut QueryEditor,
                window: &mut Window,
                cx: &mut Context<QueryEditor>,
            ) -> Result<(bool, bool), String> {
                editor.set_text(text, window, cx);
                editor
                    .prepare(false, cx)
                    .map(|prepared| (prepared.write, prepared.statement))
            }
            editor.update_in(cx, |editor, window, cx| {
                let load = "explain load csv \"s3://b/p.csv\" into node:Person mode append";
                assert_eq!(
                    kind("branch diff x into main", editor, window, cx),
                    Ok((false, true))
                );
                assert_eq!(
                    kind("branch merge x dry run", editor, window, cx),
                    Ok((false, true))
                );
                assert_eq!(kind("branch merge x", editor, window, cx), Ok((true, true)));
                assert_eq!(kind(load, editor, window, cx), Ok((false, false)));
                editor.pin_to("c1".into(), cx);
                assert!(kind(load, editor, window, cx).is_err(), "a pinned tab");
            });
        }

        /// A result wider than its view draws the columns in view and one gap for the rest, as
        /// wide as they are; scrolled to the end, the last (`docs/drawing.md`).
        #[gpui::test]
        async fn a_wide_result_draws_only_the_columns_in_view(cx: &mut TestAppContext) {
            cx.update(|cx| {
                studio_theme::init_test(cx);
                editor::init(cx);
            });
            let session = session(cx);
            let (editor, cx) =
                cx.add_window_view(|window, cx| QueryEditor::new(1, session, window, cx));
            let names: Vec<String> = (0..60).map(|i| format!("column_{i:02}")).collect();
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            editor.update_in(cx, |editor, window, _| {
                finish(editor, &names);
                let Output::Rows(rows) = &editor.output else {
                    panic!("no rows");
                };
                let places = editor.result_places(rows, window);
                assert!(places.len() < 30, "{places:?}");
                assert_eq!(places.first(), Some(&GridPlace::Column(0)));
                assert!(
                    matches!(places.last(), Some(GridPlace::Gap(_))),
                    "{places:?}"
                );
                let width = |place: &GridPlace| match *place {
                    GridPlace::Column(column) => rows.widths[column],
                    GridPlace::Gap(width) => f32::from(width),
                };
                let drawn: f32 = places.iter().map(width).sum();
                let all: f32 = rows.widths.iter().sum();
                assert!((drawn - all).abs() < 0.5, "{drawn} of {all}");

                let end = px(ROW_NUMBER_WIDTH + all) - window.viewport_size().width;
                editor.results_hscroll.set_offset(gpui::point(-end, px(0.)));
                let places = editor.result_places(rows, window);
                assert_eq!(places.last(), Some(&GridPlace::Column(59)), "{places:?}");
                assert!(
                    matches!(places.first(), Some(GridPlace::Gap(_))),
                    "{places:?}"
                );
            });
        }

        /// Typing in the query, and the check that follows it, draw the text and the toolbar
        /// again but not the rows (`docs/drawing.md`); a notify of the tab alone leaves them
        /// too, and a change of what they show draws them.
        #[gpui::test]
        async fn typing_in_the_query_leaves_the_results_as_they_were(cx: &mut TestAppContext) {
            init(cx);
            let (session, _) = served(cx, vec![schema_route(PEOPLE)]);
            cx.run_until_parked();
            assert!(
                session.read_with(cx, |session, _| session.schema_model().is_some()),
                "the schema is served"
            );
            let (editor, cx) =
                cx.add_window_view(|window, cx| QueryEditor::new(1, session, window, cx));
            let renders = |cx: &mut gpui::VisualTestContext| {
                editor.read_with(cx, |editor, cx| editor.results.read(cx).renders)
            };
            let text = "query people() { match { $p: Person } return { $p.name } }";
            editor.update_in(cx, |editor, window, cx| {
                editor.set_text(text, window, cx);
                finish(editor, &["p.name"]);
                editor.redraw_results(cx);
                // The cursor inside the declaration, after `$p.name`.
                let at = text.find("name }").unwrap() + "name".len();
                editor.editor.update(cx, |editor, cx| {
                    editor.change_selections(Default::default(), window, cx, |s| {
                        s.select_ranges(vec![MultiBufferOffset(at)..MultiBufferOffset(at)])
                    });
                });
                window.focus(&editor.editor.focus_handle(cx), cx);
            });
            // The first key also switches GPUI to keyboard input, which draws the whole
            // window once; the keys after it, and their check, are the ones counted.
            cx.simulate_input("s");
            cx.executor().advance_clock(CHECK_DELAY);
            cx.run_until_parked();
            let before = renders(cx);
            assert!(before > 0);
            cx.simulate_input("ake");
            cx.executor().advance_clock(CHECK_DELAY);
            cx.run_until_parked();
            editor.read_with(cx, |editor, cx| {
                assert!(editor.editor.read(cx).text(cx).contains("$p.namesake"));
                let analysis = editor.analysis.as_ref().expect("a check ran");
                assert!(analysis.text.contains("$p.namesake"), "of the text typed");
                assert!(!analysis.problems.is_empty(), "Person has no `namesake`");
            });
            assert_eq!(renders(cx), before, "typing or its check drew the results");

            editor.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
            assert_eq!(renders(cx), before, "a notify of the tab drew the results");
            editor.update(cx, |editor, cx| editor.move_selection(1, 0, cx));
            cx.run_until_parked();
            assert_eq!(renders(cx), before + 1);
        }

        /// What the server says a committed write did is what the tab says: a mutation `as
        /// commit` is held as a proposal and moved no branch, which its confirm bar says before
        /// it goes, and its commit is in a detached `__manifest` version, past `i64::MAX`, as the
        /// server numbers it; a `branch delete` says what went, with no counts.
        #[gpui::test]
        async fn a_write_says_what_it_did_from_the_commits_kind_and_the_outcome(
            cx: &mut TestAppContext,
        ) {
            init(cx);
            let proposal = json!({
                "branch": "main", "query_name": "add", "affected_nodes": 1, "affected_edges": 0,
                "commit": {"graph_commit_id": "01JPROPOSALAA",
                           "graph_manifest_version": 10_415_222_161_995_537_841_u64,
                           "graph_branch": null, "parent_commit_id": "c1", "published": false,
                           "created_at": 0, "kind": "unpublished"},
            });
            let (session, asked) = served(
                cx,
                vec![schema_route(PEOPLE), ("/mutate", proposal.to_string())],
            );
            cx.run_until_parked();
            let (editor, cx) =
                cx.add_window_view(|window, cx| QueryEditor::new(1, session, window, cx));
            let text = "query add() as commit { insert Person { slug: \"a\", name: \"A\" } }";
            editor.update_in(cx, |editor, window, cx| {
                editor.set_text(text, window, cx);
                editor.cursor = 0;
                editor.run(&Run, window, cx);
                let pending = editor.pending_write.as_ref().expect("a write asks first");
                // Nothing moves main: the bar says the commit is held, not that main changes.
                assert_eq!(
                    confirm_words(&pending.prepared, &pending.branch),
                    (
                        "add is held as a commit to publish or drop later; main does not change."
                            .to_owned(),
                        "Hold for main".to_owned()
                    )
                );
                editor.confirm_write(&menu::Confirm, window, cx);
            });
            cx.run_until_parked();
            assert_eq!(
                asked
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|line| line.ends_with("/mutate"))
                    .count(),
                1,
                "sent once"
            );
            editor.read_with(cx, |editor, _| {
                let Output::Written(WriteResult::Done { effect, counts, .. }) = &editor.output
                else {
                    panic!("no outcome");
                };
                assert!(matches!(effect, WriteEffect::Proposal { .. }), "{effect:?}");
                assert!(!effect.title().contains("Committed"), "{}", effect.title());
                assert_eq!(*counts, Some((1, 0)));
                let note = &editor.last_runs["add"];
                assert!(note.text.starts_with("Held as commit "), "{}", note.text);
            });
        }

        /// A destructive statement names what it destroys and goes only with that name typed:
        /// ↵ with other words in the field sends nothing. A `delete` written in the tab waits
        /// for its type's name too, also before the schema is read (this server serves none):
        /// what it destroys is in its parse alone.
        #[gpui::test]
        async fn a_destructive_write_waits_for_its_name(cx: &mut TestAppContext) {
            init(cx);
            let (session, asked) = served(cx, Vec::new());
            cx.run_until_parked();
            let (editor, cx) =
                cx.add_window_view(|window, cx| QueryEditor::new(1, session, window, cx));
            let mutations = || {
                asked
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|line| line.ends_with("/mutate"))
                    .count()
            };
            editor.update_in(cx, |editor, window, cx| {
                editor.set_text("branch delete feature", window, cx);
                editor.run(&Run, window, cx);
                let typed = editor
                    .pending_write
                    .as_ref()
                    .and_then(|pending| pending.typed.as_ref())
                    .expect("typed to confirm");
                assert_eq!(typed.phrase, "feature");
                assert!(
                    typed.what.starts_with("Deletes branch feature"),
                    "{}",
                    typed.what
                );
                let field = typed.field.clone();
                editor.confirm_write(&menu::Confirm, window, cx);
                assert!(!editor.writing, "nothing was sent");
                assert!(
                    editor
                        .pending_write
                        .as_ref()
                        .and_then(|pending| pending.typed.as_ref())
                        .is_some_and(|typed| typed.refused),
                    "it still waits, and says what to type"
                );
                field.update(cx, |field, cx| field.set_text("feature", window, cx));
                editor.confirm_write(&menu::Confirm, window, cx);
                assert!(editor.writing, "sent with its name typed");
            });
            cx.run_until_parked();
            assert_eq!(mutations(), 1);

            // A declaration that deletes, checked against no schema: its type's name is typed.
            editor.update_in(cx, |editor, window, cx| {
                assert!(editor.schema(cx).is_none(), "no schema was read");
                editor.set_text(
                    "query purge() {\n  delete Person where slug = \"a\"\n}",
                    window,
                    cx,
                );
                editor.cursor = 0;
                editor.run(&Run, window, cx);
                let typed = editor
                    .pending_write
                    .as_ref()
                    .and_then(|pending| pending.typed.as_ref())
                    .expect("typed to confirm");
                assert_eq!(typed.phrase, "Person");
                assert!(
                    typed
                        .what
                        .starts_with("Deletes what its `where` matches of Person"),
                    "{}",
                    typed.what
                );
                editor.confirm_write(&menu::Confirm, window, cx);
                assert!(!editor.writing, "nothing was sent");
            });
            cx.run_until_parked();
            assert_eq!(mutations(), 1);

            // A write that destroys nothing Studio cannot put back is confirmed with ↵ alone.
            editor.update_in(cx, |editor, window, cx| {
                editor.set_text("branch create feature", window, cx);
                editor.run(&Run, window, cx);
                assert!(
                    editor
                        .pending_write
                        .as_ref()
                        .is_some_and(|pending| pending.typed.is_none())
                );
            });
        }

        /// An answer over the read's cap is said to be, with what to do; nothing of it shows.
        #[gpui::test]
        async fn an_answer_over_the_cap_says_so_and_shows_nothing(cx: &mut TestAppContext) {
            init(cx);
            let session = session(cx);
            let (editor, cx) =
                cx.add_window_view(|window, cx| QueryEditor::new(1, session, window, cx));
            editor.update(cx, |editor, _| {
                let at = ReadAt::Branch("main".into());
                let too_large = Err(ReadError::TooLarge {
                    limit: 32 * 1024 * 1024,
                });
                editor.finish_read(people(), at, too_large, Duration::ZERO, None);
                let Output::Failed(failure) = &editor.output else {
                    panic!("no failure");
                };
                assert!(failure.title.contains("over 32 MB"), "{}", failure.title);
                assert!(
                    failure
                        .hint
                        .as_deref()
                        .is_some_and(|hint| hint.contains("`limit`")),
                    "{:?}",
                    failure.hint
                );
                assert!(editor.last_runs["people"].failed);
            });
        }

        /// A statement read names no branch of the tab's; a diff says the commits it compared
        /// and offers its next page, which reads the same statement with the server's token.
        #[gpui::test]
        async fn a_branch_diff_names_its_sides_and_reads_its_next_page(cx: &mut TestAppContext) {
            init(cx);
            let diff = json!({
                "query_name": "branch diff",
                "target": {"branch": null, "snapshot": null},
                "row_count": 1,
                "columns": ["kind", "type", "id", "op", "before", "after"],
                "rows": [{"kind": "node", "type": "Person", "id": "p1", "op": "insert"}],
                "page": {"from_commit_id": "01JFROMAAAAA", "to_commit_id": "01JTOAAAAAAA",
                         "next_page_token": "t2"},
            });
            let (session, _) = served(cx, vec![("/query", diff.to_string())]);
            cx.run_until_parked();
            session.update(cx, |session, cx| session.set_branch("dev".into(), cx));
            let (editor, cx) =
                cx.add_window_view(|window, cx| QueryEditor::new(1, session, window, cx));
            editor.update_in(cx, |editor, window, cx| {
                editor.set_text("branch diff feature into main limit 1", window, cx);
                editor.run(&Run, window, cx);
                let Output::Running { label } = &editor.output else {
                    panic!("not running");
                };
                assert_eq!(
                    label.as_ref(),
                    "Running branch diff feature into main limit 1…"
                );
            });
            cx.run_until_parked();
            editor.update_in(cx, |editor, window, cx| {
                let Output::Rows(rows) = &editor.output else {
                    panic!("no rows");
                };
                assert_eq!(rows.read_at, ReadAt::Statement, "not the tab's branch, dev");
                assert_eq!(rows.page_number, 1);
                let next = rows.next.as_ref().expect("a next page");
                assert_eq!(
                    next.text,
                    "branch diff feature into main after page \"t2\" limit 1"
                );
                assert_eq!(next.page, 2);
                editor.next_page(&NextPage, window, cx);
                let Output::Running { label } = &editor.output else {
                    panic!("not reading the next page");
                };
                assert_eq!(
                    label.as_ref(),
                    "Reading page 2 of branch diff feature into main limit 1…"
                );
            });
            cx.run_until_parked();
            editor.read_with(cx, |editor, _| {
                let Output::Rows(rows) = &editor.output else {
                    panic!("no rows");
                };
                assert_eq!(rows.page_number, 2);
            });
        }

        /// The snapshot picker reads the branch's commits when it opens, not when the tab
        /// does; what it read is kept until a commit lands, and read again then only while the
        /// picker is open.
        #[gpui::test]
        async fn commits_are_read_when_the_picker_opens(cx: &mut TestAppContext) {
            init(cx);
            let page = json!({"commits": [{"graph_commit_id": "01JCOMMITAAAA",
                "graph_manifest_version": 2, "created_at": 0, "kind": "published"}]});
            let (session, asked) = served(cx, vec![("/commits", page.to_string())]);
            cx.run_until_parked();
            let (editor, cx) =
                cx.add_window_view(|window, cx| QueryEditor::new(1, session.clone(), window, cx));
            let reads = || {
                asked
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|line| line.contains("/commits"))
                    .count()
            };
            cx.run_until_parked();
            assert_eq!(reads(), 0, "opening the tab reads no commits");
            // As the picker's button opens it: the menu built from the tab, then the tab told.
            let open = |cx: &mut gpui::VisualTestContext| {
                let this = editor.downgrade();
                let menu = cx.update(|window, cx| QueryEditor::picker_menu(this, window, cx));
                let opened = menu.downgrade();
                editor.update_in(cx, |editor, window, cx| {
                    editor.picker_opened(opened, window, cx)
                });
                menu
            };
            let menu = open(cx);
            cx.run_until_parked();
            assert_eq!(reads(), 1);
            editor.read_with(cx, |editor, _| assert_eq!(editor.commits.len(), 1));
            drop(menu);
            let menu = open(cx);
            cx.run_until_parked();
            assert_eq!(reads(), 1, "what was read is kept");
            // A commit lands while the picker is open: it reads them again.
            session.update(cx, |_, cx| {
                cx.emit(SessionEvent::Committed {
                    branch: "main".into(),
                })
            });
            cx.run_until_parked();
            assert_eq!(reads(), 2);
            drop(menu);
            cx.run_until_parked();
            // Closed, it waits until it opens.
            session.update(cx, |_, cx| {
                cx.emit(SessionEvent::Committed {
                    branch: "main".into(),
                })
            });
            cx.run_until_parked();
            assert_eq!(reads(), 2);
            let _menu = open(cx);
            cx.run_until_parked();
            assert_eq!(reads(), 3);
        }

        /// The result grid is reached by a key and its cell cursor moves by column too, so a
        /// cell of any column is copied from the keyboard.
        #[gpui::test]
        async fn the_result_grid_is_reached_and_crossed_by_keys(cx: &mut TestAppContext) {
            init(cx);
            let session = session(cx);
            let (editor, cx) =
                cx.add_window_view(|window, cx| QueryEditor::new(1, session, window, cx));
            editor.update_in(cx, |editor, window, cx| {
                finish(editor, &["name", "title", "born"]);
                window.focus(&editor.editor.focus_handle(cx), cx);
                editor.toggle_results_focus(&ToggleResultsFocus, window, cx);
                assert!(editor.results_focus.is_focused(window));
                assert_eq!(editor.selected, Some((0, 0)));
            });
            editor.update_in(cx, |editor, window, cx| {
                editor.move_selection(0, 1, cx);
                editor.move_selection(0, 1, cx);
                editor.move_selection(0, 1, cx);
                assert_eq!(editor.selected, Some((0, 2)), "held at the last column");
                editor.move_selection(0, -1, cx);
                editor.copy_cell(&CopyCell, window, cx);
            });
            assert_eq!(
                cx.read_from_clipboard().and_then(|item| item.text()),
                Some("title".into())
            );
            editor.update_in(cx, |editor, window, cx| {
                editor.toggle_results_focus(&ToggleResultsFocus, window, cx);
                assert!(editor.editor.focus_handle(cx).is_focused(window));
            });
        }

        /// The problems are gone to by key, in the order of the text and round again, and the
        /// suggested fix of the one gone to is applied by key.
        #[gpui::test]
        async fn problems_are_gone_to_and_fixed_by_keys(cx: &mut TestAppContext) {
            init(cx);
            let session = session(cx);
            let (editor, cx) =
                cx.add_window_view(|window, cx| QueryEditor::new(1, session, window, cx));
            let schema = SchemaModel::parse(PEOPLE).unwrap();
            let text = "query people { match { $p: Person } return { $p.name } }";
            editor.update_in(cx, |editor, window, cx| {
                editor.set_text(text, window, cx);
                editor.apply_analysis(Arc::new(Analysis::new(&schema, text.into())), window, cx);
                assert_eq!(editor.analysis.as_ref().unwrap().problems.len(), 1);
                editor.cursor = text.len();
                editor.next_problem(&NextProblem, window, cx);
                assert_eq!(editor.selected_problem, Some(0), "round to the first");
                assert_eq!(editor.fix_target(cx), Some(0));
                editor.apply_fix_action(&ApplyFix, window, cx);
                assert_eq!(
                    editor.editor.read(cx).text(cx),
                    "query people() { match { $p: Person } return { $p.name } }"
                );
            });
        }

        /// A parameter's input goes once no declaration declares it, but not while one that
        /// does not parse may.
        #[gpui::test]
        async fn inputs_of_parameters_no_longer_declared_are_dropped(cx: &mut TestAppContext) {
            init(cx);
            let session = session(cx);
            let (editor, cx) =
                cx.add_window_view(|window, cx| QueryEditor::new(1, session, window, cx));
            let schema = SchemaModel::parse(PEOPLE).unwrap();
            let check = |text: &str,
                         editor: &mut QueryEditor,
                         window: &mut Window,
                         cx: &mut Context<QueryEditor>| {
                editor.set_text(text, window, cx);
                editor.cursor = 0;
                editor.apply_analysis(Arc::new(Analysis::new(&schema, text.into())), window, cx);
            };
            editor.update_in(cx, |editor, window, cx| {
                check(
                    "query a($slug: String) { match { $p: Person { slug: $slug } } return { $p.name } }",
                    editor,
                    window,
                    cx,
                );
                assert!(editor.params.contains_key("slug"));
                check(
                    "query a() { match { $p: Person } return { $p.name } }\nquery b( {",
                    editor,
                    window,
                    cx,
                );
                assert!(editor.params.contains_key("slug"), "kept while one does not parse");
                check(
                    "query a() { match { $p: Person } return { $p.name } }",
                    editor,
                    window,
                    cx,
                );
                assert!(!editor.params.contains_key("slug"));
            });
        }

        /// Run takes what the check of the same text and schema said of the declaration, and
        /// checks the text itself only when that check is of other text.
        #[gpui::test]
        async fn a_run_reuses_the_check_of_the_same_text(cx: &mut TestAppContext) {
            init(cx);
            let (session, _) = served(cx, vec![schema_route(PEOPLE)]);
            cx.run_until_parked();
            let schema = session.read_with(cx, |session, _| session.schema_model().unwrap());
            let (editor, cx) =
                cx.add_window_view(|window, cx| QueryEditor::new(1, session, window, cx));
            let text = "query people() { match { $p: Person } return { $p.name } }";
            editor.update_in(cx, |editor, window, cx| {
                editor.set_text(text, window, cx);
                editor.cursor = 0;
                let mut held = Analysis::new(&schema, text.into());
                // Marked, to tell what was held from what a check now would say.
                if let Some(Some(info)) = held.queries.first_mut() {
                    info.columns[0].column = "held".into();
                }
                editor.apply_analysis(Arc::new(held), window, cx);
                let prepared = editor.prepare(false, cx).unwrap();
                assert_eq!(prepared.columns.unwrap()[0].column, "held");

                editor.set_text(&text.replace("$p.name", "$p.slug"), window, cx);
                let prepared = editor.prepare(false, cx).unwrap();
                assert_eq!(
                    prepared.columns.unwrap()[0].column,
                    "p.slug",
                    "checked again"
                );
            });
        }

        /// A scratch folder for one test's files.
        fn scratch(name: &str) -> std::path::PathBuf {
            let dir =
                std::env::temp_dir().join(format!("query-editor-{name}-{}", std::process::id()));
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        /// A remembered file tab meets its file as it is now. Still being read, it runs and
        /// saves nothing and keeps what it remembered. A saved one shows the file and is
        /// remembered without its text; a draft typed over the text the file still holds is
        /// kept; a draft over a file changed since is kept too, the bar says so, and ⌘S writes
        /// nothing until the person saves over the file.
        #[gpui::test]
        async fn a_remembered_draft_meets_its_file_as_it_is_now(cx: &mut TestAppContext) {
            init(cx);
            let session = session(cx);
            let path = scratch("remembered").join("people.gq");
            let (a, b, c) = ("query a() {}", "query b() {}", "query c() {}");
            std::fs::write(&path, a).unwrap();
            let opened = path.clone();
            let (editor, cx) = cx.add_window_view(move |window, cx| {
                QueryEditor::reading(1, session, opened, None, window, cx)
            });
            let draft = |text: &str, over: Option<&str>| Draft {
                text: text.into(),
                based_on: over.map(content_hash),
                saved: false,
            };
            let reopen = |editor: &mut QueryEditor, draft: Option<Draft>| {
                editor.loading = Some(Loading { draft });
                editor.disk_change = None;
                editor.file_error = None;
            };
            editor.update_in(cx, |editor, window, cx| {
                assert!(!editor.is_dirty(cx));
                assert!(editor.serialize(cx).unwrap().data.get("text").is_none());
                editor.save_now(window, cx);
                assert!(!editor.saving, "nothing is saved while the file is read");
                assert!(editor.file_read(Some(a.into()), window, cx));
                assert_eq!(editor.editor.read(cx).text(cx), a);
            });
            cx.run_until_parked();
            editor.update_in(cx, |editor, window, cx| {
                assert!(!editor.is_dirty(cx));
                let saved = editor.serialize(cx).unwrap();
                assert_eq!(saved.data["path"], json!(path.to_string_lossy()));
                // Its text is kept as the file's, for a restart that finds the file gone.
                assert_eq!(saved.data["saved"], json!(true));
                assert_eq!(saved.data["text"], json!(a));

                // A draft over the text the file still holds is kept, and remembered with it.
                reopen(editor, Some(draft(b, Some(a))));
                assert_eq!(
                    editor.serialize(cx).unwrap().data["text"],
                    json!(b),
                    "kept while the file is read"
                );
                assert!(editor.file_read(Some(a.into()), window, cx));
                assert_eq!(editor.editor.read(cx).text(cx), b);
                assert!(editor.is_dirty(cx) && editor.disk_change.is_none());
                let saved = editor.serialize(cx).unwrap();
                assert_eq!(saved.data["text"], json!(b));
                assert_eq!(saved.data["based_on"], json!(content_hash(a)));

                // The file changed since: the draft stays, and says so.
                reopen(editor, Some(draft(b, Some(a))));
                assert!(editor.file_read(Some(c.into()), window, cx));
                assert_eq!(editor.editor.read(cx).text(cx), b);
                assert!(
                    editor
                        .disk_change
                        .as_ref()
                        .is_some_and(|change| !change.asked)
                );
                assert_eq!(
                    editor.serialize(cx).unwrap().data["based_on"],
                    json!(content_hash(a)),
                    "still what it was typed over"
                );
            });
            std::fs::write(&path, c).unwrap();
            editor.update_in(cx, |editor, window, cx| editor.save_now(window, cx));
            cx.run_until_parked();
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                c,
                "⌘S wrote nothing"
            );
            editor.update_in(cx, |editor, window, cx| {
                assert!(
                    editor
                        .disk_change
                        .as_ref()
                        .is_some_and(|change| change.asked)
                );
                assert!(editor.file_focus.is_focused(window), "⌘S went to the bar");
                editor.save_over(window, cx);
            });
            cx.run_until_parked();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), b, "saved over it");
            editor.update_in(cx, |editor, window, cx| {
                assert!(editor.disk_change.is_none() && !editor.is_dirty(cx));

                // Remembered by a build that kept no hash: a draft that is the file is the
                // file; any other may be older, and says so.
                reopen(editor, Some(draft(b, None)));
                assert!(editor.file_read(Some(b.into()), window, cx));
                assert!(editor.disk_change.is_none());
                reopen(editor, Some(draft(a, None)));
                assert!(editor.file_read(Some(b.into()), window, cx));
                assert!(editor.disk_change.is_some());
                editor.take_disk_text(window, cx);
                assert_eq!(editor.editor.read(cx).text(cx), b, "the file's text taken");

                // A file that cannot be read keeps the draft; with nothing remembered, there is
                // no tab.
                reopen(editor, Some(draft(a, Some(b))));
                assert!(editor.file_read(None, window, cx));
                assert_eq!(editor.editor.read(cx).text(cx), a);
                assert!(editor.file_gone.is_some());
                editor.file_gone = None;
                reopen(editor, None);
                assert!(!editor.file_read(None, window, cx));
            });
        }

        /// A file changed elsewhere while its tab is open: a tab with nothing unsaved takes
        /// the file's text when the editor takes the keyboard; one with a draft is not written
        /// over by ⌘S, which finds the file holds other text, keeps the draft and asks.
        #[gpui::test]
        async fn an_open_file_changed_elsewhere_is_not_written_over(cx: &mut TestAppContext) {
            init(cx);
            let session = session(cx);
            let path = scratch("changed").join("people.gq");
            std::fs::write(&path, "query a() {}").unwrap();
            let (editor, cx) = cx.add_window_view(|window, cx| {
                QueryEditor::with_text(1, session, "query a() {}", window, cx)
            });
            editor.update(cx, |editor, cx| {
                editor.set_file(path.clone(), Some("query a() {}".into()), cx)
            });
            std::fs::write(&path, "query b() {}").unwrap();
            editor.update_in(cx, |editor, window, cx| editor.check_disk(window, cx));
            cx.run_until_parked();
            editor.update_in(cx, |editor, window, cx| {
                assert_eq!(editor.editor.read(cx).text(cx), "query b() {}");
                assert!(!editor.is_dirty(cx));
                editor.set_text("query c() {}", window, cx);
            });
            cx.run_until_parked();
            std::fs::write(&path, "query d() {}").unwrap();
            editor.update_in(cx, |editor, window, cx| {
                assert!(editor.is_dirty(cx));
                editor.save_now(window, cx);
            });
            cx.run_until_parked();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "query d() {}");
            editor.update_in(cx, |editor, window, cx| {
                assert_eq!(
                    editor.editor.read(cx).text(cx),
                    "query c() {}",
                    "the draft stays"
                );
                assert!(
                    editor
                        .disk_change
                        .as_ref()
                        .is_some_and(|change| change.asked)
                );
                assert_eq!(
                    editor.serialize(cx).unwrap().data["based_on"],
                    json!(content_hash("query b() {}"))
                );
                editor.save_over(window, cx);
            });
            cx.run_until_parked();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "query c() {}");
            editor.read_with(cx, |editor, cx| {
                assert!(editor.disk_change.is_none() && !editor.is_dirty(cx));
            });
        }

        /// A tab given text, even none, keeps it when the schema arrives; only a new query is
        /// filled with the example. An empty file's tab is not changed by it.
        #[gpui::test]
        async fn a_tab_given_no_text_stays_empty_when_the_schema_arrives(cx: &mut TestAppContext) {
            init(cx);
            let (session, _) = served(cx, vec![schema_route(PEOPLE)]);
            assert!(
                session.read_with(cx, |session, _| session.schema_model().is_none()),
                "the schema is not read yet"
            );
            let (fresh_session, file_session) = (session.clone(), session.clone());
            let (emptied, cx) = cx.add_window_view(move |window, cx| {
                QueryEditor::with_text(1, session, "", window, cx)
            });
            let (fresh, file) = emptied.update_in(cx, |_, window, cx| {
                let fresh = cx.new(|cx| QueryEditor::new(2, fresh_session, window, cx));
                let file = cx.new(|cx| {
                    let mut file =
                        QueryEditor::reading(3, file_session, "empty.gq".into(), None, window, cx);
                    file.file_read(Some(String::new()), window, cx);
                    file
                });
                (fresh, file)
            });
            cx.run_until_parked();
            emptied.read_with(cx, |emptied, cx| {
                assert!(emptied.schema(cx).is_some(), "the schema arrived");
                assert_eq!(emptied.editor.read(cx).text(cx), "");
            });
            fresh.read_with(cx, |fresh, cx| {
                assert!(
                    fresh.editor.read(cx).text(cx).contains("query"),
                    "the example"
                );
            });
            file.read_with(cx, |file, cx| {
                assert_eq!(file.editor.read(cx).text(cx), "");
                assert!(!file.is_dirty(cx));
            });
        }

        /// Values a remembered stored-query tab brings back before the graph's catalog is
        /// read wait for their inputs, and are saved with the tab meanwhile; once an input is
        /// made, what it holds is saved.
        #[gpui::test]
        async fn values_waiting_for_the_catalog_are_saved_with_the_tab(cx: &mut TestAppContext) {
            init(cx);
            let session = session(cx);
            let entry = QueryCatalogEntry {
                name: "people_titled".into(),
                tool_name: None,
                description: None,
                instruction: None,
                mutation: false,
                params: Vec::new(),
            };
            let (editor, cx) = cx.add_window_view(|window, cx| {
                QueryEditor::new_stored(1, session, entry, window, cx)
            });
            editor.update_in(cx, |editor, window, cx| {
                let values = json!({"title": "Founder", "limit": 5});
                editor.fill_params(values.as_object().unwrap(), window, cx);
                assert!(editor.params.is_empty(), "no input before the catalog");
                let saved = editor.serialize(cx).unwrap();
                assert_eq!(saved.data["values"], values);

                if let Some(stored) = &mut editor.stored {
                    stored.entry.params = vec![ParamDescriptor {
                        name: "title".into(),
                        ..param(ParamKind::String)
                    }];
                }
                editor.sync_stored_params(window, cx);
                let input = editor.params["title"].clone();
                assert_eq!(input.read(cx).text(cx), "Founder");
                input.update(cx, |input, cx| input.set_text("CEO", window, cx));
                let saved = editor.serialize(cx).unwrap();
                assert_eq!(saved.data["values"], json!({"title": "CEO", "limit": 5}));
            });
        }

        /// The Run line above a declaration runs that one, though the one before it lacks its
        /// `}` and runs on to it.
        #[gpui::test]
        async fn the_run_line_runs_the_declaration_below_it(cx: &mut TestAppContext) {
            init(cx);
            let session = session(cx);
            let (editor, cx) =
                cx.add_window_view(|window, cx| QueryEditor::new(1, session, window, cx));
            let schema = SchemaModel::parse(PEOPLE).unwrap();
            let text = "query a() {\n  match { $p: Person }\n  return { $p.name }\n\n\
                        query b() {\n  match { $p: Person }\n  return { $p.slug }\n}\n";
            editor.update_in(cx, |editor, window, cx| {
                editor.set_text(text, window, cx);
                editor.apply_analysis(Arc::new(Analysis::new(&schema, text.into())), window, cx);
                assert_eq!(editor.run_line_names, ["a", "b"]);
                editor.run_declaration(1, window, cx);
                let Output::Running { label } = &editor.output else {
                    panic!("not running");
                };
                assert_eq!(label.as_ref(), "Running b on main…");
            });
        }

        /// A read copied and changed to a delete, its name kept, asks before it writes: it is
        /// checked as itself, not as the read named like it, and nothing goes to `/query`.
        #[gpui::test]
        async fn a_write_named_like_a_read_asks_before_it_writes(cx: &mut TestAppContext) {
            init(cx);
            let (session, asked) = served(cx, vec![schema_route(PEOPLE)]);
            cx.run_until_parked();
            let schema = session.read_with(cx, |session, _| session.schema_model().unwrap());
            // The session reads the graph's counts through `/query` on its own.
            let reads = || {
                asked
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|line| line.ends_with("/query"))
                    .count()
            };
            let before = reads();
            let (editor, cx) =
                cx.add_window_view(|window, cx| QueryEditor::new(1, session, window, cx));
            let text = "query people() { match { $p: Person } return { $p.name } }\n\n\
                        query people($slug: String) {\n  delete Person where slug = $slug\n}";
            editor.update_in(cx, |editor, window, cx| {
                editor.set_text(text, window, cx);
                editor.cursor = text.find("delete").unwrap();
                editor.apply_analysis(Arc::new(Analysis::new(&schema, text.into())), window, cx);
                let input = editor.params["slug"].clone();
                input.update(cx, |input, cx| input.set_text("a", window, cx));
                editor.run(&Run, window, cx);
                let pending = editor.pending_write.as_ref().expect("a write asks first");
                assert!(pending.prepared.write);
                assert!(
                    pending.typed.is_some(),
                    "a delete waits for its type's name"
                );
            });
            cx.run_until_parked();
            assert_eq!(reads(), before, "nothing was read");
        }

        /// Typing, and a check after it that finds the same problems, leave the problems list
        /// as it was (`docs/drawing.md`); going to a problem draws it.
        #[gpui::test]
        async fn typing_leaves_the_problems_as_they_were(cx: &mut TestAppContext) {
            init(cx);
            let (session, _) = served(cx, vec![schema_route(PEOPLE)]);
            cx.run_until_parked();
            let (editor, cx) =
                cx.add_window_view(|window, cx| QueryEditor::new(1, session, window, cx));
            let renders = |cx: &mut gpui::VisualTestContext| {
                editor.read_with(cx, |editor, cx| editor.problems.read(cx).renders)
            };
            let text = "query people { match { $p: Person } return { $p.name } }";
            editor.update_in(cx, |editor, window, cx| {
                editor.set_text(text, window, cx);
                editor.editor.update(cx, |editor, cx| {
                    editor.change_selections(Default::default(), window, cx, |s| {
                        let end = MultiBufferOffset(text.len());
                        s.select_ranges(vec![end..end])
                    });
                });
                window.focus(&editor.editor.focus_handle(cx), cx);
            });
            // The first key also switches GPUI to keyboard input, which draws the whole
            // window once; the keys after it, and their check, are the ones counted.
            cx.simulate_input(" ");
            cx.executor().advance_clock(CHECK_DELAY);
            cx.run_until_parked();
            let before = renders(cx);
            assert!(before > 0, "the list is drawn");
            cx.simulate_input("// x");
            cx.executor().advance_clock(CHECK_DELAY);
            cx.run_until_parked();
            editor.read_with(cx, |editor, _| {
                let analysis = editor.analysis.as_ref().expect("a check ran");
                assert!(analysis.text.ends_with("// x"), "of the text typed");
                assert_eq!(analysis.problems.len(), 1);
            });
            assert_eq!(renders(cx), before, "typing or its check drew the problems");
            editor.update_in(cx, |editor, window, cx| {
                editor.next_problem(&NextProblem, window, cx)
            });
            cx.run_until_parked();
            assert_eq!(renders(cx), before + 1);
        }

        /// Rows read at the branch head offer Refresh once the head moves past the commit they
        /// were read at, and read nothing by themselves; Refresh reads them again. A head
        /// moving under a write's outcome sends nothing.
        #[gpui::test]
        async fn rows_offer_refresh_when_their_branch_moves(cx: &mut TestAppContext) {
            init(cx);
            let rows = json!({
                "query_name": "people", "target": {"branch": "main"}, "row_count": 1,
                "columns": ["p.name"], "rows": [{"p.name": "Ada"}],
                "graph_commit_id": "01JHEADAAAAA",
            });
            let (session, asked) = served(cx, vec![("/query", rows.to_string())]);
            cx.run_until_parked();
            let moved = |head: &str, branch: &str| SessionEvent::HeadMoved {
                branch: branch.into(),
                head: head.into(),
            };
            let reads = || {
                asked
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|line| line.ends_with("/query"))
                    .count()
            };
            let (editor, cx) =
                cx.add_window_view(|window, cx| QueryEditor::new(1, session.clone(), window, cx));
            editor.update_in(cx, |editor, window, cx| {
                editor.set_text(
                    "query people() { match { $p: Person } return { $p.name } }",
                    window,
                    cx,
                );
                editor.cursor = 0;
                editor.run(&Run, window, cx);
            });
            cx.run_until_parked();
            assert_eq!(reads(), 1);
            let newer = |cx: &mut gpui::VisualTestContext| {
                editor.read_with(cx, |editor, cx| {
                    let Output::Rows(rows) = &editor.output else {
                        panic!("no rows");
                    };
                    assert_eq!(rows.newer, editor.is_stale(cx));
                    rows.newer
                })
            };
            assert!(!newer(cx));
            session.update(cx, |_, cx| cx.emit(moved("01JHEADAAAAA", "main")));
            session.update(cx, |_, cx| cx.emit(moved("01JOTHERAAAA", "other")));
            cx.run_until_parked();
            assert!(!newer(cx), "the commit read at, or another branch");
            session.update(cx, |_, cx| cx.emit(moved("01JNEXTAAAAA", "main")));
            cx.run_until_parked();
            assert!(newer(cx), "the branch moved on");
            assert_eq!(reads(), 1, "nothing read by itself");
            editor.update(cx, |editor, cx| editor.refresh_rows(cx));
            cx.run_until_parked();
            assert_eq!(reads(), 2);
            assert!(!newer(cx));

            editor.update(cx, |editor, cx| {
                editor.replace_output(
                    Output::Written(WriteResult::Unknown {
                        reason: "the connection closed".into(),
                    }),
                    cx,
                )
            });
            session.update(cx, |_, cx| cx.emit(moved("01JLASTAAAAA", "main")));
            editor.update(cx, |editor, cx| editor.refresh_rows(cx));
            cx.run_until_parked();
            assert_eq!(reads(), 2, "a write is never sent again");
            editor.read_with(cx, |editor, cx| assert!(!editor.is_stale(cx)));
        }

        /// A refused read's diagnostic in contract 0.14's shape points into the query, and its
        /// suggested edit applies where it was meant after an edit above it.
        #[gpui::test]
        async fn a_refused_reads_suggested_edit_applies_to_the_query(cx: &mut TestAppContext) {
            init(cx);
            let session = session(cx);
            let (editor, cx) =
                cx.add_window_view(|window, cx| QueryEditor::new(1, session, window, cx));
            let text = "query people { match { $p: Person } return { $p.name } }";
            let at = text.find(" {").unwrap();
            editor.update_in(cx, |editor, window, cx| {
                editor.set_text(text, window, cx);
                editor.cursor = 0;
                let prepared = editor.prepare(false, cx).unwrap();
                assert_eq!(prepared.text, text);
                let refused = ReadError::Refused(graph_client::ApiErrorSummary {
                    status: 400,
                    message: "parse error".into(),
                    code: Some(ErrorCode::BadRequest),
                    diagnostic: Some(json!({
                        "code": "Q002", "position": {"line": 1, "column": 13, "byte": at},
                        "expected": "a parameter list", "fix": "add `()`",
                        "suggestion": {"applicability": "machine_applicable",
                                       "edits": [{"start": at, "end": at, "replacement": "()"}]},
                    })),
                    change_diff_refusal: None,
                    challenge: None,
                });
                let sent = prepared.sent.clone();
                let at = ReadAt::Branch("main".into());
                editor.finish_read(prepared, at, Err(refused), Duration::ZERO, None);
                editor.hold_diagnostic(sent.as_ref(), cx);
                let Output::Failed(failure) = &editor.output else {
                    panic!("not refused");
                };
                let diagnostic = failure
                    .diagnostic
                    .as_ref()
                    .expect("the server's diagnostic");
                assert_eq!(diagnostic.code, "Q002");
                assert_eq!(diagnostic.suggestion.as_deref(), Some("insert `()`"));
                editor.editor.update(cx, |editor, cx| {
                    editor.edit(
                        [(MultiBufferOffset(0)..MultiBufferOffset(0), "// people\n")],
                        cx,
                    )
                });
                editor.apply_server_fix(window, cx);
                assert_eq!(
                    editor.editor.read(cx).text(cx),
                    "// people\nquery people() { match { $p: Person } return { $p.name } }"
                );
                assert!(editor.diagnostic_edits.is_empty(), "applied once");
            });
        }

        /// Runs the tab's query and has the server refuse it with a diagnostic whose
        /// suggestion inserts `()` at `at` of the text sent, as a missing parameter list's does.
        fn refused_with_an_insertion(
            editor: &mut QueryEditor,
            at: usize,
            cx: &mut Context<QueryEditor>,
        ) {
            let prepared = editor.prepare(false, cx).unwrap();
            let refused = ReadError::Refused(graph_client::ApiErrorSummary {
                status: 400,
                message: "parse error".into(),
                code: Some(ErrorCode::BadRequest),
                diagnostic: Some(json!({
                    "code": "Q002", "position": {"line": 1, "column": 13, "byte": at},
                    "expected": "a parameter list",
                    "suggestion": {"applicability": "machine_applicable",
                                   "edits": [{"start": at, "end": at, "replacement": "()"}]},
                })),
                change_diff_refusal: None,
                challenge: None,
            });
            let sent = prepared.sent.clone();
            let read_at = ReadAt::Branch("main".into());
            editor.finish_read(prepared, read_at, Err(refused), Duration::ZERO, None);
            editor.hold_diagnostic(sent.as_ref(), cx);
        }

        /// The server's suggested edit applies only while the text around its place reads as
        /// it was run: `()` typed where it would insert `()`, or the whole text replaced (a
        /// clean tab taking its file's text), takes Apply back, and Apply then changes nothing.
        #[gpui::test]
        async fn a_suggested_edit_is_not_applied_over_text_changed_where_it_goes(
            cx: &mut TestAppContext,
        ) {
            init(cx);
            let session = session(cx);
            let (editor, cx) =
                cx.add_window_view(|window, cx| QueryEditor::new(1, session, window, cx));
            let text = "query people { match { $p: Person } return { $p.name } }";
            let at = text.find(" {").unwrap();
            let fixed = "query people() { match { $p: Person } return { $p.name } }";
            editor.update_in(cx, |editor, window, cx| {
                editor.set_text(text, window, cx);
                editor.cursor = 0;
                refused_with_an_insertion(editor, at, cx);
                assert!(editor.server_fix_holds, "offered as run");
                editor.editor.update(cx, |editor, cx| {
                    editor.edit([(MultiBufferOffset(at)..MultiBufferOffset(at), "()")], cx)
                });
            });
            cx.run_until_parked();
            editor.update_in(cx, |editor, window, cx| {
                assert!(!editor.server_fix_holds, "`()` was typed where it goes");
                editor.apply_server_fix(window, cx);
                assert_eq!(editor.editor.read(cx).text(cx), fixed, "not `people()()`");

                editor.set_text(text, window, cx);
                refused_with_an_insertion(editor, at, cx);
                assert!(editor.server_fix_holds);
                // The file's newer text replaces the tab's whole.
                editor.set_text(&format!("// from disk\n{text}"), window, cx);
            });
            cx.run_until_parked();
            editor.update_in(cx, |editor, window, cx| {
                assert!(!editor.server_fix_holds, "the text it was made for is gone");
                editor.apply_server_fix(window, cx);
                assert_eq!(
                    editor.editor.read(cx).text(cx),
                    format!("// from disk\n{text}")
                );
            });
        }

        /// Rows older than their branch's head stay so while the session is on another
        /// branch, where they offer no Refresh and read nothing, and offer it again back on
        /// their own; a move of their branch while the session is away is kept too.
        #[gpui::test]
        async fn rows_stay_newer_across_a_branch_switch(cx: &mut TestAppContext) {
            init(cx);
            let rows = json!({
                "query_name": "people", "target": {"branch": "main"}, "row_count": 1,
                "columns": ["p.name"], "rows": [{"p.name": "Ada"}],
                "graph_commit_id": "01JHEADAAAAA",
            });
            let (session, asked) = served(
                cx,
                vec![
                    ("/query", rows.to_string()),
                    (
                        "/branches",
                        json!({"branches": ["main", "dev"]}).to_string(),
                    ),
                ],
            );
            cx.run_until_parked();
            let reads = || {
                asked
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|line| line.ends_with("/query"))
                    .count()
            };
            let moved = |head: &str| SessionEvent::HeadMoved {
                branch: "main".into(),
                head: head.into(),
            };
            let (editor, cx) =
                cx.add_window_view(|window, cx| QueryEditor::new(1, session.clone(), window, cx));
            editor.update_in(cx, |editor, window, cx| {
                editor.set_text(
                    "query people() { match { $p: Person } return { $p.name } }",
                    window,
                    cx,
                );
                editor.cursor = 0;
                editor.run(&Run, window, cx);
            });
            cx.run_until_parked();
            let stale = |cx: &mut gpui::VisualTestContext| {
                editor.read_with(cx, |editor, cx| editor.is_stale(cx))
            };
            session.update(cx, |_, cx| cx.emit(moved("01JNEXTAAAAA")));
            cx.run_until_parked();
            assert!(stale(cx), "main moved past the rows");
            let switch = |branch: &str, cx: &mut gpui::VisualTestContext| {
                session.update(cx, |session, cx| session.set_branch(branch.into(), cx));
                cx.run_until_parked();
            };
            switch("dev", cx);
            assert!(!stale(cx), "no Refresh for main's rows on dev");
            let before = reads();
            editor.update(cx, |editor, cx| editor.refresh_rows(cx));
            cx.run_until_parked();
            assert_eq!(reads(), before, "dev's head is not where they came from");
            switch("main", cx);
            assert!(
                stale(cx),
                "back on main, the rows are still older than its head"
            );

            // Read again; then main moves while the session is on dev.
            editor.update(cx, |editor, cx| editor.refresh_rows(cx));
            cx.run_until_parked();
            assert!(!stale(cx));
            switch("dev", cx);
            session.update(cx, |_, cx| cx.emit(moved("01JLASTAAAAA")));
            cx.run_until_parked();
            assert!(!stale(cx));
            switch("main", cx);
            assert!(stale(cx), "the move while away is kept");
        }

        /// A saved file tab remembers its text for the restart that finds its file gone
        /// (deleted, or gone with a `git switch`): the tab stays, with the text and a bar that
        /// says the file is gone and offers Save As, and Save writes it back. A file that comes
        /// back holding the tab's text, or the text the tab's was typed over, is taken with no
        /// bar, whether the editor taking the keyboard or ⌘S finds it, and ⌘S writes; one
        /// that comes back holding other text, or none, keeps the tab's, and the bar says it
        /// changed: an empty file is not the tab's text either.
        /// One whose file reads shows the file, whatever it remembered; a text past the bound
        /// is not kept.
        #[gpui::test]
        async fn a_saved_tabs_text_outlives_its_file(cx: &mut TestAppContext) {
            init(cx);
            let session = session(cx);
            let path = scratch("gone").join("kept.gq");
            let (kept, draft, other) = ("query kept() {}", "query draft() {}", "query other() {}");
            let remembered = |text: &str| Draft {
                text: text.into(),
                based_on: Some(content_hash(text)),
                saved: true,
            };
            let opened = path.clone();
            let (editor, cx) = cx.add_window_view(move |window, cx| {
                QueryEditor::reading(1, session, opened, Some(remembered(kept)), window, cx)
            });
            // A restart on a branch without the file.
            let restart_gone =
                |editor: &mut QueryEditor, window: &mut Window, cx: &mut Context<QueryEditor>| {
                    editor.loading = Some(Loading {
                        draft: Some(remembered(kept)),
                    });
                    editor.disk_change = None;
                    assert!(editor.file_read(None, window, cx), "the tab stays");
                    assert!(editor.file_gone.is_some());
                };
            let file = || std::fs::read_to_string(&path).unwrap();
            editor.update_in(cx, |editor, window, cx| {
                restart_gone(editor, window, cx);
                assert_eq!(editor.editor.read(cx).text(cx), kept);
                assert!(editor.render_file_gone(cx).is_some());
                assert!(editor.is_dirty(cx), "on no disk: closing asks");
                let saved = editor.serialize(cx).unwrap();
                assert_eq!(saved.data["text"], json!(kept));
                assert_eq!(
                    saved.data["based_on"],
                    json!(content_hash(kept)),
                    "still what it was typed over, not the empty text compared while gone"
                );
                editor.save_now(window, cx);
            });
            cx.run_until_parked();
            assert_eq!(file(), kept);
            editor.update_in(cx, |editor, _, cx| {
                assert!(editor.file_gone.is_none() && !editor.is_dirty(cx));
            });

            // Back as it was, found as the editor takes the keyboard: no bar, and ⌘S writes.
            std::fs::remove_file(&path).unwrap();
            editor.update_in(cx, |editor, window, cx| restart_gone(editor, window, cx));
            std::fs::write(&path, kept).unwrap();
            editor.update_in(cx, |editor, window, cx| editor.check_disk(window, cx));
            cx.run_until_parked();
            editor.update_in(cx, |editor, window, cx| {
                assert!(editor.file_gone.is_none() && editor.disk_change.is_none());
                assert!(editor.render_disk_change(cx).is_none() && !editor.is_dirty(cx));
                editor.set_text(draft, window, cx);
                editor.save_now(window, cx);
            });
            cx.run_until_parked();
            assert_eq!(file(), draft, "⌘S wrote");

            // A draft over it, the file back holding what the draft was typed over, found by
            // ⌘S: no bar, and the save goes on over it.
            std::fs::remove_file(&path).unwrap();
            editor.update_in(cx, |editor, window, cx| {
                restart_gone(editor, window, cx);
                editor.set_text(draft, window, cx);
            });
            std::fs::write(&path, kept).unwrap();
            editor.update_in(cx, |editor, window, cx| editor.save_now(window, cx));
            cx.run_until_parked();
            assert_eq!(file(), draft, "written over the text it was typed over");
            editor.update_in(cx, |editor, _, cx| {
                assert!(editor.file_gone.is_none() && editor.disk_change.is_none());
                assert!(!editor.is_dirty(cx));
            });

            // Back holding other text, or empty, found either way: the tab's text stays, the
            // bar says the file changed, and ⌘S writes nothing until the person says.
            for (back, found_by_save) in [(other, false), (other, true), ("", false), ("", true)] {
                std::fs::remove_file(&path).unwrap();
                editor.update_in(cx, |editor, window, cx| restart_gone(editor, window, cx));
                std::fs::write(&path, back).unwrap();
                editor.update_in(cx, |editor, window, cx| match found_by_save {
                    false => editor.check_disk(window, cx),
                    true => editor.save_now(window, cx),
                });
                cx.run_until_parked();
                editor.update_in(cx, |editor, window, cx| {
                    assert_eq!(editor.editor.read(cx).text(cx), kept);
                    assert!(editor.file_gone.is_none() && editor.is_dirty(cx));
                    assert!(editor.render_disk_change(cx).is_some());
                    assert_eq!(
                        editor.serialize(cx).unwrap().data["based_on"],
                        json!(content_hash(kept))
                    );
                    if !found_by_save {
                        editor.save_now(window, cx);
                    }
                    assert!(
                        editor
                            .disk_change
                            .as_ref()
                            .is_some_and(|change| change.asked),
                        "⌘S asks"
                    );
                });
                cx.run_until_parked();
                assert_eq!(file(), back, "⌘S wrote nothing");
            }

            editor.update_in(cx, |editor, window, cx| {
                // The file reads: it is shown, not what was remembered of it.
                editor.loading = Some(Loading {
                    draft: Some(remembered("query old() {}")),
                });
                editor.disk_change = None;
                assert!(editor.file_read(Some("query new() {}".into()), window, cx));
                assert_eq!(editor.editor.read(cx).text(cx), "query new() {}");
                assert!(!editor.is_dirty(cx) && editor.disk_change.is_none());

                let long = format!("// {}", "x".repeat(REMEMBERED_SAVED_TEXT));
                editor.set_text(&long, window, cx);
                editor.saved_text = Some(long);
                editor.unsaved = false;
                assert!(editor.serialize(cx).unwrap().data.get("text").is_none());
            });
        }
    }
}

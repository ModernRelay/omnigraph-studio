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
    Anchor, Bias, Editor, EditorEvent, HighlightKey, MultiBufferOffset, SelectionEffects,
    ToOffset as _,
};
use gpui::{
    AnyView, App, ClickEvent, ClipboardItem, Context, DragMoveEvent, Entity, EventEmitter,
    FocusHandle, Focusable, HighlightStyle, MouseButton, MouseDownEvent, Pixels, Render,
    ScrollHandle, ScrollStrategy, SharedString, StyleRefinement, Subscription, Task,
    UnderlineStyle, UniformListScrollHandle, WeakEntity, Window, uniform_list,
};
use gq_language::{QueryInfo, ResultColumn, SchemaModel, TokenKind};
use graph_client::api::ErrorCode;
use graph_client::api::{
    ChangeRequest, ParamDescriptor, ParamKind, QueryCatalogEntry, QueryRequest, ReadOutput,
};
use graph_client::{ReadError, WriteOutcome};
use graph_store::{GraphSession, SessionEvent};
use serde_json::{Map, Value};
use studio_actions::query::{CopyCell, CopyRow, Explain, Run, Save, SaveAs};
use studio_ui::{
    ColumnDrag, ColumnResize, GridColumn, GridPlace, ROW_HEIGHT, columns_in_view, format_value,
    grid_cell, grid_header, grid_places, is_numeric, resizable_grid_header, short_commit_id,
    value_text,
};
use studio_workspace::{Item, ItemEvent, ItemFooter, KeyHint, SavedItem};
use ui::prelude::*;
use ui::{ContextMenu, KeyBinding, PopoverMenu, Tooltip};

use crate::analysis::Analysis;
use crate::completion::GqCompletionProvider;
use crate::results::{
    PlanItem, ResultGraph, ResultTable, ServerDiagnostic, SourceMap, format_elapsed, is_plan,
    plan_items, rows_label, server_diagnostic, type_label,
};

/// How long typing pauses before the source is checked again.
const CHECK_DELAY: Duration = Duration::from_millis(150);
/// The width of the row-number column in the result grid.
const ROW_NUMBER_WIDTH: f32 = 48.;
const PLAN_ROW_HEIGHT: f32 = 24.;

/// Highlight keys: Zed's editor has a closed set; `ConsoleAnsiHighlight(n)` is the one keyed by
/// a number. Studio's syntax classes and diagnostics take numbers no Zed console uses.
const SYNTAX_KEY: usize = 0x6751_0000;
const DIAGNOSTIC_KEY: usize = 0x6751_0100;
/// The parts of the query the hovered plan operator stands for.
const PLAN_KEY: usize = 0x6751_0200;
/// The token kinds that take a syntax colour (`TokenKind::syntax_name`), one highlight key each.
const SYNTAX: [TokenKind; 9] = [
    TokenKind::Keyword,
    TokenKind::Variable,
    TokenKind::Type,
    TokenKind::Property,
    TokenKind::Annotation,
    TokenKind::QueryName,
    TokenKind::String,
    TokenKind::Number,
    TokenKind::Comment,
];

/// A run that is ready to send.
#[derive(Clone)]
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
    /// The stored query to invoke by name instead of sending `text` (spec 007).
    stored: Option<String>,
}

/// The stored query a tab runs: the server serves it by name and never sends its text, so the
/// tab shows its description and parameters where a query tab shows its text.
struct Stored {
    entry: QueryCatalogEntry,
    /// The CLI alias whose values filled the parameters.
    alias: Option<String>,
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
}

enum Output {
    /// Nothing has run yet.
    Empty,
    Running {
        label: SharedString,
    },
    Rows(RowsOutput),
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
    target: String,
    commit: Option<String>,
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
    Committed {
        branch: String,
        nodes: i64,
        edges: i64,
        commit: Option<String>,
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

    /// A failed read; a refusal's diagnostic is placed in the editor through `map`.
    fn from_read(error: &ReadError, map: SourceMap) -> Self {
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
        };
        let diagnostic = match error {
            ReadError::Refused(summary) => summary
                .diagnostic
                .as_ref()
                .map(|d| server_diagnostic(d, &summary.message, map)),
            _ => None,
        };
        Self {
            title,
            hint: Some(hint.into()),
            diagnostic,
        }
    }
}

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
    /// The run line above each declaration (spec 007): the blocks, the declarations they were
    /// made for, and each declaration's last outcome by name.
    run_lines: Vec<CustomBlockId>,
    run_line_names: Vec<String>,
    /// Where each run line's declaration starts, kept by the text as it is edited.
    run_line_starts: Vec<Anchor>,
    /// Where each problem of the last check starts, kept by the text as it is edited: a jump
    /// made before the next check lands where the problem is now.
    problem_starts: Vec<Option<Anchor>>,
    last_runs: HashMap<String, RunNote>,
    /// The branch's latest commits, for the snapshot picker: id, Unix micros, actor.
    commits: Vec<(String, i64, Option<String>)>,
    commits_task: Option<Task<()>>,
    /// Reads run at this commit and writes are refused (spec 009); `None` uses the branch.
    at_commit: Option<String>,
    editor: Entity<Editor>,
    /// The buffer parses as GQ (`syntax`): colours, brackets, indents and folds follow the
    /// syntax tree. Without it, words are coloured lexically.
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
    example: Option<String>,
    results_scroll: UniformListScrollHandle,
    /// How far the results are scrolled sideways, for the columns a frame draws.
    results_hscroll: ScrollHandle,
    results_focus: FocusHandle,
    confirm_focus: FocusHandle,
    run_task: Option<Task<()>>,
    check_task: Option<Task<()>>,
    /// A stored query's tab (spec 007); `None` for a query written in the editor.
    stored: Option<Stored>,
    /// The result area as a view of its own, drawn again whenever the tab notifies.
    results: Entity<QueryResults>,
    _subscriptions: Vec<Subscription>,
}

/// The tab's result area, drawn as a view of its own, so a keystroke in the query draws the
/// toolbar and the text again but leaves the rows as they were (`docs/drawing.md`).
struct QueryResults {
    query: WeakEntity<QueryEditor>,
    _query_changed: Subscription,
    /// How often the results were drawn, for the test that typing leaves them alone.
    #[cfg(test)]
    renders: usize,
}

impl QueryResults {
    /// Made with the tab, not on its first draw (`docs/drawing.md`).
    fn new(cx: &mut Context<QueryEditor>) -> Entity<Self> {
        let query = cx.entity();
        cx.new(|cx| QueryResults {
            query: query.downgrade(),
            _query_changed: cx.observe(&query, |_, _, cx| cx.notify()),
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

    /// The draft comes back after a restart (spec 007: the text is never cleared). It is
    /// written to the workspace file, never to a log.
    /// The window's footer, as the table's: the result's facts on the leading side, what the
    /// keyboard does with it on the trailing side.
    fn footer(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Option<ItemFooter> {
        Some(self.footer_parts(cx))
    }

    fn serialize(&self, cx: &App) -> Option<SavedItem> {
        if let Some(stored) = &self.stored {
            // The values typed come back with the tab, as a query's text does.
            let values: Map<String, Value> = self
                .params
                .iter()
                .map(|(name, input)| (name.clone(), Value::String(input.read(cx).text(cx))))
                .collect();
            return Some(SavedItem {
                kind: "stored_query".into(),
                data: serde_json::json!({
                    "name": stored.entry.name,
                    "alias": stored.alias,
                    "at_commit": self.at_commit,
                    "values": values,
                    "widths": self.column_widths,
                }),
            });
        }
        Some(SavedItem {
            kind: "query".into(),
            data: serde_json::json!({
                "text": self.editor.read(cx).text(cx),
                "at_commit": self.at_commit,
                "path": self.path.as_ref().map(|path| path.to_string_lossy().into_owned()),
                "widths": self.column_widths,
            }),
        })
    }
}

impl QueryEditor {
    pub fn new(
        number: usize,
        session: Entity<GraphSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(number, session, None, window, cx)
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
            alias: None,
            gone: false,
            pending: Map::new(),
        };
        let mut this = Self::build(number, session, Some(stored), window, cx);
        this.sync_stored_params(window, cx);
        // Opened before the catalog was read, or after the query left it.
        this.catalog_changed(window, cx);
        this
    }

    fn build(
        number: usize,
        session: Entity<GraphSession>,
        stored: Option<Stored>,
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
                Some(session.downgrade()),
            ))));
            editor.set_semantics_provider(Some(std::rc::Rc::new(crate::hover::GqSemantics::new(
                Some(session.downgrade()),
            ))));
            editor
        });
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
                    SessionEvent::BranchChanged | SessionEvent::BranchesChanged => {
                        // A write waiting to be confirmed was for the branch shown when it
                        // was asked for.
                        if matches!(event, SessionEvent::BranchChanged) {
                            this.pending_write = None;
                        }
                        this.load_commits(cx);
                        cx.notify()
                    }
                    SessionEvent::Committed { branch }
                        if *branch == this.session.read(cx).branch =>
                    {
                        this.load_commits(cx)
                    }
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
            commits: Vec::new(),
            commits_task: None,
            run_lines: Vec::new(),
            run_line_names: Vec::new(),
            run_line_starts: Vec::new(),
            problem_starts: Vec::new(),
            last_runs: HashMap::new(),
            at_commit: None,
            editor,
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
            example: None,
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
        this.set_example(window, cx);
        this.load_commits(cx);
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

    /// The branch's latest commits for the snapshot picker, from the session's commit list
    /// (shared with every query tab, the history and the branches panel, and read again after a
    /// commit lands on the branch).
    fn load_commits(&mut self, cx: &mut Context<Self>) {
        let read = self.session.update(cx, |session, cx| {
            let branch = session.branch.clone();
            session.commits(&branch, false, cx)
        });
        self.commits_task = Some(cx.spawn(async move |this, cx| {
            let Ok(list) = read.await else {
                return;
            };
            let mut commits: Vec<(String, i64, Option<String>)> = list
                .iter()
                .map(|c| (c.graph_commit_id.clone(), c.created_at, c.actor_id.clone()))
                .collect();
            commits.sort_by_key(|commit| std::cmp::Reverse(commit.1));
            commits.truncate(PICKER_COMMITS);
            this.update(cx, |this, cx| {
                this.commits = commits;
                cx.notify();
            })
            .ok();
        }));
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
        let commits = self.commits.clone();
        let editor = cx.entity().downgrade();
        let label = match &pinned {
            Some(commit) => format!("Read only · {}", short_commit_id(commit)),
            None => format!("Head of {branch}"),
        };
        PopoverMenu::new("snapshot-picker")
            .trigger(
                studio_ui::ds::button("snapshot-picker-button", label)
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .start_icon(
                        Icon::new(if pinned.is_some() {
                            IconName::GitCommit
                        } else {
                            IconName::GitBranch
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .end_icon(
                        Icon::new(IconName::ChevronDown)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .tooltip(Tooltip::text(match &pinned {
                        Some(commit) => {
                            format!("Reads run at commit {commit}; writes are refused")
                        }
                        None => format!("Reads run at the head of {branch}"),
                    })),
            )
            .menu(move |window, cx| {
                let commits = commits.clone();
                let pinned = pinned.clone();
                let editor = editor.clone();
                let branch = branch.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
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
                    if !commits.is_empty() {
                        menu = menu.separator().header("Read at a commit");
                    }
                    let now = studio_ui::now_micros();
                    for (id, at, actor) in commits {
                        let editor = editor.clone();
                        let label = format!(
                            "{} · {}{}",
                            short_commit_id(&id),
                            studio_ui::relative_time(at, now),
                            actor.map(|a| format!(" · {a}")).unwrap_or_default()
                        );
                        let checked = pinned.as_deref() == Some(id.as_str());
                        menu = menu.toggleable_entry(
                            label,
                            checked,
                            IconPosition::Start,
                            None,
                            move |_, cx| {
                                let id = id.clone();
                                editor.update(cx, |editor, cx| editor.pin_to(id, cx)).ok();
                            },
                        );
                    }
                    menu
                }))
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
    /// read, so any text counts as changed).
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

    fn sync_unsaved(&mut self, cx: &App) {
        self.unsaved = self
            .saved_text
            .as_ref()
            .is_some_and(|saved| *saved != self.editor.read(cx).text(cx));
    }

    /// ⌘S: writes the file this tab edits; a new query is saved in this graph's queries folder,
    /// named after its first declaration.
    fn save(
        &mut self,
        _: &studio_actions::query::Save,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A stored query lives on the server; there is no text here to save.
        if self.stored.is_some() {
            return;
        }
        let path = match &self.path {
            Some(path) => path.clone(),
            None => {
                let graph = self.session.read(cx).settings_key(cx);
                let name = gq_language::outline(&self.editor.read(cx).text(cx))
                    .declarations
                    .first()
                    .map(|d| d.name.clone())
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| format!("query-{}", self.number));
                unused_path(&studio_settings::queries_dir(&graph), &name)
            }
        };
        self.write_to(path, window, cx);
    }

    /// ⌘⇧S: writes the query to a file chosen in the system's save dialog.
    fn save_as(
        &mut self,
        _: &studio_actions::query::SaveAs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.stored.is_some() {
            return;
        }
        let graph = self.session.read(cx).settings_key(cx);
        let directory = self
            .path
            .as_ref()
            .and_then(|path| path.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| studio_settings::queries_dir(&graph));
        std::fs::create_dir_all(&directory).ok();
        let suggested = self
            .path
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "query.gq".to_owned());
        let chosen = cx.prompt_for_new_path(&directory, Some(&suggested));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = chosen.await else {
                return;
            };
            this.update_in(cx, |this, window, cx| this.write_to(path, window, cx))
                .ok();
        })
        .detach();
    }

    fn write_to(&mut self, path: std::path::PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text(cx);
        let written = text.clone();
        let target = path.clone();
        let write = cx.background_spawn(async move {
            studio_settings::write_atomic(
                &target,
                written.as_bytes(),
                studio_settings::Access::Keep,
            )
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = write.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.path = Some(path);
                        this.saved_text = Some(text);
                        // Typing while it was written stays unsaved.
                        this.sync_unsaved(cx);
                        this.file_error = None;
                        crate::saved_queries_changed(cx);
                    }
                    Err(error) => {
                        this.file_error = Some(format!("Could not save: {error}"));
                    }
                }
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Replaces the draft, as when a remembered tab comes back.
    pub fn set_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
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
        if self.stored.is_some() || self.example.is_some() || !self.editor.read(cx).is_empty(cx) {
            return;
        }
        let Some(text) = self.schema(cx).and_then(|s| gq_language::example_read(&s)) else {
            return;
        };
        self.example = Some(text.clone());
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
            // The tab's changed-since-saved dot follows the text.
            self.sync_unsaved(cx);
            cx.emit(ItemEvent::UpdateTab);
        }
        self.highlight_syntax(cx);
        self.schedule_check(window, cx);
        self.cursor_moved(window, cx);
    }

    fn cursor_moved(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let before = self.current_query().map(|(name, _)| name.to_owned());
        let editor = self.editor.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        self.cursor = editor
            .selections
            .newest_anchor()
            .head()
            .to_offset(&snapshot)
            .0;
        self.sync_params(window, cx);
        // The toolbar and the window's footer name the declaration under the cursor, and before
        // a run the result area shows its columns. Only a new declaration notifies: the tab
        // draws again anyway because its editor changed, and a notify on every key would draw
        // the rows again too.
        if self.current_query().map(|(name, _)| name) != before.as_deref() {
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
        }
    }

    fn highlight_syntax(&mut self, cx: &mut Context<Self>) {
        if self.parsed {
            return;
        }
        let theme = cx.theme().clone();
        self.editor.update(cx, |editor, cx| {
            let text = editor.text(cx);
            let tokens = gq_language::tokenize(&text);
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            for (index, kind) in SYNTAX.iter().enumerate() {
                let key = HighlightKey::ConsoleAnsiHighlight(SYNTAX_KEY + index);
                let Some(style) = kind
                    .syntax_name()
                    .and_then(|name| theme.syntax().style_for_name(name))
                else {
                    continue;
                };
                let ranges: Vec<_> = tokens
                    .iter()
                    .filter(|token| token.kind == *kind)
                    .map(|token| {
                        snapshot.anchor_after(MultiBufferOffset(token.range.start))
                            ..snapshot.anchor_before(MultiBufferOffset(token.range.end))
                    })
                    .collect();
                if ranges.is_empty() {
                    editor.clear_highlights(key, cx);
                } else {
                    editor.highlight_text(key, ranges, style, cx);
                }
            }
        });
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
        let error = cx.theme().status().error;
        let key = HighlightKey::ConsoleAnsiHighlight(DIAGNOSTIC_KEY);
        self.problem_starts = self.editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let starts = analysis
                .problems
                .iter()
                .map(|(_, range)| {
                    let range = range.as_ref()?;
                    Some(snapshot.anchor_after(MultiBufferOffset(range.start)))
                })
                .collect();
            let ranges: Vec<_> = analysis
                .problems
                .iter()
                .filter_map(|(_, range)| range.clone())
                .map(|range| {
                    snapshot.anchor_after(MultiBufferOffset(range.start))
                        ..snapshot.anchor_before(MultiBufferOffset(range.end))
                })
                .collect();
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
            starts
        });
        self.analysis = Some(analysis);
        self.sync_params(window, cx);
        self.sync_run_lines(cx);
        // What the declaration returns, in the window's footer, follows the check.
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// Puts a run line above each declaration, when the declarations changed since the lines
    /// were placed; anchored to the text, the lines move with edits in between.
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
        if names == self.run_line_names {
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
                .map(|value| value_text(&value));
            let input = param_input(placeholder, text, window, cx);
            self.params.insert(param.name, input);
        }
    }

    /// Fills parameters from `values`: an alias's from the CLI config, or the text a remembered
    /// tab held. A string goes in as it is, anything else as JSON, which the inputs read back.
    pub fn fill_params(
        &mut self,
        alias: Option<String>,
        values: &Map<String, Value>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for (name, value) in values {
            match self.params.get(name) {
                Some(input) => {
                    let text = value_text(value);
                    input.update(cx, |input, cx| input.set_text(text, window, cx));
                }
                None => {
                    if let Some(stored) = &mut self.stored {
                        stored.pending.insert(name.clone(), value.clone());
                    }
                }
            }
        }
        if let Some(stored) = &mut self.stored {
            stored.alias = alias;
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
            stored: Some(name.clone()),
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
    /// description, its instruction, and where its values came from. Nothing when it says
    /// nothing.
    fn render_stored_about(&self, stored: &Stored, cx: &mut Context<Self>) -> Option<AnyElement> {
        use studio_ui::ds::{self, space};
        let entry = &stored.entry;
        let mut facts = Vec::new();
        if let Some(alias) = &stored.alias {
            facts.push(
                ds::meta(IconName::Link, format!("values from alias {alias}")).into_any_element(),
            );
        }
        if stored.gone {
            facts.push(
                h_flex()
                    .gap(px(space::XS))
                    .child(
                        Icon::new(IconName::Warning)
                            .size(IconSize::XSmall)
                            .color(Color::Warning),
                    )
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
            return Ok(Prepared {
                text: source.clone(),
                name: None,
                label,
                write: statement.writes(),
                // A load, and the plan of one, go to the request's branch, as a mutation does;
                // the others name their own targets.
                statement: !statement.targets_branch(),
                params: None,
                columns: None,
                map: SourceMap {
                    prefix_len: source.len(),
                    ..Default::default()
                },
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
        let info = self.schema(cx).and_then(|schema| {
            gq_language::check(&schema, &text)
                .queries
                .into_iter()
                .find(|q| q.name == declaration.name)
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
        Ok(Prepared {
            text,
            name: (!declaration.name.is_empty()).then(|| declaration.name.clone()),
            label,
            write: mutation,
            statement: false,
            params,
            columns: info.map(|info| info.columns),
            map: SourceMap {
                prefix_len: outline.prefix.end,
                inserted: if explained { "explain ".len() } else { 0 },
                start: declaration.range.start,
            },
            stored: None,
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
        match self.prepare(false, cx) {
            Err(message) => self.output = Output::Failed(Failure::new(message)),
            Ok(prepared) if prepared.write && self.at_commit.is_some() => {
                let commit = self
                    .at_commit
                    .as_deref()
                    .map(short_commit_id)
                    .unwrap_or_default();
                self.output = Output::Failed(Failure::new(format!(
                    "Read-only: this query reads commit {commit}. Open a new query to write to a branch."
                )));
            }
            Ok(prepared) if prepared.write => {
                // Never on ⌘↵ alone: the person confirms the branch first.
                let branch = self.branch(cx);
                self.pending_write = Some(PendingWrite { prepared, branch });
                window.focus(&self.confirm_focus, cx);
            }
            Ok(prepared) => {
                self.pending_write = None;
                self.send_read(prepared, cx);
            }
        }
        // The window's footer shows the run.
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn explain(&mut self, _: &Explain, _window: &mut Window, cx: &mut Context<Self>) {
        // A write's outcome stays on screen until it is in, as Run waits for it.
        if self.writing {
            return;
        }
        match self.prepare(true, cx) {
            Err(message) => self.output = Output::Failed(Failure::new(message)),
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
        let branch = match &self.at_commit {
            Some(commit) => format!("commit {}", short_commit_id(commit)),
            None => branch,
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
        self.output = Output::Running {
            label: format!("Running {} on {branch}…", prepared.label).into(),
        };
        self.selected = None;
        // Replacing the task cancels an earlier read still in flight.
        self.run_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let elapsed = started.elapsed();
            this.update(cx, |this, cx| {
                // A new output replaces the plan whose operator lit the text.
                this.light_plan_parts(None, cx);
                let schema = this.session.read(cx).schema_model();
                this.finish_read(prepared, branch, result, elapsed, schema);
                cx.emit(ItemEvent::UpdateTab);
                this.run_task = None;
                cx.notify();
            })
            .ok();
        }));
    }

    fn finish_read(
        &mut self,
        prepared: Prepared,
        branch: String,
        result: Result<ReadOutput, ReadError>,
        elapsed: Duration,
        schema: Option<Arc<SchemaModel>>,
    ) {
        let note = match &result {
            Ok(output) => RunNote {
                text: format!(
                    "{} · {}",
                    rows_label(output.row_count),
                    format_elapsed(elapsed)
                ),
                failed: false,
            },
            Err(_) => RunNote {
                text: "Refused".into(),
                failed: true,
            },
        };
        self.note_run(prepared.name.as_deref(), note);
        self.output = match result {
            Ok(output) if is_plan(&output.columns) => Output::Plan(PlanOutput {
                name: prepared.name.clone(),
                label: prepared.label,
                items: plan_items(&output.rows),
                elapsed,
                commit: output.graph_commit_id,
            }),
            Ok(output) => {
                let table =
                    ResultTable::new(output.columns, &output.rows, prepared.columns.as_deref());
                let widths = (0..table.columns.len())
                    .map(|i| {
                        self.column_widths
                            .get(&table.columns[i])
                            .copied()
                            .unwrap_or_else(|| table.column_width(i))
                    })
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
                Output::Rows(RowsOutput {
                    label: prepared.label,
                    graph,
                    table,
                    widths,
                    right_aligned,
                    shown,
                    row_count: output.row_count,
                    elapsed,
                    target: output
                        .target
                        .branch
                        .or(output
                            .target
                            .snapshot
                            .map(|commit| format!("commit {}", short_commit_id(&commit))))
                        .unwrap_or(branch),
                    commit: output.graph_commit_id,
                })
            }
            Err(error) => Output::Failed(Failure::from_read(&error, prepared.map)),
        };
        self.results_scroll.scroll_to_item(0, ScrollStrategy::Top);
    }

    fn confirm_write(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let Some(PendingWrite { prepared, branch }) = self.pending_write.take() else {
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
            self.output = Output::Failed(Failure::new(
                "The query, its parameters or its branch changed after Run; nothing was written. \
                 Run it again to confirm the write as it is now."
                    .to_owned(),
            ));
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
        self.output = Output::Running {
            label: format!("Writing {} to {branch}…", prepared.label).into(),
        };
        let map = prepared.map;
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
                        text: format!(
                            "Committed · {} nodes, {} edges",
                            output.affected_nodes, output.affected_edges
                        ),
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
                this.output = Output::Written(match outcome {
                    WriteOutcome::Committed(output) => WriteResult::Committed {
                        branch: output.branch,
                        nodes: output.affected_nodes,
                        edges: output.affected_edges,
                        commit: output.commit.map(|c| c.graph_commit_id),
                        elapsed,
                    },
                    WriteOutcome::Refused(error) => WriteResult::Refused {
                        status: error.status,
                        diagnostic: error
                            .body
                            .diagnostic
                            .as_ref()
                            .map(|d| server_diagnostic(d, &error.body.error, map)),
                        message: error.body.error,
                    },
                    WriteOutcome::Unknown { reason } => WriteResult::Unknown { reason },
                });
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
            // The footer offers copying once a cell is selected.
            cx.emit(ItemEvent::UpdateTab);
        }
        cx.notify();
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Output::Rows(rows) = &self.output else {
            return;
        };
        let count = rows.table.rows.len();
        if count == 0 {
            return;
        }
        let (row, column) = self.selected.unwrap_or((0, 0));
        let row = (row as isize + delta).clamp(0, count as isize - 1) as usize;
        self.selected = Some((row, column));
        self.results_scroll
            .scroll_to_item(row, ScrollStrategy::Nearest);
        cx.notify();
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

    fn footer_parts(&self, cx: &mut Context<Self>) -> ItemFooter {
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
                // Where the rows were read, when it is not the branch the footer shows: the
                // branch changed since, and a commit pin is named by the commit itself.
                if !rows.target.starts_with("commit ") && rows.target != self.branch(cx) {
                    start.push(
                        ds::meta(IconName::GitBranch, rows.target.clone()).into_any_element(),
                    );
                }

                if let Some(graph) = rows.graph.clone() {
                    let title = format!("{} · result", rows.label);
                    end.push(
                        ds::button("result-canvas", "Canvas")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::XSmall)
                            .start_icon(Icon::new(IconName::GitGraph).size(IconSize::XSmall))
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
        ItemFooter { start, end, keys }
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
                    .gap(px(studio_ui::ds::space::INLINE))
                    .items_center()
                    .tooltip(Tooltip::text(tip))
                    .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                    .child(studio_ui::ds::text::title(title).single_line().truncate())
                    .children(kind.map(studio_ui::ds::text::muted))
            }))
            .children(self.file_error.clone().map(|error| {
                Label::new(error)
                    .size(LabelSize::Small)
                    .color(Color::Error)
                    .single_line()
                    .truncate()
            }))
            .child(div().flex_1())
            .child(self.render_snapshot_picker(cx))
            // A stored query's plan needs its text, which the server does not send.
            .when(self.stored.is_none(), |toolbar| {
                toolbar.child(
                    studio_ui::ds::button("explain", "Explain")
                        .start_icon(Icon::new(IconName::ListTree).size(IconSize::XSmall))
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
                studio_ui::ds::button("run", "Run")
                    .style(ButtonStyle::Filled)
                    .start_icon(Icon::new(IconName::PlayFilled).size(IconSize::XSmall))
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

    fn render_problems(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.stored.is_some() {
            return None;
        }
        let analysis = self.analysis.clone()?;
        if analysis.problems.is_empty() {
            return None;
        }
        let colors = cx.theme().colors();
        let (border, hover) = (colors.border_variant, colors.ghost_element_hover);
        Some(
            v_flex()
                .id("query-problems")
                .flex_none()
                .max_h(px(112.))
                .overflow_y_scroll()
                .py_1()
                .border_t_1()
                .border_color(border)
                .children(
                    analysis
                        .problems
                        .iter()
                        .enumerate()
                        .map(|(index, (problem, _))| {
                            let at = self.problem_starts.get(index).copied().flatten();
                            let position = match (problem.line, problem.column) {
                                (Some(line), Some(column)) => format!("{line}:{column}"),
                                _ => String::new(),
                            };
                            h_flex()
                                .id(("problem", index))
                                .px_2()
                                .py_0p5()
                                .gap_2()
                                .cursor_pointer()
                                .hover(move |s| s.bg(hover))
                                .child(
                                    Icon::new(IconName::XCircle)
                                        .size(IconSize::XSmall)
                                        .color(Color::Error),
                                )
                                .child(
                                    Label::new(problem.code.clone())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .child(Label::new(problem.message.clone()).size(LabelSize::Small))
                                .children(problem.fix.clone().map(|fix| {
                                    Label::new(format!("Fix: {fix}"))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                }))
                                .when(!problem.edits.is_empty(), |row| {
                                    let edits = problem.edits.clone();
                                    let checked = analysis.text.clone();
                                    row.child(
                                        Button::new(("apply-fix", index), "Fix")
                                            .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                                            .label_size(LabelSize::XSmall)
                                            .tooltip(Tooltip::text(
                                                "Apply the compiler's suggested edit",
                                            ))
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.apply_fix(&checked, &edits, window, cx)
                                            })),
                                    )
                                })
                                .child(div().flex_1())
                                .child(
                                    Label::new(position)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    if let Some(at) = &at {
                                        let offset = this.offset_of(at, cx);
                                        this.jump_to(offset, window, cx);
                                    }
                                }))
                        }),
                )
                .into_any_element(),
        )
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
        let status = cx.theme().status();
        let (background, border) = (status.warning_background, status.warning_border);
        let what = if pending.prepared.statement {
            format!("`{}` changes branches.", pending.prepared.label)
        } else {
            format!(
                "{} changes data on {}.",
                pending.prepared.label, pending.branch
            )
        };
        let button = if pending.prepared.statement {
            "Run statement".to_owned()
        } else {
            format!("Run on {}", pending.branch)
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
                .child(
                    Icon::new(IconName::Warning)
                        .size(IconSize::Small)
                        .color(Color::Warning),
                )
                .child(ds::text::body(what))
                .child(div().flex_1())
                .child(
                    h_flex()
                        .gap(px(space::XS))
                        .child(
                            ds::button("cancel-write", "Cancel")
                                .key_binding(KeyBinding::for_action_in(
                                    &menu::Cancel,
                                    &self.confirm_focus,
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
                                    &self.confirm_focus,
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

    /// Before the first run: the columns the query under the cursor will return.
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
                    .map(|c| {
                        GridColumn::new(c.name.clone(), c.name.clone())
                            .detail(type_label(c))
                            .width(px(((c.name.len().max(type_label(c).len())) as f32 * 7.5
                                + 32.)
                                .clamp(96., 240.)))
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
            .on_action(cx.listener(|this, _: &menu::SelectNext, _, cx| this.move_selection(1, cx)))
            .on_action(
                cx.listener(|this, _: &menu::SelectPrevious, _, cx| this.move_selection(-1, cx)),
            )
            // A column's edge dragged anywhere over the results resizes it until released.
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<ColumnDrag>, _, cx| {
                    this.drag_column(event, cx)
                }),
            )
            .on_drop(cx.listener(|this, _: &ColumnDrag, _, cx| {
                this.resizing = None;
                cx.notify();
            }))
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
            cx.notify();
        });
        let on_reset = cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.resizing = None;
            this.resize_column(index, None, cx);
        });
        studio_ui::column_resize_handle(index, active, on_start, on_reset, cx).into_any_element()
    }

    fn drag_column(&mut self, event: &DragMoveEvent<ColumnDrag>, cx: &mut Context<Self>) {
        let Some(resizing) = &self.resizing else {
            return;
        };
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
        let border = studio_ui::ds::surface::rule(cx);
        let colors = cx.theme().colors();
        let (selected_bg, row_selected_bg) = (colors.element_selected, colors.ghost_element_hover);
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
                        .border_color(border)
                        .when(row_selected, |d| d.bg(row_selected_bg))
                        .child(
                            h_flex().w(px(ROW_NUMBER_WIDTH)).flex_none().px_2().child(
                                studio_ui::cell_text((ix + 1).to_string(), cx)
                                    .size(TextSize::XSmall, cx)
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
                .py(px(studio_ui::ds::space::XS)),
            )
            .into_any_element()
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
            editor.highlight_background(
                key,
                &anchors,
                |_, theme| theme.colors().text_accent.opacity(0.16),
                cx,
            );
        });
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
                        .px_2()
                        .child(
                            Label::new(title.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
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
                        .pr_2()
                        .gap_2()
                        .hover(|row| row.bg(hover_bg))
                        .when(hovered == Some(ix), |row| row.bg(lit_bg))
                        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                            if *hovered {
                                this.light_plan_parts(Some(ix), cx);
                            } else if this.plan_hovered == Some(ix) {
                                this.light_plan_parts(None, cx);
                            }
                        }))
                        .child(
                            Icon::new(if *depth == 0 {
                                IconName::ChevronDown
                            } else {
                                IconName::ArrowRight
                            })
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                        )
                        .child(Label::new(node.clone()).size(LabelSize::Small))
                        .child(render_facts(facts))
                        .when(!facts.is_empty(), |d| {
                            d.tooltip(Tooltip::text(facts_tooltip(facts)))
                        })
                        .into_any_element(),
                    PlanItem::Note { name, value } => h_flex()
                        .id(("plan-note", ix))
                        .h(px(PLAN_ROW_HEIGHT))
                        .pl(px(12.))
                        .pr_2()
                        .gap_2()
                        .child(
                            Label::new(name.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(value.clone())
                                .size(LabelSize::XSmall)
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
            WriteResult::Committed {
                branch,
                nodes,
                edges,
                commit,
                elapsed,
            } => {
                let title = match commit {
                    Some(_) => format!("Committed to {branch}"),
                    None => format!("Nothing changed on {branch}"),
                };
                let count =
                    |n: i64, one: &str| format!("{n} {one}{}", if n == 1 { "" } else { "s" });
                let mut facts = vec![
                    ds::meta(
                        IconName::Hash,
                        format!("{}, {}", count(*nodes, "node"), count(*edges, "edge")),
                    )
                    .into_any_element(),
                    ds::meta(IconName::Clock, format_elapsed(*elapsed)).into_any_element(),
                ];
                if let Some(commit) = commit {
                    facts.push(
                        div()
                            .id("write-commit")
                            .child(ds::meta_code(
                                IconName::GitCommit,
                                short_commit_id(commit),
                                cx,
                            ))
                            .tooltip(Tooltip::text(format!("Graph commit {commit}")))
                            .into_any_element(),
                    );
                }
                self.render_message(
                    IconName::Check,
                    Color::Success,
                    title,
                    None,
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
                            .child(Icon::new(icon).size(IconSize::Small).color(color)),
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
                                let at = diagnostic.byte.filter(|_| self.stored.is_none());
                                ds::button(
                                    "server-diagnostic",
                                    format!(
                                        "{}{}",
                                        diagnostic
                                            .code
                                            .map(|c| format!("{c} · "))
                                            .unwrap_or_default(),
                                        diagnostic.message
                                    ),
                                )
                                .start_icon(Icon::new(IconName::ArrowRight).size(IconSize::XSmall))
                                .disabled(at.is_none())
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        if let Some(at) = at {
                                            this.jump_to(at, window, cx);
                                        }
                                    },
                                ))
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
        let problems = self.render_problems(cx);
        let params = self.render_params(cx);
        let confirm = self.render_confirm(cx);
        let results = AnyView::from(self.results.clone())
            .cached(StyleRefinement::default().flex_1().min_h_0());
        let source = match &self.stored {
            Some(stored) => self.render_stored_about(stored, cx),
            None => Some(
                div()
                    .h(relative(0.42))
                    .min_h(px(120.))
                    .flex_none()
                    .pt(px(studio_ui::ds::space::XS))
                    .border_b_1()
                    .border_color(studio_ui::ds::surface::rule(cx))
                    .child(self.editor.clone())
                    .into_any_element(),
            ),
        };
        v_flex()
            .key_context("QueryEditor")
            .on_action(cx.listener(Self::run))
            .on_action(cx.listener(Self::explain))
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::save_as))
            .size_full()
            .child(toolbar)
            .children(source)
            .children(problems)
            .children(params)
            .children(confirm)
            .child(results)
    }
}

/// The dedupe key of a tab editing `path`, so a file opens once.
pub fn file_key(path: &std::path::Path) -> String {
    format!("query-file:{}", path.display())
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
        .gap_2()
        .items_center()
        .child(
            Button::new(("run-line-run", index), "Run")
                .style(ButtonStyle::Transparent)
                .label_size(LabelSize::XSmall)
                .color(Color::Muted)
                .start_icon(
                    Icon::new(IconName::PlayFilled)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .on_click(move |_, window, cx| {
                    editor
                        .update(cx, |editor, cx| editor.run_declaration(index, window, cx))
                        .ok();
                }),
        )
        .children(note.map(|note| {
            Label::new(note.text)
                .size(LabelSize::XSmall)
                .color(if note.failed {
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
        .gap_3()
        .overflow_hidden()
        .children(facts.iter().map(|(name, value)| {
            let value = if value.chars().count() > SHOWN {
                format!("{}…", value.chars().take(SHOWN - 1).collect::<String>())
            } else {
                value.clone()
            };
            h_flex()
                .flex_none()
                .gap_1()
                .when(!name.is_empty(), |fact| {
                    fact.child(
                        Label::new(name.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                })
                .child(Label::new(value).size(LabelSize::XSmall).single_line())
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

    /// What an input shows for a value from an alias or a remembered tab, and reads back.
    #[test]
    fn values_show_as_text_the_inputs_read_back() {
        assert_eq!(value_text(&json!("Ada")), "Ada");
        assert_eq!(value_text(&json!(5)), "5");
        assert_eq!(value_text(&json!(["a", "b"])), r#"["a","b"]"#);
        assert_eq!(value_text(&Value::Null), "");
        let list = ParamDescriptor {
            item_kind: Some(ParamKind::String),
            ..param(ParamKind::List)
        };
        assert_eq!(
            sent(&list, &value_text(&json!(["a", "b"]))),
            Ok(Some(json!(["a", "b"])))
        );
    }

    /// Headless tests of the tab in a window.
    mod views {
        use gpui::TestAppContext;
        use graph_store::{Connection, ServerEntry, ServerSource};
        use http_client::{AsyncBody, FakeHttpClient, Response};

        use super::*;

        fn session(cx: &mut TestAppContext) -> Entity<GraphSession> {
            let http = FakeHttpClient::create(|_| async move {
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, "0.13")
                    .body(AsyncBody::from(r#"{"branches":["main"]}"#))
                    .unwrap())
            });
            let server = ServerEntry {
                name: "dev".into(),
                url: "http://127.0.0.1:8787".into(),
                source: ServerSource::Environment,
                known_graphs: Vec::new(),
            };
            let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx))
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
            let prepared = Prepared {
                text: String::new(),
                name: Some("people".into()),
                label: "people".into(),
                write: false,
                statement: false,
                params: None,
                columns: None,
                map: SourceMap::default(),
                stored: None,
            };
            editor.finish_read(prepared, "main".into(), Ok(output), Duration::ZERO, None);
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
                    statement: false,
                    params: None,
                    columns: None,
                    map: SourceMap::default(),
                    stored: None,
                },
                branch: "main".into(),
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
                let at = editor.problem_starts[0].expect("anchored");
                assert_eq!(editor.offset_of(&at, cx), problem + note.len());
                assert_eq!(editor.offset_of(&editor.run_line_starts[0], cx), note.len());
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

        /// Typing in the query draws the text and the toolbar again but not the rows
        /// (`docs/drawing.md`); a notify from the tab draws them.
        #[gpui::test]
        async fn typing_in_the_query_leaves_the_results_as_they_were(cx: &mut TestAppContext) {
            cx.update(|cx| {
                studio_theme::init_test(cx);
                editor::init(cx);
            });
            let session = session(cx);
            let (editor, cx) =
                cx.add_window_view(|window, cx| QueryEditor::new(1, session, window, cx));
            let renders = |cx: &mut gpui::VisualTestContext| {
                editor.read_with(cx, |editor, cx| editor.results.read(cx).renders)
            };
            let text = "query people() { match { $p: Person } return { $p.name } }";
            editor.update_in(cx, |editor, window, cx| {
                editor.set_text(text, window, cx);
                finish(editor, &["name"]);
                cx.notify();
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
            // window once; the keys after it are the ones counted.
            cx.simulate_input("s");
            cx.run_until_parked();
            let before = renders(cx);
            assert!(before > 0);
            cx.simulate_input("ake");
            cx.run_until_parked();
            editor.read_with(cx, |editor, cx| {
                assert!(editor.editor.read(cx).text(cx).contains("$p.namesake"))
            });
            assert_eq!(renders(cx), before, "typing drew the results");

            editor.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
            assert_eq!(renders(cx), before + 1);
        }
    }
}

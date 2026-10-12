//! The query editor tab (spec 007): Zed's editor over `.gq` text with highlighting, checks as
//! you type and completion, then a run loop with parameters, a confirm step for writes,
//! results, plans and outcomes under it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use editor::scroll::Autoscroll;
use editor::{
    Editor, EditorEvent, HighlightKey, MultiBufferOffset, SelectionEffects, ToOffset as _,
};
use gpui::{
    App, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, HighlightStyle,
    MouseButton, Render, ScrollStrategy, SharedString, Subscription, Task, UnderlineStyle,
    UniformListScrollHandle, Window, uniform_list,
};
use gq_language::{QueryInfo, ResultColumn, SchemaModel, Statement, TokenKind};
use graph_client::api::ErrorCode;
use graph_client::api::{ChangeRequest, QueryRequest, ReadOutput};
use graph_client::{ReadError, WriteOutcome};
use graph_store::{GraphSession, SessionEvent};
use serde_json::{Map, Value};
use studio_actions::query::{CopyCell, CopyRow, Explain, Run};
use studio_ui::{
    GridColumn, ROW_HEIGHT, format_value, grid_cell, grid_header, is_numeric, short_commit_id,
};
use studio_workspace::{Item, ItemEvent, SavedItem};
use ui::prelude::*;
use ui::{KeyBinding, Tooltip};

use crate::analysis::Analysis;
use crate::completion::GqCompletionProvider;
use crate::results::{
    PlanItem, ResultTable, ServerDiagnostic, SourceMap, copy_text, format_elapsed, is_plan,
    plan_items, rows_label, server_diagnostic, short_commit, type_label,
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
const SYNTAX: [(TokenKind, &str); 9] = [
    (TokenKind::Keyword, "keyword"),
    (TokenKind::Variable, "variable"),
    (TokenKind::Type, "type"),
    (TokenKind::Property, "property"),
    (TokenKind::Annotation, "attribute"),
    (TokenKind::QueryName, "function"),
    (TokenKind::String, "string"),
    (TokenKind::Number, "number"),
    (TokenKind::Comment, "comment"),
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
    widths: Vec<f32>,
    right_aligned: Vec<bool>,
    row_count: i64,
    elapsed: Duration,
    target: String,
    commit: Option<String>,
}

struct PlanOutput {
    label: String,
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
    file_error: Option<String>,
    /// Reads run at this commit and writes are refused (spec 009); `None` uses the branch.
    at_commit: Option<String>,
    editor: Entity<Editor>,
    /// The latest check; it lags the text by `CHECK_DELAY`.
    analysis: Option<Arc<Analysis>>,
    cursor: usize,
    params: HashMap<String, Entity<Editor>>,
    output: Output,
    pending_write: Option<PendingWrite>,
    writing: bool,
    selected: Option<(usize, usize)>,
    example: Option<String>,
    results_scroll: UniformListScrollHandle,
    results_focus: FocusHandle,
    confirm_focus: FocusHandle,
    run_task: Option<Task<()>>,
    check_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ItemEvent> for QueryEditor {}

impl Focusable for QueryEditor {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Item for QueryEditor {
    fn tab_title(&self, _cx: &App) -> SharedString {
        let name = match &self.path {
            Some(path) => path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("Query {}", self.number)),
            None => format!("Query {}", self.number),
        };
        match &self.at_commit {
            Some(commit) => format!("{name} @ {}", short_commit_id(commit)).into(),
            None => name.into(),
        }
    }

    /// A file-backed query with changes since it was saved.
    fn is_dirty(&self, cx: &App) -> bool {
        self.writing
            || self
                .saved_text
                .as_ref()
                .is_some_and(|saved| *saved != self.editor.read(cx).text(cx))
    }

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(IconName::FileCode)
    }

    fn dedupe_key(&self, _cx: &App) -> Option<SharedString> {
        Some(match &self.path {
            Some(path) => file_key(path).into(),
            None => format!("query:{}", self.number).into(),
        })
    }

    /// The draft comes back after a restart (spec 007: the text is never cleared). It is
    /// written to the workspace file, never to a log.
    fn serialize(&self, cx: &App) -> Option<SavedItem> {
        Some(SavedItem {
            kind: "query".into(),
            data: serde_json::json!({
                "text": self.editor.read(cx).text(cx),
                "at_commit": self.at_commit,
                "path": self.path.as_ref().map(|path| path.to_string_lossy().into_owned()),
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
        let editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_placeholder_text(
                "Write a query. ⌘↵ runs the query under the cursor.",
                window,
                cx,
            );
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
                    SessionEvent::SchemaChanged => {
                        this.set_example(window, cx);
                        this.schedule_check(window, cx);
                    }
                    SessionEvent::BranchChanged => cx.notify(),
                    _ => {}
                },
            ),
        ];
        let mut this = Self {
            number,
            session,
            path: None,
            saved_text: None,
            file_error: None,
            at_commit: None,
            editor,
            analysis: None,
            cursor: 0,
            params: HashMap::new(),
            output: Output::Empty,
            pending_write: None,
            writing: false,
            selected: None,
            example: None,
            results_scroll: UniformListScrollHandle::new(),
            results_focus: cx.focus_handle(),
            confirm_focus: cx.focus_handle(),
            run_task: None,
            check_task: None,
            _subscriptions: subscriptions,
        };
        this.set_example(window, cx);
        this
    }

    /// Pins reads to a commit and refuses writes, for a query opened from History.
    pub fn pin_to(&mut self, commit: String, cx: &mut Context<Self>) {
        self.at_commit = Some(commit);
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
        self.file_error = None;
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// ⌘S: writes the file this tab edits; a new query is saved in this graph's queries folder,
    /// named after its first declaration.
    fn save(
        &mut self,
        _: &studio_actions::query::Save,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
            if let Some(dir) = target.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&target, written)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = write.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.path = Some(path);
                        this.saved_text = Some(text);
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
        if self.example.is_some() || !self.editor.read(cx).is_empty(cx) {
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
        if self.saved_text.is_some() {
            // The tab's changed-since-saved dot follows the text.
            cx.emit(ItemEvent::UpdateTab);
        }
        self.highlight_syntax(cx);
        self.schedule_check(window, cx);
        self.cursor_moved(window, cx);
    }

    fn cursor_moved(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.editor.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        self.cursor = editor
            .selections
            .newest_anchor()
            .head()
            .to_offset(&snapshot)
            .0;
        self.sync_params(window, cx);
        cx.notify();
    }

    fn highlight_syntax(&mut self, cx: &mut Context<Self>) {
        let theme = cx.theme().clone();
        self.editor.update(cx, |editor, cx| {
            let text = editor.text(cx);
            let tokens = gq_language::tokenize(&text);
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            for (index, (kind, name)) in SYNTAX.iter().enumerate() {
                let key = HighlightKey::ConsoleAnsiHighlight(SYNTAX_KEY + index);
                let Some(style) = theme.syntax().style_for_name(name) else {
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
        let Some(schema) = self.schema(cx) else {
            return;
        };
        self.check_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(CHECK_DELAY).await;
            let Ok(text) = this.update(cx, |this, cx| this.editor.read(cx).text(cx)) else {
                return;
            };
            let analysis = cx
                .background_spawn(async move { Analysis::new(&schema, text) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.apply_analysis(Arc::new(analysis), window, cx)
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
        let error = cx.theme().status().error;
        let key = HighlightKey::ConsoleAnsiHighlight(DIAGNOSTIC_KEY);
        self.editor.update(cx, |editor, cx| {
            // A check of older text would underline the wrong places; a newer one is coming.
            if editor.text(cx) != analysis.text {
                return;
            }
            let snapshot = editor.buffer().read(cx).snapshot(cx);
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
        });
        self.analysis = Some(analysis);
        self.sync_params(window, cx);
        cx.notify();
    }

    /// The declaration under the cursor and what the last check said about it.
    fn current_query(&self) -> Option<(String, Option<QueryInfo>)> {
        let analysis = self.analysis.as_ref()?;
        let (declaration, info) = analysis.query_at(self.cursor)?;
        Some((declaration.name.clone(), info.cloned()))
    }

    /// Makes an input for each parameter of the query under the cursor. Inputs are kept by
    /// name, so a value survives edits and moving between queries.
    fn sync_params(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, Some(info))) = self.current_query() else {
            return;
        };
        for (name, _, nullable) in &info.params {
            if self.params.contains_key(name) {
                continue;
            }
            let placeholder = if *nullable { "empty is null" } else { "value" };
            let input = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text(placeholder, window, cx);
                editor
            });
            self.params.insert(name.clone(), input);
        }
    }

    fn jump_to(&mut self, offset: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            let offset = offset.min(editor.text(cx).len());
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
        let source = self.editor.read(cx).text(cx);
        let outline = gq_language::outline(&source);
        if let Some(statement) = outline.statement {
            if explain {
                return Err("Explain shows the plan of a query, not of a statement.".into());
            }
            let label = source[outline.prefix.end..]
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_owned();
            return Ok(Prepared {
                text: source.clone(),
                name: None,
                label,
                write: statement == Statement::Write,
                statement: true,
                params: None,
                columns: None,
                map: SourceMap {
                    prefix_len: source.len(),
                    ..Default::default()
                },
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
        cx.notify();
    }

    fn explain(&mut self, _: &Explain, _window: &mut Window, cx: &mut Context<Self>) {
        match self.prepare(true, cx) {
            Err(message) => self.output = Output::Failed(Failure::new(message)),
            Ok(prepared) => self.send_read(prepared, cx),
        }
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
        let task = self
            .session
            .update(cx, |session, cx| session.read_request(request, cx));
        self.output = Output::Running {
            label: format!("Running {} on {branch}…", prepared.label).into(),
        };
        self.selected = None;
        // Replacing the task cancels an earlier read still in flight.
        self.run_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let elapsed = started.elapsed();
            this.update(cx, |this, cx| {
                this.finish_read(prepared, branch, result, elapsed);
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
    ) {
        self.output = match result {
            Ok(output) if is_plan(&output.columns) => Output::Plan(PlanOutput {
                label: prepared.label,
                items: plan_items(&output.rows),
                elapsed,
                commit: output.graph_commit_id,
            }),
            Ok(output) => {
                let table =
                    ResultTable::new(output.columns, &output.rows, prepared.columns.as_deref());
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
                Output::Rows(RowsOutput {
                    label: prepared.label,
                    table,
                    widths,
                    right_aligned,
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
        let request = ChangeRequest {
            query: prepared.text.clone(),
            name: prepared.name.clone(),
            params: prepared.params.clone(),
            branch: (!prepared.statement).then(|| branch.clone()),
        };
        let description = format!("Run {}", prepared.label);
        let started = Instant::now();
        // No read precondition: the person confirmed this write against the branch head.
        let task = self.session.update(cx, |session, cx| {
            session.write_request(request, None, description, cx)
        });
        self.writing = true;
        self.output = Output::Running {
            label: format!("Writing {} to {branch}…", prepared.label).into(),
        };
        let map = prepared.map;
        // Detached, not stored: closing the tab must not cancel a write in flight, and the
        // session logs its outcome either way (constitution III).
        cx.spawn(async move |this, cx| {
            let outcome = task.await;
            let elapsed = started.elapsed();
            this.update(cx, |this, cx| {
                this.writing = false;
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
        let focus = self.editor.focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn cancel_write(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_write = None;
        let focus = self.editor.focus_handle(cx);
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
        self.selected = Some((row, column));
        window.focus(&self.results_focus, cx);
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
            cx.write_to_clipboard(ClipboardItem::new_string(copy_text(value)));
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

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let editor_focus = self.editor.focus_handle(cx);
        let current = self.current_query();
        let (summary, writes): (String, bool) = match &current {
            Some((name, Some(info))) if info.is_mutation => (format!("{name} · writes"), true),
            Some((name, Some(_))) => (format!("{name} · read"), false),
            Some((name, None)) if !name.is_empty() => (name.clone(), false),
            _ => (String::new(), false),
        };
        let busy = self.writing;
        let pinned = self.at_commit.clone();
        let file_error = self.file_error.clone();
        studio_ui::ds::toolbar(cx)
            .px_2()
            .when_some(file_error, |toolbar, error| {
                toolbar.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .when_some(pinned, |toolbar, commit| {
                toolbar.child(
                    div()
                        .id("read-only-commit")
                        .tooltip(Tooltip::text(format!(
                            "Reads run at commit {commit}; writes are refused"
                        )))
                        .child(studio_ui::ds::meta_code(
                            IconName::GitCommit,
                            format!("Read only · {}", short_commit_id(&commit)),
                            cx,
                        )),
                )
            })
            .child(
                Button::new("run", "Run")
                    .style(ButtonStyle::Filled)
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(IconName::PlayFilled).size(IconSize::XSmall))
                    .key_binding(KeyBinding::for_action_in(&Run, &editor_focus, cx))
                    .disabled(busy)
                    .tooltip({
                        let focus = editor_focus.clone();
                        move |_window, cx| {
                            if busy {
                                Tooltip::simple("A write is in flight; wait for its outcome.", cx)
                            } else {
                                Tooltip::for_action_in(
                                    "Run the query under the cursor",
                                    &Run,
                                    &focus,
                                    cx,
                                )
                            }
                        }
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.run(&Run, window, cx))),
            )
            .child(
                Button::new("explain", "Explain")
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(IconName::ListTree).size(IconSize::XSmall))
                    .key_binding(KeyBinding::for_action_in(&Explain, &editor_focus, cx))
                    .tooltip({
                        let focus = editor_focus.clone();
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
            .child(div().flex_1())
            .when(!summary.is_empty(), |d| {
                d.child(Label::new(summary).size(LabelSize::Small).color(if writes {
                    Color::Warning
                } else {
                    Color::Muted
                }))
            })
    }

    fn render_problems(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
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
                        .map(|(index, (problem, range))| {
                            let at = range.as_ref().map(|r| r.start);
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
                                    if let Some(at) = at {
                                        this.jump_to(at, window, cx);
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

    fn render_params(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (_, Some(info)) = self.current_query()? else {
            return None;
        };
        if info.params.is_empty() {
            return None;
        }
        let colors = cx.theme().colors();
        let (border, input_bg) = (colors.border_variant, colors.editor_background);
        Some(
            h_flex()
                .flex_none()
                .flex_wrap()
                .px_2()
                .py_1p5()
                .gap_3()
                .border_t_1()
                .border_color(border)
                .child(
                    Label::new("Parameters")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .children(info.params.iter().filter_map(|(name, ty, nullable)| {
                    let input = self.params.get(name)?.clone();
                    Some(
                        h_flex()
                            .gap_1p5()
                            .child(Label::new(format!("${name}")).size(LabelSize::Small))
                            .child(
                                Label::new(format!("{ty}{}", if *nullable { "?" } else { "" }))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                div()
                                    .w(px(180.))
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(border)
                                    .bg(input_bg)
                                    .child(input),
                            ),
                    )
                }))
                .into_any_element(),
        )
    }

    fn render_confirm(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
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
                .px_2()
                .py_1p5()
                .gap_2()
                .bg(background)
                .border_t_1()
                .border_color(border)
                .child(
                    Icon::new(IconName::Warning)
                        .size(IconSize::Small)
                        .color(Color::Warning),
                )
                .child(Label::new(what).size(LabelSize::Small))
                .child(div().flex_1())
                .child(
                    Button::new("confirm-write", button)
                        .style(ButtonStyle::Filled)
                        .label_size(LabelSize::Small)
                        .key_binding(KeyBinding::for_action_in(
                            &menu::Confirm,
                            &self.confirm_focus,
                            cx,
                        ))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.confirm_write(&menu::Confirm, window, cx)
                        })),
                )
                .child(
                    Button::new("cancel-write", "Cancel")
                        .label_size(LabelSize::Small)
                        .key_binding(KeyBinding::for_action_in(
                            &menu::Cancel,
                            &self.confirm_focus,
                            cx,
                        ))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.cancel_write(&menu::Cancel, window, cx)
                        })),
                )
                .into_any_element(),
        )
    }

    fn status_line(&self, cx: &mut Context<Self>) -> Div {
        let border = cx.theme().colors().border_variant;
        h_flex()
            .h(px(30.))
            .flex_none()
            .px_2()
            .gap_3()
            .border_t_1()
            .border_b_1()
            .border_color(border)
    }

    fn render_results(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match &self.output {
            Output::Empty => self.render_expected(cx),
            Output::Running { label } => v_flex()
                .flex_1()
                .min_h_0()
                .child(
                    self.status_line(cx).child(
                        Label::new(label.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
                .into_any_element(),
            Output::Rows(rows) => self.render_rows(rows, window, cx),
            Output::Plan(plan) => self.render_plan(plan, cx),
            Output::Written(result) => self.render_written(result, cx),
            Output::Failed(failure) => self.render_failure(failure, cx),
        }
    }

    /// Before the first run: the columns the query under the cursor will return.
    fn render_expected(&self, cx: &mut Context<Self>) -> AnyElement {
        let editor_focus = self.editor.focus_handle(cx);
        let current = self.current_query();
        let mut body = v_flex().flex_1().min_h_0();
        match current {
            Some((name, Some(info))) if !info.is_mutation && !info.columns.is_empty() => {
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
                body = body
                    .child(
                        self.status_line(cx)
                            .child(
                                Label::new(format!("{name} returns"))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(div().flex_1())
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        Label::new("Run")
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .child(KeyBinding::for_action_in(&Run, &editor_focus, cx)),
                            ),
                    )
                    .child(
                        div()
                            .id("expected-columns")
                            .overflow_x_scroll()
                            .child(grid_header(&columns, px(ROW_NUMBER_WIDTH), cx)),
                    );
            }
            Some((name, Some(info))) if info.is_mutation => {
                body = body.child(studio_ui::empty_state(
                    format!("{name} changes data"),
                    Some(
                        format!(
                            "Run asks before it writes to {}. Nothing runs on ⌘↵ alone.",
                            self.branch(cx)
                        )
                        .into(),
                    ),
                ));
            }
            _ => {
                body = body.child(studio_ui::empty_state(
                    "Results appear here",
                    Some("⌘↵ runs the query under the cursor; ⌘⇧↵ shows its plan.".into()),
                ));
            }
        }
        body.into_any_element()
    }

    fn render_rows(
        &self,
        rows: &RowsOutput,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
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
        let has_selection = self.selected.is_some();
        let commit = rows.commit.clone();
        let status = self
            .status_line(cx)
            .child(Label::new(rows_label(rows.row_count)).size(LabelSize::Small))
            .child(
                Label::new(format_elapsed(rows.elapsed))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Label::new(format!("{} on {}", rows.label, rows.target))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .children(commit.map(|commit| {
                div()
                    .id("read-commit")
                    .child(
                        Label::new(format!("at {}", short_commit(&commit)))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .tooltip(Tooltip::text(format!("Read at graph commit {commit}")))
            }))
            .child(div().flex_1())
            .child(
                Button::new("copy-cell", "Copy cell")
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(IconName::Copy).size(IconSize::XSmall))
                    .disabled(!has_selection)
                    .key_binding(KeyBinding::for_action_in(
                        &CopyCell,
                        &self.results_focus,
                        cx,
                    ))
                    .tooltip(move |_window, cx| {
                        if has_selection {
                            Tooltip::simple("Copy the selected cell's value", cx)
                        } else {
                            Tooltip::simple("Click a cell to select it first", cx)
                        }
                    })
                    .on_click(
                        cx.listener(|this, _, window, cx| this.copy_cell(&CopyCell, window, cx)),
                    ),
            )
            .when_some(
                self.session
                    .read(cx)
                    .schema_model()
                    .and_then(|schema| rows.table.graph(&schema)),
                |status, (nodes, links)| {
                    let title = format!("{} · result", rows.label);
                    status.child(
                        Button::new("result-canvas", "Canvas")
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::GitGraph).size(IconSize::XSmall))
                            .tooltip(Tooltip::text(format!(
                                "Draw the result's {} nodes, linking those in the same row",
                                nodes.len()
                            )))
                            .on_click(move |_, window, cx| {
                                window.dispatch_action(
                                    Box::new(studio_actions::graph::ShowResultsOnCanvas {
                                        title: title.clone(),
                                        nodes: nodes.clone(),
                                        links: links.clone(),
                                    }),
                                    cx,
                                )
                            }),
                    )
                },
            )
            .child(
                Button::new("copy-row", "Copy row")
                    .label_size(LabelSize::Small)
                    .disabled(!has_selection)
                    .key_binding(KeyBinding::for_action_in(&CopyRow, &self.results_focus, cx))
                    .tooltip(move |_window, cx| {
                        if has_selection {
                            Tooltip::simple("Copy the selected row as JSON", cx)
                        } else {
                            Tooltip::simple("Click a cell to select it first", cx)
                        }
                    })
                    .on_click(
                        cx.listener(|this, _, window, cx| this.copy_row(&CopyRow, window, cx)),
                    ),
            );
        let body = if table.rows.is_empty() {
            v_flex()
                .child(grid_header(&columns, px(ROW_NUMBER_WIDTH), cx))
                .child(studio_ui::empty_state("No rows", None))
                .into_any_element()
        } else {
            div()
                .id("query-results-scroll")
                .flex_1()
                .min_h_0()
                .overflow_x_scroll()
                .child(
                    v_flex()
                        .h_full()
                        .w(px(total))
                        .child(grid_header(&columns, px(ROW_NUMBER_WIDTH), cx))
                        .child(
                            uniform_list(
                                "query-results",
                                table.rows.len(),
                                cx.processor(|this, range, _window, cx| {
                                    this.render_grid_rows(range, cx)
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
            .flex_1()
            .min_h_0()
            .child(status)
            .child(body)
            .into_any_element()
    }

    fn render_grid_rows(
        &mut self,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Output::Rows(rows) = &self.output else {
            return Vec::new();
        };
        let border = studio_ui::ds::surface::rule(cx);
        let colors = cx.theme().colors();
        let (selected_bg, row_selected_bg) = (colors.element_selected, colors.ghost_element_hover);
        let selected = self.selected;
        range
            .filter_map(|ix| {
                let values = rows.table.rows.get(ix)?;
                let row_selected = selected.is_some_and(|(row, _)| row == ix);
                Some(
                    h_flex()
                        .h(px(ROW_HEIGHT))
                        .border_b_1()
                        .border_color(border)
                        .when(row_selected, |d| d.bg(row_selected_bg))
                        .child(
                            div().w(px(ROW_NUMBER_WIDTH)).flex_none().px_2().child(
                                Label::new((ix + 1).to_string())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                        )
                        .children(values.iter().enumerate().map(|(column, value)| {
                            let cell_selected = selected == Some((ix, column));
                            let text = format_value(value);
                            grid_cell(px(rows.widths[column]), rows.right_aligned[column])
                                .when(cell_selected, |d| d.bg(selected_bg))
                                .child(
                                    Label::new(text)
                                        .size(LabelSize::Small)
                                        .color(if value.is_null() {
                                            Color::Disabled
                                        } else {
                                            Color::Default
                                        })
                                        .single_line()
                                        .truncate(),
                                )
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, window, cx| {
                                        this.select_cell(ix, column, window, cx)
                                    }),
                                )
                        }))
                        .into_any_element(),
                )
            })
            .collect()
    }

    fn render_plan(&self, plan: &PlanOutput, cx: &mut Context<Self>) -> AnyElement {
        let status = self
            .status_line(cx)
            .child(Label::new(format!("Plan of {}", plan.label)).size(LabelSize::Small))
            .child(
                Label::new(format_elapsed(plan.elapsed))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .children(plan.commit.clone().map(|commit| {
                div()
                    .id("plan-commit")
                    .child(
                        Label::new(format!("at {}", short_commit(&commit)))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .tooltip(Tooltip::text(format!("Planned at graph commit {commit}")))
            }));
        v_flex()
            .flex_1()
            .min_h_0()
            .child(status)
            .child(
                uniform_list(
                    "query-plan",
                    plan.items.len(),
                    cx.processor(|this, range, _window, cx| this.render_plan_rows(range, cx)),
                )
                .flex_1()
                .py_1(),
            )
            .into_any_element()
    }

    fn render_plan_rows(
        &mut self,
        range: std::ops::Range<usize>,
        _cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
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
                        summary,
                        detail,
                    } => h_flex()
                        .id(("plan-row", ix))
                        .h(px(PLAN_ROW_HEIGHT))
                        .pl(px(12. + *depth as f32 * 16.))
                        .pr_2()
                        .gap_2()
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
                        .child(
                            Label::new(summary.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .single_line()
                                .truncate(),
                        )
                        .when(!detail.is_empty(), |d| {
                            d.tooltip(Tooltip::text(detail.clone()))
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
        let (icon, color, title, detail, diagnostic) = match result {
            WriteResult::Committed {
                branch,
                nodes,
                edges,
                commit,
                elapsed,
            } => (
                IconName::Check,
                Color::Success,
                match commit {
                    Some(_) => format!("Committed to {branch}"),
                    None => format!("Nothing changed on {branch}"),
                },
                format!(
                    "{nodes} node{} and {edges} edge{} affected · {}{}",
                    if *nodes == 1 { "" } else { "s" },
                    if *edges == 1 { "" } else { "s" },
                    format_elapsed(*elapsed),
                    commit
                        .as_ref()
                        .map(|c| format!(" · commit {}", short_commit(c)))
                        .unwrap_or_default()
                ),
                None,
            ),
            WriteResult::Refused {
                status,
                message,
                diagnostic,
            } => (
                IconName::XCircle,
                Color::Error,
                format!("Refused ({status}): {message}"),
                "Nothing was written. Your query is unchanged; fix it and run again.".to_owned(),
                diagnostic.clone(),
            ),
            WriteResult::Unknown { reason } => (
                IconName::Warning,
                Color::Warning,
                format!("The outcome is unknown: {reason}"),
                "The write may or may not have landed. Check History before trying again; Studio does not retry."
                    .to_owned(),
                None,
            ),
        };
        self.render_message(icon, color, title, Some(detail), diagnostic, cx)
    }

    fn render_failure(&self, failure: &Failure, cx: &mut Context<Self>) -> AnyElement {
        self.render_message(
            IconName::XCircle,
            Color::Error,
            failure.title.clone(),
            failure.hint.clone(),
            failure.diagnostic.clone(),
            cx,
        )
    }

    fn render_message(
        &self,
        icon: IconName,
        color: Color,
        title: String,
        detail: Option<String>,
        diagnostic: Option<ServerDiagnostic>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        v_flex()
            .flex_1()
            .min_h_0()
            .p_3()
            .gap_1p5()
            .child(
                h_flex()
                    .gap_2()
                    .items_start()
                    .child(Icon::new(icon).size(IconSize::Small).color(color))
                    .child(Label::new(title).size(LabelSize::Small)),
            )
            .children(detail.map(|detail| {
                div().pl_6().child(
                    Label::new(detail)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            }))
            .children(diagnostic.map(|diagnostic| {
                let at = diagnostic.byte;
                div().pl_6().child(
                    Button::new(
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
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(IconName::ArrowRight).size(IconSize::XSmall))
                    .disabled(at.is_none())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(at) = at {
                            this.jump_to(at, window, cx);
                        }
                    })),
                )
            }))
            .into_any_element()
    }
}

impl Render for QueryEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().colors().border;
        let toolbar = self.render_toolbar(cx).into_any_element();
        let problems = self.render_problems(cx);
        let params = self.render_params(cx);
        let confirm = self.render_confirm(cx);
        let results = self.render_results(window, cx);
        v_flex()
            .key_context("QueryEditor")
            .on_action(cx.listener(Self::run))
            .on_action(cx.listener(Self::explain))
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::save_as))
            .size_full()
            .child(toolbar)
            .child(
                div()
                    .h(relative(0.42))
                    .min_h(px(120.))
                    .flex_none()
                    .pt_1()
                    .child(self.editor.clone()),
            )
            .children(problems)
            .child(div().h_px().w_full().flex_none().bg(border))
            .children(params)
            .children(confirm)
            .child(results)
    }
}

/// The dedupe key of a tab editing `path`, so a file opens once.
pub fn file_key(path: &std::path::Path) -> String {
    format!("query-file:{}", path.display())
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

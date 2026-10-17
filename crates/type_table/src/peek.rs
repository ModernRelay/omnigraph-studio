//! The node view (specs 004, 014): one row's properties, copyable, and for a node its
//! relationship groups with counts and neighbours. It shows as the peek in the inspector, and
//! at full size as a node's page in a tab of its own. Every read is pinned to the commit the row
//! was read at, or to the first read when the node was opened by id (constitution II).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use editor::{Editor, EditorEvent};
use gpui::{
    App, ClipboardItem, Context, Entity, Focusable as _, Render, SharedString, Subscription, Task,
    Window,
};
use gq_language::input::{edit_text, parse_input};
use gq_language::{Direction, RelationshipGroup, SchemaModel, builder};
use graph_store::{DraftEntry, DraftKey, GraphSession, Loadable, SessionEvent};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use serde_json::{Map, Value};
use studio_actions::graph::{OpenBlob, OpenNodePage, PeekNode};
use studio_actions::node::{CancelField, ExpandField, SaveField, SaveNodeDrafts};
use studio_actions::table::{SelectNextSuggestion, SelectPreviousSuggestion};
use ui::prelude::*;
use ui::{ContextMenu, Indicator, PopoverMenu, Switch, ToggleState, Tooltip, rems_from_px};

use crate::blob::{
    IMAGE_MAX, PREVIEW_PREFIX, Preview, bytes_wanted, preview, save_value, suggested_file_name,
};
use crate::model::{
    FROM_COLUMN, Notice, SaveResult, TO_COLUMN, Tone, display_value, format_count, node_rows,
    reads_as_markdown, same_value, save_result, short_commit,
};
use graph_client::{BlobCell, BlobMeta};

/// First neighbours shown under an expanded relationship group.
pub const NEIGHBOUR_LIMIT: u32 = 8;

/// Neighbours a page reads at a time under each relationship group, with "Show more" for the
/// next (constitution IX).
const PAGE_NEIGHBOURS: u32 = 50;

/// The widest a page's text runs, as a document's measure.
const PAGE_WIDTH: f32 = 760.;

/// Debounce before relationship counts are read, so moving the cursor quickly through rows
/// does not send a burst of reads.
const COUNT_DELAY: Duration = Duration::from_millis(120);

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
    /// Text values with markup, rendered as markdown: by property, the text and its view.
    markdown: HashMap<String, (String, Entity<Markdown>)>,
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
    /// The node's drafts being saved together.
    saving_drafts: bool,
    /// Why the drafts' last save did not land; the second press of Discard drops them.
    drafts_notice: Option<Notice>,
    confirm_discard: bool,
    _tasks: Vec<Task<()>>,
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
            // While its own writes are out the view compares when they end: the head may be its
            // own commit.
            if let SessionEvent::HeadMoved { branch, head } = event
                && *branch == session.read(cx).branch
                && !this.holds_commit
                && this.saving.is_empty()
                && this.edge_writes.is_empty()
                && this.snapshot.as_ref().is_some_and(|pin| pin != head)
            {
                this.newer = true;
                cx.notify();
            }
        });
        cx.on_release(|this: &mut Self, cx| this.release_images(cx))
            .detach();
        let mut this = Self {
            session,
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
            markdown: HashMap::new(),
            edits: HashMap::new(),
            saving: HashSet::new(),
            adding: None,
            removing: None,
            edge_writes: HashSet::new(),
            group_notices: HashMap::new(),
            saving_drafts: false,
            drafts_notice: None,
            confirm_discard: false,
            _tasks: Vec::new(),
            _subscription: subscription,
        };
        match values {
            Some(values) => {
                this.values = Loadable::Loaded(values);
                this.load_groups(debounce, cx);
                this.load_blobs(debounce, cx);
            }
            None => this.load_node(cx),
        }
        this
    }

    pub fn subject(&self) -> &PeekSubject {
        &self.subject
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
    }

    /// Reads a node again at the branch head, with its relationships and Blobs.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.newer = false;
        self.snapshot = None;
        // What the table read belongs to the old commit.
        self.known.clear();
        self.groups.clear();
        self._tasks.clear();
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
        let query = match builder::node_by_id(&schema, type_name, id) {
            Ok(query) => query,
            Err(error) => {
                self.values = Loadable::Failed(error.to_string());
                return;
            }
        };
        self.values = Loadable::Loading;
        let snapshot = self.snapshot.clone();
        let read = self
            .session
            .update(cx, |session, cx| session.read(&query, snapshot, cx));
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(output) => {
                        if this.snapshot.is_none() {
                            this.snapshot = output.graph_commit_id.clone();
                        }
                        match node_rows(&output.rows).into_iter().next() {
                            Some(row) => {
                                this.values = Loadable::Loaded(row.values);
                                this.load_groups(false, cx);
                                this.load_blobs(false, cx);
                            }
                            None => {
                                this.values = Loadable::Failed(
                                    "This node does not exist at this commit.".into(),
                                );
                            }
                        }
                    }
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
            .filter(|p| p.kind == gq_language::ScalarKind::Blob)
            .map(|p| p.name.clone())
            .collect()
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
            self.blobs[index]._task = Some(cx.spawn(async move |this, cx| {
                if debounce {
                    cx.background_executor().timer(COUNT_DELAY).await;
                }
                let Ok(read) = session.update(cx, |session, cx| {
                    session.blob_meta(cell.clone(), snapshot.clone(), cx)
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
            .gap_2()
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
                    (Loadable::Failed(error), _) | (_, Loadable::Failed(error)) => Some(
                        Label::new(error.clone())
                            .size(LabelSize::Small)
                            .color(Color::Error)
                            .into_any_element(),
                    ),
                    (Loadable::Loaded(BlobMeta::External { uri }), _) => Some(
                        v_flex()
                            .gap_1()
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
                            .p_2()
                            .rounded_md()
                            .border_1()
                            .border_color(border)
                            .child(
                                gpui::img(image.source())
                                    .max_w_full()
                                    .max_h(px(240.))
                                    .object_fit(gpui::ObjectFit::Contain),
                            )
                            .into_any_element(),
                    ),
                    (_, Loadable::Loaded(Preview::Text { text, more })) => Some(
                        v_flex()
                            .gap_1()
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
                            .gap_1()
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
                        Some(SaveState::Failed(error)) => Some(
                            Label::new(error.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Error)
                                .into_any_element(),
                        ),
                        None if managed => Some(
                            h_flex()
                                .gap(px(studio_ui::ds::space::XXS))
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
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                Icon::new(IconName::File)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(Label::new(slot.property.clone()).size(LabelSize::Small))
                            .child(studio_ui::ds::text::meta(facts))
                            .child(div().flex_1())
                            .children(save),
                    )
                    .children(body)
            }))
            .into_any_element()
    }

    fn load_groups(&mut self, debounce: bool, cx: &mut Context<Self>) {
        let PeekSubject::Node { type_name, id } = self.subject.clone() else {
            return;
        };
        let Some(schema) = self.schema.clone() else {
            return;
        };
        self.groups = schema
            .relationships_of(&type_name)
            .into_iter()
            .map(|group| Group {
                group,
                count: Loadable::Loading,
                expanded: false,
                neighbours: Loadable::NotLoaded,
                more: None,
                _task: None,
            })
            .collect();
        for index in 0..self.groups.len() {
            let key = builder::relationship_key(&self.groups[index].group);
            if let Some(known) = self.known.get(&key).cloned() {
                let group = &mut self.groups[index];
                group.count = Loadable::Loaded(known.count);
                if let Some(first) = known.first {
                    group.neighbours = Loadable::Loaded(first);
                }
                // A small group shows its neighbours without a click, as below.
                if self.opens_by_itself(known.count) {
                    self.toggle_group(index, cx);
                }
                continue;
            }
            // The session's batched counts, which the explorer asks the same way: a node
            // open in both is read once at the commit.
            let session = self.session.downgrade();
            let snapshot = self.snapshot.clone();
            let relationship = self.groups[index].group.clone();
            let (type_name, id) = (type_name.clone(), id.clone());
            self.groups[index]._task = Some(cx.spawn(async move |this, cx| {
                if debounce {
                    cx.background_executor().timer(COUNT_DELAY).await;
                }
                let Ok(count) = session.update(cx, |session, cx| {
                    session.degree(&type_name, &relationship, &id, snapshot, cx)
                }) else {
                    return;
                };
                let result = count.await;
                this.update(cx, |this, cx| {
                    let count = match result {
                        Ok(n) => Loadable::Loaded(i64::try_from(n).unwrap_or(i64::MAX)),
                        Err(error) => Loadable::Failed(error),
                    };
                    let opens = matches!(count, Loadable::Loaded(n) if this.opens_by_itself(n));
                    if let Some(group) = this.groups.get_mut(index) {
                        group.count = count;
                        // A small group shows its neighbours without a click.
                        if opens && !group.expanded {
                            this.toggle_group(index, cx);
                        }
                    }
                    cx.notify();
                })
                .ok();
            }));
        }
        cx.notify();
    }

    fn toggle_group(&mut self, index: usize, cx: &mut Context<Self>) {
        let PeekSubject::Node { type_name, id } = self.subject.clone() else {
            return;
        };
        let Some(schema) = self.schema.clone() else {
            return;
        };
        let limit = self.neighbour_limit();
        let Some(group) = self.groups.get_mut(index) else {
            return;
        };
        group.expanded = !group.expanded;
        if !group.expanded || !matches!(group.neighbours, Loadable::NotLoaded) {
            cx.notify();
            return;
        }
        let query = match builder::neighbours(
            &schema,
            &type_name,
            &group.group,
            &id,
            None,
            builder::Narrow::default(),
            limit,
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
        let read = self
            .session
            .update(cx, |session, cx| session.read(&query, snapshot, cx));
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
        let query = match builder::neighbours(
            &schema,
            &type_name,
            &group.group,
            &id,
            Some(&after),
            builder::Narrow::default(),
            PAGE_NEIGHBOURS,
        ) {
            Ok(query) => query,
            Err(_) => return,
        };
        let snapshot = self.snapshot.clone();
        let read = self
            .session
            .update(cx, |session, cx| session.read(&query, snapshot, cx));
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
        if let Some(key) = self.draft_key(name) {
            self.session
                .update(cx, |session, cx| session.remove_draft(&key, cx));
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
        let old = self.value_of(&name);
        // A draft opens with its text; the save binds to the value the view shows.
        let text = self
            .draft_of(&name, cx)
            .map(|draft| draft.text)
            .unwrap_or_else(|| edit_text(&old));
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
        let typed = self
            .edits
            .remove(name)
            .map(|edit| edit.editor.read(cx).text(cx))
            .or_else(|| self.draft_of(name, cx).map(|draft| draft.text));
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
        // Detached: closing the page or moving the peek never cancels a write.
        cx.spawn_in(window, async move |this, cx| {
            let result = write.await;
            this.update_in(cx, |this, window, cx| {
                this.finish_write(property, old, text, result, window, cx)
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

    /// Whether the branch moved past the commit shown, once this view's writes are in.
    fn compare_head(&mut self, cx: &mut Context<Self>) {
        if self.holds_commit {
            return;
        }
        let session = self.session.read(cx);
        if let (Some(head), Some(pin)) = (session.head(&session.branch), self.snapshot.as_deref()) {
            self.newer = head != pin;
        }
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
        let text = adding.editor.read(cx).text(cx);
        let query = match builder::node_search(&schema, &group.group.other_type, text.trim(), 8) {
            Ok(query) => query,
            Err(error) => {
                adding.found = Loadable::Failed(error.to_string());
                return;
            }
        };
        let session = self.session.downgrade();
        adding._search = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COUNT_DELAY).await;
            let Ok(read) = session.update(cx, |session, cx| session.read(&query, None, cx)) else {
                return;
            };
            let result = read.await;
            this.update(cx, |this, cx| {
                if let Some(adding) = this.adding.as_mut() {
                    adding.found = match result {
                        Ok(output) => Loadable::Loaded(neighbours_from(&output.rows)),
                        Err(error) => Loadable::Failed(error.to_string()),
                    };
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
        let branch = self.session.read(cx).branch.clone();
        let other = match &change {
            EdgeChange::Add(other) => other.clone(),
            EdgeChange::Remove(_) => group.other_type.clone(),
        };
        self.edge_writes.insert(index);
        self.group_notices.remove(&index);
        let session = self.session.clone();
        let edge = group.edge.clone();
        // Detached: closing the page or moving the peek never cancels a write.
        cx.spawn_in(window, async move |this, cx| {
            let mut outcome = None;
            for (query, verb) in built {
                let description = format!("{verb} {edge} of {id} · {other}");
                let write = session.update(cx, |session, cx| {
                    session.write(&query, None, description, cx)
                });
                let result = save_result(&write.await, &branch);
                let landed = matches!(result, SaveResult::Saved { .. });
                outcome = Some(result);
                if !landed {
                    break;
                }
            }
            this.update(cx, |this, cx| {
                this.edge_writes.remove(&index);
                match outcome {
                    Some(SaveResult::Saved { commit, parent }) => {
                        if parent.is_some() && parent == this.snapshot {
                            this.snapshot = Some(commit);
                        } else {
                            this.newer = true;
                        }
                    }
                    Some(SaveResult::NoMatch) => {
                        this.group_notices.insert(
                            index,
                            Notice::new(
                                Tone::Conflict,
                                "Nothing changed: it was already so, or changed elsewhere",
                            ),
                        );
                    }
                    Some(SaveResult::Refused(notice) | SaveResult::Unknown(notice)) => {
                        this.group_notices.insert(index, notice);
                    }
                    None => {}
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
        let PeekSubject::Node { type_name, id } = self.subject.clone() else {
            return;
        };
        let Some(group) = self.groups.get_mut(index) else {
            return;
        };
        group.count = Loadable::Loading;
        group.neighbours = Loadable::NotLoaded;
        group.expanded = false;
        group.more = None;
        let session = self.session.downgrade();
        let snapshot = self.snapshot.clone();
        let relationship = group.group.clone();
        group._task = Some(cx.spawn(async move |this, cx| {
            let Ok(count) = session.update(cx, |session, cx| {
                session.degree(&type_name, &relationship, &id, snapshot, cx)
            }) else {
                return;
            };
            let result = count.await;
            this.update(cx, |this, cx| {
                let count = match result {
                    Ok(n) => Loadable::Loaded(i64::try_from(n).unwrap_or(i64::MAX)),
                    Err(error) => Loadable::Failed(error),
                };
                let open = matches!(count, Loadable::Loaded(n) if n > 0);
                if let Some(group) = this.groups.get_mut(index) {
                    group.count = count;
                }
                if open {
                    this.toggle_group(index, cx);
                }
                cx.notify();
            })
            .ok();
        }));
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
            let Some(property) = schema
                .node(&type_name)
                .and_then(|node| node.property(&name))
                .cloned()
            else {
                continue;
            };
            let old = self.value_of(&name);
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
        let row = builder::RowEdit {
            id: id.clone(),
            cells: cells.clone(),
        };
        let query = match builder::update_rows(&schema, &type_name, &[row]) {
            Ok(query) => query,
            Err(error) => {
                self.drafts_notice = Some(Notice::new(Tone::Invalid, error.to_string()));
                cx.notify();
                return;
            }
        };
        let branch = self.session.read(cx).branch.clone();
        let description = format!("Save {} changes of {id}", cells.len());
        let write = self.session.update(cx, |session, cx| {
            session.write(&query, None, description, cx)
        });
        self.saving_drafts = true;
        self.drafts_notice = None;
        for (name, _, _) in &cells {
            self.saving.insert(name.clone());
        }
        cx.spawn_in(window, async move |this, cx| {
            let result = save_result(&write.await, &branch);
            this.update(cx, |this, cx| {
                this.saving_drafts = false;
                for (name, _, _) in &cells {
                    this.saving.remove(name);
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
                .gap(px(studio_ui::ds::space::SM))
                .items_center()
                .child(Indicator::dot().color(Color::Accent))
                .child(Label::new(label).size(LabelSize::Small))
                .children(self.drafts_notice.as_ref().map(|notice| {
                    Label::new(notice.message.clone())
                        .size(LabelSize::XSmall)
                        .color(notice_color(notice.tone))
                }))
                .child(div().flex_1())
                .child(
                    studio_ui::ds::button(
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
                    .size(ButtonSize::Compact)
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, _, _, cx| this.discard_drafts(cx))),
                )
                .child(
                    studio_ui::ds::button(
                        "peek-save-drafts",
                        if self.saving_drafts {
                            "Saving…"
                        } else {
                            "Save"
                        },
                    )
                    .style(ButtonStyle::Filled)
                    .size(ButtonSize::Compact)
                    .label_size(LabelSize::Small)
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

    fn copy(&mut self, name: String, value: &Value, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(crate::model::copy_text(value)));
        self.copied = Some(name);
        cx.notify();
        self._tasks.push(cx.spawn(async move |this, cx| {
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

    /// The properties to list, in schema order, with each one's declared type.
    fn property_rows(&self) -> Vec<(String, Option<gq_language::Property>, String)> {
        let Some(schema) = &self.schema else {
            return Vec::new();
        };
        match &self.subject {
            PeekSubject::Node { type_name, .. } => schema
                .node(type_name)
                .map(|node| {
                    // Blob values are shown under Blobs, with their previews.
                    node.properties
                        .iter()
                        .filter(|p| p.projectable())
                        .map(|p| (p.name.clone(), Some(p.clone()), p.display_type()))
                        .collect()
                })
                .unwrap_or_default(),
            PeekSubject::Edge { edge_type, .. } => {
                let mut rows = Vec::new();
                if let Some(edge) = schema.edge(edge_type) {
                    rows.extend(
                        edge.properties
                            .iter()
                            .filter(|p| p.projectable())
                            .map(|p| (p.name.clone(), Some(p.clone()), p.display_type())),
                    );
                }
                rows
            }
        }
    }

    /// Keeps one markdown view per text value with markup, made again only when its text
    /// changes; a value that reads as itself draws as a label.
    fn sync_markdown(&mut self, cx: &mut Context<Self>) {
        let Loadable::Loaded(values) = &self.values else {
            self.markdown.clear();
            return;
        };
        let wanted: Vec<(String, String)> = self
            .property_rows()
            .into_iter()
            .filter_map(|(name, property, _)| {
                let text = values.get(&name)?.as_str()?;
                (reads_as_markdown(property.as_ref()?) && !studio_ui::text::is_plain(text))
                    .then(|| (name, text.to_owned()))
            })
            .collect();
        self.markdown
            .retain(|name, (text, _)| wanted.iter().any(|(n, t)| n == name && t == text));
        for (name, text) in wanted {
            self.markdown.entry(name).or_insert_with(|| {
                let source: SharedString = text.clone().into();
                (text, cx.new(|cx| Markdown::new(source, None, None, cx)))
            });
        }
    }

    fn render_properties(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let values = match &self.values {
            Loadable::Loaded(values) => values.clone(),
            Loadable::Failed(error) => {
                return Label::new(error.clone())
                    .size(LabelSize::Small)
                    .color(Color::Error)
                    .into_any_element();
            }
            _ => {
                return Label::new("Reading…")
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element();
            }
        };
        let hover = cx.theme().colors().element_hover;
        let rows = self.property_rows();
        let label_width = if self.page { 160. } else { 96. };
        v_flex()
            .gap_0p5()
            .children(
                rows.into_iter()
                    .enumerate()
                    .map(|(index, (name, property, ty))| {
                        let value = values.get(&name).cloned().unwrap_or(Value::Null);
                        let title = match name.as_str() {
                            FROM_COLUMN => "from".to_owned(),
                            TO_COLUMN => "to".to_owned(),
                            _ => name.clone(),
                        };
                        let copied = self.copied.as_deref() == Some(name.as_str());
                        let editable = property.as_ref().is_some_and(|p| self.edits_property(p));
                        let editing = self.edits.contains_key(&name);
                        let saving = self.saving.contains(&name);
                        // An unsaved value shows what was typed, with a dot, and opens on a click.
                        let draft = (editable && !editing && !saving)
                            .then(|| self.draft_of(&name, cx))
                            .flatten();
                        // An enum is a menu of its values and a boolean a switch, each saving
                        // as it is chosen; other values open an editor on a click.
                        let picker = property
                            .as_ref()
                            .filter(|_| editable && !editing && !saving && draft.is_none())
                            .and_then(|p| self.render_picker(index, p, &value, cx));
                        let opens_editor = editable && picker.is_none() && !editing && !saving;
                        let body = match (self.edits.get(&name), picker) {
                            (Some(edit), _) => self.render_field_edit(edit, window, cx),
                            (None, _) if draft.is_some() => {
                                let text = draft.as_ref().map(|d| d.text.clone()).unwrap_or_default();
                                h_flex()
                                    .gap(px(studio_ui::ds::space::SM))
                                    .child(
                                        Label::new(if text.trim().is_empty() { "—".to_owned() } else { studio_ui::text::first_line(&text) })
                                            .size(LabelSize::Small),
                                    )
                                    .child(Indicator::dot().color(Color::Accent))
                                    .into_any_element()
                            }
                            (None, Some(picker)) => picker,
                            (None, None) => {
                                self.render_value(&name, property.as_ref(), &value, window, cx)
                            }
                        };
                        let edit_property = property.clone();
                        let copy_name = name.clone();
                        let copy_value = value.clone();
                        // Text opens in the editor from a button the row shows on hover.
                        let expands =
                            opens_editor && property.as_ref().is_some_and(reads_as_markdown);
                        let group = SharedString::from(format!("peek-property-{index}"));
                        let expand_name = name.clone();
                        h_flex()
                            .id(("peek-property", index))
                            .group(group.clone())
                            .px_1()
                            .py_0p5()
                            .gap_2()
                            .items_start()
                            .rounded_sm()
                            .when(!editing, |row| row.hover(move |s| s.bg(hover)))
                            .when(opens_editor || !editable, |row| row.cursor_pointer())
                            .when(!editing, |row| {
                                row.tooltip(Tooltip::text(if draft.is_some() {
                                    format!(
                                        "Not saved; it holds “{}”. Click to edit; ⌘S saves this node's changes together",
                                        display_value(property.as_ref(), &value)
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
                                if opens_editor {
                                    if let Some(property) = edit_property.clone() {
                                        this.start_edit(property, window, cx);
                                    }
                                } else if !editable {
                                    this.copy(copy_name.clone(), &copy_value, cx)
                                }
                            }))
                            .child(
                                div()
                                    .w(px(label_width))
                                    .flex_none()
                                    .pt(px(if editing {
                                        studio_ui::ds::space::SM
                                    } else {
                                        0.
                                    }))
                                    .child(
                                        Label::new(title)
                                            .size(LabelSize::Small)
                                            .color(Color::Muted)
                                            .truncate(),
                                    ),
                            )
                            .child(div().flex_1().min_w_0().child(body))
                            .when(expands, |row| {
                                row.child(
                                    IconButton::new(
                                        ("peek-open-editor", index),
                                        IconName::Maximize,
                                    )
                                    .icon_size(IconSize::XSmall)
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
                                        .pt(px(6.))
                                        .tooltip(Tooltip::text("Saving…"))
                                        .child(Indicator::dot().color(Color::Warning)),
                                )
                            })
                            .when(copied, |d| {
                                d.child(
                                    Label::new("Copied")
                                        .size(LabelSize::XSmall)
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
                .text_size(ui::TextSize::Small.rems(cx))
                .child(MarkdownElement::new(view.clone(), value_style(window, cx)))
                .into_any_element(),
            None => Label::new(display_value(property, value))
                .size(LabelSize::Small)
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
                    .ml(px(-studio_ui::ds::space::XS))
                    .child(
                        PopoverMenu::new(SharedString::from(format!("peek-enum-{index}")))
                            .trigger(
                                studio_ui::ds::button(
                                    SharedString::from(format!("peek-enum-button-{index}")),
                                    shown,
                                )
                                .style(ButtonStyle::Subtle)
                                .size(ButtonSize::Compact)
                                .label_size(LabelSize::Small)
                                .end_icon(
                                    Icon::new(IconName::ChevronDown)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                ),
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
                Switch::new(("peek-switch", index), on.into())
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
            IconButton::new(
                SharedString::from(format!("peek-clear-{}", edit.property.name)),
                IconName::Close,
            )
            .icon_size(IconSize::XSmall)
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
        let reach = px(studio_ui::ds::space::MD + 1.) + editor_inset(window, cx);
        v_flex()
            .key_context("NodeField")
            .w_full()
            .gap(px(studio_ui::ds::space::XXS))
            .child(
                studio_ui::ds::field_frame(invalid, cx)
                    .ml(-reach)
                    .when(lines, |frame| {
                        frame
                            .h_auto()
                            .min_h(px(studio_ui::ds::size::CONTROL))
                            .py(px(studio_ui::ds::space::XS))
                            .items_start()
                    })
                    .child(studio_ui::ds::field_input(edit.editor.clone()))
                    .children(clear)
                    .children(lines.then(|| {
                        let name = edit.property.name.clone();
                        IconButton::new(
                            SharedString::from(format!("peek-expand-{name}")),
                            IconName::Maximize,
                        )
                        .icon_size(IconSize::XSmall)
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
                    .gap_1p5()
                    .child(Indicator::dot().color(notice_color(notice.tone)))
                    .child(
                        Label::new(notice.message)
                            .size(LabelSize::XSmall)
                            .color(notice_color(notice.tone)),
                    )
                    // Whether the save landed is in the branch's history.
                    .when(unknown, |row| {
                        row.child(
                            studio_ui::ds::button("peek-open-history", "Open history")
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::XSmall)
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(
                                        Box::new(studio_actions::graph::OpenHistory),
                                        cx,
                                    )
                                }),
                        )
                    })
            }))
            .children(hint.map(|hint| Label::new(hint).size(LabelSize::XSmall).color(Color::Muted)))
            .child(Label::new(keys).size(LabelSize::XSmall).color(Color::Muted))
            .into_any_element()
    }

    fn render_relationships(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.groups.is_empty() {
            return Label::new("This type takes part in no relationships.")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element();
        }
        let hover = cx.theme().colors().element_hover;
        v_flex()
            .gap_0p5()
            .children(self.groups.iter().enumerate().map(|(index, group)| {
                let icon = match group.group.direction {
                    Direction::Outgoing => IconName::ArrowRight,
                    Direction::Incoming => IconName::ArrowLeft,
                };
                let count = match &group.count {
                    Loadable::Loaded(n) => format_count(*n),
                    Loadable::Failed(_) => "?".into(),
                    _ => "…".into(),
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
                    .px_1()
                    .py_0p5()
                    .gap_1p5()
                    .rounded_sm()
                    .hover(move |s| s.bg(hover))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_group(index, cx)))
                    .when_some(failed, |d, error| d.tooltip(Tooltip::text(error)))
                    .child(
                        Icon::new(if group.expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
                    .child(
                        Label::new(gq_language::traversal_name(&group.group.edge))
                            .size(LabelSize::Small),
                    )
                    .child(
                        Label::new(group.group.other_type.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .when(self.edge_writes.contains(&index), |header| {
                        header.child(Indicator::dot().color(Color::Warning))
                    })
                    .children(add)
                    .child(
                        Label::new(count)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .buffer_font(cx),
                    );
                let adding = self
                    .adding
                    .as_ref()
                    .filter(|adding| adding.index == index)
                    .map(|adding| self.render_add_row(adding, cx));
                let notice = self.group_notices.get(&index).map(|notice| {
                    h_flex()
                        .pl(px(22.))
                        .gap_1p5()
                        .child(Indicator::dot().color(notice_color(notice.tone)))
                        .child(
                            Label::new(notice.message.clone())
                                .size(LabelSize::XSmall)
                                .color(notice_color(notice.tone)),
                        )
                });
                let body = group
                    .expanded
                    .then(|| self.render_neighbours(index, group, cx));
                v_flex()
                    .child(header)
                    .children(adding)
                    .children(notice)
                    .children(body)
            }))
            .into_any_element()
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
        IconButton::new(("peek-add", index), IconName::Plus)
            .icon_size(IconSize::XSmall)
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
        let hover = cx.theme().colors().element_hover;
        let selected = cx.theme().colors().element_selected;
        let branch = self.session.read(cx).branch.clone();
        let replaces = self.holds_one(index)
            && matches!(self.groups[index].count, Loadable::Loaded(n) if n > 0);
        let found = match &adding.found {
            Loadable::Loaded(found) if found.is_empty() => Label::new("No match")
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .into_any_element(),
            Loadable::Loaded(found) => v_flex()
                .children(found.iter().enumerate().map(|(n, node)| {
                    let highlighted = n == adding.highlighted;
                    h_flex()
                        .id(SharedString::from(format!("peek-found-{index}-{n}")))
                        .px_1()
                        .py_0p5()
                        .gap_2()
                        .rounded_sm()
                        .cursor_pointer()
                        .when(highlighted, |row| row.bg(selected))
                        .when(!highlighted, |row| row.hover(move |s| s.bg(hover)))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if let Some(adding) = this.adding.as_mut() {
                                adding.highlighted = n;
                            }
                            this.add_highlighted(window, cx);
                        }))
                        .child(studio_ui::type_swatch(&other))
                        .child(
                            Label::new(node.label.clone().unwrap_or_else(|| node.id.clone()))
                                .size(LabelSize::Small)
                                .truncate(),
                        )
                        .when(node.label.is_some(), |row| {
                            row.child(
                                Label::new(node.id.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                        })
                }))
                .into_any_element(),
            Loadable::Failed(error) => Label::new(error.clone())
                .size(LabelSize::XSmall)
                .color(Color::Error)
                .into_any_element(),
            _ => Label::new("Searching…")
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .into_any_element(),
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
                cx.listener(|this, _: &menu::Confirm, window, cx| this.add_highlighted(window, cx)),
            )
            .on_action(cx.listener(|this, _: &menu::Cancel, _, cx| this.cancel_add(cx)))
            .pl(px(22.))
            .py(px(studio_ui::ds::space::XS))
            .gap(px(studio_ui::ds::space::XS))
            .child(
                studio_ui::ds::field_frame(false, cx)
                    .child(
                        Icon::new(IconName::MagnifyingGlass)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(studio_ui::ds::field_input(adding.editor.clone())),
            )
            .child(found)
            .child(Label::new(keys).size(LabelSize::XSmall).color(Color::Muted))
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
            return studio_ui::ds::button(
                SharedString::from(format!("peek-remove-{index}-{n}")),
                "Remove",
            )
            .style(ButtonStyle::Tinted(ui::TintColor::Error))
            .size(ButtonSize::Compact)
            .label_size(LabelSize::XSmall)
            .tooltip(Tooltip::text(format!(
                "Remove this {edge}; both nodes stay"
            )))
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.press_remove(index, edge_id.clone(), window, cx)
            }))
            .into_any_element();
        }
        IconButton::new(
            SharedString::from(format!("peek-remove-{index}-{n}")),
            IconName::Close,
        )
        .icon_size(IconSize::XSmall)
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
        let hover = cx.theme().colors().element_hover;
        match &group.neighbours {
            Loadable::Loaded(neighbours) if neighbours.is_empty() => div()
                .pl(px(28.))
                .child(
                    Label::new("None at this commit")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
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
                        studio_ui::ds::button(
                            SharedString::from(format!("peek-more-{index}")),
                            if reading {
                                "Reading…".to_owned()
                            } else {
                                label
                            },
                        )
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::XSmall)
                        .disabled(reading)
                        .on_click(cx.listener(move |this, _, _, cx| this.show_more(index, cx)))
                        .into_any_element()
                    } else {
                        // The peek shows the first few; the page lists them all.
                        let open = self.open_page_action();
                        div()
                            .id(SharedString::from(format!("peek-more-{index}")))
                            .px_1()
                            .cursor_pointer()
                            .tooltip(Tooltip::text("Open the node's page to see them all"))
                            .on_click(move |_, window, cx| {
                                if let Some(open) = open.clone() {
                                    window.dispatch_action(Box::new(open), cx)
                                }
                            })
                            .child(
                                Label::new(format!("{label} · Open page"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .into_any_element()
                    }
                });
                let removes = self.edits_relationships();
                v_flex()
                    .pl(px(22.))
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
                            .px_1()
                            .py_0p5()
                            .gap_2()
                            .rounded_sm()
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
                            .child(studio_ui::type_swatch(&other))
                            .child(
                                Label::new(
                                    neighbour
                                        .label
                                        .clone()
                                        .unwrap_or_else(|| neighbour.id.clone()),
                                )
                                .size(LabelSize::Small)
                                .truncate(),
                            )
                            .when(neighbour.label.is_some(), |d| {
                                d.child(
                                    Label::new(neighbour.id.clone())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .truncate(),
                                )
                            })
                            .child(div().flex_1())
                            .children(remove)
                    }))
                    .children(show_more)
                    .into_any_element()
            }
            Loadable::Failed(error) => div()
                .pl(px(28.))
                .child(
                    Label::new(error.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Error),
                )
                .into_any_element(),
            _ => div()
                .pl(px(28.))
                .child(
                    Label::new("Reading…")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
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
        let hover = cx.theme().colors().element_hover;
        let at_commit = self.held_commit();
        let endpoint = |label: &'static str, ty: &String, key: &str| {
            let id = values.get(key).and_then(Value::as_str)?.to_owned();
            let (peek, page) = node_actions(ty, &id, at_commit.clone());
            Some(
                h_flex()
                    .id(label)
                    .px_1()
                    .py_0p5()
                    .gap_2()
                    .rounded_sm()
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
                            .w(px(40.))
                            .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
                    )
                    .child(studio_ui::type_swatch(ty))
                    .child(Label::new(id).size(LabelSize::Small))
                    .child(
                        Label::new(ty.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
        };
        Some(
            v_flex()
                .gap_0p5()
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

/// A notice's colour: red for what was refused or cannot be sent, amber for a conflict or an
/// unknown outcome.
fn notice_color(tone: Tone) -> Color {
    match tone {
        Tone::Refused | Tone::Invalid => Color::Error,
        Tone::Conflict | Tone::Unknown => Color::Warning,
    }
}

/// How far Zed's editor sets its text in from its left edge without a gutter: the font's
/// descent, its gutter margin (`GutterDimensions::default_gutter_margin`).
fn editor_inset(window: &Window, cx: &App) -> gpui::Pixels {
    let font = window.text_style().font();
    let size = ui::TextSize::Small.rems(cx).to_pixels(window.rem_size());
    let text = cx.text_system();
    -text.descent(text.resolve_font(&font), size)
}

/// A text value rendered as markdown as an agent reply is, with headings near the body's size,
/// at the size of the labels beside it and its soft line breaks kept, so plain lines stay lines
/// (spec 014).
fn value_style(window: &Window, cx: &App) -> MarkdownStyle {
    let mut style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx);
    style.base_text_style.font_size = ui::TextSize::Small.rems(cx).into();
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
    div().pt_2().pb_1().child(
        Label::new(title)
            .size(LabelSize::XSmall)
            .color(Color::Muted),
    )
}

impl Render for NodePeek {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_markdown(cx);
        let type_name = self.subject.type_name().to_owned();
        let id = self.subject.id().to_owned();
        let label = self.label();
        let at = self.snapshot.clone();
        let page = self.page;
        // The peek opens the node's page; the page is it.
        let open_page = (!page).then(|| self.open_page_action()).flatten();
        let has_properties = !self.property_rows().is_empty();
        let properties = self.render_properties(window, cx);
        // A node's relationships are listed once the node is read: while it loads, or when it
        // does not exist at this commit, an empty list would say its type has none.
        let relationships = (matches!(self.subject, PeekSubject::Node { .. })
            && self.values.loaded().is_some())
        .then(|| self.render_relationships(cx));
        let endpoints = self.render_endpoints(cx);
        let is_node = matches!(self.subject, PeekSubject::Node { .. });
        let drafts_bar = self.render_drafts_bar(cx);
        v_flex()
            .id("node-peek")
            .key_context("NodeView")
            .on_action(cx.listener(Self::save_drafts))
            .on_action(cx.listener(Self::on_save_field))
            .on_action(cx.listener(Self::on_cancel_field))
            .on_action(cx.listener(Self::on_expand_field))
            .w_full()
            .map(|view| {
                if page {
                    // A page reads at a document's width, with its margins.
                    view.max_w(px(PAGE_WIDTH))
                        .px(px(studio_ui::ds::space::PAGE))
                        .py(px(studio_ui::ds::space::XL))
                        .gap_2()
                } else {
                    view.p_4().gap_1p5()
                }
            })
            .child(
                h_flex()
                    .gap_1p5()
                    .child(studio_ui::type_swatch(&type_name))
                    .child(
                        Label::new(type_name.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .children(open_page.map(|open| {
                        IconButton::new("peek-open-page", IconName::ArrowUpRight)
                            .icon_size(IconSize::Small)
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
                            studio_ui::ds::button("peek-explore", "Explore")
                                .style(ButtonStyle::Subtle)
                                .start_icon(
                                    Icon::new(IconName::Crosshair)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .tooltip(Tooltip::text("Walk the graph from this node"))
                                .on_click(move |_, window, cx| {
                                    window.dispatch_action(Box::new(explore.clone()), cx)
                                }),
                        )
                    }),
            )
            .child(
                Label::new(label.unwrap_or_else(|| id.clone()))
                    .size(if page {
                        LabelSize::Custom(rems_from_px(22_f32))
                    } else {
                        LabelSize::Large
                    })
                    .weight(gpui::FontWeight::MEDIUM),
            )
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
                                short_commit(&commit),
                                cx,
                            ))
                    }))
                    // A node opened by id reads again by id; an edge is read with its table.
                    .when(self.newer && is_node, |meta| {
                        meta.child(
                            studio_ui::ds::button("peek-newer", "Newer data · Refresh")
                                .label_size(LabelSize::XSmall)
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
                endpoints.map(|endpoints| v_flex().child(section("Endpoints")).child(endpoints)),
            )
            .when(has_properties, |d| {
                d.child(section("Properties")).child(properties)
            })
            .when(!self.blobs.is_empty(), |d| {
                d.child(section("Blobs")).child(self.render_blobs(cx))
            })
            .children(relationships.map(|relationships| {
                v_flex()
                    .child(section("Relationships"))
                    .child(relationships)
            }))
    }
}

impl NodePeek {
    /// Whether this peek shows `id` of `type_name`.
    pub fn shows(&self, id: &str, _cx: &App) -> bool {
        self.subject.id() == id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use serde_json::json;

    use crate::test_support::{PEOPLE, session, session_answering};

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
        let (view, cx) = page(session_answering(PEOPLE, 403, refused, cx), cx);
        edit_name(&view, "Ada Lovelace", cx);
        view.read_with(cx, |view, cx| {
            assert_eq!(view.value_of("name"), json!("Ada"), "nothing was written");
            let edit = &view.edits["name"];
            assert_eq!(edit.editor.read(cx).text(cx), "Ada Lovelace");
            assert_eq!(edit.notice.as_ref().map(|n| n.tone), Some(Tone::Refused));
        });
    }
}

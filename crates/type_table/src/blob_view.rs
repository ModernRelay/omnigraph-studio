//! A Blob value in a tab of its own (spec 004), shown for what it is: an image to fit, zoom and
//! pan, as Zed's image viewer does; a PDF's pages; text in a read-only editor that can be
//! searched; anything else as a hex dump, as [`value_kind`] judges it for the peek too. Read
//! whole at the commit it was opened at (one opened at the branch head, at the head it found
//! then), up to [`VIEW_MAX`]; a larger value, or one the tab cannot show, can still be saved or
//! opened in the system's own application.

use std::path::PathBuf;
use std::sync::Arc;

use editor::Editor;
use gpui::{
    App, AppContext as _, Bounds, Context, Entity, EventEmitter, ExternalPaths, FocusHandle,
    Focusable, ImageFormat, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    ParentElement, Pixels, Point, Render, ScrollDelta, ScrollWheelEvent, SharedString, Styled,
    Subscription, SvgRenderer, Task, Window, canvas, checkerboard, div, img, point,
};
use gq_language::{BuiltQuery, builder};
use graph_client::{BlobCell, BlobMeta};
use graph_store::{GraphSession, Loadable, SessionEvent};
use serde_json::json;
use studio_actions::blob::{ChooseFile, ClearValue};
use studio_actions::canvas::{ActualSize, FitToWindow, ZoomIn, ZoomOut};
use studio_ui::ds;
use studio_workspace::{Item, ItemEvent, ItemStatus, KeyHint, SavedItem};
use ui::Tooltip;
use ui::prelude::*;

use crate::blob::{
    KIND_PREFIX, ShownImage, ValueKind, hex_dump_lines, read_value, save_folder, save_value,
    suggested_file_name, value_kind,
};
use crate::blob_write::{self, Binding, BlobChange, BlobEdit, BlobWritten};
use crate::model::{Notice, Tone};
use crate::pdf;

/// The largest value the tab reads whole to show.
pub const VIEW_MAX: u64 = 64 * 1024 * 1024;
/// How many pages of a PDF are drawn: a letter page drawn 1,600 pixels wide holds about 13 MB
/// of pixels while the tab is open, so a hundred would hold over a gigabyte.
const PDF_PAGES: usize = 20;
/// How wide a PDF page is drawn, in pixels: twice the width it is shown at, for a sharp page.
const PDF_DRAW_WIDTH: u32 = 1600;
/// How much of a binary value the hex dump shows.
const HEX_MAX: usize = 64 * 1024;
/// The first bytes of a value kept to say what it is, which Save and Open name the file by:
/// those [`value_kind`] judges, as the peek names it.
const HEAD_BYTES: u64 = KIND_PREFIX as u64;
/// How much of a text value the tab shows: an editor of a whole 64 MiB value would take seconds
/// to make on the main thread.
const TEXT_SHOWN: usize = 4 * 1024 * 1024;
const ZOOM_STEP: f32 = 1.25;
const ZOOM_MIN: f32 = 0.05;
const ZOOM_MAX: f32 = 20.;
/// Room kept around a fitted image.
const FIT_MARGIN: f32 = studio_ui::ds::space::XXL;

/// What the value turned out to be, ready to show.
enum Content {
    Image {
        image: ShownImage,
        /// Its pixel size; none for an SVG, which scales freely.
        size: Option<(u32, u32)>,
        format: &'static str,
    },
    Pdf {
        pages: Vec<(ShownImage, u32, u32)>,
        count: usize,
    },
    /// The text, and whether it is cut at [`TEXT_SHOWN`].
    Text(Entity<Editor>, bool),
    /// The hex dump, in a read-only editor as text is: it lays out only the lines in view.
    Bytes(Entity<Editor>),
    TooLarge,
    External(String),
    Null,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Zoom {
    Fit,
    Scale(f32),
}

/// Whether the session's branch list answers for the tab's branch, which Choose File and Clear
/// follow ([`BlobView::branch_refusal`]). A list read before the branch was made, by Studio a
/// moment ago or elsewhere, does not hold it: one that lacks it is read again before the tab
/// says the branch is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BranchListed {
    /// The list held may be older than the branch.
    Unseen,
    /// The list held lacks the branch, and the tab asked for it again: until a list comes in,
    /// the server decides.
    Asked,
    /// A list taken since the tab asked, or one holding the branch: a later one that lacks it
    /// was read after the branch was gone.
    Answered,
}

/// Saving the value, or opening it elsewhere.
enum Export {
    Busy(&'static str),
    Saved(PathBuf),
    Failed(String),
}

pub struct BlobView {
    session: Entity<GraphSession>,
    cell: BlobCell,
    /// The commit the tab was opened at; `None` when it was opened at the branch head, whose
    /// commit it reads first ([`Self::opened_at`]).
    snapshot: Option<String>,
    /// The tab holds `snapshot`, read-only (opened from a view held at a commit). Otherwise it
    /// reads the value there and writes on its branch, bound to the value it read.
    held: bool,
    /// The branch the tab read the value on, which a file chosen or dropped is stored on, and
    /// only there: it is saved with the tab and comes back with it ([`Self::on_branch`]).
    branch: String,
    /// `branch` was given rather than taken from the session: a tab that comes back on the
    /// branch it was saved on reads that branch's head, not the one the window shows.
    branch_given: bool,
    branch_listed: BranchListed,
    /// For a tab opened at the branch head, the commit the head was when it first read: the
    /// value is read there, every range of it and a save, so nothing that lands on the branch
    /// meanwhile, and no other branch the window moves to, comes into what it shows or saves.
    opened_at: Option<String>,
    /// The commit of the tab's own last write, which it reads the value at since: the head it
    /// made, whichever branch the window shows now.
    own_commit: Option<String>,
    /// What the cell holds as the tab read it, with its ETag, which a write is bound to.
    meta: Option<BlobMeta>,
    /// A file being stored or a clear, while out, or why it did not land.
    edit: Option<BlobEdit>,
    size: Option<u64>,
    content: Loadable<Content>,
    /// Bytes read so far while reading.
    progress: u64,
    /// The value's first kilobyte, which names a saved file; the rest is let go once shown, and
    /// a save reads it again range by range.
    head: Vec<u8>,
    zoom: Zoom,
    pan: Point<Pixels>,
    drag: Option<Point<Pixels>>,
    bounds: Option<Bounds<Pixels>>,
    export: Option<Export>,
    focus_handle: FocusHandle,
    /// The read, started when the tab is first drawn: tabs restored with the workspace read
    /// nothing until they are shown.
    _load: Option<Task<()>>,
    /// The read is asked for by a draw, and starts once the draw is done ([`Self::start_load`]):
    /// it asks the session, which a draw does not change.
    load_asked: bool,
    _export: Option<Task<()>>,
    /// Draws the tab again when what the credential may do is read, or the branch list, which
    /// Choose File and Clear follow, and when the schema is, which a tab opened at the head
    /// reads the head with.
    _session: Subscription,
}

impl BlobView {
    pub fn new(
        session: Entity<GraphSession>,
        cell: BlobCell,
        snapshot: Option<String>,
        held: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.on_release(|this: &mut Self, cx| this.release_images(cx))
            .detach();
        let branch = session.read(cx).branch.clone();
        let events = cx.subscribe(&session, |this, _, event: &SessionEvent, cx| match event {
            SessionEvent::BranchesChanged => {
                if this.branch_listed == BranchListed::Asked {
                    this.branch_listed = BranchListed::Answered;
                }
                this.follow_branch_list(cx);
                cx.notify();
            }
            SessionEvent::AccessChanged | SessionEvent::SchemaChanged => cx.notify(),
            _ => {}
        });
        Self {
            session,
            cell,
            held: held && snapshot.is_some(),
            snapshot,
            branch,
            branch_given: false,
            branch_listed: BranchListed::Unseen,
            opened_at: None,
            own_commit: None,
            meta: None,
            edit: None,
            size: None,
            content: Loadable::Loading,
            progress: 0,
            head: Vec::new(),
            zoom: Zoom::Fit,
            pan: Point::default(),
            drag: None,
            bounds: None,
            export: None,
            focus_handle: cx.focus_handle(),
            _load: None,
            load_asked: false,
            _export: None,
            _session: events,
        }
    }

    /// The tab on `branch` rather than the session's: one that comes back after a restart
    /// writes to the branch it was saved on, whichever the window shows, and reads that
    /// branch's head when it had not read one yet.
    pub fn on_branch(mut self, branch: String) -> Self {
        self.branch = branch;
        self.branch_given = true;
        self
    }

    /// One tab per value held at a commit, and per value and the branch it writes to, at that
    /// branch's head or at the commit a view at the head read it at: a tab never writes to
    /// another branch than the one it was opened for.
    pub fn key(cell: &BlobCell, snapshot: Option<&str>, held: bool, branch: &str) -> String {
        let at = match (snapshot, held) {
            (None, _) => format!("head:{branch}"),
            (Some(commit), true) => commit.to_owned(),
            (Some(commit), false) => format!("head@{commit}:{branch}"),
        };
        format!(
            "blob:{}:{}:{}:{}:{at}",
            if cell.edge { "edge" } else { "node" },
            cell.type_name,
            cell.id,
            cell.property,
        )
    }

    /// The commit the tab holds, read-only.
    fn held_commit(&self) -> Option<&str> {
        self.snapshot.as_deref().filter(|_| self.held)
    }

    /// Reads what the cell holds, then the value, then what it is, all at `at`; with no `at`,
    /// at the commit `head` (the head probe, read on `branch`) finds the branch's head is now,
    /// which the tab holds to since ([`Self::opened_at`]).
    fn load(
        session: &Entity<GraphSession>,
        cell: &BlobCell,
        branch: &str,
        at: Option<String>,
        head: Option<BuiltQuery>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let head = match (&at, head) {
            (None, Some(probe)) => {
                Some(session.update(cx, |session, cx| session.read_on(branch, &probe, cx)))
            }
            _ => None,
        };
        let (session, cell) = (session.downgrade(), cell.clone());
        cx.spawn_in(window, async move |this, cx| {
            let fail = |error: String, cx: &mut gpui::AsyncWindowContext| {
                this.update(cx, |this, cx| {
                    this.content = Loadable::Failed(error);
                    cx.notify();
                })
                .ok();
            };
            let at = match (at, head) {
                (Some(at), _) => at,
                (None, Some(head)) => match head.await {
                    Ok(read) => match read.graph_commit_id.clone() {
                        Some(commit) => {
                            this.update(cx, |this, _| this.opened_at = Some(commit.clone()))
                                .ok();
                            commit
                        }
                        None => {
                            return fail(
                                "The server did not say which commit the branch is at, so the \
                                 value was not read."
                                    .into(),
                                cx,
                            );
                        }
                    },
                    Err(error) => return fail(error.to_string(), cx),
                },
                (None, None) => {
                    return fail(
                        "The schema has no node type to read the branch's head with.".into(),
                        cx,
                    );
                }
            };
            let snapshot = Some(at);
            let meta = session.update(cx, |session, cx| {
                session.blob_meta(cell.clone(), snapshot.clone(), cx)
            });
            let meta = match meta {
                Ok(meta) => meta.await,
                Err(_) => return,
            };
            let meta = match meta {
                Ok(meta) => meta,
                Err(error) => return fail(error.to_string(), cx),
            };
            let size = match &meta {
                BlobMeta::Managed { size, .. } => *size,
                BlobMeta::External { uri } => {
                    let uri = uri.clone();
                    this.update(cx, |this, cx| {
                        this.meta = Some(meta);
                        this.content = Loadable::Loaded(Content::External(uri));
                        cx.notify();
                    })
                    .ok();
                    return;
                }
                BlobMeta::Null => {
                    this.update(cx, |this, cx| {
                        this.meta = Some(meta);
                        this.content = Loadable::Loaded(Content::Null);
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            this.update(cx, |this, cx| {
                this.meta = Some(meta);
                this.size = Some(size);
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();
            })
            .ok();
            if size > VIEW_MAX {
                // Too large to show, it is still saved and opened under a name that says what
                // it is: its first bytes alone are read.
                let head = session.update(cx, |session, cx| {
                    session.blob_bytes(cell.clone(), snapshot.clone(), 0..HEAD_BYTES, cx)
                });
                let head = match head {
                    Ok(read) => read.await.unwrap_or_default(),
                    Err(_) => return,
                };
                this.update(cx, |this, cx| {
                    this.head = head;
                    this.content = Loadable::Loaded(Content::TooLarge);
                    cx.notify();
                })
                .ok();
                return;
            }
            let progress_view = this.clone();
            let bytes = match read_value(session, cell, snapshot, size, cx, |done, cx| {
                progress_view
                    .update(cx, |this, cx| {
                        this.progress = done;
                        cx.notify();
                    })
                    .ok();
            })
            .await
            {
                Ok(bytes) => Arc::new(bytes),
                Err(error) => return fail(error, cx),
            };
            let Ok(svg) = cx.update(|_, cx| cx.svg_renderer()) else {
                return;
            };
            let kind = {
                let bytes = bytes.clone();
                cx.background_spawn(async move { classify(&bytes, &svg) })
                    .await
            };
            this.update_in(cx, |this, window, cx| {
                this.release_images(cx);
                this.head = bytes[..bytes.len().min(HEAD_BYTES as usize)].to_vec();
                this.content = Loadable::Loaded(match kind {
                    Kind::Image { image, size, name } => Content::Image {
                        image,
                        size,
                        format: name,
                    },
                    Kind::Pdf { pages, count } => Content::Pdf { pages, count },
                    Kind::Text(text, cut) => Content::Text(read_only(text, window, cx), cut),
                    Kind::Bytes(hex) => Content::Bytes(read_only(hex, window, cx)),
                    Kind::Unreadable(reason) => {
                        this.content = Loadable::Failed(reason);
                        cx.notify();
                        return;
                    }
                });
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();
            })
            .ok();
        })
    }

    // ---- zoom and pan, after Zed's image viewer ----

    fn image_size(&self) -> Option<(u32, u32)> {
        match &self.content {
            Loadable::Loaded(Content::Image { size, .. }) => *size,
            _ => None,
        }
    }

    /// The scale that fits the image in the tab, never above its own size.
    fn fit_scale(&self) -> f32 {
        let (Some(bounds), Some((width, height))) = (self.bounds, self.image_size()) else {
            return 1.;
        };
        let room_w = (f32::from(bounds.size.width) - 2. * FIT_MARGIN).max(1.);
        let room_h = (f32::from(bounds.size.height) - 2. * FIT_MARGIN).max(1.);
        (room_w / width as f32).min(room_h / height as f32).min(1.)
    }

    fn scale(&self) -> f32 {
        match self.zoom {
            Zoom::Fit => self.fit_scale(),
            Zoom::Scale(scale) => scale,
        }
    }

    /// Zooms to `scale`, keeping the point under `at` where it is.
    fn zoom_to(&mut self, scale: f32, at: Option<Point<Pixels>>, cx: &mut Context<Self>) {
        let old = self.scale();
        let new = scale.clamp(ZOOM_MIN, ZOOM_MAX);
        if let (Some(at), Some(bounds)) = (at, self.bounds) {
            let centre = point(
                at.x - bounds.origin.x - bounds.size.width / 2.,
                at.y - bounds.origin.y - bounds.size.height / 2.,
            );
            let from_image = centre - self.pan;
            self.pan += from_image * (1. - new / old);
        }
        self.zoom = Zoom::Scale(new);
        cx.notify();
    }

    /// The scale shown: an image's, or a PDF page's against the width it is drawn for.
    fn shown_scale(&self) -> f32 {
        match &self.content {
            Loadable::Loaded(Content::Pdf { .. }) => self.pdf_width() / (PDF_DRAW_WIDTH / 2) as f32,
            _ => self.scale(),
        }
    }

    fn zoom_in(&mut self, _: &ZoomIn, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_to(self.shown_scale() * ZOOM_STEP, None, cx);
    }

    fn zoom_out(&mut self, _: &ZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_to(self.shown_scale() / ZOOM_STEP, None, cx);
    }

    fn actual_size(&mut self, _: &ActualSize, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom = Zoom::Scale(1.);
        self.pan = Point::default();
        cx.notify();
    }

    fn fit(&mut self, _: &FitToWindow, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom = Zoom::Fit;
        self.pan = Point::default();
        cx.notify();
    }

    fn scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let delta = match event.delta {
            ScrollDelta::Pixels(pixels) => pixels,
            ScrollDelta::Lines(lines) => point(px(lines.x * 20.), px(lines.y * 20.)),
        };
        if event.modifiers.platform || event.modifiers.control {
            let factor = (1. + f32::from(delta.y) / 200.).clamp(0.5, 2.);
            self.zoom_to(self.scale() * factor, Some(event.position), cx);
        } else {
            self.pan += delta;
            cx.notify();
        }
    }

    // ---- save and open elsewhere ----

    fn file_name(&self) -> String {
        suggested_file_name(&self.cell.id, &self.cell.property, &self.head)
    }

    /// Writes the value to `path`, range by range: what was shown is not kept whole.
    fn write_to(&mut self, path: PathBuf, then_open: bool, cx: &mut Context<Self>) {
        let (Some(size), session) = (self.size, self.session.downgrade()) else {
            return;
        };
        let (cell, snapshot) = (self.cell.clone(), self.reading_at());
        self.export = Some(Export::Busy(if then_open {
            "Opening…"
        } else {
            "Saving…"
        }));
        self._export = Some(cx.spawn(async move |this, cx| {
            let written = save_value(session, cell, snapshot, size, path, cx, |_, _| {}).await;
            this.update(cx, |this, cx| {
                this.export = Some(match written {
                    Ok(path) if then_open => {
                        cx.open_with_system(&path);
                        this.export = None;
                        cx.notify();
                        return;
                    }
                    Ok(path) => Export::Saved(path),
                    Err(error) => Export::Failed(error),
                });
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_new_path(&save_folder(), Some(&self.file_name()));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = chosen.await else {
                return;
            };
            this.update(cx, |this, cx| this.write_to(path, false, cx))
                .ok();
        })
        .detach();
    }

    /// Hands the value to the system's own application for its kind, through an owner-only
    /// folder in Studio's data directory that keeps a day's worth.
    fn open_elsewhere(&mut self, cx: &mut Context<Self>) {
        let directory = studio_settings::data_dir().join("blobs");
        let path = directory.join(self.file_name());
        self.export = Some(Export::Busy("Opening…"));
        cx.notify();
        // The folder is made private and emptied of day-old files off the main thread.
        self._export = Some(cx.spawn(async move |this, cx| {
            let prepared = cx
                .background_spawn(async move { prepare_open_folder(&directory) })
                .await;
            this.update(cx, |this, cx| match prepared {
                Ok(()) => this.write_to(path, true, cx),
                Err(error) => {
                    this.export = Some(Export::Failed(format!("Could not write: {error}")));
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    // ---- a file stored in the value, or the value cleared ----

    /// The commit the value is read at: the tab's own write's, else the one it was opened at,
    /// or the head it found when opened at the head; `None` only before that head is read.
    fn reading_at(&self) -> Option<String> {
        self.own_commit
            .clone()
            .or_else(|| self.snapshot.clone())
            .or_else(|| self.opened_at.clone())
    }

    /// What a write is bound to now: the value the tab shows, read where it reads the value.
    /// `None` while it has not read what the cell holds.
    fn binding(&self) -> Option<Binding> {
        Some(Binding {
            shown: self.meta.clone()?,
            read_at: self.reading_at(),
        })
    }

    /// Follows the session's branch list for the tab's branch, which writes there: a list that
    /// holds it answers for it, and one that lacks it is read again, once, before it answers.
    fn follow_branch_list(&mut self, cx: &mut Context<Self>) {
        if self.held {
            return;
        }
        let Loadable::Loaded(branches) = &self.session.read(cx).branches else {
            return;
        };
        if branches.contains(&self.branch) {
            self.branch_listed = BranchListed::Answered;
        } else if self.branch_listed == BranchListed::Unseen {
            self.branch_listed = BranchListed::Asked;
            self.session
                .update(cx, |session, cx| session.reload_branches(cx));
        }
    }

    /// Why nothing can be written on the tab's branch, or `None` while it can or the server
    /// decides: a branch list that answers for it ([`BranchListed::Answered`]) does not hold it
    /// (deleted since the tab was saved, or elsewhere since it opened), or the credential may
    /// not change it.
    fn branch_refusal(&self, cx: &App) -> Option<String> {
        if self.branch_listed == BranchListed::Answered
            && let Loadable::Loaded(branches) = &self.session.read(cx).branches
            && !branches.contains(&self.branch)
        {
            return Some(format!(
                "{} no longer exists, so nothing is written here. Open the value from a \
                 table on another branch to change it.",
                self.branch
            ));
        }
        crate::write::change_refusal(&self.session, &self.branch, cx)
    }

    /// Why the value cannot take a file here, or `None` when it can.
    fn write_refusal(&self, cx: &App) -> Option<String> {
        let schema = self.session.read(cx).schema_model();
        let denied = self.branch_refusal(cx);
        blob_write::write_refusal(
            schema.as_deref(),
            &self.cell,
            self.held_commit(),
            denied.as_deref(),
        )
    }

    /// Why the value cannot be cleared here, or `None` when it can.
    fn clear_refusal(&self, cx: &App) -> Option<String> {
        let schema = self.session.read(cx).schema_model();
        let denied = self.branch_refusal(cx);
        blob_write::clear_refusal(
            schema.as_deref(),
            &self.cell,
            self.held_commit(),
            denied.as_deref(),
            self.meta.as_ref(),
        )
    }

    fn saving(&self) -> bool {
        self.edit.as_ref().is_some_and(BlobEdit::is_saving)
    }

    /// Choose File…: the system's picker, then the file chosen stored in the value.
    fn choose_file(&mut self, cx: &mut Context<Self>) {
        let chosen = blob_write::choose_file(cx);
        cx.spawn(async move |this, cx| {
            if let Some(path) = chosen.await {
                this.update(cx, |this, cx| this.upload(path, cx)).ok();
            }
        })
        .detach();
    }

    /// A drop from the file manager: its one file stored in the value.
    fn drop_files(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        match blob_write::dropped_file(paths) {
            Ok(path) => self.upload(path, cx),
            Err(notice) => {
                self.edit = Some(BlobEdit::Failed {
                    notice,
                    again: None,
                });
                cx.notify();
            }
        }
    }

    /// Stores the file at `path` in the value on the tab's branch, bound to the value it shows.
    pub(crate) fn upload(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.send(BlobChange::Upload(path), None, cx);
    }

    /// Clear: the value set to null, bound to the value the tab shows.
    pub(crate) fn clear(&mut self, cx: &mut Context<Self>) {
        self.send(BlobChange::Clear, None, cx);
    }

    /// The change a conflict offers again, bound to what the cell held at the head.
    fn send_again(&mut self, cx: &mut Context<Self>) {
        if let Some(BlobEdit::Failed {
            again: Some((change, now)),
            ..
        }) = self.edit.clone()
        {
            self.send(change, Some(now), cx);
        }
    }

    /// Sends `change`, bound to `over` (what a conflict found at the head) or else to the value
    /// shown, read at the commit the tab reads. Detached: closing the tab never cancels it.
    fn send(&mut self, change: BlobChange, over: Option<BlobMeta>, cx: &mut Context<Self>) {
        if self.saving() {
            return;
        }
        let refusal = match &change {
            BlobChange::Clear if over.is_none() => self.clear_refusal(cx),
            _ => self.write_refusal(cx),
        };
        let shown = over.clone().or_else(|| self.meta.clone());
        let (Some(shown), None) = (shown, refusal.clone()) else {
            let reason = refusal.unwrap_or_else(|| {
                format!(
                    "Nothing was written: Studio has not read what {} holds yet.",
                    self.cell.property
                )
            });
            self.edit = Some(BlobEdit::Failed {
                notice: Notice::new(Tone::Invalid, reason),
                again: None,
            });
            cx.notify();
            return;
        };
        let read_at = if over.is_some() {
            None
        } else {
            self.reading_at()
        };
        let (cell, branch) = (self.cell.clone(), self.branch.clone());
        let write = match &change {
            BlobChange::Upload(path) => {
                // Bound to what the tab shows as the write goes, after the file is read, where
                // the tab reads the value then. After a conflict it stays bound to what the head
                // held then; with the tab closed, to what it showed when the file was chosen.
                let tab = cx.entity().downgrade();
                let again = over.is_some();
                let chose = Binding { shown, read_at };
                let binding = move |cx: &mut App| {
                    tab.upgrade()
                        .filter(|_| !again)
                        .and_then(|tab| tab.read(cx).binding())
                        .unwrap_or(chose)
                };
                blob_write::upload_bound(&self.session, &branch, cell, binding, path.clone(), cx)
            }
            BlobChange::Clear => {
                blob_write::clear(&self.session, &branch, cell, shown, read_at, cx)
            }
        };
        self.edit = Some(BlobEdit::Saving(change.clone()));
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let written = write.await;
            this.update(cx, |this, cx| this.written(change, written, cx))
                .ok();
        })
        .detach();
    }

    /// Lands a write: a stored value is read again at its commit, which the tab reads at since;
    /// one that did not land says why.
    fn written(&mut self, change: BlobChange, written: BlobWritten, cx: &mut Context<Self>) {
        let saved = match &written {
            BlobWritten::Saved { commit, meta, .. } => Some((commit.clone(), meta.clone())),
            _ => None,
        };
        self.edit = BlobEdit::ended(written, change, &self.cell.property);
        if let Some((commit, meta)) = saved {
            self.release_images(cx);
            self.export = None;
            self.head.clear();
            self.progress = 0;
            match commit {
                Some(commit) => {
                    self.own_commit = Some(commit);
                    self.meta = None;
                    self.size = None;
                    self.content = Loadable::Loading;
                    self._load = None;
                }
                // Cleared already: nothing was published.
                None => {
                    self.meta = Some(meta);
                    self.size = None;
                    self.content = Loadable::Loaded(Content::Null);
                }
            }
        }
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// The status bar's part for writing the value: a write out, or why the last did not land,
    /// then Choose File… and Clear, each disabled with its reason where the value cannot change.
    fn write_status(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut parts = Vec::new();
        match &self.edit {
            Some(BlobEdit::Saving(_)) => parts.push(
                h_flex()
                    .gap(px(ds::space::SM))
                    .child(ui::Indicator::dot().color(Color::Warning))
                    .child(ds::text::meta(format!("Saving to {}…", self.branch)))
                    .into_any_element(),
            ),
            Some(BlobEdit::Failed { notice, again }) => {
                let again = again.as_ref().map(|(change, now)| {
                    ds::button("blob-write-again", blob_write::again_label(change, now))
                        .style(ButtonStyle::Subtle)
                        .tooltip(Tooltip::text(format!(
                            "Write it over what {} holds now, on {}",
                            self.cell.property, self.branch
                        )))
                        .on_click(cx.listener(|this, _, _, cx| this.send_again(cx)))
                        .into_any_element()
                });
                parts.push(blob_write::notice_row("blob-write", notice, again).into_any_element());
            }
            None => {}
        }
        let saving = self.saving();
        let (cell, branch) = (self.cell.clone(), self.branch.clone());
        let tooltip = move |refusal: Option<String>, clear: bool| {
            let (cell, branch) = (cell.clone(), branch.clone());
            move |_: &mut Window, cx: &mut App| match &refusal {
                Some(reason) => Tooltip::simple(reason.clone(), cx),
                None if clear => blob_write::clear_tooltip(&cell, &branch, cx),
                None => blob_write::upload_tooltip(&cell, &branch, cx),
            }
        };
        let upload_refusal = self.write_refusal(cx);
        let clear_refusal = self.clear_refusal(cx);
        parts.push(
            ds::button("choose-blob-file", "Choose File…")
                .style(ButtonStyle::Subtle)
                .color(Color::Muted)
                .disabled(upload_refusal.is_some() || saving)
                .tooltip(tooltip(upload_refusal, false))
                .on_click(cx.listener(|this, _, _, cx| this.choose_file(cx)))
                .into_any_element(),
        );
        parts.push(
            ds::button("clear-blob", "Clear")
                .style(ButtonStyle::Subtle)
                .color(Color::Muted)
                .disabled(clear_refusal.is_some() || saving)
                .tooltip(tooltip(clear_refusal, true))
                .on_click(cx.listener(|this, _, _, cx| this.clear(cx)))
                .into_any_element(),
        );
        parts
    }

    // ---- what the tab says ----

    fn facts(&self) -> String {
        let size = self.size.map(studio_ui::format_bytes);
        let mut facts: Vec<String> = Vec::new();
        match &self.content {
            Loadable::Loaded(Content::Image {
                size: dims, format, ..
            }) => {
                facts.push((*format).to_owned());
                if let Some((w, h)) = dims {
                    facts.push(format!("{w} × {h}"));
                }
            }
            Loadable::Loaded(Content::Pdf { count, .. }) => {
                facts.push("PDF".into());
                facts.push(match count {
                    1 => "1 page".into(),
                    n => format!("{n} pages"),
                });
            }
            Loadable::Loaded(Content::Text(_, cut)) => {
                facts.push("Text".into());
                if *cut {
                    let shown = studio_ui::format_bytes(TEXT_SHOWN as u64);
                    facts.push(format!("the first {shown} shown"));
                }
            }
            Loadable::Loaded(Content::Bytes { .. }) => facts.push("Binary".into()),
            _ => {}
        }
        facts.extend(size);
        if let Some(commit) = self.reading_at() {
            facts.push(format!("at {}", studio_ui::short_commit_id(&commit)));
        }
        facts.join(" · ")
    }

    /// Frees the images this tab decoded; GPUI would keep them otherwise.
    fn release_images(&self, cx: &mut App) {
        match &self.content {
            Loadable::Loaded(Content::Image { image, .. }) => image.release(cx),
            Loadable::Loaded(Content::Pdf { pages, .. }) => {
                for (image, _, _) in pages {
                    image.release(cx);
                }
            }
            _ => {}
        }
    }

    fn render_image(
        &self,
        image: &ShownImage,
        size: Option<(u32, u32)>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity();
        // The tab's bounds, for fitting; read where the image is laid out.
        let measure = canvas(
            move |bounds, _, cx| {
                this.update(cx, |view, cx| {
                    if view.bounds != Some(bounds) {
                        view.bounds = Some(bounds);
                        cx.notify();
                    }
                })
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        let frame = match (size, self.bounds) {
            (Some((width, height)), Some(bounds)) => {
                let scale = self.scale();
                let (w, h) = (px(width as f32 * scale), px(height as f32 * scale));
                let left = (bounds.size.width - w) / 2. + self.pan.x;
                let top = (bounds.size.height - h) / 2. + self.pan.y;
                div()
                    .absolute()
                    .left(left)
                    .top(top)
                    .w(w)
                    .h(h)
                    // Transparent pixels show a checkerboard, as Zed's viewer does.
                    .bg(checkerboard(
                        ds::surface::chrome(cx),
                        ds::space::MD * scale.max(0.5),
                    ))
                    .child(img(image.source()).size_full())
                    // On the image's edge, over it: a border would sit outside it.
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .border_1()
                            .border_color(ds::surface::image_outline(cx)),
                    )
                    .into_any_element()
            }
            // An SVG, or before the tab is measured: fitted by the element.
            _ => div()
                .absolute()
                .inset_0()
                .p(px(FIT_MARGIN))
                .child(
                    img(image.source())
                        .size_full()
                        .object_fit(gpui::ObjectFit::Contain),
                )
                .into_any_element(),
        };
        div()
            .id("blob-image")
            .relative()
            .size_full()
            .overflow_hidden()
            .cursor_grab()
            .child(measure)
            .child(frame)
            .on_scroll_wheel(cx.listener(Self::scroll))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    this.drag = Some(event.position);
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if let Some(from) = this.drag {
                    if event.pressed_button == Some(MouseButton::Left) {
                        this.pan += event.position - from;
                        this.drag = Some(event.position);
                        cx.notify();
                    } else {
                        this.drag = None;
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.drag = None;
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    /// How wide a PDF page is shown: pages are drawn twice that wide; Fit shows them at that
    /// width or the tab's, whichever is narrower.
    fn pdf_width(&self) -> f32 {
        let natural = (PDF_DRAW_WIDTH / 2) as f32;
        let room = self
            .bounds
            .map(|bounds| f32::from(bounds.size.width) - 2. * FIT_MARGIN)
            .unwrap_or(natural);
        match self.zoom {
            Zoom::Fit => natural.min(room),
            Zoom::Scale(scale) => natural * scale,
        }
    }

    fn render_pdf(&self, pages: &[(ShownImage, u32, u32)], count: usize, cx: &App) -> AnyElement {
        let width = self.pdf_width();
        v_flex()
            .id("blob-pdf")
            .size_full()
            .overflow_scroll()
            .items_center()
            .py(px(FIT_MARGIN))
            .gap(px(ds::space::XL))
            .children(pages.iter().map(|(image, w, h)| {
                let height = width * *h as f32 / *w as f32;
                div()
                    .flex_none()
                    .w(px(width))
                    .h(px(height))
                    .bg(ds::surface::paper(cx))
                    .border_1()
                    .border_color(ds::surface::rule(cx))
                    .shadow(ds::elevation::raised(cx))
                    .child(img(image.source()).size_full())
            }))
            .when(count > pages.len(), |list| {
                list.child(ds::text::meta(format!(
                    "Showing the first {} of {count} pages; open it in the default app for \
                     the rest.",
                    pages.len()
                )))
            })
            .into_any_element()
    }
}

/// The folder values are handed to other applications from: owner-only, emptied of anything a
/// day old.
fn prepare_open_folder(directory: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    }
    let day = std::time::Duration::from_secs(24 * 60 * 60);
    for entry in std::fs::read_dir(directory)?.flatten() {
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > day);
        if old {
            std::fs::remove_file(entry.path()).ok();
        }
    }
    Ok(())
}

/// What a value is, worked out off the main thread.
enum Kind {
    Image {
        image: ShownImage,
        size: Option<(u32, u32)>,
        name: &'static str,
    },
    Pdf {
        pages: Vec<(ShownImage, u32, u32)>,
        count: usize,
    },
    /// The text the value starts with ([`text_shown`]), and whether some of the value is not.
    Text(String, bool),
    Bytes(String),
    Unreadable(String),
}

/// A read-only editor of `text`, which lays out only the lines in view.
fn read_only(text: String, window: &mut Window, cx: &mut App) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::multi_line(window, cx);
        editor.set_text(text, window, cx);
        editor.set_read_only(true);
        editor
    })
}

/// What a value is ([`value_kind`], as the peek and a saved file's name judge it), with its
/// images decoded and its pages drawn; call it from a background task.
fn classify(bytes: &[u8], svg: &SvgRenderer) -> Kind {
    match value_kind(bytes) {
        ValueKind::Image(format) => {
            let size = image::ImageReader::new(std::io::Cursor::new(bytes))
                .with_guessed_format()
                .ok()
                .and_then(|reader| reader.into_dimensions().ok());
            let name = match format {
                ImageFormat::Png => "PNG",
                ImageFormat::Jpeg => "JPEG",
                ImageFormat::Gif => "GIF",
                ImageFormat::Webp => "WebP",
                _ => "Image",
            };
            match ShownImage::decode(format, bytes.to_vec(), svg) {
                Some(image) => Kind::Image { image, size, name },
                None => Kind::Unreadable("The image could not be decoded.".into()),
            }
        }
        ValueKind::Pdf => match pdf::render(bytes, PDF_DRAW_WIDTH, PDF_PAGES) {
            Ok((pages, count)) => Kind::Pdf {
                pages: pages
                    .into_iter()
                    .filter_map(|page| {
                        let image = ShownImage::from_rgba(page.width, page.height, page.rgba)?;
                        Some((image, page.width, page.height))
                    })
                    .collect(),
                count,
            },
            Err(reason) => Kind::Unreadable(reason),
        },
        // An SVG that does not parse is shown as the text it is.
        ValueKind::Svg => match ShownImage::decode(ImageFormat::Svg, bytes.to_vec(), svg) {
            Some(image) => Kind::Image {
                image,
                size: None,
                name: "SVG",
            },
            None => text_shown(bytes),
        },
        ValueKind::Text => text_shown(bytes),
        ValueKind::Binary => Kind::Bytes(hex_dump_lines(&bytes[..bytes.len().min(HEX_MAX)], 16)),
    }
}

/// The text a value starts with: at most [`TEXT_SHOWN`] bytes of it, cut on a character's
/// boundary and before the first byte that is no text (a NUL, or one no character is made of),
/// and whether that leaves some of the value out.
fn text_shown(bytes: &[u8]) -> Kind {
    let window = &bytes[..bytes.len().min(TEXT_SHOWN)];
    let valid = match std::str::from_utf8(window) {
        Ok(text) => text,
        Err(error) => std::str::from_utf8(&window[..error.valid_up_to()]).unwrap_or_default(),
    };
    let text = valid.split('\0').next().unwrap_or_default();
    Kind::Text(text.to_owned(), text.len() < bytes.len())
}

impl EventEmitter<ItemEvent> for BlobView {}

impl Focusable for BlobView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for BlobView {
    fn tab_title(&self, _cx: &App) -> SharedString {
        format!("{} · {}", self.cell.property, self.cell.id).into()
    }

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(match &self.content {
            Loadable::Loaded(Content::Image { .. }) => IconName::Image,
            Loadable::Loaded(Content::Pdf { .. }) => IconName::FileDoc,
            Loadable::Loaded(Content::Text(..)) => IconName::FileTextOutlined,
            _ => IconName::File,
        })
    }

    fn dedupe_key(&self, _cx: &App) -> Option<SharedString> {
        Some(
            Self::key(
                &self.cell,
                self.snapshot.as_deref(),
                self.held,
                &self.branch,
            )
            .into(),
        )
    }

    /// The tab's dot shows while a file is being stored or the value cleared.
    fn is_dirty(&self, _cx: &App) -> bool {
        self.saving()
    }

    fn serialize(&self, _cx: &App) -> Option<SavedItem> {
        Some(SavedItem {
            kind: "blob".into(),
            data: json!({
                "type_name": self.cell.type_name,
                "id": self.cell.id,
                "property": self.cell.property,
                "edge": self.cell.edge,
                // Where it reads now, past its own writes, so it comes back showing that value
                // and binds a write to it, not to the one its write replaced.
                "at_commit": self.reading_at(),
                "held": self.held,
                // And the branch that commit is read for, which it writes to: it comes back
                // there, not on the branch the window shows then.
                "branch": self.branch,
            }),
        })
    }

    /// The value's kind, size and commit; zoom for an image or a PDF; Save and Open with.
    fn status(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Option<ItemStatus> {
        let mut start = vec![ds::text::meta(self.facts()).into_any_element()];
        if let Loadable::Loading = self.content
            && let Some(size) = self.size.filter(|size| *size > 0)
        {
            start.push(
                ds::text::meta(format!("Reading {}%", self.progress * 100 / size))
                    .into_any_element(),
            );
        }
        let mut end = Vec::new();
        let zoomable = matches!(
            self.content,
            Loadable::Loaded(Content::Image { size: Some(_), .. } | Content::Pdf { .. })
        );
        if zoomable {
            let scale = self.shown_scale();
            let percent = format!("{}%", (scale * 100.).round());
            let focus = self.focus_handle.clone();
            end.push(
                h_flex()
                    .gap(px(ds::space::XXS))
                    .items_center()
                    .child(
                        ds::icon_button("zoom-out", IconName::Dash)
                            .icon_color(Color::Muted)
                            .tooltip({
                                let focus = focus.clone();
                                move |_, cx| {
                                    Tooltip::for_action_in("Zoom out", &ZoomOut, &focus, cx)
                                }
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.zoom_out(&ZoomOut, window, cx)
                            })),
                    )
                    .child(
                        ds::button("zoom-actual", percent)
                            .style(ButtonStyle::Subtle)
                            .color(Color::Muted)
                            .tooltip({
                                let focus = focus.clone();
                                move |_, cx| {
                                    Tooltip::for_action_in("Actual size", &ActualSize, &focus, cx)
                                }
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.actual_size(&ActualSize, window, cx)
                            })),
                    )
                    .child(
                        ds::icon_button("zoom-in", IconName::Plus)
                            .icon_color(Color::Muted)
                            .tooltip({
                                let focus = focus.clone();
                                move |_, cx| Tooltip::for_action_in("Zoom in", &ZoomIn, &focus, cx)
                            })
                            .on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.zoom_in(&ZoomIn, window, cx)
                                }),
                            ),
                    )
                    .child(
                        ds::button("zoom-fit", "Fit")
                            .style(ButtonStyle::Subtle)
                            .color(Color::Muted)
                            .toggle_state(self.zoom == Zoom::Fit)
                            .tooltip(move |_, cx| {
                                Tooltip::for_action_in("Fit to the tab", &FitToWindow, &focus, cx)
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.fit(&FitToWindow, window, cx)
                            })),
                    )
                    .into_any_element(),
            );
        }
        match &self.export {
            Some(Export::Busy(what)) => end.push(ds::text::meta(*what).into_any_element()),
            Some(Export::Saved(path)) => {
                let path = path.clone();
                end.push(
                    ds::button("show-saved", "Show in Finder")
                        .style(ButtonStyle::Subtle)
                        .color(Color::Muted)
                        .tooltip(Tooltip::text(format!("Saved to {}", path.display())))
                        .on_click(move |_, _, cx| cx.reveal_path(&path))
                        .into_any_element(),
                );
            }
            Some(Export::Failed(error)) => {
                end.push(studio_ui::ds::text::error_note(error.clone()).into_any_element())
            }
            None => {}
        }
        if self.size.is_some() {
            end.push(
                ds::button("save-blob-file", "Save…")
                    .style(ButtonStyle::Subtle)
                    .color(Color::Muted)
                    .tooltip(Tooltip::text("Save the whole value to a file"))
                    .on_click(cx.listener(|this, _, _, cx| this.save(cx)))
                    .into_any_element(),
            );
            end.push(
                ds::button("open-blob-elsewhere", "Open in default app")
                    .style(ButtonStyle::Subtle)
                    .color(Color::Muted)
                    .tooltip(Tooltip::text(
                        "Hand it to the application the system uses for its kind",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.open_elsewhere(cx)))
                    .into_any_element(),
            );
        }
        end.extend(self.write_status(cx));
        let keys = if zoomable {
            let focus = &self.focus_handle;
            vec![
                KeyHint::new("Zoom in", &ZoomIn, focus),
                KeyHint::new("Zoom out", &ZoomOut, focus),
                KeyHint::new("Actual size", &ActualSize, focus),
            ]
        } else {
            Vec::new()
        };
        Some(ItemStatus { start, end, keys })
    }
}

impl BlobView {
    /// Whether the read can start: the tab reads at a commit, or the schema it reads the head
    /// with is in. Asked by a draw, which changes nothing in the session.
    fn can_load(&self, cx: &App) -> bool {
        self.reading_at().is_some() || matches!(self.session.read(cx).schema, Loadable::Loaded(_))
    }

    /// Starts the read a draw asked for ([`Self::load_asked`]), unless one is out.
    fn start_load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self._load.is_some() {
            return;
        }
        match self.reading_at() {
            Some(at) => {
                let load = Self::load(
                    &self.session,
                    &self.cell,
                    &self.branch,
                    Some(at),
                    None,
                    window,
                    cx,
                );
                self._load = Some(load);
            }
            // Opened at the head, the value is the session's branch's, which a write goes to,
            // or the branch the tab was saved on: the head is read first with the schema's
            // probe, so this waits for the schema, and is drawn again when it comes.
            None => {
                let session = self.session.read(cx);
                if !self.branch_given {
                    self.branch = session.branch.clone();
                }
                match &session.schema {
                    Loadable::Loaded(schema) => {
                        let probe = builder::head_probe(schema);
                        self.content = Loadable::Loading;
                        let load = Self::load(
                            &self.session,
                            &self.cell,
                            &self.branch,
                            None,
                            probe,
                            window,
                            cx,
                        );
                        self._load = Some(load);
                    }
                    // A schema that failed, or is not in any more, leaves the tab to its next
                    // draw.
                    Loadable::Failed(_) | Loadable::NotLoaded | Loadable::Loading => {}
                }
            }
        }
    }
}

impl Render for BlobView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Asked only when it can start, so a tab waiting for the schema draws once and waits
        // for it, which draws the tab again ([`SessionEvent::SchemaChanged`]).
        if self._load.is_none() && !self.load_asked {
            if self.can_load(cx) {
                self.load_asked = true;
                cx.defer_in(window, |this, window, cx| {
                    this.load_asked = false;
                    this.start_load(window, cx);
                });
            } else if let Loadable::Failed(error) = &self.session.read(cx).schema {
                self.content = Loadable::Failed(error.clone());
            }
        }
        // The branch the tab writes to is known once it is first drawn; a list held then may
        // be older than it. Asked after the draw, as the read is.
        if self.branch_listed == BranchListed::Unseen {
            cx.defer_in(window, |this, _, cx| this.follow_branch_list(cx));
        }
        let body = match &self.content {
            Loadable::Loading | Loadable::NotLoaded => {
                studio_ui::ds::loading_state(match self.size {
                    Some(size) => format!(
                        "Reading {}, {}…",
                        self.cell.property,
                        studio_ui::format_bytes(size)
                    ),
                    None => format!("Reading {}…", self.cell.property),
                })
                .into_any_element()
            }
            Loadable::Failed(error) => studio_ui::ds::error_state(
                format!("{} could not be shown", self.cell.property),
                error.clone(),
            )
            .into_any_element(),
            Loadable::Loaded(Content::Null) => {
                // An empty value's one action is a file to put in it, a drop doing the same.
                let writable = self.write_refusal(cx).is_none() && !self.saving();
                let (cell, branch) = (self.cell.clone(), self.branch.clone());
                studio_ui::ds::empty_state(IconName::FileGeneric, "No value")
                    .when(writable, |state| {
                        state.child(
                            ds::button("choose-blob-empty", "Choose File…")
                                .style(ButtonStyle::Outlined)
                                .tooltip(move |_, cx| {
                                    blob_write::upload_tooltip(&cell, &branch, cx)
                                })
                                .on_click(cx.listener(|this, _, _, cx| this.choose_file(cx))),
                        )
                    })
                    .into_any_element()
            }
            // Studio does not fetch objects outside the graph.
            Loadable::Loaded(Content::External(uri)) => {
                studio_ui::ds::empty_state(IconName::Link, "Stored outside the graph")
                    .child(studio_ui::ds::state_detail(uri.clone()))
                    .into_any_element()
            }
            Loadable::Loaded(Content::TooLarge) => studio_ui::ds::empty_state(
                IconName::FileGeneric,
                format!(
                    "{} is {}",
                    self.cell.property,
                    studio_ui::format_bytes(self.size.unwrap_or(0))
                ),
            )
            .child(studio_ui::ds::state_detail(format!(
                "Studio shows values up to {}; save it, or open it in the default app.",
                studio_ui::format_bytes(VIEW_MAX)
            )))
            .into_any_element(),
            Loadable::Loaded(Content::Image { image, size, .. }) => {
                let (image, size) = (image.clone(), *size);
                self.render_image(&image, size, cx)
            }
            Loadable::Loaded(Content::Pdf { pages, count }) => {
                let this = cx.entity();
                let measure = canvas(
                    move |bounds, _, cx| {
                        this.update(cx, |view, cx| {
                            if view.bounds.map(|b| b.size) != Some(bounds.size) {
                                view.bounds = Some(bounds);
                                cx.notify();
                            }
                        })
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full();
                div()
                    .relative()
                    .size_full()
                    .child(measure)
                    .child(self.render_pdf(pages, *count, cx))
                    .into_any_element()
            }
            Loadable::Loaded(Content::Text(editor, _)) => div()
                .size_full()
                .px(px(ds::space::LG))
                .py(px(ds::space::MD))
                .child(editor.clone())
                .into_any_element(),
            Loadable::Loaded(Content::Bytes(hex)) => v_flex()
                .size_full()
                .p(px(ds::space::LG))
                .gap(px(ds::space::SM))
                .child(ds::text::meta(format!(
                    "Binary · the first {} shown",
                    studio_ui::format_bytes(
                        (HEX_MAX as u64).min(self.size.unwrap_or(HEX_MAX as u64))
                    )
                )))
                .child(div().flex_1().min_h_0().child(hex.clone()))
                .into_any_element(),
        };
        // Images and PDFs take zoom keys; text and bytes keep their keys for the editor.
        let context = match &self.content {
            Loadable::Loaded(Content::Text(..) | Content::Bytes(_)) => "BlobText",
            _ => "BlobView",
        };
        // A file dropped from the file manager anywhere on the tab is stored in the value. Choose
        // File and Clear are actions only while the value can take them, so the palette offers
        // them only then.
        let droppable = self.write_refusal(cx).is_none() && !self.saving();
        let clearable = self.clear_refusal(cx).is_none() && !self.saving();
        let drop_bg = studio_ui::ds::surface::drop_target(cx);
        div()
            .key_context(context)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::actual_size))
            .on_action(cx.listener(Self::fit))
            .when(droppable, |tab| {
                tab.on_action(cx.listener(|this, _: &ChooseFile, _, cx| this.choose_file(cx)))
            })
            .when(clearable, |tab| {
                tab.on_action(cx.listener(|this, _: &ClearValue, _, cx| this.clear(cx)))
            })
            .size_full()
            .bg(studio_ui::ds::surface::document(cx))
            .when(droppable, |tab| {
                tab.drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(drop_bg))
                    .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                        this.drop_files(paths.paths(), cx)
                    }))
            })
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A renderer for decoding in tests; no asset is ever read.
    fn svg() -> SvgRenderer {
        SvgRenderer::new(Arc::new(()))
    }

    /// A value too large to show still saves and opens under a name that says what it is:
    /// its first bytes alone are read for it.
    #[gpui::test]
    async fn a_value_too_large_to_show_is_named_by_its_first_bytes(cx: &mut gpui::TestAppContext) {
        use http_client::{AsyncBody, FakeHttpClient, Response, http::Method};

        cx.update(studio_theme::init_test);
        let ranges = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = ranges.clone();
        let http = FakeHttpClient::create(move |request| {
            let seen = seen.clone();
            async move {
                let ok = || {
                    Response::builder()
                        .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                };
                let response = if !request.uri().path().ends_with("/blob") {
                    ok().status(404).body(AsyncBody::empty())
                } else if request.method() == Method::HEAD {
                    ok().status(200)
                        .header("content-length", "100000000")
                        .body(AsyncBody::empty())
                } else {
                    let range = request
                        .headers()
                        .get("range")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    seen.lock().unwrap().push(range);
                    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
                    png.resize(1024, 0);
                    ok().status(206)
                        .header("content-range", "bytes 0-1023/100000000")
                        .body(AsyncBody::from(png))
                };
                Ok(response.unwrap())
            }
        });
        let server = graph_store::ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: graph_store::ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection =
            cx.new(|_| graph_store::Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        let cell = BlobCell {
            edge: false,
            type_name: "Note".into(),
            id: "n1".into(),
            property: "scan".into(),
        };
        let (view, cx) = cx.add_window_view(|window, cx| {
            BlobView::new(session, cell, Some("c1".into()), false, window, cx)
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(matches!(view.content, Loadable::Loaded(Content::TooLarge)));
            assert!(view.file_name().ends_with(".png"), "{}", view.file_name());
        });
        assert_eq!(
            *ranges.lock().unwrap(),
            [format!("bytes=0-{}", HEAD_BYTES - 1)]
        );
    }

    const DOCS: &str = "node Person {\n  slug: String @key\n  name: String\n  doc: Blob?\n}\n";

    fn doc() -> BlobCell {
        BlobCell {
            edge: false,
            type_name: "Person".into(),
            id: "p1".into(),
            property: "doc".into(),
        }
    }

    /// A file stored from the tab is bound to the ETag the tab read, and the tab shows the new
    /// value read at the commit its write made; Clear empties it the same way.
    #[gpui::test]
    async fn a_file_stored_from_the_tab_shows_at_its_own_commit(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, blobs) = crate::test_support::session_with_blobs(DOCS, cx);
        let read = blobs.set("Person/p1/doc", &[0xff, 0x00, 0x13]);
        let (view, cx) =
            cx.add_window_view(|window, cx| BlobView::new(session, doc(), None, false, window, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(matches!(view.content, Loadable::Loaded(Content::Bytes(_))));
            assert_eq!(view.branch, "main");
        });
        let file = crate::test_support::upload_file("hello.txt", b"hello", None);
        view.update(cx, |view, cx| {
            view.upload(file, cx);
            assert!(view.saving());
        });
        cx.run_until_parked();
        let writes = blobs.writes.lock().unwrap().clone();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].if_match.as_deref(), Some(read.as_str()));
        let stored = blobs.get("Person/p1/doc").unwrap();
        view.read_with(cx, |view, _| {
            assert_eq!(view.own_commit.as_deref(), Some("c2"));
            assert_eq!(view.edit, None);
            assert_eq!(view.size, Some(5));
            assert!(matches!(
                view.content,
                Loadable::Loaded(Content::Text(_, false))
            ));
            assert_eq!(
                view.meta,
                Some(BlobMeta::Managed {
                    size: 5,
                    etag: Some(stored.1.clone())
                })
            );
        });

        view.update(cx, |view, cx| view.clear(cx));
        cx.run_until_parked();
        let writes = blobs.writes.lock().unwrap().clone();
        assert_eq!(writes[1].method, "DELETE");
        assert_eq!(writes[1].if_match.as_deref(), Some(stored.1.as_str()));
        view.read_with(cx, |view, _| {
            assert_eq!(view.own_commit.as_deref(), Some("c3"));
            assert!(matches!(view.content, Loadable::Loaded(Content::Null)));
        });
    }

    /// A tab opened from a view at the branch head reads the value at that view's commit, so it
    /// shows what the view showed, and stores a file on its branch bound to the value it read
    /// there. It is a tab of its own, apart from one held at that commit and from the head's.
    #[gpui::test]
    async fn a_tab_from_a_view_at_the_head_stores_a_file_bound_to_what_it_read(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, blobs) = crate::test_support::session_with_blobs(DOCS, cx);
        let read = blobs.set("Person/p1/doc", b"old");
        let (view, cx) = cx.add_window_view(|window, cx| {
            BlobView::new(session, doc(), Some("c1".into()), false, window, cx)
        });
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            assert_eq!(view.write_refusal(cx), None);
            assert_eq!(view.clear_refusal(cx), None);
            let key = |at: Option<&str>, held: bool| BlobView::key(&doc(), at, held, "main");
            assert_eq!(
                view.dedupe_key(cx).as_deref(),
                Some(key(Some("c1"), false).as_str())
            );
            assert_ne!(key(Some("c1"), false), key(Some("c1"), true));
            assert_ne!(key(Some("c1"), false), key(None, false));
            // A tab that writes is one per branch; one held writes nowhere.
            let on_fix = |at: Option<&str>, held: bool| BlobView::key(&doc(), at, held, "fix");
            assert_ne!(key(Some("c1"), false), on_fix(Some("c1"), false));
            assert_ne!(key(None, false), on_fix(None, false));
            assert_eq!(key(Some("c1"), true), on_fix(Some("c1"), true));
        });
        let file = crate::test_support::upload_file("from-head.txt", b"new", None);
        view.update(cx, |view, cx| view.upload(file, cx));
        cx.run_until_parked();
        let writes = blobs.writes.lock().unwrap().clone();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].branch, "main");
        assert_eq!(writes[0].if_match.as_deref(), Some(read.as_str()));
        view.read_with(cx, |view, _| {
            assert_eq!(view.edit, None, "stored");
            assert_eq!(view.own_commit.as_deref(), Some("c2"));
        });
    }

    /// A file stored from the tab is bound as it is sent, after the file is read, to what the
    /// tab shows then, read where it reads the value then. The cell is saved while the file is
    /// read, and the tab moves to that save's commit and value, as the landing of its own write
    /// moves it (`written`); the upload carries the ETag read there and is stored. Bound when
    /// the file was chosen, it named the value the tab no longer showed and was refused as
    /// changed.
    ///
    /// No call of the tab's reaches that state, so this test sets it by hand: `send` refuses a
    /// second write while one is out (`saving`), a write that lands (`written`) empties `meta`
    /// until the value is read again, which `send` refuses too, and the tab follows no other
    /// view's writes. What is set is what `written` and the read after it leave. The binding
    /// guards a tab that comes to follow other writes; the table's upload, which does see its
    /// own writes land meanwhile, is driven through
    /// `table::tests::a_tables_upload_is_bound_where_its_values_hold_as_it_goes`.
    #[gpui::test]
    async fn a_tabs_upload_is_bound_to_what_the_tab_shows_as_it_is_sent(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, blobs) = crate::test_support::session_with_blobs(DOCS, cx);
        blobs
            .versioned
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let chosen = blobs.set("Person/p1/doc", b"old");
        let (view, cx) = cx.add_window_view(|window, cx| {
            BlobView::new(session.clone(), doc(), None, false, window, cx)
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.reading_at().as_deref(), Some("c1"));
            assert_eq!(
                view.meta,
                Some(BlobMeta::Managed {
                    size: 3,
                    etag: Some(chosen.clone())
                })
            );
        });
        // The cell is saved at c2: a value of the same size, under a new ETag.
        let saved = session.update(cx, |session, cx| {
            let bytes = b"mid".to_vec();
            session.blob_put_on("main", doc(), Some(chosen.clone()), bytes, "mid".into(), cx)
        });
        cx.run_until_parked();
        saved.await;
        let now = blobs.get("Person/p1/doc").unwrap().1;

        let file = crate::test_support::upload_file("bound-in-tab.txt", b"bound", None);
        view.update(cx, |view, cx| {
            view.upload(file, cx);
            // While the file is read, the tab comes to show the save, at its commit.
            view.own_commit = Some("c2".into());
            view.meta = Some(BlobMeta::Managed {
                size: 3,
                etag: Some(now.clone()),
            });
        });
        cx.run_until_parked();
        let writes = blobs.writes.lock().unwrap().clone();
        assert_eq!(writes.len(), 2, "the save, then the upload");
        assert_eq!(writes[1].if_match.as_deref(), Some(now.as_str()));
        assert_eq!(blobs.get("Person/p1/doc").unwrap().0, b"bound");
        view.read_with(cx, |view, _| {
            assert_eq!(view.edit, None, "stored, not refused as changed");
            assert_eq!(view.own_commit.as_deref(), Some("c3"));
        });
    }

    /// A tab opened at a commit, or on a branch the access route says the credential may not
    /// change, stores no file and clears nothing, and says why; on the branch, Choose File and
    /// Clear are not registered either.
    #[gpui::test]
    async fn a_tab_at_a_commit_or_on_a_refused_branch_writes_nothing(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(studio_theme::init_test);
        let (session, blobs) = crate::test_support::session_with_blobs(DOCS, cx);
        blobs.set("Person/p1/doc", b"old");
        let (view, held_cx) = cx.add_window_view(|window, cx| {
            BlobView::new(session, doc(), Some("c1".into()), true, window, cx)
        });
        held_cx.run_until_parked();
        let file = crate::test_support::upload_file("tab.txt", b"tab", None);
        view.update(held_cx, |view, cx| {
            assert!(view.write_refusal(cx).is_some() && view.clear_refusal(cx).is_some());
            view.upload(file, cx);
        });
        held_cx.run_until_parked();
        assert!(blobs.writes.lock().unwrap().is_empty());
        view.read_with(held_cx, |view, _| {
            assert!(matches!(
                &view.edit,
                Some(BlobEdit::Failed { notice, .. }) if notice.message.starts_with("Read-only")
            ));
        });

        let (session, _, blobs) = crate::test_support::session_with_access(
            DOCS,
            crate::test_support::read_only_access(),
            cx,
        );
        blobs.set("Person/p1/doc", b"old");
        let (view, cx) =
            cx.add_window_view(|window, cx| BlobView::new(session, doc(), None, false, window, cx));
        cx.run_until_parked();
        cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
        cx.run_until_parked();
        let refused = "Your credential may not change main";
        let offered = cx.update(|window, cx| {
            window.is_action_available(&ChooseFile, cx)
                || window.is_action_available(&ClearValue, cx)
        });
        assert!(!offered);
        let file = crate::test_support::upload_file("branch.txt", b"branch", None);
        view.update(cx, |view, cx| {
            assert_eq!(view.write_refusal(cx).as_deref(), Some(refused));
            assert_eq!(view.clear_refusal(cx).as_deref(), Some(refused));
            view.upload(file, cx);
        });
        cx.run_until_parked();
        assert!(blobs.writes.lock().unwrap().is_empty());
        view.read_with(cx, |view, _| {
            assert!(matches!(
                &view.edit,
                Some(BlobEdit::Failed { notice, .. }) if notice.message == refused
            ));
        });
    }

    /// The value of main at `c1` spans two of the client's ranges; once the first range is
    /// served, another writer's commit `c2` puts other bytes of the same size there. `fix` holds
    /// ten bytes. A read names its commit, or the branch's head.
    const TWO_RANGES: usize = 16 * 1024 * 1024 + 10;

    /// A server whose main moves while a value is read ([`TWO_RANGES`]); every Blob request's
    /// query is kept.
    #[derive(Default)]
    struct MovingHead {
        moved: std::sync::atomic::AtomicBool,
        asked: std::sync::Mutex<Vec<String>>,
    }

    impl MovingHead {
        fn main(&self) -> &'static str {
            if self.moved.load(std::sync::atomic::Ordering::SeqCst) {
                "c2"
            } else {
                "c1"
            }
        }

        /// The commit a read names: its snapshot, else its branch's head.
        fn at(&self, snapshot: Option<&str>, branch: Option<&str>) -> String {
            match (snapshot, branch) {
                (Some(commit), _) => commit.to_owned(),
                (None, Some("fix")) => "f1".to_owned(),
                _ => self.main().to_owned(),
            }
        }
    }

    fn moving_head_session(
        cx: &mut gpui::TestAppContext,
    ) -> (Entity<GraphSession>, Arc<MovingHead>) {
        use futures::AsyncReadExt as _;
        use http_client::{AsyncBody, FakeHttpClient, Response, http::Method};

        let server = Arc::new(MovingHead::default());
        let state = server.clone();
        let http = FakeHttpClient::create(move |request| {
            let state = state.clone();
            async move {
                let ok = || {
                    Response::builder()
                        .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                };
                let path = request.uri().path().to_owned();
                let query = request.uri().query().unwrap_or_default().to_owned();
                if path.ends_with("/blob") {
                    state.asked.lock().unwrap().push(query.clone());
                    let param = |name: &str| {
                        query
                            .split('&')
                            .filter_map(|pair| pair.split_once('='))
                            .find(|(key, _)| *key == name)
                            .map(|(_, value)| value.to_owned())
                    };
                    let at = state.at(param("snapshot").as_deref(), param("branch").as_deref());
                    let (byte, len) = match at.as_str() {
                        "c1" => (0xfe, TWO_RANGES),
                        "c2" => (0xff, TWO_RANGES),
                        _ => (0xfd, 10),
                    };
                    let etag = format!("\"{at}\"");
                    if request.method() == Method::HEAD {
                        return Ok(ok()
                            .status(200)
                            .header("etag", etag)
                            .header("content-length", len.to_string())
                            .body(AsyncBody::empty())
                            .unwrap());
                    }
                    let (from, to) = request
                        .headers()
                        .get("range")
                        .and_then(|value| value.to_str().ok())
                        .and_then(|range| range.strip_prefix("bytes="))
                        .and_then(|range| range.split_once('-'))
                        .and_then(|(from, to)| Some((from.parse().ok()?, to.parse().ok()?)))
                        .unwrap_or((0usize, len - 1));
                    if from >= len {
                        return Ok(ok()
                            .status(416)
                            .header("content-range", format!("bytes */{len}"))
                            .body(AsyncBody::empty())
                            .unwrap());
                    }
                    let to = to.min(len - 1);
                    // The first range served, the other writer's commit lands on main.
                    state.moved.store(true, std::sync::atomic::Ordering::SeqCst);
                    return Ok(ok()
                        .status(206)
                        .header("etag", etag)
                        .header("content-range", format!("bytes {from}-{to}/{len}"))
                        .body(AsyncBody::from(vec![byte; to + 1 - from]))
                        .unwrap());
                }
                let mut bytes = Vec::new();
                request.into_body().read_to_end(&mut bytes).await.ok();
                let body = if path.ends_with("/schema") {
                    json!({ "schema_source": DOCS })
                } else if path.ends_with("/query") {
                    let asked: serde_json::Value =
                        serde_json::from_slice(&bytes).unwrap_or_default();
                    let at = state.at(asked["snapshot"].as_str(), asked["branch"].as_str());
                    json!({
                        "query_name": "q",
                        "target": {"branch": "main", "snapshot": null},
                        "row_count": 0,
                        "columns": ["n"],
                        "rows": [],
                        "graph_commit_id": at,
                    })
                } else {
                    json!({ "branches": ["main", "fix"] })
                };
                Ok(ok()
                    .status(200)
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let entry = graph_store::ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: graph_store::ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection =
            cx.new(|_| graph_store::Connection::new(entry, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        (session, server)
    }

    /// A tab opened at the branch head reads which commit the head is, and holds to it: every
    /// range of the value, and a save, read that commit. Another writer's commit landing between
    /// two ranges splices nothing in, and a save after the window moved to another branch
    /// writes the value shown, whole. Before, each range and the save read the session's head,
    /// whichever branch and commit that was by then.
    #[gpui::test]
    async fn a_tab_opened_at_the_head_reads_and_saves_at_the_head_it_found(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, server) = moving_head_session(cx);
        let (view, cx) = cx.add_window_view(|window, cx| {
            BlobView::new(session.clone(), doc(), None, false, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(server.main(), "c2", "main moved between the two ranges");
        view.read_with(cx, |view, _| {
            assert_eq!(view.opened_at.as_deref(), Some("c1"));
            assert_eq!(view.size, Some(TWO_RANGES as u64));
            assert!(matches!(view.content, Loadable::Loaded(Content::Bytes(_))));
            assert!(view.facts().ends_with("at c1"), "{}", view.facts());
        });

        session.update(cx, |session, cx| session.set_branch("fix".into(), cx));
        cx.run_until_parked();
        let dir = std::env::temp_dir().join(format!("studio-blob-head-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p1-doc.bin");
        std::fs::remove_file(&path).ok();
        view.update(cx, |view, cx| view.write_to(path.clone(), false, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(matches!(view.export, Some(Export::Saved(_))), "saved");
            assert_eq!(view.branch, "main", "it writes where it read");
        });
        let saved = std::fs::read(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(saved.len(), TWO_RANGES);
        assert!(saved.iter().all(|byte| *byte == 0xfe), "one value, not two");
        // What the cell holds, the two ranges shown, and the two saved: all at c1.
        let asked = server.asked.lock().unwrap().clone();
        assert_eq!(asked.len(), 5, "{asked:?}");
        assert!(
            asked
                .iter()
                .all(|query| query.contains("snapshot=c1") && !query.contains("branch=")),
            "{asked:?}"
        );
    }

    /// A tab saved after its own write comes back reading where that write left it, so it
    /// shows the value it stored and binds its next write to that. Saved at the commit it was
    /// opened at, it came back showing the value its write replaced, and its next write was
    /// refused as changed elsewhere. A tab saved without saying whether it holds its commit
    /// does not come back.
    #[gpui::test]
    async fn a_restored_tab_reads_and_binds_where_its_own_write_left_it(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, blobs) = crate::test_support::session_with_blobs(DOCS, cx);
        blobs
            .versioned
            .store(true, std::sync::atomic::Ordering::SeqCst);
        blobs.set("Person/p1/doc", b"old");
        cx.run_until_parked();
        // Another view's write publishes c2, and a view at the head that read it opens the tab.
        let seed = session.update(cx, |session, cx| {
            session.blob_put_on("main", doc(), None, b"mid".to_vec(), "seed".into(), cx)
        });
        cx.run_until_parked();
        seed.await;
        let (view, view_cx) = cx.add_window_view(|window, cx| {
            BlobView::new(session.clone(), doc(), Some("c2".into()), false, window, cx)
        });
        view_cx.run_until_parked();
        let file = crate::test_support::upload_file("restored-first.txt", b"new", None);
        view.update(view_cx, |view, cx| view.upload(file, cx));
        view_cx.run_until_parked();
        let saved = view.read_with(view_cx, |view, cx| {
            assert_eq!(view.own_commit.as_deref(), Some("c3"));
            view.serialize(cx).unwrap()
        });
        assert_eq!(saved.data["at_commit"], "c3");
        assert_eq!(saved.data["held"], false);
        assert_eq!(saved.data["branch"], "main");

        let back = crate::saved_blob(&saved.data).expect("the tab comes back");
        let (restored, cx) = cx.add_window_view(|window, cx| {
            crate::blob_view(session, &back.open, back.branch, window, cx)
        });
        cx.run_until_parked();
        let stored = blobs.get("Person/p1/doc").unwrap();
        restored.read_with(cx, |view, _| {
            assert_eq!(
                view.meta,
                Some(BlobMeta::Managed {
                    size: 3,
                    etag: Some(stored.1.clone())
                })
            );
        });
        let file = crate::test_support::upload_file("restored-second.txt", b"newer", None);
        restored.update(cx, |view, cx| view.upload(file, cx));
        cx.run_until_parked();
        let writes = blobs.writes.lock().unwrap().clone();
        assert_eq!(
            writes.last().unwrap().if_match.as_deref(),
            Some(stored.1.as_str())
        );
        restored.read_with(cx, |view, _| {
            assert_eq!(view.edit, None, "stored, not refused as changed elsewhere");
            assert_eq!(view.own_commit.as_deref(), Some("c4"));
        });

        let mut old = saved.data.clone();
        old.as_object_mut().unwrap().remove("held");
        assert!(
            crate::saved_blob(&old).is_none(),
            "the old shape stays closed"
        );
    }

    /// A restored tab writes to the branch it was saved on, whichever branch the window shows
    /// then: it reads that branch's commit, and a write bound to a value read there belongs on
    /// it. Before, it took the session's branch, so it read main's value and stored the file on
    /// fix. A branch gone since says so, and nothing is sent; one made since the session's list
    /// was read is not taken for gone, as the list is read again first; a tab that writes and
    /// was saved without its branch does not come back, while one held at a commit, which
    /// writes nowhere, does.
    #[gpui::test]
    async fn a_restored_tab_writes_only_to_the_branch_it_was_saved_on(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let (session, blobs) = crate::test_support::session_with_blobs(DOCS, cx);
        let read = blobs.set("Person/p1/doc", b"old");
        session.update(cx, |session, cx| session.reload_branches(cx));
        let (view, view_cx) = cx.add_window_view(|window, cx| {
            BlobView::new(session.clone(), doc(), Some("c1".into()), false, window, cx)
        });
        view_cx.run_until_parked();
        let saved = view.read_with(view_cx, |view, cx| view.serialize(cx).unwrap());
        assert_eq!(saved.data["branch"], "main");
        // The window is on fix when the tab comes back.
        session.update(cx, |session, cx| session.set_branch("fix".into(), cx));
        cx.run_until_parked();
        /// The tab `data` saves, back as the workspace restores it.
        fn restore<'a>(
            session: &Entity<GraphSession>,
            data: &serde_json::Value,
            cx: &'a mut gpui::TestAppContext,
        ) -> (Entity<BlobView>, &'a mut gpui::VisualTestContext) {
            let back = crate::saved_blob(data).expect("the tab comes back");
            let session = session.clone();
            let (restored, cx) = cx.add_window_view(|window, cx| {
                crate::blob_view(session, &back.open, back.branch, window, cx)
            });
            cx.run_until_parked();
            (restored, cx)
        }

        let (restored, restored_cx) = restore(&session, &saved.data, cx);
        let file = crate::test_support::upload_file("restored-branch.txt", b"new", None);
        restored.update(restored_cx, |view, cx| {
            assert_eq!(view.branch, "main");
            assert_eq!(view.write_refusal(cx), None);
            let key = BlobView::key(&doc(), Some("c1"), false, "main");
            assert_eq!(view.dedupe_key(cx).as_deref(), Some(key.as_str()));
            view.upload(file, cx);
        });
        restored_cx.run_until_parked();
        let writes = blobs.writes.lock().unwrap().clone();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].branch, "main", "where it was saved, not fix");
        assert_eq!(writes[0].if_match.as_deref(), Some(read.as_str()));

        let mut gone = saved.data.clone();
        gone["branch"] = "gone".into();
        let (restored, restored_cx) = restore(&session, &gone, cx);
        let file = crate::test_support::upload_file("restored-gone.txt", b"newer", None);
        restored.update(restored_cx, |view, cx| {
            let refusal = view.write_refusal(cx).unwrap_or_default();
            assert!(refusal.starts_with("gone no longer exists"), "{refusal}");
            assert_eq!(view.clear_refusal(cx), Some(refusal.clone()));
            view.upload(file, cx);
        });
        restored_cx.run_until_parked();
        assert_eq!(blobs.writes.lock().unwrap().len(), 1, "nothing sent");
        restored.read_with(restored_cx, |view, _| {
            assert!(matches!(
                &view.edit,
                Some(BlobEdit::Failed { notice, .. })
                    if notice.message.starts_with("gone no longer exists")
            ));
        });

        // Fix was made after the session's list was read: the list is read again before the
        // tab says fix is gone, and the file is stored there.
        session.update(cx, |session, _| {
            session.branches = Loadable::Loaded(vec!["main".into()]);
        });
        let mut made = saved.data.clone();
        made["branch"] = "fix".into();
        let back = crate::saved_blob(&made).expect("the tab comes back");
        let held = session.clone();
        let (restored, restored_cx) = cx.add_window_view(|window, cx| {
            crate::blob_view(held, &back.open, back.branch, window, cx)
        });
        restored.update(restored_cx, |view, cx| {
            assert_eq!(
                view.write_refusal(cx),
                None,
                "not gone while the list is read"
            );
        });
        restored_cx.run_until_parked();
        let file = crate::test_support::upload_file("restored-made.txt", b"made", None);
        restored.update(restored_cx, |view, cx| {
            assert!(
                session
                    .read(cx)
                    .branches
                    .loaded()
                    .is_some_and(|branches| branches.iter().any(|b| b == "fix")),
                "the list was read again"
            );
            assert_eq!(view.write_refusal(cx), None);
            view.upload(file, cx);
        });
        restored_cx.run_until_parked();
        let writes = blobs.writes.lock().unwrap().clone();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[1].branch, "fix");

        let mut unsaid = saved.data.clone();
        unsaid.as_object_mut().unwrap().remove("branch");
        assert!(crate::saved_blob(&unsaid).is_none(), "writes, but to where");
        unsaid["held"] = true.into();
        let held = crate::saved_blob(&unsaid).expect("held, it writes nowhere");
        assert_eq!(held.branch, None);
    }

    /// One judgement of what a value is, which the peek, the tab and a saved file's name share:
    /// a PNG, an SVG (past its declaration, a comment and a doctype), a PDF, text and other
    /// bytes each show and save as the same kind in all three. Before, an SVG was an image in
    /// the tab but text in the peek, saved as `.txt`, and a PDF was text or a hex dump in the
    /// peek. A page holding an icon's markup is text.
    #[test]
    fn the_peek_the_tab_and_a_saved_name_tell_a_value_alike() {
        use crate::blob::{Preview, bytes_wanted, preview};

        let renderer = svg();
        let png = {
            let mut png = Vec::new();
            image::RgbaImage::new(3, 2)
                .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .unwrap();
            png
        };
        let icon = b"<?xml version=\"1.0\"?>\n<!-- drawn by hand -->\n<!DOCTYPE svg PUBLIC \
            \"-//W3C//DTD SVG 1.1//EN\" \"http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd\">\n\
            <svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 1 1\"/>"
            .to_vec();
        let page = b"<html><body><svg viewBox=\"0 0 1 1\"></svg><p>hi</p></body></html>".to_vec();
        let pdf = crate::pdf::tests::one_page_pdf();
        let text = b"plain notes".to_vec();
        let binary = vec![0xff, 0x00, 0x13];
        let peek = |bytes: &[u8]| preview(bytes.to_vec(), bytes.len() as u64, &renderer);
        let name = |bytes: &[u8]| suggested_file_name("n1", "v", bytes);

        assert_eq!(value_kind(&png), ValueKind::Image(ImageFormat::Png));
        assert!(matches!(peek(&png), Preview::Image(ShownImage::Raster(_))));
        assert!(matches!(
            classify(&png, &renderer),
            Kind::Image { name: "PNG", .. }
        ));
        assert_eq!(name(&png), "n1-v.png");

        assert_eq!(value_kind(&icon), ValueKind::Svg);
        assert_eq!(bytes_wanted(&icon, 5_000), 5_000, "read whole to draw");
        assert!(matches!(peek(&icon), Preview::Image(ShownImage::Svg(_))));
        assert!(matches!(
            classify(&icon, &renderer),
            Kind::Image { name: "SVG", .. }
        ));
        assert_eq!(name(&icon), "n1-v.svg");

        assert_eq!(value_kind(&page), ValueKind::Text);
        assert!(matches!(peek(&page), Preview::Text { .. }));
        assert!(matches!(classify(&page, &renderer), Kind::Text(..)));
        assert_eq!(name(&page), "n1-v.txt");

        assert_eq!(value_kind(&pdf), ValueKind::Pdf);
        assert_eq!(bytes_wanted(&pdf[..16], 5_000), 5_000, "read whole to draw");
        assert_eq!(name(&pdf), "n1-v.pdf");
        if cfg!(target_os = "macos") {
            assert!(matches!(peek(&pdf), Preview::Image(ShownImage::Raster(_))));
            assert!(matches!(
                classify(&pdf, &renderer),
                Kind::Pdf { count: 1, .. }
            ));
        } else {
            // Where the system draws no PDF, neither does Studio: never its source as text.
            assert!(matches!(peek(&pdf), Preview::Bytes { .. }));
            assert!(matches!(classify(&pdf, &renderer), Kind::Unreadable(_)));
        }

        assert_eq!(value_kind(&text), ValueKind::Text);
        assert!(matches!(peek(&text), Preview::Text { text, more: 0 } if text == "plain notes"));
        assert!(matches!(classify(&text, &renderer), Kind::Text(..)));
        assert_eq!(name(&text), "n1-v.txt");

        assert_eq!(value_kind(&binary), ValueKind::Binary);
        assert!(matches!(peek(&binary), Preview::Bytes { .. }));
        assert!(matches!(classify(&binary, &renderer), Kind::Bytes(_)));
        assert_eq!(name(&binary), "n1-v.bin");
    }

    #[test]
    fn values_are_told_apart_by_their_bytes() {
        let png = {
            let mut png = Vec::new();
            image::RgbaImage::new(3, 2)
                .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .unwrap();
            png
        };
        assert!(matches!(
            classify(&png, &svg()),
            Kind::Image {
                size: Some((3, 2)),
                name: "PNG",
                ..
            }
        ));
        assert!(matches!(
            classify(
                b"<?xml version=\"1.0\"?><svg xmlns=\"http://www.w3.org/2000/svg\"/>",
                &svg()
            ),
            Kind::Image {
                name: "SVG",
                size: None,
                ..
            }
        ));
        assert!(
            matches!(classify(b"plain notes", &svg()), Kind::Text(text, false) if text == "plain notes")
        );
        // A character straddling the 4,096th byte, where the SVG check stops looking.
        let long = format!("{}é more", "a".repeat(4095));
        assert!(
            matches!(classify(long.as_bytes(), &svg()), Kind::Text(text, false) if text == long)
        );
        // A text past what is shown is cut on a character's boundary, and says so.
        let huge = format!("{}é", "a".repeat(TEXT_SHOWN - 1));
        assert!(matches!(
            classify(huge.as_bytes(), &svg()),
            Kind::Text(text, true) if text.len() == TEXT_SHOWN - 1
        ));
        assert!(matches!(
            classify(&[0xff, 0x00, 0x13], &svg()),
            Kind::Bytes(_)
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_pdf_is_drawn_page_by_page() {
        let Kind::Pdf { pages, count } = classify(&crate::pdf::tests::one_page_pdf(), &svg())
        else {
            panic!("not read as a PDF");
        };
        assert_eq!((pages.len(), count), (1, 1));
        assert_eq!(pages[0].1, PDF_DRAW_WIDTH);
    }

    #[test]
    fn the_open_folder_keeps_a_day() {
        let dir = std::env::temp_dir().join(format!("studio-blob-open-{}", std::process::id()));
        prepare_open_folder(&dir).unwrap();
        std::fs::write(dir.join("fresh.txt"), b"x").unwrap();
        prepare_open_folder(&dir).unwrap();
        assert!(dir.join("fresh.txt").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}

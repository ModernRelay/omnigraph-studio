//! A Blob value in a tab of its own (spec 004), shown for what it is: an image to fit, zoom and
//! pan, as Zed's image viewer does; a PDF's pages; text in a read-only editor that can be
//! searched; anything else as a hex dump. Read whole at the commit it was opened at, up to
//! [`VIEW_MAX`]; a larger value, or one the tab cannot show, can still be saved or opened in the
//! system's own application.

use std::path::PathBuf;
use std::sync::Arc;

use editor::Editor;
use gpui::{
    App, AppContext as _, Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable,
    ImageFormat, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement, Pixels,
    Point, Render, ScrollDelta, ScrollWheelEvent, SharedString, Styled, SvgRenderer, Task, Window,
    canvas, checkerboard, div, img, point,
};
use graph_client::{BlobCell, BlobMeta};
use graph_store::{GraphSession, Loadable};
use serde_json::json;
use studio_actions::canvas::{ActualSize, FitToWindow, ZoomIn, ZoomOut};
use studio_ui::ds;
use studio_workspace::{Item, ItemEvent, ItemFooter, KeyHint, SavedItem};
use ui::Tooltip;
use ui::prelude::*;

use crate::blob::{
    ShownImage, hex_dump_lines, read_value, save_folder, save_value, sniff_image,
    suggested_file_name, text_prefix,
};
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
/// The first bytes of a value kept to say what it is, which Save and Open name the file by.
const HEAD_BYTES: u64 = 1024;
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

/// Saving the value, or opening it elsewhere.
enum Export {
    Busy(&'static str),
    Saved(PathBuf),
    Failed(String),
}

pub struct BlobView {
    session: Entity<GraphSession>,
    cell: BlobCell,
    snapshot: Option<String>,
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
    _export: Option<Task<()>>,
}

impl BlobView {
    pub fn new(
        session: Entity<GraphSession>,
        cell: BlobCell,
        snapshot: Option<String>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.on_release(|this: &mut Self, cx| this.release_images(cx))
            .detach();
        Self {
            session,
            cell,
            snapshot,
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
            _export: None,
        }
    }

    pub fn key(cell: &BlobCell, snapshot: Option<&str>) -> String {
        format!(
            "blob:{}:{}:{}:{}:{}",
            if cell.edge { "edge" } else { "node" },
            cell.type_name,
            cell.id,
            cell.property,
            snapshot.unwrap_or("head")
        )
    }

    /// Reads what the cell holds, then the value, then what it is.
    fn load(
        session: &Entity<GraphSession>,
        cell: &BlobCell,
        snapshot: &Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let meta = session.update(cx, |session, cx| {
            session.blob_meta(cell.clone(), snapshot.clone(), cx)
        });
        let (session, cell, snapshot) = (session.downgrade(), cell.clone(), snapshot.clone());
        cx.spawn_in(window, async move |this, cx| {
            let fail = |error: String, cx: &mut gpui::AsyncWindowContext| {
                this.update(cx, |this, cx| {
                    this.content = Loadable::Failed(error);
                    cx.notify();
                })
                .ok();
            };
            let size = match meta.await {
                Ok(BlobMeta::Managed { size, .. }) => size,
                Ok(BlobMeta::External { uri }) => {
                    this.update(cx, |this, cx| {
                        this.content = Loadable::Loaded(Content::External(uri));
                        cx.notify();
                    })
                    .ok();
                    return;
                }
                Ok(BlobMeta::Null) => {
                    this.update(cx, |this, cx| {
                        this.content = Loadable::Loaded(Content::Null);
                        cx.notify();
                    })
                    .ok();
                    return;
                }
                Err(error) => return fail(error.to_string(), cx),
            };
            this.update(cx, |this, cx| {
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
        let (cell, snapshot) = (self.cell.clone(), self.snapshot.clone());
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
        if let Some(commit) = &self.snapshot {
            facts.push(format!("at {}", studio_ui::short_commit_id(commit)));
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
                    .border_1()
                    .border_color(ds::surface::rule(cx))
                    .child(img(image.source()).size_full())
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
    /// The text, cut at [`TEXT_SHOWN`] on a character's boundary, and whether it was.
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

/// What a value is, with its images decoded; call it from a background task.
fn classify(bytes: &[u8], svg: &SvgRenderer) -> Kind {
    let unreadable = || Kind::Unreadable("The image could not be decoded.".into());
    if let Some(format) = sniff_image(bytes) {
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
        return match ShownImage::decode(format, bytes.to_vec(), svg) {
            Some(image) => Kind::Image { image, size, name },
            None => unreadable(),
        };
    }
    if pdf::is_pdf(&bytes[..bytes.len().min(1024)]) {
        return match pdf::render(bytes, PDF_DRAW_WIDTH, PDF_PAGES) {
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
        };
    }
    // Judged by what is shown, and the rest of a character it may cut.
    if let Some(text) = text_prefix(&bytes[..bytes.len().min(TEXT_SHOWN + 3)]) {
        // An SVG only if it parses as one: a page or a source file may hold an icon's markup.
        // Looked for in the bytes: a character may straddle the 4,096th.
        if bytes[..bytes.len().min(4096)]
            .windows(4)
            .any(|window| window == b"<svg")
            && let Some(image) = ShownImage::decode(ImageFormat::Svg, bytes.to_vec(), svg)
        {
            return Kind::Image {
                image,
                size: None,
                name: "SVG",
            };
        }
        let mut end = text.len().min(TEXT_SHOWN);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        return Kind::Text(text[..end].to_owned(), end < bytes.len());
    }
    Kind::Bytes(hex_dump_lines(&bytes[..bytes.len().min(HEX_MAX)], 16))
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
        Some(Self::key(&self.cell, self.snapshot.as_deref()).into())
    }

    fn serialize(&self, _cx: &App) -> Option<SavedItem> {
        Some(SavedItem {
            kind: "blob".into(),
            data: json!({
                "type_name": self.cell.type_name,
                "id": self.cell.id,
                "property": self.cell.property,
                "edge": self.cell.edge,
                "at_commit": self.snapshot,
            }),
        })
    }

    /// The value's kind, size and commit; zoom for an image or a PDF; Save and Open with.
    fn footer(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Option<ItemFooter> {
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
        Some(ItemFooter { start, end, keys })
    }
}

impl Render for BlobView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self._load.is_none() {
            self._load = Some(Self::load(
                &self.session,
                &self.cell,
                &self.snapshot,
                window,
                cx,
            ));
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
                studio_ui::ds::empty_state(IconName::FileGeneric, "No value").into_any_element()
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
        div()
            .key_context(context)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::actual_size))
            .on_action(cx.listener(Self::fit))
            .size_full()
            .bg(studio_ui::ds::surface::document(cx))
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
                let ok = || Response::builder().header(graph_client::CONTRACT_HEADER, "0.13");
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
        let (view, cx) =
            cx.add_window_view(|window, cx| BlobView::new(session, cell, None, window, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(matches!(view.content, Loadable::Loaded(Content::TooLarge)));
            assert!(view.file_name().ends_with(".png"), "{}", view.file_name());
        });
        assert_eq!(*ranges.lock().unwrap(), ["bytes=0-1023"]);
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

//! Blob cells (spec 004): what a cell says ("Blob · 2.1 KB · Preview"), and the preview the peek
//! draws from a value's first bytes. A Blob is never projected by a query; each cell is read on
//! its own through the server's Blob route, at the commit the row was read at.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    App, AppContext as _, AsyncApp, Image, ImageFormat, ImageSource, RenderImage, SvgRenderer,
    WeakEntity,
};
use graph_client::{BlobCell, BlobMeta};
use graph_store::GraphSession;

/// How much of a value the peek reads to decide how to show it: the text it shows, and the
/// rest of a character that text may cut.
pub const PREVIEW_PREFIX: u64 = TEXT_SHOWN as u64 + 3;
/// The largest image the peek reads whole to draw it.
pub const IMAGE_MAX: u64 = 8 * 1024 * 1024;
/// How much text the peek shows.
const TEXT_SHOWN: usize = 4 * 1024;
/// How many bytes a hex preview shows, eight a line so a line fits the inspector.
const HEX_SHOWN: usize = 128;
const HEX_LINE: usize = 8;

/// A decoded image Studio holds, so it can free it. GPUI keeps an `Image` it decoded in its
/// asset cache until asked, and its textures in every window that drew it: a peek moving
/// through rows of photos would otherwise keep every one (an 8 MB JPEG decodes to about
/// 100 MB).
#[derive(Clone)]
pub enum ShownImage {
    /// Decoded off the main thread, drawn as is.
    Raster(Arc<RenderImage>),
    /// An SVG, which GPUI draws at the size it is shown.
    Svg(Arc<Image>),
}

impl std::fmt::Debug for ShownImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShownImage::Raster(_) => f.write_str("ShownImage::Raster"),
            ShownImage::Svg(_) => f.write_str("ShownImage::Svg"),
        }
    }
}

impl ShownImage {
    /// Decodes an image of `format`; call it from a background task. `None` when the bytes do
    /// not decode.
    pub fn decode(format: ImageFormat, bytes: Vec<u8>, svg: &SvgRenderer) -> Option<Self> {
        // GPUI draws an SVG as it is shown: parsed here, so text that is no SVG is not one.
        if format == ImageFormat::Svg {
            svg.parse_svg(&bytes).ok()?;
            return Some(ShownImage::Svg(Arc::new(Image::from_bytes(format, bytes))));
        }
        let image = Image::from_bytes(format, bytes);
        image
            .to_image_data(svg.clone())
            .ok()
            .map(ShownImage::Raster)
    }

    /// An image of pixels already drawn, RGBA row by row from the top; call it from a
    /// background task. `None` when they do not fill `width` by `height`.
    pub fn from_rgba(width: u32, height: u32, mut rgba: Vec<u8>) -> Option<Self> {
        // What GPUI draws is BGRA, as its own decoding leaves it.
        for pixel in rgba.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        let frame = image::Frame::new(image::RgbaImage::from_raw(width, height, rgba)?);
        Some(ShownImage::Raster(Arc::new(RenderImage::new(vec![frame]))))
    }

    pub fn source(&self) -> ImageSource {
        match self {
            ShownImage::Raster(image) => ImageSource::Render(image.clone()),
            ShownImage::Svg(image) => ImageSource::Image(image.clone()),
        }
    }

    /// Frees the decoded pixels and every window's texture of the image.
    pub fn release(&self, cx: &mut App) {
        match self {
            ShownImage::Raster(image) => cx.drop_image(image.clone(), None),
            ShownImage::Svg(image) => image.clone().remove_asset(cx),
        }
    }
}

/// What the peek shows for a managed value.
#[derive(Debug, Clone)]
pub enum Preview {
    Empty,
    Image(ShownImage),
    /// The start of a UTF-8 value, and how many bytes are not shown.
    Text {
        text: String,
        more: u64,
    },
    /// The first bytes of any other value, as a hex dump.
    Bytes {
        hex: String,
        more: u64,
    },
}

/// The image format a value's first bytes announce.
pub fn sniff_image(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageFormat::Png)
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(ImageFormat::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(ImageFormat::Gif)
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(ImageFormat::Webp)
    } else {
        None
    }
}

/// The text a value's first bytes are, when they read as text: UTF-8 up to a character their
/// end may cut, with no NUL. A value is judged by the bytes shown of it, so an invalid byte
/// past them does not make the whole a hex dump.
pub fn text_prefix(bytes: &[u8]) -> Option<&str> {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) if error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).ok()?
        }
        Err(_) => return None,
    };
    (!text.contains('\0')).then_some(text)
}

/// The file name a saved value is offered under: the row and the property, and the extension
/// its first bytes announce (`.txt` for text, `.pdf`, `.bin` otherwise).
pub fn suggested_file_name(id: &str, property: &str, prefix: &[u8]) -> String {
    let safe = |part: &str| -> String {
        part.chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect()
    };
    let extension = match sniff_image(prefix) {
        Some(ImageFormat::Png) => "png",
        Some(ImageFormat::Jpeg) => "jpg",
        Some(ImageFormat::Gif) => "gif",
        Some(ImageFormat::Webp) => "webp",
        _ if crate::pdf::is_pdf(prefix) => "pdf",
        _ if text_prefix(prefix).is_some_and(|text| !text.is_empty()) => "txt",
        _ => "bin",
    };
    format!("{}-{}.{extension}", safe(id), safe(property))
}

/// A value being saved to a file: written to a `.part` file of its own beside it, range by
/// range, and moved to the chosen name only when whole. A save stopped any way, a closed tab
/// included, removes its part file, and two saves to one name never share one.
pub struct RangeSave {
    path: std::path::PathBuf,
    part: std::path::PathBuf,
    size: u64,
    written: u64,
    /// Moved into place: nothing to remove.
    placed: bool,
}

impl RangeSave {
    pub fn begin(path: std::path::PathBuf, size: u64) -> std::io::Result<Self> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let mut part = path.clone().into_os_string();
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        part.push(format!(".{}-{n}.part", std::process::id()));
        let part = std::path::PathBuf::from(part);
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part)?;
        Ok(Self {
            path,
            part,
            size,
            written: 0,
            placed: false,
        })
    }

    pub fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write as _;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&self.part)?
            .write_all(bytes)?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    /// Moves the file into place, or removes it when the value came back short.
    pub fn finish(mut self) -> Result<std::path::PathBuf, String> {
        if self.written != self.size {
            return Err("The value ended before its size; nothing was saved.".into());
        }
        std::fs::rename(&self.part, &self.path)
            .map_err(|error| format!("Could not write: {error}"))?;
        self.placed = true;
        Ok(self.path.clone())
    }

    /// Stops the save; its part file goes.
    pub fn abandon(self) {}
}

impl Drop for RangeSave {
    fn drop(&mut self) {
        if !self.placed {
            std::fs::remove_file(&self.part).ok();
        }
    }
}

/// The folder a save of a value is offered in: Downloads, else the system's temporary folder.
pub fn save_folder() -> PathBuf {
    std::env::home_dir()
        .map(|home| home.join("Downloads"))
        .filter(|dir| dir.is_dir())
        .unwrap_or_else(std::env::temp_dir)
}

/// The bytes of a managed value of `size` bytes from `from` on, at `snapshot`, as much as one
/// read gives; `None` once the value ends.
pub async fn read_range(
    session: &WeakEntity<GraphSession>,
    cell: &BlobCell,
    snapshot: &Option<String>,
    from: u64,
    size: u64,
    cx: &mut AsyncApp,
) -> Result<Option<Vec<u8>>, String> {
    let read = session
        .update(cx, |session, cx| {
            session.blob_bytes(cell.clone(), snapshot.clone(), from..size, cx)
        })
        .map_err(|_| "The graph closed.".to_owned())?;
    match read.await {
        Ok(chunk) if !chunk.is_empty() => Ok(Some(chunk)),
        Ok(_) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

/// Reads a managed value whole, in ranges at `snapshot`, telling `progress` how far it got.
pub async fn read_value(
    session: WeakEntity<GraphSession>,
    cell: BlobCell,
    snapshot: Option<String>,
    size: u64,
    cx: &mut AsyncApp,
    mut progress: impl FnMut(u64, &mut AsyncApp),
) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::with_capacity(size as usize);
    while (bytes.len() as u64) < size {
        let from = bytes.len() as u64;
        match read_range(&session, &cell, &snapshot, from, size, cx).await? {
            Some(chunk) => bytes.extend(chunk),
            None => break,
        }
        progress(bytes.len() as u64, cx);
    }
    if bytes.len() as u64 != size {
        return Err("The value ended before its size.".into());
    }
    Ok(bytes)
}

/// Saves a managed value to `path` without holding it whole: each range read at `snapshot` is
/// written off the main thread to a [`RangeSave`], moved into place when complete.
pub async fn save_value(
    session: WeakEntity<GraphSession>,
    cell: BlobCell,
    snapshot: Option<String>,
    size: u64,
    path: PathBuf,
    cx: &mut AsyncApp,
    mut progress: impl FnMut(u64, &mut AsyncApp),
) -> Result<PathBuf, String> {
    let mut save = cx
        .background_spawn(async move { RangeSave::begin(path, size) })
        .await
        .map_err(|error| format!("Could not write: {error}"))?;
    while save.written() < size {
        let from = save.written();
        let chunk = match read_range(&session, &cell, &snapshot, from, size, cx).await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) => {
                save.abandon();
                return Err(error);
            }
        };
        let (returned, written) = cx
            .background_spawn(async move {
                let written = save.append(&chunk);
                (save, written)
            })
            .await;
        save = returned;
        if let Err(error) = written {
            save.abandon();
            return Err(format!("Could not write: {error}"));
        }
        progress(save.written(), cx);
    }
    cx.background_spawn(async move { save.finish() }).await
}

/// How many bytes to read for a preview of a value of `size` bytes whose first bytes are
/// `prefix`: the whole value for an image that fits, else nothing more.
pub fn bytes_wanted(prefix: &[u8], size: u64) -> u64 {
    if sniff_image(prefix).is_some() && size <= IMAGE_MAX {
        size
    } else {
        prefix.len() as u64
    }
}

/// The preview of a value of `size` bytes, from its first `bytes` (all of them for an image).
/// An image is decoded here, so call it from a background task.
pub fn preview(bytes: Vec<u8>, size: u64, svg: &SvgRenderer) -> Preview {
    if size == 0 {
        return Preview::Empty;
    }
    if let Some(format) = sniff_image(&bytes)
        && bytes.len() as u64 == size
        && let Some(image) = ShownImage::decode(format, bytes.clone(), svg)
    {
        return Preview::Image(image);
    }
    if let Some(text) = text_prefix(&bytes) {
        let mut end = text.len().min(TEXT_SHOWN);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        return Preview::Text {
            text: text[..end].to_owned(),
            more: size - end as u64,
        };
    }
    let shown = bytes.len().min(HEX_SHOWN);
    Preview::Bytes {
        hex: hex_dump(&bytes[..shown]),
        more: size - shown as u64,
    }
}

/// Eight bytes a line: offset, hex, and the printable ASCII.
pub fn hex_dump(bytes: &[u8]) -> String {
    hex_dump_lines(bytes, HEX_LINE)
}

/// [`hex_dump`] with `per_line` bytes a line: eight fit the inspector, sixteen a tab.
pub fn hex_dump_lines(bytes: &[u8], per_line: usize) -> String {
    bytes
        .chunks(per_line)
        .enumerate()
        .map(|(line, chunk)| {
            let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
            let ascii: String = chunk
                .iter()
                .map(|b| {
                    if b.is_ascii_graphic() || *b == b' ' {
                        *b as char
                    } else {
                        '.'
                    }
                })
                .collect();
            format!(
                "{:04x}  {:<width$}  {ascii}",
                line * per_line,
                hex.join(" "),
                width = per_line * 3 - 1
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// What a Blob cell says once its metadata is read.
pub fn cell_label(meta: &BlobMeta) -> String {
    match meta {
        BlobMeta::Null => "—".into(),
        BlobMeta::Managed { size, .. } => format!("Blob · {}", studio_ui::format_bytes(*size)),
        BlobMeta::External { uri } => format!("External · {}", host(uri)),
    }
}

/// The host of a URI, or the URI when it has none (`s3://bucket/key` → `bucket`).
pub fn host(uri: &str) -> &str {
    uri.split_once("://")
        .map(|(_, rest)| rest.split('/').next().unwrap_or(rest))
        .filter(|host| !host.is_empty())
        .unwrap_or(uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A renderer for decoding in tests; no asset is ever read.
    fn svg() -> SvgRenderer {
        SvgRenderer::new(Arc::new(()))
    }

    #[test]
    fn a_save_lands_whole_or_not_at_all() {
        let dir = std::env::temp_dir().join(format!("studio-blob-save-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("value.bin");
        let parts = || {
            std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
                .filter(|name| name.ends_with(".part"))
                .count()
        };

        let mut save = RangeSave::begin(path.clone(), 6).unwrap();
        save.append(b"abc").unwrap();
        assert!(
            parts() == 1 && !path.exists(),
            "nothing under the name until whole"
        );
        // A second save to the name writes a part of its own.
        let other = RangeSave::begin(path.clone(), 6).unwrap();
        assert_eq!(parts(), 2);
        drop(other);
        assert_eq!(parts(), 1, "a dropped save removes its part");
        save.append(b"def").unwrap();
        assert_eq!(save.finish().unwrap(), path);
        assert_eq!(std::fs::read(&path).unwrap(), b"abcdef");
        assert_eq!(parts(), 0);

        // A short value, or a stopped save, leaves neither file.
        std::fs::remove_file(&path).unwrap();
        let mut short = RangeSave::begin(path.clone(), 6).unwrap();
        short.append(b"abc").unwrap();
        assert!(short.finish().is_err());
        assert!(!path.exists() && parts() == 0);
        RangeSave::begin(path.clone(), 6).unwrap().abandon();
        assert_eq!(parts(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_saved_value_is_named_for_its_row_property_and_kind() {
        assert_eq!(
            suggested_file_name("doc/1", "body", b"\x89PNG\r\n\x1a\nrest"),
            "doc-1-body.png"
        );
        assert_eq!(suggested_file_name("n1", "notes", b"hello"), "n1-notes.txt");
        assert_eq!(
            suggested_file_name("n1", "raw", &[0xff, 0x00, 0x13]),
            "n1-raw.bin"
        );
        // Text whose first bytes cut a character, and a PDF.
        let cut = &"日本語のテキスト".as_bytes()[..16];
        assert_eq!(suggested_file_name("n1", "notes", cut), "n1-notes.txt");
        assert_eq!(
            suggested_file_name("n1", "scan", b"%PDF-1.7\n%"),
            "n1-scan.pdf"
        );
    }

    /// Text that holds an SVG's markup is no SVG unless it parses as one.
    #[test]
    fn only_an_svg_decodes_as_one() {
        let page = b"<html><body><svg viewBox=\"0 0 1 1\"></svg><p>hi</body></html>".to_vec();
        assert!(ShownImage::decode(ImageFormat::Svg, page, &svg()).is_none());
        let icon = b"<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 1 1\"/>".to_vec();
        assert!(ShownImage::decode(ImageFormat::Svg, icon, &svg()).is_some());
    }

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    #[test]
    fn images_are_read_whole_when_they_fit() {
        assert_eq!(sniff_image(PNG), Some(ImageFormat::Png));
        assert_eq!(
            sniff_image(b"RIFF\0\0\0\0WEBPVP8 "),
            Some(ImageFormat::Webp)
        );
        assert_eq!(sniff_image(b"hello"), None);
        assert_eq!(bytes_wanted(PNG, 690), 690);
        assert_eq!(bytes_wanted(PNG, IMAGE_MAX + 1), PNG.len() as u64);
        assert_eq!(bytes_wanted(b"text", 9_000_000), 4);
        let png = {
            let mut png = Vec::new();
            image::RgbaImage::new(3, 2)
                .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .unwrap();
            png
        };
        assert!(matches!(
            preview(png.clone(), png.len() as u64, &svg()),
            Preview::Image(ShownImage::Raster(_))
        ));
        assert!(
            matches!(preview(png, 5_000, &svg()), Preview::Bytes { .. }),
            "a prefix of an image is not drawn"
        );
        assert!(
            matches!(
                preview(PNG.to_vec(), PNG.len() as u64, &svg()),
                Preview::Bytes { .. }
            ),
            "an image that does not decode shows its bytes"
        );
    }

    #[test]
    fn text_shows_its_start_and_binary_a_hex_dump() {
        let Preview::Text { text, more } = preview(b"query q() {}".to_vec(), 12, &svg()) else {
            panic!("text");
        };
        assert_eq!((text.as_str(), more), ("query q() {}", 0));
        // A character cut by the prefix is dropped, not an error.
        let cut = "café".as_bytes()[..4].to_vec();
        let Preview::Text { text, more } = preview(cut, 100, &svg()) else {
            panic!("text with a cut character");
        };
        assert_eq!((text.as_str(), more), ("caf", 97));
        let Preview::Bytes { hex, more } = preview((0u8..=255).collect(), 1024, &svg()) else {
            panic!("bytes");
        };
        assert_eq!(more, 896);
        assert_eq!(
            hex.lines().next(),
            Some("0000  00 01 02 03 04 05 06 07  ........")
        );
        assert_eq!(
            hex.lines().nth(5),
            Some("0028  28 29 2a 2b 2c 2d 2e 2f  ()*+,-./")
        );
        assert_eq!(hex.lines().count(), 16);
        assert!(matches!(preview(Vec::new(), 0, &svg()), Preview::Empty));
    }

    #[test]
    fn cells_name_size_or_reference() {
        assert_eq!(
            cell_label(&BlobMeta::Managed {
                size: 2_140,
                etag: None
            }),
            "Blob · 2.1 KB"
        );
        assert_eq!(
            cell_label(&BlobMeta::External {
                uri: "s3://reports/2026/q3.pdf".into()
            }),
            "External · reports"
        );
        assert_eq!(cell_label(&BlobMeta::Null), "—");
        assert_eq!(host("not a uri"), "not a uri");
    }
}

//! What a gallery tile shows of a file (spec 035): its kind and size from its first bytes, and
//! a preview made off the main thread at most [`THUMB_EDGE`] pixels on its long side. Pure but
//! for the decoded image, which the gallery holds and frees; no reads.

use std::io::Cursor;

use gpui::{ImageFormat, SvgRenderer};

use crate::blob::{IMAGE_MAX, KIND_PREFIX, ShownImage, ValueKind, text_prefix, value_kind};

/// How much of a file a tile reads first: its kind, an image's header and a text's first lines
/// are in it, and most icons, diagrams and short texts are whole in it.
pub const FIRST_READ: u64 = 64 * 1024;
/// The long side of a tile's preview in pixels: twice the large tile, so it is sharp on a 2x
/// display at every tile size and a size change needs no read.
pub const THUMB_EDGE: u32 = (studio_ui::ds::size::TILE_LARGE * 2.) as u32;
/// The most pixels an image may have to be decoded for a tile: past it, decoding would hold
/// hundreds of megabytes for a moment, and the tile shows its kind and size.
pub const MAX_PIXELS: u64 = 40_000_000;
/// How much of a text a tile shows.
const TEXT_SHOWN: usize = 600;
const TEXT_LINES: usize = 12;

/// What a file's first bytes say of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    pub kind: ValueKind,
    /// An image's pixel size, from its header.
    pub pixels: Option<(u32, u32)>,
}

impl Facts {
    /// What a file whose first bytes are `prefix` is.
    pub fn of(prefix: &[u8]) -> Self {
        let kind = value_kind(prefix);
        let pixels = match kind {
            ValueKind::Image(format) => image_size(format, prefix),
            _ => None,
        };
        Self { kind, pixels }
    }

    /// The kind in a word, as a tile's meta line starts.
    pub fn kind_word(&self) -> &'static str {
        kind_word(self.kind)
    }
}

/// A kind in a word.
pub fn kind_word(kind: ValueKind) -> &'static str {
    match kind {
        ValueKind::Image(ImageFormat::Png) => "PNG",
        ValueKind::Image(ImageFormat::Jpeg) => "JPEG",
        ValueKind::Image(ImageFormat::Gif) => "GIF",
        ValueKind::Image(ImageFormat::Webp) => "WebP",
        ValueKind::Image(_) => "Image",
        ValueKind::Svg => "SVG",
        ValueKind::Pdf => "PDF",
        ValueKind::Text => "Text",
        ValueKind::Binary => "Bytes",
    }
}

/// An image's pixel size from its header, when the bytes hold it.
fn image_size(format: ImageFormat, bytes: &[u8]) -> Option<(u32, u32)> {
    let format = match format {
        ImageFormat::Png => image::ImageFormat::Png,
        ImageFormat::Jpeg => image::ImageFormat::Jpeg,
        ImageFormat::Gif => image::ImageFormat::Gif,
        ImageFormat::Webp => image::ImageFormat::WebP,
        _ => return None,
    };
    image::ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .ok()
}

/// What to read past the first bytes of a file of `size` bytes: `true` when the whole is read
/// to draw it (an image, an SVG or a PDF of at most [`IMAGE_MAX`], an image of at most
/// [`MAX_PIXELS`]).
pub fn draws_whole(facts: &Facts, size: u64) -> bool {
    let drawn = matches!(
        facts.kind,
        ValueKind::Image(_) | ValueKind::Svg | ValueKind::Pdf
    );
    let pixels_fit = facts
        .pixels
        .is_none_or(|(w, h)| u64::from(w) * u64::from(h) <= MAX_PIXELS);
    drawn && size <= IMAGE_MAX && pixels_fit
}

/// A tile's preview, made from what was read of its file.
#[derive(Debug, Clone)]
pub enum Look {
    /// Drawn: decoded and scaled, held until freed. `bytes` is what it holds.
    Image {
        image: ShownImage,
        bytes: usize,
    },
    /// The first lines of a text.
    Text(String),
    /// The kind's icon alone: bytes, a PDF where pages are not drawn, a file too large to draw,
    /// or one that did not decode.
    Icon,
    Empty,
}

impl Look {
    /// The bytes the look holds, against the gallery's cap.
    pub fn held(&self) -> usize {
        match self {
            Look::Image { bytes, .. } => *bytes,
            Look::Text(text) => text.len(),
            Look::Icon | Look::Empty => 0,
        }
    }
}

/// The preview of a file of `size` bytes whose first bytes, or all of them, are `bytes`; call it
/// from a background task, as it decodes. An image is scaled to [`THUMB_EDGE`] on its long side
/// and the bytes dropped; a PDF's first page drawn as wide; an SVG kept for GPUI to draw.
pub fn look(bytes: Vec<u8>, size: u64, svg: &SvgRenderer) -> Look {
    if size == 0 {
        return Look::Empty;
    }
    let facts = Facts::of(&bytes);
    let whole = bytes.len() as u64 == size;
    let drawn = match facts.kind {
        _ if !whole || !draws_whole(&facts, size) => None,
        ValueKind::Image(format) => thumbnail(format, &bytes),
        ValueKind::Svg => {
            let held = bytes.len();
            ShownImage::decode(ImageFormat::Svg, bytes.clone(), svg).map(|image| (image, held))
        }
        ValueKind::Pdf => crate::pdf::render(&bytes, THUMB_EDGE, 1)
            .ok()
            .and_then(|(pages, _)| pages.into_iter().next())
            .and_then(|page| {
                let held = page.rgba.len();
                ShownImage::from_rgba(page.width, page.height, page.rgba).map(|image| (image, held))
            }),
        ValueKind::Text | ValueKind::Binary => None,
    };
    if let Some((image, bytes)) = drawn {
        return Look::Image { image, bytes };
    }
    match facts.kind {
        ValueKind::Text | ValueKind::Svg => {
            match text_prefix(&bytes[..bytes.len().min(KIND_PREFIX)]) {
                Some(text) => Look::Text(first_lines(text)),
                None => Look::Icon,
            }
        }
        _ => Look::Icon,
    }
}

/// An image decoded and scaled to fit [`THUMB_EDGE`], as GPUI draws it, and the bytes it holds.
fn thumbnail(format: ImageFormat, bytes: &[u8]) -> Option<(ShownImage, usize)> {
    let format = match format {
        ImageFormat::Png => image::ImageFormat::Png,
        ImageFormat::Jpeg => image::ImageFormat::Jpeg,
        ImageFormat::Gif => image::ImageFormat::Gif,
        ImageFormat::Webp => image::ImageFormat::WebP,
        _ => return None,
    };
    let decoded = image::load_from_memory_with_format(bytes, format).ok()?;
    // Never larger than the image: a small icon is drawn at its own pixels.
    let scaled = if decoded.width() > THUMB_EDGE || decoded.height() > THUMB_EDGE {
        decoded.thumbnail(THUMB_EDGE, THUMB_EDGE)
    } else {
        decoded
    };
    let rgba = scaled.to_rgba8();
    let (width, height) = rgba.dimensions();
    let rgba = rgba.into_raw();
    let held = rgba.len();
    ShownImage::from_rgba(width, height, rgba).map(|image| (image, held))
}

/// A text's first lines, as a tile shows them: at most [`TEXT_LINES`] lines and
/// [`TEXT_SHOWN`] bytes, cut on a character's edge.
fn first_lines(text: &str) -> String {
    let end = text.floor_char_boundary(TEXT_SHOWN);
    text[..end]
        .lines()
        .take(TEXT_LINES)
        .collect::<Vec<_>>()
        .join("\n")
}

/// A tile's meta line for a managed file: its kind, an image's pixel size and its size
/// ("PNG · 640 × 400 · 2.6 KB"); past what a tile draws, "open to see it".
pub fn meta_line(facts: Option<&Facts>, size: u64) -> String {
    let size_text = studio_ui::format_bytes(size);
    // An empty value has no kind: its preview says it is empty.
    let Some(facts) = facts.filter(|_| size > 0) else {
        return size_text;
    };
    let mut parts = vec![facts.kind_word().to_owned()];
    if let Some((w, h)) = facts.pixels {
        parts.push(format!("{w} × {h}"));
    }
    parts.push(size_text);
    let drawn = matches!(
        facts.kind,
        ValueKind::Image(_) | ValueKind::Svg | ValueKind::Pdf
    );
    if drawn && !draws_whole(facts, size) {
        parts.push("open to see it".to_owned());
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn svg() -> SvgRenderer {
        SvgRenderer::new(Arc::new(()))
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::RgbaImage::from_pixel(width, height, image::Rgba([200, 40, 40, 255]))
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes
    }

    /// An image's size is read from its first bytes, before it is decoded, and an image is
    /// held at most [`THUMB_EDGE`] on its long side: a large picture costs a tile under a
    /// megabyte, a small one its own pixels.
    #[test]
    fn an_image_is_judged_by_its_header_and_held_small() {
        let large = png(1200, 600);
        let facts = Facts::of(&large[..64]);
        assert_eq!(facts.kind, ValueKind::Image(ImageFormat::Png));
        assert_eq!(facts.pixels, Some((1200, 600)));
        let size = large.len() as u64;
        match look(large, size, &svg()) {
            Look::Image { bytes, .. } => assert_eq!(bytes, 480 * 240 * 4),
            other => panic!("{other:?}"),
        }
        let small = png(40, 20);
        let size = small.len() as u64;
        match look(small, size, &svg()) {
            Look::Image { bytes, .. } => assert_eq!(bytes, 40 * 20 * 4),
            other => panic!("{other:?}"),
        }
    }

    /// Only the first bytes of a drawn kind show its icon, not a broken image; text shows its
    /// first lines; bytes and an empty value say what they are.
    #[test]
    fn a_tile_shows_what_was_read_for_its_kind() {
        let picture = png(300, 300);
        let size = picture.len() as u64;
        assert!(matches!(
            look(picture[..100].to_vec(), size, &svg()),
            Look::Icon
        ));
        let text: String = (0..40).map(|n| format!("line {n}\n")).collect();
        match look(text.clone().into_bytes(), text.len() as u64, &svg()) {
            Look::Text(shown) => {
                assert_eq!(shown.lines().count(), TEXT_LINES);
                assert!(shown.starts_with("line 0\nline 1"));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(look(vec![0, 1, 2, 255], 4, &svg()), Look::Icon));
        assert!(matches!(look(Vec::new(), 0, &svg()), Look::Empty));
        let drawing = br#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"/>"#;
        assert!(matches!(
            look(drawing.to_vec(), drawing.len() as u64, &svg()),
            Look::Image { .. }
        ));
    }

    /// A file too large to draw is not read whole, and its meta line says to open it.
    #[test]
    fn a_file_past_the_caps_is_named_not_drawn() {
        let facts = Facts {
            kind: ValueKind::Image(ImageFormat::Jpeg),
            pixels: Some((3024, 4032)),
        };
        assert!(draws_whole(&facts, 2_400_000));
        assert!(!draws_whole(&facts, IMAGE_MAX + 1));
        let huge = Facts {
            pixels: Some((10_000, 10_000)),
            ..facts
        };
        assert!(!draws_whole(&huge, 1_000_000));
        assert_eq!(
            meta_line(Some(&facts), 2_400_000),
            format!(
                "JPEG · 3024 × 4032 · {}",
                studio_ui::format_bytes(2_400_000)
            )
        );
        assert!(meta_line(Some(&huge), 1_000_000).ends_with("open to see it"));
        let text = Facts::of(b"hello");
        assert!(!draws_whole(&text, 5));
        assert_eq!(meta_line(Some(&text), 5), "Text · 5 B");
        assert_eq!(meta_line(Some(&Facts::of(b"")), 0), "0 B");
    }
}

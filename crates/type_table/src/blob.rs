//! Blob cells (spec 004): what a cell says ("Blob · 2.1 KB · Preview"), and the preview the peek
//! draws from a value's first bytes. A Blob is never projected by a query; each cell is read on
//! its own through the server's Blob route, at the commit the row was read at.

use std::sync::Arc;

use gpui::{Image, ImageFormat};
use graph_client::BlobMeta;

/// How much of a value the peek reads to decide how to show it.
pub const PREVIEW_PREFIX: u64 = 64 * 1024;
/// The largest image the peek reads whole to draw it.
pub const IMAGE_MAX: u64 = 8 * 1024 * 1024;
/// How much text the peek shows.
const TEXT_SHOWN: usize = 4 * 1024;
/// How many bytes a hex preview shows, eight a line so a line fits the inspector.
const HEX_SHOWN: usize = 128;
const HEX_LINE: usize = 8;

/// What the peek shows for a managed value.
#[derive(Debug, Clone)]
pub enum Preview {
    Empty,
    Image(Arc<Image>),
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
pub fn preview(bytes: Vec<u8>, size: u64) -> Preview {
    if size == 0 {
        return Preview::Empty;
    }
    if let Some(format) = sniff_image(&bytes)
        && bytes.len() as u64 == size
    {
        return Preview::Image(Arc::new(Image::from_bytes(format, bytes)));
    }
    // Text, allowing for a character cut at the end of the prefix.
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => Some(text),
        Err(error) if error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).ok()
        }
        Err(_) => None,
    };
    if let Some(text) = text.filter(|text| !text.contains('\0')) {
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
    bytes
        .chunks(HEX_LINE)
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
                line * HEX_LINE,
                hex.join(" "),
                width = HEX_LINE * 3 - 1
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
        assert!(matches!(
            preview(PNG.to_vec(), PNG.len() as u64),
            Preview::Image(_)
        ));
        assert!(
            matches!(preview(PNG.to_vec(), 5_000), Preview::Bytes { .. }),
            "a prefix of an image is not drawn"
        );
    }

    #[test]
    fn text_shows_its_start_and_binary_a_hex_dump() {
        let Preview::Text { text, more } = preview(b"query q() {}".to_vec(), 12) else {
            panic!("text");
        };
        assert_eq!((text.as_str(), more), ("query q() {}", 0));
        // A character cut by the prefix is dropped, not an error.
        let cut = "café".as_bytes()[..4].to_vec();
        let Preview::Text { text, more } = preview(cut, 100) else {
            panic!("text with a cut character");
        };
        assert_eq!((text.as_str(), more), ("caf", 97));
        let Preview::Bytes { hex, more } = preview((0u8..=255).collect(), 1024) else {
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
        assert!(matches!(preview(Vec::new(), 0), Preview::Empty));
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

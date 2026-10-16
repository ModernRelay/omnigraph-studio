//! Text values read as markdown (spec 014): every String value is parsed as CommonMark, with
//! the parser Zed's `markdown` renders with. Plain text has no markup, so it reads as itself; a
//! value with markup, or with more than one line, is rendered. The value itself is never
//! rewritten.

use pulldown_cmark::{Event, Parser, Tag, TagEnd};

/// Whether `text` reads as itself: one paragraph of text with no markup and no line break. Such
/// a value draws as a plain label; any other is rendered as markdown.
pub fn is_plain(text: &str) -> bool {
    if text.contains('\n') {
        return false;
    }
    let mut events = Parser::new(text);
    match events.next() {
        None => true,
        Some(Event::Start(Tag::Paragraph)) => {
            for event in events {
                match event {
                    Event::Text(_) => {}
                    Event::End(TagEnd::Paragraph) => {}
                    _ => return false,
                }
            }
            true
        }
        Some(_) => false,
    }
}

/// The value's first line as plain text, its markup removed, with "…" when more follows: what
/// a table cell shows of it.
pub fn first_line(text: &str) -> String {
    if is_plain(text) {
        return text.to_owned();
    }
    let mut line = String::new();
    let (mut ended, mut more) = (false, false);
    for event in Parser::new(text) {
        match event {
            Event::Text(part) | Event::Code(part) if ended => {
                if !part.trim().is_empty() {
                    more = true;
                    break;
                }
            }
            Event::Text(part) | Event::Code(part) => line.push_str(&part),
            Event::SoftBreak
            | Event::HardBreak
            | Event::End(
                TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item | TagEnd::CodeBlock,
            ) => ended = !line.trim().is_empty(),
            _ => {}
        }
    }
    let mut line = line.trim().to_owned();
    if more {
        line.push('…');
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_reads_as_itself_and_markup_is_rendered() {
        for plain in [
            "Ada Lovelace",
            "",
            "graphs, systems, web",
            "foo_bar_baz",
            "3 < 4",
        ] {
            assert!(is_plain(plain), "{plain:?} is plain");
        }
        for rich in [
            "# Notes",
            "**bold** move",
            "- one\n- two",
            "first line\nsecond line",
            "see [docs](https://example.com)",
            "`code`",
            "> quoted",
        ] {
            assert!(!is_plain(rich), "{rich:?} has markup");
        }
    }

    #[test]
    fn a_cell_shows_the_first_line_without_its_markup() {
        assert_eq!(first_line("Ada Lovelace"), "Ada Lovelace");
        assert_eq!(first_line("# Notes\n\nMore below"), "Notes…");
        assert_eq!(first_line("**bold** move"), "bold move");
        assert_eq!(first_line("first line\nsecond line"), "first line…");
        assert_eq!(first_line("- one\n- two"), "one…");
        assert_eq!(first_line("see [docs](https://example.com)"), "see docs");
    }
}

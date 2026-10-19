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

/// `text` cut to at most `chars` characters, the last of them "…" when it was cut, with no space
/// left before it: a title or a name where room is short.
pub fn cut(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        text.to_owned()
    } else {
        let kept: String = text.chars().take(chars.saturating_sub(1)).collect();
        format!("{}…", kept.trim_end())
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
    'events: for event in Parser::new(text) {
        match event {
            // One text can hold several lines (a code block's); the line ends at the first.
            Event::Text(part) | Event::Code(part) => {
                let mut pieces = part.split('\n').peekable();
                while let Some(piece) = pieces.next() {
                    if ended {
                        if !piece.trim().is_empty() {
                            more = true;
                            break 'events;
                        }
                        continue;
                    }
                    line.push_str(piece);
                    if pieces.peek().is_some() {
                        ended = !line.trim().is_empty();
                    }
                }
            }
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

/// Whether text held in pieces, `len` bytes in all (an editor's buffer, chunk by chunk), is
/// `text`: compared where it lies, so a keystroke copies none of it.
pub fn pieces_are<'a>(len: usize, pieces: impl IntoIterator<Item = &'a str>, text: &str) -> bool {
    if len != text.len() {
        return false;
    }
    let mut rest = text;
    for piece in pieces {
        match rest.strip_prefix(piece) {
            Some(after) => rest = after,
            None => return false,
        }
    }
    rest.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_text_is_cut_on_a_character() {
        assert_eq!(cut("Claude Sonnet", 32), "Claude Sonnet");
        let long = "Claude Sonnet with an extended thinking budget";
        // No space is left before the ellipsis.
        assert_eq!(cut(long, 32), "Claude Sonnet with an extended…");
        assert_eq!(cut("模型模型模型", 4), "模型模…");
    }

    #[test]
    fn pieces_are_compared_with_a_text_in_place() {
        assert!(pieces_are(11, ["hello", " ", "world"], "hello world"));
        assert!(!pieces_are(11, ["hello", " ", "worle"], "hello world"));
        assert!(
            !pieces_are(5, ["hello"], "hello world"),
            "a length that differs"
        );
        assert!(pieces_are(0, std::iter::empty(), ""));
    }

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
        assert_eq!(
            first_line("```\nlet a = 1;\nlet b = 2;\n```"),
            "let a = 1;…"
        );
        assert_eq!(first_line("```\nlet a = 1;\n```"), "let a = 1;");
    }
}

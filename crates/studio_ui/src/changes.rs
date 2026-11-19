//! Entity change cards shared by review and history (spec 008, 009): one card per changed
//! entity, its changed properties before and after, the relationship changes that came with
//! it, and per-type counts.

use std::rc::Rc;

use crate::ds::Tipped;
use crate::ds::{radii, space};
use gpui::{AnyElement, App, ClickEvent, Div, ElementId, SharedString, Window};
use serde_json::Value;
use ui::prelude::*;

use crate::format_value;

/// What happened to an entity, as a card shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Changed,
    Removed,
    /// An entity in a review only for the relationships it is an end of.
    Unchanged,
}

impl ChangeKind {
    pub fn symbol(self) -> &'static str {
        match self {
            ChangeKind::Added => "+",
            ChangeKind::Changed => "~",
            ChangeKind::Removed => "−",
            ChangeKind::Unchanged => "·",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ChangeKind::Added => "Added",
            ChangeKind::Changed => "Changed",
            ChangeKind::Removed => "Removed",
            ChangeKind::Unchanged => "Unchanged, with relationships that changed",
        }
    }

    pub fn color(self) -> Color {
        match self {
            ChangeKind::Added => Color::Created,
            ChangeKind::Changed => Color::Modified,
            ChangeKind::Removed => Color::Deleted,
            ChangeKind::Unchanged => Color::Muted,
        }
    }
}

/// One property row of a card: its name, each side's value as text (`None` when the side has
/// none), and how the change inside the value reads ([`Body`]). Made once for a change and
/// shared by every frame that draws it ([`PropertyRow::new`]): a card is drawn at every frame,
/// and the comparison takes time that grows with the text.
#[derive(Debug, Clone)]
pub struct PropertyRow {
    name: SharedString,
    before: Option<SharedString>,
    after: Option<SharedString>,
    body: Body,
}

/// How a row reads what changed.
#[derive(Debug, Clone)]
enum Body {
    /// Short values: before → after on one line.
    Inline,
    /// A list's gains and losses.
    List(Rc<[Piece]>),
    /// A text, line by line: the lines that changed with the words that changed, and the
    /// unchanged rest folded.
    Text(Rc<[DiffLine]>),
}

/// Characters of a value a row shows before it is cut, until the whole text is asked for: a
/// long text is neither copied whole nor measured whole at each frame.
const SHOWN_CHARS: usize = 2_000;
/// A value longer than this, or with a line break, is text: it reads line by line.
const INLINE_CHARS: usize = 72;
/// A line of text longer than this is read sentence by sentence.
const SENTENCE_LINE: usize = 160;
/// Lines (or sentences) a text difference compares at most on each side: the comparison
/// takes time that grows with the product of the counts. Past it the whole before and after
/// show.
const DIFF_SEGMENTS: usize = 600;

impl PropertyRow {
    pub fn new(
        name: impl Into<SharedString>,
        before: Option<&Value>,
        after: Option<&Value>,
    ) -> Self {
        let before_text = before.map(text_of);
        let after_text = after.map(text_of);
        let body = match (before, after) {
            (Some(b @ Value::Array(_)), Some(a @ Value::Array(_))) => match list_diff(b, a) {
                Some(pieces) => Body::List(pieces.into()),
                None => Body::Inline,
            },
            (Some(_), Some(_)) => {
                let (b, a) = (
                    before_text.as_deref().unwrap_or_default(),
                    after_text.as_deref().unwrap_or_default(),
                );
                match (is_text(b) || is_text(a))
                    .then(|| text_lines(b, a))
                    .flatten()
                {
                    Some(lines) => Body::Text(lines.into()),
                    None => Body::Inline,
                }
            }
            _ => Body::Inline,
        };
        Self {
            name: name.into(),
            before: before_text,
            after: after_text,
            body,
        }
    }

    pub fn name(&self) -> &SharedString {
        &self.name
    }

    /// Whether the row holds more than it shows until its whole text is asked for: a text
    /// past [`SHOWN_CHARS`], or lines folded away.
    pub fn has_more(&self) -> bool {
        match &self.body {
            Body::Text(lines) => lines.iter().any(|line| line.folded),
            _ => [&self.before, &self.after]
                .into_iter()
                .flatten()
                .any(|text| text.chars().count() > SHOWN_CHARS),
        }
    }
}

/// A value as text: a string as it is, anything else as a row formats it.
fn text_of(value: &Value) -> SharedString {
    match value {
        Value::String(text) => text.clone().into(),
        other => format_value(other).into(),
    }
}

fn is_text(text: &str) -> bool {
    text.contains('\n') || text.chars().count() > INLINE_CHARS
}

/// `text` cut to [`SHOWN_CHARS`], and how many characters were left out (0 when none).
fn cut(text: &str) -> (SharedString, usize) {
    let chars = text.chars().count();
    if chars <= SHOWN_CHARS {
        return (text.into(), 0);
    }
    let kept: String = text.chars().take(SHOWN_CHARS).collect();
    (kept.trim_end().to_owned().into(), chars - SHOWN_CHARS)
}

/// One line of a text difference: a line (or sentence) kept, removed or added, with the
/// words that changed in it when it stands against one line on the other side.
#[derive(Debug, Clone)]
pub struct DiffLine {
    kind: PieceKind,
    text: SharedString,
    /// For a changed pair, the words of both sides: each side shows its own with the kept.
    pieces: Option<Rc<[Piece]>>,
    /// A kept line away from any change, shown only with the whole text.
    folded: bool,
}

/// `text` as lines, a line past [`SENTENCE_LINE`] as its sentences.
fn segments(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for line in text.lines() {
        // A blank line is spacing, not a line to show gained or lost.
        if line.trim().is_empty() {
            continue;
        }
        if line.chars().count() <= SENTENCE_LINE {
            out.push(line);
            continue;
        }
        let bytes = line.as_bytes();
        let (mut start, mut at) = (0, 0);
        while at + 1 < bytes.len() {
            // A sentence ends at its mark and the space after it, both ASCII, so the cut
            // falls on a character boundary.
            if matches!(bytes[at], b'.' | b'!' | b'?' | b';') && bytes[at + 1] == b' ' {
                out.push(&line[start..=at]);
                start = at + 2;
                at += 2;
            } else {
                at += 1;
            }
        }
        if start < line.len() {
            out.push(&line[start..]);
        }
    }
    out
}

/// The lines of `before` against `after`: kept, removed and added, a removed line against the
/// added line that took its place showing the words that changed, and kept lines away from
/// any change folded, one left beside each change. `None` for the same text.
fn text_lines(before: &str, after: &str) -> Option<Vec<DiffLine>> {
    if before == after {
        return None;
    }
    let old = segments(before);
    let new = segments(after);
    let line = |kind, text: &str| DiffLine {
        kind,
        text: text.to_owned().into(),
        pieces: None,
        folded: false,
    };
    if old.len() > DIFF_SEGMENTS || new.len() > DIFF_SEGMENTS {
        return Some(vec![
            line(PieceKind::Removed, before),
            line(PieceKind::Added, after),
        ]);
    }
    let ops = lcs_ops(&old, &new, |a, b| {
        a.split_whitespace().eq(b.split_whitespace())
    });
    if !ops.iter().any(|(kind, _)| *kind != PieceKind::Same) {
        // The same words spaced otherwise: the two texts, whole.
        return Some(vec![
            line(PieceKind::Removed, before),
            line(PieceKind::Added, after),
        ]);
    }
    let mut lines: Vec<DiffLine> = ops.iter().map(|(kind, text)| line(*kind, text)).collect();
    // A run of removed lines before a run of added ones: each pair shows its words.
    let mut at = 0;
    while at < lines.len() {
        if lines[at].kind != PieceKind::Removed {
            at += 1;
            continue;
        }
        let removed_end = at + run_len(&lines[at..], PieceKind::Removed);
        let added_end = removed_end + run_len(&lines[removed_end..], PieceKind::Added);
        let pairs = (removed_end - at).min(added_end - removed_end);
        for pair in 0..pairs {
            let (r, a) = (at + pair, removed_end + pair);
            let old_words = tokens(&lines[r].text, DIFF_WORDS + 1);
            let new_words = tokens(&lines[a].text, DIFF_WORDS + 1);
            if old_words.len() <= DIFF_WORDS && new_words.len() <= DIFF_WORDS {
                let pieces: Rc<[Piece]> = word_pieces(&old_words, &new_words, None).into();
                lines[r].pieces = Some(pieces.clone());
                lines[a].pieces = Some(pieces);
            }
        }
        at = added_end;
    }
    // Kept lines fold, but for one beside each change; a lone line stays.
    let mut at = 0;
    while at < lines.len() {
        if lines[at].kind != PieceKind::Same {
            at += 1;
            continue;
        }
        let end = at + run_len(&lines[at..], PieceKind::Same);
        let keep_head = usize::from(at > 0);
        let keep_tail = usize::from(end < lines.len());
        let (fold_from, fold_to) = (at + keep_head, end.saturating_sub(keep_tail));
        if fold_to > fold_from && fold_to - fold_from >= 2 {
            for line in &mut lines[fold_from..fold_to] {
                line.folded = true;
            }
        }
        at = end;
    }
    Some(lines)
}

fn run_len(lines: &[DiffLine], kind: PieceKind) -> usize {
    lines.iter().take_while(|line| line.kind == kind).count()
}

/// `old` and `new` merged in order, each item kept, removed or added (a longest common
/// subsequence).
fn lcs_ops<T: Copy>(old: &[T], new: &[T], same: impl Fn(T, T) -> bool) -> Vec<(PieceKind, T)> {
    // lcs[i][j]: the longest common subsequence of old[i..] and new[j..].
    let mut lcs = vec![vec![0u16; new.len() + 1]; old.len() + 1];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            lcs[i][j] = if same(old[i], new[j]) {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut ops = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < old.len() || j < new.len() {
        if i < old.len() && j < new.len() && same(old[i], new[j]) {
            ops.push((PieceKind::Same, old[i]));
            i += 1;
            j += 1;
        } else if i < old.len() && (j == new.len() || lcs[i + 1][j] >= lcs[i][j + 1]) {
            // A removal before the addition that takes its place: "move → hold".
            ops.push((PieceKind::Removed, old[i]));
            i += 1;
        } else {
            ops.push((PieceKind::Added, new[j]));
            j += 1;
        }
    }
    ops
}

type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// What a click on a row's fold does: shows the row's whole text, or folds it again. The view
/// keeps which rows show whole, by property name.
pub type FoldHandler = Rc<dyn Fn(&SharedString, &mut Window, &mut App)>;

/// How a view shows the rows' long texts: which show whole, and what a fold's click does.
/// Without one, a long text is cut and its folds stay folded.
#[derive(Clone)]
pub struct TextFolds {
    pub whole: Rc<dyn Fn(&SharedString) -> bool>,
    pub on_toggle: FoldHandler,
}

impl TextFolds {
    fn whole(folds: Option<&TextFolds>, name: &SharedString) -> bool {
        folds.is_some_and(|folds| (folds.whole)(name))
    }
}

/// A row's property name in its column, then its body.
pub fn property_row(
    row: &PropertyRow,
    kind: ChangeKind,
    folds: Option<&TextFolds>,
    cx: &App,
) -> Div {
    let name = crate::ds::text::muted(row.name.clone())
        .single_line()
        .truncate();
    h_flex()
        .gap(px(space::LG))
        .min_w_0()
        .items_start()
        .child(
            div()
                .w(px(crate::ds::size::FIELD_LABEL))
                .flex_none()
                .pt(px(space::XXS))
                .child(name),
        )
        .child(property_body(row, kind, folds, cx))
}

/// What a row shows of its value: before → after on one line when both are short, a list's
/// gains and losses, or a text's changed lines with the unchanged folded; an addition its
/// value, a removal its last value, each cut until the whole is asked for.
pub fn property_body(
    row: &PropertyRow,
    kind: ChangeKind,
    folds: Option<&TextFolds>,
    cx: &App,
) -> Div {
    let whole = TextFolds::whole(folds, &row.name);
    let toggle = |label: &str| fold_link(row.name.clone(), label, folds, cx);
    match (kind, &row.body) {
        (ChangeKind::Changed, Body::List(pieces)) => h_flex()
            .flex_1()
            .min_w_0()
            .flex_wrap()
            .pt(px(space::XXS))
            .children(piece_words(pieces, None, cx)),
        (ChangeKind::Changed, Body::Text(lines)) => {
            let mut body = v_flex().flex_1().min_w_0().gap(px(space::WITHIN));
            let mut at = 0;
            while at < lines.len() {
                let line = &lines[at];
                if line.folded && !whole {
                    let end = at + lines[at..].iter().take_while(|l| l.folded).count();
                    let chars: usize = lines[at..end].iter().map(|l| l.text.chars().count()).sum();
                    body = body.child(toggle(&format!(
                        "{} unchanged characters",
                        crate::group_digits(chars)
                    )));
                    at = end;
                    continue;
                }
                body = body.child(diff_line(line, cx));
                at += 1;
            }
            if whole {
                body = body.child(toggle("Show the changes only"));
            }
            body
        }
        (ChangeKind::Changed, _) if row.before.is_some() || row.after.is_some() => {
            // Both short, or the same words spaced otherwise.
            changed_value(row.before.clone(), row.after.clone(), cx)
                .flex_1()
                .min_w_0()
                .pt(px(space::XXS))
        }
        (ChangeKind::Removed, _) => plain_value(&row.before, Color::Muted, whole, toggle),
        _ => plain_value(&row.after, Color::Default, whole, toggle),
    }
}

/// One side's value as wrapping text, cut past [`SHOWN_CHARS`] with the rest offered.
fn plain_value(
    value: &Option<SharedString>,
    color: Color,
    whole: bool,
    toggle: impl Fn(&str) -> AnyElement,
) -> Div {
    let text = value.clone().unwrap_or_else(|| "—".into());
    let (shown, left_out) = if whole { (text, 0) } else { cut(&text) };
    v_flex()
        .flex_1()
        .min_w_0()
        .gap(px(space::WITHIN))
        .pt(px(space::XXS))
        .child(crate::ds::text::body(shown).color(color))
        .when(left_out > 0, |body| {
            body.child(toggle(&format!(
                "Show the whole text · {} more characters",
                crate::group_digits(left_out)
            )))
        })
        .when(
            whole
                && value
                    .as_ref()
                    .is_some_and(|t| t.chars().count() > SHOWN_CHARS),
            |body| body.child(toggle("Show less")),
        )
}

/// A value a change replaced, beside the value it put in its place (spec 033): what it took
/// away struck through on the removed colour's mark, what it brought on the added colour's, as
/// a list's elements and a text's words read. A side with no value reads "—".
pub fn changed_value(before: Option<SharedString>, after: Option<SharedString>, cx: &App) -> Div {
    let side = |value: Option<SharedString>, color: Color, struck: bool| {
        let label = crate::ds::text::body(value.unwrap_or_else(|| "—".into())).color(color);
        div()
            .min_w_0()
            .px(px(space::XS))
            .rounded(px(crate::ds::radii(cx).xs))
            .bg(crate::ds::surface::mark(color, cx))
            .child(if struck { label.strikethrough() } else { label })
    };
    h_flex()
        .flex_wrap()
        .items_baseline()
        .gap(px(space::XS))
        .child(side(before, Color::Deleted, true))
        .child(side(after, Color::Created, false))
}

/// The quiet line that opens a row's folded text, or folds it again: a dotted lead, then
/// its words, muted; without a handler it only says what is folded.
fn fold_link(name: SharedString, label: &str, folds: Option<&TextFolds>, cx: &App) -> AnyElement {
    let hover = crate::ds::surface::highlight(cx);
    let lead = div()
        .w(px(crate::ds::size::DISCLOSURE))
        .flex_none()
        .border_t_1()
        .border_color(crate::ds::surface::rule(cx));
    let line = h_flex()
        .id(ElementId::Name(format!("fold-{name}").into()))
        .pl(px(SIGN_WIDTH + space::SM + space::SM))
        .pr(px(space::MD))
        .py(px(space::XXS))
        .gap(px(space::SM))
        .items_center()
        .rounded(px(crate::ds::radii(cx).xs))
        .child(lead)
        .child(crate::ds::text::meta(label.to_owned()));
    match folds {
        Some(folds) => {
            let on_toggle = folds.on_toggle.clone();
            line.cursor_pointer()
                .hover(move |style| style.bg(hover))
                .on_click(move |_, window, cx| on_toggle(&name, window, cx))
                .into_any_element()
        }
        None => line.into_any_element(),
    }
}

/// The gutter a diff line's sign takes.
const SIGN_WIDTH: f32 = 12.;

/// One line of a text difference: its sign in the gutter and its text on a faint tint of its
/// kind's colour, the words that changed on a stronger one; a kept line muted, with no sign.
fn diff_line(line: &DiffLine, cx: &App) -> Div {
    let (sign, color) = match line.kind {
        PieceKind::Removed => ("−", Some(Color::Deleted)),
        PieceKind::Added => ("+", Some(Color::Created)),
        _ => ("", None),
    };
    let body = match (&line.pieces, color) {
        (Some(pieces), Some(_)) => line_words(pieces, line.kind, cx).into_any_element(),
        _ => crate::ds::text::body(line.text.clone())
            .color(color.unwrap_or(Color::Muted))
            .into_any_element(),
    };
    h_flex()
        .w_full()
        .min_w_0()
        .items_start()
        .gap(px(space::SM))
        .pl(px(space::SM))
        .pr(px(space::MD))
        .py(px(space::XXS))
        .rounded(px(crate::ds::radii(cx).xs))
        .when_some(color, |line, color| {
            line.bg(crate::ds::surface::wash(color, cx))
        })
        .child(
            div()
                .w(px(SIGN_WIDTH))
                .flex_none()
                .child(crate::ds::text::meta(sign).color(color.unwrap_or(Color::Muted))),
        )
        .child(div().flex_1().min_w_0().child(body))
}

/// One side of a changed line as one text that wraps as prose does: the kept words, and that
/// side's own on the mark of its colour. Drawn as one text, a word that changed never takes
/// the rest of its line to a line of its own, as a row of separate words did.
fn line_words(pieces: &[Piece], side: PieceKind, cx: &App) -> Div {
    let color = match side {
        PieceKind::Removed => Color::Deleted,
        _ => Color::Created,
    };
    let (text, marks) = line_text(pieces, side);
    let mark = gpui::HighlightStyle {
        background_color: Some(crate::ds::surface::mark(color, cx)),
        ..Default::default()
    };
    div()
        .flex_1()
        .min_w_0()
        .text_size(crate::ds::text::body_size(cx))
        .text_color(Color::Default.color(cx))
        .child(
            gpui::StyledText::new(text)
                .with_highlights(marks.into_iter().map(|range| (range, mark))),
        )
}

/// One side of a changed line as its text, and the byte ranges of the words that side alone
/// has.
fn line_text(pieces: &[Piece], side: PieceKind) -> (String, Vec<std::ops::Range<usize>>) {
    let mut text = String::new();
    let mut marks = Vec::new();
    for piece in pieces {
        if !(piece.kind == side || matches!(piece.kind, PieceKind::Same | PieceKind::Gap)) {
            continue;
        }
        if piece.spaced && !text.is_empty() {
            text.push(' ');
        }
        let start = text.len();
        text.push_str(&piece.text);
        if piece.kind == side {
            marks.push(start..text.len());
        }
    }
    (text, marks)
}

/// A list's pieces as elements: every piece in its colour, the gained and lost on a tint.
fn piece_words(pieces: &[Piece], side: Option<PieceKind>, cx: &App) -> Vec<AnyElement> {
    let mut words = Vec::new();
    for piece in pieces {
        let own = match side {
            Some(side) => piece.kind == side,
            None => piece.kind != PieceKind::Same,
        };
        let shown = own || matches!(piece.kind, PieceKind::Same | PieceKind::Gap);
        if !shown {
            continue;
        }
        let color = match piece.kind {
            PieceKind::Removed => Color::Deleted,
            PieceKind::Added => Color::Created,
            _ => match side {
                Some(_) => Color::Default,
                None => Color::Muted,
            },
        };
        if piece.spaced && !words.is_empty() {
            words.push(crate::ds::text::body(" ").into_any_element());
        }
        let label = crate::ds::text::body(piece.text.clone()).color(color);
        words.push(if own {
            // The tint reaches past the word's letters, as a highlighter does.
            div()
                .px(px(space::XS))
                .rounded(px(crate::ds::radii(cx).xs))
                .bg(crate::ds::surface::mark(color, cx))
                .child(label)
                .into_any_element()
        } else {
            label.into_any_element()
        });
    }
    words
}

/// One piece of what changed inside a value, with whether a space comes before it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Piece {
    kind: PieceKind,
    text: SharedString,
    spaced: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PieceKind {
    Same,
    Removed,
    Added,
    /// Unchanged text left out, drawn as "…".
    Gap,
}

impl Piece {
    fn new(kind: PieceKind, text: impl Into<SharedString>, spaced: bool) -> Self {
        Self {
            kind,
            text: text.into(),
            spaced,
        }
    }
}

/// Words and marks a difference compares at most on each side, past which the line shows as
/// it is: the comparison takes time that grows with the product of the lengths. The same
/// holds a list's elements.
const DIFF_WORDS: usize = 200;
/// Unchanged words and marks kept on each side of a change, where a difference folds them.
#[cfg(test)]
const CONTEXT_WORDS: usize = 3;

/// The elements a list gained and lost with how many it kept. `None` when only the order
/// changed, or either list is past [`DIFF_WORDS`].
fn list_diff(before: &Value, after: &Value) -> Option<Vec<Piece>> {
    let (Value::Array(before), Value::Array(after)) = (before, after) else {
        return None;
    };
    if before.len() > DIFF_WORDS || after.len() > DIFF_WORDS {
        return None;
    }
    let mut left: Vec<&Value> = before.iter().collect();
    let mut added = Vec::new();
    for item in after {
        match left.iter().position(|kept| *kept == item) {
            Some(at) => {
                left.remove(at);
            }
            None => added.push(item),
        }
    }
    let kept = after.len() - added.len();
    let mut pieces: Vec<Piece> = added
        .into_iter()
        .map(|item| (PieceKind::Added, format!("+ {}", crate::format_value(item))))
        .chain(left.into_iter().map(|item| {
            (
                PieceKind::Removed,
                format!("− {}", crate::format_value(item)),
            )
        }))
        .enumerate()
        .map(|(at, (kind, text))| Piece::new(kind, text, at > 0))
        .collect();
    if pieces.is_empty() {
        // Only the order changed.
        return None;
    }
    if kept > 0 {
        pieces.push(Piece::new(
            PieceKind::Same,
            format!("· {kept} unchanged"),
            true,
        ));
    }
    Some(pieces)
}

/// One word, or one mark of punctuation, of a text, and whether space came before it: a
/// difference compares the words and marks alone, and joins them back as the text spaced them.
#[derive(Debug, Clone, Copy)]
struct Token<'a> {
    text: &'a str,
    spaced: bool,
}

/// The first `limit` tokens of `text`, at most.
fn tokens(text: &str, limit: usize) -> Vec<Token<'_>> {
    let mut tokens = Vec::new();
    let mut spaced = false;
    let mut word: Option<usize> = None;
    for (at, c) in text.char_indices() {
        if tokens.len() >= limit {
            return tokens;
        }
        let in_word = c.is_alphanumeric() || c == '_' || c == '\'' || c == '-';
        if let Some(start) = word
            && !in_word
        {
            tokens.push(Token {
                text: &text[start..at],
                spaced,
            });
            spaced = false;
            word = None;
        }
        if c.is_whitespace() {
            spaced = true;
        } else if in_word {
            word.get_or_insert(at);
        } else {
            tokens.push(Token {
                text: &text[at..at + c.len_utf8()],
                spaced,
            });
            spaced = false;
        }
    }
    if let Some(start) = word {
        tokens.push(Token {
            text: &text[start..],
            spaced,
        });
    }
    tokens
}

/// Tokens back into text, spaced as their text spaced them; a run begins without its space.
fn join(tokens: &[Token]) -> String {
    let mut text = String::new();
    for (ix, token) in tokens.iter().enumerate() {
        if ix > 0 && token.spaced {
            text.push(' ');
        }
        text.push_str(token.text);
    }
    text
}

/// The words of `old` and `new` as kept, removed and added runs (a longest common
/// subsequence); with `context`, unchanged runs cut to that many words beside each change.
fn word_pieces(old: &[Token], new: &[Token], context: Option<usize>) -> Vec<Piece> {
    let words = lcs_ops(old, new, |a, b| a.text == b.text);
    let mut pieces: Vec<Piece> = Vec::new();
    // A run's piece, spaced as its first token was, but never at the start of the row.
    let push = |pieces: &mut Vec<Piece>, kind: PieceKind, run: &[Token]| {
        let spaced = !pieces.is_empty() && run[0].spaced;
        pieces.push(Piece::new(kind, join(run), spaced));
    };
    let mut at = 0;
    while at < words.len() {
        let kind = words[at].0;
        let end = words[at..]
            .iter()
            .position(|(k, _)| *k != kind)
            .map_or(words.len(), |n| at + n);
        let run: Vec<Token> = words[at..end].iter().map(|(_, token)| *token).collect();
        match (kind, context) {
            (PieceKind::Same, Some(context)) => {
                let first = at == 0;
                let last = end == words.len();
                let keep_head = if first { 0 } else { context };
                let keep_tail = if last { 0 } else { context };
                if run.len() > keep_head + keep_tail {
                    if keep_head > 0 {
                        push(&mut pieces, PieceKind::Same, &run[..keep_head]);
                    }
                    let spaced = !pieces.is_empty();
                    pieces.push(Piece::new(PieceKind::Gap, "…", spaced));
                    if keep_tail > 0 {
                        push(&mut pieces, PieceKind::Same, &run[run.len() - keep_tail..]);
                    }
                } else {
                    push(&mut pieces, PieceKind::Same, &run);
                }
            }
            _ => push(&mut pieces, kind, &run),
        }
        at = end;
    }
    pieces
}

/// One relationship change on an entity's card, read from the card's entity: its symbol, the
/// relationship in words, an arrow that says which way it points, and the entity at its other
/// end. A changed relationship's property rows follow. A mirror (`kind` `None`) is a
/// relationship reviewed on another card: muted, with no symbol, its `note` saying where.
#[derive(IntoElement)]
pub struct RelationshipLine {
    id: ElementId,
    kind: Option<ChangeKind>,
    label: SharedString,
    outgoing: bool,
    other: SharedString,
    other_detail: Option<SharedString>,
    other_facts: Option<SharedString>,
    note: Option<SharedString>,
    rows: Rc<[PropertyRow]>,
    status: Option<(SharedString, Color)>,
    selected: bool,
    hollow: bool,
    hover_group: Option<SharedString>,
    actions: Vec<AnyElement>,
    on_click: Option<ClickHandler>,
}

impl RelationshipLine {
    pub fn new(
        id: impl Into<ElementId>,
        kind: Option<ChangeKind>,
        label: impl Into<SharedString>,
        outgoing: bool,
        other: impl Into<SharedString>,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            label: label.into(),
            outgoing,
            other: other.into(),
            other_detail: None,
            other_facts: None,
            note: None,
            rows: Rc::from([]),
            status: None,
            selected: false,
            hollow: false,
            hover_group: None,
            actions: Vec::new(),
            on_click: None,
        }
    }

    /// Shown muted after the other end: its type.
    pub fn other_detail(mut self, detail: impl Into<SharedString>) -> Self {
        self.other_detail = Some(detail.into());
        self
    }

    /// What tells the other end apart beside its name, after its type on the same line, cut to
    /// the room left and whole in its tooltip (spec 042): "stage: in-diligence · confidence:
    /// 0.6". On the line, so facts landing after the card is drawn move nothing (constitution
    /// V).
    pub fn other_facts(mut self, facts: impl Into<SharedString>) -> Self {
        self.other_facts = Some(facts.into());
        self
    }

    /// A short note at the end of the line: "new", or where a mirror is reviewed.
    pub fn note(mut self, note: impl Into<SharedString>) -> Self {
        self.note = Some(note.into());
        self
    }

    pub fn rows(mut self, rows: Rc<[PropertyRow]>) -> Self {
        self.rows = rows;
        self
    }

    pub fn status(mut self, text: impl Into<SharedString>, color: Color) -> Self {
        self.status = Some((text.into(), color));
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// A reviewed relationship: dimmed. It keeps its place.
    pub fn hollow(mut self, hollow: bool) -> Self {
        self.hollow = hollow;
        self
    }

    /// The hover group the line declares, so its actions can show only on hover
    /// (`visible_on_hover`).
    pub fn hover_group(mut self, group: impl Into<SharedString>) -> Self {
        self.hover_group = Some(group.into());
        self
    }

    pub fn action(mut self, action: impl IntoElement) -> Self {
        self.actions.push(action.into_any_element());
        self
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for RelationshipLine {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let mirror = self.kind.is_none();
        let symbol = match self.kind {
            Some(kind) => crate::ds::text::body(kind.symbol()).color(kind.color()),
            None => crate::ds::text::muted("·"),
        };
        let tooltip = match self.kind {
            Some(kind) => kind.label(),
            None => "Reviewed on the other entity's card",
        };
        let line = h_flex()
            .gap(px(space::INLINE))
            .min_w_0()
            .child(
                div()
                    .id("relationship-kind")
                    .w(px(space::MD))
                    .flex_none()
                    .tip(crate::ds::tooltip(tooltip))
                    .child(symbol),
            )
            .child(
                div()
                    .w(px(crate::ds::size::FIELD_LABEL - space::MD - space::INLINE))
                    .flex_none()
                    .child(crate::ds::text::muted(self.label).single_line().truncate()),
            )
            .child(crate::ds::text::muted(if self.outgoing {
                "→"
            } else {
                "←"
            }))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(space::INLINE))
                    .child(
                        crate::ds::text::body(self.other)
                            .color(if mirror { Color::Muted } else { Color::Default })
                            .single_line()
                            .truncate(),
                    )
                    .children(
                        self.other_detail
                            .map(|detail| crate::ds::text::meta(detail).single_line()),
                    )
                    .children(self.other_facts.map(|facts| {
                        div()
                            .id("other-facts")
                            .min_w_0()
                            .flex_shrink(1.)
                            .tip(crate::ds::tooltip(facts.clone()))
                            .child(
                                crate::ds::text::meta(format!("· {facts}"))
                                    .single_line()
                                    .truncate(),
                            )
                    })),
            )
            .children(self.note.map(crate::ds::text::meta))
            .children(
                self.status
                    .map(|(text, color)| crate::ds::text::meta(text).color(color)),
            )
            .children(self.actions);
        let kind = self.kind.unwrap_or(ChangeKind::Unchanged);
        let rows = self.rows.iter().map(|row| {
            div()
                .pl(px(space::MD + space::INLINE))
                .child(property_row(row, kind, None, cx))
        });
        let element = v_flex()
            .id(self.id)
            .min_w_0()
            .px(px(space::XS))
            .rounded(px(radii(cx).xs))
            .when_some(self.hover_group, |d, group| d.group(group))
            .when(self.selected, |d| d.bg(crate::ds::surface::pressed(cx)))
            .when(!self.selected, |d| {
                let hover = crate::ds::surface::highlight(cx);
                d.hover(move |d| d.bg(hover))
            })
            .when(self.hollow, |d| d.opacity(crate::ds::opacity::DIMMED))
            .child(line)
            .children(rows);
        match self.on_click {
            Some(handler) => element
                .cursor_pointer()
                .on_click(move |event, window, cx| {
                    cx.stop_propagation();
                    handler(event, window, cx)
                })
                .into_any_element(),
            None => element.into_any_element(),
        }
    }
}

/// "+3 ~2 −1", each colored, zeros left out.
pub fn change_counts(added: usize, changed: usize, removed: usize) -> Div {
    let part = |count: usize, kind: ChangeKind| {
        (count > 0).then(|| {
            crate::ds::text::body(format!("{}{}", kind.symbol(), crate::group_digits(count)))
                .color(kind.color())
        })
    };
    h_flex()
        .gap(px(space::SM))
        .children(part(added, ChangeKind::Added))
        .children(part(changed, ChangeKind::Changed))
        .children(part(removed, ChangeKind::Removed))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The row as it reads, removals as `[-…]` and additions as `[+…]`.
    fn text(pieces: &[Piece]) -> String {
        let mut text = String::new();
        for piece in pieces {
            if piece.spaced {
                text.push(' ');
            }
            match piece.kind {
                PieceKind::Same | PieceKind::Gap => text.push_str(&piece.text),
                PieceKind::Removed => text.push_str(&format!("[-{}]", piece.text)),
                PieceKind::Added => text.push_str(&format!("[+{}]", piece.text)),
            }
        }
        text
    }

    /// One side of a changed line is one text, the words that side alone has marked in it, so
    /// the punctuation and words after a changed word stay on its line (spec 033; before, each
    /// word was an element of its own and the rest of the line wrapped below the change).
    #[test]
    fn a_changed_line_is_one_text_with_its_words_marked() {
        let lines = text_lines(
            "He teaches every spring, and runs the suite in CI.\nKept as it was.",
            "He teaches every autumn, and runs the suite in CI.\nKept as it was.",
        )
        .expect("the texts differ");
        let side = |kind: PieceKind| {
            let line = lines.iter().find(|line| line.kind == kind).expect("a line");
            let (text, marks) = line_text(line.pieces.as_deref().expect("its words"), kind);
            let marked: Vec<String> = marks
                .iter()
                .map(|range| text[range.clone()].to_owned())
                .collect();
            (text, marked)
        };
        assert_eq!(
            side(PieceKind::Removed),
            (
                "He teaches every spring, and runs the suite in CI.".to_owned(),
                vec!["spring".to_owned()]
            )
        );
        assert_eq!(
            side(PieceKind::Added),
            (
                "He teaches every autumn, and runs the suite in CI.".to_owned(),
                vec!["autumn".to_owned()]
            )
        );
    }

    /// A list shows what it gained and lost and how much it kept; a reordered list keeps
    /// before → after.
    #[test]
    fn a_list_shows_its_gains_and_losses() {
        let pieces = list_diff(
            &json!(["graphs", "hiring", "rust"]),
            &json!(["rust", "graphs", "design"]),
        )
        .unwrap();
        assert_eq!(text(&pieces), "[++ design] [-− hiring] · 2 unchanged");
        assert_eq!(list_diff(&json!(["a", "b"]), &json!(["b", "a"])), None);
        let row = PropertyRow::new("tags", Some(&json!(["a"])), Some(&json!(["b"])));
        assert!(matches!(row.body, Body::List(_)));
    }

    /// A row shows a long value cut, and knows there is more; a short one whole.
    #[test]
    fn a_long_value_is_cut_where_a_row_shows_it() {
        let long = "word ".repeat(1_000);
        let (shown, left_out) = cut(&long);
        assert_eq!(
            shown.chars().count(),
            SHOWN_CHARS - 1,
            "trailing space trimmed"
        );
        assert_eq!(left_out, 3_000);
        let row = PropertyRow::new("note", None, Some(&json!(long)));
        assert!(row.has_more());
        let short = PropertyRow::new("note", Some(&json!(12)), None);
        assert_eq!(short.before.as_deref(), Some("12"));
        assert!(!short.has_more());
        assert!(matches!(short.body, Body::Inline));
    }

    /// Short values read before → after; a text reads line by line, the line that changed
    /// showing its words, the unchanged lines folded but for one beside the change.
    #[test]
    fn a_text_shows_its_changed_lines_and_folds_the_rest() {
        let short = PropertyRow::new("status", Some(&json!("blocked")), Some(&json!("waiting")));
        assert!(matches!(short.body, Body::Inline));
        let before = "First paragraph stays.\nSecond stays.\nThird stays.\nWe agreed to move \
                      the offsite to the coast after the hiring plan is done.\nLast stays.";
        let after = "First paragraph stays.\nSecond stays.\nThird stays.\nWe agreed to hold \
                     the offsite to the coast after the hiring plan is done.\nLast stays.\nAnd \
                     one more line at the end.";
        let lines = text_lines(before, after).unwrap();
        let read: Vec<String> = lines
            .iter()
            .map(|line| {
                let sign = match line.kind {
                    PieceKind::Removed => "-",
                    PieceKind::Added => "+",
                    _ if line.folded => "~",
                    _ => " ",
                };
                format!("{sign}{}", line.text)
            })
            .collect();
        assert_eq!(
            read,
            [
                "~First paragraph stays.",
                "~Second stays.",
                " Third stays.",
                "-We agreed to move the offsite to the coast after the hiring plan is done.",
                "+We agreed to hold the offsite to the coast after the hiring plan is done.",
                " Last stays.",
                "+And one more line at the end.",
            ]
        );
        let pieces = lines[3].pieces.as_ref().unwrap();
        assert_eq!(
            text(pieces),
            "We agreed to [-move] [+hold] the offsite to the coast after the hiring plan is done."
        );
        assert!(
            lines[6].pieces.is_none(),
            "an added line with no pair shows whole"
        );
        // Other spacing alone shows the two texts whole, with no line folded away.
        let spaced = text_lines(before, &before.replace(' ', "  ")).unwrap();
        assert_eq!(spaced.len(), 2);
        assert!(spaced.iter().all(|line| !line.folded));
        // A long line reads sentence by sentence.
        let one = "A split is a tree of panes with shares. ".repeat(6);
        let two = format!("{one}Each pane keeps its own tabs.");
        let lines = text_lines(&one, &two).unwrap();
        assert_eq!(lines.len(), 7);
        assert_eq!(lines[6].kind, PieceKind::Added);
        assert_eq!(
            lines.iter().filter(|l| l.folded).count(),
            5,
            "one kept beside the change"
        );
    }

    /// Words compared with context keep a few around each change and fold the rest.
    #[test]
    fn a_longer_text_shows_its_changed_words_in_context() {
        let before = "We agreed to move the offsite to the coast after the hiring plan is \
                      done and the budget is in";
        let after = "We agreed to hold the offsite to the coast after the hiring plan is done \
                     and the budget is in November";
        let pieces = word_pieces(
            &tokens(before, usize::MAX),
            &tokens(after, usize::MAX),
            Some(CONTEXT_WORDS),
        );
        assert_eq!(
            text(&pieces),
            "We agreed to [-move] [+hold] the offsite to … budget is in [+November]"
        );
        // Punctuation is its own mark: a word that gained a comma is the same word.
        let pieces = word_pieces(
            &tokens(
                "A split is a tree of panes with shares. Each pane keeps its own tabs.",
                usize::MAX,
            ),
            &tokens(
                "A split is a tree of panes, each with its share of the space. Each pane keeps \
                 its own tabs.",
                usize::MAX,
            ),
            Some(CONTEXT_WORDS),
        );
        assert_eq!(
            text(&pieces),
            "… tree of panes[+, each] with [-shares] [+its share of the space]. Each pane …"
        );
    }
}

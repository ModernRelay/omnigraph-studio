//! Entity change cards shared by review and history (spec 008, 009): one card per changed
//! entity, its changed properties before and after, the relationship changes that came with
//! it, and per-type counts.

use std::rc::Rc;

use crate::ds::{radii, radius, space};
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

/// One property row of a card. `None` means the side has no value.
#[derive(Debug, Clone)]
pub struct PropertyRow {
    pub name: SharedString,
    pub before: Option<Value>,
    pub after: Option<Value>,
}

type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// A card for one changed entity, under its type's [`change_group_header`]: the kind's symbol,
/// the entity's name and id, then before → after per changed property for a changed entity,
/// the values of an added one, the last values of a removed one, struck through, and last the
/// [`RelationshipLine`]s reviewed with it. The type is the group's, so a card does not repeat
/// it.
#[derive(IntoElement)]
pub struct ChangeCard {
    id: ElementId,
    kind: ChangeKind,
    entity_id: SharedString,
    name: Option<SharedString>,
    endpoints: Option<(SharedString, SharedString)>,
    rows: Vec<PropertyRow>,
    relationships: Vec<RelationshipLine>,
    relationship_counts: (usize, usize, usize),
    /// Below the relationship lines: for example a control that shows the ones left out.
    relationships_footer: Option<AnyElement>,
    selected: bool,
    hollow: bool,
    status: Option<(SharedString, Color)>,
    actions: Vec<AnyElement>,
    on_click: Option<ClickHandler>,
}

impl ChangeCard {
    pub fn new(
        id: impl Into<ElementId>,
        kind: ChangeKind,
        entity_id: impl Into<SharedString>,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            entity_id: entity_id.into(),
            name: None,
            endpoints: None,
            rows: Vec::new(),
            relationships: Vec::new(),
            relationship_counts: (0, 0, 0),
            relationships_footer: None,
            selected: false,
            hollow: false,
            status: None,
            actions: Vec::new(),
            on_click: None,
        }
    }

    /// A relationship's endpoints, shown as `from → to`.
    pub fn endpoints(mut self, from: impl Into<SharedString>, to: impl Into<SharedString>) -> Self {
        self.endpoints = Some((from.into(), to.into()));
        self
    }

    pub fn rows(mut self, rows: Vec<PropertyRow>) -> Self {
        self.rows = rows;
        self
    }

    /// What a reader knows the entity by, shown before its id; `None` when it has only its id.
    pub fn name(mut self, name: Option<impl Into<SharedString>>) -> Self {
        self.name = name.map(Into::into);
        self
    }

    /// The relationship changes shown with the entity, under one header with the added,
    /// changed and removed counts of those reviewed here (mirrors are not counted).
    pub fn relationships(
        mut self,
        (added, changed, removed): (usize, usize, usize),
        lines: Vec<RelationshipLine>,
    ) -> Self {
        self.relationship_counts = (added, changed, removed);
        self.relationships = lines;
        self
    }

    /// Shown below the relationship lines.
    pub fn relationships_footer(mut self, footer: impl IntoElement) -> Self {
        self.relationships_footer = Some(footer.into_any_element());
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// A reviewed card: outlined instead of filled, and dimmed. It keeps its place.
    pub fn hollow(mut self, hollow: bool) -> Self {
        self.hollow = hollow;
        self
    }

    /// A short state shown in the header, for example "Reverted".
    pub fn status(mut self, text: impl Into<SharedString>, color: Color) -> Self {
        self.status = Some((text.into(), color));
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

/// One property's row: its name, then before → after for a change, the value of an addition,
/// or the last value of a removal, struck through.
fn property_row(row: &PropertyRow, kind: ChangeKind) -> Div {
    let name = crate::ds::text::muted(row.name.clone())
        .single_line()
        .truncate();
    let inner = match (kind, &row.before, &row.after) {
        (ChangeKind::Changed, Some(before), Some(after)) => inner_diff(before, after),
        _ => None,
    };
    let body = match (kind, inner) {
        (_, Some(pieces)) => pieces_row(pieces),
        (ChangeKind::Changed, None) => h_flex()
            .flex_1()
            .min_w_0()
            .gap(px(space::MD))
            .child(div().min_w_0().max_w(relative(0.48)).child(value_label(
                &row.before,
                Color::Deleted,
                true,
            )))
            .child(crate::ds::text::muted("→"))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(value_label(&row.after, Color::Created, false)),
            ),
        (ChangeKind::Added | ChangeKind::Unchanged, None) => h_flex()
            .flex_1()
            .min_w_0()
            .child(value_label(&row.after, Color::Default, false)),
        (ChangeKind::Removed, None) => {
            h_flex()
                .flex_1()
                .min_w_0()
                .child(value_label(&row.before, Color::Muted, true))
        }
    };
    h_flex()
        .gap(px(space::LG))
        .min_w_0()
        .child(
            div()
                .w(px(crate::ds::size::FIELD_LABEL))
                .flex_none()
                .child(name),
        )
        .child(body)
}

/// What changed inside a value, drawn in one wrapping row: kept text muted, removals struck,
/// additions in the created colour.
fn pieces_row(pieces: Vec<Piece>) -> Div {
    h_flex()
        .flex_1()
        .min_w_0()
        .flex_wrap()
        .children(pieces.into_iter().flat_map(|piece| {
            let (color, strike) = match piece.kind {
                PieceKind::Same | PieceKind::Gap => (Color::Muted, false),
                PieceKind::Removed => (Color::Deleted, true),
                PieceKind::Added => (Color::Created, false),
            };
            let label = crate::ds::text::body(piece.text).color(color);
            // Its own space, so a struck removal does not strike it.
            let space = piece.spaced.then(|| crate::ds::text::body(" "));
            space
                .into_iter()
                .chain([if strike { label.strikethrough() } else { label }])
        }))
}

/// One piece of what changed inside a value, with whether a space comes before it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Piece {
    kind: PieceKind,
    text: String,
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
    fn new(kind: PieceKind, text: impl Into<String>, spaced: bool) -> Self {
        Self {
            kind,
            text: text.into(),
            spaced,
        }
    }
}

/// Text at least this long, in words and marks ([`Token`]), shows its changed words rather than
/// before → after.
const LONG_TEXT_WORDS: usize = 8;
/// Words and marks a difference compares at most on each side, past which before → after shows:
/// the comparison runs as the row draws, in time that grows with the product of the lengths.
/// The same holds a list's elements.
const DIFF_WORDS: usize = 200;
/// Unchanged words and marks kept on each side of a change.
const CONTEXT_WORDS: usize = 3;

/// What changed inside a value, where that says more than before → after: the elements a list
/// gained and lost with how many it kept, or the words a longer text changed with a few words
/// around each change. `None` for any other value.
fn inner_diff(before: &Value, after: &Value) -> Option<Vec<Piece>> {
    match (before, after) {
        (Value::Array(before), Value::Array(after)) => {
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
        (Value::String(before), Value::String(after)) => {
            // Cut past the limit: a long text is not split whole at each draw to be refused.
            let old = tokens(before, DIFF_WORDS + 1);
            let new = tokens(after, DIFF_WORDS + 1);
            let long = old.len().max(new.len()) >= LONG_TEXT_WORDS;
            if !long || old.len() > DIFF_WORDS || new.len() > DIFF_WORDS {
                return None;
            }
            let pieces = word_pieces(&old, &new);
            // The same words with other spacing: before → after shows what differs.
            pieces
                .iter()
                .any(|piece| matches!(piece.kind, PieceKind::Removed | PieceKind::Added))
                .then_some(pieces)
        }
        _ => None,
    }
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
/// subsequence), unchanged runs cut to [`CONTEXT_WORDS`] beside each change.
fn word_pieces(old: &[Token], new: &[Token]) -> Vec<Piece> {
    // lcs[i][j]: the longest common subsequence of old[i..] and new[j..].
    let mut lcs = vec![vec![0u16; new.len() + 1]; old.len() + 1];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            lcs[i][j] = if old[i].text == new[j].text {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    // Tokens in order, each kept (0), removed (1) or added (2).
    let mut words: Vec<(u8, Token)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < old.len() || j < new.len() {
        if i < old.len() && j < new.len() && old[i].text == new[j].text {
            words.push((0, old[i]));
            i += 1;
            j += 1;
        } else if i < old.len() && (j == new.len() || lcs[i + 1][j] >= lcs[i][j + 1]) {
            // A removal before the addition that takes its place: "move → hold".
            words.push((1, old[i]));
            i += 1;
        } else {
            words.push((2, new[j]));
            j += 1;
        }
    }
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
        match kind {
            0 => {
                let first = at == 0;
                let last = end == words.len();
                let keep_head = if first { 0 } else { CONTEXT_WORDS };
                let keep_tail = if last { 0 } else { CONTEXT_WORDS };
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
            1 => push(&mut pieces, PieceKind::Removed, &run),
            _ => push(&mut pieces, PieceKind::Added, &run),
        }
        at = end;
    }
    pieces
}

fn value_label(value: &Option<Value>, color: Color, strike: bool) -> Label {
    let text = match value {
        Some(value) => format_value(value),
        None => "—".into(),
    };
    let mut label = crate::ds::text::body(text)
        .color(color)
        .single_line()
        .truncate();
    if strike {
        label = label.strikethrough();
    }
    label
}

impl RenderOnce for ChangeCard {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let accent = self.kind.color().color(cx);
        let border = if self.selected {
            crate::ds::surface::focus(cx)
        } else {
            crate::ds::surface::rule(cx)
        };
        let kind = self.kind;
        let header = h_flex()
            .id("change-kind")
            .gap(px(space::INLINE))
            .min_w_0()
            .tooltip(ui::Tooltip::text(kind.label()))
            .child(crate::ds::text::body(kind.symbol()).color(kind.color()))
            .map(|header| match &self.endpoints {
                Some((from, to)) => header.child(
                    crate::ds::text::body(format!("{from} → {to}"))
                        .buffer_font(cx)
                        .single_line()
                        .truncate(),
                ),
                None => match &self.name {
                    Some(name) => header
                        .child(crate::ds::text::body(name.clone()).single_line().truncate())
                        .child(
                            crate::ds::text::meta(self.entity_id.clone())
                                .buffer_font(cx)
                                .single_line()
                                .truncate(),
                        ),
                    None => header.child(
                        crate::ds::text::body(self.entity_id.clone())
                            .buffer_font(cx)
                            .single_line()
                            .truncate(),
                    ),
                },
            })
            .child(div().flex_1())
            .children(
                self.status
                    .clone()
                    .map(|(text, color)| crate::ds::text::meta(text).color(color)),
            )
            .children(self.actions);
        let rows = self.rows.iter().map(|row| property_row(row, kind));
        let (added, changed, removed) = self.relationship_counts;
        let footer = self.relationships_footer;
        let relationships = (!self.relationships.is_empty()).then(|| {
            v_flex()
                .pt(px(space::XS))
                .gap(px(space::WITHIN))
                .child(
                    h_flex()
                        .gap(px(space::MD))
                        .child(crate::ds::text::meta("Relationships"))
                        .child(change_counts(added, changed, removed)),
                )
                .children(self.relationships)
                .children(footer)
        });
        let card = h_flex()
            .id(self.id)
            .w_full()
            .rounded(px(radii(cx).md))
            .border_1()
            .border_color(border)
            .overflow_hidden()
            .child(
                // The change's mark: a line of its colour down the card's leading edge.
                div()
                    .w(px(crate::ds::size::ACCENT_LINE))
                    .h_full()
                    .flex_none()
                    // Its fill meets the card's left corners.
                    .rounded_l(px(radius::flush(radii(cx).md)))
                    .when(!self.hollow && kind != ChangeKind::Unchanged, |d| {
                        d.bg(accent)
                    }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .px(px(space::LG))
                    .py(px(space::MD))
                    .gap(px(space::XS))
                    .when(self.hollow, |d| d.opacity(crate::ds::opacity::DIMMED))
                    .child(header)
                    .children(rows)
                    .children(relationships),
            );
        match self.on_click {
            Some(handler) => card
                .cursor_pointer()
                .on_click(move |event, window, cx| handler(event, window, cx))
                .into_any_element(),
            None => card.into_any_element(),
        }
    }
}

/// One relationship change on a [`ChangeCard`], read from the card's entity: its symbol, the
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
    note: Option<SharedString>,
    rows: Vec<PropertyRow>,
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
            note: None,
            rows: Vec::new(),
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

    /// A short note at the end of the line: "new", or where a mirror is reviewed.
    pub fn note(mut self, note: impl Into<SharedString>) -> Self {
        self.note = Some(note.into());
        self
    }

    pub fn rows(mut self, rows: Vec<PropertyRow>) -> Self {
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
                    .tooltip(ui::Tooltip::text(tooltip))
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
                    ),
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
                .child(property_row(row, kind))
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

/// A type group's header above its cards: swatch, name, counts.
pub fn change_group_header(
    type_name: &str,
    is_edge: bool,
    added: usize,
    changed: usize,
    removed: usize,
    cx: &App,
) -> Div {
    h_flex()
        .gap(px(space::MD))
        .pt(px(space::LG))
        .pb(px(space::XS))
        .child(crate::kind_swatch(type_name, is_edge, cx))
        .child(crate::ds::text::body(type_name.to_owned()))
        .child(crate::ds::text::meta(if is_edge {
            "relationship"
        } else {
            "node"
        }))
        .child(change_counts(added, changed, removed))
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

    /// A list shows what it gained and lost and how much it kept; a reordered list, a short
    /// text and a number keep before → after.
    #[test]
    fn a_list_shows_its_gains_and_losses() {
        let pieces = inner_diff(
            &json!(["graphs", "hiring", "rust"]),
            &json!(["rust", "graphs", "design"]),
        )
        .unwrap();
        assert_eq!(text(&pieces), "[++ design] [-− hiring] · 2 unchanged");
        assert_eq!(inner_diff(&json!(["a", "b"]), &json!(["b", "a"])), None);
        assert_eq!(inner_diff(&json!("Engineer"), &json!("Lead")), None);
        assert_eq!(inner_diff(&json!(12), &json!(15)), None);
    }

    /// A longer text shows its changed words with a few words around each change, and the
    /// unchanged rest as a gap.
    #[test]
    fn a_longer_text_shows_its_changed_words_in_context() {
        let before = "We agreed to move the offsite to the coast after the hiring plan is \
                      done and the budget is in";
        let after = "We agreed to hold the offsite to the coast after the hiring plan is done \
                     and the budget is in November";
        let pieces = inner_diff(&json!(before), &json!(after)).unwrap();
        assert_eq!(
            text(&pieces),
            "We agreed to [-move] [+hold] the offsite to … budget is in [+November]"
        );
        // Other spacing alone is not a change of words.
        assert_eq!(
            inner_diff(&json!(before), &json!(before.replace(' ', "  "))),
            None
        );
        // Punctuation is its own mark: a word that gained a comma is the same word.
        let pieces = inner_diff(
            &json!("A split is a tree of panes with shares. Each pane keeps its own tabs."),
            &json!(
                "A split is a tree of panes, each with its share of the space. Each pane keeps \
                 its own tabs."
            ),
        )
        .unwrap();
        assert_eq!(
            text(&pieces),
            "… tree of panes[+, each] with [-shares] [+its share of the space]. Each pane …"
        );
    }
}

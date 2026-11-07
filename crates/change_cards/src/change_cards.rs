//! The entity cards of a review and of a commit's view in History (spec 008, 009): an entity's
//! card from its folded change, the lines of the relationship changes reviewed on it, and the
//! header above a run of one type's cards.
//!
//! Layer 4: it puts `graph_store`'s changes by entity onto `studio_ui`'s components, which
//! neither of those layers can do alone, so the two views draw the same cards from one place.
//! What a card can do (review, revert, edit) stays with the view that offers it.
//!
//! Since 2026-11-01 both views draw the review's feed: a run of one type is a row ("2 changed
//! in Event") folding a bordered table of its entities ([`run_row`]), each entity a row of its
//! name, what changed in a few words and its id ([`entity_row`]), unfolding to its property
//! rows and relationship lines ([`entity_body`]).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui::{AnyElement, App, ClickEvent, Div, ElementId, Window};
use gq_language::SchemaModel;
use graph_store::{
    EntityGroup, EntityGroups, FoldedChange, FoldedChanges, GroupRun, NetChange, NodeNames,
    RelationshipEntry, entity_name,
};
use studio_ui::ds::{radii, space};
use studio_ui::{
    ChangeKind, PropertyRow, RelationshipLine, TextFolds, change_counts, group_digits,
};
use ui::prelude::*;

/// One row of a list of cards: the header of a run of one type's cards, by its index in the
/// runs [`rows`] returns, or an entity's card, by its group.
#[derive(Debug, Clone, Copy)]
pub enum Row {
    Header(usize),
    Entity(usize),
}

/// The rows of the cards of the groups `keep` leaves, each run of one type under its header,
/// and the runs.
pub fn rows(
    folded: &FoldedChanges,
    groups: &EntityGroups,
    keep: impl FnMut(&EntityGroup) -> bool,
) -> (Vec<Row>, Vec<GroupRun>) {
    let runs = groups.runs(folded, keep);
    let mut rows = Vec::new();
    for (ix, run) in runs.iter().enumerate() {
        rows.push(Row::Header(ix));
        rows.extend(run.groups.iter().map(|&group| Row::Entity(group)));
    }
    (rows, runs)
}

/// Relationship lines a card shows until more are asked for: a node that gained hundreds of
/// relationships stays one readable card, and one row its list lays out quickly.
pub const CARD_LINES: usize = 20;

/// Relationship lines a card adds each time more are asked for. A card is one row of its
/// list, laid out whole at every frame, so it grows by this many rather than to every line.
pub const MORE_LINES: usize = 200;

/// How many relationship lines each card shows, by entity `(type, id)`: [`CARD_LINES`] until
/// more are asked for, then [`MORE_LINES`] more each time.
#[derive(Default)]
pub struct CardLines(HashMap<(String, String), usize>);

impl CardLines {
    /// How many of `card`'s `total` lines show: those asked for, and every line through the
    /// selected one (`selected_at`).
    pub fn shown(
        &self,
        card: &(String, String),
        total: usize,
        selected_at: Option<usize>,
    ) -> usize {
        let asked = self.0.get(card).copied().unwrap_or(CARD_LINES);
        let through_selected = selected_at.map_or(0, |at| at + 1);
        total.min(asked.max(through_selected))
    }

    /// Shows [`MORE_LINES`] more of `card`'s lines, past those it shows now (`shown`).
    pub fn show_more(&mut self, card: (String, String), shown: usize) {
        let asked = self.0.entry(card).or_insert(CARD_LINES);
        *asked = (*asked).max(shown) + MORE_LINES;
    }
}

/// Each change's property rows, made the first time its card or line is drawn and kept with
/// the changes they were made from: a view draws its cards at every frame, and a row's word
/// difference takes time that grows with its text. A view makes a new one with new changes.
#[derive(Default)]
pub struct CardRows(RefCell<HashMap<usize, Rc<[PropertyRow]>>>);

impl CardRows {
    /// Change `change`'s rows, made by `make` the first time they are asked for.
    fn get(&self, change: usize, make: impl FnOnce() -> Vec<PropertyRow>) -> Rc<[PropertyRow]> {
        if let Some(rows) = self.0.borrow().get(&change) {
            return rows.clone();
        }
        let rows: Rc<[PropertyRow]> = make().into();
        self.0.borrow_mut().insert(change, rows.clone());
        rows
    }
}

/// The button below card `ix`'s lines, with `left_out` of them not shown; its click, the
/// view's, asks [`CardLines::show_more`].
pub fn more_lines_button(
    ix: usize,
    left_out: usize,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> studio_ui::ds::Button {
    studio_ui::ds::button(("more-relationships", ix), more_lines_label(left_out))
        .style(ButtonStyle::Subtle)
        .on_click(on_click)
}

/// What the button below a card's lines says, with `left_out` of them not shown.
fn more_lines_label(left_out: usize) -> String {
    match left_out {
        1 => "Show 1 more relationship".to_owned(),
        n if n <= MORE_LINES => format!("Show {} more relationships", group_digits(n)),
        n => format!(
            "Show {MORE_LINES} more of the {} relationships not shown",
            group_digits(n)
        ),
    }
}

/// Change `ix`'s property rows, made once ([`CardRows`]): its changed properties, after the
/// ends a lone relationship moved.
pub fn change_rows(
    folded: &FoldedChanges,
    ix: usize,
    schema: Option<&SchemaModel>,
    rows: &CardRows,
) -> Rc<[PropertyRow]> {
    let change = &folded.changes[ix];
    rows.get(ix, || {
        let mut rows = moved_end_rows(change);
        rows.extend(property_rows(change, schema));
        rows
    })
}

/// What a change did to its entity, as a card says it.
pub fn kind_of_change(change: &FoldedChange) -> ChangeKind {
    kind_of(change.net())
}

/// A lone relationship's rows for the ends it moved, `from` and `to`, each its old end against
/// its new: no schema names their types, so their ids.
fn moved_end_rows(change: &FoldedChange) -> Vec<PropertyRow> {
    if change.moved_ends().is_none() {
        return Vec::new();
    }
    let before = change
        .before
        .as_ref()
        .and_then(|image| image.endpoints.as_ref());
    let after = change
        .after
        .as_ref()
        .and_then(|image| image.endpoints.as_ref());
    let (Some(before), Some(after)) = (before, after) else {
        return Vec::new();
    };
    ["from", "to"]
        .into_iter()
        .filter_map(|end| {
            let (was, now) = (before.get(end)?, after.get(end)?);
            (was != now).then(|| PropertyRow::new(end, Some(was), Some(now)))
        })
        .collect()
}

/// A card's line for one relationship entry of `folded`: what changed, which way it points,
/// the other end, and a changed relationship's property rows; for one moved to another end
/// ([`RelationshipEntry::replaces`]), both ends and the removed edge's values against the new
/// one's, and for one edge whose ends moved ([`RelationshipEntry::other_was`]), the end it had
/// and the end it has, and who held this entity's end when both moved. A mirror's line carries
/// `mirror_note`, which says where it is shown in full.
pub fn relationship_line(
    entry: RelationshipEntry,
    folded: &FoldedChanges,
    schema: Option<&SchemaModel>,
    rows: &CardRows,
    mirror_note: &'static str,
) -> RelationshipLine {
    let kind = entry.net.map(|net| kind_of(Some(net)));
    let id = if kind.is_some() {
        ("relationship", entry.change)
    } else {
        ("mirror", entry.change)
    };
    let edge = &folded.changes[entry.change];
    let was = entry
        .replaces
        .as_ref()
        .map(|(removed, _)| &folded.changes[*removed]);
    let rows = match kind {
        // A line's rows are its edge's, compared with the removed edge's when it moved.
        Some(_) => rows.get(entry.change, || match was {
            Some(was) => property_rows(
                &FoldedChange {
                    before: was.before.clone(),
                    ..edge.clone()
                },
                schema,
            ),
            None => property_rows(edge, schema),
        }),
        None => Rc::from([]),
    };
    let (other, note) = other_end(&entry, mirror_note);
    let line = RelationshipLine::new(id, kind, entry.words, entry.outgoing, other)
        .other_detail(entry.other_type)
        .rows(rows);
    match note {
        Some(note) => line.note(note),
        None => line,
    }
}

/// What a relationship line names at its other end, and the note at its end: a mirror's says
/// where it is shown in full (`mirror_note`); a relationship that moved names the end it had
/// before the end it has ("Acme → Globex"), and says "moved", or, when both its ends moved,
/// who held this entity's end ("moved from Ana"); "new" marks an end the branch added.
fn other_end(
    entry: &RelationshipEntry,
    mirror_note: &'static str,
) -> (String, Option<SharedString>) {
    if entry.net.is_none() {
        return (entry.other.clone(), Some(mirror_note.into()));
    }
    let was = entry
        .replaces
        .as_ref()
        .map(|(_, was)| was)
        .or(entry.other_was.as_ref());
    let other = match was {
        Some(was) => format!("{was} → {}", entry.other),
        None => entry.other.clone(),
    };
    let moved = was.is_some() || entry.here_was.is_some();
    let note = match &entry.here_was {
        Some(here_was) if entry.other_is_new => Some(format!("moved from {here_was}, new").into()),
        Some(here_was) => Some(format!("moved from {here_was}").into()),
        None if moved && entry.other_is_new => Some("moved, new".into()),
        None if moved => Some("moved".into()),
        None if entry.other_is_new => Some("new".into()),
        None => None,
    };
    (other, note)
}

/// The section a run opens, in the view's words, above the run's row.
pub fn section_title(title: impl Into<SharedString>) -> Div {
    div()
        .px(px(space::EDGE))
        .pt(px(space::LG))
        .pb(px(space::XS))
        .child(studio_ui::ds::text::meta(title.into()))
}

/// A run's counts as the feed says them, and the word to its type: "1 changed" "in" Task,
/// "2 added" "to" Email, "1 removed" "from" Person; a run of entities standing for their
/// relationships alone, "3 entities" "with relationships that changed in".
pub fn run_sentence(run: &GroupRun) -> (String, &'static str) {
    let mut parts = Vec::new();
    if run.changed > 0 {
        parts.push(format!("{} changed", group_digits(run.changed)));
    }
    if run.added > 0 {
        parts.push(format!("{} added", group_digits(run.added)));
    }
    if run.removed > 0 {
        parts.push(format!("{} removed", group_digits(run.removed)));
    }
    if parts.is_empty() {
        return (
            studio_ui::plural(run.groups.len(), "entity", "entities"),
            "with relationships that changed in",
        );
    }
    let joiner = match (run.changed, run.added, run.removed) {
        (0, _, 0) => "to",
        (0, 0, _) => "from",
        _ => "in",
    };
    (parts.join(", "), joiner)
}

/// What a run's row says beside its type, past the counts: how many relationships its entities
/// carry, when that is all that changed. The properties each change touched are said once, on
/// its entity's row, and in full when it opens, never again on the type's row. A run of one
/// entity says nothing more, since that entity's own row says it.
pub fn run_detail(run: &GroupRun) -> String {
    if run.groups.len() < 2 || run.changed + run.added + run.removed > 0 {
        String::new()
    } else if run.relationships > 0 {
        format!(
            "with {}",
            studio_ui::plural(run.relationships, "relationship", "relationships")
        )
    } else {
        String::new()
    }
}

/// The properties a change set or dropped, by name.
fn changed_properties(change: &FoldedChange) -> Vec<&str> {
    let (Some(before), Some(after)) = (&change.before, &change.after) else {
        return Vec::new();
    };
    after
        .properties
        .iter()
        .filter(|(key, value)| before.properties.get(*key) != Some(value))
        .map(|(key, _)| key.as_str())
        .chain(
            before
                .properties
                .keys()
                .filter(|key| !after.properties.contains_key(*key))
                .map(String::as_str),
        )
        .collect()
}

/// What a run's row says besides its counts and type.
#[derive(Default)]
pub struct RunWords {
    /// The title of the section the run opens.
    pub section: Option<String>,
    /// Muted, after the type ([`run_detail`]).
    pub detail: String,
    /// At its end: when its changes were made.
    pub when: Option<String>,
}

/// The row of a run of one type: its status dot, a chevron, "1 changed in" and the type, then
/// `words`. It folds the run's entities away (`folded`, a rule under it) and back on a click.
pub fn run_row(
    id: impl Into<ElementId>,
    run: &GroupRun,
    words: RunWords,
    folded: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let RunWords {
        section,
        detail,
        when,
    } = words;
    let (counts, joiner) = run_sentence(run);
    let color = if run.changed > 0 {
        Color::Modified
    } else if run.removed > 0 {
        Color::Deleted
    } else if run.added > 0 {
        Color::Created
    } else {
        Color::Muted
    };
    let hover = studio_ui::ds::surface::hover(cx);
    // The hover is a rounded row the feed's width, inset by `space::SM` so its text keeps the
    // feed's edge, and kept `space::XS` clear of the table under it, whose open head is grey too.
    let row = h_flex()
        .id(id)
        .w_full()
        .h(px(studio_ui::ds::size::FEED_ROW))
        .px(px(space::XL - space::SM))
        .gap(px(space::MD))
        .items_center()
        .rounded(px(radii(cx).sm))
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        .child(studio_ui::ds::dot(color))
        .child(studio_ui::ds::icon::meta(if folded {
            IconName::ChevronRight
        } else {
            IconName::ChevronDown
        }))
        .child(studio_ui::ds::text::body(counts))
        .child(studio_ui::ds::text::muted(joiner))
        .child(studio_ui::kind_swatch(&run.type_name, run.is_edge, cx))
        .child(studio_ui::ds::text::body(run.type_name.clone()))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(studio_ui::ds::text::muted(detail).single_line().truncate()),
        )
        .children(when.map(studio_ui::ds::text::meta))
        .on_click(on_click);
    v_flex()
        .w_full()
        .children(section.map(section_title))
        .child(
            div()
                .w_full()
                .px(px(space::SM))
                .when(folded, |wrap| {
                    wrap.border_b_1()
                        .border_color(studio_ui::ds::surface::rule(cx))
                })
                .when(!folded, |wrap| wrap.pb(px(space::XS)))
                .child(row),
        )
        .into_any_element()
}

/// What an entity's row names: what happened to it, its name (a relationship's ends), and its
/// id when the name is not it; a node standing unchanged for its relationships, the name read
/// for it (`names`).
pub fn identity(
    groups: &EntityGroups,
    folded: &FoldedChanges,
    ix: usize,
    schema: Option<&SchemaModel>,
    names: &NodeNames,
) -> (ChangeKind, String, Option<String>) {
    let g = &groups.groups[ix];
    match g.change {
        Some(own) => {
            let change = &folded.changes[own];
            let kind = kind_of_change(change);
            if change.is_edge() {
                return match change.endpoints() {
                    Some((from, to)) => (kind, format!("{from} → {to}"), None),
                    None => (kind, change.id.clone(), None),
                };
            }
            match entity_name(change, schema) {
                Some(name) => (kind, name, Some(change.id.clone())),
                None => (kind, change.id.clone(), None),
            }
        }
        None => match names.get(&(g.type_name.clone(), g.id.clone())) {
            Some(name) => (ChangeKind::Unchanged, name.clone(), Some(g.id.clone())),
            None => (ChangeKind::Unchanged, g.id.clone(), None),
        },
    }
}

/// What an entity's folded row says: the properties its change touched, or how many an added
/// or removed record has, then its relationship changes.
pub fn entity_summary(folded: &FoldedChanges, group: &EntityGroup) -> String {
    let mut parts = Vec::new();
    if let Some(own) = group.change {
        let change = &folded.changes[own];
        match (&change.before, &change.after) {
            (Some(_), Some(_)) => {
                let mut names = changed_properties(change);
                names.sort();
                names.dedup();
                if !names.is_empty() {
                    let shown: Vec<&str> = names.iter().take(4).copied().collect();
                    let mut text = shown.join(", ");
                    if names.len() > 4 {
                        text.push_str(&format!(" and {} more", names.len() - 4));
                    }
                    text.push_str(" changed");
                    parts.push(text);
                }
            }
            (None, Some(after)) => parts.push(studio_ui::plural(
                after.properties.len(),
                "property",
                "properties",
            )),
            (Some(before), None) => parts.push(format!(
                "{} removed",
                studio_ui::plural(before.properties.len(), "property", "properties")
            )),
            (None, None) => {}
        }
    }
    let relationships = group.relationships.len();
    if relationships > 0 {
        parts.push(studio_ui::plural(
            relationships,
            "relationship",
            "relationships",
        ));
    }
    parts.join(" · ")
}

/// One entity's row in its run's table: what it is and how it is drawn.
pub struct EntityRow {
    /// The row's head, for its click.
    pub id: ElementId,
    pub kind: ChangeKind,
    pub name: String,
    /// Its id, when the name is not it.
    pub entity_id: Option<String>,
    pub summary: String,
    /// What the view says of it (a review's "Reviewed"), at its end.
    pub status: Option<(String, Color)>,
    /// The first and last rows of the table carry its rounded ends.
    pub first: bool,
    pub last: bool,
    pub open: bool,
    pub selected: bool,
    /// Drawn receded: a reviewed entity.
    pub dimmed: bool,
}

/// An entity's row in the table under its run: a chevron, its name (struck through when
/// removed), what changed in a few words while it is folded, its status and its id; `body`
/// under it when open, which says what changed in full, so the row does not say it again.
/// The table is drawn row by row, inside the run's gutter.
pub fn entity_row(
    row: EntityRow,
    body: Option<Div>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let EntityRow {
        id,
        kind,
        name,
        entity_id,
        summary,
        status,
        first,
        last,
        open,
        selected,
        dimmed,
    } = row;
    let radius = studio_ui::ds::radius::flush(radii(cx).md);
    let hover = studio_ui::ds::surface::hover(cx);
    let head = h_flex()
        .id(id)
        .min_h(px(studio_ui::ds::size::HEADER))
        .px(px(space::LG))
        .gap(px(space::MD))
        .items_center()
        .cursor_pointer()
        .when(first, |head| head.rounded_t(px(radius)))
        .when(last && !open, |head| head.rounded_b(px(radius)))
        .when(selected, |head| {
            head.bg(studio_ui::ds::surface::selected_document(cx))
        })
        .when(!selected, |head| head.hover(move |style| style.bg(hover)))
        .when(dimmed, |head| head.opacity(studio_ui::ds::opacity::DIMMED))
        .child(studio_ui::ds::icon::meta(if open {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        }))
        .child(
            studio_ui::ds::text::body(name)
                .when(kind == ChangeKind::Removed, |name| name.strikethrough())
                .single_line()
                .truncate(),
        )
        .child(div().flex_1().min_w_0().when(!open, |slot| {
            slot.child(studio_ui::ds::text::muted(summary).single_line().truncate())
        }))
        .children(status.map(|(text, color)| studio_ui::ds::text::meta(text).color(color)))
        .children(entity_id.map(|id| {
            studio_ui::ds::text::meta(id)
                .buffer_font(cx)
                .single_line()
                .truncate()
        }))
        .on_click(on_click);
    let outline = studio_ui::ds::surface::outline(cx);
    div()
        .pl(px(space::XL + studio_ui::ds::size::GUTTER))
        .pr(px(space::XL))
        .when(last, |row| row.pb(px(space::LG)))
        .child(
            v_flex()
                .w_full()
                .border_l_1()
                .border_r_1()
                .border_color(outline)
                .when(first, |table| {
                    table.border_t_1().rounded_t(px(radii(cx).md))
                })
                .when(last, |table| table.border_b_1().rounded_b(px(radii(cx).md)))
                .child(if first {
                    head.into_any_element()
                } else {
                    studio_ui::ds::dotted(div().border_t_1(), cx)
                        .child(head)
                        .into_any_element()
                })
                .children(body),
        )
        .into_any_element()
}

/// What changed in an entity, in full, as its row unfolds: its property rows, each under a
/// dotted rule, then "Relationships" with their counts over their lines, and `more` (the
/// button that shows more of them) at their end. The view pads it and adds what it offers.
pub fn entity_body(
    rows: Option<Rc<[PropertyRow]>>,
    kind: ChangeKind,
    folds: Option<&TextFolds>,
    lines: Vec<RelationshipLine>,
    counts: (usize, usize, usize),
    more: Option<AnyElement>,
    cx: &App,
) -> Div {
    let property_rows: Vec<AnyElement> = rows
        .iter()
        .flat_map(|rows| rows.iter())
        .map(|row| {
            studio_ui::ds::dotted(div().border_t_1(), cx)
                .py(px(space::SM))
                .child(studio_ui::property_row(row, kind, folds, cx))
                .into_any_element()
        })
        .collect();
    let (added, changed, removed) = counts;
    let relationships = (!lines.is_empty()).then(|| {
        studio_ui::ds::dotted(v_flex().border_t_1(), cx)
            .py(px(space::SM))
            .gap(px(space::WITHIN))
            .child(
                h_flex()
                    .gap(px(space::MD))
                    .pb(px(space::XXS))
                    .child(studio_ui::ds::text::meta("Relationships"))
                    .child(change_counts(added, changed, removed)),
            )
            .children(lines)
            .children(more)
    });
    v_flex()
        .w_full()
        .min_w_0()
        .children(property_rows)
        .children(relationships)
}

fn kind_of(net: Option<NetChange>) -> ChangeKind {
    match net {
        Some(NetChange::Added) => ChangeKind::Added,
        Some(NetChange::Removed) => ChangeKind::Removed,
        _ => ChangeKind::Changed,
    }
}

fn property_rows(change: &FoldedChange, schema: Option<&SchemaModel>) -> Vec<PropertyRow> {
    change
        .card_diffs(schema)
        .into_iter()
        .map(|diff| PropertyRow::new(diff.name, diff.before.as_ref(), diff.after.as_ref()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A type's row names no properties: each entity's row says which of its changed, so a
    /// type's row says only how many relationships a run of unchanged entities carries.
    #[test]
    fn a_types_row_leaves_the_properties_to_its_entities() {
        let run = |changed, relationships| GroupRun {
            section: None,
            type_name: "Capability".into(),
            is_edge: false,
            groups: vec![0, 1],
            added: 0,
            changed,
            removed: 0,
            relationships,
        };
        assert_eq!(run_detail(&run(2, 3)), "");
        assert_eq!(run_detail(&run(0, 3)), "with 3 relationships");
        let one = GroupRun {
            groups: vec![0],
            ..run(0, 3)
        };
        assert_eq!(run_detail(&one), "", "its entity's row says it");
    }

    /// A card of many relationships shows its first few, more each time they are asked for,
    /// and always every line through the one selected.
    #[test]
    fn a_card_shows_its_first_relationships_and_more_when_asked() {
        let mut lines = CardLines::default();
        let card = ("Person".to_owned(), "ada".to_owned());
        assert_eq!(lines.shown(&card, 5, None), 5);
        assert_eq!(lines.shown(&card, 1_400, None), CARD_LINES);
        assert_eq!(lines.shown(&card, 1_400, Some(3)), CARD_LINES);
        assert_eq!(lines.shown(&card, 1_400, Some(CARD_LINES)), CARD_LINES + 1);
        lines.show_more(card.clone(), CARD_LINES);
        assert_eq!(lines.shown(&card, 1_400, None), CARD_LINES + MORE_LINES);
        // Asked for while the selection showed more: more past those.
        lines.show_more(card.clone(), 600);
        assert_eq!(lines.shown(&card, 1_400, None), 600 + MORE_LINES);
        assert_eq!(lines.shown(&card, 700, None), 700);
        let other = ("Person".to_owned(), "grace".to_owned());
        assert_eq!(lines.shown(&other, 1_400, None), CARD_LINES);
        assert_eq!(more_lines_label(1), "Show 1 more relationship");
        assert_eq!(more_lines_label(150), "Show 150 more relationships");
        assert_eq!(
            more_lines_label(1_380),
            "Show 200 more of the 1,380 relationships not shown"
        );
    }

    /// A relationship that moved names the end it had before the end it has; when both its
    /// ends moved, who held this entity's end. A plain change names its other end alone, and a
    /// mirror says where it is reviewed.
    #[test]
    fn a_moved_relationship_names_the_end_it_had() {
        let entry = |net, other_was: Option<&str>, here_was: Option<&str>| RelationshipEntry {
            change: 0,
            net,
            words: "knows".into(),
            outgoing: false,
            other: "Bob".into(),
            other_type: "Person".into(),
            other_is_new: false,
            replaces: None,
            other_was: other_was.map(Into::into),
            here_was: here_was.map(Into::into),
        };
        let line = |entry: RelationshipEntry| {
            let (other, note) = other_end(&entry, "reviewed there");
            (other, note.map(|note| note.to_string()))
        };
        let changed = Some(NetChange::Changed);
        assert_eq!(
            line(entry(changed, Some("Ana"), None)),
            ("Ana → Bob".to_owned(), Some("moved".to_owned()))
        );
        assert_eq!(
            line(entry(changed, Some("Maya"), Some("Cleo"))),
            ("Maya → Bob".to_owned(), Some("moved from Cleo".to_owned()))
        );
        assert_eq!(
            line(RelationshipEntry {
                other_is_new: true,
                ..entry(changed, Some("Ana"), None)
            }),
            ("Ana → Bob".to_owned(), Some("moved, new".to_owned()))
        );
        assert_eq!(
            line(RelationshipEntry {
                other_is_new: true,
                ..entry(changed, Some("Maya"), Some("Cleo"))
            }),
            (
                "Maya → Bob".to_owned(),
                Some("moved from Cleo, new".to_owned())
            )
        );
        assert_eq!(line(entry(changed, None, None)), ("Bob".to_owned(), None));
        assert_eq!(
            line(entry(None, None, None)),
            ("Bob".to_owned(), Some("reviewed there".to_owned()))
        );
    }

    /// A change's rows are made the first time they are asked for and shared after.
    #[test]
    fn a_changes_rows_are_made_once() {
        let rows = CardRows::default();
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            vec![PropertyRow::new("name", None, None)]
        };
        let first = rows.get(3, make);
        let again = rows.get(3, make);
        assert!(Rc::ptr_eq(&first, &again));
        assert_eq!(made.get(), 1);
        rows.get(4, make);
        assert_eq!(made.get(), 2, "another change makes its own");
    }
}

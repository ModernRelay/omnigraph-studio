//! The entity cards of a review and of a commit's view in History (spec 008, 009): an entity's
//! card from its folded change, the lines of the relationship changes reviewed on it, and the
//! header above a run of one type's cards.
//!
//! Layer 4: it puts `graph_store`'s changes by entity onto `studio_ui`'s components, which
//! neither of those layers can do alone, so the two views draw the same cards from one place.
//! What a card can do (review, revert, edit) stays with the view that offers it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui::{App, Div, ElementId};
use gq_language::SchemaModel;
use graph_store::{
    EntityGroup, EntityGroups, FoldedChange, FoldedChanges, GroupRun, GroupSection, NetChange,
    NodeNames, RelationshipEntry, entity_name,
};
use studio_ui::ds::space;
use studio_ui::{
    ChangeCard, ChangeKind, PropertyRow, RelationshipLine, change_group_header, group_digits,
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

/// The card for change `ix` of `folded`: its kind, id and changed property rows, and a lone
/// relationship's ends, with a row for each end it moved from ([`FoldedChange::moved_ends`]).
fn change_card(
    folded: &FoldedChanges,
    ix: usize,
    schema: Option<&SchemaModel>,
    rows: &CardRows,
    id: impl Into<ElementId>,
) -> ChangeCard {
    let change = &folded.changes[ix];
    let mut card =
        ChangeCard::new(id, kind_of(change.net()), change.id.clone()).rows(rows.get(ix, || {
            let mut rows = moved_end_rows(change);
            rows.extend(property_rows(change, schema));
            rows
        }));
    if let Some((from, to)) = change.endpoints() {
        card = card.endpoints(from, to);
    }
    card
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

/// The card for group `ix`: its own change named by its label, or for a node standing
/// unchanged for its relationships, its id and the name read for it.
pub fn entity_card(
    groups: &EntityGroups,
    folded: &FoldedChanges,
    ix: usize,
    schema: Option<&SchemaModel>,
    names: &NodeNames,
    rows: &CardRows,
    id: impl Into<ElementId>,
) -> ChangeCard {
    let group = &groups.groups[ix];
    match group.change {
        Some(own) => change_card(folded, own, schema, rows, id)
            .name(entity_name(&folded.changes[own], schema)),
        None => ChangeCard::new(id, ChangeKind::Unchanged, group.id.clone()).name(
            names
                .get(&(group.type_name.clone(), group.id.clone()))
                .cloned(),
        ),
    }
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

/// The header above a run of one type's cards: the section's title when the run opens it, in
/// the view's words (`section_title`), then the type with its counts and how many
/// relationship changes its cards carry. The view gives it its own side padding.
pub fn run_header(
    run: &GroupRun,
    section_title: impl FnOnce(GroupSection) -> String,
    cx: &App,
) -> Div {
    v_flex()
        .children(run.section.map(|section| {
            div()
                .pt(px(space::XL))
                .child(studio_ui::ds::text::muted(section_title(section)))
        }))
        .child(
            change_group_header(
                &run.type_name,
                run.is_edge,
                run.added,
                run.changed,
                run.removed,
                cx,
            )
            .when(run.relationships > 0, |d| {
                d.child(studio_ui::ds::text::meta(match run.relationships {
                    1 => "· 1 relationship".to_owned(),
                    n => format!("· {} relationships", group_digits(n)),
                }))
            }),
        )
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

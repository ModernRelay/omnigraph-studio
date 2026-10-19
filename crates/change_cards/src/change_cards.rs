//! The entity cards of a review and of a commit's view in History (spec 008, 009): an entity's
//! card from its folded change, the lines of the relationship changes reviewed on it, and the
//! header above a run of one type's cards.
//!
//! Layer 4: it puts `graph_store`'s changes by entity onto `studio_ui`'s components, which
//! neither of those layers can do alone, so the two views draw the same cards from one place.
//! What a card can do (review, revert, edit) stays with the view that offers it.

use gpui::{App, Div, ElementId};
use gq_language::SchemaModel;
use graph_store::{
    EntityGroups, FoldedChange, FoldedChanges, GroupRun, GroupSection, NetChange, NodeNames,
    RelationshipEntry, entity_name,
};
use studio_ui::ds::space;
use studio_ui::{
    ChangeCard, ChangeKind, PropertyRow, RelationshipLine, change_group_header, group_digits,
};
use ui::prelude::*;

/// Relationship lines a card shows until it is opened: a node that gained hundreds of
/// relationships stays one readable card, and one row its list lays out quickly.
pub const CARD_LINES: usize = 20;

/// How many of a card's `total` relationship lines show: all once the card is opened or when
/// the selected one (`selected_at`) is past the first [`CARD_LINES`], else at most those.
pub fn lines_shown(total: usize, open: bool, selected_at: Option<usize>) -> usize {
    if open || selected_at.is_some_and(|at| at >= CARD_LINES) {
        total
    } else {
        total.min(CARD_LINES)
    }
}

/// What the button below a card's first lines says.
pub fn more_lines_label(left_out: usize) -> String {
    match left_out {
        1 => "Show 1 more relationship".to_owned(),
        n => format!("Show {} more relationships", group_digits(n)),
    }
}

/// The card for one folded change: its kind, id and changed property rows, and a lone
/// relationship's ends.
pub fn change_card(
    change: &FoldedChange,
    schema: Option<&SchemaModel>,
    id: impl Into<ElementId>,
) -> ChangeCard {
    let mut card = ChangeCard::new(id, kind_of(change.net()), change.id.clone())
        .rows(property_rows(change, schema));
    if let Some((from, to)) = change.endpoints() {
        card = card.endpoints(from, to);
    }
    card
}

/// The card for group `ix`: its own change named by its label, or for a node standing
/// unchanged for its relationships, its id and the name read for it.
pub fn entity_card(
    groups: &EntityGroups,
    folded: &FoldedChanges,
    ix: usize,
    schema: Option<&SchemaModel>,
    names: &NodeNames,
    id: impl Into<ElementId>,
) -> ChangeCard {
    let group = &groups.groups[ix];
    match group.change {
        Some(own) => change_card(&folded.changes[own], schema, id)
            .name(entity_name(&folded.changes[own], schema)),
        None => ChangeCard::new(id, ChangeKind::Unchanged, group.id.clone()).name(
            names
                .get(&(group.type_name.clone(), group.id.clone()))
                .cloned(),
        ),
    }
}

/// A card's line for one relationship entry: what changed, which way it points, the other
/// end, and a changed relationship's property rows; for one moved to another end
/// ([`RelationshipEntry::replaces`], `was` its removed edge), both ends and the old edge's
/// values against the new one's. A mirror's line carries `mirror_note`, which says where it
/// is shown in full.
pub fn relationship_line(
    entry: RelationshipEntry,
    edge: &FoldedChange,
    was: Option<&FoldedChange>,
    schema: Option<&SchemaModel>,
    mirror_note: &'static str,
) -> RelationshipLine {
    let kind = entry.net.map(|net| kind_of(Some(net)));
    let id = if kind.is_some() {
        ("relationship", entry.change)
    } else {
        ("mirror", entry.change)
    };
    let rows = match kind {
        Some(_) => match was {
            Some(was) => property_rows(
                &FoldedChange {
                    before: was.before.clone(),
                    ..edge.clone()
                },
                schema,
            ),
            None => property_rows(edge, schema),
        },
        None => Vec::new(),
    };
    let note = match (kind, &entry.replaces) {
        (None, _) => Some(mirror_note),
        (Some(_), Some(_)) if entry.other_is_new => Some("moved, new"),
        (Some(_), Some(_)) => Some("moved"),
        (Some(_), None) if entry.other_is_new => Some("new"),
        (Some(_), None) => None,
    };
    let other = match &entry.replaces {
        Some((_, was)) => format!("{was} → {}", entry.other),
        None => entry.other,
    };
    let line = RelationshipLine::new(id, kind, entry.words, entry.outgoing, other)
        .other_detail(entry.other_type)
        .rows(rows);
    match note {
        Some(note) => line.note(note),
        None => line,
    }
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
            div().pt(px(space::XL)).child(
                Label::new(section_title(section))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
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
                d.child(
                    Label::new(match run.relationships {
                        1 => "· 1 relationship".to_owned(),
                        n => format!("· {} relationships", group_digits(n)),
                    })
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                )
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
        .map(|diff| PropertyRow {
            name: diff.name.into(),
            before: diff.before,
            after: diff.after,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A card of many relationships shows its first few until opened, or until the one selected
    /// is past them.
    #[test]
    fn a_card_shows_its_first_relationships_until_opened() {
        assert_eq!(lines_shown(5, false, None), 5);
        assert_eq!(lines_shown(400, false, None), CARD_LINES);
        assert_eq!(lines_shown(400, false, Some(3)), CARD_LINES);
        assert_eq!(lines_shown(400, false, Some(CARD_LINES)), 400);
        assert_eq!(lines_shown(400, true, None), 400);
        assert_eq!(more_lines_label(1), "Show 1 more relationship");
        assert_eq!(more_lines_label(1_380), "Show 1,380 more relationships");
    }
}

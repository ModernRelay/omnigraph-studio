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
    let name = Label::new(row.name.clone())
        .size(LabelSize::Small)
        .color(Color::Muted)
        .single_line()
        .truncate();
    let body = match kind {
        ChangeKind::Changed => h_flex()
            .flex_1()
            .min_w_0()
            .gap(px(space::MD))
            .child(div().min_w_0().max_w(relative(0.48)).child(value_label(
                &row.before,
                Color::Deleted,
                true,
            )))
            .child(Label::new("→").size(LabelSize::Small).color(Color::Muted))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(value_label(&row.after, Color::Created, false)),
            ),
        ChangeKind::Added | ChangeKind::Unchanged => h_flex()
            .flex_1()
            .min_w_0()
            .child(value_label(&row.after, Color::Default, false)),
        ChangeKind::Removed => {
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

fn value_label(value: &Option<Value>, color: Color, strike: bool) -> Label {
    let text = match value {
        Some(value) => format_value(value),
        None => "—".into(),
    };
    let mut label = Label::new(text)
        .size(LabelSize::Small)
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
        let colors = cx.theme().colors();
        let accent = self.kind.color().color(cx);
        let border = if self.selected {
            colors.border_focused
        } else {
            crate::ds::surface::rule(cx)
        };
        let kind = self.kind;
        let header = h_flex()
            .id("change-kind")
            .gap(px(space::INLINE))
            .min_w_0()
            .tooltip(ui::Tooltip::text(kind.label()))
            .child(
                Label::new(kind.symbol())
                    .size(LabelSize::Small)
                    .color(kind.color()),
            )
            .map(|header| match &self.endpoints {
                Some((from, to)) => header.child(
                    Label::new(format!("{from} → {to}"))
                        .size(LabelSize::Small)
                        .buffer_font(cx)
                        .single_line()
                        .truncate(),
                ),
                None => match &self.name {
                    Some(name) => header
                        .child(
                            Label::new(name.clone())
                                .size(LabelSize::Small)
                                .single_line()
                                .truncate(),
                        )
                        .child(
                            Label::new(self.entity_id.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .buffer_font(cx)
                                .single_line()
                                .truncate(),
                        ),
                    None => header.child(
                        Label::new(self.entity_id.clone())
                            .size(LabelSize::Small)
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
                    .map(|(text, color)| Label::new(text).size(LabelSize::XSmall).color(color)),
            )
            .children(self.actions);
        let rows = self.rows.iter().map(|row| property_row(row, kind));
        let (added, changed, removed) = self.relationship_counts;
        let relationships = (!self.relationships.is_empty()).then(|| {
            v_flex()
                .pt(px(space::XS))
                .gap(px(space::WITHIN))
                .child(
                    h_flex()
                        .gap(px(space::MD))
                        .child(
                            Label::new("Relationships")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(change_counts(added, changed, removed)),
                )
                .children(self.relationships)
        });
        let card = h_flex()
            .id(self.id)
            .w_full()
            .rounded(px(radii(cx).md))
            .border_1()
            .border_color(border)
            .overflow_hidden()
            .child(
                div()
                    .w(px(2.))
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
                    .when(self.hollow, |d| d.opacity(0.6))
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
        let colors = cx.theme().colors();
        let mirror = self.kind.is_none();
        let symbol = match self.kind {
            Some(kind) => Label::new(kind.symbol())
                .size(LabelSize::Small)
                .color(kind.color()),
            None => Label::new("·").size(LabelSize::Small).color(Color::Muted),
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
                    .child(
                        Label::new(self.label)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .single_line()
                            .truncate(),
                    ),
            )
            .child(
                Label::new(if self.outgoing { "→" } else { "←" })
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(space::INLINE))
                    .child(
                        Label::new(self.other)
                            .size(LabelSize::Small)
                            .color(if mirror { Color::Muted } else { Color::Default })
                            .single_line()
                            .truncate(),
                    )
                    .children(self.other_detail.map(|detail| {
                        Label::new(detail)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .single_line()
                    })),
            )
            .children(
                self.note
                    .map(|note| Label::new(note).size(LabelSize::XSmall).color(Color::Muted)),
            )
            .children(
                self.status
                    .map(|(text, color)| Label::new(text).size(LabelSize::XSmall).color(color)),
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
            .when(self.selected, |d| d.bg(colors.element_selected))
            .when(!self.selected, |d| d.hover(|d| d.bg(colors.element_hover)))
            .when(self.hollow, |d| d.opacity(0.6))
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
            Label::new(format!("{}{}", kind.symbol(), crate::group_digits(count)))
                .size(LabelSize::Small)
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
        .child(Label::new(type_name.to_owned()).size(LabelSize::Small))
        .child(
            Label::new(if is_edge { "relationship" } else { "node" })
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .child(change_counts(added, changed, removed))
}

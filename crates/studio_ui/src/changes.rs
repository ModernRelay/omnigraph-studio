//! Entity change cards shared by review and history (spec 008, 009): one card per changed
//! entity, its changed properties before and after, and per-type counts.

use std::rc::Rc;

use crate::ds::space;
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
}

impl ChangeKind {
    pub fn symbol(self) -> &'static str {
        match self {
            ChangeKind::Added => "+",
            ChangeKind::Changed => "~",
            ChangeKind::Removed => "−",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ChangeKind::Added => "Added",
            ChangeKind::Changed => "Changed",
            ChangeKind::Removed => "Removed",
        }
    }

    pub fn color(self) -> Color {
        match self {
            ChangeKind::Added => Color::Created,
            ChangeKind::Changed => Color::Modified,
            ChangeKind::Removed => Color::Deleted,
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

/// A card for one changed entity, under its type's [`change_group_header`]: the kind's symbol
/// and the entity's id, then before → after per changed property for a changed entity, the
/// values of an added one, the last values of a removed one, struck through. The type is the
/// group's, so a card does not repeat it.
#[derive(IntoElement)]
pub struct ChangeCard {
    id: ElementId,
    kind: ChangeKind,
    entity_id: SharedString,
    endpoints: Option<(SharedString, SharedString)>,
    rows: Vec<PropertyRow>,
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
            endpoints: None,
            rows: Vec::new(),
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
                None => header.child(
                    Label::new(self.entity_id.clone())
                        .size(LabelSize::Small)
                        .buffer_font(cx)
                        .single_line()
                        .truncate(),
                ),
            })
            .child(div().flex_1())
            .children(
                self.status
                    .clone()
                    .map(|(text, color)| Label::new(text).size(LabelSize::XSmall).color(color)),
            )
            .children(self.actions);
        let rows = self.rows.iter().map(|row| {
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
                    .child(div().flex_1().min_w_0().child(value_label(
                        &row.after,
                        Color::Created,
                        false,
                    ))),
                ChangeKind::Added => h_flex().flex_1().min_w_0().child(value_label(
                    &row.after,
                    Color::Default,
                    false,
                )),
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
        });
        let card = h_flex()
            .id(self.id)
            .w_full()
            .rounded_md()
            .border_1()
            .border_color(border)
            .overflow_hidden()
            .child(
                div()
                    .w(px(2.))
                    .h_full()
                    .flex_none()
                    .when(!self.hollow, |d| d.bg(accent)),
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
                    .children(rows),
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
) -> Div {
    h_flex()
        .gap(px(space::MD))
        .pt(px(space::LG))
        .pb(px(space::XS))
        .child(crate::kind_swatch(type_name, is_edge))
        .child(Label::new(type_name.to_owned()).size(LabelSize::Small))
        .child(
            Label::new(if is_edge { "relationship" } else { "node" })
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .child(change_counts(added, changed, removed))
}

//! Entity change cards shared by review and history (spec 008, 009): one card per changed
//! entity, its changed properties before and after, and per-type counts.

use std::rc::Rc;

use gpui::{AnyElement, App, ClickEvent, Div, ElementId, SharedString, Window};
use serde_json::Value;
use ui::prelude::*;

use crate::{format_value, type_swatch};

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

/// A card for one changed entity. Changed entities show before → after per changed property;
/// added ones their values; removed ones their last values, struck through, as a real row.
#[derive(IntoElement)]
pub struct ChangeCard {
    id: ElementId,
    kind: ChangeKind,
    type_name: SharedString,
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
        type_name: impl Into<SharedString>,
        entity_id: impl Into<SharedString>,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            type_name: type_name.into(),
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

fn value_label(value: &Option<Value>, color: Color, strike: bool, cx: &App) -> Label {
    let text = match value {
        Some(value) => format_value(value),
        None => "—".into(),
    };
    let mut label = Label::new(text)
        .size(LabelSize::Small)
        .buffer_font(cx)
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
            colors.border_variant
        };
        let background = if self.hollow {
            colors.editor_background
        } else {
            colors.surface_background
        };
        let kind = self.kind;
        let header = h_flex()
            .gap_2()
            .min_w_0()
            .child(
                Label::new(format!("{} {}", kind.symbol(), kind.label()))
                    .size(LabelSize::Small)
                    .color(kind.color()),
            )
            .child(type_swatch(&self.type_name))
            .child(
                Label::new(self.type_name.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
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
                    .gap_2()
                    .child(div().min_w_0().max_w(relative(0.48)).child(value_label(
                        &row.before,
                        Color::Deleted,
                        true,
                        cx,
                    )))
                    .child(Label::new("→").size(LabelSize::Small).color(Color::Muted))
                    .child(div().flex_1().min_w_0().child(value_label(
                        &row.after,
                        Color::Created,
                        false,
                        cx,
                    ))),
                ChangeKind::Added => h_flex().flex_1().min_w_0().child(value_label(
                    &row.after,
                    Color::Default,
                    false,
                    cx,
                )),
                ChangeKind::Removed => h_flex().flex_1().min_w_0().child(value_label(
                    &row.before,
                    Color::Muted,
                    true,
                    cx,
                )),
            };
            h_flex()
                .gap_3()
                .min_w_0()
                .child(div().w(px(150.)).flex_none().child(name))
                .child(body)
        });
        let card = h_flex()
            .id(self.id)
            .w_full()
            .rounded_md()
            .border_1()
            .border_color(border)
            .bg(background)
            .overflow_hidden()
            .child(
                div()
                    .w(px(3.))
                    .h_full()
                    .flex_none()
                    .when(!self.hollow, |d| d.bg(accent)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .px_3()
                    .py_2()
                    .gap_1()
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
        .gap_1p5()
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
        .gap_2()
        .pt_3()
        .pb_1()
        .child(crate::kind_swatch(type_name, is_edge))
        .child(Label::new(type_name.to_owned()).size(LabelSize::Small))
        .child(
            Label::new(if is_edge { "relationship" } else { "node" })
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .child(change_counts(added, changed, removed))
}

//! The filters on a relationship's neighbours (spec 005, "Filtering a relationship's
//! neighbours"): a popover from the group's row that lists the filters set, each a pill that
//! removes itself, and takes one more as a property, a test and a value. The filters belong to
//! the relationship, as a grouping does, so they narrow it wherever it appears in the walk:
//! its counts, its pages and its grouped values.

use std::sync::Arc;

use editor::Editor;
use gpui::{
    AnyElement, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    MouseDownEvent, Render, Role, Window,
};
use gq_language::builder::{Filter, FilterOp};
use gq_language::input::{edit_text, parse_input};
use gq_language::{NodeType, Property, RelationshipGroup, SchemaModel};
use graph_store::Exploration;
use studio_ui::ds::Tipped;
use studio_ui::ds::{size, space};
use studio_ui::menu::{ContextMenu, menu_header, menu_surface};
use ui::prelude::*;
use ui::{IconName, IconPosition, PopoverMenu, PopoverMenuHandle};

/// The popover: the relationship's filters as pills, and a draft for the next.
pub(crate) struct NeighbourFilters {
    exploration: Entity<Exploration>,
    group: RelationshipGroup,
    /// The other type, whose properties the filters test.
    other: Arc<NodeType>,
    /// The draft: a property and a test; its value is the editor's text.
    property: String,
    op: FilterOp,
    pub(crate) value: Entity<Editor>,
    pub(crate) error: Option<String>,
    menus: [PopoverMenuHandle<ContextMenu>; 2],
    focus_handle: FocusHandle,
}

impl EventEmitter<DismissEvent> for NeighbourFilters {}

impl Focusable for NeighbourFilters {
    /// The value field: opening the popover puts the keyboard there, so a filter is typed at
    /// once; ↵ adds it and esc closes, through the surface's `menu` context.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.value.focus_handle(cx)
    }
}

/// Whether `group`'s neighbours can be filtered: their type is in the schema read, with a
/// property that takes a test.
pub(crate) fn can_filter(exploration: &Exploration, group: &RelationshipGroup) -> bool {
    exploration
        .schema()
        .and_then(|schema| {
            schema
                .node(&group.other_type)
                // Asked for each relationship row drawn: the first property with a test answers.
                .map(|other| {
                    other
                        .properties
                        .iter()
                        .any(|property| !FilterOp::for_property(property).is_empty())
                })
        })
        .unwrap_or(false)
}

/// The properties of `other` a filter can test: those with a test to offer, the key included.
pub(crate) fn filterable(other: &NodeType) -> Vec<&Property> {
    other
        .properties
        .iter()
        .filter(|property| !FilterOp::for_property(property).is_empty())
        .collect()
}

/// A filter in words, as the group's row says it: "city is London", "born after 1900".
pub(crate) fn filter_words(filter: &Filter) -> String {
    let mut words = format!("{} {}", filter.property, filter.op.label());
    if filter.op.takes_value() {
        words.push(' ');
        words.push_str(&edit_text(&filter.value));
    }
    words
}

/// The icon of a property's kind, as the table's pills show it.
fn property_icon(property: &Property) -> IconName {
    use gq_language::ScalarKind;
    if property.enum_values.is_some() {
        return IconName::ListTodo;
    }
    match property.kind {
        ScalarKind::Text => IconName::CaseSensitive,
        ScalarKind::Bool => IconName::Check,
        ScalarKind::Integer | ScalarKind::Float => IconName::Hash,
        ScalarKind::Date | ScalarKind::DateTime => IconName::Clock,
        ScalarKind::Vector => IconName::Sparkle,
        ScalarKind::Blob => IconName::File,
    }
}

impl NeighbourFilters {
    /// The popover for `group`'s neighbours in `exploration`, which [`can_filter`] says can be
    /// filtered.
    pub(crate) fn new(
        exploration: Entity<Exploration>,
        group: RelationshipGroup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let schema: Arc<SchemaModel> = exploration
            .read(cx)
            .schema()
            .expect("a schema, as can_filter checked");
        let other = Arc::new(
            schema
                .node(&group.other_type)
                .expect("the other type, as can_filter checked")
                .clone(),
        );
        // The draft starts on the property that names the neighbours, as a filter mostly
        // does; else the first that takes a test.
        let first = other
            .label_property()
            .and_then(|label| filterable(&other).into_iter().find(|p| p.name == label))
            .unwrap_or_else(|| filterable(&other)[0]);
        let property = first.name.clone();
        let op = FilterOp::for_property(first)[0];
        let value = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Value", window, cx);
            editor.set_text_style_refinement(studio_ui::ds::field_text(cx));
            editor
        });
        // The keyboard goes to the value at once.
        window.focus(&value.focus_handle(cx), cx);
        Self {
            exploration,
            group,
            other,
            property,
            op,
            value,
            error: None,
            menus: [PopoverMenuHandle::default(), PopoverMenuHandle::default()],
            focus_handle: cx.focus_handle(),
        }
    }

    /// The filters set on the relationship now.
    fn filters(&self, cx: &App) -> Vec<Filter> {
        self.exploration.read(cx).filters(&self.group).to_vec()
    }

    fn property(&self) -> Option<&Property> {
        self.other
            .properties
            .iter()
            .find(|property| property.name == self.property)
    }

    pub(crate) fn set_property(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(property) = self.other.properties.iter().find(|p| p.name == name) else {
            return;
        };
        let ops = FilterOp::for_property(property);
        if !ops.contains(&self.op) {
            self.op = ops.first().copied().unwrap_or(self.op);
        }
        self.property = name;
        self.error = None;
        cx.notify();
    }

    pub(crate) fn set_op(&mut self, op: FilterOp, cx: &mut Context<Self>) {
        self.op = op;
        self.error = None;
        cx.notify();
    }

    /// Adds the draft as a filter: the value as typed, parsed as the property's kind, or none
    /// for a test that takes none. An empty value for a test that takes one is a notice, not
    /// a filter.
    pub(crate) fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(property) = self.property().cloned() else {
            return;
        };
        let text = self.value.read(cx).text(cx);
        let value = if self.op.takes_value() {
            if text.trim().is_empty() {
                self.error = Some("Type a value to filter by.".into());
                cx.notify();
                return;
            }
            // A list is tested one element at a time, and `in` takes several values.
            let as_list = self.op == FilterOp::In;
            let element = Property {
                list: as_list,
                nullable: false,
                ..property.clone()
            };
            match parse_input(&element, &text) {
                Ok(value) => value,
                Err(error) => {
                    self.error = Some(error);
                    cx.notify();
                    return;
                }
            }
        } else {
            serde_json::Value::Null
        };
        let mut filters = self.filters(cx);
        filters.push(Filter {
            property: property.name,
            op: self.op,
            value,
        });
        self.exploration.update(cx, |exploration, cx| {
            exploration.set_filters(&self.group, filters, cx)
        });
        self.value
            .update(cx, |editor, cx| editor.set_text("", window, cx));
        self.error = None;
        cx.notify();
    }

    pub(crate) fn remove(&mut self, index: usize, cx: &mut Context<Self>) {
        let mut filters = self.filters(cx);
        if index < filters.len() {
            filters.remove(index);
            self.exploration.update(cx, |exploration, cx| {
                exploration.set_filters(&self.group, filters, cx)
            });
            cx.notify();
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.add(window, cx);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    /// The property menu's trigger and menu: the other type's filterable properties.
    fn render_property_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let other = self.other.clone();
        let current = self.property.clone();
        PopoverMenu::new("neighbour-filter-property")
            .with_handle(self.menus[0].clone())
            .trigger(
                studio_ui::ds::button("neighbour-filter-property-button", current.clone())
                    .start_icon(Icon::new(
                        self.property().map_or(IconName::Filter, property_icon),
                    ))
                    .end_icon(Icon::new(IconName::ChevronDown)),
            )
            .menu(move |window, cx| {
                let (this, other, current) = (this.clone(), other.clone(), current.clone());
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for property in filterable(&other) {
                        let name = property.name.clone();
                        let this = this.clone();
                        menu = menu.toggleable_entry(
                            name.clone(),
                            name == current,
                            IconPosition::Start,
                            None,
                            move |_, cx| {
                                this.update(cx, |this, cx| this.set_property(name.clone(), cx))
                                    .ok();
                            },
                        );
                    }
                    menu
                }))
            })
            .into_any_element()
    }

    /// The test menu's trigger and menu: the tests the draft's property takes.
    fn render_op_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let ops = self
            .property()
            .map(FilterOp::for_property)
            .unwrap_or_default();
        let current = self.op;
        PopoverMenu::new("neighbour-filter-op")
            .with_handle(self.menus[1].clone())
            .trigger(
                studio_ui::ds::button("neighbour-filter-op-button", current.label())
                    .end_icon(Icon::new(IconName::ChevronDown)),
            )
            .menu(move |window, cx| {
                let (this, ops) = (this.clone(), ops.clone());
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for op in ops {
                        let this = this.clone();
                        menu = menu.toggleable_entry(
                            op.label(),
                            op == current,
                            IconPosition::Start,
                            None,
                            move |_, cx| {
                                this.update(cx, |this, cx| this.set_op(op, cx)).ok();
                            },
                        );
                    }
                    menu
                }))
            })
            .into_any_element()
    }

    fn render_pills(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let other = self.other.clone();
        self.filters(cx)
            .into_iter()
            .enumerate()
            .map(|(index, filter)| {
                let icon = other
                    .properties
                    .iter()
                    .find(|property| property.name == filter.property)
                    .map_or(IconName::Filter, property_icon);
                let value = filter
                    .op
                    .takes_value()
                    .then(|| SharedString::from(edit_text(&filter.value)));
                studio_ui::ds::filter_pill(
                    ("neighbour-filter", index),
                    icon,
                    filter.property.clone(),
                    filter.op.label(),
                    value,
                    cx.listener(move |this, _, _, cx| this.remove(index, cx)),
                    cx,
                )
                .into_any_element()
            })
            .collect()
    }
}

impl Render for NeighbourFilters {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pills = self.render_pills(cx);
        let takes_value = self.op.takes_value();
        // The row names the relationship; the popover names what its filters keep.
        let title = self.group.other_type.clone();
        // The draft's editor takes the keyboard, so typing goes to the value; the menus and
        // the Add button are a click away.
        let field = studio_ui::ds::field_frame(self.error.is_some(), cx)
            .flex_1()
            .child(studio_ui::ds::field_input(self.value.clone()));
        menu_surface(v_flex(), cx)
            .id("neighbour-filters")
            .role(Role::Menu)
            .key_context("menu")
            .track_focus(&self.focus_handle)
            .occlude()
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                // A menu of the draft open takes the click; the popover stays for it.
                if !this.menus.iter().any(|menu| menu.is_deployed()) {
                    cx.emit(DismissEvent);
                }
            }))
            .w(px(size::STATE_DETAIL_WIDTH))
            .p(px(space::XS))
            .gap(px(space::XS))
            .child(menu_header(SharedString::from(format!(
                "Only {title} where"
            ))))
            .when(!pills.is_empty(), |menu| {
                menu.child(
                    h_flex()
                        .flex_wrap()
                        .px(px(space::MD))
                        .gap(px(space::SM))
                        .children(pills),
                )
            })
            .child(
                h_flex()
                    .px(px(space::MD))
                    .py(px(space::XS))
                    .gap(px(space::SM))
                    .items_center()
                    .child(self.render_property_menu(cx))
                    .child(self.render_op_menu(cx))
                    .when(takes_value, |row| row.child(field))
                    .child(
                        studio_ui::ds::button("neighbour-filter-add", "Add")
                            .tip(studio_ui::ds::tooltip("Add this filter (↵)"))
                            .on_click(cx.listener(|this, _, window, cx| this.add(window, cx))),
                    ),
            )
            .when_some(self.error.clone(), |menu, error| {
                menu.child(
                    div()
                        .px(px(space::MD))
                        .pb(px(space::XS))
                        .child(studio_ui::ds::text::error(error)),
                )
            })
    }
}

/// What opens the popover: the group row's filter button, lit while filters are set.
pub(crate) fn filter_button(
    id: impl Into<ElementId>,
    set: bool,
    exploration: Entity<Exploration>,
    group: RelationshipGroup,
) -> PopoverMenu<NeighbourFilters> {
    let tooltip = if set {
        "Filter these neighbours: some are left out"
    } else {
        "Filter these neighbours by a property"
    };
    PopoverMenu::new(SharedString::from(format!("{}-filters", group.edge)))
        .trigger(
            studio_ui::ds::row_icon_button(id, IconName::Filter)
                .icon_color(Color::Muted)
                .toggle_state(set)
                .tip(studio_ui::ds::tooltip(tooltip)),
        )
        .anchor(gpui::Anchor::TopRight)
        .menu(move |window, cx| {
            // A type with nothing to filter by opens no popover.
            if !can_filter(exploration.read(cx), &group) {
                return None;
            }
            let (exploration, group) = (exploration.clone(), group.clone());
            Some(cx.new(|cx| NeighbourFilters::new(exploration, group, window, cx)))
        })
}

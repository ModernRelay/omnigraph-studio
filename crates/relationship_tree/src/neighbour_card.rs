//! A relationship's card (spec 005, "A relationship's card"): a popover from the group's row
//! that groups, orders and filters its neighbours, and takes a sentence for the model to set
//! all three. Its rows are Group, Sort and Filter; its grey footer is the sentence field, as
//! the type table's toolbar has one (spec 019). What it sets belongs to the relationship, as a
//! grouping does, so it holds wherever the relationship appears in the walk: its counts, its
//! pages and its grouped values.

use std::sync::Arc;

use editor::Editor;
use filter_words as words;
use gpui::Task;
use gpui::{
    AnyElement, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    MouseDownEvent, Render, Role, Window,
};
use gq_language::builder::{Filter, FilterOp, GroupBy, Sort};
use gq_language::input::{edit_text, parse_input};
use gq_language::{NodeType, Property, RelationshipGroup, SchemaModel};
use graph_store::{Exploration, ExplorePath};
use studio_ui::ds::Tipped;
use studio_ui::ds::{size, space};
use studio_ui::menu::{ContextMenu, menu_surface};
use studio_workspace::models::{Error as ModelError, Models, Request as ModelRequest};
use ui::prelude::*;
use ui::{IconName, IconPosition, PopoverMenu, PopoverMenuHandle};

/// Where the card's sentence is: being typed, out with the model, or answered with a note of
/// what could not be used, or failed with why.
pub(crate) enum Words {
    Typing(Option<String>),
    Running { _call: Task<()> },
    Failed(String),
}

/// One line of a card's menu: a heading over the choices after it, or a choice.
#[derive(Clone)]
enum Choice<T> {
    Section(SharedString),
    Item {
        label: SharedString,
        value: T,
        checked: bool,
    },
}

impl<T> Choice<T> {
    fn item(label: impl Into<SharedString>, value: T, checked: bool) -> Self {
        Choice::Item {
            label: label.into(),
            value,
            checked,
        }
    }
}

/// The card: the relationship's grouping and order, its filters as pills with a draft for the
/// next, and the sentence field.
pub(crate) struct NeighbourCard {
    exploration: Entity<Exploration>,
    group: RelationshipGroup,
    /// The group's path where the card was opened: choosing a grouping opens it on its values.
    path: Option<ExplorePath>,
    schema: Arc<SchemaModel>,
    /// The other type, whose properties the rows test, order and group by.
    other: Arc<NodeType>,
    /// The draft: a property and a test; its value is the editor's text.
    property: String,
    op: FilterOp,
    pub(crate) value: Entity<Editor>,
    pub(crate) error: Option<String>,
    /// The sentence for the model, in the footer.
    pub(crate) sentence: Entity<Editor>,
    pub(crate) words: Words,
    /// The draft's property and test, the grouping and the order: a click on a menu open
    /// leaves the card open.
    menus: [PopoverMenuHandle<ContextMenu>; 4],
    focus_handle: FocusHandle,
}

impl EventEmitter<DismissEvent> for NeighbourCard {}

impl Focusable for NeighbourCard {
    /// The sentence field: opening the card puts the keyboard there, so what to show is typed
    /// at once; ↵ asks (or, in the value field, adds the filter) and esc closes, through the
    /// surface's `menu` context.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.sentence.focus_handle(cx)
    }
}

/// Whether `group` has a card: its other type is in the schema read.
pub(crate) fn can_open(exploration: &Exploration, group: &RelationshipGroup) -> bool {
    exploration
        .schema()
        .is_some_and(|schema| schema.node(&group.other_type).is_some())
}

/// The properties of `other` its neighbours can be ordered by.
pub(crate) fn sortable(other: &NodeType) -> Vec<&Property> {
    other
        .properties
        .iter()
        .filter(|property| Sort::allowed(property))
        .collect()
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

impl NeighbourCard {
    /// The card for `group`'s neighbours in `exploration`, which [`can_open`] says has one;
    /// `path` is the group's row it was opened from.
    pub(crate) fn new(
        exploration: Entity<Exploration>,
        group: RelationshipGroup,
        path: Option<ExplorePath>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let schema: Arc<SchemaModel> = exploration
            .read(cx)
            .schema()
            .expect("a schema, as can_open checked");
        let other = Arc::new(
            schema
                .node(&group.other_type)
                .expect("the other type, as can_open checked")
                .clone(),
        );
        // The draft starts on the property that names the neighbours, as a filter mostly
        // does; else the first that takes a test. A type with none has no draft.
        let first = other
            .label_property()
            .and_then(|label| filterable(&other).into_iter().find(|p| p.name == label))
            .or_else(|| filterable(&other).into_iter().next());
        let property = first.map(|p| p.name.clone()).unwrap_or_default();
        let op = first
            .and_then(|p| FilterOp::for_property(p).first().copied())
            .unwrap_or(FilterOp::Eq);
        let field = |placeholder: String, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text(&placeholder, window, cx);
                editor.set_text_style_refinement(studio_ui::ds::field_text(cx));
                editor
            })
        };
        let value = field("Value".to_owned(), window, cx);
        let sentence = field(
            format!("Describe the {} to show", group.other_type),
            window,
            cx,
        );
        // The keyboard goes to the sentence at once: one line says what all three rows can.
        window.focus(&sentence.focus_handle(cx), cx);
        Self {
            exploration,
            group,
            path,
            schema,
            other,
            property,
            op,
            value,
            error: None,
            sentence,
            words: Words::Typing(None),
            menus: Default::default(),
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
        if self.sentence.focus_handle(cx).is_focused(window) {
            self.ask(cx);
        } else {
            self.add(window, cx);
        }
    }

    /// Groups the neighbours by `by`, or stops; a grouping chosen opens the row it was asked
    /// from on its values.
    pub(crate) fn set_grouping(&mut self, by: Option<GroupBy>, cx: &mut Context<Self>) {
        let (group, path) = (self.group.clone(), self.path.clone());
        self.exploration.update(cx, |exploration, cx| {
            let open = by.is_some();
            exploration.set_grouping(&group, by, cx);
            if open && let Some(path) = &path {
                exploration.expand(path, cx);
            }
        });
        cx.notify();
    }

    /// Orders the neighbours by `property`, keeping the direction set, or stops.
    pub(crate) fn set_sort(&mut self, property: Option<String>, cx: &mut Context<Self>) {
        let descending = self
            .exploration
            .read(cx)
            .sort(&self.group)
            .is_some_and(|sort| sort.descending);
        let sort = property.map(|property| Sort {
            property,
            descending,
        });
        let group = self.group.clone();
        self.exploration
            .update(cx, |exploration, cx| exploration.set_sort(&group, sort, cx));
        cx.notify();
    }

    /// Turns the order around.
    pub(crate) fn flip_sort(&mut self, cx: &mut Context<Self>) {
        let Some(sort) = self.exploration.read(cx).sort(&self.group).cloned() else {
            return;
        };
        let group = self.group.clone();
        self.exploration.update(cx, |exploration, cx| {
            exploration.set_sort(
                &group,
                Some(Sort {
                    descending: !sort.descending,
                    ..sort
                }),
                cx,
            )
        });
        cx.notify();
    }

    /// Every grouping the relationship offers, its sections run together.
    fn groupings(&self) -> Vec<GroupBy> {
        GroupBy::sections(&self.schema, &self.group)
            .into_iter()
            .flat_map(|(_, section)| section)
            .collect()
    }

    /// The keys a sentence may filter by: the other type's properties that take a test.
    pub(crate) fn choices(&self) -> Vec<words::Choice> {
        filterable(&self.other)
            .into_iter()
            .map(|property| words::Choice {
                key: property.name.clone(),
                label: property.name.clone(),
                ops: FilterOp::for_property(property),
                property: Some(property.clone()),
                other_type: None,
                traversal: None,
            })
            .collect()
    }

    /// What a sentence may set beside its filters: an order by a property, and a grouping, by
    /// its label.
    pub(crate) fn offered(&self) -> words::ViewChoices {
        words::ViewChoices {
            sorts: sortable(&self.other)
                .into_iter()
                .map(|property| property.name.clone())
                .collect(),
            groups: self.groupings().iter().map(GroupBy::label).collect(),
            columns: Vec::new(),
        }
    }

    /// ↵ in the sentence field: the sentence goes to the model, which answers with the
    /// relationship's filters, order and grouping, each checked against what the card offers;
    /// what it could not use is said under the field.
    pub(crate) fn ask(&mut self, cx: &mut Context<Self>) {
        let sentence = self.sentence.read(cx).text(cx).trim().to_owned();
        if sentence.is_empty() || matches!(self.words, Words::Running { .. }) {
            return;
        }
        let Some(models) = Models::try_global(cx) else {
            self.words = Words::Failed(ModelError::NoKey.to_string());
            cx.notify();
            return;
        };
        let (choices, offered) = (self.choices(), self.offered());
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let request = ModelRequest {
            task: &words::TASK,
            instructions: words::INSTRUCTIONS,
            input: words::input(
                &self.group.other_type,
                &choices,
                &offered,
                &today,
                &sentence,
            ),
            schema: words::schema(&choices, &offered),
        };
        let call = models.read(cx).call::<words::Answer>(request, cx);
        let task = cx.spawn(async move |this, cx| {
            let answer = call.await;
            this.update(cx, |this, cx| {
                let answer = match answer {
                    Ok(answer) => answer.value,
                    Err(error) => {
                        this.words = Words::Failed(error.to_string());
                        cx.notify();
                        return;
                    }
                };
                let note = this.apply_answer(answer, &choices, &offered, cx);
                this.words = Words::Typing(note);
                cx.notify();
            })
            .ok();
        });
        self.words = Words::Running { _call: task };
        cx.notify();
    }

    /// Sets what an answer says: its filters in place of the relationship's, and its order
    /// and grouping when it names them. What could not be used comes back as a note.
    pub(crate) fn apply_answer(
        &mut self,
        answer: words::Answer,
        choices: &[words::Choice],
        offered: &words::ViewChoices,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let (view, mut refused) = words::view(&answer, offered);
        if answer
            .predicate
            .as_deref()
            .is_some_and(|predicate| !predicate.trim().is_empty())
        {
            refused.push("a condition the filters cannot say".to_owned());
        }
        let (filters, filters_refused) = words::filters(answer, choices);
        refused.extend(filters_refused);
        let group = self.group.clone();
        self.exploration.update(cx, |exploration, cx| {
            exploration.set_filters(&group, filters, cx)
        });
        if let Some(sort) = view.sort {
            self.exploration.update(cx, |exploration, cx| {
                exploration.set_sort(&group, Some(sort), cx)
            });
        }
        if let Some(label) = view.group_by {
            let by = self.groupings().into_iter().find(|by| by.label() == label);
            if by.is_some() {
                self.set_grouping(by, cx);
            }
        }
        (!refused.is_empty()).then(|| format!("Could not use: {}", refused.join("; ")))
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    /// A menu of the card: `trigger`, opening `items`, the one checked ticked; picking one
    /// calls `pick` with its value. Menu `handle` of the card's, so a click in it leaves the
    /// card open.
    fn choice_menu<T: Clone + 'static>(
        &self,
        id: &'static str,
        handle: usize,
        trigger: studio_ui::ds::Button,
        items: Vec<Choice<T>>,
        pick: fn(&mut Self, T, &mut Context<Self>),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        PopoverMenu::new(id)
            .with_handle(self.menus[handle].clone())
            .trigger(trigger.end_icon(Icon::new(IconName::ChevronDown)))
            .menu(move |window, cx| {
                let (this, items) = (this.clone(), items.clone());
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for item in items {
                        menu = match item {
                            Choice::Section(heading) => menu.separator().header(heading),
                            Choice::Item {
                                label,
                                value,
                                checked,
                            } => {
                                let this = this.clone();
                                menu.toggleable_entry(
                                    label,
                                    checked,
                                    IconPosition::Start,
                                    None,
                                    move |_, cx| {
                                        this.update(cx, |this, cx| pick(this, value.clone(), cx))
                                            .ok();
                                    },
                                )
                            }
                        };
                    }
                    menu
                }))
            })
            .into_any_element()
    }

    /// The draft's property: the other type's properties that take a test.
    fn render_property_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let items = filterable(&self.other)
            .into_iter()
            .map(|property| {
                Choice::item(
                    &property.name,
                    property.name.clone(),
                    property.name == self.property,
                )
            })
            .collect();
        let trigger =
            studio_ui::ds::button("neighbour-filter-property-button", self.property.clone())
                .start_icon(Icon::new(
                    self.property().map_or(IconName::Filter, property_icon),
                ));
        self.choice_menu(
            "neighbour-filter-property",
            0,
            trigger,
            items,
            Self::set_property,
            cx,
        )
    }

    /// The draft's test: the tests its property takes.
    fn render_op_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let items = self
            .property()
            .map(FilterOp::for_property)
            .unwrap_or_default()
            .into_iter()
            .map(|op| Choice::item(op.label(), op, op == self.op))
            .collect();
        let trigger = studio_ui::ds::button("neighbour-filter-op-button", self.op.label());
        self.choice_menu("neighbour-filter-op", 1, trigger, items, Self::set_op, cx)
    }

    /// The grouping: none, or one of the other type's, the edge's or the other type's
    /// relationships, under their headings.
    fn render_grouping_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let current = self.exploration.read(cx).grouping(&self.group).cloned();
        let mut items = vec![Choice::item("No grouping", None, current.is_none())];
        for (heading, section) in GroupBy::sections(&self.schema, &self.group) {
            items.push(Choice::Section(heading.into()));
            items.extend(section.into_iter().map(|by| {
                let checked = current.as_ref() == Some(&by);
                Choice::item(by.label(), Some(by), checked)
            }));
        }
        let label = current.as_ref().map_or("None".to_owned(), GroupBy::label);
        let trigger = studio_ui::ds::button("neighbour-group-button", label)
            .start_icon(Icon::new(IconName::ListTree));
        self.choice_menu("neighbour-group", 2, trigger, items, Self::set_grouping, cx)
    }

    /// The order: none, by id, or one of the other type's properties that can order rows.
    fn render_sort_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let current = self.exploration.read(cx).sort(&self.group).cloned();
        let named = current.as_ref().map(|sort| sort.property.clone());
        let mut items = vec![Choice::item("No order", None, named.is_none())];
        items.extend(sortable(&self.other).into_iter().map(|property| {
            let checked = named.as_deref() == Some(property.name.as_str());
            Choice::item(&property.name, Some(property.name.clone()), checked)
        }));
        let trigger = studio_ui::ds::button(
            "neighbour-sort-button",
            named.unwrap_or_else(|| "None".to_owned()),
        )
        .start_icon(Icon::new(IconName::ChevronUpDown));
        self.choice_menu("neighbour-sort", 3, trigger, items, Self::set_sort, cx)
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

/// A row of the card: its name in a short column, then what sets it.
fn card_row(label: &'static str, control: impl IntoElement) -> Div {
    h_flex()
        .gap(px(space::MD))
        .items_start()
        .child(
            div()
                .w(px(size::SHORT_LABEL))
                .flex_none()
                .h(px(size::CONTROL))
                .flex()
                .items_center()
                .child(studio_ui::ds::text::meta(label)),
        )
        .child(h_flex().flex_1().min_w_0().child(control))
}

impl Render for NeighbourCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pills = self.render_pills(cx);
        let takes_value = self.op.takes_value();
        let can_filter = !self.property.is_empty();
        let sort = self.exploration.read(cx).sort(&self.group).cloned();
        // The row names the relationship; the card names what it shows.
        let title = match self.group.direction {
            gq_language::Direction::Outgoing => {
                format!("{} {}", self.group.edge, self.group.other_type)
            }
            gq_language::Direction::Incoming => {
                format!("{} from {}", self.group.edge, self.group.other_type)
            }
        };
        let field = studio_ui::ds::field_frame(self.error.is_some(), cx)
            .flex_1()
            .child(studio_ui::ds::field_input(self.value.clone()));
        let filter = v_flex()
            .flex_1()
            .min_w_0()
            .gap(px(space::SM))
            .when(!pills.is_empty(), |rows| {
                rows.child(h_flex().flex_wrap().gap(px(space::SM)).children(pills))
            })
            .child(
                h_flex()
                    .gap(px(space::SM))
                    .items_center()
                    .flex_wrap()
                    .child(self.render_property_menu(cx))
                    .child(self.render_op_menu(cx))
                    .when(takes_value, |row| row.child(field))
                    .child(
                        studio_ui::ds::button("neighbour-filter-add", "Add")
                            .tip(studio_ui::ds::tooltip("Add this filter (↵)"))
                            .on_click(cx.listener(|this, _, window, cx| this.add(window, cx))),
                    ),
            )
            .when_some(self.error.clone(), |rows, error| {
                rows.child(studio_ui::ds::text::error(error))
            });
        let order = h_flex()
            .gap(px(space::SM))
            .child(self.render_sort_menu(cx))
            .when_some(sort, |row, sort| {
                row.child(
                    studio_ui::ds::row_icon_button(
                        "neighbour-sort-direction",
                        if sort.descending {
                            IconName::ArrowDown
                        } else {
                            IconName::ArrowUp
                        },
                    )
                    .tip(studio_ui::ds::tooltip(if sort.descending {
                        "Descending: turn to ascending"
                    } else {
                        "Ascending: turn to descending"
                    }))
                    .on_click(cx.listener(|this, _, _, cx| this.flip_sort(cx))),
                )
            });
        let (running, failed, note) = match &self.words {
            Words::Running { .. } => (true, None, None),
            Words::Failed(why) => (false, Some(why.clone()), None),
            Words::Typing(note) => (false, None, note.clone()),
        };
        let about = format!(
            "Show in words (↵): {} go to {}",
            words::TASK.sends,
            Models::model_for(words::TASK.tier, cx)
        );
        // The sentence, in the card's grey footer as a card's actions are (`ds::card_footer`).
        let footer = v_flex()
            .px(px(space::LG))
            .py(px(space::SM))
            .gap(px(space::XS))
            .rounded_b(px(studio_ui::ds::radius::flush(
                studio_ui::ds::radii(cx).menu(),
            )))
            .border_t_1()
            .border_color(studio_ui::ds::surface::floating_border(cx))
            .bg(studio_ui::ds::surface::footer(cx))
            .child(
                studio_ui::ds::pill_field("neighbour-words", failed.is_some(), cx)
                    .w_full()
                    .tip(studio_ui::ds::tooltip(about))
                    .child(studio_ui::ds::icon::meta(IconName::ZedAssistant))
                    .child(studio_ui::ds::field_input(self.sentence.clone()))
                    .when(running, |field| field.child(studio_ui::ds::spinner())),
            )
            .children(failed.map(studio_ui::ds::text::error))
            .children(note.map(studio_ui::ds::text::meta));
        menu_surface(v_flex(), cx)
            .id("neighbour-card")
            .role(Role::Menu)
            .key_context("menu")
            .track_focus(&self.focus_handle)
            .occlude()
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                // A menu of the card open takes the click; the card stays for it.
                if !this.menus.iter().any(|menu| menu.is_deployed()) {
                    cx.emit(DismissEvent);
                }
            }))
            .w(px(size::POPOVER_MAX))
            .child(
                div()
                    .px(px(space::LG))
                    .pt(px(space::MD))
                    .pb(px(space::SM))
                    .child(studio_ui::ds::text::plain(title)),
            )
            .child(
                v_flex()
                    .px(px(space::LG))
                    .pb(px(space::MD))
                    .gap(px(space::SM))
                    .child(card_row("Group", self.render_grouping_menu(cx)))
                    .child(card_row("Sort", order))
                    .when(can_filter, |body| body.child(card_row("Filter", filter))),
            )
            .child(footer)
    }
}

/// What opens the card: the group row's button, lit while the relationship is grouped,
/// ordered or filtered.
pub(crate) fn card_button(
    id: impl Into<ElementId>,
    set: bool,
    exploration: Entity<Exploration>,
    group: RelationshipGroup,
    path: ExplorePath,
) -> PopoverMenu<NeighbourCard> {
    let tooltip = if set {
        "Group, sort and filter these neighbours: some are set"
    } else {
        "Group, sort and filter these neighbours"
    };
    PopoverMenu::new(SharedString::from(format!("{}-card", group.edge)))
        .trigger(
            studio_ui::ds::row_icon_button(id, IconName::Filter)
                .icon_color(Color::Muted)
                .toggle_state(set)
                .tip(studio_ui::ds::tooltip(tooltip)),
        )
        .anchor(gpui::Anchor::TopRight)
        .menu(move |window, cx| {
            if !can_open(exploration.read(cx), &group) {
                return None;
            }
            let (exploration, group, path) = (exploration.clone(), group.clone(), path.clone());
            Some(cx.new(|cx| NeighbourCard::new(exploration, group, Some(path), window, cx)))
        })
}

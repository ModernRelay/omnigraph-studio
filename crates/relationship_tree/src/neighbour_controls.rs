//! A relationship's controls (spec 005, "A relationship's controls"): the group row's Sort,
//! Group and Filter buttons, drawn with the type table's own pieces. Sort and Group open the
//! table's menus; Filter opens the table's filter pills, the pill a filter is written in, and
//! the field that sets all three from a sentence (spec 019). What they set belongs to the
//! relationship, as a grouping does, so it holds wherever the relationship appears in the walk:
//! its counts, its pages and its grouped values.

use std::rc::Rc;
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
use studio_workspace::property_icon;
use ui::prelude::*;
use ui::{IconName, IconPosition, PopoverMenu, PopoverMenuHandle};

/// Where the sentence is: being typed, out with the model, or answered with a note of what
/// could not be used, or failed with why.
pub(crate) enum Words {
    Typing(Option<String>),
    Running { _call: Task<()> },
    Failed(String),
}

/// One line of a menu: a heading over the choices after it, or a choice.
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

/// What a menu's choice calls with its value.
type Pick<T> = Rc<dyn Fn(T, &mut App)>;

/// `items` as a menu's entries, the one checked ticked; picking one calls `pick` with its value.
fn choice_entries<T: Clone + 'static>(
    mut menu: ContextMenu,
    items: Vec<Choice<T>>,
    pick: Pick<T>,
) -> ContextMenu {
    for item in items {
        menu = match item {
            Choice::Section(heading) => menu.separator().header(heading),
            Choice::Item {
                label,
                value,
                checked,
            } => {
                let pick = pick.clone();
                menu.toggleable_entry(label, checked, IconPosition::Start, None, move |_, cx| {
                    pick(value.clone(), cx)
                })
            }
        };
    }
    menu
}

/// Whether `group` has controls: its other type is in the schema read.
pub(crate) fn can_open(exploration: &Exploration, group: &RelationshipGroup) -> bool {
    exploration
        .schema()
        .is_some_and(|schema| schema.node(&group.other_type).is_some())
}

/// The other type of `group`, as the schema read has it.
fn other_type(exploration: &Exploration, group: &RelationshipGroup) -> Option<Arc<NodeType>> {
    let schema = exploration.schema()?;
    schema.node(&group.other_type).cloned().map(Arc::new)
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

/// Every grouping `group` offers, its sections run together.
fn groupings(schema: &SchemaModel, group: &RelationshipGroup) -> Vec<GroupBy> {
    GroupBy::sections(schema, group)
        .into_iter()
        .flat_map(|(_, section)| section)
        .collect()
}

/// Groups `group`'s neighbours by `by`, or stops; a grouping chosen opens `path`, the row it
/// was asked from, on its values.
pub(crate) fn set_grouping(
    exploration: &Entity<Exploration>,
    group: &RelationshipGroup,
    path: Option<&ExplorePath>,
    by: Option<GroupBy>,
    cx: &mut App,
) {
    exploration.update(cx, |exploration, cx| {
        let open = by.is_some();
        exploration.set_grouping(group, by, cx);
        if open && let Some(path) = path {
            exploration.expand(path, cx);
        }
    });
}

/// Orders `group`'s neighbours by `sort`, or stops.
pub(crate) fn set_sort(
    exploration: &Entity<Exploration>,
    group: &RelationshipGroup,
    sort: Option<Sort>,
    cx: &mut App,
) {
    exploration.update(cx, |exploration, cx| exploration.set_sort(group, sort, cx));
}

/// The group row's buttons, each a menu of its own: Sort, Group and Filter, each lit while it
/// is set. A button the relationship's other type cannot use is left out.
pub(crate) struct Controls {
    pub sort: Option<AnyElement>,
    pub group: Option<AnyElement>,
    pub filter: Option<AnyElement>,
}

/// The controls of `group`'s row at `path`; `id` keeps a sticky row's apart from its own.
pub(crate) fn controls(
    id: &'static str,
    row: usize,
    exploration: &Entity<Exploration>,
    group: &RelationshipGroup,
    path: &ExplorePath,
    cx: &App,
) -> Option<Controls> {
    let read = exploration.read(cx);
    let schema = read.schema()?;
    let other = other_type(read, group)?;
    // A set control's icon is the accent, as a set chip's value is in the table; its tip
    // says what it is set to, and hides while its menu is open.
    let button = |name: &'static str, icon: IconName, set: bool| {
        studio_ui::ds::row_icon_button((SharedString::from(format!("{id}-{name}")), row), icon)
            .icon_color(if set { Color::Accent } else { Color::Muted })
    };
    let tip = |tip: String| {
        move |_: &mut Window, cx: &mut App| studio_ui::ds::tooltip_view(tip.clone(), cx)
    };
    let menu_id = |name: &str| SharedString::from(format!("{id}-{name}-{}-{row}", group.edge));

    let sort = (!sortable(&other).is_empty()).then(|| {
        let current = read.sort(group).cloned();
        let label = match &current {
            Some(sort) => format!(
                "Ordered by {} {}",
                sort.property,
                if sort.descending { "↓" } else { "↑" }
            ),
            None => "Order these neighbours by a property".to_owned(),
        };
        let (exploration, group, other) = (exploration.clone(), group.clone(), other.clone());
        PopoverMenu::new(menu_id("sort"))
            .anchor(gpui::Anchor::TopRight)
            .trigger_with_tooltip(
                button("sort", IconName::ChevronUpDown, current.is_some()),
                tip(label),
            )
            .menu(move |window, cx| {
                let choices: Vec<(String, String)> = sortable(&other)
                    .into_iter()
                    .map(|property| (property.name.clone(), property.name.clone()))
                    .collect();
                let current = exploration
                    .read(cx)
                    .sort(&group)
                    .map(|sort| (sort.property.clone(), sort.descending));
                let (exploration, group) = (exploration.clone(), group.clone());
                let set: studio_ui::ds::SetSort = Rc::new(move |sort, cx| {
                    let sort = sort.map(|(property, descending)| Sort {
                        property,
                        descending,
                    });
                    set_sort(&exploration, &group, sort, cx);
                });
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    studio_ui::ds::sort_menu(menu, &choices, current.clone(), set.clone())
                }))
            })
            .into_any_element()
    });

    let group_menu = (!groupings(&schema, group).is_empty()).then(|| {
        let current = read.grouping(group).cloned();
        let label = match &current {
            Some(by) => format!("Grouped by {}", by.label()),
            None => "Group these neighbours".to_owned(),
        };
        let (exploration, group, path) = (exploration.clone(), group.clone(), path.clone());
        PopoverMenu::new(menu_id("group"))
            .anchor(gpui::Anchor::TopRight)
            .trigger_with_tooltip(
                button("group", IconName::ListTree, current.is_some()),
                tip(label),
            )
            .menu(move |window, cx| {
                let current = exploration.read(cx).grouping(&group).cloned();
                let mut items = vec![Choice::item("No grouping", None, current.is_none())];
                for (heading, section) in GroupBy::sections(&schema, &group) {
                    items.push(Choice::Section(heading.into()));
                    items.extend(section.into_iter().map(|by| {
                        let checked = current.as_ref() == Some(&by);
                        Choice::item(by.label(), Some(by), checked)
                    }));
                }
                let (exploration, group, path) = (exploration.clone(), group.clone(), path.clone());
                let pick: Pick<Option<GroupBy>> =
                    Rc::new(move |by, cx| set_grouping(&exploration, &group, Some(&path), by, cx));
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    choice_entries(menu, items.clone(), pick.clone())
                }))
            })
            .into_any_element()
    });

    let filter = (!filterable(&other).is_empty()).then(|| {
        let count = read.filters(group).len();
        let label = match count {
            0 => "Filter these neighbours".to_owned(),
            1 => "1 filter".to_owned(),
            n => format!("{n} filters"),
        };
        let (exploration, group, path) = (exploration.clone(), group.clone(), path.clone());
        PopoverMenu::new(menu_id("filter"))
            .anchor(gpui::Anchor::TopRight)
            .trigger_with_tooltip(button("filter", IconName::Filter, count > 0), tip(label))
            .menu(move |window, cx| {
                if !can_open(exploration.read(cx), &group) {
                    return None;
                }
                let (exploration, group, path) = (exploration.clone(), group.clone(), path.clone());
                Some(cx.new(|cx| NeighbourFilters::new(exploration, group, Some(path), window, cx)))
            })
            .into_any_element()
    });

    Some(Controls {
        sort,
        group: group_menu,
        filter,
    })
}

/// The Filter button's popover: the relationship's filters as the table's pills, the pill the
/// next is written in, and the field that sets filters, order and grouping from a sentence.
pub(crate) struct NeighbourFilters {
    exploration: Entity<Exploration>,
    group: RelationshipGroup,
    /// The group's path where it was opened: a grouping a sentence sets opens it on its values.
    path: Option<ExplorePath>,
    schema: Arc<SchemaModel>,
    /// The other type, whose properties the filters test.
    other: Arc<NodeType>,
    /// The draft: a property and a test; its value is the editor's text.
    property: String,
    op: FilterOp,
    pub(crate) value: Entity<Editor>,
    pub(crate) error: Option<String>,
    /// The sentence for the model.
    pub(crate) sentence: Entity<Editor>,
    pub(crate) words: Words,
    /// The draft's property and test menus: a click on one open leaves the popover open.
    menus: [PopoverMenuHandle<ContextMenu>; 2],
    focus_handle: FocusHandle,
}

impl EventEmitter<DismissEvent> for NeighbourFilters {}

impl Focusable for NeighbourFilters {
    /// The value field: opening the popover puts the keyboard there, as the table's draft
    /// does; ↵ adds the filter (or, in the sentence, asks) and esc closes, through the
    /// surface's `menu` context.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.value.focus_handle(cx)
    }
}

impl NeighbourFilters {
    /// The popover for `group`'s neighbours in `exploration`, which [`can_open`] says has one;
    /// `path` is the group's row it was opened from.
    pub(crate) fn new(
        exploration: Entity<Exploration>,
        group: RelationshipGroup,
        path: Option<ExplorePath>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let read = exploration.read(cx);
        let schema = read.schema().expect("a schema, as can_open checked");
        let other = other_type(read, &group).expect("the other type, as can_open checked");
        // The draft starts on the property that names the neighbours, as a filter mostly
        // does; else the first that takes a test.
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
        window.focus(&value.focus_handle(cx), cx);
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

    /// Empties the draft's value, as the table's discard does.
    fn discard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
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
            groups: groupings(&self.schema, &self.group)
                .iter()
                .map(GroupBy::label)
                .collect(),
            columns: Vec::new(),
        }
    }

    /// ↵ in the sentence field: the sentence goes to the model, which answers with the
    /// relationship's filters, order and grouping, each checked against what is offered; what
    /// it could not use is said under the field.
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
            set_sort(&self.exploration, &group, Some(sort), cx);
        }
        if let Some(label) = view.group_by {
            let by = groupings(&self.schema, &group)
                .into_iter()
                .find(|by| by.label() == label);
            if by.is_some() {
                set_grouping(&self.exploration, &group, self.path.as_ref(), by, cx);
            }
        }
        cx.notify();
        (!refused.is_empty()).then(|| format!("Could not use: {}", refused.join("; ")))
    }

    /// A menu of the draft: `trigger` opening `items`; picking one calls `pick` on the popover.
    fn draft_menu<T: Clone + 'static>(
        &self,
        id: &'static str,
        handle: usize,
        trigger: studio_ui::ds::Button,
        items: Vec<Choice<T>>,
        pick: fn(&mut Self, T, &mut Context<Self>),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let pick: Pick<T> = Rc::new(move |value, cx| {
            this.update(cx, |this, cx| pick(this, value, cx)).ok();
        });
        PopoverMenu::new(id)
            .with_handle(self.menus[handle].clone())
            .trigger(trigger)
            .menu(move |window, cx| {
                let (items, pick) = (items.clone(), pick.clone());
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    choice_entries(menu, items, pick)
                }))
            })
            .into_any_element()
    }

    /// The pill the next filter is written in, as the table's: property ▾ | test ▾ | value | ✓ ×.
    fn render_draft(&self, cx: &mut Context<Self>) -> AnyElement {
        let properties = filterable(&self.other)
            .into_iter()
            .map(|property| {
                Choice::item(
                    &property.name,
                    property.name.clone(),
                    property.name == self.property,
                )
            })
            .collect();
        let ops = self
            .property()
            .map(FilterOp::for_property)
            .unwrap_or_default()
            .into_iter()
            .map(|op| Choice::item(op.label(), op, op == self.op))
            .collect();
        let icon = self.property().map_or(IconName::Filter, property_icon);
        studio_ui::ds::filter_draft("neighbour-filter-draft", self.error.is_some(), cx)
            .child(self.draft_menu(
                "neighbour-filter-property",
                0,
                studio_ui::ds::filter_draft_choice(
                    "neighbour-filter-property-button",
                    self.property.clone(),
                    Some(icon),
                ),
                properties,
                Self::set_property,
                cx,
            ))
            .child(studio_ui::ds::pill_rule(cx))
            .child(self.draft_menu(
                "neighbour-filter-op",
                1,
                studio_ui::ds::filter_draft_choice(
                    "neighbour-filter-op-button",
                    self.op.label(),
                    None,
                ),
                ops,
                Self::set_op,
                cx,
            ))
            .when(self.op.takes_value(), |pill| {
                pill.child(studio_ui::ds::pill_rule(cx))
                    .child(studio_ui::ds::filter_draft_value().child(self.value.clone()))
            })
            .child(studio_ui::ds::pill_rule(cx))
            .child(studio_ui::ds::filter_draft_end(
                cx.listener(|this, _, window, cx| this.add(window, cx)),
                cx.listener(|this, _, window, cx| this.discard(window, cx)),
                cx,
            ))
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
        let draft = self.render_draft(cx);
        let (running, failed, note) = match &self.words {
            Words::Running { .. } => (true, None, None),
            Words::Failed(why) => (false, Some(why.clone()), None),
            Words::Typing(note) => (false, None, note.clone()),
        };
        let about = format!(
            "Filter in words (↵): {} go to {}",
            words::TASK.sends,
            Models::model_for(words::TASK.tier, cx)
        );
        // The sentence, in the grey footer as a card's actions are (`ds::card_footer`).
        let footer = v_flex()
            .px(px(space::MD))
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
            .max_w(px(size::POPOVER_MAX))
            .child(
                // The filters, then the one being written, wrapping as the table's bar does.
                h_flex()
                    .p(px(space::MD))
                    .flex_wrap()
                    .gap(px(space::SM))
                    .items_center()
                    .children(pills)
                    .child(draft),
            )
            .children(self.error.clone().map(|error| {
                div()
                    .px(px(space::MD))
                    .pb(px(space::SM))
                    .child(studio_ui::ds::text::error(error))
            }))
            .child(footer)
    }
}

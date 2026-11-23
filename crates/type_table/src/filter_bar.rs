//! The filter bar of a view of one type's rows (spec 004; a relationship's group, spec 025): the
//! filter being written and where it goes when applied, its key and test menus, the values its
//! field offers, a group pill's menu, and the bar itself.
//!
//! [`FilterBar`] is the bar's own state. [`FilterHost`] is what the bar asks of the view that
//! holds it: the view keeps its filters and reads its rows again when they change, and all the
//! bar does is the trait's provided methods, so every host has the same bar.

use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use editor::{Editor, EditorEvent};
use gpui::{
    Anchor, AnyElement, App, ClickEvent, Context, Entity, FocusHandle, Focusable, MouseButton,
    MouseDownEvent, Pixels, Subscription, Task, WeakEntity, Window, anchored, deferred, point,
};
use gq_language::builder::{Group, Quantifier};
use gq_language::{Filter, FilterOp, PageStart, Property, ScalarKind, SchemaModel, builder};
use graph_store::GraphSession;
use serde_json::Value;
use studio_actions::table::{ClearFilters, SelectNextSuggestion, SelectPreviousSuggestion};
use studio_ui::ds::Tipped;
use studio_ui::ds::space;
use studio_ui::menu::{ContextMenu, ContextMenuEntry};
use studio_workspace::property_icon;
use ui::prelude::*;
use ui::{PopoverMenu, PopoverMenuHandle};

use crate::model::{
    ColumnSpec, TableKind, edit_text, filter_parts, parse_filter_list, parse_filter_value,
    preset_range,
};
use crate::offers::Offers;
use crate::suggest;

/// The bar's own state, which its host holds ([`FilterHost::bar`]).
pub(crate) struct FilterBar {
    /// The filter being written.
    pub(crate) draft: Option<FilterDraft>,
    /// The draft's value field.
    pub(crate) editor: Entity<Editor>,
    /// The filter being written's property and operator menus; the offered values hide while
    /// either is open.
    pub(crate) menus: [PopoverMenuHandle<ContextMenu>; 2],
    /// A group pill's menu (spec 025), where it was opened.
    pub(crate) group_menu: Option<(Entity<ContextMenu>, gpui::Point<Pixels>, Subscription)>,
    /// Labels of nodes picked for "related to" filters, so their pills read by name.
    pub(crate) node_labels: HashMap<String, String>,
    /// Every filter key, how it reads and its icon ([`FilterHost::filter_choices`]).
    choices: Rc<Vec<(String, String, IconName)>>,
    /// The Filter menu's entries over [`Self::choices`].
    menu: Rc<Vec<FilterMenuEntry>>,
    _editor_events: Subscription,
}

impl FilterBar {
    /// The bar of the view being made: its value field, and what typing in the field does.
    pub(crate) fn new<H: FilterHost>(window: &mut Window, cx: &mut Context<H>) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Value", window, cx);
            editor.set_text_style_refinement(studio_ui::ds::field_text(cx));
            editor
        });
        let editor_events =
            cx.subscribe(
                &editor,
                |this: &mut H, _, event: &EditorEvent, cx| match event {
                    // No `notify`: the table draws again because the field it holds changed,
                    // and the grid, which does not show the field, keeps its last frame.
                    EditorEvent::BufferEdited => {
                        if let Some(draft) = &mut this.bar_mut().draft {
                            draft.error = None;
                        }
                        this.refresh_suggestions(cx);
                    }
                    // The offered values show only while the field has focus.
                    EditorEvent::Focused | EditorEvent::Blurred => this.redraw_offers(cx),
                    _ => {}
                },
            );
        Self {
            draft: None,
            editor,
            menus: Default::default(),
            group_menu: None,
            node_labels: HashMap::new(),
            choices: Default::default(),
            menu: Default::default(),
            _editor_events: editor_events,
        }
    }

    /// Forgets what the Filter menu offers: the schema no longer has the type.
    pub(crate) fn clear_choices(&mut self) {
        self.choices = Default::default();
        self.menu = Default::default();
    }
}

/// Where a draft's filter goes when applied (spec 025): the bar, a test of a group in the bar
/// (a new one, or the one at `test` it replaces), or a group's aggregate.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DraftTarget {
    Bar,
    Test {
        /// The group, as applied: found in the bar by equality, as `replacing` is.
        group: Filter,
        test: Option<usize>,
    },
    Aggregate {
        group: Filter,
        quantifier: Quantifier,
    },
}

impl DraftTarget {
    fn group(&self) -> Option<&Filter> {
        match self {
            DraftTarget::Bar => None,
            DraftTarget::Test { group, .. } | DraftTarget::Aggregate { group, .. } => Some(group),
        }
    }
}

pub(crate) struct FilterDraft {
    pub(crate) property: String,
    pub(crate) op: FilterOp,
    target: DraftTarget,
    pub(crate) error: Option<String>,
    /// The applied filter this draft edits: still applied, and so still filtering the rows and
    /// every page read, while its pill shows as this draft. Replaced where it stands when the
    /// draft is applied; left as it was when the draft goes.
    replacing: Option<Filter>,
    /// What the value field offers for its text, the one ↵ picks, and the node search under way.
    pub(crate) offers: Offers,
    /// The text `offers` answers; while it differs from the field, a node search is out.
    pub(crate) answers: Option<String>,
    suggest_generation: u64,
    _suggest_task: Option<Task<()>>,
}

/// Where a filter's value field finds what to offer.
pub(crate) enum SuggestSource {
    Values(Vec<String>),
    /// Nodes of this type, searched on the server.
    Nodes(String),
}

/// An entry of the Filter menu, by its place in the table's filter choices: a key, or a
/// relationship whose neighbours' properties open in a submenu of their own. A type reached
/// by hundreds of relationships lists only those, not every neighbour property after each.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FilterMenuEntry {
    Key(usize),
    Relationship {
        key: usize,
        /// The type at its other end, which the submenu's properties belong to.
        other_type: String,
        properties: Range<usize>,
    },
}

/// A Filter menu entry that sets the filter being written to `key`: the draft's own key menu,
/// so a filter opened from its pill is still the one it replaces ([`FilterHost::change_draft_key`]).
fn filter_key_entry<H: FilterHost>(
    table: &WeakEntity<H>,
    key: String,
    label: String,
    icon: IconName,
) -> ContextMenuEntry {
    let table = table.clone();
    ContextMenuEntry::new(label)
        .icon(icon)
        .icon_color(Color::Muted)
        .handler(move |window, cx| {
            table
                .update(cx, |this, cx| {
                    this.change_draft_key(key.clone(), window, cx)
                })
                .ok();
        })
}

/// A change a group pill's menu makes to the table.
type GroupEdit<H> = dyn Fn(&mut H, &mut Window, &mut Context<H>);

/// What a pick in a path menu does with the key it picked.
type PickKey = Rc<dyn Fn(String, &mut Window, &mut App)>;

/// A path menu's pick that sets the draft's key, as the Filter menu's own entries do.
fn pick_for<H: FilterHost>(table: &WeakEntity<H>) -> PickKey {
    let table = table.clone();
    Rc::new(move |key, window, cx| {
        table
            .update(cx, |this, cx| this.change_draft_key(key, window, cx))
            .ok();
    })
}

/// Where a draft into a group roots its key menu: the group's record.
#[derive(Clone)]
pub(crate) struct DraftRoot {
    key: String,
    type_name: String,
    edge: String,
    depth: usize,
    icon: IconName,
    offer: PathOffer,
}

/// What a path menu offers: every test, or (for a group's total, average, smallest or largest)
/// the properties that aggregate takes, with no further hop.
#[derive(Clone, Copy)]
pub(crate) enum PathOffer {
    Tests,
    Aggregate(Quantifier),
}

/// A relationship's submenu beyond its own tests (spec 025): its edge's properties, the related
/// type's properties, and its relationships, each opening the same menu one hop further, to
/// [`builder::MAX_HOPS`]. `prefix` is the path key to here (`out:WorksAt`), `type_name` the
/// record's type there and `edge` the edge that reached it.
#[allow(clippy::too_many_arguments)]
fn path_menu(
    mut menu: ContextMenu,
    pick: &PickKey,
    schema: &Arc<SchemaModel>,
    prefix: &str,
    type_name: &str,
    edge: &str,
    depth: usize,
    icon: IconName,
    offer: PathOffer,
) -> ContextMenu {
    let entry = |key: String, label: String| {
        let pick = pick.clone();
        ContextMenuEntry::new(label)
            .icon(icon)
            .icon_color(Color::Muted)
            .handler(move |window, cx| pick(key.clone(), window, cx))
    };
    let offered = |property: &gq_language::Property| match offer {
        PathOffer::Tests => !FilterOp::for_property(property).is_empty(),
        PathOffer::Aggregate(quantifier) => quantifier.takes(property),
    };
    if let Some(edge_type) = schema.edge(edge) {
        let properties: Vec<_> = edge_type.properties.iter().filter(|p| offered(p)).collect();
        if !properties.is_empty() {
            menu = menu.header(format!("{} itself", gq_language::traversal_name(edge)));
            for property in properties {
                menu = menu.item(entry(
                    format!("{prefix}.{}", builder::edge_property_key(&property.name)),
                    property.name.clone(),
                ));
            }
        }
    }
    if let Some(node) = schema.node(type_name) {
        let properties: Vec<_> = node
            .properties
            .iter()
            .filter(|p| !p.is_key && offered(p))
            .collect();
        if !properties.is_empty() {
            menu = menu.header(format!("{type_name}'s property"));
            for property in properties {
                menu = menu.item(entry(
                    format!("{prefix}.{}", property.name),
                    property.name.clone(),
                ));
            }
        }
    }
    if matches!(offer, PathOffer::Tests) && depth < builder::MAX_HOPS {
        let groups = schema.relationships_of(type_name);
        if !groups.is_empty() {
            menu = menu.header(format!("{type_name}'s relationships"));
        }
        for group in groups {
            let key = format!("{prefix}.{}", builder::relationship_key(group));
            let label = format!(
                "{} {}",
                gq_language::traversal_name(&group.edge),
                group.other_type
            );
            let (pick, schema) = (pick.clone(), schema.clone());
            let (other, edge) = (group.other_type.clone(), group.edge.clone());
            let icon = direction_icon(group.direction);
            menu = menu.submenu_with_colored_icon(label, icon, Color::Muted, move |menu, _, _| {
                path_menu(
                    menu,
                    &pick,
                    &schema,
                    &key,
                    &other,
                    &edge,
                    depth + 1,
                    icon,
                    PathOffer::Tests,
                )
            });
        }
    }
    menu
}

/// `group` with a draft applied into it (spec 025): a test added or put in place of the one it
/// edits, or the aggregate set, from the draft's path key `key` (which starts at the group's
/// relationship).
fn into_group(
    group: &Filter,
    target: &DraftTarget,
    key: &str,
    op: FilterOp,
    value: Value,
) -> Result<Filter, String> {
    let (group_key, mut kept) =
        builder::as_group(group).ok_or_else(|| "That filter is no longer applied".to_owned())?;
    let Some(relative) = builder::path_within(key, &group_key) else {
        return Err(format!(
            "Pick a test on {}",
            path_label(&format!("{group_key}.x")).trim_end_matches(" · x")
        ));
    };
    match target {
        DraftTarget::Bar => unreachable!(),
        DraftTarget::Test { test, .. } => {
            let made = builder::group_test(relative, op, value);
            match test.and_then(|at| kept.tests.get_mut(at)) {
                Some(slot) => *slot = made,
                None => kept.tests.push(made),
            }
        }
        DraftTarget::Aggregate { quantifier, .. } => {
            kept.quantifier = *quantifier;
            kept.of = Some(relative.to_owned());
            kept.compare = Some(op);
            kept.than = Some(value);
            kept.count = None;
        }
    }
    Ok(builder::group_filter(&group_key, &kept))
}

/// How a path key reads where the Filter menu names it: `worksAt → locatedIn · name`, and an
/// edge's own property `worksAt · since`.
fn path_label(key: &str) -> String {
    let Some((relationships, end)) = builder::split_path(key) else {
        return key.to_owned();
    };
    let names: Vec<String> = relationships
        .iter()
        .filter_map(|relationship| builder::parse_relationship_key(relationship))
        .map(|(_, edge)| gq_language::traversal_name(edge))
        .collect();
    format!("{} · {}", names.join(" → "), end.trim_start_matches('@'))
}

/// The Filter menu over `choices` (in [`FilterHost::filter_choices`]' order: each relationship
/// followed by its neighbours' properties), each relationship naming its other end through
/// `other_type`.
pub(crate) fn filter_menu(
    choices: &[(String, String, IconName)],
    other_type: impl Fn(&str) -> Option<String>,
) -> Vec<FilterMenuEntry> {
    let mut entries = Vec::new();
    let mut at = 0;
    while at < choices.len() {
        let key = &choices[at].0;
        if builder::parse_relationship_key(key).is_none() {
            entries.push(FilterMenuEntry::Key(at));
            at += 1;
            continue;
        }
        let prefix = format!("{key}.");
        let end = choices[at + 1..]
            .iter()
            .position(|(nested, _, _)| !nested.starts_with(&prefix))
            .map_or(choices.len(), |offset| at + 1 + offset);
        entries.push(match other_type(key) {
            Some(other_type) => FilterMenuEntry::Relationship {
                key: at,
                other_type,
                properties: at + 1..end,
            },
            _ => FilterMenuEntry::Key(at),
        });
        at = end;
    }
    entries
}

/// What the filter bar asks of the view that holds it. The view keeps its own filters and reads
/// its rows again when they change ([`Self::filters_changed`]), and holds the bar's state
/// ([`FilterBar`]); what the bar does is the provided methods, the same for every host.
pub(crate) trait FilterHost: Sized + 'static {
    /// The bar's state.
    fn bar(&self) -> &FilterBar;
    fn bar_mut(&mut self) -> &mut FilterBar;

    /// The filters applied: the host keeps them, the bar shows them as pills and changes them.
    fn filters(&self) -> &[Filter];
    fn filters_mut(&mut self) -> &mut Vec<Filter>;

    /// The schema, once it is read.
    fn schema(&self) -> Option<&Arc<SchemaModel>>;

    /// The type whose rows are filtered, and whether it is a node's or an edge's.
    fn type_name(&self) -> &str;
    fn kind(&self) -> TableKind;

    /// Every column the schema gives the type, shown or not: a filter can test any of them.
    fn all_columns(&self) -> &[ColumnSpec];

    /// Where the nodes a "related to" value names are searched: this session, at the commit
    /// the host's reads are pinned to.
    fn session(&self) -> &Entity<GraphSession>;
    fn pinned(&self) -> Option<String>;

    /// Where the keyboard goes back to when the draft closes.
    fn host_focus(&self) -> FocusHandle;

    /// The filters changed: the host reads its rows again.
    fn filters_changed(&mut self, cx: &mut Context<Self>);

    /// The property a new draft starts on, when the host has one in hand (a table's cursor
    /// is on its column); the first that can be filtered otherwise.
    fn draft_starts_on(&self) -> Option<&Property> {
        None
    }

    /// A note the host puts in the bar, after the pills; the bar shows for it alone.
    fn bar_note(&self) -> Option<Label> {
        None
    }

    /// A draft was asked for and no column can be filtered: the host says so.
    fn nothing_to_filter(&self, _cx: &mut Context<Self>) {}

    /// The filters with each date preset's days computed now, in local time: a preset holds no
    /// value of its own, so a row is judged against these, as the engine is asked with them.
    fn resolved_filters(&self) -> Vec<Filter> {
        // Reading the local time zone is not free: only a preset needs it.
        if !self
            .filters()
            .iter()
            .any(|filter| filter.op.is_date_preset())
        {
            return self.filters().to_vec();
        }
        let now = chrono::Local::now();
        self.filters()
            .iter()
            .map(|filter| {
                if !filter.op.is_date_preset() {
                    return filter.clone();
                }
                // Of every column, not those shown: a filter can be on a hidden one.
                let kind = self
                    .all_columns()
                    .iter()
                    .filter_map(ColumnSpec::property)
                    .find(|p| p.name == filter.property)
                    .map(|p| p.kind);
                Filter {
                    value: preset_range(filter.op, kind, now),
                    ..filter.clone()
                }
            })
            .collect()
    }

    fn filterable(&self) -> impl Iterator<Item = &Property> {
        self.all_columns()
            .iter()
            .filter_map(ColumnSpec::property)
            .filter(|p| !FilterOp::for_property(p).is_empty())
    }

    /// The icon of a filter key: the property's kind, or the relationship's direction.
    fn key_icon(&self, key: &str) -> IconName {
        if key == builder::WHERE_FILTER {
            return IconName::Code;
        }
        if let Some((direction, _)) =
            builder::parse_relationship_key(key.split('.').next().unwrap_or(key))
        {
            return direction_icon(direction);
        }
        self.filterable()
            .find(|p| p.name == key)
            .map_or(IconName::CaseSensitive, property_icon)
    }

    /// The property a filter key tests: the table's own, or a neighbour's through a
    /// relationship key (`out:Attended.name`). None for a relationship itself.
    fn property_for_key(&self, key: &str) -> Option<Property> {
        if builder::is_deep_path(key) {
            return builder::path_property(self.schema()?, self.type_name(), key);
        }
        if let Some((relationship, property)) = key.split_once('.') {
            let (direction, edge) = builder::parse_relationship_key(relationship)?;
            let schema = self.schema()?;
            let group = schema
                .relationships_of(self.type_name())
                .iter()
                .find(|group| group.direction == direction && group.edge == edge)?;
            return schema
                .node(&group.other_type)?
                .properties
                .iter()
                .find(|p| p.name == property)
                .filter(|p| !FilterOp::for_property(p).is_empty())
                .cloned();
        }
        self.filterable().find(|p| p.name == key).cloned()
    }

    /// What a filter can test: each filterable property, then (in a node table) each
    /// relationship group, as `(key, label, icon)`.
    fn filter_choices(&self) -> Rc<Vec<(String, String, IconName)>> {
        self.bar().choices.clone()
    }

    /// The Filter menu's entries over [`Self::filter_choices`].
    fn filter_menu(&self) -> Rc<Vec<FilterMenuEntry>> {
        self.bar().menu.clone()
    }

    /// Builds what the Filter menu offers, once the columns are known.
    fn rebuild_filter_choices(&mut self) {
        let filters = self.build_filter_choices();
        let filter_menu = filter_menu(&filters, |key| self.related_type(key));
        let bar = self.bar_mut();
        bar.choices = Rc::new(filters);
        bar.menu = Rc::new(filter_menu);
    }

    /// Builds [`Self::filter_choices`] for these columns ([`Self::rebuild_filter_choices`]).
    fn build_filter_choices(&self) -> Vec<(String, String, IconName)> {
        let mut choices: Vec<(String, String, IconName)> = self
            .filterable()
            .map(|p| (p.name.clone(), p.name.clone(), property_icon(p)))
            .collect();
        if self.kind() == TableKind::Node
            && let Some(schema) = self.schema()
        {
            for group in schema.relationships_of(self.type_name()) {
                let key = builder::relationship_key(group);
                // The entry's icon shows the direction, so the label names the traversal only.
                let label = format!(
                    "{} {}",
                    gq_language::traversal_name(&group.edge),
                    group.other_type
                );
                choices.push((key.clone(), label.clone(), direction_icon(group.direction)));
                // Then the neighbour's own properties, a nested search each: "attended Person
                // · name".
                if let Some(other) = schema.node(&group.other_type) {
                    for property in &other.properties {
                        if property.is_key || FilterOp::for_property(property).is_empty() {
                            continue;
                        }
                        choices.push((
                            builder::neighbour_property_key(group, &property.name),
                            format!("{label} · {}", property.name),
                            direction_icon(group.direction),
                        ));
                    }
                }
            }
            // Anything the pills cannot say, as GQ on the row (spec 004, "Condition").
            choices.push((
                builder::WHERE_FILTER.to_owned(),
                "Condition (GQ)".to_owned(),
                IconName::Code,
            ));
        }
        choices
    }

    /// The type at the other end of a relationship key.
    fn related_type(&self, key: &str) -> Option<String> {
        let (direction, edge) = builder::parse_relationship_key(key)?;
        self.schema()?
            .relationships_of(self.type_name())
            .iter()
            .find(|group| group.direction == direction && group.edge == edge)
            .map(|group| group.other_type.clone())
    }

    /// Judges a condition as the compiler would before a read: the page query with the
    /// condition in it type-checks against the schema, or the first diagnostic says why not.
    fn check_condition(&self, text: &str) -> Result<(), String> {
        let Some(schema) = self.schema() else {
            return Err("The schema is not loaded yet.".to_owned());
        };
        let filter = Filter {
            property: builder::WHERE_FILTER.to_owned(),
            op: FilterOp::Expr,
            value: Value::String(text.to_owned()),
        };
        let query = builder::node_page(
            schema,
            self.type_name(),
            &[filter],
            None,
            &PageStart::First,
            1,
        )
        .map_err(|error| error.to_string())?;
        let checked = gq_language::check(schema, &query.text);
        match checked.diagnostics.first() {
            None => Ok(()),
            Some(diagnostic) => Err(match &diagnostic.fix {
                Some(fix) => format!("{} ({fix})", diagnostic.message),
                None => diagnostic.message.clone(),
            }),
        }
    }

    /// The tests offered for a filter key.
    fn ops_for(&self, key: &str) -> Vec<FilterOp> {
        if key == builder::WHERE_FILTER {
            return vec![FilterOp::Expr];
        }
        if builder::parse_relationship_key(key).is_some() {
            return FilterOp::for_relationship();
        }
        self.property_for_key(key)
            .map(|property| FilterOp::for_property(&property))
            .unwrap_or_default()
    }

    /// The tests a draft offers: its key's, and for a group's aggregate only the comparisons.
    fn draft_ops(&self, draft: &FilterDraft) -> Vec<FilterOp> {
        let ops = self.ops_for(&draft.property);
        match draft.target {
            DraftTarget::Aggregate { .. } => ops
                .into_iter()
                .filter(|op| {
                    matches!(
                        op,
                        FilterOp::Eq
                            | FilterOp::Ne
                            | FilterOp::Lt
                            | FilterOp::Le
                            | FilterOp::Gt
                            | FilterOp::Ge
                    )
                })
                .collect(),
            _ => ops,
        }
    }

    /// Where a draft into a group roots its key menu: the record the group's relationship
    /// reaches. `None` for a draft in the bar, whose menu is the whole Filter menu.
    fn draft_root(&self, draft: &FilterDraft) -> Option<DraftRoot> {
        let (key, _) = builder::as_group(draft.target.group()?)?;
        self.group_root(
            &key,
            match draft.target {
                DraftTarget::Aggregate { quantifier, .. } => PathOffer::Aggregate(quantifier),
                _ => PathOffer::Tests,
            },
        )
    }

    fn group_root(&self, key: &str, offer: PathOffer) -> Option<DraftRoot> {
        let schema = self.schema()?;
        let (direction, edge) = builder::parse_relationship_key(key)?;
        let (node, _) = builder::path_end(schema, self.type_name(), &[key])?;
        Some(DraftRoot {
            key: key.to_owned(),
            type_name: node.name.clone(),
            edge: edge.to_owned(),
            depth: 1,
            icon: direction_icon(direction),
            offer,
        })
    }

    fn open_filter_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let preferred = self
            .draft_starts_on()
            .filter(|p| self.filterable().any(|f| f.name == p.name))
            .or_else(|| self.filterable().next())
            .cloned();
        let Some(property) = preferred else {
            self.nothing_to_filter(cx);
            return;
        };
        self.set_draft_property(property.name, window, cx);
    }

    /// The draft tests another key, from its own key menu: opened from a pill, it still replaces
    /// that pill's filter where it stands. A draft started anew ([`Self::set_draft_property`])
    /// leaves a filter being edited as it was.
    fn change_draft_key(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let replacing = self
            .bar()
            .draft
            .as_ref()
            .and_then(|draft| draft.replacing.clone());
        let target = self
            .bar()
            .draft
            .as_ref()
            .map_or(DraftTarget::Bar, |draft| draft.target.clone());
        self.set_draft_property(name, window, cx);
        if let Some(draft) = &mut self.bar_mut().draft {
            if draft.replacing.is_none() {
                draft.replacing = replacing;
            }
            draft.target = target;
        }
        if let Some(draft) = &self.bar().draft {
            let op = self.draft_ops(draft).first().copied().unwrap_or(draft.op);
            if let Some(draft) = &mut self.bar_mut().draft {
                draft.op = op;
            }
        }
    }

    fn set_draft_property(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(&op) = self.ops_for(&name).first() else {
            return;
        };
        self.bar_mut().draft = Some(FilterDraft {
            property: name,
            op,
            target: DraftTarget::Bar,
            error: None,
            replacing: None,
            offers: Offers::default(),
            // An empty field answers itself; text put in it is searched for.
            answers: Some(String::new()),
            suggest_generation: 0,
            _suggest_task: None,
        });
        self.bar().editor.update(cx, |editor, cx| {
            editor.set_text("", window, cx);
            editor.set_placeholder_text(filter_placeholder(op), window, cx);
        });
        window.focus(&self.bar().editor.focus_handle(cx), cx);
        self.refresh_suggestions(cx);
        cx.notify();
    }

    /// Where the value field of a filter on `key` with `op` finds what to offer: an enum's
    /// values, `true` and `false`, or for "related to" the other type's nodes.
    fn suggestion_source(&self, key: &str, op: FilterOp) -> Option<SuggestSource> {
        if !op.takes_value() {
            return None;
        }
        if op == FilterOp::RelatedTo {
            let group = self
                .schema()?
                .relationships_of(self.type_name())
                .iter()
                .find(|group| builder::relationship_key(group) == key)?;
            return Some(SuggestSource::Nodes(group.other_type.clone()));
        }
        let property = self.property_for_key(key)?;
        if let Some(values) = &property.enum_values {
            return Some(SuggestSource::Values(values.clone()));
        }
        (property.kind == ScalarKind::Bool)
            .then(|| SuggestSource::Values(vec!["true".to_owned(), "false".to_owned()]))
    }

    /// Updates what the value field offers for its text. Nodes are searched on the server,
    /// at the table's commit, once typing pauses.
    fn refresh_suggestions(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = &self.bar().draft else {
            return;
        };
        let text = self.bar().editor.read(cx).text(cx);
        let list = draft.op == FilterOp::In;
        let source = suggest::completes(&text, list)
            .then(|| self.suggestion_source(&draft.property, draft.op))
            .flatten();
        let typed = !suggest::needle(&text, list).is_empty();
        let schema = self.schema().cloned();
        let snapshot = self.pinned();
        let session = self.session().clone();
        let Some(draft) = &mut self.bar_mut().draft else {
            return;
        };
        draft.suggest_generation += 1;
        let generation = draft.suggest_generation;
        match source {
            None => {
                draft.offers.clear();
                draft.answers = Some(text);
                draft._suggest_task = None;
            }
            Some(SuggestSource::Values(values)) => {
                draft
                    .offers
                    .set(suggest::from_values(&values, &text, list), typed);
                draft.answers = Some(text);
                draft._suggest_task = None;
            }
            Some(SuggestSource::Nodes(type_name)) => {
                if schema.is_none() {
                    draft.offers.clear();
                    draft.answers = Some(text);
                    return;
                }
                let needle = suggest::needle(&text, list);
                let search = suggest::search_nodes(&session, &type_name, &needle, snapshot, cx);
                draft._suggest_task = Some(cx.spawn(async move |this, cx| {
                    let result = search.await;
                    this.update(cx, |this, cx| {
                        let Some(draft) = &mut this.bar_mut().draft else {
                            return;
                        };
                        if draft.suggest_generation != generation {
                            return;
                        }
                        // A failed search offers nothing, not what an earlier one found.
                        match result {
                            Ok(hits) => draft.offers.set(suggest::from_hits(&hits), typed),
                            Err(_) => draft.offers.clear(),
                        }
                        draft.answers = Some(text);
                        this.redraw_offers(cx);
                    })
                    .ok();
                }));
            }
        }
        // No `notify`: it would draw the grid on every key. The callers that change the draft
        // notify; a keystroke draws the table again because its field changed.
    }

    /// ↑/↓ in the value field: highlights another offered value.
    fn move_suggestion(&mut self, step: isize, cx: &mut Context<Self>) {
        if let Some(draft) = &mut self.bar_mut().draft
            && draft.offers.step(step)
        {
            self.redraw_offers(cx);
        }
    }

    /// Draws the offers again: what the value field offers, or the one highlighted, changed.
    /// They are drawn under the field, beside the grid, so the field is what is notified: that
    /// draws the views around it again, as a key typed in it does, and leaves the grid as it
    /// was. A notify of the table would draw the grid again too (`TableGrid`).
    fn redraw_offers(&self, cx: &mut Context<Self>) {
        self.bar().editor.update(cx, |_, cx| cx.notify());
    }

    /// Puts an offered value in the field. A single value adds the filter at once; a list
    /// waits for its next value.
    fn pick_suggestion(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = &self.bar().draft else {
            return;
        };
        let Some(suggestion) = draft.offers.get(index).cloned() else {
            return;
        };
        let list = draft.op == FilterOp::In;
        let text = self.bar().editor.read(cx).text(cx);
        let text = suggest::complete(&text, &suggestion.value, list);
        if let Some(label) = &suggestion.label {
            self.bar_mut()
                .node_labels
                .insert(suggestion.value.clone(), label.clone());
        }
        self.bar().editor.update(cx, |editor, cx| {
            editor.set_text(text, window, cx);
            // Not `move_to_end`: in a single-line editor it only propagates, and the ↵ that
            // picked would then also be typed into the field.
            let end = editor.buffer().read(cx).read(cx).len();
            editor.change_selections(Default::default(), window, cx, |s| {
                s.select_ranges(vec![end..end])
            });
        });
        if list {
            self.refresh_suggestions(cx);
        } else {
            self.apply_filter_draft(window, cx);
        }
    }

    /// A click on a pill: the pill shows as the draft, its key, test and value as they were, to
    /// change and apply in its place; Esc leaves it as it was. The filter stays applied the
    /// while, so the rows and every page read follow it.
    fn edit_filter(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.filters().len() {
            return;
        }
        let filter = self.filters()[index].clone();
        self.set_draft_property(filter.property.clone(), window, cx);
        let Some(draft) = &mut self.bar_mut().draft else {
            return;
        };
        draft.op = filter.op;
        let text = if filter.op.takes_value() {
            edit_text(&filter.value)
        } else {
            String::new()
        };
        draft.replacing = Some(filter);
        let op = draft.op;
        self.bar().editor.update(cx, |editor, cx| {
            editor.set_text(text, window, cx);
            editor.set_placeholder_text(filter_placeholder(op), window, cx);
            let end = editor.buffer().read(cx).read(cx).len();
            editor.change_selections(Default::default(), window, cx, |s| {
                s.select_ranges(vec![end..end])
            });
        });
        self.refresh_suggestions(cx);
        cx.notify();
    }

    /// Closes the draft; a filter it was editing stays as it was.
    fn cancel_filter_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.bar_mut().draft.take().is_some() {
            window.focus(&self.host_focus(), cx);
            cx.notify();
        }
    }

    fn set_draft_op(&mut self, op: FilterOp, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(draft) = &mut self.bar_mut().draft {
            draft.op = op;
            draft.error = None;
        }
        self.bar().editor.update(cx, |editor, cx| {
            editor.set_placeholder_text(filter_placeholder(op), window, cx)
        });
        if op.takes_value() {
            window.focus(&self.bar().editor.focus_handle(cx), cx);
        }
        self.refresh_suggestions(cx);
        cx.notify();
    }

    fn apply_filter_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = &self.bar().draft else {
            return;
        };
        let key = draft.property.clone();
        let op = draft.op;
        let text = self.bar().editor.read(cx).text(cx);
        let parsed = if op == FilterOp::Expr {
            let text = text.trim();
            if text.is_empty() {
                Err("Enter a condition".to_owned())
            } else {
                self.check_condition(text)
                    .map(|()| Value::String(text.to_owned()))
            }
        } else if op.is_relationship() {
            match op {
                FilterOp::RelatedTo if text.trim().is_empty() => Err("Enter a node id".to_owned()),
                FilterOp::RelatedTo => Ok(Value::String(text.trim().to_owned())),
                _ => Ok(Value::Null),
            }
        } else {
            let Some(mut property) = self.property_for_key(&key) else {
                return;
            };
            // An average of whole numbers is a fraction.
            if let Some(FilterDraft {
                target:
                    DraftTarget::Aggregate {
                        quantifier: Quantifier::Avg,
                        ..
                    },
                ..
            }) = &self.bar().draft
            {
                property.kind = ScalarKind::Float;
            }
            if op == FilterOp::In {
                parse_filter_list(&property, &text)
            } else if op.takes_value() {
                parse_filter_value(&property, &text)
            } else {
                Ok(Value::Null)
            }
        };
        let value = {
            match parsed {
                Ok(value) => value,
                Err(error) => {
                    if let Some(draft) = &mut self.bar_mut().draft {
                        draft.error = Some(error);
                    }
                    cx.notify();
                    return;
                }
            }
        };
        let target = self
            .bar()
            .draft
            .as_ref()
            .map_or(DraftTarget::Bar, |draft| draft.target.clone());
        if let Some(group) = target.group() {
            let Some(index) = self.filters().iter().position(|f| f == group) else {
                self.bar_mut().draft = None;
                cx.notify();
                return;
            };
            match into_group(group, &target, &key, op, value) {
                Ok(filter) => self.filters_mut()[index] = filter,
                Err(error) => {
                    if let Some(draft) = &mut self.bar_mut().draft {
                        draft.error = Some(error);
                    }
                    cx.notify();
                    return;
                }
            }
            self.bar_mut().draft = None;
            window.focus(&self.host_focus(), cx);
            self.filters_changed(cx);
            return;
        }
        let filter = if builder::is_deep_path(&key) {
            match builder::path_filter(&key, op, value) {
                Some(filter) => filter,
                None => return,
            }
        } else {
            Filter {
                property: key,
                op,
                value,
            }
        };
        let replacing = self
            .bar_mut()
            .draft
            .take()
            .and_then(|draft| draft.replacing);
        match replacing.and_then(|old| self.filters().iter().position(|f| *f == old)) {
            Some(index) => self.filters_mut()[index] = filter,
            None => self.filters_mut().push(filter),
        }
        window.focus(&self.host_focus(), cx);
        self.filters_changed(cx);
    }

    /// ↵ for the draft, and whether the key was its: a draft is open and its value field has
    /// the keyboard. The host's own ↵ calls this before it does anything else of its own.
    fn confirm_filter_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !(self.bar().draft.is_some() && self.bar().editor.focus_handle(cx).is_focused(window)) {
            return false;
        }
        // ↵ picks the highlighted offer, unless it is just what is typed. While the nodes
        // for what is typed are being read it waits: the ones offered were found for
        // earlier text.
        let text = self.bar().editor.read(cx).text(cx);
        if self
            .bar()
            .draft
            .as_ref()
            .is_some_and(|draft| draft.answers.as_deref() != Some(text.as_str()))
        {
            return true;
        }
        let pick = self.bar().draft.as_ref().and_then(|draft| {
            let index = draft.offers.highlighted()?;
            let suggestion = draft.offers.get(index)?;
            let list = draft.op == FilterOp::In;
            (!suggest::already_typed(&text, &suggestion.value, list)).then_some(index)
        });
        match pick {
            Some(index) => self.pick_suggestion(index, window, cx),
            None => self.apply_filter_draft(window, cx),
        }
        true
    }

    /// A group pill's menu (spec 025): how many related records must pass, each test (edit,
    /// separate, remove), a test to add, another pill on the same relationship to merge, and
    /// the filter's removal. A neighbour's property pill opens it too, as the group of one test
    /// it is.
    fn open_group_menu(
        &mut self,
        index: usize,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(filter) = self.filters().get(index).cloned() else {
            return;
        };
        let Some((key, group)) = builder::as_group(&filter) else {
            return;
        };
        let Some(schema) = self.schema().cloned() else {
            return;
        };
        let Some(root) = self.group_root(&key, PathOffer::Tests) else {
            return;
        };
        let table = cx.entity().downgrade();
        let other_type = root.type_name.clone();
        // Other pills on this relationship that any of its records passes: merged, their tests
        // join this group's, one record passing all of them.
        let merges: Vec<(Filter, String)> = self
            .filters()
            .iter()
            .enumerate()
            .filter(|(at, _)| *at != index)
            .filter_map(|(_, other)| {
                let (other_key, other_group) = builder::as_group(other)?;
                (other_key == key
                    && other_group.quantifier == Quantifier::Any
                    && !other_group.tests.is_empty())
                .then(|| (other.clone(), crate::model::filter_sentence(other)))
            })
            .collect();
        let can_merge = group.quantifier == Quantifier::Any;
        let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            let edit = |f: Rc<GroupEdit<Self>>| {
                let table = table.clone();
                move |window: &mut Window, cx: &mut App| {
                    table.update(cx, |this, cx| f(this, window, cx)).ok();
                }
            };
            let set = |made: Group| {
                let (filter, key) = (filter.clone(), key.clone());
                edit(Rc::new(move |this, _, cx| {
                    this.replace_filter(&filter, Some(builder::group_filter(&key, &made)), cx)
                }))
            };
            let with = |quantifier: Quantifier, count: Option<u32>| Group {
                quantifier,
                count,
                of: None,
                compare: None,
                than: None,
                ..group.clone()
            };
            menu = menu.header("Which of them");
            for (quantifier, label) in [
                (Quantifier::Any, "Any of them"),
                (Quantifier::None, "None of them"),
                (Quantifier::Every, "Every one"),
            ] {
                menu = menu.toggleable_entry(
                    label,
                    group.quantifier == quantifier,
                    IconPosition::Start,
                    None,
                    set(with(quantifier, None)),
                );
            }
            for (quantifier, label, counts) in [
                (Quantifier::AtLeast, "At least", [1u32, 2, 3, 5, 10]),
                (Quantifier::AtMost, "At most", [0u32, 1, 2, 3, 5]),
            ] {
                let current = (group.quantifier == quantifier)
                    .then_some(group.count)
                    .flatten();
                let label = match current {
                    Some(n) => format!("{label} {n}"),
                    None => label.to_owned(),
                };
                let items: Vec<(u32, _)> = counts
                    .into_iter()
                    .map(|n| (n, set(with(quantifier, Some(n)))))
                    .collect();
                let items = Rc::new(std::cell::RefCell::new(Some(items)));
                menu = menu.submenu(label, move |mut menu, _, _| {
                    for (n, handler) in items.borrow_mut().take().into_iter().flatten() {
                        menu = menu.toggleable_entry(
                            n.to_string(),
                            current == Some(n),
                            IconPosition::Start,
                            None,
                            handler,
                        );
                    }
                    menu
                });
            }
            for (quantifier, label) in [
                (Quantifier::Sum, "Total of"),
                (Quantifier::Avg, "Average of"),
                (Quantifier::Min, "Smallest"),
                (Quantifier::Max, "Largest"),
            ] {
                let (table, schema, filter) = (table.clone(), schema.clone(), filter.clone());
                let root = root.clone();
                let pick: PickKey = Rc::new(move |key, window, cx| {
                    let filter = filter.clone();
                    table
                        .update(cx, |this, cx| {
                            this.start_group_draft(
                                key,
                                DraftTarget::Aggregate {
                                    group: filter,
                                    quantifier,
                                },
                                window,
                                cx,
                            )
                        })
                        .ok();
                });
                menu = menu.submenu(label, move |menu, _, _| {
                    path_menu(
                        menu,
                        &pick,
                        &schema,
                        &root.key,
                        &root.type_name,
                        &root.edge,
                        root.depth,
                        root.icon,
                        PathOffer::Aggregate(quantifier),
                    )
                });
            }
            menu = menu.separator().header(format!("Each {other_type} where"));
            let relationship = crate::model::group_parts(&key, &group).0;
            for (at, test) in group.tests.iter().enumerate() {
                let label = crate::model::filter_sentence(&builder::group_filter(
                    &key,
                    &Group {
                        tests: vec![test.clone()],
                        ..Group::default()
                    },
                ))
                .trim_start_matches(relationship.as_str())
                .trim_start_matches(" any where ")
                .to_owned();
                let path = builder::test_path(test)
                    .map(|(relative, op, value)| (format!("{key}.{relative}"), op, value));
                let (edit_test, separate, remove) = {
                    let (filter, key, group) = (filter.clone(), key.clone(), group.clone());
                    let editing = path.clone().map(|(path, op, value)| {
                        let filter = filter.clone();
                        edit(Rc::new(move |this, window, cx| {
                            this.start_group_draft(
                                path.clone(),
                                DraftTarget::Test {
                                    group: filter.clone(),
                                    test: Some(at),
                                },
                                window,
                                cx,
                            );
                            this.fill_draft(op, &value, window, cx);
                        }))
                    });
                    let separate = {
                        let (filter, key, group) = (filter.clone(), key.clone(), group.clone());
                        edit(Rc::new(move |this, _, cx| {
                            let mut kept = group.clone();
                            let moved = kept.tests.remove(at);
                            let own = builder::group_filter(
                                &key,
                                &Group {
                                    tests: vec![moved],
                                    ..Group::default()
                                },
                            );
                            let rest = (!kept.tests.is_empty())
                                .then(|| builder::group_filter(&key, &kept));
                            this.replace_filter(&filter, rest, cx);
                            this.filters_mut().push(own);
                            this.filters_changed(cx);
                        }))
                    };
                    let remove = edit(Rc::new(move |this, _, cx| {
                        let mut kept = group.clone();
                        kept.tests.remove(at);
                        // A group of none or every without a test still says something.
                        let rest = (!kept.tests.is_empty()
                            || !matches!(kept.quantifier, Quantifier::Any))
                        .then(|| builder::group_filter(&key, &kept));
                        this.replace_filter(&filter, rest, cx);
                    }));
                    (editing, separate, remove)
                };
                let several = group.tests.len() > 1;
                let cell = Rc::new(std::cell::RefCell::new(Some((edit_test, separate, remove))));
                menu = menu.submenu(label, move |mut menu, _, _| {
                    if let Some((edit_test, separate, remove)) = cell.borrow_mut().take() {
                        if let Some(edit_test) = edit_test {
                            menu = menu.entry("Edit", None, edit_test);
                        }
                        if several {
                            menu = menu.entry("Separate into its own filter", None, separate);
                        }
                        menu = menu.entry("Remove", None, remove);
                    }
                    menu
                });
            }
            {
                let (table, filter) = (table.clone(), filter.clone());
                let pick: PickKey = Rc::new(move |key, window, cx| {
                    let filter = filter.clone();
                    table
                        .update(cx, |this, cx| {
                            this.start_group_draft(
                                key,
                                DraftTarget::Test {
                                    group: filter,
                                    test: None,
                                },
                                window,
                                cx,
                            )
                        })
                        .ok();
                });
                let (schema, root) = (schema.clone(), root.clone());
                menu = menu.submenu("Add a test", move |menu, _, _| {
                    path_menu(
                        menu,
                        &pick,
                        &schema,
                        &root.key,
                        &root.type_name,
                        &root.edge,
                        root.depth,
                        root.icon,
                        PathOffer::Tests,
                    )
                });
            }
            if can_merge && !merges.is_empty() {
                menu = menu.separator().header(format!("The same {other_type} as"));
                for (other, sentence) in &merges {
                    let (filter, other, key, group) =
                        (filter.clone(), other.clone(), key.clone(), group.clone());
                    menu = menu.entry(
                        sentence.clone(),
                        None,
                        edit(Rc::new(move |this, _, cx| {
                            let Some((_, theirs)) = builder::as_group(&other) else {
                                return;
                            };
                            let mut merged = group.clone();
                            merged.tests.extend(theirs.tests);
                            this.filters_mut().retain(|f| *f != other);
                            this.replace_filter(
                                &filter,
                                Some(builder::group_filter(&key, &merged)),
                                cx,
                            );
                        })),
                    );
                }
            }
            let filter = filter.clone();
            menu.separator().entry(
                "Remove filter",
                None,
                edit(Rc::new(move |this, _, cx| {
                    this.replace_filter(&filter, None, cx)
                })),
            )
        });
        let subscription = cx.subscribe_in(
            &menu,
            window,
            |this, _, _: &gpui::DismissEvent, window, cx| {
                this.bar_mut().group_menu = None;
                if this.bar().draft.is_none() {
                    window.focus(&this.host_focus(), cx);
                }
                cx.notify();
            },
        );
        window.focus(&menu.focus_handle(cx), cx);
        self.bar_mut().group_menu = Some((menu, position, subscription));
        cx.notify();
    }

    /// Puts `made` where `old` stands in the bar, or takes `old` out, and reads again.
    fn replace_filter(&mut self, old: &Filter, made: Option<Filter>, cx: &mut Context<Self>) {
        let Some(index) = self.filters().iter().position(|f| f == old) else {
            return;
        };
        match made {
            Some(made) => self.filters_mut()[index] = made,
            None => {
                self.filters_mut().remove(index);
            }
        }
        self.filters_changed(cx);
    }

    /// A draft whose key is a path from a group's relationship, applied into the group.
    fn start_group_draft(
        &mut self,
        key: String,
        target: DraftTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_draft_property(key, window, cx);
        if let Some(draft) = &mut self.bar_mut().draft {
            draft.target = target;
        }
        if let Some(draft) = &self.bar().draft {
            let op = self.draft_ops(draft).first().copied();
            if let Some(op) = op {
                self.set_draft_op(op, window, cx);
            }
        }
        cx.notify();
    }

    /// The draft's test and value as `op` and `value` say, as a pill opened to edit shows them.
    fn fill_draft(
        &mut self,
        op: FilterOp,
        value: &Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_draft_op(op, window, cx);
        let text = if op.takes_value() {
            edit_text(value)
        } else {
            String::new()
        };
        self.bar().editor.update(cx, |editor, cx| {
            editor.set_text(text, window, cx);
            let end = editor.buffer().read(cx).read(cx).len();
            editor.change_selections(Default::default(), window, cx, |s| {
                s.select_ranges(vec![end..end])
            });
        });
        self.refresh_suggestions(cx);
    }

    fn remove_filter(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.filters().len() {
            self.filters_mut().remove(index);
            self.filters_changed(cx);
        }
    }

    fn clear_filters(&mut self, cx: &mut Context<Self>) {
        self.bar_mut().draft = None;
        if !self.filters().is_empty() {
            self.filters_mut().clear();
            self.filters_changed(cx);
        }
        cx.notify();
    }

    fn render_filter_bar(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let note = self.bar_note();
        if self.filters().is_empty() && self.bar().draft.is_none() && note.is_none() {
            return None;
        }
        let mut bar = h_flex()
            .flex_none()
            .flex_wrap()
            .px(px(space::EDGE))
            .py(px(space::SM))
            .gap(px(space::SM))
            .items_center()
            .border_b_1()
            .border_color(studio_ui::ds::surface::rule(cx));
        // The filter being edited shows as the draft, after the others.
        let editing = self
            .bar()
            .draft
            .as_ref()
            .and_then(|draft| draft.replacing.as_ref())
            .and_then(|old| self.filters().iter().position(|f| f == old));
        for (index, filter) in self.filters().iter().enumerate() {
            if Some(index) == editing {
                continue;
            }
            let (field, test, mut value) = filter_parts(filter);
            if filter.op == FilterOp::RelatedTo
                && let Some(label) = value.as_ref().and_then(|id| self.bar().node_labels.get(id))
            {
                value = Some(label.clone());
            }
            let pill = studio_ui::ds::filter_pill(
                ("filter", index),
                self.key_icon(&filter.property),
                field,
                test,
                value.map(Into::into),
                cx.listener(move |this, _, _, cx| this.remove_filter(index, cx)),
                cx,
            )
            .cursor_pointer();
            // A group opens its menu (spec 025); a neighbour's property, the group of one test
            // it is, opens it on a right click, and edits as any pill on a click.
            let pill = if filter.op == FilterOp::Where {
                pill.tip(studio_ui::ds::tooltip(crate::model::filter_sentence(
                    filter,
                )))
                .on_click(cx.listener(
                    move |this, event: &ClickEvent, window, cx| {
                        this.open_group_menu(index, event.position(), window, cx)
                    },
                ))
            } else {
                let grouped = builder::as_group(filter).is_some();
                pill.tip(studio_ui::ds::tooltip(if grouped {
                    "Edit this filter · right-click to test the same record again"
                } else {
                    "Edit this filter"
                }))
                .on_click(
                    cx.listener(move |this, _, window, cx| this.edit_filter(index, window, cx)),
                )
                .when(grouped, |pill| {
                    pill.on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            this.open_group_menu(index, event.position, window, cx)
                        }),
                    )
                })
            };
            bar = bar.child(pill);
        }
        if let Some(draft) = &self.bar().draft {
            let table = cx.entity().downgrade();
            let choices = self.filter_choices();
            let entries = self.filter_menu();
            let chosen = choices
                .iter()
                .find(|(key, _, _)| *key == draft.property)
                .map(|(_, label, _)| label.clone())
                .unwrap_or_else(|| path_label(&draft.property));
            let ops = self.draft_ops(draft);
            let property_table = table.clone();
            let schema = self.schema().cloned();
            let rooted = self.draft_root(draft);
            let chosen_icon = self.key_icon(&draft.property);
            // The filter being written is the pill it will become, outlined while it is open:
            // a ring outside it as the pill's own edge is, so its buttons keep a button's height.
            let pill = studio_ui::ds::filter_draft("filter-draft", draft.error.is_some(), cx)
                .child(
                    PopoverMenu::new("filter-property")
                        .with_handle(self.bar().menus[0].clone())
                        .trigger(studio_ui::ds::filter_draft_choice(
                            "filter-property-button",
                            chosen,
                            Some(chosen_icon),
                        ))
                        .menu(move |window, cx| {
                            let table = property_table.clone();
                            let choices = choices.clone();
                            let entries = entries.clone();
                            let (schema, rooted) = (schema.clone(), rooted.clone());
                            Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                if let Some((schema, root)) = schema.clone().zip(rooted.clone()) {
                                    return path_menu(
                                        menu,
                                        &pick_for(&table),
                                        &schema,
                                        &root.key,
                                        &root.type_name,
                                        &root.edge,
                                        root.depth,
                                        root.icon,
                                        root.offer,
                                    );
                                }
                                let mut relationships = false;
                                for entry in entries.iter() {
                                    let at = match entry {
                                        FilterMenuEntry::Key(at)
                                        | FilterMenuEntry::Relationship { key: at, .. } => *at,
                                    };
                                    let (key, label, icon) = &choices[at];
                                    if !relationships
                                        && builder::parse_relationship_key(key).is_some()
                                    {
                                        relationships = true;
                                        menu = menu.separator().header("Relationships");
                                    }
                                    match entry {
                                        FilterMenuEntry::Key(_) => {
                                            menu = menu.item(filter_key_entry(
                                                &table,
                                                key.clone(),
                                                label.clone(),
                                                *icon,
                                            ));
                                        }
                                        // The neighbours' properties open beside it, as the
                                        // Group menu's do; the relationship itself comes first.
                                        FilterMenuEntry::Relationship { other_type, .. } => {
                                            let Some(schema) = schema.clone() else {
                                                continue;
                                            };
                                            let edge = builder::parse_relationship_key(key)
                                                .map(|(_, edge)| edge.to_owned())
                                                .unwrap_or_default();
                                            let table = table.clone();
                                            let (key, label, icon) =
                                                (key.clone(), label.clone(), *icon);
                                            let other_type = other_type.clone();
                                            menu = menu.submenu_with_colored_icon(
                                                label.clone(),
                                                icon,
                                                Color::Muted,
                                                move |menu, _, _| {
                                                    let menu = menu
                                                        .item(filter_key_entry(
                                                            &table,
                                                            key.clone(),
                                                            label.clone(),
                                                            icon,
                                                        ))
                                                        .separator();
                                                    path_menu(
                                                        menu,
                                                        &pick_for(&table),
                                                        &schema,
                                                        &key,
                                                        &other_type,
                                                        &edge,
                                                        1,
                                                        icon,
                                                        PathOffer::Tests,
                                                    )
                                                },
                                            );
                                        }
                                    }
                                }
                                menu
                            }))
                        }),
                )
                .child(studio_ui::ds::pill_rule(cx))
                .child(
                    PopoverMenu::new("filter-op")
                        .with_handle(self.bar().menus[1].clone())
                        .trigger(studio_ui::ds::filter_draft_choice(
                            "filter-op-button",
                            draft.op.label(),
                            None,
                        ))
                        .menu(move |window, cx| {
                            let table = table.clone();
                            let ops = ops.clone();
                            Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                for op in ops {
                                    let table = table.clone();
                                    menu = menu.entry(op.label(), None, move |window, cx| {
                                        table
                                            .update(cx, |this, cx| {
                                                this.set_draft_op(op, window, cx)
                                            })
                                            .ok();
                                    });
                                }
                                menu
                            }))
                        }),
                )
                .when(draft.op.takes_value(), |pill| {
                    let menu_open = self.bar().menus.iter().any(PopoverMenuHandle::is_deployed);
                    let offers = (self.bar().editor.focus_handle(cx).is_focused(window)
                        && !menu_open
                        && !draft.offers.is_empty())
                    .then(|| self.render_suggestions(draft, cx));
                    pill.child(studio_ui::ds::pill_rule(cx)).child(
                        studio_ui::ds::filter_draft_value()
                            .key_context("FilterValue")
                            .on_action(cx.listener(|this, _: &SelectNextSuggestion, _, cx| {
                                this.move_suggestion(1, cx)
                            }))
                            .on_action(cx.listener(|this, _: &SelectPreviousSuggestion, _, cx| {
                                this.move_suggestion(-1, cx)
                            }))
                            .child(self.bar().editor.clone())
                            .children(offers),
                    )
                })
                .child(studio_ui::ds::pill_rule(cx))
                .child(studio_ui::ds::filter_draft_end(
                    cx.listener(|this, _, window, cx| this.apply_filter_draft(window, cx)),
                    cx.listener(|this, _, window, cx| this.cancel_filter_draft(window, cx)),
                    cx,
                ));
            bar = bar
                .child(pill)
                .children(draft.error.clone().map(studio_ui::ds::text::error));
        }
        bar = bar.children(note);
        if !self.filters().is_empty() {
            bar = bar.child(div().flex_1()).child(
                studio_ui::ds::button("clear-filters", "Clear all")
                    .style(ButtonStyle::Subtle)
                    .tip_fn(|_, cx| {
                        studio_ui::ds::tooltip_for_action_view(
                            "Remove every filter",
                            &ClearFilters,
                            cx,
                        )
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.clear_filters(cx))),
            );
        }
        Some(bar.into_any_element())
    }

    /// The values the filter's value field offers, under it: ↑/↓ highlight, ↵ or a click picks.
    fn render_suggestions(&self, draft: &FilterDraft, cx: &mut Context<Self>) -> AnyElement {
        let pick =
            cx.listener(|this, index: &usize, window, cx| this.pick_suggestion(*index, window, cx));
        let items = draft.offers.rows(
            "filter-suggestion",
            Rc::new(move |index, window, cx| pick(&index, window, cx)),
            cx,
        );
        // Under the value field, its list's rows on the field's text.
        floating_under(
            point(px(-space::XS), px(studio_ui::ds::size::CONTROL)),
            1,
            studio_ui::ds::popover_list(cx)
                .id("filter-suggestions")
                .occlude()
                .min_w(px(studio_ui::ds::size::POPOVER_MIN))
                .max_w(px(studio_ui::ds::size::POPOVER_MAX))
                .max_h(px(studio_ui::ds::size::LIST_MAX_HEIGHT))
                .overflow_y_scroll()
                .children(items),
        )
    }
}

pub(crate) fn filter_placeholder(op: FilterOp) -> &'static str {
    match op {
        FilterOp::In => "Values, separated by commas",
        FilterOp::RelatedTo => "Name or id",
        FilterOp::StartsWith => "Start of the value",
        FilterOp::Search => "Words, in any order",
        FilterOp::Expr => "GQ on $n, for example $n.kind = \"dinner\" or $n.kind = \"party\"",
        _ => "Value",
    }
}

/// A relationship's direction, as filters and the Group menu show it.
pub(crate) fn direction_icon(direction: gq_language::Direction) -> IconName {
    match direction {
        gq_language::Direction::Outgoing => IconName::ArrowRight,
        gq_language::Direction::Incoming => IconName::ArrowLeft,
    }
}

/// `content` floating under the element this is placed in, `offset` from its top-left corner:
/// drawn over the rest of the window, `priority` among other overlays, and kept inside it by a
/// margin.
pub(crate) fn floating_under(
    offset: gpui::Point<Pixels>,
    priority: usize,
    content: impl IntoElement,
) -> AnyElement {
    div()
        .absolute()
        .top_0()
        .left_0()
        .child(
            deferred(
                anchored()
                    .anchor(Anchor::TopLeft)
                    .offset(offset)
                    .snap_to_window_with_margin(px(space::MD))
                    .child(content),
            )
            .with_priority(priority),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use gpui::{Render, TestAppContext, VisualTestContext};
    use serde_json::json;
    use studio_actions::table::{Cancel, ConfirmEdit};

    use super::*;
    use crate::model::node_columns;
    use crate::test_support::{session, session_routing};

    const SCHEMA: &str = "node Person {\n  slug: String @key\n  name: String\n  city: String?\n  \
                          active: Bool?\n}\n\
                          node Org {\n  slug: String @key\n  name: String\n}\n\
                          edge WorksAt: Person -> Org\n";

    /// A second host of the bar: a view of Person nodes with filters of its own and nothing of
    /// the table's. It has no rows: what it would read again is counted.
    struct Probe {
        bar: FilterBar,
        filters: Vec<Filter>,
        session: Entity<GraphSession>,
        schema: Arc<SchemaModel>,
        columns: Vec<ColumnSpec>,
        pinned: Option<String>,
        focus: FocusHandle,
        /// How often the bar said the filters changed.
        changed: usize,
    }

    impl Probe {
        fn new(session: Entity<GraphSession>, window: &mut Window, cx: &mut Context<Self>) -> Self {
            let schema = Arc::new(SchemaModel::parse(SCHEMA).unwrap());
            let columns = node_columns(
                schema.node("Person").unwrap(),
                schema.relationships_of("Person"),
            );
            let mut probe = Self {
                bar: FilterBar::new(window, cx),
                filters: Vec::new(),
                session,
                schema,
                columns,
                pinned: None,
                focus: cx.focus_handle(),
                changed: 0,
            };
            probe.rebuild_filter_choices();
            probe
        }
    }

    impl FilterHost for Probe {
        fn bar(&self) -> &FilterBar {
            &self.bar
        }

        fn bar_mut(&mut self) -> &mut FilterBar {
            &mut self.bar
        }

        fn filters(&self) -> &[Filter] {
            &self.filters
        }

        fn filters_mut(&mut self) -> &mut Vec<Filter> {
            &mut self.filters
        }

        fn schema(&self) -> Option<&Arc<SchemaModel>> {
            Some(&self.schema)
        }

        fn type_name(&self) -> &str {
            "Person"
        }

        fn kind(&self) -> TableKind {
            TableKind::Node
        }

        fn all_columns(&self) -> &[ColumnSpec] {
            &self.columns
        }

        fn session(&self) -> &Entity<GraphSession> {
            &self.session
        }

        fn pinned(&self) -> Option<String> {
            self.pinned.clone()
        }

        fn host_focus(&self) -> FocusHandle {
            self.focus.clone()
        }

        fn filters_changed(&mut self, _cx: &mut Context<Self>) {
            self.changed += 1;
        }
    }

    impl Render for Probe {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .track_focus(&self.focus)
                .size_full()
                // ↵ and Esc are the host's keys: it asks the bar first.
                .on_action(cx.listener(|this, _: &ConfirmEdit, window, cx| {
                    this.confirm_filter_draft(window, cx);
                }))
                .on_action(cx.listener(|this, _: &Cancel, window, cx| {
                    if this.bar().draft.is_some() {
                        this.cancel_filter_draft(window, cx);
                    }
                }))
                .children(self.render_filter_bar(window, cx))
                .children(
                    self.bar()
                        .group_menu
                        .as_ref()
                        .map(|(menu, position, _)| studio_ui::ds::menu_at(menu.clone(), *position)),
                )
        }
    }

    /// The probe in a window of its own, on `session`.
    fn probe_on(
        session: Entity<GraphSession>,
        cx: &mut TestAppContext,
    ) -> (Entity<Probe>, &mut VisualTestContext) {
        let (probe, cx) = cx.add_window_view(|window, cx| Probe::new(session, window, cx));
        cx.run_until_parked();
        (probe, cx)
    }

    fn probe(cx: &mut TestAppContext) -> (Entity<Probe>, &mut VisualTestContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let session = session(SCHEMA, cx);
        probe_on(session, cx)
    }

    /// Opens the draft's key menu (0) or test menu (1) and takes its `nth` entry with the
    /// keyboard.
    fn pick_from_menu(probe: &Entity<Probe>, menu: usize, nth: usize, cx: &mut VisualTestContext) {
        probe.update_in(cx, |probe, window, cx| {
            probe.bar().menus[menu].show(window, cx)
        });
        cx.run_until_parked();
        // The menu takes the keyboard two frames after it opens, as Zed's popover menus do.
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
            window.simulate_next_frame(cx);
        });
        for _ in 0..nth {
            cx.dispatch_action(menu::SelectNext);
        }
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
    }

    fn text_of(probe: &Entity<Probe>, cx: &mut VisualTestContext) -> String {
        probe.read_with(cx, |probe, cx| probe.bar().editor.read(cx).text(cx))
    }

    /// A view that is not the table has the whole bar: a draft opened, its key and its test
    /// chosen from their menus, a value typed, and ↵ adds the filter, tells the host once and
    /// gives it the keyboard back.
    #[gpui::test]
    async fn a_second_host_writes_a_filter_and_is_told_once(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        probe.update_in(cx, |probe, window, cx| {
            assert!(probe.render_filter_bar(window, cx).is_none(), "no bar yet");
            probe.open_filter_draft(window, cx);
        });
        cx.run_until_parked();
        probe.update_in(cx, |probe, window, cx| {
            let draft = probe.bar().draft.as_ref().expect("a draft is open");
            // No cursor to start from: the first property that can be filtered.
            assert_eq!(draft.property, "name");
            assert!(probe.bar().editor.focus_handle(cx).is_focused(window));
        });

        // The key menu lists name, city, active, then the relationship.
        pick_from_menu(&probe, 0, 2, cx);
        let ops = probe.read_with(cx, |probe, _| {
            let draft = probe.bar().draft.as_ref().unwrap();
            assert_eq!(draft.property, "city");
            probe.draft_ops(draft)
        });
        assert!(ops.len() > 1, "{ops:?}");
        pick_from_menu(&probe, 1, 2, cx);
        probe.update_in(cx, |probe, window, cx| {
            assert_eq!(probe.bar().draft.as_ref().unwrap().op, ops[1]);
            assert!(probe.bar().editor.focus_handle(cx).is_focused(window));
        });

        cx.simulate_input("Paris");
        cx.run_until_parked();
        assert_eq!(text_of(&probe, cx), "Paris");
        probe.read_with(cx, |probe, _| {
            assert!(probe.filters.is_empty(), "nothing is filtered until ↵");
            assert_eq!(probe.changed, 0);
        });
        cx.dispatch_action(ConfirmEdit);
        cx.run_until_parked();
        probe.update_in(cx, |probe, window, _| {
            assert_eq!(
                probe.filters,
                [Filter {
                    property: "city".into(),
                    op: ops[1],
                    value: json!("Paris"),
                }]
            );
            assert_eq!(probe.changed, 1, "the host reads its rows again once");
            assert!(probe.bar().draft.is_none());
            assert!(probe.focus.is_focused(window), "the keyboard is the host's");
        });
    }

    /// A pill opens as the draft and is replaced where it stands; its × takes it out; and
    /// "Clear all" the rest. Each tells the host once.
    #[gpui::test]
    async fn a_second_hosts_pills_are_edited_and_removed(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        let named = |name: &str| Filter {
            property: "name".into(),
            op: FilterOp::Eq,
            value: json!(name),
        };
        let active = Filter {
            property: "active".into(),
            op: FilterOp::Eq,
            value: json!(true),
        };
        probe.update_in(cx, |probe, window, cx| {
            probe.filters = vec![named("Ada"), active.clone()];
            assert!(probe.render_filter_bar(window, cx).is_some());
            probe.edit_filter(0, window, cx);
            assert_eq!(
                probe.filters,
                [named("Ada"), active.clone()],
                "still applied while edited"
            );
            assert_eq!(probe.bar().editor.read(cx).text(cx), "Ada");
        });
        cx.run_until_parked();
        cx.simulate_input("h");
        cx.run_until_parked();
        cx.dispatch_action(ConfirmEdit);
        cx.run_until_parked();
        probe.update_in(cx, |probe, _, cx| {
            assert_eq!(
                probe.filters,
                [named("Adah"), active.clone()],
                "replaced where it stood"
            );
            assert_eq!(probe.changed, 1);

            probe.remove_filter(0, cx);
            assert_eq!(probe.filters, std::slice::from_ref(&active));
            assert_eq!(probe.changed, 2);

            probe.clear_filters(cx);
            assert!(probe.filters.is_empty());
            assert_eq!(probe.changed, 3);
        });
        cx.run_until_parked();
        probe.update_in(cx, |probe, window, cx| {
            assert!(probe.render_filter_bar(window, cx).is_none(), "no bar left");
        });
    }

    /// Esc closes the draft, changes no filter and gives the host the keyboard back.
    #[gpui::test]
    async fn esc_closes_a_second_hosts_draft(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        probe.update_in(cx, |probe, window, cx| probe.open_filter_draft(window, cx));
        cx.run_until_parked();
        cx.simulate_input("Ada");
        cx.run_until_parked();
        probe.update_in(cx, |probe, window, cx| {
            assert!(probe.bar().draft.is_some());
            assert!(probe.bar().editor.focus_handle(cx).is_focused(window));
        });
        cx.dispatch_action(Cancel);
        cx.run_until_parked();
        probe.update_in(cx, |probe, window, _| {
            assert!(probe.bar().draft.is_none());
            assert!(probe.filters.is_empty());
            assert_eq!(probe.changed, 0);
            assert!(probe.focus.is_focused(window), "the keyboard is the host's");
        });
    }

    /// Typing in the value field and moving through what it offers do not notify the host: a
    /// part of it drawn cached, as the table's grid is, keeps its frame. Opening the draft
    /// does.
    #[gpui::test]
    async fn typing_in_the_value_field_does_not_notify_a_second_host(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        let notified = Rc::new(Cell::new(0));
        cx.update(|_, cx| {
            let notified = notified.clone();
            cx.observe(&probe, move |_, _| notified.set(notified.get() + 1))
                .detach();
        });
        probe.update_in(cx, |probe, window, cx| probe.open_filter_draft(window, cx));
        cx.run_until_parked();
        assert!(notified.get() > 0, "opening the draft draws the host");

        let before = notified.get();
        cx.simulate_input("Ad");
        cx.run_until_parked();
        assert_eq!(text_of(&probe, cx), "Ad");
        assert_eq!(notified.get(), before, "typing notified the host");

        // A Bool's field offers true and false; ↓ highlights the first.
        probe.update_in(cx, |probe, window, cx| {
            probe.set_draft_property("active".into(), window, cx)
        });
        cx.run_until_parked();
        let before = notified.get();
        cx.dispatch_action(SelectNextSuggestion);
        cx.run_until_parked();
        probe.read_with(cx, |probe, _| {
            let offers = &probe.bar().draft.as_ref().unwrap().offers;
            assert_eq!(offers.len(), 2);
            assert_eq!(offers.highlighted(), Some(0));
        });
        assert_eq!(notified.get(), before, "the offers notified the host");
    }

    /// A value the field offers is picked with ↵ and filters at once.
    #[gpui::test]
    async fn a_second_host_is_offered_values_and_picks_one(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        probe.update_in(cx, |probe, window, cx| {
            probe.set_draft_property("active".into(), window, cx)
        });
        cx.run_until_parked();
        cx.dispatch_action(SelectNextSuggestion);
        cx.dispatch_action(SelectNextSuggestion);
        cx.run_until_parked();
        probe.read_with(cx, |probe, _| {
            let offers = &probe.bar().draft.as_ref().unwrap().offers;
            assert_eq!(offers.highlighted(), Some(1));
            assert_eq!(
                offers.get(1).map(|offer| offer.value.as_str()),
                Some("false")
            );
        });
        cx.dispatch_action(ConfirmEdit);
        cx.run_until_parked();
        probe.read_with(cx, |probe, _| {
            assert_eq!(
                probe.filters,
                [Filter {
                    property: "active".into(),
                    op: FilterOp::Eq,
                    value: json!(false),
                }]
            );
            assert_eq!(probe.changed, 1);
            assert!(probe.bar().draft.is_none());
        });
    }

    /// "Related to" searches the host's session for the nodes the text names, at the commit
    /// the host is pinned to, and the one picked filters by its id and reads by its name.
    #[gpui::test]
    async fn a_second_hosts_related_to_searches_its_session(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
        let searched_at = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = searched_at.clone();
        let (session, _) = session_routing(
            SCHEMA,
            move |request| match request["name"].as_str() {
                Some("studio_search_index") => {
                    seen.lock().unwrap().push(request["snapshot"].clone());
                    json!([
                        {"id": "acme.com", "p0": "ACME Corp"},
                        {"id": "initech.com", "p0": "Initech"},
                    ])
                }
                _ => json!([]),
            },
            cx,
        );
        let (probe, cx) = probe_on(session, cx);
        // Not the branch's head, which the fake server calls `c1`.
        probe.update(cx, |probe, _| probe.pinned = Some("c7".into()));
        let key = probe.read_with(cx, |probe, _| {
            builder::relationship_key(&probe.schema.relationships_of("Person")[0])
        });
        probe.update_in(cx, |probe, window, cx| {
            probe.set_draft_property(key.clone(), window, cx);
            probe.set_draft_op(FilterOp::RelatedTo, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input("acme");
        cx.executor().advance_clock(suggest::NODE_SEARCH_DELAY);
        cx.run_until_parked();
        probe.read_with(cx, |probe, _| {
            let offers = &probe.bar().draft.as_ref().unwrap().offers;
            assert_eq!(offers.len(), 1, "the node the text names");
            assert_eq!(offers.highlighted(), Some(0));
        });
        assert_eq!(
            *searched_at.lock().unwrap(),
            [json!("c7")],
            "the host's session was asked, at the host's commit"
        );
        cx.dispatch_action(ConfirmEdit);
        cx.run_until_parked();
        probe.read_with(cx, |probe, _| {
            assert_eq!(
                probe.filters,
                [Filter {
                    property: key.clone(),
                    op: FilterOp::RelatedTo,
                    value: json!("acme.com"),
                }]
            );
            assert_eq!(probe.changed, 1);
            assert_eq!(
                probe.bar().node_labels.get("acme.com").map(String::as_str),
                Some("ACME Corp")
            );
        });
    }
}

//! A filter from a sentence (spec 019): what the table, or a relationship in the tree, sends the
//! model (the type's
//! properties, enums, relationships and operators, and the sentence), the shape it asks the
//! answer in, and the answer checked against what the table offers. Every filter that comes
//! back goes through the same keys, operators and value parsing a filter typed by hand does;
//! one the type cannot take is dropped and named.

use gq_language::SchemaModel;
use gq_language::builder::{
    self, Filter, FilterOp, Group, GroupTest, MAX_HOPS, NODE_VAR, PageStart, Quantifier, Sort,
};
use gq_language::input::parse_scalar;
use gq_language::schema::Property;
use model::{Limits, Task as ModelTask, Tier};
use serde::Deserialize;
use serde_json::{Value, json};

pub static TASK: ModelTask = ModelTask {
    name: "table-filter",
    sends: "The type's property names, its enum values and your sentence",
    tier: Tier::Fast,
    // A wide type with many relationships describes itself in more than the tier's cap; the
    // call is still one short answer. The personal graph's widest type, Artifact, with its
    // order, grouping and columns, comes to about 33 KB. The answer names the filters, the
    // order, the grouping and the columns shown, which for a wide type ("every column but the
    // notes") runs past the tier's 300 tokens: cut off, the whole answer would be lost.
    limits: Limits {
        input_bytes: 48 * 1024,
        output_tokens: 1000,
        time: std::time::Duration::from_secs(20),
    },
};

pub const INSTRUCTIONS: &str = "Turn the request into filters on a table of rows of one type. \
Use only the properties and operators listed. Answer with the JSON object only. A date is \
ISO 8601. An operator that takes no value gets an empty list; `in` lists every member; every \
other operator takes one value. Prefer an enum's exact value. Filters are joined with and. \
When `groups` is offered, put tests on one related record in one group; use a group's \
quantifier for none, every, at least, at most and totals, and a group inside a group for a \
further hop. Put only or, and anything groups cannot say, in `predicate` as a GQ condition on \
the row variable $n and leave it out of the filters; otherwise predicate is null. When the request names nothing the table has, \
answer with no filters and a null predicate. Set `sort`, `group_by` and `columns` only when \
the request asks how the rows are ordered or grouped or which columns show; leave each empty \
otherwise.";

/// A key the table's filter menu offers: a property, or a relationship key.
#[derive(Debug, Clone)]
pub struct Choice {
    pub key: String,
    /// How the menu names it ("attended Event" for a relationship).
    pub label: String,
    pub ops: Vec<FilterOp>,
    pub property: Option<Property>,
    /// For a relationship key, the type at its other end, whose records `related_to` names.
    pub other_type: Option<String>,
    /// For a relationship key, the GQ that names it in a predicate's block, from the row `$n`
    /// to the related record `$r`: `$n attended $r`, or `$r knows $n` for an incoming key
    /// ([`gq_language::builder::relationship_traversal`]).
    pub traversal: Option<String>,
}

/// What a sentence may set beside its filters, as the table offers it: the properties that can
/// order the rows, the keys that can group them, and the columns, each by its key.
#[derive(Debug, Clone, Default)]
pub struct ViewChoices {
    pub sorts: Vec<String>,
    pub groups: Vec<String>,
    pub columns: Vec<String>,
}

/// What an answer sets beside its filters; `None` leaves that part of the table as it is.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct View {
    pub sort: Option<Sort>,
    pub group_by: Option<String>,
    /// The columns to show, in the order asked.
    pub columns: Option<Vec<String>>,
}

/// The answer's shape: filters whose property and operator are each one of the keys offered,
/// and the order, grouping and columns `view` offers, each a list the answer leaves empty to
/// keep the table's own. Each is a list, as a strict schema has no optional field; one whose
/// choices are none is left out, as an enum of none is no schema.
pub fn schema(choices: &[Choice], view: &ViewChoices) -> Value {
    schema_with(choices, view, None)
}

/// What a sentence may reach through groups (spec 025): the schema and the row's type. Without
/// it, an answer has no groups (the tree's relationship filter).
#[derive(Clone, Copy)]
pub struct Reach<'a> {
    pub schema: &'a SchemaModel,
    pub type_name: &'a str,
}

/// A type a group reaches: how many hops from the row, its name, and its relationships as
/// (key, edge, other type).
type Reached = (usize, String, Vec<(String, String, String)>);

impl Reach<'_> {
    /// The types a group can reach from the row, by how many hops: each with the edge that
    /// reached it, after the first hop's.
    fn types(&self) -> Vec<Reached> {
        let mut seen: Vec<String> = vec![self.type_name.to_owned()];
        let mut out = Vec::new();
        let mut frontier = vec![self.type_name.to_owned()];
        for hops in 1..=MAX_HOPS {
            let mut next = Vec::new();
            for type_name in &frontier {
                for group in self.schema.relationships_of(type_name) {
                    if seen.contains(&group.other_type) {
                        continue;
                    }
                    seen.push(group.other_type.clone());
                    next.push(group.other_type.clone());
                }
            }
            for type_name in &next {
                let relationships = self
                    .schema
                    .relationships_of(type_name)
                    .iter()
                    .map(|group| {
                        (
                            builder::relationship_key(group),
                            group.edge.clone(),
                            group.other_type.clone(),
                        )
                    })
                    .collect();
                out.push((hops, type_name.clone(), relationships));
            }
            frontier = next;
        }
        out
    }

    /// What a filter inside a group may name: every reachable type's filterable property and
    /// every edge property, as `@name`, once each.
    fn inside_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        let mut add = |name: String| {
            if !names.contains(&name) {
                names.push(name);
            }
        };
        for (_, type_name, _) in self.types() {
            if let Some(node) = self.schema.node(&type_name) {
                for property in &node.properties {
                    if !FilterOp::for_property(property).is_empty() {
                        add(property.name.clone());
                    }
                }
            }
        }
        for edge in &self.schema.edges {
            for property in &edge.properties {
                if !FilterOp::for_property(property).is_empty() {
                    add(builder::edge_property_key(&property.name));
                }
            }
        }
        add(builder::ID_FILTER.to_owned());
        names
    }
}

/// [`schema`], with groups when `reach` is given.
pub fn schema_with(choices: &[Choice], view: &ViewChoices, reach: Option<Reach>) -> Value {
    let inside = reach.map(|reach| reach.inside_names()).unwrap_or_default();
    let mut keys: Vec<&str> = choices.iter().map(|choice| choice.key.as_str()).collect();
    for name in &inside {
        if !keys.contains(&name.as_str()) {
            keys.push(name);
        }
    }
    let mut ops: Vec<&str> = choices
        .iter()
        .flat_map(|choice| choice.ops.iter().map(|op| op.key()))
        .collect();
    ops.sort_unstable();
    ops.dedup();
    let mut shape = json!({
        "type": "object",
        "properties": {
            "filters": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "property": {"type": "string", "enum": keys},
                        "op": {"type": "string", "enum": ops},
                        "value": {"type": "array", "items": {"type": "string"}},
                    },
                    "required": ["property", "op", "value"],
                    "additionalProperties": false,
                },
            },
            "predicate": {
                "type": ["string", "null"],
                "description": "A GQ condition on $n for what the filters cannot say: or, not, a count, a further hop. Null when the filters say it all.",
            },
        },
        "required": ["filters", "predicate"],
        "additionalProperties": false,
    });
    if reach.is_some() {
        let item = &mut shape["properties"]["filters"]["items"];
        item["properties"]["group"] = json!({
            "type": "string",
            "description": "Empty for a test on the row; else the id of the group whose related record it tests, and the property is that record's own or its edge's (@name).",
        });
        if let Some(required) = item["required"].as_array_mut() {
            required.push(json!("group"));
        }
    }
    let mut add = |name: &str, field: Value| {
        shape["properties"][name] = field;
        if let Some(required) = shape["required"].as_array_mut() {
            required.push(json!(name));
        }
    };
    if reach.is_some() {
        let quantifiers: Vec<&str> = Quantifier::ALL.iter().map(|q| q.key()).collect();
        add(
            "groups",
            json!({
                "type": "array",
                "description": "Tests on one related record, joined: each group is one relationship from the row (parent empty) or from another group's record (parent its id), and how many of its records must pass its filters.",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "parent": {"type": "string"},
                        "relationship": {"type": "string", "description": "A relationship key of the parent's record type, or of the row's."},
                        "quantifier": {"type": "string", "enum": quantifiers},
                        "count": {"type": ["integer", "null"], "description": "The n of at_least and at_most; else null."},
                        "of": {"type": "string", "description": "For sum, avg, min, max: the record's property or its edge's (@name); else empty."},
                        "compare": {"type": "string", "description": "For sum, avg, min, max: eq, ne, lt, le, gt or ge; else empty."},
                        "than": {"type": "string", "description": "For sum, avg, min, max: the value compared with; else empty."},
                    },
                    "required": ["id", "parent", "relationship", "quantifier", "count", "of", "compare", "than"],
                    "additionalProperties": false,
                },
            }),
        );
    }
    if !view.sorts.is_empty() {
        add(
            "sort",
            json!({
                "type": "array",
                "description": "At most one order; empty keeps the table's.",
                "items": {
                    "type": "object",
                    "properties": {
                        "property": {"type": "string", "enum": view.sorts},
                        "descending": {"type": "boolean"},
                    },
                    "required": ["property", "descending"],
                    "additionalProperties": false,
                },
            }),
        );
    }
    // A key, not an enum: a type reached by many relationships groups by thousands of keys,
    // each checked when the answer comes back ([`view`]).
    if !view.groups.is_empty() {
        add(
            "group_by",
            json!({
                "type": "array",
                "description": "At most one key to group the rows by; empty keeps the table's.",
                "items": {"type": "string"},
            }),
        );
    }
    if !view.columns.is_empty() {
        add(
            "columns",
            json!({
                "type": "array",
                "description": "The columns to show, in order; empty keeps the table's.",
                "items": {"type": "string", "enum": view.columns},
            }),
        );
    }
    shape
}

/// What the model is told: the type, each key with its kind, values and operators, and the
/// sentence.
pub fn input(
    type_name: &str,
    choices: &[Choice],
    view: &ViewChoices,
    today: &str,
    sentence: &str,
) -> String {
    input_with(type_name, choices, view, today, sentence, None)
}

/// [`input`], saying what groups can reach when `reach` is given: each relationship's own
/// properties, and the types one, two and three hops away with their properties and
/// relationships. The further hops go first when the input would pass the cap.
pub fn input_with(
    type_name: &str,
    choices: &[Choice],
    view: &ViewChoices,
    today: &str,
    sentence: &str,
    reach: Option<Reach>,
) -> String {
    let mut text = format!("Table: rows of type {type_name}. Today is {today}.\n\nKeys:\n");
    // The type's own properties, one line each; then each relationship on a line of its own,
    // their operators said once for all; then each related type's properties once, however
    // many relationships reach it. A wide type is reached by hundreds of relationships, and
    // repeating either for each would pass the task's cap.
    for choice in choices
        .iter()
        .filter(|c| c.property.is_some() && !c.key.contains('.'))
    {
        let property = choice.property.as_ref().expect("filtered");
        text.push_str(&format!(
            "- {}: {}; operators {}\n",
            choice.key,
            kind_of(property),
            op_keys(&choice.ops)
        ));
    }
    let relationships: Vec<&Choice> = choices.iter().filter(|c| c.property.is_none()).collect();
    let common = relationships
        .first()
        .map(|relationship| relationship.ops.clone())
        .unwrap_or_default();
    if !relationships.is_empty() {
        text.push_str(&format!(
            "\nRelationships, each with operators {}, its GQ in a predicate from the row {NODE_VAR} \
             to the related record $r, and the related type; `key.<property>` tests the related \
             record's property, with that property's operators:\n",
            op_keys(&common)
        ));
    }
    let mut related: Vec<(&str, Vec<String>)> = Vec::new();
    for relationship in relationships {
        // The GQ names the edge and the related type its end: the menu's label says no more.
        match (&relationship.traversal, &relationship.other_type) {
            (Some(traversal), Some(other)) => {
                text.push_str(&format!("- {}: `{traversal}`, {other}", relationship.key));
            }
            _ => text.push_str(&format!("- {} ({})", relationship.key, relationship.label)),
        }
        if relationship.ops != common {
            text.push_str(&format!("; operators {}", op_keys(&relationship.ops)));
        }
        if let Some(reach) = reach
            && let Some((_, edge)) = builder::parse_relationship_key(&relationship.key)
            && let Some(edge) = reach.schema.edge(edge)
        {
            let own: Vec<String> = edge
                .properties
                .iter()
                .filter(|p| !FilterOp::for_property(p).is_empty())
                .map(|p| format!("@{} {}", p.name, kind_of(p)))
                .collect();
            if !own.is_empty() {
                text.push_str(&format!("; its own properties: {}", own.join(", ")));
            }
        }
        text.push('\n');
        let Some(other) = relationship.other_type.as_deref() else {
            continue;
        };
        if related.iter().any(|(seen, _)| *seen == other) {
            continue;
        }
        let prefix = format!("{}.", relationship.key);
        let nested: Vec<String> = choices
            .iter()
            .filter(|c| c.key.starts_with(&prefix))
            .filter_map(|c| {
                let property = c.property.as_ref()?;
                Some(format!("{} {}", property.name, kind_of(property)))
            })
            .collect();
        if !nested.is_empty() {
            related.push((other, nested));
        }
    }
    // What may order, group and show: the keys are those above, so only the order's and the
    // grouping's own properties are named again.
    if !view.sorts.is_empty() {
        text.push_str(&format!(
            "\nOrder (`sort`) by one of: {}; descending for the largest, latest or last first.\n",
            view.sorts.join(", ")
        ));
    }
    if !view.groups.is_empty() {
        let own: Vec<&str> = view
            .groups
            .iter()
            .map(String::as_str)
            .filter(|key| !key.contains(':'))
            .collect();
        text.push_str(&format!(
            "Group (`group_by`) by one of: {}; a relationship key, by each related record; or a \
             relationship key, a dot and a related record's property.\n",
            own.join(", ")
        ));
    }
    if !view.columns.is_empty() {
        text.push_str(
            "Columns (`columns`): the property and relationship keys above to show, in order.\n",
        );
    }
    let mut listed = String::new();
    if !related.is_empty() {
        listed.push_str("\nProperties of related types:\n");
        for (other, nested) in related {
            listed.push_str(&format!("- {other}: {}\n", nested.join(", ")));
        }
    }
    // What groups reach past the relationships above, as far as fits: each type's
    // relationships (but not at the last hop listed, which goes no further), and the
    // properties of types two and three hops away (one hop away's are listed above).
    let hops_to = |limit: usize| -> String {
        let Some(reach) = reach else {
            return String::new();
        };
        let mut out = String::from(
            "\nGroups: a group is one relationship key from the row (parent \"\") or from a group's \
             record (parent its id); its filters name that record's property, or its edge's as \
             @name, with group set to its id. Quantifier any (default), none, every, at_least or \
             at_most with count, or sum, avg, min, max of `of` compared by `compare` with \
             `than`. Two tests on one related record go in one group.\n",
        );
        if limit < 2 {
            return out;
        }
        out.push_str("\nFurther hops, by type:\n");
        for (count, type_name, relationships) in reach.types() {
            if count >= limit {
                if count == limit {
                    // The last hop: its properties only.
                } else {
                    continue;
                }
            }
            let Some(node) = reach.schema.node(&type_name) else {
                continue;
            };
            let mut parts = Vec::new();
            if count > 1 {
                let properties: Vec<String> = node
                    .properties
                    .iter()
                    .filter(|p| !FilterOp::for_property(p).is_empty())
                    .map(|p| format!("{} {}", p.name, kind_of(p)))
                    .collect();
                parts.push(properties.join(", "));
            }
            if count < limit && !relationships.is_empty() {
                let relationships: Vec<String> = relationships
                    .iter()
                    .map(|(key, _, other)| format!("{key} {other}"))
                    .collect();
                parts.push(format!("relationships: {}", relationships.join(", ")));
            }
            if !parts.is_empty() {
                out.push_str(&format!(
                    "- {type_name} ({count} away): {}\n",
                    parts.join("; ")
                ));
            }
        }
        out
    };
    let rest = format!(
        "\nOperators: eq, ne, lt, le, gt, ge compare one value; contains matches text or a \
         list member; starts_with and search (the words, in any order) match text; in takes \
         several values; is_empty and is_not_empty, today, this_week and last_30_days take no \
         value; has_any and has_none take no value and test a relationship; a key with a dot \
         tests the related records' property, which is how to filter by a related record's \
         name or kind; related_to takes one value, the related record's name, only for one \
         exact record.\n\nGQ for `predicate`, on the row {NODE_VAR}: a property is {NODE_VAR}.<key>; compare \
         with =, !=, <, <=, >, >=, contains, starts_with, in [..]; `is null`; combine with and, \
         or, not, and parentheses. Or, as the whole predicate, one block on a related record: \
         `exists {{ $r: <Type>  <its GQ>  $r.<property> = \"x\"  $r.<other> > 3 }}` or \
         `not {{ ... }}`, with the relationship's GQ as given above, in its direction; a \
         block cannot sit inside or, and holds no count.\n\nRequest: {}",
        sentence.trim()
    );
    // The related types' properties are what a wide type cannot always fit: left out, and said
    // to be, when they would pass the task's cap, so the request is still sent.
    // The hops are a help, not the request: they take at most this much, so a well-connected
    // type does not send the whole schema with every sentence.
    const HOPS_BUDGET: usize = 12 * 1024;
    let fits = |hops: &str, listed: &str| {
        hops.len() <= HOPS_BUDGET
            && text.len() + listed.len() + hops.len() + rest.len() <= TASK.limits.input_bytes
    };
    let mut hops = (2..=MAX_HOPS)
        .rev()
        .map(&hops_to)
        .find(|hops| fits(hops, &listed))
        .unwrap_or_else(|| {
            let mut short = hops_to(1);
            if reach.is_some() {
                short.push_str(
                    "Further hops: not listed for this type; groups go one relationship from the \
                     row, and a further hop goes in `predicate`.\n",
                );
            }
            short
        });
    if !fits(&hops, &listed) && reach.is_some() {
        hops = hops_to(1);
    }
    if text.len() + listed.len() + hops.len() + rest.len() > TASK.limits.input_bytes {
        listed = "\nThe related types' properties are left out: there are too many to list.\n"
            .to_owned();
    }
    text + &listed + &hops + &rest
}

/// Operators by their keys, as the answer names them: `eq, ne, contains`.
fn op_keys(ops: &[FilterOp]) -> String {
    ops.iter().map(|op| op.key()).collect::<Vec<_>>().join(", ")
}

/// A property's kind in a few words: `String`, `list of String`, `enum(a, b, …)`.
fn kind_of(property: &Property) -> String {
    const ENUM_VALUES_SHOWN: usize = 12;
    let base = match &property.enum_values {
        Some(values) if values.len() > ENUM_VALUES_SHOWN => format!(
            "enum({}, … {} more)",
            values[..ENUM_VALUES_SHOWN].join(", "),
            values.len() - ENUM_VALUES_SHOWN
        ),
        Some(values) => format!("enum({})", values.join(", ")),
        None => property.base_type.clone(),
    };
    if property.list {
        format!("list of {base}")
    } else {
        base
    }
}

#[derive(Debug, Deserialize)]
pub struct Answer {
    pub filters: Vec<Item>,
    /// Tests on one related record (spec 025), when the schema offered them.
    #[serde(default)]
    pub groups: Vec<GroupItem>,
    /// A GQ condition for what the filters cannot say; the table judges it with the compiler.
    #[serde(default)]
    pub predicate: Option<String>,
    /// The rows' order, at most one; none keeps the table's.
    #[serde(default)]
    pub sort: Vec<SortItem>,
    /// What the rows are grouped by, at most one; none keeps the table's.
    #[serde(default)]
    pub group_by: Vec<String>,
    /// The columns to show, in order; none keeps the table's.
    #[serde(default)]
    pub columns: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct SortItem {
    pub property: String,
    pub descending: bool,
}

/// What the answer sets beside its filters, each checked against what the table offers, and
/// what it could not use, each with why. Past the first order or grouping, the rest is said.
pub fn view(answer: &Answer, offered: &ViewChoices) -> (View, Vec<String>) {
    let mut refused = Vec::new();
    let sort = answer.sort.first().and_then(|item| {
        if offered.sorts.contains(&item.property) {
            Some(Sort {
                property: item.property.clone(),
                descending: item.descending,
            })
        } else {
            refused.push(format!("the rows cannot be ordered by {}", item.property));
            None
        }
    });
    if answer.sort.len() > 1 {
        refused.push("the rows take one order".to_owned());
    }
    let group_by = answer.group_by.first().and_then(|key| {
        if offered.groups.contains(key) {
            Some(key.clone())
        } else {
            refused.push(format!("the rows cannot be grouped by {key}"));
            None
        }
    });
    if answer.group_by.len() > 1 {
        refused.push("the rows take one grouping".to_owned());
    }
    let mut columns: Vec<String> = Vec::new();
    for key in &answer.columns {
        if !offered.columns.contains(key) {
            refused.push(format!("{key} is not a column"));
        } else if !columns.contains(key) {
            columns.push(key.clone());
        }
    }
    let columns = (!columns.is_empty()).then_some(columns);
    (
        View {
            sort,
            group_by,
            columns,
        },
        refused,
    )
}

#[derive(Debug, Deserialize)]
pub struct Item {
    pub property: String,
    pub op: String,
    #[serde(default)]
    pub value: Vec<String>,
    /// The group whose related record this tests; empty for the row.
    #[serde(default)]
    pub group: String,
}

/// One group of an answer (spec 025), flat: its parent names the group it sits in.
#[derive(Debug, Deserialize, Default)]
pub struct GroupItem {
    pub id: String,
    #[serde(default)]
    pub parent: String,
    pub relationship: String,
    #[serde(default)]
    pub quantifier: String,
    #[serde(default)]
    pub count: Option<u32>,
    #[serde(default)]
    pub of: String,
    #[serde(default)]
    pub compare: String,
    #[serde(default)]
    pub than: String,
}

/// The answer as filters the table takes, and the keys it could not use, each with why.
pub fn filters(answer: Answer, choices: &[Choice]) -> (Vec<Filter>, Vec<String>) {
    filters_with(answer, choices, None)
}

/// [`filters`], with the answer's groups when `reach` is given: each built from its filters and
/// the groups inside it, checked against the schema at every level, and dropped and named when
/// the type cannot take it.
pub fn filters_with(
    answer: Answer,
    choices: &[Choice],
    reach: Option<Reach>,
) -> (Vec<Filter>, Vec<String>) {
    let mut filters = Vec::new();
    let mut refused = Vec::new();
    let (grouped, own): (Vec<Item>, Vec<Item>) = answer
        .filters
        .into_iter()
        .partition(|item| !item.group.trim().is_empty());
    match reach {
        Some(reach) => {
            let (made, why) = groups(&answer.groups, grouped, reach);
            filters.extend(made);
            refused.extend(why);
        }
        None => {
            for item in grouped {
                refused.push(format!(
                    "{} is in a group, which this view cannot take",
                    item.property
                ));
            }
        }
    }
    for item in own {
        match filter(&item, choices) {
            Ok(filter) => filters.push(filter),
            Err(why) => refused.push(why),
        }
    }
    if let Some(predicate) = answer.predicate.map(|p| p.trim().to_owned())
        && !predicate.is_empty()
    {
        // The table judges it with the compiler before it is applied.
        filters.push(Filter {
            property: gq_language::builder::WHERE_FILTER.to_owned(),
            op: FilterOp::Expr,
            value: Value::String(predicate),
        });
    }
    (filters, refused)
}

fn filter(item: &Item, choices: &[Choice]) -> Result<Filter, String> {
    let choice = choices
        .iter()
        .find(|choice| choice.key == item.property)
        .ok_or_else(|| format!("{} is not a key of this type", item.property))?;
    let op = FilterOp::from_key(&item.op)
        .filter(|op| choice.ops.contains(op))
        .ok_or_else(|| format!("{} cannot be tested with {}", item.property, item.op))?;
    let value = if !op.takes_value() {
        Value::Null
    } else {
        let first = item
            .value
            .first()
            .ok_or_else(|| format!("{} {} needs a value", item.property, item.op))?;
        match (&choice.property, op) {
            (Some(property), FilterOp::In) => Value::Array(
                item.value
                    .iter()
                    .map(|text| parse_filter_value(property, text))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|why| format!("{}: {why}", item.property))?,
            ),
            (Some(property), _) => parse_filter_value(property, first)
                .map_err(|why| format!("{}: {why}", item.property))?,
            (None, _) => Value::String(first.clone()),
        }
    };
    Ok(Filter {
        property: choice.key.clone(),
        op,
        value,
    })
}

/// The answer's groups as filters: each top group (parent empty) a [`FilterOp::Where`] filter
/// (or the neighbour-property form it equals), holding its filters and the groups whose parent
/// it is. What the schema refuses is named; a group whose parent is missing, or that sits too
/// deep, is dropped with its filters.
fn groups(items: &[GroupItem], filters: Vec<Item>, reach: Reach) -> (Vec<Filter>, Vec<String>) {
    let mut refused = Vec::new();
    let mut by_group: Vec<(String, Vec<Item>)> = Vec::new();
    for item in filters {
        match by_group.iter_mut().find(|(id, _)| *id == item.group) {
            Some((_, items)) => items.push(item),
            None => by_group.push((item.group.clone(), vec![item])),
        }
    }
    let known = |id: &str| items.iter().any(|g| g.id == id);
    for (id, group_items) in &by_group {
        if !known(id) {
            for item in group_items {
                refused.push(format!("{} names no group {id}", item.property));
            }
        }
    }
    let mut out = Vec::new();
    for top in items.iter().filter(|g| g.parent.trim().is_empty()) {
        if let Some(group) = build_group(
            top,
            items,
            &by_group,
            reach,
            reach.type_name,
            1,
            &mut refused,
        ) {
            {
                let filter = builder::group_filter(top.relationship.trim(), &group);
                let built = builder::node_page(
                    reach.schema,
                    reach.type_name,
                    std::slice::from_ref(&filter),
                    None,
                    &PageStart::First,
                    1,
                );
                match built {
                    Ok(_) => out.push(filter),
                    Err(why) => refused.push(format!("{}: {why}", top.relationship)),
                }
            }
        }
    }
    for orphan in items
        .iter()
        .filter(|g| !g.parent.trim().is_empty() && !items.iter().any(|p| p.id == g.parent))
    {
        refused.push(format!(
            "{} sits in no group {}",
            orphan.relationship, orphan.parent
        ));
    }
    (out, refused)
}

/// One group of the answer on a record of `near_type`, `depth` hops from the row.
fn build_group(
    item: &GroupItem,
    items: &[GroupItem],
    by_group: &[(String, Vec<Item>)],
    reach: Reach,
    near_type: &str,
    depth: usize,
    refused: &mut Vec<String>,
) -> Option<Group> {
    let key = item.relationship.trim();
    if depth > MAX_HOPS {
        refused.push(format!("{key} is more than {MAX_HOPS} relationships away"));
        return None;
    }
    let Some((node, edge)) = builder::path_end(reach.schema, near_type, &[key]) else {
        refused.push(format!("{near_type} has no relationship {key}"));
        return None;
    };
    let edge = edge?;
    let property_of = |name: &str| -> Option<gq_language::Property> {
        match name.strip_prefix('@') {
            Some(own) if builder::is_edge_property_key(name) => {
                edge.properties.iter().find(|p| p.name == own).cloned()
            }
            _ => node.property(name).cloned(),
        }
    };
    let mut tests = Vec::new();
    let mine = by_group
        .iter()
        .find(|(id, _)| *id == item.id)
        .map(|(_, items)| items.as_slice())
        .unwrap_or_default();
    for filter in mine {
        let name = filter.property.trim();
        let op = FilterOp::from_key(&filter.op);
        if name == builder::ID_FILTER {
            match (op, filter.value.first()) {
                (Some(FilterOp::Eq), Some(id)) => tests.push(GroupTest {
                    property: name.to_owned(),
                    op: FilterOp::Eq,
                    value: Value::String(id.clone()),
                }),
                _ => refused.push(format!("{key} @id takes eq and one id")),
            }
            continue;
        }
        let Some(property) = property_of(name) else {
            refused.push(format!("{} has no property {name}", node.name));
            continue;
        };
        let Some(op) = op.filter(|op| FilterOp::for_property(&property).contains(op)) else {
            refused.push(format!("{key} {name} cannot be tested with {}", filter.op));
            continue;
        };
        let value = if !op.takes_value() {
            Value::Null
        } else if op == FilterOp::In {
            match filter
                .value
                .iter()
                .map(|text| parse_filter_value(&property, text))
                .collect::<Result<Vec<_>, _>>()
            {
                Ok(values) => Value::Array(values),
                Err(why) => {
                    refused.push(format!("{key} {name}: {why}"));
                    continue;
                }
            }
        } else {
            match filter
                .value
                .first()
                .map(|text| parse_filter_value(&property, text))
            {
                Some(Ok(value)) => value,
                Some(Err(why)) => {
                    refused.push(format!("{key} {name}: {why}"));
                    continue;
                }
                None => {
                    refused.push(format!("{key} {name} {} needs a value", filter.op));
                    continue;
                }
            }
        };
        tests.push(GroupTest {
            property: name.to_owned(),
            op,
            value,
        });
    }
    for inner in items
        .iter()
        .filter(|g| g.parent == item.id && !g.id.is_empty() && g.id != item.id)
    {
        if let Some(group) = build_group(
            inner,
            items,
            by_group,
            reach,
            &node.name,
            depth + 1,
            refused,
        ) {
            let nested = builder::group_filter(inner.relationship.trim(), &group);
            tests.push(GroupTest::from(&nested));
        }
    }
    let quantifier = match item.quantifier.trim() {
        "" => Quantifier::Any,
        key => match Quantifier::from_key(key) {
            Some(quantifier) => quantifier,
            None => {
                refused.push(format!("{key} is not a quantifier"));
                return None;
            }
        },
    };
    let mut group = Group {
        quantifier,
        tests,
        ..Group::default()
    };
    if quantifier.counts() {
        let Some(count) = item.count else {
            refused.push(format!("{key} {} needs a count", quantifier.key()));
            return None;
        };
        group.count = Some(count);
    }
    if quantifier.aggregates() {
        let of = item.of.trim();
        let property = property_of(of).filter(|p| quantifier.takes(p));
        let compare = FilterOp::from_key(item.compare.trim()).filter(|op| {
            matches!(
                op,
                FilterOp::Eq
                    | FilterOp::Ne
                    | FilterOp::Lt
                    | FilterOp::Le
                    | FilterOp::Gt
                    | FilterOp::Ge
            )
        });
        let (Some(mut property), Some(compare)) = (property, compare) else {
            refused.push(format!(
                "{key}: the {} of {of} cannot be compared so",
                quantifier.key()
            ));
            return None;
        };
        if quantifier == Quantifier::Avg {
            property.kind = gq_language::schema::ScalarKind::Float;
        }
        let than = match parse_filter_value(&property, &item.than) {
            Ok(than) => than,
            Err(why) => {
                refused.push(format!("{key} {of}: {why}"));
                return None;
            }
        };
        group.of = Some(of.to_owned());
        group.compare = Some(compare);
        group.than = Some(than);
    }
    Some(group)
}

/// A filter's value as typed for `property`: one element, parsed as its kind; an empty text is
/// refused.
pub fn parse_filter_value(property: &Property, text: &str) -> Result<Value, String> {
    let element = Property {
        list: false,
        nullable: false,
        ..property.clone()
    };
    if text.trim().is_empty() {
        return Err("Enter a value".into());
    }
    parse_scalar(&element, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gq_language::schema::ScalarKind;

    fn property(name: &str, kind: ScalarKind, base: &str, values: Option<&[&str]>) -> Property {
        Property {
            name: name.into(),
            kind,
            base_type: base.into(),
            nullable: true,
            list: false,
            enum_values: values.map(|values| values.iter().map(|v| (*v).to_owned()).collect()),
            is_key: false,
            full_text: false,
        }
    }

    fn choices() -> Vec<Choice> {
        let kind = property(
            "kind",
            ScalarKind::Text,
            "String",
            Some(&["dinner", "trip"]),
        );
        let date = property("date", ScalarKind::DateTime, "DateTime", None);
        let mut out: Vec<Choice> = [kind, date]
            .into_iter()
            .map(|property| Choice {
                key: property.name.clone(),
                label: property.name.clone(),
                ops: FilterOp::for_property(&property),
                property: Some(property),
                other_type: None,
                traversal: None,
            })
            .collect();
        out.push(Choice {
            key: "out:Attended".into(),
            label: "attended Person".into(),
            ops: FilterOp::for_relationship(),
            property: None,
            other_type: Some("Person".into()),
            traversal: Some("$n attended $r".into()),
        });
        out
    }

    /// People, their companies with the edge's own properties, and the companies' cities.
    const HOPS: &str = "node Person {\n  slug: String @key\n  name: String\n}\n\
        node Company {\n  slug: String @key\n  city: String?\n  employees: I32?\n}\n\
        node City {\n  slug: String @key\n  country: String?\n}\n\
        edge WorksAt: Person -> Company {\n  role: String?\n  hours: I32?\n}\n\
        edge LocatedIn: Company -> City\n";

    fn group_item(id: &str, parent: &str, relationship: &str, quantifier: &str) -> GroupItem {
        GroupItem {
            id: id.into(),
            parent: parent.into(),
            relationship: relationship.into(),
            quantifier: quantifier.into(),
            ..GroupItem::default()
        }
    }

    fn in_group(group: &str, property: &str, op: &str, value: &str) -> Item {
        Item {
            property: property.into(),
            op: op.into(),
            value: if value.is_empty() {
                Vec::new()
            } else {
                vec![value.into()]
            },
            group: group.into(),
        }
    }

    /// With the schema offered, an answer's groups become filters on one related record: two
    /// tests on one company, the edge's own property, a further hop, a count and a total; what
    /// the schema cannot take is named and left out (spec 025).
    #[test]
    fn an_answers_groups_test_one_related_record() {
        let model = gq_language::SchemaModel::parse(HOPS).unwrap();
        let reach = Reach {
            schema: &model,
            type_name: "Person",
        };
        let shape = schema_with(&[], &ViewChoices::default(), Some(reach));
        let names = &shape["properties"]["filters"]["items"]["properties"]["property"]["enum"];
        for name in ["city", "employees", "country", "@role", "@hours"] {
            assert!(
                names.as_array().unwrap().contains(&json!(name)),
                "{name}: {names}"
            );
        }
        assert!(
            shape["required"]
                .as_array()
                .unwrap()
                .contains(&json!("groups"))
        );
        assert_eq!(
            schema(&[], &ViewChoices::default())["properties"]["groups"],
            Value::Null,
            "no groups without the schema"
        );
        let input = input_with(
            "Person",
            &[],
            &ViewChoices::default(),
            "2026-11-08",
            "x",
            Some(reach),
        );
        assert!(input.contains("- Company (1 away)"), "{input}");
        assert!(input.contains("- City (2 away)"), "{input}");
        assert!(
            input.contains("relationships: out:LocatedIn City"),
            "{input}"
        );

        let mut total = group_item("g3", "", "out:WorksAt", "sum");
        total.of = "@hours".into();
        total.compare = "gt".into();
        total.than = "100".into();
        let mut two = group_item("g4", "", "out:WorksAt", "at_least");
        two.count = Some(2);
        let answer = Answer {
            filters: vec![
                in_group("g1", "city", "eq", "Berlin"),
                in_group("g1", "employees", "gt", "100"),
                in_group("g1", "@role", "eq", "CTO"),
                in_group("g2", "country", "eq", "Germany"),
                in_group("g4", "city", "eq", "Paris"),
                in_group("g1", "mood", "eq", "good"),
                in_group("g9", "city", "eq", "Rome"),
            ],
            groups: vec![
                group_item("g1", "", "out:WorksAt", "any"),
                group_item("g2", "g1", "out:LocatedIn", ""),
                total,
                two,
                group_item("g5", "", "out:Likes", "any"),
                group_item("g6", "", "out:WorksAt", "at_most"),
            ],
            predicate: None,
            sort: Vec::new(),
            group_by: Vec::new(),
            columns: Vec::new(),
        };
        let (made, refused) = filters_with(answer, &[], Some(reach));
        assert_eq!(made.len(), 3, "{made:?} {refused:?}");
        let (key, first) = builder::as_group(&made[0]).unwrap();
        assert_eq!(key, "out:WorksAt");
        let tested: Vec<&str> = first.tests.iter().map(|t| t.property.as_str()).collect();
        assert_eq!(
            tested,
            ["city", "employees", "@role", "out:LocatedIn.country"]
        );
        let (_, sum) = builder::as_group(&made[1]).unwrap();
        assert_eq!(sum.quantifier, Quantifier::Sum);
        assert_eq!(sum.than, Some(json!(100)));
        let (_, at_least) = builder::as_group(&made[2]).unwrap();
        assert_eq!(at_least.count, Some(2));
        assert_eq!(
            refused,
            [
                "city names no group g9",
                "Company has no property mood",
                "Person has no relationship out:Likes",
                "out:WorksAt at_most needs a count",
            ]
        );
        // Without the schema, a grouped filter is named, not kept.
        let (made, refused) = filters_with(
            Answer {
                filters: vec![in_group("g1", "city", "eq", "Berlin")],
                groups: vec![group_item("g1", "", "out:WorksAt", "any")],
                predicate: None,
                sort: Vec::new(),
                group_by: Vec::new(),
                columns: Vec::new(),
            },
            &[],
            None,
        );
        assert!(made.is_empty());
        assert_eq!(refused.len(), 1);
    }

    /// The schema allows only the keys and operators offered, and the answer's filters are
    /// parsed as typed ones would be; one the type cannot take is named, not kept.
    #[test]
    fn an_answer_becomes_filters_the_type_takes() {
        let choices = choices();
        let schema = schema(&choices, &ViewChoices::default());
        assert_eq!(
            schema["properties"]["filters"]["items"]["properties"]["property"]["enum"],
            json!(["kind", "date", "out:Attended"])
        );
        let answer = Answer {
            groups: Vec::new(),
            predicate: Some(" $n.kind = \"dinner\" or $n.kind = \"party\" ".into()),
            sort: Vec::new(),
            group_by: Vec::new(),
            columns: Vec::new(),
            filters: vec![
                Item {
                    property: "kind".into(),
                    op: "in".into(),
                    value: vec!["dinner".into(), "trip".into()],
                    group: String::new(),
                },
                Item {
                    property: "date".into(),
                    op: "last_30_days".into(),
                    value: vec![],
                    group: String::new(),
                },
                Item {
                    property: "out:Attended".into(),
                    op: "has_any".into(),
                    value: vec![],
                    group: String::new(),
                },
                Item {
                    property: "mood".into(),
                    op: "eq".into(),
                    value: vec!["good".into()],
                    group: String::new(),
                },
                Item {
                    property: "kind".into(),
                    op: "gt".into(),
                    value: vec!["dinner".into()],
                    group: String::new(),
                },
            ],
        };
        let (filters, refused) = filters(answer, &choices);
        assert_eq!(filters.len(), 4);
        assert_eq!(filters[3].op, FilterOp::Expr);
        assert_eq!(
            filters[3].value,
            json!("$n.kind = \"dinner\" or $n.kind = \"party\"")
        );
        assert_eq!(filters[0].op, FilterOp::In);
        assert_eq!(filters[0].value, json!(["dinner", "trip"]));
        assert_eq!(filters[1].op, FilterOp::Last30Days);
        assert_eq!(filters[1].value, Value::Null);
        assert_eq!(filters[2].property, "out:Attended");
        assert_eq!(
            refused,
            [
                "mood is not a key of this type",
                "kind cannot be tested with gt"
            ]
        );
        assert!(
            INSTRUCTIONS.contains(&format!("the row variable {NODE_VAR} ")),
            "the instructions name the row the queries bind"
        );
        let text = input(
            "Event",
            &choices,
            &ViewChoices::default(),
            "2026-10-29",
            " dinners last month ",
        );
        assert!(
            text.contains("- kind: enum(dinner, trip); operators eq"),
            "{text}"
        );
        assert!(
            text.contains(
                "Relationships, each with operators has_any, has_none, related_to, its GQ"
            ),
            "{text}"
        );
        assert!(
            text.contains("- out:Attended: `$n attended $r`, Person\n"),
            "{text}"
        );
        assert!(text.ends_with("Request: dinners last month"));
    }

    /// A sentence may set the order, the grouping and the columns beside its filters, each
    /// checked against what the table offers: one it cannot take is named, not kept, and none
    /// keeps the table's own. The schema offers each the table has choices for.
    #[test]
    fn an_answer_sets_the_order_grouping_and_columns_the_table_offers() {
        let offered = ViewChoices {
            sorts: vec!["date".into(), "kind".into()],
            groups: vec![
                "kind".into(),
                "out:Attended".into(),
                "out:Attended.role".into(),
            ],
            columns: vec!["kind".into(), "date".into(), "out:Attended".into()],
        };
        let answer: Answer = serde_json::from_value(json!({
            "filters": [],
            "predicate": null,
            "sort": [{"property": "date", "descending": true}],
            "group_by": ["out:Attended.role"],
            "columns": ["date", "kind", "date", "nope"],
        }))
        .unwrap();
        let (set, refused) = view(&answer, &offered);
        assert_eq!(
            set,
            View {
                sort: Some(Sort {
                    property: "date".into(),
                    descending: true,
                }),
                group_by: Some("out:Attended.role".into()),
                columns: Some(vec!["date".into(), "kind".into()]),
            }
        );
        assert_eq!(refused, ["nope is not a column"]);

        let answer: Answer = serde_json::from_value(json!({
            "filters": [],
            "predicate": null,
            "sort": [{"property": "name", "descending": false}],
        }))
        .unwrap();
        let (set, refused) = view(&answer, &offered);
        assert_eq!(set, View::default(), "none taken keeps the table's");
        assert_eq!(refused, ["the rows cannot be ordered by name"]);

        let shape = schema(&choices(), &offered);
        assert_eq!(
            shape["properties"]["sort"]["items"]["properties"]["property"]["enum"],
            json!(["date", "kind"])
        );
        assert_eq!(
            shape["properties"]["columns"]["items"]["enum"],
            json!(["kind", "date", "out:Attended"])
        );
        assert_eq!(
            shape["required"],
            json!(["filters", "predicate", "sort", "group_by", "columns"])
        );
        let bare = schema(&choices(), &ViewChoices::default());
        assert_eq!(bare["required"], json!(["filters", "predicate"]));
        assert!(bare["properties"].get("sort").is_none());

        let text = input(
            "Event",
            &choices(),
            &offered,
            "2026-10-29",
            "latest dinners by role",
        );
        assert!(
            text.contains("Order (`sort`) by one of: date, kind;"),
            "{text}"
        );
        assert!(
            text.contains("Group (`group_by`) by one of: kind;"),
            "{text}"
        );
        assert!(text.contains("Columns (`columns`)"), "{text}");
    }

    /// A related type's properties are said once, however many relationships reach it, and a
    /// type too wide for the task's cap leaves them out, says so, and is still asked. Before,
    /// every relationship repeated its operators and the related type's properties, so the
    /// personal graph's widest types (1,122 keys for Person) passed the cap and the request
    /// was refused.
    #[test]
    fn the_input_says_each_related_type_once_and_fits_the_cap() {
        let relationship = |key: &str, other: &str| Choice {
            key: key.into(),
            label: format!("{key} {other}"),
            ops: FilterOp::for_relationship(),
            property: None,
            other_type: Some(other.into()),
            traversal: gq_language::builder::relationship_traversal(key, "$n", "$r"),
        };
        let nested = |key: &str, name: &str, values: &[&str]| {
            let property = property(name, ScalarKind::Text, "String", Some(values));
            Choice {
                key: format!("{key}.{name}"),
                label: name.into(),
                ops: FilterOp::for_property(&property),
                property: Some(property),
                other_type: None,
                traversal: None,
            }
        };
        let mut choices = choices();
        for key in ["out:Attended", "in:Invited"] {
            if key != "out:Attended" {
                choices.push(relationship(key, "Person"));
            }
            choices.push(nested(key, "role", &["host", "guest"]));
        }
        let text = input(
            "Event",
            &choices,
            &ViewChoices::default(),
            "2026-10-29",
            "dinners Ada hosted",
        );
        assert_eq!(
            text.matches("- Person: role enum(host, guest)\n").count(),
            1,
            "{text}"
        );
        assert!(
            text.contains("- in:Invited: `$r invited $n`, Person\n"),
            "{text}"
        );

        let values: Vec<String> = (0..12).map(|n| format!("value-{n:02}")).collect();
        let values: Vec<&str> = values.iter().map(String::as_str).collect();
        let mut wide = choices.clone();
        for n in 0..400 {
            let key = format!("out:Rel{n:03}");
            wide.push(relationship(&key, &format!("Type{n:03}")));
            wide.push(nested(&key, "status", &values));
        }
        let text = input(
            "Event",
            &wide,
            &ViewChoices::default(),
            "2026-10-29",
            "dinners Ada hosted",
        );
        assert!(
            text.len() <= TASK.limits.input_bytes,
            "{} bytes",
            text.len()
        );
        assert!(
            text.contains("related types' properties are left out"),
            "{text}"
        );
        assert!(!text.contains("- Type000: status"), "{text}");
        assert!(text.contains("- out:Rel399: `$n rel399 $r`, Type399\n"));
        assert!(text.ends_with("Request: dinners Ada hosted"));
    }
}

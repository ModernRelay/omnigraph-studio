//! Every query Studio generates, in one place (spec 004, 005).
//!
//! Each builder returns GQ text and its JSON parameters. Values always travel as parameters,
//! never spliced into the text. The tests type-check every builder's output with
//! `omnigraph-compiler` against the dev schema.

use serde_json::{Map, Value};

use crate::schema::{
    Direction, EdgeType, NodeType, Property, RelationshipGroup, ScalarKind, SchemaModel,
    traversal_name,
};

/// A generated query: GQ source and the parameters to send with it.
#[derive(Debug, Clone, PartialEq)]
pub struct BuiltQuery {
    pub name: String,
    pub text: String,
    pub params: Map<String, Value>,
}

impl BuiltQuery {
    pub fn params_value(&self) -> Option<Value> {
        if self.params.is_empty() {
            None
        } else {
            Some(Value::Object(self.params.clone()))
        }
    }
}

/// A comparison on one property, or a test on one relationship (spec 004).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Contains,
    /// Membership in a list of values (`$n.p in $f`).
    In,
    IsEmpty,
    IsNotEmpty,
    /// Date presets: on or after the start of today, of this week (Monday), or of the day 30
    /// days ago. The table fills the value with that start each time it reads.
    Today,
    ThisWeek,
    Last30Days,
    /// Relationship tests; the filter's property is a [`relationship_key`].
    HasAny,
    HasNone,
    /// Joined to the node whose id is the value.
    RelatedTo,
}

impl FilterOp {
    /// Every operator, for [`FilterOp::from_key`].
    const ALL: [FilterOp; 16] = [
        FilterOp::Eq,
        FilterOp::Ne,
        FilterOp::Lt,
        FilterOp::Le,
        FilterOp::Gt,
        FilterOp::Ge,
        FilterOp::Contains,
        FilterOp::In,
        FilterOp::IsEmpty,
        FilterOp::IsNotEmpty,
        FilterOp::Today,
        FilterOp::ThisWeek,
        FilterOp::Last30Days,
        FilterOp::HasAny,
        FilterOp::HasNone,
        FilterOp::RelatedTo,
    ];

    /// A stable name, for saved views.
    pub fn key(self) -> &'static str {
        match self {
            FilterOp::Eq => "eq",
            FilterOp::Ne => "ne",
            FilterOp::Lt => "lt",
            FilterOp::Le => "le",
            FilterOp::Gt => "gt",
            FilterOp::Ge => "ge",
            FilterOp::Contains => "contains",
            FilterOp::In => "in",
            FilterOp::IsEmpty => "is_empty",
            FilterOp::IsNotEmpty => "is_not_empty",
            FilterOp::Today => "today",
            FilterOp::ThisWeek => "this_week",
            FilterOp::Last30Days => "last_30_days",
            FilterOp::HasAny => "has_any",
            FilterOp::HasNone => "has_none",
            FilterOp::RelatedTo => "related_to",
        }
    }

    pub fn from_key(key: &str) -> Option<FilterOp> {
        Self::ALL.into_iter().find(|op| op.key() == key)
    }

    pub fn label(self) -> &'static str {
        match self {
            FilterOp::Eq => "is",
            FilterOp::Ne => "is not",
            FilterOp::Lt => "less than",
            FilterOp::Le => "at most",
            FilterOp::Gt => "greater than",
            FilterOp::Ge => "at least",
            FilterOp::Contains => "contains",
            FilterOp::In => "is one of",
            FilterOp::IsEmpty => "is empty",
            FilterOp::IsNotEmpty => "is not empty",
            FilterOp::Today => "is today",
            FilterOp::ThisWeek => "is this week",
            FilterOp::Last30Days => "is in the last 30 days",
            FilterOp::HasAny => "has any",
            FilterOp::HasNone => "has none",
            FilterOp::RelatedTo => "is related to",
        }
    }

    /// Whether the person types a value: presets compute theirs, and emptiness and
    /// relationship presence need none.
    pub fn takes_value(self) -> bool {
        !matches!(
            self,
            FilterOp::IsEmpty
                | FilterOp::IsNotEmpty
                | FilterOp::Today
                | FilterOp::ThisWeek
                | FilterOp::Last30Days
                | FilterOp::HasAny
                | FilterOp::HasNone
        )
    }

    pub fn is_date_preset(self) -> bool {
        matches!(
            self,
            FilterOp::Today | FilterOp::ThisWeek | FilterOp::Last30Days
        )
    }

    pub fn is_relationship(self) -> bool {
        matches!(
            self,
            FilterOp::HasAny | FilterOp::HasNone | FilterOp::RelatedTo
        )
    }

    /// The tests a relationship supports, in menu order.
    pub fn for_relationship() -> Vec<FilterOp> {
        vec![FilterOp::HasAny, FilterOp::HasNone, FilterOp::RelatedTo]
    }

    /// The comparisons a property supports, in menu order. Lists take membership only (the
    /// engine refuses list equality); `is empty` needs a nullable property.
    pub fn for_property(property: &Property) -> Vec<FilterOp> {
        let mut ops = if property.list {
            vec![FilterOp::Contains]
        } else {
            match property.kind {
                ScalarKind::Text if property.enum_values.is_none() => {
                    vec![FilterOp::Eq, FilterOp::Ne, FilterOp::Contains, FilterOp::In]
                }
                ScalarKind::Text => vec![FilterOp::Eq, FilterOp::Ne, FilterOp::In],
                ScalarKind::Bool => vec![FilterOp::Eq, FilterOp::Ne],
                ScalarKind::Integer | ScalarKind::Float => {
                    vec![
                        FilterOp::Eq,
                        FilterOp::Ne,
                        FilterOp::Lt,
                        FilterOp::Le,
                        FilterOp::Gt,
                        FilterOp::Ge,
                        FilterOp::In,
                    ]
                }
                ScalarKind::Date | ScalarKind::DateTime => {
                    vec![
                        FilterOp::Today,
                        FilterOp::ThisWeek,
                        FilterOp::Last30Days,
                        FilterOp::Eq,
                        FilterOp::Ne,
                        FilterOp::Lt,
                        FilterOp::Le,
                        FilterOp::Gt,
                        FilterOp::Ge,
                    ]
                }
                ScalarKind::Vector | ScalarKind::Blob => Vec::new(),
            }
        };
        if property.nullable && property.projectable() {
            ops.extend([FilterOp::IsEmpty, FilterOp::IsNotEmpty]);
        }
        ops
    }
}

/// The filter key that matches a node by its `@id`.
pub const ID_FILTER: &str = "@id";

/// The filter key of a relationship group: `out:WorksAt`, `in:Knows`.
pub fn relationship_key(group: &RelationshipGroup) -> String {
    let direction = match group.direction {
        Direction::Outgoing => "out",
        Direction::Incoming => "in",
    };
    format!("{direction}:{}", group.edge)
}

/// The direction and edge of a [`relationship_key`]. A key with a property after the edge
/// ([`neighbour_property_key`]) is not one.
pub fn parse_relationship_key(key: &str) -> Option<(Direction, &str)> {
    let (direction, edge) = key.split_once(':')?;
    if edge.contains('.') {
        return None;
    }
    let direction = match direction {
        "out" => Direction::Outgoing,
        "in" => Direction::Incoming,
        _ => return None,
    };
    Some((direction, edge))
}

/// One end of an edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeEnd {
    From,
    To,
}

/// What a grouping or a section's filter names (spec 004): the row's own property, a node's
/// neighbours through a relationship, a property of those neighbours, or a property of an
/// edge's endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupKey<'a> {
    /// `title`.
    Property(&'a str),
    /// `out:WorksAt`.
    Relationship(Direction, &'a str),
    /// `out:WorksAt.city`: "people by the city of their company".
    NeighbourProperty {
        direction: Direction,
        edge: &'a str,
        property: &'a str,
    },
    /// `from.title`, `to.city`: an edge table by a property of one of its ends.
    EndpointProperty { end: EdgeEnd, property: &'a str },
}

impl GroupKey<'_> {
    /// Whether a row's own values hold the key's value, so a row can be placed and checked
    /// against a section without reading again.
    pub fn is_own_property(&self) -> bool {
        matches!(self, GroupKey::Property(_))
    }
}

/// What `key` names. Property names hold no `.` or `:`, so the forms do not overlap.
pub fn parse_group_key(key: &str) -> GroupKey<'_> {
    if let Some(property) = key.strip_prefix("from.") {
        return GroupKey::EndpointProperty {
            end: EdgeEnd::From,
            property,
        };
    }
    if let Some(property) = key.strip_prefix("to.") {
        return GroupKey::EndpointProperty {
            end: EdgeEnd::To,
            property,
        };
    }
    if let Some((relationship, property)) = key.split_once('.')
        && let Some((direction, edge)) = parse_relationship_key(relationship)
    {
        return GroupKey::NeighbourProperty {
            direction,
            edge,
            property,
        };
    }
    match parse_relationship_key(key) {
        Some((direction, edge)) => GroupKey::Relationship(direction, edge),
        None => GroupKey::Property(key),
    }
}

/// The key of a property of a node's neighbours through `group`: `out:WorksAt.city`.
pub fn neighbour_property_key(group: &RelationshipGroup, property: &str) -> String {
    format!("{}.{property}", relationship_key(group))
}

/// The key of a property of an edge's endpoint: `from.title`, `to.city`.
pub fn endpoint_property_key(end: EdgeEnd, property: &str) -> String {
    match end {
        EdgeEnd::From => format!("from.{property}"),
        EdgeEnd::To => format!("to.{property}"),
    }
}

/// How a relationship key reads: `worksAt →`, `← knows`.
pub fn relationship_label(key: &str) -> Option<String> {
    let (direction, edge) = parse_relationship_key(key)?;
    let name = traversal_name(edge);
    Some(match direction {
        Direction::Outgoing => format!("{name} →"),
        Direction::Incoming => format!("← {name}"),
    })
}

/// A table's order: one property, then `@id` (spec 004). Rows without a value come last in
/// either direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sort {
    pub property: String,
    pub descending: bool,
}

impl Sort {
    /// Whether a property can order a table: a single scalar a query returns.
    pub fn allowed(property: &Property) -> bool {
        property.projectable() && !property.list
    }
}

/// Where a page starts. Without a sort, pages follow `@id`. With one, the rows that have a
/// value come first in the sort's order, keyset-paged by (value, `@id`); then the rows without
/// one, by `@id`. The engine puts nulls first when ascending, so the two runs are separate
/// reads rather than one order.
#[derive(Debug, Clone, PartialEq)]
pub enum PageStart {
    First,
    /// After the row with this `@id` and, with a sort, this value.
    After {
        id: String,
        value: Value,
    },
    /// With a sort: the rows without a value, after this `@id` (`""` from their start).
    Nulls {
        after: String,
    },
}

/// The match clauses and order of one keyset page over binding `var`.
fn keyset(
    var: &str,
    type_name: &str,
    properties: &[Property],
    sort: Option<&Sort>,
    start: &PageStart,
    params: &mut Params,
) -> Result<(Vec<String>, String), BuildError> {
    let by_id = format!("${var}.@id asc");
    let Some(sort) = sort else {
        let after = match start {
            PageStart::After { id, .. } | PageStart::Nulls { after: id } => id.clone(),
            PageStart::First => String::new(),
        };
        let after = params.add("after", "String", Value::String(after));
        return Ok((vec![format!("${var}.@id > {after}")], by_id));
    };
    let property = properties
        .iter()
        .find(|p| p.name == sort.property && Sort::allowed(p))
        .ok_or_else(|| BuildError::UnknownProperty {
            type_name: type_name.to_owned(),
            property: sort.property.clone(),
        })?;
    let field = format!("${var}.{}", property.name);
    let ordered = format!(
        "{field} {}, {by_id}",
        if sort.descending { "desc" } else { "asc" }
    );
    Ok(match start {
        PageStart::First => (vec![format!("{field} is not null")], ordered),
        PageStart::After { id, value } if !value.is_null() => {
            let v = params.add("after_value", &property.param_type(false), value.clone());
            let after = params.add("after", "String", Value::String(id.clone()));
            let beyond = if sort.descending { "<" } else { ">" };
            (
                vec![
                    format!("{field} is not null"),
                    format!("{field} {beyond} {v} or ({field} = {v} and ${var}.@id > {after})"),
                ],
                ordered,
            )
        }
        PageStart::After { id: after, .. } | PageStart::Nulls { after } => {
            let after = params.add("after", "String", Value::String(after.clone()));
            (
                vec![format!("{field} is null"), format!("${var}.@id > {after}")],
                by_id,
            )
        }
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    pub property: String,
    pub op: FilterOp,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    UnknownType(String),
    UnknownProperty { type_name: String, property: String },
    UnknownEdge(String),
    NotEditable(String),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildError::UnknownType(name) => write!(f, "The schema has no type {name}"),
            BuildError::UnknownProperty {
                type_name,
                property,
            } => write!(f, "{type_name} has no property {property}"),
            BuildError::UnknownEdge(name) => write!(f, "The schema has no relationship {name}"),
            BuildError::NotEditable(name) => write!(f, "{name} cannot be edited in a cell"),
        }
    }
}

impl std::error::Error for BuildError {}

fn node<'a>(schema: &'a SchemaModel, type_name: &str) -> Result<&'a NodeType, BuildError> {
    schema
        .node(type_name)
        .ok_or_else(|| BuildError::UnknownType(type_name.to_owned()))
}

fn property<'a>(node: &'a NodeType, name: &str) -> Result<&'a Property, BuildError> {
    node.property(name)
        .ok_or_else(|| BuildError::UnknownProperty {
            type_name: node.name.clone(),
            property: name.to_owned(),
        })
}

struct Params {
    decls: Vec<String>,
    values: Map<String, Value>,
}

impl Params {
    fn new() -> Self {
        Self {
            decls: Vec::new(),
            values: Map::new(),
        }
    }

    fn add(&mut self, name: &str, ty: &str, value: Value) -> String {
        self.decls.push(format!("${name}: {ty}"));
        self.values.insert(name.to_owned(), value);
        format!("${name}")
    }

    fn signature(&self) -> String {
        self.decls.join(", ")
    }
}

fn filter_clauses(
    schema: &SchemaModel,
    type_name: &str,
    properties: &[Property],
    var: &str,
    filters: &[Filter],
    params: &mut Params,
) -> Result<Vec<String>, BuildError> {
    let mut clauses = Vec::new();
    for (i, filter) in filters.iter().enumerate() {
        if filter.op.is_relationship() {
            clauses.push(relationship_clause(schema, var, i, filter, params)?);
            continue;
        }
        match parse_group_key(&filter.property) {
            GroupKey::NeighbourProperty {
                direction,
                edge,
                property,
            } => {
                clauses.push(neighbour_property_clause(
                    schema, var, i, filter, direction, edge, property, params,
                )?);
                continue;
            }
            // An edge's ends are `$a` and `$b` in every edge query.
            GroupKey::EndpointProperty { end, property } => {
                let edge = edge(schema, type_name)?;
                let (end_var, end_type) = match end {
                    EdgeEnd::From => ("$a", &edge.from),
                    EdgeEnd::To => ("$b", &edge.to),
                };
                let node = node(schema, end_type)?;
                let property = node
                    .properties
                    .iter()
                    .find(|p| p.name == property)
                    .ok_or_else(|| BuildError::UnknownProperty {
                        type_name: node.name.clone(),
                        property: property.to_owned(),
                    })?;
                clauses.push(property_clause(
                    &format!("{end_var}.{}", property.name),
                    property,
                    filter,
                    i,
                    params,
                ));
                continue;
            }
            GroupKey::Property(_) | GroupKey::Relationship(..) => {}
        }
        // The node's own id, as a filter to one row (spec 005: a table opened from the tree).
        if filter.property == ID_FILTER {
            let id = params.add(&format!("f{i}"), "String", filter.value.clone());
            clauses.push(format!("${var}.@id = {id}"));
            continue;
        }
        let property = properties
            .iter()
            .find(|p| p.name == filter.property)
            .ok_or_else(|| BuildError::UnknownProperty {
                type_name: type_name.to_owned(),
                property: filter.property.clone(),
            })?;
        clauses.push(property_clause(
            &format!("${var}.{}", property.name),
            property,
            filter,
            i,
            params,
        ));
    }
    Ok(clauses)
}

/// A filter on one property at `target` (`$n.title`, `$b.city`): its value travels as the
/// parameter `f{i}`.
fn property_clause(
    target: &str,
    property: &Property,
    filter: &Filter,
    i: usize,
    params: &mut Params,
) -> String {
    match filter.op {
        FilterOp::IsEmpty => format!("{target} is null"),
        FilterOp::IsNotEmpty => format!("{target} is not null"),
        op => {
            // A list is filtered by membership, so its value is one element.
            let ty = if property.list {
                Property {
                    list: false,
                    ..property.clone()
                }
                .param_type(false)
            } else {
                property.param_type(false)
            };
            // `in` takes a list of the property's values.
            let ty = if op == FilterOp::In {
                Property {
                    list: true,
                    nullable: false,
                    ..property.clone()
                }
                .param_type(false)
            } else {
                ty
            };
            let param = params.add(&format!("f{i}"), &ty, filter.value.clone());
            let operator = match op {
                FilterOp::Eq => "=",
                FilterOp::Ne => "!=",
                FilterOp::Lt => "<",
                FilterOp::Le => "<=",
                FilterOp::Gt => ">",
                FilterOp::Ge | FilterOp::Today | FilterOp::ThisWeek | FilterOp::Last30Days => ">=",
                FilterOp::Contains => "contains",
                FilterOp::In => "in",
                FilterOp::IsEmpty
                | FilterOp::IsNotEmpty
                | FilterOp::HasAny
                | FilterOp::HasNone
                | FilterOp::RelatedTo => unreachable!(),
            };
            format!("{target} {operator} {param}")
        }
    }
}

/// A relationship filter as a correlated block: `exists { $r0: Company  $n worksAt $r0 }`, or
/// `not { … }` for "has none"; "related to" also binds the other node's id.
fn relationship_clause(
    schema: &SchemaModel,
    var: &str,
    i: usize,
    filter: &Filter,
    params: &mut Params,
) -> Result<String, BuildError> {
    let Some((direction, edge_name)) = parse_relationship_key(&filter.property) else {
        return Err(BuildError::UnknownEdge(filter.property.clone()));
    };
    let edge = edge(schema, edge_name)?;
    let other = format!("$r{i}");
    let (other_type, hop) = match direction {
        Direction::Outgoing => (&edge.to, format!("${var} {} {other}", edge.traversal())),
        Direction::Incoming => (&edge.from, format!("{other} {} ${var}", edge.traversal())),
    };
    let mut body = vec![format!("{other}: {other_type}"), hop];
    if filter.op == FilterOp::RelatedTo {
        let id = params.add(&format!("f{i}"), "String", filter.value.clone());
        body.push(format!("{other}.@id = {id}"));
    }
    let block = if filter.op == FilterOp::HasNone {
        "not"
    } else {
        "exists"
    };
    Ok(format!(
        "{block} {{\n      {}\n    }}",
        body.join("\n      ")
    ))
}

/// A filter on a property of the node's neighbours through one relationship, as a correlated
/// block, so a node with several such neighbours stays one row: `exists { $r0: Company
/// $n worksAt $r0  $r0.city = $f0 }`. "Is empty" is `not { … $r0.city is not null }`: no
/// neighbour holds a value, including when there is no neighbour.
#[allow(clippy::too_many_arguments)]
fn neighbour_property_clause(
    schema: &SchemaModel,
    var: &str,
    i: usize,
    filter: &Filter,
    direction: Direction,
    edge_name: &str,
    property_name: &str,
    params: &mut Params,
) -> Result<String, BuildError> {
    let edge = edge(schema, edge_name)?;
    let other = format!("$r{i}");
    let (other_type, hop) = match direction {
        Direction::Outgoing => (&edge.to, format!("${var} {} {other}", edge.traversal())),
        Direction::Incoming => (&edge.from, format!("{other} {} ${var}", edge.traversal())),
    };
    let other_node = node(schema, other_type)?;
    let property = other_node
        .properties
        .iter()
        .find(|p| p.name == property_name)
        .ok_or_else(|| BuildError::UnknownProperty {
            type_name: other_node.name.clone(),
            property: property_name.to_owned(),
        })?;
    let target = format!("{other}.{}", property.name);
    let mut body = vec![format!("{other}: {other_type}"), hop];
    let block = if filter.op == FilterOp::IsEmpty {
        body.push(format!("{target} is not null"));
        "not"
    } else {
        body.push(property_clause(&target, property, filter, i, params));
        "exists"
    };
    Ok(format!(
        "{block} {{\n      {}\n    }}",
        body.join("\n      ")
    ))
}

/// One page of a node type (spec 004): keyset-paged by `@id`, or by `sort` then `@id`, from
/// `start`.
pub fn node_page(
    schema: &SchemaModel,
    type_name: &str,
    filters: &[Filter],
    sort: Option<&Sort>,
    start: &PageStart,
    limit: u32,
) -> Result<BuiltQuery, BuildError> {
    let node = node(schema, type_name)?;
    let mut params = Params::new();
    let (keyset, order) = keyset("n", &node.name, &node.properties, sort, start, &mut params)?;
    let mut clauses = vec![format!("$n: {}", node.name)];
    clauses.extend(keyset);
    clauses.extend(filter_clauses(
        schema,
        &node.name,
        &node.properties,
        "n",
        filters,
        &mut params,
    )?);
    let text = format!(
        "query studio_page({sig}) {{\n  match {{\n    {clauses}\n  }}\n  return {{ $n }}\n  order {{ {order} }}\n  limit {limit}\n}}\n",
        sig = params.signature(),
        clauses = clauses.join("\n    "),
    );
    Ok(BuiltQuery {
        name: "studio_page".into(),
        text,
        params: params.values,
    })
}

/// How many rows of a node type match the filters.
pub fn node_count(
    schema: &SchemaModel,
    type_name: &str,
    filters: &[Filter],
) -> Result<BuiltQuery, BuildError> {
    let node = node(schema, type_name)?;
    let mut params = Params::new();
    let mut clauses = vec![format!("$n: {}", node.name)];
    clauses.extend(filter_clauses(
        schema,
        &node.name,
        &node.properties,
        "n",
        filters,
        &mut params,
    )?);
    let text = format!(
        "query studio_count({sig}) {{\n  match {{\n    {clauses}\n  }}\n  return {{ count($n) as n }}\n}}\n",
        sig = params.signature(),
        clauses = clauses.join("\n    "),
    );
    Ok(BuiltQuery {
        name: "studio_count".into(),
        text,
        params: params.values,
    })
}

/// One node by `@id`, at most one row (spec 004: re-reading a row after a conflicting edit, and
/// the peek of a node opened by id).
pub fn node_by_id(
    schema: &SchemaModel,
    type_name: &str,
    id: &str,
) -> Result<BuiltQuery, BuildError> {
    let node = node(schema, type_name)?;
    let mut params = Params::new();
    let id = params.add("id", "String", Value::String(id.to_owned()));
    let text = format!(
        "query studio_node({sig}) {{\n  match {{\n    $n: {ty}\n    $n.@id = {id}\n  }}\n  return {{ $n }}\n  limit 1\n}}\n",
        sig = params.signature(),
        ty = node.name,
    );
    Ok(BuiltQuery {
        name: "studio_node".into(),
        text,
        params: params.values,
    })
}

/// A read that returns no rows, sent only for the commit its answer names: a branch's head
/// when it is read at the branch. `None` when the schema has no node type to match.
pub fn head_probe(schema: &SchemaModel) -> Option<BuiltQuery> {
    let node = schema.nodes.first()?;
    let text = format!(
        "query studio_head() {{\n  match {{\n    $n: {ty}\n  }}\n  return {{ $n.@id }}\n  limit 0\n}}\n",
        ty = node.name,
    );
    Some(BuiltQuery {
        name: "studio_head".into(),
        text,
        params: Map::new(),
    })
}

fn edge<'a>(schema: &'a SchemaModel, name: &str) -> Result<&'a EdgeType, BuildError> {
    schema
        .edge(name)
        .ok_or_else(|| BuildError::UnknownEdge(name.to_owned()))
}

/// The column an edge property is returned under by [`edge_page`].
pub fn edge_property_column(property: &str) -> String {
    format!("e_{property}")
}

/// One page of an edge type, keyset-paged by the edge's `@id` (spec 004). Columns: `edge_id`,
/// `from_id`, `to_id`, then `e_<property>` for each projectable edge property. The engine needs
/// a typed source binding to traverse, so the source is bound to the edge's `from` type.
pub fn edge_page(
    schema: &SchemaModel,
    edge_name: &str,
    filters: &[Filter],
    sort: Option<&Sort>,
    start: &PageStart,
    limit: u32,
) -> Result<BuiltQuery, BuildError> {
    let edge = edge(schema, edge_name)?;
    let mut params = Params::new();
    let (keyset, order) = keyset("e", &edge.name, &edge.properties, sort, start, &mut params)?;
    let mut clauses = vec![
        format!("$a: {}", edge.from),
        format!("$a $e:{} $b", edge.traversal()),
    ];
    clauses.extend(keyset);
    clauses.extend(filter_clauses(
        schema,
        &edge.name,
        &edge.properties,
        "e",
        filters,
        &mut params,
    )?);
    let mut columns = vec![
        "$e.@id as edge_id".to_owned(),
        "$a.@id as from_id".to_owned(),
        "$b.@id as to_id".to_owned(),
    ];
    for property in edge.properties.iter().filter(|p| p.projectable()) {
        columns.push(format!(
            "$e.{} as {}",
            property.name,
            edge_property_column(&property.name)
        ));
    }
    let text = format!(
        "query studio_edge_page({sig}) {{\n  match {{\n    {clauses}\n  }}\n  return {{ {columns} }}\n  order {{ {order} }}\n  limit {limit}\n}}\n",
        sig = params.signature(),
        clauses = clauses.join("\n    "),
        columns = columns.join(", "),
    );
    Ok(BuiltQuery {
        name: "studio_edge_page".into(),
        text,
        params: params.values,
    })
}

/// How many edges of a type match the filters. An edge binding cannot be counted bare, so this
/// counts its `@id`.
pub fn edge_count(
    schema: &SchemaModel,
    edge_name: &str,
    filters: &[Filter],
) -> Result<BuiltQuery, BuildError> {
    let edge = edge(schema, edge_name)?;
    let mut params = Params::new();
    let mut clauses = vec![
        format!("$a: {}", edge.from),
        format!("$a $e:{} $b", edge.traversal()),
    ];
    clauses.extend(filter_clauses(
        schema,
        &edge.name,
        &edge.properties,
        "e",
        filters,
        &mut params,
    )?);
    let text = format!(
        "query studio_edge_count({sig}) {{\n  match {{\n    {clauses}\n  }}\n  return {{ count($e.@id) as n }}\n}}\n",
        sig = params.signature(),
        clauses = clauses.join("\n    "),
    );
    Ok(BuiltQuery {
        name: "studio_edge_count".into(),
        text,
        params: params.values,
    })
}

/// How many rows of a node or edge type matching the filters hold each value of a property, by
/// value (spec 004, group-by). Columns: `value`, absent for rows without one, and `n`. At most
/// `limit` values.
pub fn value_counts(
    schema: &SchemaModel,
    type_name: &str,
    edge_table: bool,
    filters: &[Filter],
    property_name: &str,
    limit: u32,
) -> Result<BuiltQuery, BuildError> {
    let mut params = Params::new();
    let (var, counted, mut clauses, properties, owner) = if edge_table {
        let edge = edge(schema, type_name)?;
        (
            "e",
            "$e.@id",
            vec![
                format!("$a: {}", edge.from),
                format!("$a $e:{} $b", edge.traversal()),
            ],
            edge.properties.as_slice(),
            edge.name.clone(),
        )
    } else {
        let node = node(schema, type_name)?;
        (
            "n",
            "$n",
            vec![format!("$n: {}", node.name)],
            node.properties.as_slice(),
            node.name.clone(),
        )
    };
    let property = properties
        .iter()
        .find(|p| p.name == property_name && GroupBy::allowed(p))
        .ok_or_else(|| BuildError::UnknownProperty {
            type_name: owner.clone(),
            property: property_name.to_owned(),
        })?;
    clauses.extend(filter_clauses(
        schema,
        &owner,
        properties,
        var,
        filters,
        &mut params,
    )?);
    let text = format!(
        "query studio_value_counts({sig}) {{\n  match {{\n    {clauses}\n  }}\n  return {{ ${var}.{prop} as value, count({counted}) as n }}\n  order {{ value asc }}\n  limit {limit}\n}}\n",
        sig = params.signature(),
        clauses = clauses.join("\n    "),
        prop = property.name,
    );
    Ok(BuiltQuery {
        name: "studio_value_counts".into(),
        text,
        params: params.values,
    })
}

/// How many filtered rows of a node type each neighbour through one relationship has (spec 004,
/// grouping by a relationship: "people by company"). Columns: `value` (the neighbour's
/// `@id`), `label` when the neighbour's type has a label other than its key, and `n`. A row
/// with no neighbour has no group here; [`node_count`] with a "has none" filter counts those. A
/// row related to two neighbours counts in both.
pub fn neighbour_counts(
    schema: &SchemaModel,
    type_name: &str,
    filters: &[Filter],
    key: &str,
    limit: u32,
) -> Result<BuiltQuery, BuildError> {
    let node = node(schema, type_name)?;
    let (direction, edge_name) =
        parse_relationship_key(key).ok_or_else(|| BuildError::UnknownEdge(key.to_owned()))?;
    let edge = edge(schema, edge_name)?;
    let (other_type, hop) = match direction {
        Direction::Outgoing => (&edge.to, format!("$n {} $g", edge.traversal())),
        Direction::Incoming => (&edge.from, format!("$g {} $n", edge.traversal())),
    };
    let other = self::node(schema, other_type)?;
    let mut params = Params::new();
    let mut clauses = vec![format!("$n: {}", node.name)];
    clauses.extend(filter_clauses(
        schema,
        &node.name,
        &node.properties,
        "n",
        filters,
        &mut params,
    )?);
    clauses.push(format!("$g: {}", other.name));
    clauses.push(hop);
    let label = other
        .label_property()
        .filter(|label| Some(*label) != other.key.as_deref());
    let (label_column, order) = match label {
        Some(label) => (
            format!(", $g.{label} as label"),
            "label asc, value asc".to_owned(),
        ),
        None => (String::new(), "value asc".to_owned()),
    };
    let text = format!(
        "query studio_value_counts({sig}) {{\n  match {{\n    {clauses}\n  }}\n  return {{ $g.@id as value{label_column}, count($n) as n }}\n  order {{ {order} }}\n  limit {limit}\n}}\n",
        sig = params.signature(),
        clauses = clauses.join("\n    "),
    );
    Ok(BuiltQuery {
        name: "studio_value_counts".into(),
        text,
        params: params.values,
    })
}

/// How many filtered rows of a node type have a neighbour with each value of one of its
/// properties (spec 004, grouping by a neighbour's property: "people by the city of their
/// company"). A row counts once per value, however many of its neighbours hold it: of a
/// row's neighbours with one value, only the one with the least `@id` is matched, since GQ's
/// `count` counts matches. Neighbours without a value are left out; the rows with no value at
/// all are an "is empty" count. Columns: `value`, `n`.
pub fn neighbour_value_counts(
    schema: &SchemaModel,
    type_name: &str,
    filters: &[Filter],
    key: &str,
    limit: u32,
) -> Result<BuiltQuery, BuildError> {
    let GroupKey::NeighbourProperty {
        direction,
        edge: edge_name,
        property: property_name,
    } = parse_group_key(key)
    else {
        return Err(BuildError::UnknownEdge(key.to_owned()));
    };
    let node = node(schema, type_name)?;
    let edge = edge(schema, edge_name)?;
    let (other_type, hop, other_hop) = match direction {
        Direction::Outgoing => (
            &edge.to,
            format!("$n {} $g", edge.traversal()),
            format!("$n {} $d", edge.traversal()),
        ),
        Direction::Incoming => (
            &edge.from,
            format!("$g {} $n", edge.traversal()),
            format!("$d {} $n", edge.traversal()),
        ),
    };
    let other = self::node(schema, other_type)?;
    let property = other
        .properties
        .iter()
        .find(|p| p.name == property_name && GroupBy::allowed(p))
        .ok_or_else(|| BuildError::UnknownProperty {
            type_name: other.name.clone(),
            property: property_name.to_owned(),
        })?;
    let mut params = Params::new();
    let mut clauses = vec![format!("$n: {}", node.name)];
    clauses.extend(filter_clauses(
        schema,
        &node.name,
        &node.properties,
        "n",
        filters,
        &mut params,
    )?);
    let prop = &property.name;
    clauses.push(format!("$g: {}", other.name));
    clauses.push(hop);
    clauses.push(format!("$g.{prop} is not null"));
    clauses.push(format!(
        "not {{\n      $d: {}\n      {other_hop}\n      $d.{prop} = $g.{prop}\n      $d.@id < $g.@id\n    }}",
        other.name
    ));
    let text = format!(
        "query studio_value_counts({sig}) {{\n  match {{\n    {clauses}\n  }}\n  return {{ $g.{prop} as value, count($n) as n }}\n  order {{ value asc }}\n  limit {limit}\n}}\n",
        sig = params.signature(),
        clauses = clauses.join("\n    "),
    );
    Ok(BuiltQuery {
        name: "studio_value_counts".into(),
        text,
        params: params.values,
    })
}

/// How many filtered rows of an edge type have each value of a property of one of their ends
/// (spec 004: "jobs by the city of the company"). An edge has one of each end, so it counts
/// once; ends without a value are counted as the null value. Columns: `value`, `n`.
pub fn endpoint_value_counts(
    schema: &SchemaModel,
    edge_name: &str,
    filters: &[Filter],
    key: &str,
    limit: u32,
) -> Result<BuiltQuery, BuildError> {
    let GroupKey::EndpointProperty {
        end,
        property: property_name,
    } = parse_group_key(key)
    else {
        return Err(BuildError::UnknownProperty {
            type_name: edge_name.to_owned(),
            property: key.to_owned(),
        });
    };
    let edge = edge(schema, edge_name)?;
    let (end_var, end_type) = match end {
        EdgeEnd::From => ("a", &edge.from),
        EdgeEnd::To => ("b", &edge.to),
    };
    let end_node = node(schema, end_type)?;
    let property = end_node
        .properties
        .iter()
        .find(|p| p.name == property_name && GroupBy::allowed(p))
        .ok_or_else(|| BuildError::UnknownProperty {
            type_name: end_node.name.clone(),
            property: property_name.to_owned(),
        })?;
    let mut params = Params::new();
    let mut clauses = vec![
        format!("$a: {}", edge.from),
        format!("$a $e:{} $b", edge.traversal()),
    ];
    clauses.extend(filter_clauses(
        schema,
        &edge.name,
        &edge.properties,
        "e",
        filters,
        &mut params,
    )?);
    let text = format!(
        "query studio_value_counts({sig}) {{\n  match {{\n    {clauses}\n  }}\n  return {{ ${end_var}.{prop} as value, count($e.@id) as n }}\n  order {{ value asc }}\n  limit {limit}\n}}\n",
        sig = params.signature(),
        clauses = clauses.join("\n    "),
        prop = property.name,
    );
    Ok(BuiltQuery {
        name: "studio_value_counts".into(),
        text,
        params: params.values,
    })
}

fn traversal_clause(group: &RelationshipGroup, edge_var: Option<&str>) -> String {
    let edge = match edge_var {
        Some(var) => format!("${var}:{}", traversal_name(&group.edge)),
        None => traversal_name(&group.edge),
    };
    match group.direction {
        Direction::Outgoing => format!("$n {edge} $m"),
        Direction::Incoming => format!("$m {edge} $n"),
    }
}

/// How many neighbours a node has through one relationship group (spec 005).
pub fn degree(
    schema: &SchemaModel,
    node_type: &str,
    group: &RelationshipGroup,
    id: &str,
) -> Result<BuiltQuery, BuildError> {
    let node = node(schema, node_type)?;
    schema
        .edge(&group.edge)
        .ok_or_else(|| BuildError::UnknownEdge(group.edge.clone()))?;
    let mut params = Params::new();
    let id = params.add("id", "String", Value::String(id.to_owned()));
    let text = format!(
        "query studio_degree({sig}) {{\n  match {{\n    $n: {ty}\n    $n.@id = {id}\n    {traversal}\n  }}\n  return {{ count($m) as n }}\n}}\n",
        sig = params.signature(),
        ty = node.name,
        traversal = traversal_clause(group, None),
    );
    Ok(BuiltQuery {
        name: "studio_degree".into(),
        text,
        params: params.values,
    })
}

/// How many neighbours each node of a page has through a relationship group (spec 004,
/// relationship columns). Columns: `id`, `n`; a node with none has no row.
pub fn page_degrees(
    schema: &SchemaModel,
    node_type: &str,
    group: &RelationshipGroup,
    ids: &[String],
) -> Result<BuiltQuery, BuildError> {
    let node = node(schema, node_type)?;
    schema
        .edge(&group.edge)
        .ok_or_else(|| BuildError::UnknownEdge(group.edge.clone()))?;
    let mut params = Params::new();
    let ids = params.add("ids", "[String]", Value::from(ids.to_vec()));
    let text = format!(
        "query studio_page_degrees({sig}) {{\n  match {{\n    $n: {ty}\n    $n.@id in {ids}\n    {traversal}\n  }}\n  return {{ $n.@id as id, count($m) as n }}\n}}\n",
        sig = params.signature(),
        ty = node.name,
        traversal = traversal_clause(group, None),
    );
    Ok(BuiltQuery {
        name: "studio_page_degrees".into(),
        text,
        params: params.values,
    })
}

/// The first neighbours of a page's nodes through a relationship group, for the column's
/// chips: `id`, `other`, then `label` when the other type has one. At most `limit` rows in all,
/// by node then neighbour; a node past the limit shows its count alone.
pub fn page_neighbours(
    schema: &SchemaModel,
    node_type: &str,
    group: &RelationshipGroup,
    ids: &[String],
    limit: u32,
) -> Result<BuiltQuery, BuildError> {
    let node = node(schema, node_type)?;
    schema
        .edge(&group.edge)
        .ok_or_else(|| BuildError::UnknownEdge(group.edge.clone()))?;
    let other = self::node(schema, &group.other_type)?;
    let mut params = Params::new();
    let ids = params.add("ids", "[String]", Value::from(ids.to_vec()));
    let mut columns = vec!["$n.@id as id".to_owned(), "$m.@id as other".to_owned()];
    if let Some(label) = other.label_property()
        && Some(label) != other.key.as_deref()
    {
        columns.push(format!("$m.{label} as label"));
    }
    let text = format!(
        "query studio_page_neighbours({sig}) {{\n  match {{\n    $n: {ty}\n    $n.@id in {ids}\n    {traversal}\n  }}\n  return {{ {columns} }}\n  order {{ $n.@id asc, $m.@id asc }}\n  limit {limit}\n}}\n",
        sig = params.signature(),
        ty = node.name,
        traversal = traversal_clause(group, None),
        columns = columns.join(", "),
    );
    Ok(BuiltQuery {
        name: "studio_page_neighbours".into(),
        text,
        params: params.values,
    })
}

/// The same relationship seen from the other end: from a `node_type` row to its neighbours,
/// back from a neighbour to `node_type` rows.
pub fn reversed(group: &RelationshipGroup, node_type: &str) -> RelationshipGroup {
    RelationshipGroup {
        edge: group.edge.clone(),
        direction: match group.direction {
            Direction::Outgoing => Direction::Incoming,
            Direction::Incoming => Direction::Outgoing,
        },
        other_type: node_type.to_owned(),
    }
}

/// Where a grouping property lives: on the neighbour, or on the edge that reaches it; or the
/// neighbours are grouped by a relationship of their own ("people Ada knows, by company"),
/// whose key the property holds (`out:WorksAt`) and whose other end is each value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GroupOn {
    Node,
    Edge,
    Relationship,
}

/// A property a relationship group's neighbours are grouped by in the tree (spec 005).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GroupBy {
    pub on: GroupOn,
    pub property: String,
}

impl GroupBy {
    /// Whether neighbours can be grouped by a property: one value per row that can repeat
    /// (text, enum, Bool, integer or date), and not the key, which never repeats.
    pub fn allowed(property: &Property) -> bool {
        !property.list
            && !property.is_key
            && matches!(
                property.kind,
                ScalarKind::Text | ScalarKind::Bool | ScalarKind::Integer | ScalarKind::Date
            )
    }

    /// How a choice reads: the property's name, or the relationship as `worksAt →`.
    pub fn label(&self) -> String {
        match self.on {
            GroupOn::Relationship => {
                relationship_label(&self.property).unwrap_or_else(|| self.property.clone())
            }
            GroupOn::Node | GroupOn::Edge => self.property.clone(),
        }
    }

    /// What a group's neighbours can be grouped by: the other type's properties, then the
    /// edge's, in schema order, then the other type's relationships.
    pub fn choices(schema: &SchemaModel, group: &RelationshipGroup) -> Vec<GroupBy> {
        let node = schema
            .node(&group.other_type)
            .map(|node| node.properties.as_slice())
            .unwrap_or_default();
        let edge = schema
            .edge(&group.edge)
            .map(|edge| edge.properties.as_slice())
            .unwrap_or_default();
        let pick = |on: GroupOn, properties: &[Property]| -> Vec<GroupBy> {
            properties
                .iter()
                .filter(|property| Self::allowed(property))
                .map(|property| GroupBy {
                    on,
                    property: property.name.clone(),
                })
                .collect()
        };
        let mut choices = pick(GroupOn::Node, node);
        choices.extend(pick(GroupOn::Edge, edge));
        choices.extend(
            schema
                .relationships_of(&group.other_type)
                .iter()
                .map(|relationship| GroupBy {
                    on: GroupOn::Relationship,
                    property: relationship_key(relationship),
                }),
        );
        choices
    }

    /// For a grouping by the neighbours' relationship: the type at its other end, and the hop
    /// from a neighbour `$m` to a value `$v`.
    fn hop<'a>(
        &self,
        schema: &'a SchemaModel,
        group: &RelationshipGroup,
    ) -> Result<(&'a NodeType, String), BuildError> {
        let unknown = || BuildError::UnknownEdge(self.property.clone());
        let (direction, edge_name) = parse_relationship_key(&self.property).ok_or_else(unknown)?;
        let edge = edge(schema, edge_name)?;
        let (from, other, hop) = match direction {
            Direction::Outgoing => (&edge.from, &edge.to, format!("$m {} $v", edge.traversal())),
            Direction::Incoming => (&edge.to, &edge.from, format!("$v {} $m", edge.traversal())),
        };
        if *from != group.other_type {
            return Err(unknown());
        }
        Ok((self::node(schema, other)?, hop))
    }

    fn resolve<'a>(
        &self,
        schema: &'a SchemaModel,
        group: &RelationshipGroup,
    ) -> Result<&'a Property, BuildError> {
        let (owner, properties) = match self.on {
            GroupOn::Relationship => {
                return Err(BuildError::UnknownProperty {
                    type_name: group.other_type.clone(),
                    property: self.property.clone(),
                });
            }
            GroupOn::Node => {
                let node = self::node(schema, &group.other_type)?;
                (&node.name, node.properties.as_slice())
            }
            GroupOn::Edge => {
                let edge = schema
                    .edge(&group.edge)
                    .ok_or_else(|| BuildError::UnknownEdge(group.edge.clone()))?;
                (&edge.name, edge.properties.as_slice())
            }
        };
        properties
            .iter()
            .find(|property| property.name == self.property && Self::allowed(property))
            .ok_or_else(|| BuildError::UnknownProperty {
                type_name: owner.clone(),
                property: self.property.clone(),
            })
    }

    /// The property as the neighbour queries spell it: `$m.title` or `$e.role`; for a
    /// relationship, the node at its other end, `$v.@id`.
    fn target(&self) -> String {
        match self.on {
            GroupOn::Node => format!("$m.{}", self.property),
            GroupOn::Edge => format!("$e.{}", self.property),
            GroupOn::Relationship => "$v.@id".to_owned(),
        }
    }
}

/// What narrows a page of neighbours: ids or labels matching a text, and one value of the
/// property the group is grouped by (`Value::Null` for neighbours without one).
#[derive(Debug, Clone, Copy, Default)]
pub struct Narrow<'a> {
    pub search: Option<&'a str>,
    pub value: Option<(&'a GroupBy, &'a Value)>,
}

/// One page of a node's neighbours through a relationship group, with the neighbour's label
/// and the edge's properties (spec 005). Columns: `id`, `edge_id` (the edge's own id), `label`
/// (when the other type has one), then `e_<property>` for each projectable edge property.
pub fn neighbours(
    schema: &SchemaModel,
    node_type: &str,
    group: &RelationshipGroup,
    id: &str,
    after: Option<&str>,
    narrow: Narrow<'_>,
    limit: u32,
) -> Result<BuiltQuery, BuildError> {
    let node = node(schema, node_type)?;
    let edge = schema
        .edge(&group.edge)
        .ok_or_else(|| BuildError::UnknownEdge(group.edge.clone()))?;
    let other = self::node(schema, &group.other_type)?;
    let mut params = Params::new();
    let id = params.add("id", "String", Value::String(id.to_owned()));
    let after = params.add(
        "after",
        "String",
        Value::String(after.unwrap_or("").to_owned()),
    );
    // The edge's own id too, which removing one relationship names.
    let mut columns = vec!["$m.@id as id".to_owned(), "$e.@id as edge_id".to_owned()];
    if let Some(label) = other.label_property()
        && Some(label) != other.key.as_deref()
    {
        columns.push(format!("$m.{label} as label"));
    }
    for property in edge.properties.iter().filter(|p| p.projectable()) {
        columns.push(format!("$e.{0} as e_{0}", property.name));
    }
    let mut extra = String::new();
    // A search narrows the neighbours to ids starting with, or labels containing, the text.
    if let Some(text) = narrow.search.filter(|text| !text.is_empty()) {
        let label = other
            .label_property()
            .filter(|label| Some(*label) != other.key.as_deref());
        extra.push_str(&format!(
            "\n    {}",
            text_match("m", label, text, &mut params)
        ));
    }
    if let Some((by, value)) = narrow.value {
        let clause = if by.on == GroupOn::Relationship {
            // The neighbours related to the value, or to nothing through that relationship.
            let (other, hop) = by.hop(schema, group)?;
            let mut body = vec![format!("$v: {}", other.name), hop];
            if !value.is_null() {
                let value = params.add("value", "String", value.clone());
                body.push(format!("$v.@id = {value}"));
            }
            format!(
                "{} {{\n      {}\n    }}",
                if value.is_null() { "not" } else { "exists" },
                body.join("\n      ")
            )
        } else {
            let property = by.resolve(schema, group)?;
            if value.is_null() {
                format!("{} is null", by.target())
            } else {
                let value = params.add("value", &property.param_type(false), value.clone());
                format!("{} = {value}", by.target())
            }
        };
        extra.push_str(&format!("\n    {clause}"));
    }
    let text = format!(
        "query studio_neighbours({sig}) {{\n  match {{\n    $n: {ty}\n    $n.@id = {id}\n    {traversal}\n    $m.@id > {after}{extra}\n  }}\n  return {{ {columns} }}\n  order {{ $m.@id asc }}\n  limit {limit}\n}}\n",
        sig = params.signature(),
        ty = node.name,
        traversal = traversal_clause(group, Some("e")),
        columns = columns.join(", "),
    );
    Ok(BuiltQuery {
        name: "studio_neighbours".into(),
        text,
        params: params.values,
    })
}

/// How many of a node's neighbours through a relationship group have each value of a property,
/// most first, then by value (spec 005, grouping). Columns: `value`, absent where the
/// neighbours have none, and `n`. At most `limit` values.
pub fn neighbour_values(
    schema: &SchemaModel,
    node_type: &str,
    group: &RelationshipGroup,
    id: &str,
    by: &GroupBy,
    limit: u32,
) -> Result<BuiltQuery, BuildError> {
    let node = node(schema, node_type)?;
    // By a relationship of the neighbours: each value is the node at its other end, with its
    // label when its type has one; neighbours with none are counted by `neighbours_without`.
    let (hop, label) = if by.on == GroupOn::Relationship {
        let (other, hop) = by.hop(schema, group)?;
        let label = other
            .label_property()
            .filter(|label| Some(*label) != other.key.as_deref())
            .map(|label| format!(", $v.{label} as label"))
            .unwrap_or_default();
        (format!("\n    $v: {}\n    {hop}", other.name), label)
    } else {
        by.resolve(schema, group)?;
        (String::new(), String::new())
    };
    let mut params = Params::new();
    let id = params.add("id", "String", Value::String(id.to_owned()));
    let text = format!(
        "query studio_neighbour_values({sig}) {{\n  match {{\n    $n: {ty}\n    $n.@id = {id}\n    {traversal}{hop}\n  }}\n  return {{ {target} as value{label}, count($m) as n }}\n  order {{ n desc, value asc }}\n  limit {limit}\n}}\n",
        sig = params.signature(),
        ty = node.name,
        traversal = traversal_clause(group, Some("e")),
        target = by.target(),
    );
    Ok(BuiltQuery {
        name: "studio_neighbour_values".into(),
        text,
        params: params.values,
    })
}

/// How many of a node's neighbours through a relationship group have none of the relationship
/// the group is grouped by (spec 005): the "no value" bucket of [`neighbour_values`], which a
/// grouped read cannot return. Column: `n`.
pub fn neighbours_without(
    schema: &SchemaModel,
    node_type: &str,
    group: &RelationshipGroup,
    id: &str,
    by: &GroupBy,
) -> Result<BuiltQuery, BuildError> {
    let node = node(schema, node_type)?;
    let (other, hop) = by.hop(schema, group)?;
    let mut params = Params::new();
    let id = params.add("id", "String", Value::String(id.to_owned()));
    let text = format!(
        "query studio_neighbours_without({sig}) {{\n  match {{\n    $n: {ty}\n    $n.@id = {id}\n    {traversal}\n    not {{\n      $v: {other}\n      {hop}\n    }}\n  }}\n  return {{ count($m) as n }}\n}}\n",
        sig = params.signature(),
        ty = node.name,
        traversal = traversal_clause(group, Some("e")),
        other = other.name,
    );
    Ok(BuiltQuery {
        name: "studio_neighbours_without".into(),
        text,
        params: params.values,
    })
}

/// One node's key and label, to root the relationship tree and the canvas at it (spec 005,
/// 006). Columns: `id`, then `label` when the type's label property is not its key. No row
/// means the node does not exist at the commit read.
pub fn node_label(
    schema: &SchemaModel,
    type_name: &str,
    id: &str,
) -> Result<BuiltQuery, BuildError> {
    let node = node(schema, type_name)?;
    let mut params = Params::new();
    let id = params.add("id", "String", Value::String(id.to_owned()));
    let mut columns = vec!["$n.@id as id".to_owned()];
    if let Some(label) = node.label_property()
        && Some(label) != node.key.as_deref()
    {
        columns.push(format!("$n.{label} as label"));
    }
    let text = format!(
        "query studio_node({sig}) {{\n  match {{\n    $n: {ty}\n    $n.@id = {id}\n  }}\n  return {{ {columns} }}\n  limit 1\n}}\n",
        sig = params.signature(),
        ty = node.name,
        columns = columns.join(", "),
    );
    Ok(BuiltQuery {
        name: "studio_node".into(),
        text,
        params: params.values,
    })
}

/// Nodes of one type whose id starts with `text`, or whose label contains it (spec 002, ⌘P go
/// to a node). Columns: `id`, then `label` when the type has a label other than its key.
pub fn node_search(
    schema: &SchemaModel,
    type_name: &str,
    text: &str,
    limit: u32,
) -> Result<BuiltQuery, BuildError> {
    let node = node(schema, type_name)?;
    let mut params = Params::new();
    let label = node
        .label_property()
        .filter(|label| Some(*label) != node.key.as_deref());
    let mut columns = vec!["$n.@id as id".to_owned()];
    if let Some(label) = label {
        columns.push(format!("$n.{label} as label"));
    }
    let predicate = text_match("n", label, text, &mut params);
    let text = format!(
        "query studio_find({sig}) {{\n  match {{\n    $n: {ty}\n    {predicate}\n  }}\n  return {{ {columns} }}\n  order {{ $n.@id asc }}\n  limit {limit}\n}}\n",
        sig = params.signature(),
        ty = node.name,
        columns = columns.join(", "),
    );
    Ok(BuiltQuery {
        name: "studio_find".into(),
        text,
        params: params.values,
    })
}

/// The spellings a search tries, since `contains` and `starts_with` compare exactly: as typed,
/// lowercase, and with each word capitalized ("ada lov" also finds "Ada Lovelace").
fn spellings(text: &str) -> Vec<String> {
    let capitalize = |word: &str| {
        let mut chars = word.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
            None => String::new(),
        }
    };
    let words: Vec<String> = text.split(' ').map(capitalize).collect();
    let mut found: Vec<String> = Vec::new();
    for spelling in [
        text.to_owned(),
        text.to_lowercase(),
        capitalize(text),
        words.join(" "),
    ] {
        if !spelling.is_empty() && !found.contains(&spelling) {
            found.push(spelling);
        }
    }
    found
}

/// `$var`'s id starting with, or its label containing, `text` in any of its [`spellings`].
fn text_match(var: &str, label: Option<&str>, text: &str, params: &mut Params) -> String {
    let mut tests = Vec::new();
    for (index, spelling) in spellings(text).into_iter().enumerate() {
        let q = params.add(&format!("q{index}"), "String", Value::String(spelling));
        tests.push(format!("${var}.@id starts_with {q}"));
        if let Some(label) = label {
            tests.push(format!("${var}.{label} contains {q}"));
        }
    }
    format!("({})", tests.join(" or "))
}

/// Whether a node with this id and label answers [`node_search`] for `text`, decided as the
/// query's predicate decides it (`label` is the search's `label` column).
pub fn search_matches(id: &str, label: Option<&str>, text: &str) -> bool {
    spellings(text).iter().any(|spelling| {
        id.starts_with(spelling.as_str())
            || label.is_some_and(|label| label.contains(spelling.as_str()))
    })
}

/// Whether every node that answers [`node_search`] for `text` also answered it for
/// `previous`: each spelling `text` is tried in starts with one `previous` was tried in, so
/// an id starting with it, or a label containing it, matched before. A caller holding every
/// node that answered `previous` filters them with [`search_matches`] instead of asking again.
/// Checked rather than assumed from `text` starting with `previous`: lowercasing can change a
/// letter by what follows it (a final sigma).
pub fn search_narrows(previous: &str, text: &str) -> bool {
    let before = spellings(previous);
    !before.is_empty()
        && spellings(text).iter().all(|spelling| {
            before
                .iter()
                .any(|earlier| spelling.starts_with(earlier.as_str()))
        })
}

/// Sets one property of one node, bound to the value the cell showed (spec 004). Zero affected
/// nodes means the value changed elsewhere. List properties cannot be compared, so they are
/// bound to the row only.
pub fn update_cell(
    schema: &SchemaModel,
    type_name: &str,
    property_name: &str,
    id: &str,
    old: &Value,
    new: &Value,
) -> Result<BuiltQuery, BuildError> {
    let node = node(schema, type_name)?;
    let property = property(node, property_name)?;
    if !property.editable_in_cell() {
        return Err(BuildError::NotEditable(property.name.clone()));
    }
    let mut params = Params::new();
    let id = params.add("id", "String", Value::String(id.to_owned()));
    let new = params.add("new", &property.param_type(true), new.clone());
    let mut predicate = format!("@id = {id}");
    if !property.list {
        if old.is_null() {
            predicate.push_str(&format!(" and {} is null", property.name));
        } else {
            let old = params.add("old", &property.param_type(false), old.clone());
            predicate.push_str(&format!(" and {} = {old}", property.name));
        }
    }
    let text = format!(
        "query studio_set({sig}) {{\n  update {ty} set {{ {prop}: {new} }} where {predicate}\n}}\n",
        sig = params.signature(),
        ty = node.name,
        prop = property.name,
    );
    Ok(BuiltQuery {
        name: "studio_set".into(),
        text,
        params: params.values,
    })
}

/// One row's edits in a batch: its id, and per property the value the row showed and the new
/// value.
#[derive(Debug, Clone, PartialEq)]
pub struct RowEdit {
    pub id: String,
    pub cells: Vec<(String, Value, Value)>,
}

/// Saves edits of different values on several rows in one mutation, so they publish as one
/// commit (spec 004, drafts). Each row is one statement setting all its edited properties, bound
/// to every value the row showed: a row changed elsewhere is left alone whole, and
/// `affected_nodes` comes back short by it.
pub fn update_rows(
    schema: &SchemaModel,
    type_name: &str,
    rows: &[RowEdit],
) -> Result<BuiltQuery, BuildError> {
    let node = node(schema, type_name)?;
    let mut params = Params::new();
    let mut statements = Vec::new();
    for (r, row) in rows.iter().enumerate() {
        let id = params.add(&format!("id{r}"), "String", Value::String(row.id.clone()));
        let mut sets = Vec::new();
        let mut predicate = format!("@id = {id}");
        for (c, (name, old, new)) in row.cells.iter().enumerate() {
            let property = property(node, name)?;
            if !property.editable_in_cell() {
                return Err(BuildError::NotEditable(property.name.clone()));
            }
            let new = params.add(
                &format!("new{r}_{c}"),
                &property.param_type(true),
                new.clone(),
            );
            sets.push(format!("{}: {new}", property.name));
            if property.list {
                continue;
            }
            if old.is_null() {
                predicate.push_str(&format!(" and {} is null", property.name));
            } else {
                let old = params.add(
                    &format!("old{r}_{c}"),
                    &property.param_type(false),
                    old.clone(),
                );
                predicate.push_str(&format!(" and {} = {old}", property.name));
            }
        }
        statements.push(format!(
            "update {ty} set {{ {sets} }} where {predicate}",
            ty = node.name,
            sets = sets.join(", "),
        ));
    }
    let text = format!(
        "query studio_save_rows({sig}) {{\n  {statements}\n}}\n",
        sig = params.signature(),
        statements = statements.join("\n  "),
    );
    Ok(BuiltQuery {
        name: "studio_save_rows".into(),
        text,
        params: params.values,
    })
}

/// Sets one property on several rows in one mutation, so they publish as one commit (spec 004,
/// bulk set). Each row's statement is bound to the value the row showed, as a cell edit is: a
/// row changed elsewhere is left alone, and `affected_nodes` comes back short by it.
pub fn update_cells(
    schema: &SchemaModel,
    type_name: &str,
    property_name: &str,
    rows: &[(String, Value)],
    new: &Value,
) -> Result<BuiltQuery, BuildError> {
    let node = node(schema, type_name)?;
    let property = property(node, property_name)?;
    if !property.editable_in_cell() {
        return Err(BuildError::NotEditable(property.name.clone()));
    }
    let mut params = Params::new();
    let new = params.add("new", &property.param_type(true), new.clone());
    let statements: Vec<String> = rows
        .iter()
        .enumerate()
        .map(|(index, (id, old))| {
            let id = params.add(&format!("id{index}"), "String", Value::String(id.clone()));
            let mut predicate = format!("@id = {id}");
            if !property.list {
                if old.is_null() {
                    predicate.push_str(&format!(" and {} is null", property.name));
                } else {
                    let old = params.add(
                        &format!("old{index}"),
                        &property.param_type(false),
                        old.clone(),
                    );
                    predicate.push_str(&format!(" and {} = {old}", property.name));
                }
            }
            format!(
                "update {ty} set {{ {prop}: {new} }} where {predicate}",
                ty = node.name,
                prop = property.name,
            )
        })
        .collect();
    let text = format!(
        "query studio_set_rows({sig}) {{\n  {statements}\n}}\n",
        sig = params.signature(),
        statements = statements.join("\n  "),
    );
    Ok(BuiltQuery {
        name: "studio_set_rows".into(),
        text,
        params: params.values,
    })
}

/// Inserts one node with the given property values.
pub fn insert_node(
    schema: &SchemaModel,
    type_name: &str,
    values: &[(String, Value)],
) -> Result<BuiltQuery, BuildError> {
    let node = node(schema, type_name)?;
    let mut params = Params::new();
    let mut assignments = Vec::new();
    for (i, (name, value)) in values.iter().enumerate() {
        let property = property(node, name)?;
        let param = params.add(
            &format!("p{i}"),
            &property.param_type(property.nullable),
            value.clone(),
        );
        assignments.push(format!("{}: {param}", property.name));
    }
    let text = format!(
        "query studio_insert({sig}) {{\n  insert {ty} {{ {assignments} }}\n}}\n",
        sig = params.signature(),
        ty = node.name,
        assignments = assignments.join(", "),
    );
    Ok(BuiltQuery {
        name: "studio_insert".into(),
        text,
        params: params.values,
    })
}

/// Deletes one node by key.
pub fn delete_node(
    schema: &SchemaModel,
    type_name: &str,
    id: &str,
) -> Result<BuiltQuery, BuildError> {
    let node = node(schema, type_name)?;
    let mut params = Params::new();
    let id = params.add("id", "String", Value::String(id.to_owned()));
    let text = format!(
        "query studio_delete({sig}) {{\n  delete {ty} where @id = {id}\n}}\n",
        sig = params.signature(),
        ty = node.name,
    );
    Ok(BuiltQuery {
        name: "studio_delete".into(),
        text,
        params: params.values,
    })
}

/// Inserts one edge between two existing nodes with the given property values. An unkeyed edge
/// gets a new id: a `.gq` insert cannot supply one.
pub fn insert_edge(
    schema: &SchemaModel,
    edge_name: &str,
    from: &str,
    to: &str,
    values: &[(String, Value)],
) -> Result<BuiltQuery, BuildError> {
    let edge = edge(schema, edge_name)?;
    let mut params = Params::new();
    let from = params.add("from", "String", Value::String(from.to_owned()));
    let to = params.add("to", "String", Value::String(to.to_owned()));
    let mut assignments = vec![format!("from: {from}"), format!("to: {to}")];
    for (i, (name, value)) in values.iter().enumerate() {
        let property = edge
            .properties
            .iter()
            .find(|p| &p.name == name)
            .ok_or_else(|| BuildError::UnknownProperty {
                type_name: edge.name.clone(),
                property: name.clone(),
            })?;
        let param = params.add(
            &format!("p{i}"),
            &property.param_type(property.nullable),
            value.clone(),
        );
        assignments.push(format!("{}: {param}", property.name));
    }
    let text = format!(
        "query studio_insert_edge({sig}) {{\n  insert {ty} {{ {assignments} }}\n}}\n",
        sig = params.signature(),
        ty = edge.name,
        assignments = assignments.join(", "),
    );
    Ok(BuiltQuery {
        name: "studio_insert_edge".into(),
        text,
        params: params.values,
    })
}

/// Deletes one edge by its id.
pub fn delete_edge(
    schema: &SchemaModel,
    edge_name: &str,
    id: &str,
) -> Result<BuiltQuery, BuildError> {
    let edge = edge(schema, edge_name)?;
    let mut params = Params::new();
    let id = params.add("id", "String", Value::String(id.to_owned()));
    let text = format!(
        "query studio_delete_edge({sig}) {{\n  delete {ty} where @id = {id}\n}}\n",
        sig = params.signature(),
        ty = edge.name,
    );
    Ok(BuiltQuery {
        name: "studio_delete_edge".into(),
        text,
        params: params.values,
    })
}

/// The text a new query editor starts with (spec 007): a read over the first node type, with
/// its key and a few properties, ordered by its label. `None` when the schema has no node type.
pub fn example_read(schema: &SchemaModel) -> Option<String> {
    let node = schema.nodes.first()?;
    let var = node
        .name
        .chars()
        .next()
        .map(|c| c.to_ascii_lowercase())
        .filter(char::is_ascii_lowercase)
        .unwrap_or('n');
    let mut columns: Vec<&str> = Vec::new();
    for name in node
        .key
        .iter()
        .map(String::as_str)
        .chain(node.label_property())
    {
        if !columns.contains(&name) {
            columns.push(name);
        }
    }
    for property in &node.properties {
        if columns.len() >= 4 {
            break;
        }
        if property.projectable() && !columns.contains(&property.name.as_str()) {
            columns.push(&property.name);
        }
    }
    let returns = if columns.is_empty() {
        format!("${var}")
    } else {
        columns
            .iter()
            .map(|c| format!("${var}.{c}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let order = node
        .label_property()
        .map(|label| format!("  order {{ ${var}.{label} asc }}\n"))
        .unwrap_or_default();
    let name = snake_case(&node.name);
    Some(format!(
        "query list_{name}() {{\n  match {{ ${var}: {ty} }}\n  return {{ {returns} }}\n{order}  limit 50\n}}\n",
        ty = node.name,
    ))
}

/// `WorksAt` → `works_at`.
fn snake_case(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::check::check;
    use crate::schema::tests::dev;

    fn assert_compiles(query: &BuiltQuery) {
        let schema = dev();
        let result = check(&schema, &query.text);
        assert!(
            result.diagnostics.is_empty(),
            "generated query does not type-check:\n{}\n{:?}",
            query.text,
            result.diagnostics
        );
    }

    #[test]
    fn searches_try_the_usual_spellings() {
        assert_eq!(spellings("ada lov"), ["ada lov", "Ada lov", "Ada Lov"]);
        assert_eq!(spellings("Ada"), ["Ada", "ada"]);
        assert_eq!(spellings("person-0001"), ["person-0001", "Person-0001"]);
        let schema = dev();
        let query = node_search(&schema, "Person", "ada", 5).unwrap();
        assert!(
            query.text.contains("$n.name contains $q1"),
            "{}",
            query.text
        );
        assert_compiles(&query);
    }

    #[test]
    fn a_bulk_set_is_one_mutation_bound_row_by_row() {
        let schema = dev();
        let rows = vec![
            ("person-000001".to_owned(), Value::String("Engineer".into())),
            ("person-000006".to_owned(), Value::Null),
        ];
        let query = update_cells(
            &schema,
            "Person",
            "title",
            &rows,
            &Value::String("Founder".into()),
        )
        .unwrap();
        assert!(
            query
                .text
                .contains("update Person set { title: $new } where @id = $id0 and title = $old0"),
            "{}",
            query.text
        );
        assert!(query.text.contains("where @id = $id1 and title is null"));
        assert_eq!(query.params["old0"], "Engineer");
        assert_compiles(&query);
        assert!(matches!(
            update_cells(&schema, "Person", "slug", &rows, &Value::Null),
            Err(BuildError::NotEditable(_))
        ));
    }

    #[test]
    fn edits_of_different_values_are_one_mutation_bound_row_by_row() {
        let schema = dev();
        let rows = vec![
            RowEdit {
                id: "person-000001".into(),
                cells: vec![
                    ("title".into(), json!("Engineer"), json!("Founder")),
                    ("born".into(), Value::Null, json!("1990-02-03")),
                ],
            },
            RowEdit {
                id: "person-000006".into(),
                cells: vec![("tags".into(), json!(["a"]), json!(["a", "b"]))],
            },
        ];
        let query = update_rows(&schema, "Person", &rows).unwrap();
        assert!(
            query.text.contains(
                "update Person set { title: $new0_0, born: $new0_1 } where @id = $id0 \
                 and title = $old0_0 and born is null"
            ),
            "{}",
            query.text
        );
        assert!(
            query
                .text
                .contains("update Person set { tags: $new1_0 } where @id = $id1\n"),
            "a list is not bound to its old value: {}",
            query.text
        );
        assert_eq!(query.params["new0_1"], "1990-02-03");
        assert_compiles(&query);
        let key = vec![RowEdit {
            id: "x".into(),
            cells: vec![("slug".into(), json!("x"), json!("y"))],
        }];
        assert!(matches!(
            update_rows(&schema, "Person", &key),
            Err(BuildError::NotEditable(_))
        ));
    }

    #[test]
    fn value_counts_group_filtered_rows_of_nodes_and_edges() {
        let schema = dev();
        let filters = [Filter {
            property: "name".into(),
            op: FilterOp::Contains,
            value: json!("a"),
        }];
        for node in ["Person", "Company", "Project"] {
            let node_type = schema.node(node).unwrap();
            for property in node_type.properties.iter().filter(|p| GroupBy::allowed(p)) {
                let query =
                    value_counts(&schema, node, false, &filters, &property.name, 200).unwrap();
                assert_compiles(&query);
                assert!(
                    query
                        .text
                        .contains(&format!("$n.{} as value", property.name))
                );
            }
        }
        let query = value_counts(&schema, "WorksAt", true, &[], "role", 200).unwrap();
        assert_compiles(&query);
        assert!(query.text.contains("count($e.@id) as n"), "{}", query.text);
        assert!(value_counts(&schema, "Person", false, &[], "slug", 10).is_err());
        assert!(value_counts(&schema, "Person", false, &[], "tags", 10).is_err());
    }

    #[test]
    fn neighbour_counts_group_filtered_nodes_by_each_relationship() {
        let schema = dev();
        let named = Filter {
            property: "name".into(),
            op: FilterOp::Contains,
            value: json!("a"),
        };
        let knows = Filter {
            property: "out:Knows".into(),
            op: FilterOp::HasAny,
            value: Value::Null,
        };
        for node in ["Person", "Company", "Project"] {
            let mut filters = vec![named.clone()];
            if node == "Person" {
                filters.push(knows.clone());
            }
            for group in schema.relationships_of(node) {
                let key = relationship_key(&group);
                let query = neighbour_counts(&schema, node, &filters, &key, 200).unwrap();
                assert_compiles(&query);
                assert!(query.text.contains("$g.@id as value"), "{}", query.text);
            }
        }
        let query = neighbour_counts(&schema, "Person", &[], "out:WorksAt", 200).unwrap();
        assert!(query.text.contains("$n worksAt $g"), "{}", query.text);
        assert!(query.text.contains("$g.name as label"), "{}", query.text);
        let query = neighbour_counts(&schema, "Company", &[], "in:WorksAt", 200).unwrap();
        assert!(query.text.contains("$g worksAt $n"), "{}", query.text);
        assert!(neighbour_counts(&schema, "Person", &[], "name", 10).is_err());
    }

    /// A node table by each property of each neighbour, and an edge table by each property of
    /// each end: the counts and both section filters (a value, and "no value") type-check in
    /// the table's page and count reads.
    #[test]
    fn rows_group_by_a_neighbours_or_an_ends_property() {
        let schema = dev();
        let named = Filter {
            property: "name".into(),
            op: FilterOp::Contains,
            value: json!("a"),
        };
        for node_type in ["Person", "Company", "Project"] {
            for group in schema.relationships_of(node_type) {
                let other = schema.node(&group.other_type).unwrap();
                for property in other.properties.iter().filter(|p| GroupBy::allowed(p)) {
                    let key = neighbour_property_key(&group, &property.name);
                    let counts = neighbour_value_counts(
                        &schema,
                        node_type,
                        std::slice::from_ref(&named),
                        &key,
                        200,
                    )
                    .unwrap();
                    assert_compiles(&counts);
                    assert!(counts.text.contains("$d.@id < $g.@id"), "{}", counts.text);
                    for (op, value) in [
                        (FilterOp::Eq, sample_property_value(property)),
                        (FilterOp::IsEmpty, Value::Null),
                    ] {
                        let section = Filter {
                            property: key.clone(),
                            op,
                            value,
                        };
                        let filters = [named.clone(), section];
                        let page =
                            node_page(&schema, node_type, &filters, None, &PageStart::First, 50)
                                .unwrap();
                        assert_compiles(&page);
                        let block = if op == FilterOp::IsEmpty {
                            "not {"
                        } else {
                            "exists {"
                        };
                        assert!(page.text.contains(block), "{}", page.text);
                        assert_compiles(&node_count(&schema, node_type, &filters).unwrap());
                    }
                }
            }
        }
        for edge_name in ["WorksAt", "Knows", "Contributes", "Owns"] {
            let edge = schema.edge(edge_name).unwrap();
            for (end, end_type) in [(EdgeEnd::From, &edge.from), (EdgeEnd::To, &edge.to)] {
                let node = schema.node(end_type).unwrap();
                for property in node.properties.iter().filter(|p| GroupBy::allowed(p)) {
                    let key = endpoint_property_key(end, &property.name);
                    let counts = endpoint_value_counts(&schema, edge_name, &[], &key, 200).unwrap();
                    assert_compiles(&counts);
                    for (op, value) in [
                        (FilterOp::Eq, sample_property_value(property)),
                        (FilterOp::IsEmpty, Value::Null),
                    ] {
                        let filters = [Filter {
                            property: key.clone(),
                            op,
                            value,
                        }];
                        let page =
                            edge_page(&schema, edge_name, &filters, None, &PageStart::First, 50)
                                .unwrap();
                        assert_compiles(&page);
                        assert_compiles(&edge_count(&schema, edge_name, &filters).unwrap());
                    }
                }
            }
        }
        // A neighbour's or an end's key is not a property of the row, nor a relationship.
        assert!(neighbour_value_counts(&schema, "Person", &[], "out:WorksAt", 10).is_err());
        assert!(endpoint_value_counts(&schema, "WorksAt", &[], "role", 10).is_err());
        assert!(neighbour_value_counts(&schema, "Person", &[], "out:WorksAt.slug", 10).is_err());
    }

    #[test]
    fn group_keys_name_one_kind_each() {
        assert_eq!(parse_group_key("title"), GroupKey::Property("title"));
        assert_eq!(
            parse_group_key("out:WorksAt"),
            GroupKey::Relationship(Direction::Outgoing, "WorksAt")
        );
        assert_eq!(
            parse_group_key("in:WorksAt.city"),
            GroupKey::NeighbourProperty {
                direction: Direction::Incoming,
                edge: "WorksAt",
                property: "city",
            }
        );
        assert_eq!(
            parse_group_key("to.city"),
            GroupKey::EndpointProperty {
                end: EdgeEnd::To,
                property: "city",
            }
        );
        // A neighbour's property is not a relationship.
        assert_eq!(parse_relationship_key("out:WorksAt.city"), None);
        assert!(parse_group_key("title").is_own_property());
        assert!(!parse_group_key("out:WorksAt.city").is_own_property());
    }

    /// A value of `property`'s type, for a filter on it.
    fn sample_property_value(property: &Property) -> Value {
        match property.kind {
            ScalarKind::Bool => json!(true),
            ScalarKind::Integer => json!(1),
            ScalarKind::Date => json!("2020-01-01"),
            _ => match &property.enum_values {
                Some(values) => json!(values[0]),
                None => json!("x"),
            },
        }
    }

    #[test]
    fn filter_operators_round_trip_through_their_keys() {
        for op in FilterOp::ALL {
            assert_eq!(FilterOp::from_key(op.key()), Some(op));
        }
        assert_eq!(FilterOp::from_key("nope"), None);
    }

    #[test]
    fn page_relationship_reads_compile_for_every_group() {
        let schema = dev();
        let ids = vec!["person-000000".to_owned(), "person-000001".to_owned()];
        for node in &schema.nodes {
            for group in schema.relationships_of(&node.name) {
                let degrees = page_degrees(&schema, &node.name, &group, &ids).unwrap();
                assert!(degrees.text.contains("$n.@id in $ids"), "{}", degrees.text);
                assert_compiles(&degrees);
                let sample = page_neighbours(&schema, &node.name, &group, &ids, 300).unwrap();
                assert!(sample.text.contains("limit 300"));
                assert_compiles(&sample);
            }
        }
        let group = &schema.relationships_of("Person")[0];
        let back = reversed(group, "Person");
        assert_eq!(back.other_type, "Person");
        assert_ne!(back.direction, group.direction);
    }

    #[test]
    fn sorted_pages_read_values_then_nulls_and_compile() {
        let schema = dev();
        let sort = Sort {
            property: "title".into(),
            descending: false,
        };
        let first = node_page(&schema, "Person", &[], Some(&sort), &PageStart::First, 50).unwrap();
        assert!(
            first.text.contains("$n.title is not null"),
            "{}",
            first.text
        );
        assert!(first.text.contains("order { $n.title asc, $n.@id asc }"));
        assert_compiles(&first);
        let next = node_page(
            &schema,
            "Person",
            &[],
            Some(&Sort {
                descending: true,
                ..sort.clone()
            }),
            &PageStart::After {
                id: "person-000010".into(),
                value: Value::String("Engineer".into()),
            },
            50,
        )
        .unwrap();
        assert!(
            next.text.contains(
                "$n.title < $after_value or ($n.title = $after_value and $n.@id > $after)"
            ),
            "{}",
            next.text
        );
        assert_eq!(next.params["after_value"], "Engineer");
        assert_compiles(&next);
        let nulls = node_page(
            &schema,
            "Person",
            &[],
            Some(&sort),
            &PageStart::Nulls {
                after: String::new(),
            },
            50,
        )
        .unwrap();
        assert!(nulls.text.contains("$n.title is null"));
        assert!(nulls.text.contains("order { $n.@id asc }"));
        assert_compiles(&nulls);
        // Typed values: a date and an integer bind with their own types.
        for (type_name, property, value) in [
            ("Person", "born", Value::String("1980-01-01".into())),
            ("Company", "founded", Value::from(1990)),
        ] {
            let page = node_page(
                &schema,
                type_name,
                &[],
                Some(&Sort {
                    property: property.into(),
                    descending: false,
                }),
                &PageStart::After {
                    id: "x".into(),
                    value,
                },
                10,
            )
            .unwrap();
            assert_compiles(&page);
        }
        let edges = edge_page(
            &schema,
            "WorksAt",
            &[],
            Some(&Sort {
                property: "since".into(),
                descending: true,
            }),
            &PageStart::First,
            10,
        )
        .unwrap();
        assert_compiles(&edges);
        assert!(matches!(
            node_page(
                &schema,
                "Person",
                &[],
                Some(&Sort {
                    property: "tags".into(),
                    descending: false
                }),
                &PageStart::First,
                10
            ),
            Err(BuildError::UnknownProperty { .. })
        ));
    }

    #[test]
    fn the_example_read_compiles_and_names_its_columns() {
        let schema = dev();
        let text = example_read(&schema).unwrap();
        let result = check(&schema, &text);
        assert!(
            result.diagnostics.is_empty(),
            "{text}\n{:?}",
            result.diagnostics
        );
        let query = &result.queries[0];
        // The first node type in the compiled schema; the dev schema's is `Company`.
        assert_eq!(schema.nodes[0].name, "Company");
        assert_eq!(query.name, "list_company");
        assert!(!query.is_mutation);
        // The compiler names a projected property by the property alone (`slug`, not
        // `c.slug` as the server does), in projection order.
        let names: Vec<&str> = query.columns.iter().map(|c| c.name.as_str()).collect();
        let returned = text.split("return { ").nth(1).unwrap();
        let returned: Vec<&str> = returned[..returned.find(" }").unwrap()]
            .split(", ")
            .map(|c| c.trim_start_matches("$c."))
            .collect();
        assert_eq!(names, returned);
        assert_eq!(names[..2], ["slug", "name"]);
        assert_eq!(snake_case("WorksAt"), "works_at");
    }

    #[test]
    fn node_pages_compile_with_and_without_filters() {
        let schema = dev();
        assert_compiles(&node_page(&schema, "Person", &[], None, &PageStart::First, 100).unwrap());
        let filters = [
            Filter {
                property: "title".into(),
                op: FilterOp::Eq,
                value: json!("Engineer"),
            },
            Filter {
                property: "email".into(),
                op: FilterOp::IsNotEmpty,
                value: Value::Null,
            },
            Filter {
                property: "born".into(),
                op: FilterOp::Gt,
                value: json!("1980-01-01"),
            },
        ];
        let page = node_page(
            &schema,
            "Person",
            &filters,
            None,
            &PageStart::After {
                id: "person-000010".into(),
                value: Value::Null,
            },
            50,
        )
        .unwrap();
        assert_compiles(&page);
        assert_eq!(page.params["after"], json!("person-000010"));
        assert_compiles(&node_count(&schema, "Person", &filters).unwrap());
    }

    #[test]
    fn list_filters_take_one_element() {
        let schema = dev();
        let tags = schema.node("Person").unwrap().property("tags").unwrap();
        assert_eq!(
            FilterOp::for_property(tags),
            vec![FilterOp::Contains, FilterOp::IsEmpty, FilterOp::IsNotEmpty]
        );
        let filters = [Filter {
            property: "tags".into(),
            op: FilterOp::Contains,
            value: json!("graphs"),
        }];
        let page = node_page(&schema, "Person", &filters, None, &PageStart::First, 100).unwrap();
        assert!(page.text.contains("$f0: String"), "{}", page.text);
        assert_compiles(&page);
        assert_compiles(&node_count(&schema, "Person", &filters).unwrap());
    }

    #[test]
    fn every_offered_filter_op_compiles() {
        let schema = dev();
        for node in &schema.nodes {
            for property in node.properties.iter().filter(|p| p.projectable()) {
                for op in FilterOp::for_property(property) {
                    let value = match property.kind {
                        crate::ScalarKind::Integer => json!(1),
                        crate::ScalarKind::Float => json!(1.5),
                        crate::ScalarKind::Bool => json!(true),
                        crate::ScalarKind::Date => json!("2020-01-01"),
                        _ => json!("x"),
                    };
                    let filters = [Filter {
                        property: property.name.clone(),
                        op,
                        value,
                    }];
                    assert_compiles(
                        &node_page(&schema, &node.name, &filters, None, &PageStart::First, 10)
                            .unwrap(),
                    );
                }
            }
        }
        let name = schema.node("Person").unwrap().property("name").unwrap();
        assert!(!FilterOp::for_property(name).contains(&FilterOp::IsEmpty));
    }

    #[test]
    fn node_by_id_compiles() {
        let schema = dev();
        let query = node_by_id(&schema, "Person", "person-000000").unwrap();
        assert_compiles(&query);
        assert_eq!(query.params["id"], json!("person-000000"));
    }

    #[test]
    fn the_head_probe_compiles_and_reads_no_rows() {
        let query = head_probe(&dev()).unwrap();
        assert_compiles(&query);
        assert!(query.text.contains("limit 0"), "{}", query.text);
        assert!(query.params.is_empty());
    }

    #[test]
    fn edge_pages_and_counts_compile() {
        let schema = dev();
        for edge in &schema.edges {
            assert_compiles(
                &edge_page(&schema, &edge.name, &[], None, &PageStart::First, 100).unwrap(),
            );
            assert_compiles(&edge_count(&schema, &edge.name, &[]).unwrap());
        }
        let filters = [
            Filter {
                property: "role".into(),
                op: FilterOp::Eq,
                value: json!("lead"),
            },
            Filter {
                property: "since".into(),
                op: FilterOp::IsNotEmpty,
                value: Value::Null,
            },
        ];
        let page = edge_page(
            &schema,
            "WorksAt",
            &filters,
            None,
            &PageStart::After {
                id: "01J".into(),
                value: Value::Null,
            },
            50,
        )
        .unwrap();
        assert_compiles(&page);
        assert!(page.text.contains("$e.role as e_role"), "{}", page.text);
        assert_compiles(&edge_count(&schema, "WorksAt", &filters).unwrap());
        assert!(matches!(
            edge_page(&schema, "Nope", &[], None, &PageStart::First, 1),
            Err(BuildError::UnknownEdge(_))
        ));
    }

    #[test]
    fn relationship_queries_compile_in_both_directions() {
        let schema = dev();
        for group in schema.relationships_of("Person") {
            assert_compiles(&degree(&schema, "Person", &group, "person-000000").unwrap());
            let page = neighbours(
                &schema,
                "Person",
                &group,
                "person-000000",
                None,
                Narrow::default(),
                50,
            )
            .unwrap();
            assert_compiles(&page);
            // Each neighbour comes with the edge that relates it, which removing it names.
            assert!(page.text.contains("$e.@id as edge_id"), "{}", page.text);
        }
        for group in schema.relationships_of("Company") {
            let search = Narrow {
                search: Some("ada"),
                value: None,
            };
            assert_compiles(
                &neighbours(
                    &schema,
                    "Company",
                    &group,
                    "company-00000",
                    None,
                    search,
                    50,
                )
                .unwrap(),
            );
        }
    }

    #[test]
    fn neighbours_group_by_a_property_of_either_end_and_narrow_to_one_value() {
        let schema = dev();
        for node_type in ["Person", "Company", "Project"] {
            for group in schema.relationships_of(node_type) {
                for by in GroupBy::choices(&schema, &group) {
                    let values =
                        neighbour_values(&schema, node_type, &group, "x", &by, 100).unwrap();
                    assert_compiles(&values);
                    assert!(values.text.contains("order { n desc, value asc }"));
                    let related = by.on == GroupOn::Relationship;
                    if related {
                        let without =
                            neighbours_without(&schema, node_type, &group, "x", &by).unwrap();
                        assert_compiles(&without);
                    }
                    for value in [json!(null), sample_value(&schema, &group, &by)] {
                        let narrow = Narrow {
                            search: Some("a"),
                            value: Some((&by, &value)),
                        };
                        let page =
                            neighbours(&schema, node_type, &group, "x", None, narrow, 50).unwrap();
                        assert_compiles(&page);
                        let none = if related { "not {" } else { "is null" };
                        assert_eq!(page.text.contains(none), value.is_null(), "{}", page.text);
                    }
                }
            }
        }
        let works_at = schema
            .relationships_of("Company")
            .into_iter()
            .find(|group| group.edge == "WorksAt")
            .unwrap();
        let choices: Vec<String> = GroupBy::choices(&schema, &works_at)
            .iter()
            .map(|by| format!("{:?} {}", by.on, by.property))
            .collect();
        assert_eq!(
            choices,
            [
                "Node name",
                "Node title",
                "Node email",
                "Node born",
                "Edge role",
                "Edge since",
                "Relationship out:Contributes",
                "Relationship out:Knows",
                "Relationship out:WorksAt",
                "Relationship in:Knows"
            ],
            "the key, lists, and vectors are left out"
        );
        let by_company = GroupBy {
            on: GroupOn::Relationship,
            property: "out:WorksAt".into(),
        };
        let knows = schema
            .relationships_of("Person")
            .into_iter()
            .find(|group| group.edge == "Knows")
            .unwrap();
        let values = neighbour_values(&schema, "Person", &knows, "x", &by_company, 10).unwrap();
        assert!(values.text.contains("$m worksAt $v"), "{}", values.text);
        assert!(values.text.contains("$v.name as label"), "{}", values.text);
        assert_eq!(by_company.label(), "worksAt →");
        let elsewhere = GroupBy {
            on: GroupOn::Relationship,
            property: "out:Owns".into(),
        };
        assert!(
            neighbour_values(&schema, "Person", &knows, "x", &elsewhere, 10).is_err(),
            "a relationship of another type"
        );
        let key = GroupBy {
            on: GroupOn::Node,
            property: "slug".into(),
        };
        assert!(neighbour_values(&schema, "Company", &works_at, "x", &key, 10).is_err());
    }

    /// A value of the grouping property's type, as a bucket would hold it.
    fn sample_value(schema: &SchemaModel, group: &RelationshipGroup, by: &GroupBy) -> Value {
        if by.on == GroupOn::Relationship {
            return json!("company-00001");
        }
        let property = by.resolve(schema, group).unwrap();
        match property.kind {
            ScalarKind::Bool => json!(true),
            ScalarKind::Integer => json!(3),
            ScalarKind::Date => json!("2024-01-02"),
            _ => match &property.enum_values {
                Some(values) => json!(values[0]),
                None => json!("Engineer"),
            },
        }
    }

    #[test]
    fn membership_presets_and_relationship_filters_compile() {
        let schema = dev();
        let filters = [
            Filter {
                property: "title".into(),
                op: FilterOp::In,
                value: json!(["Founder", "Engineer"]),
            },
            Filter {
                property: "born".into(),
                op: FilterOp::Last30Days,
                value: json!("2026-09-12"),
            },
            Filter {
                property: "out:WorksAt".into(),
                op: FilterOp::HasAny,
                value: Value::Null,
            },
            Filter {
                property: "in:Knows".into(),
                op: FilterOp::HasNone,
                value: Value::Null,
            },
            Filter {
                property: "out:Contributes".into(),
                op: FilterOp::RelatedTo,
                value: json!("project-00036"),
            },
        ];
        let page = node_page(&schema, "Person", &filters, None, &PageStart::First, 100).unwrap();
        assert_compiles(&page);
        assert!(page.text.contains("$n.title in $f0"), "{}", page.text);
        assert!(page.text.contains("$n.born >= $f1"), "{}", page.text);
        assert!(page.text.contains("exists {"), "{}", page.text);
        assert!(
            page.text
                .contains("not {\n      $r3: Person\n      $r3 knows $n"),
            "{}",
            page.text
        );
        assert!(page.text.contains("$r4.@id = $f4"), "{}", page.text);
        assert_compiles(&node_count(&schema, "Person", &filters).unwrap());
        assert_eq!(relationship_label("in:Knows").as_deref(), Some("← knows"));
        assert!(matches!(
            node_page(
                &schema,
                "Person",
                &[Filter {
                    property: "out:Likes".into(),
                    op: FilterOp::HasAny,
                    value: Value::Null,
                }],
                None,
                &PageStart::First,
                10
            ),
            Err(BuildError::UnknownEdge(_))
        ));
    }

    #[test]
    fn node_search_compiles_for_every_type() {
        let schema = dev();
        for node in &schema.nodes {
            let query = node_search(&schema, &node.name, "ada", 5).unwrap();
            assert_compiles(&query);
            assert_eq!(query.params["q0"], json!("ada"));
            assert_eq!(query.params["q1"], json!("Ada"));
        }
        let person = node_search(&schema, "Person", "ada", 5).unwrap();
        assert!(
            person.text.contains("$n.name contains $q0"),
            "{}",
            person.text
        );
    }

    #[test]
    fn longer_search_text_matches_among_what_shorter_text_matched() {
        assert!(search_matches("ada-lovelace", None, "ada"));
        assert!(search_matches("p-1", Some("Ada Lovelace"), "ada lov"));
        assert!(search_matches("p-1", Some("Ada Lovelace"), "Lovelace"));
        assert!(!search_matches("p-1", Some("Ada Lovelace"), "grace"));
        assert!(!search_matches("p-1", None, "ada"));

        assert!(search_narrows("ad", "ada"));
        assert!(search_narrows("ada", "ada lov"));
        assert!(search_narrows("Ada", "Ada L"));
        assert!(!search_narrows("ada", "ad"), "shorter text can match more");
        assert!(!search_narrows("ada", "grace"));
        assert!(!search_narrows("", "ada"));
        // "ΑΣ" lowercases to "ας" but "ΑΣΑ" to "ασα": the lowercase spelling does not grow,
        // so the narrower search asks again.
        assert!(!search_narrows("ΑΣ", "ΑΣΑ"));
    }

    #[test]
    fn node_label_compiles_for_every_type() {
        let schema = dev();
        for node in &schema.nodes {
            let query = node_label(&schema, &node.name, "x").unwrap();
            assert_compiles(&query);
            assert!(query.text.contains("$n.name as label"), "{}", query.text);
        }
        assert!(matches!(
            node_label(&schema, "Nope", "x"),
            Err(BuildError::UnknownType(_))
        ));
    }

    #[test]
    fn node_label_without_a_label_property_returns_only_the_key() {
        let schema = SchemaModel::parse("node Tag {\n  slug: String @key\n}\n").unwrap();
        let query = node_label(&schema, "Tag", "t").unwrap();
        assert!(!query.text.contains("as label"), "{}", query.text);
        let result = check(&schema, &query.text);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    }

    #[test]
    fn writes_compile() {
        let schema = dev();
        assert_compiles(
            &update_cell(
                &schema,
                "Person",
                "title",
                "person-000000",
                &json!("Founder"),
                &json!("CEO"),
            )
            .unwrap(),
        );
        assert_compiles(
            &update_cell(
                &schema,
                "Person",
                "title",
                "person-000000",
                &Value::Null,
                &json!("CEO"),
            )
            .unwrap(),
        );
        assert_compiles(
            &update_cell(
                &schema,
                "Person",
                "tags",
                "person-000000",
                &json!(["a"]),
                &json!(["b"]),
            )
            .unwrap(),
        );
        assert_compiles(
            &update_cell(
                &schema,
                "Project",
                "status",
                "project-00000",
                &json!("active"),
                &json!("done"),
            )
            .unwrap(),
        );
        assert_compiles(
            &insert_node(
                &schema,
                "Person",
                &[
                    ("slug".into(), json!("person-new")),
                    ("name".into(), json!("New Person")),
                ],
            )
            .unwrap(),
        );
        assert_compiles(&delete_node(&schema, "Person", "person-new").unwrap());
        assert_compiles(
            &insert_edge(
                &schema,
                "WorksAt",
                "person-000000",
                "company-00002",
                &[
                    ("role".into(), json!("Engineer")),
                    ("since".into(), json!("2020-01-01")),
                ],
            )
            .unwrap(),
        );
        assert_compiles(
            &insert_edge(&schema, "Knows", "person-000000", "person-000001", &[]).unwrap(),
        );
        assert_compiles(&delete_edge(&schema, "Knows", "edge-1").unwrap());
        assert!(matches!(
            insert_edge(&schema, "Knows", "a", "b", &[("role".into(), json!("x"))]),
            Err(BuildError::UnknownProperty { .. })
        ));
        assert!(matches!(
            update_cell(&schema, "Person", "slug", "x", &json!("a"), &json!("b")),
            Err(BuildError::NotEditable(_))
        ));
    }
}

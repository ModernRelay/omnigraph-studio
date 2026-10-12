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

/// A comparison on one property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Contains,
    IsEmpty,
    IsNotEmpty,
}

impl FilterOp {
    pub fn label(self) -> &'static str {
        match self {
            FilterOp::Eq => "is",
            FilterOp::Ne => "is not",
            FilterOp::Lt => "less than",
            FilterOp::Le => "at most",
            FilterOp::Gt => "greater than",
            FilterOp::Ge => "at least",
            FilterOp::Contains => "contains",
            FilterOp::IsEmpty => "is empty",
            FilterOp::IsNotEmpty => "is not empty",
        }
    }

    pub fn takes_value(self) -> bool {
        !matches!(self, FilterOp::IsEmpty | FilterOp::IsNotEmpty)
    }

    /// The comparisons a property supports, in menu order. Lists take membership only (the
    /// engine refuses list equality); `is empty` needs a nullable property.
    pub fn for_property(property: &Property) -> Vec<FilterOp> {
        let mut ops = if property.list {
            vec![FilterOp::Contains]
        } else {
            match property.kind {
                ScalarKind::Text if property.enum_values.is_none() => {
                    vec![FilterOp::Eq, FilterOp::Ne, FilterOp::Contains]
                }
                ScalarKind::Text | ScalarKind::Bool => vec![FilterOp::Eq, FilterOp::Ne],
                ScalarKind::Integer
                | ScalarKind::Float
                | ScalarKind::Date
                | ScalarKind::DateTime => {
                    vec![
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
    type_name: &str,
    properties: &[Property],
    var: &str,
    filters: &[Filter],
    params: &mut Params,
) -> Result<Vec<String>, BuildError> {
    let mut clauses = Vec::new();
    for (i, filter) in filters.iter().enumerate() {
        let property = properties
            .iter()
            .find(|p| p.name == filter.property)
            .ok_or_else(|| BuildError::UnknownProperty {
                type_name: type_name.to_owned(),
                property: filter.property.clone(),
            })?;
        let target = format!("${var}.{}", property.name);
        let clause = match filter.op {
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
                let param = params.add(&format!("f{i}"), &ty, filter.value.clone());
                let operator = match op {
                    FilterOp::Eq => "=",
                    FilterOp::Ne => "!=",
                    FilterOp::Lt => "<",
                    FilterOp::Le => "<=",
                    FilterOp::Gt => ">",
                    FilterOp::Ge => ">=",
                    FilterOp::Contains => "contains",
                    FilterOp::IsEmpty | FilterOp::IsNotEmpty => unreachable!(),
                };
                format!("{target} {operator} {param}")
            }
        };
        clauses.push(clause);
    }
    Ok(clauses)
}

/// One page of a node type, keyset-paged by `@id` (spec 004). Pass the last row's `@id` as
/// `after` for the next page.
pub fn node_page(
    schema: &SchemaModel,
    type_name: &str,
    filters: &[Filter],
    after: Option<&str>,
    limit: u32,
) -> Result<BuiltQuery, BuildError> {
    let node = node(schema, type_name)?;
    let mut params = Params::new();
    let after = params.add(
        "after",
        "String",
        Value::String(after.unwrap_or("").to_owned()),
    );
    let mut clauses = vec![format!("$n: {}", node.name), format!("$n.@id > {after}")];
    clauses.extend(filter_clauses(
        &node.name,
        &node.properties,
        "n",
        filters,
        &mut params,
    )?);
    let text = format!(
        "query studio_page({sig}) {{\n  match {{\n    {clauses}\n  }}\n  return {{ $n }}\n  order {{ $n.@id asc }}\n  limit {limit}\n}}\n",
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
    after: Option<&str>,
    limit: u32,
) -> Result<BuiltQuery, BuildError> {
    let edge = edge(schema, edge_name)?;
    let mut params = Params::new();
    let after = params.add(
        "after",
        "String",
        Value::String(after.unwrap_or("").to_owned()),
    );
    let mut clauses = vec![
        format!("$a: {}", edge.from),
        format!("$a $e:{} $b", edge.traversal()),
        format!("$e.@id > {after}"),
    ];
    clauses.extend(filter_clauses(
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
        "query studio_edge_page({sig}) {{\n  match {{\n    {clauses}\n  }}\n  return {{ {columns} }}\n  order {{ $e.@id asc }}\n  limit {limit}\n}}\n",
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

/// One page of a node's neighbours through a relationship group, with the neighbour's label
/// and the edge's properties (spec 005). Columns: `id`, `label` (when the other type has one),
/// then `e_<property>` for each projectable edge property.
pub fn neighbours(
    schema: &SchemaModel,
    node_type: &str,
    group: &RelationshipGroup,
    id: &str,
    after: Option<&str>,
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
    let mut columns = vec!["$m.@id as id".to_owned()];
    if let Some(label) = other.label_property()
        && Some(label) != other.key.as_deref()
    {
        columns.push(format!("$m.{label} as label"));
    }
    for property in edge.properties.iter().filter(|p| p.projectable()) {
        columns.push(format!("$e.{0} as e_{0}", property.name));
    }
    let text = format!(
        "query studio_neighbours({sig}) {{\n  match {{\n    $n: {ty}\n    $n.@id = {id}\n    {traversal}\n    $m.@id > {after}\n  }}\n  return {{ {columns} }}\n  order {{ $m.@id asc }}\n  limit {limit}\n}}\n",
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
    fn node_pages_compile_with_and_without_filters() {
        let schema = dev();
        assert_compiles(&node_page(&schema, "Person", &[], None, 100).unwrap());
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
        let page = node_page(&schema, "Person", &filters, Some("person-000010"), 50).unwrap();
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
        let page = node_page(&schema, "Person", &filters, None, 100).unwrap();
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
                    assert_compiles(&node_page(&schema, &node.name, &filters, None, 10).unwrap());
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
    fn edge_pages_and_counts_compile() {
        let schema = dev();
        for edge in &schema.edges {
            assert_compiles(&edge_page(&schema, &edge.name, &[], None, 100).unwrap());
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
        let page = edge_page(&schema, "WorksAt", &filters, Some("01J"), 50).unwrap();
        assert_compiles(&page);
        assert!(page.text.contains("$e.role as e_role"), "{}", page.text);
        assert_compiles(&edge_count(&schema, "WorksAt", &filters).unwrap());
        assert!(matches!(
            edge_page(&schema, "Nope", &[], None, 1),
            Err(BuildError::UnknownEdge(_))
        ));
    }

    #[test]
    fn relationship_queries_compile_in_both_directions() {
        let schema = dev();
        for group in schema.relationships_of("Person") {
            assert_compiles(&degree(&schema, "Person", &group, "person-000000").unwrap());
            assert_compiles(
                &neighbours(&schema, "Person", &group, "person-000000", None, 50).unwrap(),
            );
        }
        for group in schema.relationships_of("Company") {
            assert_compiles(
                &neighbours(&schema, "Company", &group, "company-00000", None, 50).unwrap(),
            );
        }
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
        assert!(matches!(
            update_cell(&schema, "Person", "slug", "x", &json!("a"), &json!("b")),
            Err(BuildError::NotEditable(_))
        ));
    }
}

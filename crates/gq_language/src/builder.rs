//! Every query Studio generates, in one place (spec 004, 005).
//!
//! Each builder returns GQ text and its JSON parameters. Values always travel as parameters,
//! never spliced into the text. The tests type-check every builder's output with
//! `omnigraph-compiler` against the dev schema.

use serde_json::{Map, Value};

use crate::schema::{
    Direction, NodeType, Property, RelationshipGroup, SchemaModel, traversal_name,
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
    node: &NodeType,
    var: &str,
    filters: &[Filter],
    params: &mut Params,
) -> Result<Vec<String>, BuildError> {
    let mut clauses = Vec::new();
    for (i, filter) in filters.iter().enumerate() {
        let property = property(node, &filter.property)?;
        let target = format!("${var}.{}", property.name);
        let clause = match filter.op {
            FilterOp::IsEmpty => format!("{target} is null"),
            FilterOp::IsNotEmpty => format!("{target} is not null"),
            op => {
                let param = params.add(
                    &format!("f{i}"),
                    &property.param_type(false),
                    filter.value.clone(),
                );
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
    clauses.extend(filter_clauses(node, "n", filters, &mut params)?);
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
    clauses.extend(filter_clauses(node, "n", filters, &mut params)?);
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
        assert!(matches!(
            update_cell(&schema, "Person", "slug", "x", &json!("a"), &json!("b")),
            Err(BuildError::NotEditable(_))
        ));
    }
}

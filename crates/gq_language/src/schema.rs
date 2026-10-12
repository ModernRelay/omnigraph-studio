//! The schema model every surface reads: node and edge types, their properties, keys, and the
//! relationship groups a node type takes part in. Built from `.pg` source with
//! `omnigraph-compiler`, so Studio reads the schema exactly as the engine does.

use std::sync::Arc;

use omnigraph_compiler::catalog::Catalog;
use omnigraph_compiler::schema::ast::{Constraint, SchemaDecl, SchemaFile};
use omnigraph_compiler::schema::parser::parse_schema;
use omnigraph_compiler::types::{PropType, ScalarType};
use omnigraph_compiler::{
    PropertyConstraintShape, PropertyShape, build_catalog, compile_schema_shape,
};

/// The scalar family of a property, for alignment, editing and parameter types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarKind {
    Text,
    Bool,
    Integer,
    Float,
    Date,
    DateTime,
    Vector,
    Blob,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Property {
    pub name: String,
    pub kind: ScalarKind,
    /// The GQ spelling of the base type: `String`, `I32`, `Date`, `Vector(3)`, ….
    pub base_type: String,
    pub nullable: bool,
    pub list: bool,
    pub enum_values: Option<Vec<String>>,
    pub is_key: bool,
}

impl Property {
    /// The GQ parameter type for a value of this property, for example `String?` or `[I32]`.
    pub fn param_type(&self, nullable: bool) -> String {
        let base = if self.enum_values.is_some() {
            "String".to_owned()
        } else {
            self.base_type.clone()
        };
        let ty = if self.list { format!("[{base}]") } else { base };
        if nullable { format!("{ty}?") } else { ty }
    }

    /// Shown in table headers and the inspector: `String?`, `[String]`, `enum(a, b)`.
    pub fn display_type(&self) -> String {
        let base = match &self.enum_values {
            Some(values) => format!("enum({})", values.join(", ")),
            None => self.base_type.clone(),
        };
        let ty = if self.list { format!("[{base}]") } else { base };
        if self.nullable { format!("{ty}?") } else { ty }
    }

    /// Whether a table can edit this property in a cell.
    pub fn editable_in_cell(&self) -> bool {
        !self.is_key && !matches!(self.kind, ScalarKind::Vector | ScalarKind::Blob)
    }

    /// Whether a query can return this property (Blob and Vector values are read separately).
    pub fn projectable(&self) -> bool {
        !matches!(self.kind, ScalarKind::Vector | ScalarKind::Blob)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeType {
    pub name: String,
    pub properties: Vec<Property>,
    /// The `@key` property, if the type declares a single one.
    pub key: Option<String>,
}

impl NodeType {
    pub fn property(&self, name: &str) -> Option<&Property> {
        self.properties.iter().find(|p| p.name == name)
    }

    /// The property shown as a row's label: `name`, `title` or `display_name` when present,
    /// else the key.
    pub fn label_property(&self) -> Option<&str> {
        for candidate in ["name", "title", "display_name", "label"] {
            if let Some(property) = self.property(candidate)
                && matches!(property.kind, ScalarKind::Text)
                && !property.list
            {
                return Some(&property.name);
            }
        }
        self.key.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EdgeType {
    pub name: String,
    pub from: String,
    pub to: String,
    /// The edge allows at most one target per source (`@card(..1)`).
    pub at_most_one: bool,
    pub properties: Vec<Property>,
}

impl EdgeType {
    /// The traversal spelling: the edge name with a lowercase first letter (`worksAt`).
    pub fn traversal(&self) -> String {
        traversal_name(&self.name)
    }
}

/// Which way a relationship group points from the node being looked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    Outgoing,
    Incoming,
}

/// One edge type, seen from one node type: the rows under a node in the relationship tree and
/// the hubs on the canvas.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RelationshipGroup {
    pub edge: String,
    pub direction: Direction,
    pub other_type: String,
}

/// A compiled schema.
#[derive(Clone)]
pub struct SchemaModel {
    pub source: Arc<str>,
    pub nodes: Vec<NodeType>,
    pub edges: Vec<EdgeType>,
    pub(crate) catalog: Arc<Catalog>,
}

impl std::fmt::Debug for SchemaModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchemaModel")
            .field("nodes", &self.nodes.len())
            .field("edges", &self.edges.len())
            .finish()
    }
}

impl SchemaModel {
    /// Compiles `.pg` source. The error is the compiler's message.
    pub fn parse(source: &str) -> Result<Self, String> {
        let file = parse_schema(source).map_err(|e| e.to_string())?;
        let shape = compile_schema_shape(&file).map_err(|e| e.to_string())?;
        let catalog = build_catalog(&file).map_err(|e| e.to_string())?;

        let nodes = shape
            .nodes
            .iter()
            .map(|node| {
                let keys: Vec<String> = node
                    .constraints
                    .iter()
                    .filter_map(|c| match c {
                        Constraint::Key(names) if names.len() == 1 => Some(names[0].clone()),
                        _ => None,
                    })
                    .collect();
                let mut properties: Vec<Property> =
                    node.properties.iter().map(|p| property(p, &keys)).collect();
                in_declared_order(&file, &node.name, &mut properties);
                let key = properties.iter().find(|p| p.is_key).map(|p| p.name.clone());
                NodeType {
                    name: node.name.clone(),
                    properties,
                    key,
                }
            })
            .collect();
        let edges = shape
            .edges
            .iter()
            .map(|edge| {
                let mut properties: Vec<Property> =
                    edge.properties.iter().map(|p| property(p, &[])).collect();
                in_declared_order(&file, &edge.name, &mut properties);
                EdgeType {
                    name: edge.name.clone(),
                    from: edge.from_type.clone(),
                    to: edge.to_type.clone(),
                    at_most_one: edge.cardinality.max == Some(1),
                    properties,
                }
            })
            .collect();
        Ok(Self {
            source: source.into(),
            nodes,
            edges,
            catalog: Arc::new(catalog),
        })
    }

    pub fn node(&self, name: &str) -> Option<&NodeType> {
        self.nodes.iter().find(|n| n.name == name)
    }

    pub fn edge(&self, name: &str) -> Option<&EdgeType> {
        self.edges.iter().find(|e| e.name == name)
    }

    /// The relationship groups of a node type, outgoing first, each in schema order.
    pub fn relationships_of(&self, node_type: &str) -> Vec<RelationshipGroup> {
        let mut groups = Vec::new();
        for edge in &self.edges {
            if edge.from == node_type {
                groups.push(RelationshipGroup {
                    edge: edge.name.clone(),
                    direction: Direction::Outgoing,
                    other_type: edge.to.clone(),
                });
            }
        }
        for edge in &self.edges {
            if edge.to == node_type {
                groups.push(RelationshipGroup {
                    edge: edge.name.clone(),
                    direction: Direction::Incoming,
                    other_type: edge.from.clone(),
                });
            }
        }
        groups
    }
}

/// The compiled shape lists properties by name; tables and forms show them as the `.pg` file
/// declares them. Properties a type inherits from an interface follow its own, in the
/// interface's order.
fn in_declared_order(file: &SchemaFile, type_name: &str, properties: &mut [Property]) {
    let mut declared: Vec<&str> = Vec::new();
    let mut interfaces: Vec<&str> = Vec::new();
    for decl in &file.declarations {
        match decl {
            SchemaDecl::Node(node) if node.name == type_name => {
                declared.extend(node.properties.iter().map(|p| p.name.as_str()));
                interfaces.extend(node.implements.iter().map(String::as_str));
            }
            SchemaDecl::Edge(edge) if edge.name == type_name => {
                declared.extend(edge.properties.iter().map(|p| p.name.as_str()));
            }
            _ => {}
        }
    }
    for decl in &file.declarations {
        if let SchemaDecl::Interface(interface) = decl
            && interfaces.contains(&interface.name.as_str())
        {
            declared.extend(interface.properties.iter().map(|p| p.name.as_str()));
        }
    }
    properties.sort_by_key(|p| {
        declared
            .iter()
            .position(|name| *name == p.name)
            .unwrap_or(usize::MAX)
    });
}

/// `WorksAt` → `worksAt`.
pub fn traversal_name(edge: &str) -> String {
    let mut chars = edge.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn property(shape: &PropertyShape, keys: &[String]) -> Property {
    let PropType {
        scalar,
        nullable,
        list,
        enum_values,
    } = &shape.prop_type;
    let (kind, base_type) = match scalar {
        ScalarType::String => (ScalarKind::Text, "String".to_owned()),
        ScalarType::Bool => (ScalarKind::Bool, "Bool".to_owned()),
        ScalarType::I32 => (ScalarKind::Integer, "I32".to_owned()),
        ScalarType::I64 => (ScalarKind::Integer, "I64".to_owned()),
        ScalarType::U32 => (ScalarKind::Integer, "U32".to_owned()),
        ScalarType::U64 => (ScalarKind::Integer, "U64".to_owned()),
        ScalarType::F32 => (ScalarKind::Float, "F32".to_owned()),
        ScalarType::F64 => (ScalarKind::Float, "F64".to_owned()),
        ScalarType::Date => (ScalarKind::Date, "Date".to_owned()),
        ScalarType::DateTime => (ScalarKind::DateTime, "DateTime".to_owned()),
        ScalarType::Vector(n) => (ScalarKind::Vector, format!("Vector({n})")),
        ScalarType::Blob => (ScalarKind::Blob, "Blob".to_owned()),
    };
    let is_key = keys.contains(&shape.name)
        || shape
            .property_constraints
            .iter()
            .any(|c| matches!(c, PropertyConstraintShape::Key));
    Property {
        name: shape.name.clone(),
        kind,
        base_type,
        nullable: *nullable,
        list: *list,
        enum_values: enum_values.clone(),
        is_key,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const DEV_SCHEMA: &str = include_str!("../../../dev/cluster/knowledge.pg");

    pub fn dev() -> SchemaModel {
        SchemaModel::parse(DEV_SCHEMA).expect("dev schema compiles")
    }

    #[test]
    fn reads_types_keys_and_properties() {
        let schema = dev();
        let person = schema.node("Person").unwrap();
        assert_eq!(person.key.as_deref(), Some("slug"));
        assert_eq!(person.label_property(), Some("name"));
        let tags = person.property("tags").unwrap();
        assert!(tags.list && tags.nullable);
        assert_eq!(tags.display_type(), "[String]?");
        let status = schema.node("Project").unwrap().property("status").unwrap();
        assert_eq!(status.enum_values.as_deref().unwrap().len(), 3);
        assert_eq!(status.param_type(false), "String");
    }

    #[test]
    fn properties_keep_their_declared_order() {
        let schema = dev();
        let names: Vec<_> = schema
            .node("Person")
            .unwrap()
            .properties
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, ["slug", "name", "title", "email", "born", "tags"]);
        let edge: Vec<_> = schema
            .edge("WorksAt")
            .unwrap()
            .properties
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(edge, ["role", "since"]);
    }

    #[test]
    fn relationship_groups_cover_both_directions() {
        let schema = dev();
        let groups = schema.relationships_of("Person");
        assert!(groups.contains(&RelationshipGroup {
            edge: "WorksAt".into(),
            direction: Direction::Outgoing,
            other_type: "Company".into(),
        }));
        assert!(groups.contains(&RelationshipGroup {
            edge: "Knows".into(),
            direction: Direction::Incoming,
            other_type: "Person".into(),
        }));
        assert_eq!(traversal_name("WorksAt"), "worksAt");
    }
}

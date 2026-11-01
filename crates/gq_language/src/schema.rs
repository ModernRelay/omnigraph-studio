//! The schema model every surface reads: node and edge types, their properties, keys, and the
//! relationship groups a node type takes part in. Built from `.pg` source with
//! `omnigraph-compiler`, so Studio reads the schema exactly as the engine does.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use omnigraph_compiler::catalog::Catalog;
use omnigraph_compiler::schema::ast::{Annotation, Constraint, SchemaDecl, SchemaFile};
use omnigraph_compiler::schema::parser::parse_persisted_schema_contract;
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
    /// Declared `@index`: the engine's full-text `search` works on it.
    pub indexed: bool,
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

    /// Whether the property can name a node: one text value.
    pub fn can_name(&self) -> bool {
        matches!(self.kind, ScalarKind::Text) && !self.list
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
    /// The property the person chose to name this type's nodes by, for this graph (spec 002,
    /// "Node names"); [`NodeType::label_property`] takes it when it can name a node.
    pub label: Option<String>,
    /// What the schema says the type is ([`about`]).
    pub about: Option<String>,
}

impl NodeType {
    pub fn property(&self, name: &str) -> Option<&Property> {
        self.properties.iter().find(|p| p.name == name)
    }

    /// The property that names a node wherever Studio shows one (a canvas chip, a tree row,
    /// the inspector's title, a relationship cell): the one chosen for this graph when it can
    /// name a node, else [`NodeType::default_label_property`].
    pub fn label_property(&self) -> Option<&str> {
        match self.label.as_deref() {
            Some(chosen) if self.property(chosen).is_some_and(Property::can_name) => Some(chosen),
            _ => self.default_label_property(),
        }
    }

    /// The property that names a node when none is chosen: `name`, `title`, `display_name`
    /// or `label` when present, else the key.
    pub fn default_label_property(&self) -> Option<&str> {
        for candidate in ["name", "title", "display_name", "label"] {
            if let Some(property) = self.property(candidate)
                && property.can_name()
            {
                return Some(&property.name);
            }
        }
        self.key.as_deref()
    }

    /// The properties a node can be named by, in schema order: its single text values.
    pub fn label_candidates(&self) -> impl Iterator<Item = &Property> {
        self.properties.iter().filter(|p| p.can_name())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EdgeType {
    pub name: String,
    pub from: String,
    pub to: String,
    /// The edge allows at most one target per source (`@card(..1)`).
    pub at_most_one: bool,
    /// Each source needs at least this many of these edges (`@card(n..)`); 0 when optional.
    /// Deleting the targets a source needs refuses the delete, or takes the source with it
    /// (`builder::required_dependents`).
    pub min: u32,
    pub properties: Vec<Property>,
    /// What the schema says the relationship is ([`about`]).
    pub about: Option<String>,
}

impl EdgeType {
    /// How a query names the edge in a traversal ([`traversal_token`]).
    pub fn traversal(&self) -> String {
        traversal_token(&self.name)
    }
}

/// Which way a relationship group points from the node being looked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    Outgoing,
    Incoming,
}

impl Direction {
    /// The same edges seen from their other end.
    pub fn reversed(self) -> Self {
        match self {
            Self::Outgoing => Self::Incoming,
            Self::Incoming => Self::Outgoing,
        }
    }

    /// The arrow a group is drawn with, pointing the way its edges run from this end.
    pub fn arrow(self) -> &'static str {
        match self {
            Self::Outgoing => "→",
            Self::Incoming => "←",
        }
    }
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
    /// Where each node and edge type is, by name, and each node type's relationship groups:
    /// views look these up every frame, and a schema can hold hundreds of edge types.
    node_index: HashMap<String, usize>,
    edge_index: HashMap<String, usize>,
    relationships: HashMap<String, Arc<[RelationshipGroup]>>,
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
    /// Compiles `.pg` source the server accepted. The error is the compiler's message.
    ///
    /// Read as the engine reads an accepted schema (`parse_persisted_schema_contract`, its
    /// compatibility parser for stored contracts): a graph admitted before a rule tightened
    /// (an integer `@range` bound past I64, a body-level `@unique` on a Blob) still opens, where
    /// the admission parser would refuse it and leave the graph blank.
    pub fn parse(source: &str) -> Result<Self, String> {
        let file = parse_persisted_schema_contract(source).map_err(|e| e.to_string())?;
        let shape = compile_schema_shape(&file).map_err(|e| e.to_string())?;
        let catalog = build_catalog(&file).map_err(|e| e.to_string())?;

        let nodes: Vec<NodeType> = shape
            .nodes
            .iter()
            .map(|node| {
                let keys = key_members(&node.constraints);
                let mut properties: Vec<Property> =
                    node.properties.iter().map(|p| property(p, &keys)).collect();
                in_declared_order(&file, &node.name, &mut properties);
                // The node's id is its key's value only when the key is one property.
                let composite = composite_members(&node.constraints);
                let key = properties
                    .iter()
                    .find(|p| p.is_key && !composite.contains(&p.name))
                    .map(|p| p.name.clone());
                NodeType {
                    name: node.name.clone(),
                    properties,
                    key,
                    label: None,
                    about: declared(&file, &node.name).and_then(about),
                }
            })
            .collect();
        let edges: Vec<EdgeType> = shape
            .edges
            .iter()
            .map(|edge| {
                let keys = key_members(&edge.constraints);
                let mut properties: Vec<Property> =
                    edge.properties.iter().map(|p| property(p, &keys)).collect();
                in_declared_order(&file, &edge.name, &mut properties);
                EdgeType {
                    name: edge.name.clone(),
                    from: edge.from_type.clone(),
                    to: edge.to_type.clone(),
                    at_most_one: edge.cardinality.max == Some(1),
                    min: edge.cardinality.min,
                    properties,
                    about: declared(&file, &edge.name).and_then(about),
                }
            })
            .collect();
        let node_index = nodes
            .iter()
            .enumerate()
            .map(|(at, node)| (node.name.clone(), at))
            .collect();
        let edge_index = edges
            .iter()
            .enumerate()
            .map(|(at, edge)| (edge.name.clone(), at))
            .collect();
        let relationships = nodes
            .iter()
            .map(|node| (node.name.clone(), groups_of(&edges, &node.name).into()))
            .collect();
        Ok(Self {
            source: source.into(),
            nodes,
            edges,
            catalog: Arc::new(catalog),
            node_index,
            edge_index,
            relationships,
        })
    }

    pub fn node(&self, name: &str) -> Option<&NodeType> {
        self.nodes.get(*self.node_index.get(name)?)
    }

    /// This schema with each type's nodes named by the property `labels` chooses for it, and
    /// every other type by its default: a graph's choices (spec 002, "Node names"). A choice for
    /// a type or a property the schema does not hold is kept and has no effect.
    pub fn with_labels(&self, labels: &BTreeMap<String, String>) -> Self {
        let mut labelled = self.clone();
        for node in &mut labelled.nodes {
            node.label = labels.get(&node.name).cloned();
        }
        labelled
    }

    /// Whether `other` is this schema with the same node names chosen: what a view built under
    /// one would show under the other.
    pub fn same_as(&self, other: &SchemaModel) -> bool {
        self.source == other.source
            && self
                .nodes
                .iter()
                .map(NodeType::label_property)
                .eq(other.nodes.iter().map(NodeType::label_property))
    }

    /// [`Self::same_as`] for two schemas a view may hold, either absent: none is the same as
    /// none, and the same model is the same without comparing it.
    pub fn same_held(a: Option<&std::sync::Arc<Self>>, b: Option<&std::sync::Arc<Self>>) -> bool {
        match (a, b) {
            (Some(a), Some(b)) => std::sync::Arc::ptr_eq(a, b) || a.same_as(b),
            (None, None) => true,
            _ => false,
        }
    }

    pub fn edge(&self, name: &str) -> Option<&EdgeType> {
        self.edges.get(*self.edge_index.get(name)?)
    }

    /// The relationship groups of a node type, outgoing first, each in schema order; none for
    /// a type the schema does not hold.
    pub fn relationships_of(&self, node_type: &str) -> &[RelationshipGroup] {
        self.relationships
            .get(node_type)
            .map(|groups| &**groups)
            .unwrap_or_default()
    }
}

/// A node type's relationship groups, outgoing first, each in schema order.
fn groups_of(edges: &[EdgeType], node_type: &str) -> Vec<RelationshipGroup> {
    let outgoing = edges
        .iter()
        .filter(|edge| edge.from == node_type)
        .map(|edge| RelationshipGroup {
            edge: edge.name.clone(),
            direction: Direction::Outgoing,
            other_type: edge.to.clone(),
        });
    let incoming = edges
        .iter()
        .filter(|edge| edge.to == node_type)
        .map(|edge| RelationshipGroup {
            edge: edge.name.clone(),
            direction: Direction::Incoming,
            other_type: edge.from.clone(),
        });
    outgoing.chain(incoming).collect()
}

/// The annotations a node or edge type is declared with.
fn declared<'a>(file: &'a SchemaFile, type_name: &str) -> Option<&'a [Annotation]> {
    file.declarations.iter().find_map(|decl| match decl {
        SchemaDecl::Node(node) if node.name == type_name => Some(node.annotations.as_slice()),
        SchemaDecl::Edge(edge) if edge.name == type_name => Some(edge.annotations.as_slice()),
        _ => None,
    })
}

/// What a type is, in the schema's words: its `@description`, else its `@instruction` (the
/// guidance for whoever writes it, which says what it is where no description does).
fn about(annotations: &[Annotation]) -> Option<String> {
    ["description", "instruction"].iter().find_map(|name| {
        annotations
            .iter()
            .find(|annotation| annotation.name == *name)
            .and_then(|annotation| annotation.value.clone())
            .filter(|value| !value.trim().is_empty())
    })
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

/// `WorksAt` → `worksAt`: the edge as a relationship reads.
pub fn traversal_name(edge: &str) -> String {
    let mut chars = edge.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// GQ's reserved words, which cannot name a property or an edge bare.
pub(crate) const RESERVED: [&str; 6] = ["and", "or", "not", "is", "null", "in"];

/// Whether a property can be named bare in a mutation's `where`: not a reserved word, nor a
/// Boolean literal (the compiler reads `true` there as the value).
pub(crate) fn nameable_in_where(name: &str) -> bool {
    !RESERVED.contains(&name) && name != "true" && name != "false"
}

/// How a query names the edge in a traversal: its [`traversal_name`], quoted when that is a
/// reserved word (`In` → `"in"`), as GQ spells such an edge.
pub fn traversal_token(edge: &str) -> String {
    let name = traversal_name(edge);
    if RESERVED.contains(&name.as_str()) {
        format!("\"{name}\"")
    } else {
        name
    }
}

/// The properties named by a type's `@key` constraints, a composite key's members included.
fn key_members(constraints: &[Constraint]) -> Vec<String> {
    constraints
        .iter()
        .filter_map(|c| match c {
            Constraint::Key(names) => Some(names.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

/// The members of a type's keys of more than one property.
fn composite_members(constraints: &[Constraint]) -> Vec<String> {
    constraints
        .iter()
        .filter_map(|c| match c {
            Constraint::Key(names) if names.len() > 1 => Some(names.clone()),
            _ => None,
        })
        .flatten()
        .collect()
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
    // Every member of a key, a composite one's too: the server refuses to change any of them.
    let is_key = keys.contains(&shape.name)
        || shape
            .property_constraints
            .iter()
            .any(|c| matches!(c, PropertyConstraintShape::Key));
    // A one-column `@index` on free text is a full-text index, and a key declares one too
    // (the compiler's T27).
    let indexed = is_key
        || shape
            .property_constraints
            .iter()
            .any(|c| matches!(c, PropertyConstraintShape::Index));
    Property {
        name: shape.name.clone(),
        kind,
        base_type,
        nullable: *nullable,
        list: *list,
        enum_values: enum_values.clone(),
        is_key,
        indexed,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const DEV_SCHEMA: &str = include_str!("../../../dev/cluster/knowledge.pg");

    pub fn dev() -> SchemaModel {
        SchemaModel::parse(DEV_SCHEMA).expect("dev schema compiles")
    }

    /// A graph admitted before a rule tightened opens: the server keeps its schema, which the
    /// admission parser would now refuse.
    #[test]
    fn a_schema_admitted_before_a_rule_tightened_still_opens() {
        let historical =
            "node Document {\n  slug: String @key\n  payload: Blob?\n  @unique(payload)\n}\n";
        assert!(omnigraph_compiler::schema::parser::parse_schema(historical).is_err());
        let schema = SchemaModel::parse(historical).expect("an accepted schema opens");
        assert!(schema.node("Document").is_some());
    }

    /// Every member of a composite key is a key the server will not let change; the node has
    /// no single key for its id.
    #[test]
    fn a_composite_keys_members_are_keys() {
        let schema = SchemaModel::parse(
            "node Pair {\n  alpha: String\n  zeta: String\n  note: String?\n  @key(alpha, zeta)\n}\n",
        )
        .unwrap();
        let pair = schema.node("Pair").unwrap();
        assert_eq!(pair.key, None);
        for name in ["alpha", "zeta"] {
            let member = pair.property(name).unwrap();
            assert!(member.is_key && !member.editable_in_cell(), "{name}");
        }
        assert!(pair.property("note").unwrap().editable_in_cell());
    }

    /// A type's description is what it is; without one, its instruction says it; neither, and
    /// the schema says nothing.
    #[test]
    fn a_type_says_what_it_is() {
        let schema = SchemaModel::parse(
            r#"
node Task @description("A unit of work") @instruction("Query by slug") { slug: String @key }
node Note @instruction("A remark kept for later") { slug: String @key }
node Tag { slug: String @key }
edge Blocks: Task -> Task @instruction("Only for hard blockers")
"#,
        )
        .unwrap();
        let about = |name: &str| schema.node(name).unwrap().about.clone();
        assert_eq!(about("Task").as_deref(), Some("A unit of work"));
        assert_eq!(about("Note").as_deref(), Some("A remark kept for later"));
        assert_eq!(about("Tag"), None);
        assert_eq!(
            schema.edges[0].about.as_deref(),
            Some("Only for hard blockers")
        );
    }

    #[test]
    fn reads_types_keys_and_properties() {
        let schema = dev();
        let person = schema.node("Person").unwrap();
        assert_eq!(person.key.as_deref(), Some("slug"));
        assert_eq!(person.label_property(), Some("name"));
        // A chosen name holds when it is one text value; otherwise the default does.
        let names = |choice: &str| {
            let labelled = schema.with_labels(&BTreeMap::from([("Person".into(), choice.into())]));
            let person = labelled
                .node("Person")
                .unwrap()
                .label_property()
                .map(str::to_owned);
            (person, labelled.same_as(&schema))
        };
        assert_eq!(names("title"), (Some("title".into()), false));
        assert_eq!(names("slug"), (Some("slug".into()), false));
        assert_eq!(
            names("tags"),
            (Some("name".into()), true),
            "a list cannot name a node"
        );
        assert_eq!(
            names("born"),
            (Some("name".into()), true),
            "a date cannot name a node"
        );
        assert_eq!(
            names("nickname"),
            (Some("name".into()), true),
            "no such property"
        );
        assert_eq!(person.default_label_property(), Some("name"));
        let candidates: Vec<_> = person.label_candidates().map(|p| p.name.as_str()).collect();
        assert_eq!(candidates, ["slug", "name", "title", "email"]);
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
        assert_eq!(traversal_token("WorksAt"), "worksAt");
        assert_eq!(traversal_token("In"), "\"in\"");
        assert_eq!(traversal_name("In"), "in");
    }
}

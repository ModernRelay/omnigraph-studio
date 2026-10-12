//! Completion for `.gq` source from the schema (spec 007).
//!
//! A lexical heuristic, not a parser: after `$var.` it offers the bound type's properties;
//! otherwise types, relationships (traversal spelling) and keywords that start with the word
//! being typed.

use crate::schema::{SchemaModel, traversal_name};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    Keyword,
    Type,
    Relationship,
    Property,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub label: String,
    pub kind: CompletionKind,
    pub detail: Option<String>,
}

const KEYWORDS: &[&str] = &[
    "query", "match", "return", "order", "limit", "insert", "update", "set", "where", "delete",
    "not", "exists", "count", "sum", "min", "max", "avg", "and", "or", "in", "is", "null", "asc",
    "desc", "nearest", "search", "fuzzy", "bm25", "rrf", "now", "explain", "branch", "show",
];

/// Completions for the word ending at `offset` (a byte offset into `source`). Returns the byte
/// offset where the word starts, which the editor replaces, and the candidates.
pub fn completions(schema: &SchemaModel, source: &str, offset: usize) -> (usize, Vec<Completion>) {
    let offset = offset.min(source.len());
    let before = &source[..offset];
    let start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
        .last()
        .map(|(i, _)| i)
        .unwrap_or(offset);
    let prefix = &source[start..offset];
    let lower = prefix.to_lowercase();
    let matches = |label: &str| label.to_lowercase().starts_with(&lower);

    // `$var.` → properties of the type `$var` is bound to.
    if before[..start].ends_with('.') {
        let var_end = start - 1;
        let var_start = before[..var_end]
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
            .last()
            .map(|(i, _)| i)
            .unwrap_or(var_end);
        if var_start > 0 && before.as_bytes()[var_start - 1] == b'$' {
            let var = &before[var_start..var_end];
            let mut items = Vec::new();
            if let Some(type_name) = bound_type(source, var) {
                let properties = schema
                    .node(&type_name)
                    .map(|n| n.properties.clone())
                    .or_else(|| schema.edge(&type_name).map(|e| e.properties.clone()))
                    .unwrap_or_default();
                for property in properties {
                    if matches(&property.name) {
                        items.push(Completion {
                            label: property.name.clone(),
                            kind: CompletionKind::Property,
                            detail: Some(property.display_type()),
                        });
                    }
                }
            }
            for meta in ["@id"] {
                if prefix.is_empty() {
                    items.push(Completion {
                        label: meta.into(),
                        kind: CompletionKind::Property,
                        detail: Some("String".into()),
                    });
                }
            }
            return (start, items);
        }
    }

    let mut items = Vec::new();
    for node in &schema.nodes {
        if matches(&node.name) {
            items.push(Completion {
                label: node.name.clone(),
                kind: CompletionKind::Type,
                detail: Some("node".into()),
            });
        }
    }
    for edge in &schema.edges {
        let name = traversal_name(&edge.name);
        if matches(&name) {
            items.push(Completion {
                label: name,
                kind: CompletionKind::Relationship,
                detail: Some(format!("{} → {}", edge.from, edge.to)),
            });
        }
    }
    for keyword in KEYWORDS {
        if !prefix.is_empty() && matches(keyword) {
            items.push(Completion {
                label: (*keyword).into(),
                kind: CompletionKind::Keyword,
                detail: None,
            });
        }
    }
    (start, items)
}

/// The type in the first `$var: Type` binding, or the edge in `$var:edge` for edge bindings.
fn bound_type(source: &str, var: &str) -> Option<String> {
    let needle = format!("${var}:");
    let at = source.find(&needle)? + needle.len();
    let rest = source[at..].trim_start();
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() {
        return None;
    }
    // `$e:worksAt` binds an edge; edges are declared with a capital first letter.
    let mut chars = name.chars();
    let first = chars.next()?;
    Some(first.to_uppercase().chain(chars).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::tests::dev;

    #[test]
    fn completes_properties_of_a_bound_variable() {
        let source = "query q() { match { $p: Person } return { $p.na";
        let (start, items) = completions(&dev(), source, source.len());
        assert_eq!(&source[start..], "na");
        assert_eq!(items[0].label, "name");
    }

    #[test]
    fn completes_edge_properties() {
        let source = "query q() { match { $a $e:worksAt $b } return { $e.ro";
        let (_, items) = completions(&dev(), source, source.len());
        assert_eq!(items[0].label, "role");
    }

    #[test]
    fn completes_types_relationships_and_keywords() {
        let schema = dev();
        let (_, items) = completions(&schema, "query q() { match { $p: Pe", 26);
        assert!(items.iter().any(|i| i.label == "Person"));
        let (_, items) = completions(&schema, "$p wo", 5);
        assert!(items.iter().any(|i| i.label == "worksAt"));
        let (_, items) = completions(&schema, "ret", 3);
        assert!(items.iter().any(|i| i.label == "return"));
    }
}

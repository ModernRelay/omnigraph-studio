//! Fixes made from a review (spec 008): the mutation that restores an entity's before image.
//!
//! Every statement comes from `gq_language::builder` (`update_cell`, `insert_node`,
//! `delete_node`). A revert that needs several statements joins them into one mutation with
//! [`compose`], so it publishes once. Values stay parameters; only parameter names are renamed.

use gq_language::builder::{delete_edge, delete_node, insert_edge, insert_node, update_cell};
use gq_language::{BuiltQuery, SchemaModel};
use serde_json::{Map, Value};

use crate::changes::{FoldedChange, NetChange, endpoints_of};

/// Why a change cannot be reverted from Studio; shown on the disabled control (constitution VI).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevertUnavailable(pub String);

impl std::fmt::Display for RevertUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The mutation that restores `change`'s before image on the branch it was read from.
///
/// - Added node: delete it (its relationships go with it).
/// - Removed node: insert it again with its before values.
/// - Changed node: set each changed property back, each bound to the value the review saw, so
///   a value changed since then is left alone (zero affected rows).
///
/// - Added relationship: delete it.
/// - Removed relationship: insert it again between its before endpoints with its before
///   values. An unkeyed relationship comes back under a new id; a `.gq` insert cannot supply one.
///
/// A changed relationship, Blob and Vector values, and types missing from the schema cannot be
/// reverted here.
pub fn revert_query(
    schema: &SchemaModel,
    change: &FoldedChange,
) -> Result<BuiltQuery, RevertUnavailable> {
    if change.is_edge() {
        return revert_edge(schema, change);
    }
    let Some(node) = schema.node(&change.type_name) else {
        return Err(RevertUnavailable(format!(
            "The current schema has no type {}.",
            change.type_name
        )));
    };
    let unavailable = |error: gq_language::BuildError| RevertUnavailable(error.to_string());
    match change.net() {
        None => Err(RevertUnavailable("Nothing to revert.".into())),
        Some(NetChange::Added) => delete_node(schema, &node.name, &change.id).map_err(unavailable),
        Some(NetChange::Removed) => {
            let before = &change
                .before
                .as_ref()
                .expect("a removed entity has a before image")
                .properties;
            let mut values = Vec::new();
            for property in &node.properties {
                match before.get(&property.name) {
                    Some(Value::Null) | None => {}
                    Some(value) if property.projectable() => {
                        values.push((property.name.clone(), value.clone()))
                    }
                    Some(_) => {
                        return Err(RevertUnavailable(format!(
                            "{} holds a {} value, which the review cannot restore.",
                            property.name, property.base_type
                        )));
                    }
                }
            }
            insert_node(schema, &node.name, &values).map_err(unavailable)
        }
        Some(NetChange::Changed) => {
            let order: Vec<String> = node.properties.iter().map(|p| p.name.clone()).collect();
            let mut parts = Vec::new();
            for diff in change.property_diffs(&order) {
                // A property the before image lacks was outside that commit's schema.
                let Some(before) = diff.before else { continue };
                let Some(property) = node.property(&diff.name) else {
                    continue;
                };
                if !property.editable_in_cell() {
                    return Err(RevertUnavailable(format!(
                        "{} cannot be set from the review.",
                        property.name
                    )));
                }
                let after = diff.after.unwrap_or(Value::Null);
                parts.push(
                    update_cell(
                        schema,
                        &node.name,
                        &property.name,
                        &change.id,
                        &after,
                        &before,
                    )
                    .map_err(unavailable)?,
                );
            }
            if parts.is_empty() {
                return Err(RevertUnavailable("No property can be set back.".into()));
            }
            Ok(compose("studio_revert", parts))
        }
    }
}

fn revert_edge(
    schema: &SchemaModel,
    change: &FoldedChange,
) -> Result<BuiltQuery, RevertUnavailable> {
    let Some(edge) = schema.edge(&change.type_name) else {
        return Err(RevertUnavailable(format!(
            "The current schema has no relationship {}.",
            change.type_name
        )));
    };
    let unavailable = |error: gq_language::BuildError| RevertUnavailable(error.to_string());
    match change.net() {
        None => Err(RevertUnavailable("Nothing to revert.".into())),
        Some(NetChange::Added) => delete_edge(schema, &edge.name, &change.id).map_err(unavailable),
        Some(NetChange::Removed) => {
            let before = change
                .before
                .as_ref()
                .expect("a removed entity has a before image");
            let Some((from, to)) = before.endpoints.as_ref().and_then(endpoints_of) else {
                return Err(RevertUnavailable(
                    "The change feed did not say which nodes this relationship joined.".into(),
                ));
            };
            let mut values = Vec::new();
            for property in &edge.properties {
                match before.properties.get(&property.name) {
                    Some(Value::Null) | None => {}
                    Some(value) if property.projectable() => {
                        values.push((property.name.clone(), value.clone()))
                    }
                    Some(_) => {
                        return Err(RevertUnavailable(format!(
                            "{} holds a {} value, which the review cannot restore.",
                            property.name, property.base_type
                        )));
                    }
                }
            }
            insert_edge(schema, &edge.name, &from, &to, &values).map_err(unavailable)
        }
        // Relationships cannot be updated: restoring one means a delete and an insert, which a
        // mutation cannot combine, so the revert would publish twice.
        Some(NetChange::Changed) => Err(RevertUnavailable(
            "A changed relationship is restored by deleting it and inserting it again, two \
             commits. Use the query editor."
                .into(),
        )),
    }
}

/// Joins mutations into one, renaming each part's parameters `$name` to `$name_<i>` so they
/// cannot collide. A single part is returned unchanged.
///
/// Relies on the builder's layout: `query <name>(<params>) {\n<body>\n}\n`.
pub fn compose(name: &str, parts: Vec<BuiltQuery>) -> BuiltQuery {
    if parts.len() == 1 {
        return parts.into_iter().next().expect("one part");
    }
    let mut signature = Vec::new();
    let mut bodies = Vec::new();
    let mut params = Map::new();
    for (i, part) in parts.into_iter().enumerate() {
        let (sig, body) = split_query(&part.text);
        let mut names: Vec<&String> = part.params.keys().collect();
        // Longest first, so `$id` never rewrites part of `$id2`.
        names.sort_by_key(|n| std::cmp::Reverse(n.len()));
        let mut sig = sig.to_owned();
        let mut body = body.to_owned();
        for param in &names {
            let renamed = format!("{param}_{i}");
            sig = rename_param(&sig, param, &renamed);
            body = rename_param(&body, param, &renamed);
        }
        if !sig.trim().is_empty() {
            signature.push(sig);
        }
        bodies.push(body);
        for (key, value) in part.params {
            params.insert(format!("{key}_{i}"), value);
        }
    }
    BuiltQuery {
        name: name.to_owned(),
        text: format!(
            "query {name}({}) {{\n{}\n}}\n",
            signature.join(", "),
            bodies.join("\n")
        ),
        params,
        depends: Default::default(),
        seeds: None,
    }
}

/// `(signature, body)` of a builder query.
fn split_query(text: &str) -> (&str, &str) {
    let open = text.find('(').map(|i| i + 1).unwrap_or(0);
    let close = text[open..].find(") {").map(|i| open + i).unwrap_or(open);
    let body_start = text[close..]
        .find('{')
        .map(|i| close + i + 1)
        .unwrap_or(close);
    let body_end = text.rfind('}').unwrap_or(text.len());
    (
        &text[open..close],
        text[body_start..body_end].trim_matches('\n'),
    )
}

/// Replaces `$from` with `$to` where `$from` is a whole parameter name.
fn rename_param(text: &str, from: &str, to: &str) -> String {
    let needle = format!("${from}");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(&needle) {
        let after = &rest[at + needle.len()..];
        let whole = !after
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_');
        out.push_str(&rest[..at]);
        if whole {
            out.push('$');
            out.push_str(to);
        } else {
            out.push_str(&needle);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use gq_language::check;
    use graph_client::api::{ChangeImageOutput, EntityKind};
    use serde_json::json;

    use super::*;

    const DEV_SCHEMA: &str = include_str!("../../../dev/cluster/knowledge.pg");

    fn schema() -> SchemaModel {
        SchemaModel::parse(DEV_SCHEMA).expect("dev schema compiles")
    }

    fn assert_compiles(query: &BuiltQuery) {
        let result = check(&schema(), &query.text);
        assert!(
            result.diagnostics.is_empty(),
            "generated query does not type-check:\n{}\n{:?}",
            query.text,
            result.diagnostics
        );
        let declared = &result.queries[0].params;
        assert_eq!(declared.len(), query.params.len(), "{}", query.text);
        for (name, _, _) in declared {
            assert!(query.params.contains_key(name), "missing value for ${name}");
        }
    }

    fn change(
        kind: EntityKind,
        type_name: &str,
        before: Option<Value>,
        after: Option<Value>,
    ) -> FoldedChange {
        let image = |v: Value| ChangeImageOutput {
            properties: v.as_object().unwrap().clone(),
            endpoints: Some(json!({"from": "person-000001", "to": "company-00002"})),
        };
        FoldedChange {
            kind,
            type_id: format!("t-{type_name}"),
            type_name: type_name.into(),
            id: "person-000001".into(),
            before: before.map(image),
            after: after.map(image),
            commits: vec!["c1".into()],
        }
    }

    #[test]
    fn a_changed_node_sets_every_property_back_in_one_mutation() {
        let c = change(
            EntityKind::Node,
            "Person",
            Some(
                json!({"slug": "person-000001", "title": "Researcher", "email": null, "tags": ["a"]}),
            ),
            Some(
                json!({"slug": "person-000001", "title": "Principal", "email": "x@y", "tags": ["b"]}),
            ),
        );
        let query = revert_query(&schema(), &c).unwrap();
        assert_compiles(&query);
        assert_eq!(query.text.matches("update Person").count(), 3);
        // The bound value is the after image; the new value the before image.
        let pairs: Vec<(Value, Option<Value>)> = (0..3)
            .map(|i| {
                (
                    query.params[&format!("new_{i}")].clone(),
                    query.params.get(&format!("old_{i}")).cloned(),
                )
            })
            .collect();
        assert!(pairs.contains(&(json!("Researcher"), Some(json!("Principal")))));
        assert!(pairs.contains(&(Value::Null, Some(json!("x@y")))));
        assert!(
            pairs.contains(&(json!(["a"]), None)),
            "lists bind to the row only"
        );
    }

    #[test]
    fn a_single_property_revert_is_the_builders_query() {
        let c = change(
            EntityKind::Node,
            "Person",
            Some(json!({"title": "A"})),
            Some(json!({"title": "B"})),
        );
        let query = revert_query(&schema(), &c).unwrap();
        assert_compiles(&query);
        assert_eq!(query.name, "studio_set");
    }

    #[test]
    fn added_and_removed_nodes_revert_with_delete_and_insert() {
        let added = change(
            EntityKind::Node,
            "Person",
            None,
            Some(json!({"slug": "p", "name": "N"})),
        );
        let query = revert_query(&schema(), &added).unwrap();
        assert_compiles(&query);
        assert!(query.text.contains("delete Person"));

        let removed = change(
            EntityKind::Node,
            "Person",
            Some(json!({"slug": "person-000001", "name": "N", "email": null, "tags": ["x"]})),
            None,
        );
        let query = revert_query(&schema(), &removed).unwrap();
        assert_compiles(&query);
        assert!(query.text.contains("insert Person"));
        assert_eq!(query.params.len(), 3, "nulls are left out");
    }

    #[test]
    fn added_and_removed_relationships_revert_with_delete_and_insert() {
        let added = change(
            EntityKind::Edge,
            "WorksAt",
            None,
            Some(json!({"role": "Engineer"})),
        );
        let query = revert_query(&schema(), &added).unwrap();
        assert_compiles(&query);
        assert!(query.text.contains("delete WorksAt where @id"));
        assert_eq!(query.params["id"], json!("person-000001"));

        let removed = change(
            EntityKind::Edge,
            "WorksAt",
            Some(json!({"role": "Engineer", "since": null})),
            None,
        );
        let query = revert_query(&schema(), &removed).unwrap();
        assert_compiles(&query);
        assert!(query.text.contains("insert WorksAt"));
        assert_eq!(query.params["from"], json!("person-000001"));
        assert_eq!(query.params["to"], json!("company-00002"));
        assert_eq!(query.params.len(), 3, "nulls are left out");
    }

    #[test]
    fn relationships_and_unknown_types_say_why_not() {
        let changed = change(
            EntityKind::Edge,
            "WorksAt",
            Some(json!({"role": "Engineer"})),
            Some(json!({"role": "Manager"})),
        );
        assert!(
            revert_query(&schema(), &changed)
                .unwrap_err()
                .0
                .contains("two")
        );
        let mut removed = change(EntityKind::Edge, "Knows", Some(json!({})), None);
        removed.before.as_mut().unwrap().endpoints = None;
        assert!(
            revert_query(&schema(), &removed)
                .unwrap_err()
                .0
                .contains("nodes")
        );
        let unknown = change(EntityKind::Edge, "Likes", None, Some(json!({})));
        assert!(
            revert_query(&schema(), &unknown)
                .unwrap_err()
                .0
                .contains("Likes")
        );
        let gone = change(EntityKind::Node, "Robot", None, Some(json!({})));
        assert!(
            revert_query(&schema(), &gone)
                .unwrap_err()
                .0
                .contains("Robot")
        );
    }

    #[test]
    fn renaming_respects_whole_names() {
        assert_eq!(
            rename_param("$id = $id2 and $id", "id", "id_0"),
            "$id_0 = $id2 and $id_0"
        );
        let (sig, body) = split_query("query q($a: String) {\n  delete T where @id = $a\n}\n");
        assert_eq!(sig, "$a: String");
        assert_eq!(body, "  delete T where @id = $a");
    }
}

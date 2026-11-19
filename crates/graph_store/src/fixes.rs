//! Fixes made from a review (spec 008): the mutation that restores an entity's before image.
//!
//! Every statement comes from `gq_language::builder` (`update_rows`, `insert_node`,
//! `delete_node`, `insert_edge`, `delete_edge`), and a revert is one statement. Several
//! mutations join into one with [`compose`], so they publish once. Values stay parameters; only
//! parameter names are renamed.

use gq_language::builder::{
    RowEdit, delete_edge, delete_node, insert_edge, insert_node, update_rows,
};
use gq_language::{BuiltQuery, Property, SchemaModel};
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
/// - Removed node: insert it again with its before values. A node without a key cannot be:
///   it would come back under a new id, and its relationships' reverts would miss it.
/// - Changed node: set every changed property back in one statement, bound to every value the
///   review saw, so a node with any of them changed since is left alone whole (zero affected
///   rows), never set back in part.
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
        Some(NetChange::Removed) if node.key.is_none() => Err(RevertUnavailable(format!(
            "{} has no key, so a revert would bring it back under a new id.",
            node.name
        ))),
        Some(NetChange::Removed) => {
            let before = change
                .before
                .as_ref()
                .expect("a removed entity has a before image");
            let values = restorable(&node.properties, &before.properties)?;
            insert_node(schema, &node.name, &values).map_err(unavailable)
        }
        Some(NetChange::Changed) => {
            let order: Vec<String> = node.properties.iter().map(|p| p.name.clone()).collect();
            let mut cells = Vec::new();
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
                cells.push((property.name.clone(), after, before));
            }
            if cells.is_empty() {
                return Err(RevertUnavailable("No property can be set back.".into()));
            }
            let row = RowEdit {
                id: change.id.clone(),
                cells,
            };
            update_rows(schema, &node.name, &[row]).map_err(unavailable)
        }
    }
}

/// The values a removed node or relationship held (`before`), to insert it again: each of
/// `properties` it held a value of, or why one of them cannot be restored.
fn restorable(
    properties: &[Property],
    before: &Map<String, Value>,
) -> Result<Vec<(String, Value)>, RevertUnavailable> {
    let mut values = Vec::new();
    for property in properties {
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
    Ok(values)
}

/// Whether `change`'s revert deletes the entity: an addition's. The delete is bound to the
/// entity's id alone, so it takes whatever later commits changed with it; only a review that
/// shows what the branch holds now, whose fixes are bound to that head, sends one.
pub fn revert_deletes(change: &FoldedChange) -> bool {
    change.net() == Some(NetChange::Added)
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
            let values = restorable(&edge.properties, &before.properties)?;
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

/// What one side of a merge conflict holds of an entity (spec 039): its properties as read, a
/// property a read could not return (a Blob, a vector) left out, which is not a null.
pub type SideImage = Map<String, Value>;

/// The mutation that makes the branch `have` was read on hold what the other side holds of
/// entity `id` (`want`), so the merge finds the two sides alike and takes it (spec 039,
/// "Keep main's" and "Keep this branch's"). Omnigraph compares a conflicting row whole, so the
/// row is made alike, not only the properties both sides changed.
///
/// - Both hold it: set every property whose values differ, bound to each value `have` holds,
///   so a row changed since is left alone whole (zero affected rows), never set in part.
/// - Only `want` holds it: insert it with its values; a node without a key cannot be, since it
///   would come back under another id. An edge cannot be either: an insert cannot give it its
///   id (E26).
/// - Only `have` holds it: delete it.
///
/// Properties held by one image and not the other (outside one side's schema, or not read) are
/// not compared. A differing value a mutation cannot set (a Blob, a vector) makes it
/// unavailable, with why.
pub fn take_side(
    schema: &SchemaModel,
    is_edge: bool,
    type_name: &str,
    id: &str,
    want: Option<&SideImage>,
    have: Option<&SideImage>,
) -> Result<BuiltQuery, RevertUnavailable> {
    let unavailable = |error: gq_language::BuildError| RevertUnavailable(error.to_string());
    if is_edge {
        let Some(edge) = schema.edge(type_name) else {
            return Err(RevertUnavailable(format!(
                "The current schema has no relationship {type_name}."
            )));
        };
        return match (want, have) {
            (None, Some(_)) => delete_edge(schema, &edge.name, id).map_err(unavailable),
            (None, None) => Err(RevertUnavailable("Neither side holds it.".into())),
            (Some(_), _) => Err(RevertUnavailable(
                "A relationship cannot be changed or brought back under its own id: \
                 remove it on one side instead."
                    .into(),
            )),
        };
    }
    let Some(node) = schema.node(type_name) else {
        return Err(RevertUnavailable(format!(
            "The current schema has no type {type_name}."
        )));
    };
    match (want, have) {
        (None, None) => Err(RevertUnavailable("Neither side holds it.".into())),
        (None, Some(_)) => delete_node(schema, &node.name, id).map_err(unavailable),
        (Some(_), None) if node.key.is_none() => Err(RevertUnavailable(format!(
            "{} has no key, so it would come back under another id.",
            node.name
        ))),
        (Some(want), None) => {
            let values = restorable(&node.properties, want)?;
            insert_node(schema, &node.name, &values).map_err(unavailable)
        }
        (Some(want), Some(have)) => {
            let mut cells = Vec::new();
            for property in &node.properties {
                let (Some(wanted), Some(held)) =
                    (want.get(&property.name), have.get(&property.name))
                else {
                    continue;
                };
                if gq_language::input::same_value(wanted, held) {
                    continue;
                }
                if !property.editable_in_cell() {
                    return Err(RevertUnavailable(format!(
                        "{} cannot be set from the review.",
                        property.name
                    )));
                }
                cells.push((property.name.clone(), held.clone(), wanted.clone()));
            }
            if cells.is_empty() {
                return Err(RevertUnavailable(
                    "Both sides already hold the same values.".into(),
                ));
            }
            let row = RowEdit {
                id: id.to_owned(),
                cells,
            };
            update_rows(schema, &node.name, &[row]).map_err(unavailable)
        }
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
        empty_without: None,
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

    /// A changed node is set back by one statement bound to every value the review saw: with
    /// any of them changed since, the statement matches nothing and nothing is set back, rather
    /// than the properties still as seen being set back alone.
    #[test]
    fn a_changed_node_sets_every_property_back_in_one_statement_bound_to_every_value() {
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
        assert_eq!(
            query.text.matches("update Person").count(),
            1,
            "{}",
            query.text
        );
        assert!(
            query
                .text
                .contains("where @id = $id0 and title = $old0_0 and email = $old0_1\n"),
            "{}",
            query.text
        );
        // The bound value is the after image; the new value the before image.
        assert_eq!(query.params["new0_0"], json!("Researcher"));
        assert_eq!(query.params["old0_0"], json!("Principal"));
        assert_eq!(query.params["new0_1"], Value::Null);
        assert_eq!(query.params["old0_1"], json!("x@y"));
        assert_eq!(query.params["new0_2"], json!(["a"]));
        assert!(
            !query.params.contains_key("old0_2"),
            "lists bind to the row only"
        );

        let one = change(
            EntityKind::Node,
            "Person",
            Some(json!({"title": "A"})),
            Some(json!({"title": "B"})),
        );
        let query = revert_query(&schema(), &one).unwrap();
        assert_compiles(&query);
        assert!(
            query
                .text
                .contains("where @id = $id0 and title = $old0_0\n"),
            "{}",
            query.text
        );
        assert!(!revert_deletes(&c));
        assert!(!revert_deletes(&one));
    }

    /// A conflict's side is taken by making the other side's row alike (spec 039): every
    /// differing property set in one statement bound to the values held, a row only one side
    /// holds inserted or deleted, and a relationship only removed.
    #[test]
    fn taking_a_side_makes_the_other_rows_alike() {
        let image = |v: Value| v.as_object().unwrap().clone();
        let main = image(json!({"slug": "ada", "name": "Ada", "title": "CTO", "tags": ["x"]}));
        let here = image(json!({"slug": "ada", "name": "Ada", "title": "CEO", "tags": ["y"]}));
        let query = take_side(&schema(), false, "Person", "ada", Some(&main), Some(&here)).unwrap();
        assert_compiles(&query);
        assert_eq!(
            query.text.matches("update Person").count(),
            1,
            "{}",
            query.text
        );
        assert!(
            query.text.contains("set { title: $new0_0, tags: $new0_1 }"),
            "{}",
            query.text
        );
        assert!(
            query
                .text
                .contains("where @id = $id0 and title = $old0_0\n"),
            "{}",
            query.text
        );
        assert_eq!(query.params["new0_0"], json!("CTO"));
        assert_eq!(query.params["old0_0"], json!("CEO"));
        assert_eq!(query.params["new0_1"], json!(["x"]));

        // A property one side did not read is not compared, and one alike asks for nothing.
        let partial = image(json!({"slug": "ada", "name": "Ada"}));
        assert!(
            take_side(
                &schema(),
                false,
                "Person",
                "ada",
                Some(&partial),
                Some(&here)
            )
            .is_err()
        );

        // Only the wanted side holds it: inserted with its values; only the other: deleted.
        let insert = take_side(&schema(), false, "Person", "ada", Some(&main), None).unwrap();
        assert_compiles(&insert);
        assert!(insert.text.contains("insert Person"), "{}", insert.text);
        let delete = take_side(&schema(), false, "Person", "ada", None, Some(&here)).unwrap();
        assert_compiles(&delete);
        assert!(
            delete.text.contains("delete Person where @id = $id"),
            "{}",
            delete.text
        );

        // A relationship is only removed.
        let edge = image(json!({"role": "x"}));
        let remove = take_side(&schema(), true, "WorksAt", "e1", None, Some(&edge)).unwrap();
        assert_compiles(&remove);
        let refused = take_side(&schema(), true, "WorksAt", "e1", Some(&edge), Some(&edge));
        assert!(refused.unwrap_err().0.contains("remove it on one side"));
        assert!(take_side(&schema(), false, "Nobody", "x", None, Some(&here)).is_err());
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
        assert!(revert_deletes(&added), "a delete is bound to the id alone");

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
        assert!(!revert_deletes(&removed));
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
        assert!(revert_deletes(&added));

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
        // A node without a key would come back under a new id.
        let keyless = SchemaModel::parse("node Note {\n  text: String\n}\n").unwrap();
        let removed = change(EntityKind::Node, "Note", Some(json!({"text": "x"})), None);
        assert!(
            revert_query(&keyless, &removed)
                .unwrap_err()
                .0
                .contains("no key")
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

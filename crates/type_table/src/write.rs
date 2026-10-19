//! A node's properties written from its page, its peek or its text editor (spec 014): bound to
//! the values they were read at, as a table cell saves (spec 004), so a write changes nothing
//! if a value changed elsewhere meanwhile.

use gpui::{App, Entity, Task};
use gq_language::{SchemaModel, builder};
use graph_store::GraphSession;
use serde_json::Value;

use crate::model::{Notice, SaveResult, Tone, node_rows, same_value, save_result};

/// Sends the write to `branch`, the one the value was read on, and resolves with how it ended;
/// `Err` when no write can be built.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_property(
    session: &Entity<GraphSession>,
    branch: &str,
    schema: &SchemaModel,
    type_name: &str,
    id: &str,
    property: &str,
    old: &Value,
    new: &Value,
    cx: &mut App,
) -> Result<Task<SaveResult>, String> {
    let query = builder::update_cell(schema, type_name, property, id, old, new)
        .map_err(|error| error.to_string())?;
    let description = format!("Set {property} of {id}");
    let read = (property.to_owned(), old.clone());
    send_bound(
        session,
        branch,
        schema,
        type_name,
        id,
        &[read],
        query,
        description,
        cx,
    )
}

/// Writes several properties of node `id` in one commit, each `(property, the value it was
/// read at, the new value)`, as [`write_property`] writes one.
pub(crate) fn write_properties(
    session: &Entity<GraphSession>,
    branch: &str,
    schema: &SchemaModel,
    type_name: &str,
    id: &str,
    cells: &[(String, Value, Value)],
    cx: &mut App,
) -> Result<Task<SaveResult>, String> {
    let row = builder::RowEdit {
        id: id.to_owned(),
        cells: cells.to_vec(),
    };
    let query =
        builder::update_rows(schema, type_name, &[row]).map_err(|error| error.to_string())?;
    let description = format!("Save {} changes of {id}", cells.len());
    let read: Vec<(String, Value)> = cells
        .iter()
        .map(|(property, old, _)| (property.clone(), old.clone()))
        .collect();
    send_bound(
        session,
        branch,
        schema,
        type_name,
        id,
        &read,
        query,
        description,
        cx,
    )
}

/// Sends `query`, which writes node `id`'s properties `read` (each with the value it was read
/// at), to `branch`. Its `where` binds the scalars; a `where` cannot compare a list, so with a
/// list among them the node is read at the branch's head first and the write is bound to that
/// head: a change made elsewhere since the values were read is found by the comparison, and one
/// made after the read by the head having moved.
#[allow(clippy::too_many_arguments)]
fn send_bound(
    session: &Entity<GraphSession>,
    branch: &str,
    schema: &SchemaModel,
    type_name: &str,
    id: &str,
    read: &[(String, Value)],
    query: builder::BuiltQuery,
    description: String,
    cx: &mut App,
) -> Result<Task<SaveResult>, String> {
    let branch = branch.to_owned();
    let lists: Vec<(String, Value)> = read
        .iter()
        .filter(|(property, _)| {
            schema
                .node(type_name)
                .and_then(|node| node.property(property))
                .is_some_and(|property| property.list)
        })
        .cloned()
        .collect();
    if lists.is_empty() {
        let write = session.update(cx, |session, cx| {
            session.write_on(&branch, &query, None, description, None, cx)
        });
        return Ok(cx.spawn(async move |_| save_result(&write.await, &branch)));
    }
    let node = builder::node_by_id(schema, type_name, id).map_err(|error| error.to_string())?;
    let read = session.update(cx, |session, cx| session.read_on(&branch, &node, cx));
    let session = session.clone();
    Ok(cx.spawn(async move |cx| {
        let output = match read.await {
            Ok(output) => output,
            Err(error) => {
                return SaveResult::Refused(Notice::new(
                    Tone::Refused,
                    format!("Nothing was written. {error}"),
                ));
            }
        };
        let (Some(head), Some(row)) = (output.graph_commit_id, node_rows(&output.rows).pop())
        else {
            return SaveResult::NoMatch;
        };
        let unchanged = lists.iter().all(|(property, old)| {
            same_value(row.values.get(property).unwrap_or(&Value::Null), old)
        });
        if !unchanged {
            return SaveResult::NoMatch;
        }
        let write = session.update(cx, |session, cx| {
            session.write_on(&branch, &query, Some(head), description, None, cx)
        });
        save_result(&write.await, &branch)
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use gpui::{AppContext as _, TestAppContext};
    use graph_store::{Connection, ServerEntry, ServerSource};
    use http_client::{AsyncBody, FakeHttpClient, Response};
    use serde_json::json;

    use super::*;

    const TAGGED: &str = "node Person {\n  slug: String @key\n  tags: [String]?\n}\n";

    /// A list cannot be compared in a write's `where`: its write is sent only when the value
    /// at the branch's head is the one read, and bound to that head.
    #[gpui::test]
    async fn a_list_write_is_bound_to_the_head_it_found_the_value_at(cx: &mut TestAppContext) {
        let writes = Arc::new(Mutex::new(Vec::<(String, Option<String>)>::new()));
        let sent = writes.clone();
        let http = FakeHttpClient::create(move |request| {
            let sent = sent.clone();
            async move {
                let path = request.uri().path().to_owned();
                let body = if path.ends_with("/query") {
                    json!({
                        "query_name": "studio_node",
                        "target": { "branch": "main", "snapshot": null },
                        "row_count": 1,
                        "columns": ["n"],
                        "rows": [{ "n": { "@id": "p1", "slug": "p1", "tags": ["a", "b"] } }],
                        "graph_commit_id": "c5",
                    })
                } else if path.contains("/mutate") {
                    let header = request
                        .headers()
                        .get(graph_client::IF_GRAPH_COMMIT_HEADER)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    sent.lock().unwrap().push((path, header));
                    crate::test_support::committed("c6", "c5")
                } else {
                    json!({ "branches": ["main"] })
                };
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, "0.13")
                    .body(AsyncBody::from(body.to_string()))
                    .unwrap())
            }
        });
        let server = ServerEntry {
            name: "dev".into(),
            url: "http://127.0.0.1:8787".into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, Some("t".into()), http).unwrap());
        let session =
            cx.new(|cx| GraphSession::new(connection, "knowledge".into(), "main".into(), cx));
        let schema = gq_language::SchemaModel::parse(TAGGED).unwrap();
        let save = |old: Value, cx: &mut TestAppContext| {
            let task = cx.update(|cx| {
                write_property(
                    &session,
                    "main",
                    &schema,
                    "Person",
                    "p1",
                    "tags",
                    &old,
                    &json!(["a", "c"]),
                    cx,
                )
                .unwrap()
            });
            cx.run_until_parked();
            cx.foreground_executor().block_on(task)
        };
        assert!(
            matches!(save(json!(["a"]), cx), SaveResult::NoMatch),
            "changed elsewhere since it was read"
        );
        assert!(writes.lock().unwrap().is_empty(), "nothing written");
        assert!(matches!(
            save(json!(["a", "b"]), cx),
            SaveResult::Saved { .. }
        ));
        let writes = writes.lock().unwrap();
        assert_eq!(writes.len(), 1);
        assert!(writes[0].0.ends_with("/if-graph-commit"), "{}", writes[0].0);
        assert_eq!(writes[0].1.as_deref(), Some("c5"));
    }
}

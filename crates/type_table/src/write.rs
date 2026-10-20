//! Node properties written from a table's cells (spec 004), and from a node's page, its peek or
//! its text editor (spec 014): bound to the values they were read at, so a write changes
//! nothing if a value changed elsewhere meanwhile.

use gpui::{App, Entity, Task};
use gq_language::input::same_value;
use gq_language::{BuildError, BuiltQuery, SchemaModel, builder};
use graph_client::WriteOutcome;
use graph_client::api::ChangeOutput;
use graph_store::GraphSession;
use serde_json::Value;

use crate::model::{Notice, SaveResult, Tone, node_rows, save_result};

/// How a write bound to the values it was read at ended: sent, with the server's outcome, or
/// not sent.
// Made once per write and matched at once, into the outcome: boxing it saves nothing.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Bound {
    Sent(WriteOutcome<ChangeOutput>),
    /// A list read at the branch's head holds another value than the one the write is bound
    /// to, or its node is gone: nothing was sent.
    Changed,
    /// The head could not be read to compare a list: nothing was sent.
    Unread(Notice),
}

impl Bound {
    /// How the write of one node ended. `branch` names the write target in a denial.
    pub(crate) fn result(&self, branch: &str) -> SaveResult {
        match self {
            Bound::Sent(outcome) => save_result(outcome, branch),
            Bound::Changed => SaveResult::NoMatch,
            Bound::Unread(notice) => SaveResult::Refused(notice.clone()),
        }
    }

    /// Whether it committed, setting every one of the `rows` nodes it was sent for.
    pub(crate) fn landed(&self, rows: usize) -> bool {
        matches!(
            self,
            Bound::Sent(WriteOutcome::Committed(output))
                if output.commit.is_some() && output.affected_nodes as usize == rows
        )
    }
}

/// One node a write sets, by id, with each property it sets and the value it was read at.
type Read = (String, Vec<(String, Value)>);

/// Sends the write to `branch`, the one the value was read on, and resolves with how it ended;
/// `Err` when no write can be built. `read_at` is the commit the caller read `old` at, when it
/// read it there (see [`send_bound`]).
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
    read_at: Option<String>,
    cx: &mut App,
) -> Result<Task<SaveResult>, String> {
    let query = builder::update_cell(schema, type_name, property, id, old, new)
        .map_err(|error| error.to_string())?;
    let description = format!("Set {property} of {id}");
    let read = vec![(id.to_owned(), vec![(property.to_owned(), old.clone())])];
    let sent = send_bound(
        session,
        branch,
        schema,
        type_name,
        read,
        read_at,
        query,
        description,
        cx,
    )?;
    let branch = branch.to_owned();
    Ok(cx.spawn(async move |_| sent.await.result(&branch)))
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
    let description = format!("Save {} changes of {id}", cells.len());
    let sent = write_rows(
        session,
        branch,
        schema,
        type_name,
        &[row],
        None,
        description,
        cx,
    )?;
    let branch = branch.to_owned();
    Ok(cx.spawn(async move |_| sent.await.result(&branch)))
}

/// Saves edits of several nodes in one commit, each row's cells `(property, the value it was
/// read at, the new value)`: a row whose values changed elsewhere is left alone whole, and the
/// commit's `affected_nodes` comes back short by it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_rows(
    session: &Entity<GraphSession>,
    branch: &str,
    schema: &SchemaModel,
    type_name: &str,
    rows: &[builder::RowEdit],
    read_at: Option<String>,
    description: String,
    cx: &mut App,
) -> Result<Task<Bound>, String> {
    let query = builder::update_rows(schema, type_name, rows).map_err(|error| error.to_string())?;
    let read = rows
        .iter()
        .map(|row| {
            let cells = row
                .cells
                .iter()
                .map(|(property, old, _)| (property.clone(), old.clone()))
                .collect();
            (row.id.clone(), cells)
        })
        .collect();
    send_bound(
        session,
        branch,
        schema,
        type_name,
        read,
        read_at,
        query,
        description,
        cx,
    )
}

/// Sets `property` to `new` on several nodes in one commit, each `(id, the value it was read
/// at)`, as [`write_rows`] saves rows.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_cells(
    session: &Entity<GraphSession>,
    branch: &str,
    schema: &SchemaModel,
    type_name: &str,
    property: &str,
    rows: &[(String, Value)],
    new: &Value,
    read_at: Option<String>,
    description: String,
    cx: &mut App,
) -> Result<Task<Bound>, String> {
    let query = builder::update_cells(schema, type_name, property, rows, new)
        .map_err(|error| error.to_string())?;
    let read = rows
        .iter()
        .map(|(id, old)| (id.clone(), vec![(property.to_owned(), old.clone())]))
        .collect();
    send_bound(
        session,
        branch,
        schema,
        type_name,
        read,
        read_at,
        query,
        description,
        cx,
    )
}

/// Sends `query`, which writes the properties `read` names of each node it names (each with the
/// value it was read at), to `branch`. Its `where` binds the scalars; a `where` cannot compare a
/// list, so with lists among them the write is bound to a commit holding the values read. That
/// is `read_at` when the caller read every value there and it is the head the session holds;
/// otherwise each node holding a list is read, the first at the branch's head and the others at
/// the commit it was found at, and the write is bound to that commit. A change made elsewhere
/// since the values were read is found by the comparison, and one made after it by the head
/// having moved.
#[allow(clippy::too_many_arguments)]
fn send_bound(
    session: &Entity<GraphSession>,
    branch: &str,
    schema: &SchemaModel,
    type_name: &str,
    read: Vec<Read>,
    read_at: Option<String>,
    query: BuiltQuery,
    description: String,
    cx: &mut App,
) -> Result<Task<Bound>, String> {
    let branch = branch.to_owned();
    let is_list = |property: &str| {
        schema
            .node(type_name)
            .and_then(|node| node.property(property))
            .is_some_and(|property| property.list)
    };
    let lists: Vec<Read> = read
        .into_iter()
        .filter_map(|(id, read)| {
            let lists: Vec<(String, Value)> = read
                .into_iter()
                .filter(|(property, _)| is_list(property))
                .collect();
            (!lists.is_empty()).then_some((id, lists))
        })
        .collect();
    // What the write is bound to without a read: nothing past its `where` for scalars alone,
    // and the commit the caller read the lists at while it is the head the session holds.
    let bound_to = if lists.is_empty() {
        Some(None)
    } else {
        read_at
            .filter(|at| session.read(cx).head(&branch) == Some(at.as_str()))
            .map(Some)
    };
    if let Some(bound_to) = bound_to {
        let write = session.update(cx, |session, cx| {
            session.write_on(&branch, &query, bound_to, description, None, cx)
        });
        return Ok(cx.spawn(async move |_| Bound::Sent(write.await)));
    }
    let reads = lists
        .into_iter()
        .map(|(id, read)| Ok((builder::node_by_id(schema, type_name, &id)?, read)))
        .collect::<Result<Vec<_>, BuildError>>()
        .map_err(|error| error.to_string())?;
    let session = session.clone();
    Ok(cx.spawn(async move |cx| {
        let unread = |error: graph_client::ReadError| {
            Bound::Unread(Notice::new(
                Tone::Refused,
                format!("Nothing was written. {error}"),
            ))
        };
        let mut reads = reads.into_iter();
        let Some((first, first_read)) = reads.next() else {
            return Bound::Changed;
        };
        let output = match session
            .update(cx, |session, cx| session.read_on(&branch, &first, cx))
            .await
        {
            Ok(output) => output,
            Err(error) => return unread(error),
        };
        let Some(head) = output.graph_commit_id.clone() else {
            return Bound::Changed;
        };
        if !holds(&output.rows, &first_read) {
            return Bound::Changed;
        }
        // The other nodes at the head the first was found at, read together.
        let rest: Vec<_> = session.update(cx, |session, cx| {
            reads
                .map(|(query, read)| (session.read(&query, Some(head.clone()), cx), read))
                .collect()
        });
        for (output, read) in rest {
            match output.await {
                Ok(output) if holds(&output.rows, &read) => {}
                Ok(_) => return Bound::Changed,
                Err(error) => return unread(error),
            }
        }
        let write = session.update(cx, |session, cx| {
            session.write_on(&branch, &query, Some(head), description, None, cx)
        });
        Bound::Sent(write.await)
    }))
}

/// Whether the node a `node_by_id` read found holds each property's value as `read` says.
fn holds(rows: &Value, read: &[(String, Value)]) -> bool {
    node_rows(rows).pop().is_some_and(|row| {
        read.iter().all(|(property, old)| {
            same_value(row.values.get(property).unwrap_or(&Value::Null), old)
        })
    })
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
                    None,
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

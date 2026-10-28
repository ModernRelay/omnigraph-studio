//! Node properties written from a table's cells (spec 004), and from a node's page, its peek or
//! its text editor (spec 014): bound to the values they were read at, so a write changes
//! nothing if a value changed elsewhere meanwhile.

use std::collections::HashMap;

use gpui::{App, Entity, Task};
use gq_language::input::same_value;
use gq_language::{BuiltQuery, SchemaModel, builder};
use graph_client::WriteOutcome;
use graph_client::api::ChangeOutput;
use graph_store::{AccessAction, AccessScope, GraphSession};
use serde_json::{Map, Value};

use crate::model::{Notice, SaveResult, Tone, published, save_result};

/// Why the credential may not change `branch`, once the access route refused it (spec 002):
/// every control that writes there is disabled with this as its reason. `None` while it may,
/// or before the route answered, so the control stays offered and the server decides.
pub(crate) fn change_refusal(
    session: &Entity<GraphSession>,
    branch: &str,
    cx: &App,
) -> Option<String> {
    session
        .read(cx)
        .refusal(AccessAction::Change, &AccessScope::branch(branch))
}

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

    /// Whether it published a commit on its branch, setting every one of the `rows` nodes it
    /// was sent for: a commit held off every branch, or of a kind not known, did not land.
    pub(crate) fn landed(&self, rows: usize) -> bool {
        match self {
            Bound::Sent(WriteOutcome::Committed(output)) => {
                output.affected_nodes as usize == rows
                    && published(output.commit.as_ref(), "").is_ok_and(|commit| commit.is_some())
            }
            _ => false,
        }
    }
}

/// One node a write sets, by id, with each property it sets and the value it was read at.
type Read = (String, Vec<(String, Value)>);

/// Writes properties of node `id` to `branch`, the one they were read on, in one commit, each
/// `(property, the value it was read at, the new value)`, and resolves with how it ended; `Err`
/// when no write can be built. `read_at` is the commit the caller read the values at, when it
/// read them there (see [`send_bound`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_properties(
    session: &Entity<GraphSession>,
    branch: &str,
    schema: &SchemaModel,
    type_name: &str,
    id: &str,
    cells: &[(String, Value, Value)],
    read_at: Option<String>,
    cx: &mut App,
) -> Result<Task<SaveResult>, String> {
    let row = builder::RowEdit {
        id: id.to_owned(),
        cells: cells.to_vec(),
    };
    let description = match cells {
        [(property, _, _)] => format!("Set {property} of {id}"),
        _ => format!("Save {} changes of {id}", cells.len()),
    };
    let sent = write_rows(
        session,
        branch,
        schema,
        type_name,
        &[row],
        read_at,
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
/// otherwise the nodes holding a list are read together at the branch's head, and the write is
/// bound to the commit they were found at. A change made elsewhere since the values were read
/// is found by the comparison, and one made after it by the head having moved.
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
    let written = read.len();
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
    let ids: Vec<String> = lists.iter().map(|(id, _)| id.clone()).collect();
    let mut properties: Vec<String> = lists
        .iter()
        .flat_map(|(_, read)| read.iter().map(|(property, _)| property.clone()))
        .collect();
    properties.sort();
    properties.dedup();
    let nodes = builder::nodes_fields_by_id(schema, type_name, &ids, &properties)
        .map_err(|error| error.to_string())?;
    let session = session.clone();
    Ok(cx.spawn(async move |cx| {
        let output = match session
            .update(cx, |session, cx| session.read_on(&branch, &nodes, cx))
            .await
        {
            Ok(output) => output,
            Err(error) => {
                return Bound::Unread(Notice::new(
                    Tone::Refused,
                    format!("Nothing was written. {error}"),
                ));
            }
        };
        let Some(head) = output.graph_commit_id.clone() else {
            return Bound::Changed;
        };
        // Every node the write sets gone: nothing to send.
        let found = found(&output.rows);
        if lists.len() == written && lists.iter().all(|(id, _)| !found.contains_key(id.as_str())) {
            return Bound::Changed;
        }
        if !holds(&found, &lists) {
            return Bound::Changed;
        }
        let write = session.update(cx, |session, cx| {
            session.write_on(&branch, &query, Some(head), description, None, cx)
        });
        Bound::Sent(write.await)
    }))
}

/// The nodes a `nodes_fields_by_id` read found, by id.
fn found(rows: &Value) -> HashMap<&str, &Map<String, Value>> {
    rows.as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let row = row.as_object()?;
            Some((row.get("id")?.as_str()?, row))
        })
        .collect()
}

/// Whether each node `found` holds each value as `read` says. A node not found is gone: its
/// statement sets nothing, so it fails only its own part of the write, which the commit's
/// `affected_nodes` comes back short by and the caller reads again, finding the row gone; the
/// other nodes are written.
fn holds(found: &HashMap<&str, &Map<String, Value>>, read: &[Read]) -> bool {
    read.iter().all(|(id, values)| {
        found.get(id.as_str()).is_none_or(|row| {
            values.iter().all(|(property, old)| {
                let held = row.get(&builder::field_column(property));
                same_value(held.unwrap_or(&Value::Null), old)
            })
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

    /// Every write sent, by path and the commit it was bound to.
    type Sent = Arc<Mutex<Vec<(String, Option<String>)>>>;

    /// A server whose head `c5` holds `a, b` in the tags of every node asked for but those in
    /// `gone`, and commits `c6` on it: the session, the writes sent, and the snapshot of every
    /// read.
    fn tagged_session(
        gone: &'static [&'static str],
        cx: &mut TestAppContext,
    ) -> (Entity<GraphSession>, Sent, Arc<Mutex<Vec<Value>>>) {
        use futures::AsyncReadExt as _;
        let writes: Sent = Arc::default();
        let reads = Arc::new(Mutex::new(Vec::<Value>::new()));
        let (sent, read) = (writes.clone(), reads.clone());
        let http = FakeHttpClient::create(move |request| {
            let (sent, read) = (sent.clone(), read.clone());
            async move {
                let path = request.uri().path().to_owned();
                let header = request
                    .headers()
                    .get(graph_client::IF_GRAPH_COMMIT_HEADER)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                let body = if path.ends_with("/query") {
                    let mut bytes = Vec::new();
                    request.into_body().read_to_end(&mut bytes).await.ok();
                    let asked: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                    read.lock().unwrap().push(asked["snapshot"].clone());
                    // Each node asked for, holding `a, b`.
                    let ids = asked["params"]["ids"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default();
                    let rows: Vec<Value> = ids
                        .iter()
                        .filter(|id| !gone.contains(&id.as_str().unwrap_or_default()))
                        .map(|id| json!({ "id": id, "v_tags": ["a", "b"] }))
                        .collect();
                    json!({
                        "query_name": "studio_nodes_fields",
                        "target": { "branch": "main", "snapshot": null },
                        "row_count": rows.len(),
                        "columns": ["id", "v_tags"],
                        "rows": rows,
                        "graph_commit_id": "c5",
                    })
                } else if path.contains("/mutate") {
                    sent.lock().unwrap().push((path, header));
                    crate::test_support::committed("c6", "c5")
                } else {
                    json!({ "branches": ["main"] })
                };
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
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
        (session, writes, reads)
    }

    /// A list cannot be compared in a write's `where`: its write is sent only when the value
    /// at the branch's head is the one read, and bound to that head. A bulk set reads every
    /// row there in one read before it is sent.
    #[gpui::test]
    async fn a_list_write_is_bound_to_the_head_it_found_the_value_at(cx: &mut TestAppContext) {
        let (session, writes, reads) = tagged_session(&[], cx);
        let schema = gq_language::SchemaModel::parse(TAGGED).unwrap();
        let save = |old: Value, cx: &mut TestAppContext| {
            let task = cx.update(|cx| {
                write_properties(
                    &session,
                    "main",
                    &schema,
                    "Person",
                    "p1",
                    &[("tags".into(), old, json!(["a", "c"]))],
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
        {
            let writes = writes.lock().unwrap();
            assert_eq!(writes.len(), 1);
            assert!(writes[0].0.ends_with("/if-graph-commit"), "{}", writes[0].0);
            assert_eq!(writes[0].1.as_deref(), Some("c5"));
        }

        reads.lock().unwrap().clear();
        let rows: Vec<(String, Value)> = (0..20)
            .map(|i| (format!("q{i}"), json!(["a", "b"])))
            .collect();
        let task = cx.update(|cx| {
            write_cells(
                &session,
                "main",
                &schema,
                "Person",
                "tags",
                &rows,
                &json!(["c"]),
                None,
                "Set tags".into(),
                cx,
            )
            .unwrap()
        });
        cx.run_until_parked();
        let bound = cx.foreground_executor().block_on(task);
        assert!(matches!(bound, Bound::Sent(WriteOutcome::Committed(_))));
        let reads = reads.lock().unwrap();
        assert_eq!(*reads, vec![Value::Null], "one read, at the branch's head");
        let writes = writes.lock().unwrap();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[1].1.as_deref(), Some("c5"));
    }

    /// A node gone at the head fails only its own part of a list write: its statement sets
    /// nothing, the other nodes are written, bound to that head, and the commit's count comes
    /// back short by it. A write whose every node is gone sends nothing.
    #[gpui::test]
    async fn a_node_gone_elsewhere_fails_only_its_own_part_of_a_list_write(
        cx: &mut TestAppContext,
    ) {
        let (session, writes, _) = tagged_session(&["gone"], cx);
        let schema = gq_language::SchemaModel::parse(TAGGED).unwrap();
        let rows = vec![
            ("q1".to_owned(), json!(["a", "b"])),
            ("gone".to_owned(), json!(["a", "b"])),
        ];
        let set = |rows: &[(String, Value)], cx: &mut TestAppContext| {
            let task = cx.update(|cx| {
                write_cells(
                    &session,
                    "main",
                    &schema,
                    "Person",
                    "tags",
                    rows,
                    &json!(["c"]),
                    None,
                    "Set tags".into(),
                    cx,
                )
                .unwrap()
            });
            cx.run_until_parked();
            cx.foreground_executor().block_on(task)
        };
        assert!(matches!(
            set(&rows, cx),
            Bound::Sent(WriteOutcome::Committed(_))
        ));
        {
            let writes = writes.lock().unwrap();
            assert_eq!(writes.len(), 1);
            assert_eq!(writes[0].1.as_deref(), Some("c5"), "bound to the head read");
        }
        assert!(matches!(set(&rows[1..], cx), Bound::Changed));
        assert_eq!(writes.lock().unwrap().len(), 1, "nothing more sent");
    }
}

//! What the agent's `omnigraph` CLI may send through Studio's gateway (spec 013). Reads go
//! through as they are. A write goes through only when it names the conversation's agent branch
//! in a structured field; a write that names its target only in query text (branch and commit
//! statements) or names no branch (`commit publish`, `commit drop`) is refused, so nothing the
//! gateway cannot classify reaches the server. The gateway narrows what the person's token may
//! do; the server's policy still decides (constitution IV).

use serde_json::{Map, Value};

/// The conversation's sandbox: its graph, its own branch, and the branch that one starts from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sandbox {
    pub graph: String,
    pub branch: String,
    pub base: String,
}

/// One request as the gateway read it. `path` is percent-encoded as it came, query pairs are
/// decoded, and `json` is the body when it is JSON.
#[derive(Debug)]
pub struct Incoming<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a [(String, String)],
    pub json: Option<&'a Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteKind {
    Mutate,
    Load,
    BlobPut,
    BlobClear,
    BranchCreate,
    BranchDelete,
    Merge,
    StoredQuery,
}

/// What the person sees on the write's card.
#[derive(Debug, Clone, PartialEq)]
pub struct WriteSummary {
    pub kind: WriteKind,
    pub summary: String,
    /// The query text, for a mutation.
    pub query: Option<String>,
    pub params: Option<Map<String, Value>>,
    /// The write creates the agent's branch.
    pub creates_branch: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Forward as it came.
    Read,
    /// Forward with this JSON body instead of the one that came.
    ReadWith(Value),
    /// A write to the agent's branch: forward once the person allows it.
    Write(WriteSummary),
    /// Refuse with this message, before anything is sent.
    Refuse(String),
}

pub fn classify(request: &Incoming<'_>, sandbox: &Sandbox) -> Verdict {
    let segments: Vec<&str> = request
        .path
        .trim_start_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    let reading = matches!(request.method, "GET" | "HEAD");
    match segments.as_slice() {
        ["readyz"] | ["openapi.json"] if reading => Verdict::Read,
        ["graphs"] | ["graphs", "discovery"] if reading => Verdict::Read,
        ["cluster", ..] if reading => Verdict::Read,
        ["cluster", ..] => Verdict::Refuse(
            "Studio does not let the agent change the cluster's configuration. Nothing was sent."
                .into(),
        ),
        ["graphs", graph, rest @ ..] => graph_route(request, sandbox, graph, rest),
        _ => Verdict::Refuse(format!(
            "Studio does not forward {} {}. Nothing was sent.",
            request.method, request.path
        )),
    }
}

fn graph_route(request: &Incoming<'_>, sandbox: &Sandbox, graph: &str, rest: &[&str]) -> Verdict {
    if matches!(request.method, "GET" | "HEAD") {
        return Verdict::Read;
    }
    match (request.method, rest) {
        ("POST", ["query"]) | ("POST", ["export"]) | ("POST", ["changes", "baseline"]) => {
            return Verdict::Read;
        }
        _ => {}
    }
    if decode(graph) != sandbox.graph {
        return Verdict::Refuse(format!(
            "Studio forwards the agent's writes to graph {} only. Nothing was sent.",
            sandbox.graph
        ));
    }
    let on_agent_branch = |branch: Option<&str>| branch == Some(sandbox.branch.as_str());
    match (request.method, rest) {
        ("POST", ["mutate"]) | ("POST", ["mutate", "if-graph-commit"]) => {
            let branch = json_str(request.json, "branch");
            if branch.is_none() {
                return Verdict::Refuse(statement_refusal(sandbox));
            }
            if !on_agent_branch(branch) {
                return Verdict::Refuse(branch_refusal(sandbox));
            }
            let query = json_str(request.json, "query").map(str::to_owned);
            Verdict::Write(WriteSummary {
                kind: WriteKind::Mutate,
                summary: match json_str(request.json, "name") {
                    Some(name) => format!("Mutation {name}"),
                    None => "Mutation".into(),
                },
                query,
                params: request
                    .json
                    .and_then(|json| json.get("params"))
                    .and_then(Value::as_object)
                    .cloned(),
                creates_branch: false,
            })
        }
        ("POST", ["load", "ndjson"]) | ("POST", ["load"]) => {
            let (branch, from, mode) = if rest == ["load"] {
                (
                    json_str(request.json, "branch"),
                    json_str(request.json, "from"),
                    json_str(request.json, "mode"),
                )
            } else {
                (
                    query_value(request.query, "branch"),
                    query_value(request.query, "from"),
                    query_value(request.query, "mode"),
                )
            };
            if !on_agent_branch(branch) {
                return Verdict::Refuse(branch_refusal(sandbox));
            }
            if let Some(from) = from
                && from != sandbox.base
            {
                return Verdict::Refuse(format!(
                    "Studio: the agent's branch {} starts from {}. Pass `--from {}`, or leave \
                     `--from` out once the branch exists. Nothing was sent.",
                    sandbox.branch, sandbox.base, sandbox.base
                ));
            }
            Verdict::Write(WriteSummary {
                kind: WriteKind::Load,
                summary: format!("Load ({}) into {}", mode.unwrap_or("?"), sandbox.branch),
                query: None,
                params: None,
                creates_branch: from.is_some(),
            })
        }
        ("PUT", ["blob"]) | ("DELETE", ["blob"]) => {
            if !on_agent_branch(query_value(request.query, "branch")) {
                return Verdict::Refuse(branch_refusal(sandbox));
            }
            let cell = ["type", "id", "property"]
                .map(|key| query_value(request.query, key).unwrap_or("?"))
                .join(".");
            let clear = request.method == "DELETE";
            Verdict::Write(WriteSummary {
                kind: if clear {
                    WriteKind::BlobClear
                } else {
                    WriteKind::BlobPut
                },
                summary: format!("{} Blob {cell}", if clear { "Clear" } else { "Set" }),
                query: None,
                params: None,
                creates_branch: false,
            })
        }
        ("POST", ["branches"]) => {
            let name = json_str(request.json, "name");
            let from = json_str(request.json, "from").unwrap_or("main");
            if name != Some(sandbox.branch.as_str()) || from != sandbox.base {
                return Verdict::Refuse(format!(
                    "Studio: the agent may create only its own branch, {} from {}: `omnigraph \
                     branch create {} --from {}`. Nothing was sent.",
                    sandbox.branch, sandbox.base, sandbox.branch, sandbox.base
                ));
            }
            Verdict::Write(WriteSummary {
                kind: WriteKind::BranchCreate,
                summary: format!("Create {} from {}", sandbox.branch, sandbox.base),
                query: None,
                params: None,
                creates_branch: true,
            })
        }
        ("DELETE", ["branches", name]) => {
            if decode(name) != sandbox.branch {
                return Verdict::Refuse(format!(
                    "Studio: the agent may delete only its own branch, {}. Nothing was sent.",
                    sandbox.branch
                ));
            }
            Verdict::Write(WriteSummary {
                kind: WriteKind::BranchDelete,
                summary: format!("Delete {}", sandbox.branch),
                query: None,
                params: None,
                creates_branch: false,
            })
        }
        ("POST", ["branches", "merge"]) => {
            let target = json_str(request.json, "target");
            if !on_agent_branch(target) {
                return Verdict::Refuse(format!(
                    "Studio: the agent may merge only into its own branch, {}. Merging into \
                     other branches is the person's, in review. Nothing was sent.",
                    sandbox.branch
                ));
            }
            if request
                .json
                .and_then(|json| json.get("delete_branch"))
                .and_then(Value::as_bool)
                == Some(true)
            {
                return Verdict::Refuse(
                    "Studio: a merge that deletes its source branch is not forwarded. Nothing \
                     was sent."
                        .into(),
                );
            }
            Verdict::Write(WriteSummary {
                kind: WriteKind::Merge,
                summary: format!(
                    "Merge {} into {}",
                    json_str(request.json, "source").unwrap_or("?"),
                    sandbox.branch
                ),
                query: None,
                params: None,
                creates_branch: false,
            })
        }
        ("POST", ["queries", name]) | ("POST", ["queries", name, "if-graph-commit"]) => {
            if on_agent_branch(json_str(request.json, "branch")) {
                return Verdict::Write(WriteSummary {
                    kind: WriteKind::StoredQuery,
                    summary: format!("Stored query {}", decode(name)),
                    query: None,
                    params: request
                        .json
                        .and_then(|json| json.get("params"))
                        .and_then(Value::as_object)
                        .cloned(),
                    creates_branch: false,
                });
            }
            // Off the agent's branch a stored query may only read: the server refuses one that
            // writes when the caller says it does not.
            let mut body = request
                .json
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            match body.as_object_mut() {
                Some(object) => {
                    object.insert("expect_mutation".into(), Value::Bool(false));
                    Verdict::ReadWith(body)
                }
                None => Verdict::Refuse(
                    "Studio: a stored query's body must be a JSON object. Nothing was sent.".into(),
                ),
            }
        }
        ("POST", ["commits", _, "publish"]) | ("DELETE", ["commits", _]) => Verdict::Refuse(
            "Studio does not let the agent publish or drop commits. Nothing was sent.".into(),
        ),
        _ => Verdict::Refuse(format!(
            "Studio does not forward {} {}. Nothing was sent.",
            request.method, request.path
        )),
    }
}

fn branch_refusal(sandbox: &Sandbox) -> String {
    format!(
        "Studio: the agent writes only to its own branch, {}. Pass `--branch {}`. Nothing was \
         sent.",
        sandbox.branch, sandbox.branch
    )
}

/// A mutation naming no branch: a plain mutation without `--branch`, or branch and commit
/// statements, whose target is only in the query text.
fn statement_refusal(sandbox: &Sandbox) -> String {
    format!(
        "Studio: the mutation names no branch. Pass `--branch {}` (create it first with \
         `omnigraph branch create {} --from {}`); branch and commit statements, whose target is \
         only in the query text, are not forwarded. Nothing was sent.",
        sandbox.branch, sandbox.branch, sandbox.base
    )
}

fn json_str<'a>(json: Option<&'a Value>, key: &str) -> Option<&'a str> {
    json?.get(key)?.as_str()
}

fn query_value<'a>(query: &'a [(String, String)], key: &str) -> Option<&'a str> {
    query
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

fn decode(segment: &str) -> String {
    percent_encoding::percent_decode_str(segment)
        .decode_utf8_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sandbox() -> Sandbox {
        Sandbox {
            graph: "knowledge".into(),
            branch: "agent/20261013-101500".into(),
            base: "main".into(),
        }
    }

    fn verdict(method: &str, path: &str, query: &[(&str, &str)], json: Option<Value>) -> Verdict {
        let query: Vec<(String, String)> = query
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        classify(
            &Incoming {
                method,
                path,
                query: &query,
                json: json.as_ref(),
            },
            &sandbox(),
        )
    }

    fn refused(verdict: &Verdict) -> bool {
        matches!(verdict, Verdict::Refuse(_))
    }

    const AGENT: &str = "agent/20261013-101500";

    #[test]
    fn reads_go_anywhere_and_health_is_answered_here() {
        for (method, path) in [
            ("GET", "/graphs"),
            ("GET", "/graphs/other/schema"),
            ("GET", "/graphs/knowledge/commits"),
            ("HEAD", "/graphs/knowledge/blob"),
            ("POST", "/graphs/other/query"),
            ("POST", "/graphs/knowledge/export"),
            ("POST", "/graphs/knowledge/changes/baseline"),
        ] {
            assert_eq!(
                verdict(method, path, &[], None),
                Verdict::Read,
                "{method} {path}"
            );
        }
    }

    #[test]
    fn mutations_need_the_agent_branch_in_their_branch_field() {
        let write = verdict(
            "POST",
            "/graphs/knowledge/mutate",
            &[],
            Some(json!({"query": "query m() { … }", "name": "m", "branch": AGENT})),
        );
        let Verdict::Write(summary) = write else {
            panic!("{write:?}")
        };
        assert_eq!(summary.kind, WriteKind::Mutate);
        assert_eq!(summary.query.as_deref(), Some("query m() { … }"));

        assert!(refused(&verdict(
            "POST",
            "/graphs/knowledge/mutate",
            &[],
            Some(json!({"query": "q", "branch": "main"})),
        )));
        // Branch and commit statements send no branch: their target is in the text.
        let statement = verdict(
            "POST",
            "/graphs/knowledge/mutate",
            &[],
            Some(json!({"query": "branch merge \"x\" into \"main\"", "branch": null})),
        );
        assert!(matches!(&statement, Verdict::Refuse(text) if text.contains("statements")));
        // Another graph's agent-named branch is still not this conversation's.
        assert!(refused(&verdict(
            "POST",
            "/graphs/other/mutate",
            &[],
            Some(json!({"query": "q", "branch": AGENT})),
        )));
    }

    #[test]
    fn loads_name_their_branch_and_base_in_the_query_string() {
        let write = verdict(
            "POST",
            "/graphs/knowledge/load/ndjson",
            &[("branch", AGENT), ("mode", "append"), ("from", "main")],
            None,
        );
        assert!(
            matches!(&write, Verdict::Write(s) if s.creates_branch && s.kind == WriteKind::Load)
        );
        assert!(matches!(
            verdict(
                "POST",
                "/graphs/knowledge/load/ndjson",
                &[("branch", AGENT), ("mode", "merge")],
                None
            ),
            Verdict::Write(WriteSummary {
                creates_branch: false,
                ..
            })
        ));
        // No branch means main; another base is refused.
        assert!(refused(&verdict(
            "POST",
            "/graphs/knowledge/load/ndjson",
            &[("mode", "append")],
            None
        )));
        assert!(refused(&verdict(
            "POST",
            "/graphs/knowledge/load/ndjson",
            &[("branch", AGENT), ("mode", "append"), ("from", "edits")],
            None
        )));
    }

    #[test]
    fn branches_merges_and_blobs_stay_on_the_agent_branch() {
        assert!(matches!(
            verdict(
                "POST",
                "/graphs/knowledge/branches",
                &[],
                Some(json!({"name": AGENT, "from": "main"}))
            ),
            Verdict::Write(WriteSummary {
                kind: WriteKind::BranchCreate,
                creates_branch: true,
                ..
            })
        ));
        assert!(refused(&verdict(
            "POST",
            "/graphs/knowledge/branches",
            &[],
            Some(json!({"name": "feature", "from": "main"}))
        )));
        assert!(matches!(
            verdict(
                "DELETE",
                "/graphs/knowledge/branches/agent%2F20261013-101500",
                &[],
                None
            ),
            Verdict::Write(WriteSummary {
                kind: WriteKind::BranchDelete,
                ..
            })
        ));
        assert!(refused(&verdict(
            "DELETE",
            "/graphs/knowledge/branches/main",
            &[],
            None
        )));

        assert!(matches!(
            verdict(
                "POST",
                "/graphs/knowledge/branches/merge",
                &[],
                Some(json!({"source": "main", "target": AGENT}))
            ),
            Verdict::Write(WriteSummary {
                kind: WriteKind::Merge,
                ..
            })
        ));
        assert!(refused(&verdict(
            "POST",
            "/graphs/knowledge/branches/merge",
            &[],
            Some(json!({"source": AGENT, "target": "main"}))
        )));
        assert!(refused(&verdict(
            "POST",
            "/graphs/knowledge/branches/merge",
            &[],
            Some(json!({"source": "main", "target": AGENT, "delete_branch": true}))
        )));

        assert!(matches!(
            verdict(
                "PUT",
                "/graphs/knowledge/blob",
                &[
                    ("branch", AGENT),
                    ("type", "Note"),
                    ("id", "n1"),
                    ("property", "attachment")
                ],
                None
            ),
            Verdict::Write(WriteSummary {
                kind: WriteKind::BlobPut,
                ..
            })
        ));
        assert!(refused(&verdict(
            "DELETE",
            "/graphs/knowledge/blob",
            &[("type", "Note"), ("id", "n1"), ("property", "attachment")],
            None
        )));
    }

    #[test]
    fn stored_queries_read_elsewhere_and_commits_are_never_published() {
        let read = verdict(
            "POST",
            "/graphs/knowledge/queries/people",
            &[],
            Some(json!({"params": {}, "branch": "main"})),
        );
        assert_eq!(
            read,
            Verdict::ReadWith(json!({"params": {}, "branch": "main", "expect_mutation": false}))
        );
        assert!(matches!(
            verdict(
                "POST",
                "/graphs/knowledge/queries/retitle",
                &[],
                Some(json!({"branch": AGENT}))
            ),
            Verdict::Write(WriteSummary {
                kind: WriteKind::StoredQuery,
                ..
            })
        ));
        assert!(refused(&verdict(
            "POST",
            "/graphs/knowledge/commits/c1/publish",
            &[],
            None
        )));
        assert!(refused(&verdict(
            "DELETE",
            "/graphs/knowledge/commits/c1",
            &[],
            None
        )));
        assert!(refused(&verdict("POST", "/cluster/deployments", &[], None)));
        assert!(refused(&verdict(
            "PATCH",
            "/graphs/knowledge/schema",
            &[],
            None
        )));
    }
}

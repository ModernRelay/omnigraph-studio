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
    /// Whether the write creates the agent's branch.
    pub creates_branch: Creates,
}

/// Whether a write creates the agent's branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Creates {
    Never,
    /// A branch create.
    Always,
    /// A load given `from`, which creates the branch only when it does not exist yet: its
    /// answer says whether it did (`branch_created`). The gateway's own refusal of a load tells
    /// the agent to pass `--from`, so such a load often lands on a branch that exists.
    IfMissing,
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
    let flag = |key: &str| {
        request
            .json
            .and_then(|json| json.get(key))
            .and_then(Value::as_bool)
            == Some(true)
    };
    match (request.method, rest) {
        ("POST", ["query"]) | ("POST", ["export"]) | ("POST", ["changes", "baseline"]) => {
            return Verdict::Read;
        }
        // A merge's dry run reads both branches and writes nothing: a preview, into any branch
        // of any graph, as the other reads are.
        ("POST", ["branches", "merge"])
            if flag("dry_run") && !flag("as_commit") && !flag("delete_branch") =>
        {
            return Verdict::Read;
        }
        // A stored query its caller says reads (`omnigraph query <name>` sends `expect_mutation:
        // false`) runs only if it reads: the server refuses one that writes when the caller says
        // it does not. So it is a read, on any graph and branch, the agent's own included.
        ("POST", ["queries", _])
            if request
                .json
                .and_then(|json| json.get("expect_mutation"))
                .and_then(Value::as_bool)
                == Some(false) =>
        {
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
            let name = json_str(request.json, "name");
            // A mutation held as an unpublished commit or an unattached snapshot does not move
            // the agent's branch: Studio lists such commits nowhere, and publishing one is the
            // person's.
            if query
                .as_deref()
                .is_some_and(|query| held_off_the_branch(query, name))
            {
                return Verdict::Refuse(
                    "Studio: a mutation written `as commit` or `as unattached branch` is held as a \
                     commit no branch shows, and is not forwarded; leave the clause out to write \
                     to the agent's branch. Nothing was sent."
                        .into(),
                );
            }
            Verdict::Write(WriteSummary {
                kind: WriteKind::Mutate,
                summary: match name {
                    Some(name) => format!("Mutation {name}"),
                    None => "Mutation".into(),
                },
                query,
                params: request
                    .json
                    .and_then(|json| json.get("params"))
                    .and_then(Value::as_object)
                    .cloned(),
                creates_branch: Creates::Never,
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
                // The server merges a load whose mode is left out, so the card says it.
                summary: format!("Load ({}) into {}", mode.unwrap_or("merge"), sandbox.branch),
                query: None,
                params: None,
                creates_branch: if from.is_some() {
                    Creates::IfMissing
                } else {
                    Creates::Never
                },
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
                creates_branch: Creates::Never,
            })
        }
        ("POST", ["branches"]) => {
            let name = json_str(request.json, "name");
            let from = json_str(request.json, "from").unwrap_or("main");
            // The agent's branch starts at its base's head: one started at a commit (`from
            // commit`) holds whatever that commit held, which the card cannot show.
            let at_commit = request
                .json
                .and_then(|json| json.get("from_commit"))
                .is_some_and(|commit| !commit.is_null());
            if name != Some(sandbox.branch.as_str()) || from != sandbox.base || at_commit {
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
                creates_branch: Creates::Always,
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
                creates_branch: Creates::Never,
            })
        }
        // A dry run was read above.
        ("POST", ["branches", "merge"]) => {
            // A merge held as an unpublished commit is listed nowhere in Studio, and publishing
            // it is the person's.
            if flag("as_commit") {
                return Verdict::Refuse(
                    "Studio: a merge held as an unpublished commit is not forwarded; merge into \
                     the agent's branch as it is. Nothing was sent."
                        .into(),
                );
            }
            let target = json_str(request.json, "target");
            if !on_agent_branch(target) {
                return Verdict::Refuse(format!(
                    "Studio: the agent may merge only into its own branch, {}. Merging into \
                     other branches is the person's, in review. Nothing was sent.",
                    sandbox.branch
                ));
            }
            if flag("delete_branch") {
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
                creates_branch: Creates::Never,
            })
        }
        // One that may write (`omnigraph mutate <name>`, or a caller that does not say) is a
        // write on the agent's branch; its answer says whether it wrote.
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
                    creates_branch: Creates::Never,
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

/// Whether the declaration a mutation runs, `name` (every declaration when none is named, or
/// when none has that name), is written `as commit` or `as unattached branch`: the server then
/// holds the write as a commit no branch shows rather than moving the branch.
///
/// The clause stands in a declaration's head, between its parameters (and annotations) and the
/// `{` of its body (`query_decl` in the compiler's grammar), where `gq_language`'s lexer marks
/// `as commit` and `as unattached` as keywords; comments between the words are passed over, as
/// the server passes them over, and a string or a comment holding the words is not read as
/// them. The body is not read: there `as` names a column. A source in which the lexer finds no
/// declaration is read whole, so what it cannot place is not let through unread.
fn held_off_the_branch(query: &str, name: Option<&str>) -> bool {
    use gq_language::TokenKind;
    let tokens = gq_language::tokenize(query);
    let mut declarations: Vec<std::ops::Range<usize>> = Vec::new();
    let outline = gq_language::outline(query).declarations;
    let named = outline
        .iter()
        .any(|declaration| Some(declaration.name.as_str()) == name);
    for declaration in &outline {
        if !named || Some(declaration.name.as_str()) == name {
            declarations.push(declaration.range.clone());
        }
    }
    let body_opens = |token: &gq_language::Token| {
        !outline.is_empty()
            && token.kind == TokenKind::Punctuation
            && &query[token.range.clone()] == "{"
    };
    if outline.is_empty() {
        declarations.push(0..query.len());
    }
    let keyword = |token: &gq_language::Token, word: &str| {
        token.kind == TokenKind::Keyword && &query[token.range.clone()] == word
    };
    declarations.iter().any(|declaration| {
        let head: Vec<&gq_language::Token> = tokens
            .iter()
            .filter(|token| declaration.contains(&token.range.start))
            .filter(|token| token.kind != TokenKind::Comment)
            .take_while(|token| !body_opens(token))
            .collect();
        head.windows(2).any(|pair| {
            keyword(pair[0], "as") && (keyword(pair[1], "commit") || keyword(pair[1], "unattached"))
        })
    })
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

    /// A read goes through on any graph, branch or snapshot. The health probe never reaches the
    /// rules: the gateway answers it itself (`gateway`'s
    /// `the_gateway_checks_the_session_then_forwards_reads_and_approved_writes`).
    #[test]
    fn reads_go_anywhere() {
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

        // A mutation held as an unpublished commit or an unattached snapshot never moves the
        // agent's branch: refused, as its words are written in the declaration's head.
        let mutation = |query: &str, name: Option<&str>| {
            verdict(
                "POST",
                "/graphs/knowledge/mutate",
                &[],
                Some(json!({"query": query, "name": name, "branch": AGENT})),
            )
        };
        for held in [
            "query add($n: String) as commit { insert Person { name: $n } }",
            "query add() @description(\"one\") as /* held */ commit {\n insert Person { name: \"a\" }\n}",
            "query add() as unattached branch { insert Person { name: \"a\" } }",
            "query add($b: String) as unattached branch on $b { insert Person { name: \"a\" } }",
            "set x = 1;\nquery add() as unattached branch on \"01J\" { delete Person where name = \"a\" }",
        ] {
            let refusal = mutation(held, None);
            assert!(
                matches!(&refusal, Verdict::Refuse(text) if text.contains("as commit")),
                "{held}: {refusal:?}"
            );
        }
        let two = "query keep() { insert Person { name: \"a\" } }\n\
                   query hold() as commit { insert Person { name: \"b\" } }";
        assert!(refused(&mutation(two, Some("hold"))));
        assert!(refused(&mutation(two, None)), "unnamed, either may run");
        assert!(refused(&mutation(two, Some("missing"))));
        // The words elsewhere than the head are not the clause.
        for written in [
            "query keep() { insert Person { name: \"as commit\" } }",
            "// as commit\nquery keep() { insert Person { name: \"a\" } }",
            "query keep() /* as commit */ { insert Person { name: \"a\" } }",
        ] {
            assert!(
                matches!(mutation(written, None), Verdict::Write(_)),
                "{written}"
            );
        }
        assert!(matches!(mutation(two, Some("keep")), Verdict::Write(_)));
    }

    #[test]
    fn loads_name_their_branch_and_base_in_the_query_string() {
        let write = verdict(
            "POST",
            "/graphs/knowledge/load/ndjson",
            &[("branch", AGENT), ("mode", "append"), ("from", "main")],
            None,
        );
        // Given `from`, the load creates the branch only when it is missing: its answer says.
        assert!(matches!(&write, Verdict::Write(s)
            if s.creates_branch == Creates::IfMissing && s.kind == WriteKind::Load));
        assert!(matches!(
            verdict(
                "POST",
                "/graphs/knowledge/load/ndjson",
                &[("branch", AGENT), ("mode", "merge")],
                None
            ),
            Verdict::Write(WriteSummary {
                creates_branch: Creates::Never,
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
                creates_branch: Creates::Always,
                ..
            })
        ));
        assert!(refused(&verdict(
            "POST",
            "/graphs/knowledge/branches",
            &[],
            Some(json!({"name": "feature", "from": "main"}))
        )));
        // Started at a commit, the branch holds what that commit held, not its base's head.
        assert!(refused(&verdict(
            "POST",
            "/graphs/knowledge/branches",
            &[],
            Some(json!({"name": AGENT, "from_commit": "01J"}))
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
        // A dry run writes nothing: a read, into any branch of any graph. A merge held as an
        // unpublished commit is never forwarded.
        for graph in ["knowledge", "other"] {
            assert_eq!(
                verdict(
                    "POST",
                    &format!("/graphs/{graph}/branches/merge"),
                    &[],
                    Some(json!({"source": AGENT, "target": "main", "dry_run": true}))
                ),
                Verdict::Read,
                "{graph}"
            );
        }
        assert!(refused(&verdict(
            "POST",
            "/graphs/other/branches/merge",
            &[],
            Some(json!({"source": "main", "target": AGENT, "dry_run": true, "as_commit": true}))
        )));
        assert!(refused(&verdict(
            "POST",
            "/graphs/knowledge/branches/merge",
            &[],
            Some(json!({"source": "main", "target": AGENT, "as_commit": true}))
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

    /// A stored query the CLI sends as a read (`omnigraph query <name>`, `expect_mutation:
    /// false`) is a read wherever it runs, the agent's branch and another graph included: the
    /// server refuses it if it writes. One that may write is a write on the agent's branch.
    #[test]
    fn a_stored_read_is_a_read_anywhere_and_commits_are_never_published() {
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
        for (graph, branch) in [
            ("knowledge", AGENT),
            ("knowledge", "main"),
            ("other", AGENT),
        ] {
            assert_eq!(
                verdict(
                    "POST",
                    &format!("/graphs/{graph}/queries/people"),
                    &[],
                    Some(json!({"params": {}, "branch": branch, "expect_mutation": false}))
                ),
                Verdict::Read,
                "{graph} {branch}"
            );
        }
        for body in [
            json!({"branch": AGENT}),
            json!({"branch": AGENT, "expect_mutation": true}),
            json!({"branch": AGENT, "expect_mutation": null}),
        ] {
            assert!(
                matches!(
                    verdict(
                        "POST",
                        "/graphs/knowledge/queries/retitle",
                        &[],
                        Some(body.clone())
                    ),
                    Verdict::Write(WriteSummary {
                        kind: WriteKind::StoredQuery,
                        ..
                    })
                ),
                "{body}"
            );
        }
        // The conditional route runs stored mutations only: it stays a write.
        assert!(matches!(
            verdict(
                "POST",
                "/graphs/knowledge/queries/retitle/if-graph-commit",
                &[],
                Some(json!({"branch": AGENT, "expect_mutation": false}))
            ),
            Verdict::Write(_)
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

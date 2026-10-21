//! Diagnostics and result types for `.gq` source, from `omnigraph-compiler` (spec 007).

use omnigraph_compiler::error::CompilerError;
use omnigraph_compiler::query::ast::{
    BranchStmt, BranchWrite, CommitWrite, Expr, FileBody, LoadStmtMode, LoadTarget, Mutation,
    MutationWriteMode, QueryDecl,
};
use omnigraph_compiler::query::diagnostic::QueryDiagnostic;
use omnigraph_compiler::query::parser::parse_query_diagnostic;
use omnigraph_compiler::query::typecheck::{
    BoundVariable, CheckedQuery, executed_column_name, typecheck_query_decl,
};
use omnigraph_compiler::{QueryResultFieldDescriptor, QueryValueKind, describe_query_operation};

use crate::schema::SchemaModel;

/// One problem in the source, with a position when the compiler gives one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: String,
    pub message: String,
    pub fix: Option<String>,
    /// 1-based line and column, and the byte offset, when known.
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub byte: Option<u32>,
    /// The query declaration the problem belongs to, when known.
    pub query: Option<String>,
    /// The compiler's suggested edits to the checked source, as byte ranges and their
    /// replacements; empty when it suggests none (spec 007, quick fixes).
    pub edits: Vec<(std::ops::Range<usize>, String)>,
}

impl Diagnostic {
    fn from_compiler(diagnostic: &QueryDiagnostic, query: Option<&str>) -> Self {
        Self {
            // `T12`, not the code's `Debug` form.
            code: diagnostic.code.as_str().to_owned(),
            message: diagnostic.message.clone(),
            fix: diagnostic.fix.clone(),
            line: diagnostic.position.as_ref().map(|p| p.line),
            column: diagnostic.position.as_ref().map(|p| p.column),
            byte: diagnostic.position.as_ref().map(|p| p.byte),
            query: query.map(str::to_owned),
            edits: diagnostic
                .suggestion
                .as_ref()
                .map(|suggestion| {
                    suggestion
                        .edits
                        .iter()
                        .map(|edit| (edit.start..edit.end, edit.replacement.clone()))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// A result column a read query will return, known before it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultColumn {
    /// The name the compiler's inferred schema gives it (`name` for `$p.name`).
    pub name: String,
    /// The name of its column in the server's answer (`p.name` for `$p.name`, an alias as
    /// written): the compiler's `executed_column_name`, which the executor names its result
    /// batch by. A view of the answer names and matches its columns by this one.
    pub column: String,
    /// The value's kind as a type reads: `String`, `Int`, `[Date]`, `Vector(3)`, or the node
    /// type of a whole node.
    pub kind: String,
    pub nullable: bool,
    /// The node type of a column that returns a whole node (`return { $p }`), from the
    /// variable's binding.
    pub node_type: Option<String>,
}

/// A result column's kind as a type reads: `String`, `[Date]`, `Vector(3)`, a whole node's
/// type.
fn kind_label(field: &QueryResultFieldDescriptor, node_type: Option<&str>) -> String {
    let name = |kind: QueryValueKind| match kind {
        QueryValueKind::String => "String",
        QueryValueKind::Bool => "Bool",
        QueryValueKind::Int => "Int",
        QueryValueKind::BigInt => "BigInt",
        QueryValueKind::Float => "Float",
        QueryValueKind::Date => "Date",
        QueryValueKind::DateTime => "DateTime",
        QueryValueKind::Blob => "Blob",
        QueryValueKind::Vector => "Vector",
        QueryValueKind::List => "List",
        QueryValueKind::Object => "Object",
    };
    match (field.kind, field.item_kind, field.vector_dim) {
        (QueryValueKind::List, Some(item), _) => format!("[{}]", name(item)),
        (QueryValueKind::Vector, _, Some(dim)) => format!("Vector({dim})"),
        (QueryValueKind::Object, ..) if let Some(node_type) = node_type => node_type.to_owned(),
        (kind, ..) => name(kind).to_owned(),
    }
}

/// One declaration in the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryInfo {
    pub name: String,
    pub is_mutation: bool,
    /// `(name, type, nullable)` for each parameter.
    pub params: Vec<(String, String, bool)>,
    pub columns: Vec<ResultColumn>,
    /// What running it destroys: the rows a `delete` removes from its branch.
    pub destroys: Option<Destroys>,
    /// Where a write's result goes: the branch it is sent to, or held off every branch.
    pub publication: Publication,
}

/// Where a write's result goes, by how it is declared (`as commit`, `as unattached branch`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Publication {
    /// It publishes on the branch it is sent to.
    #[default]
    Branch,
    /// `as commit`: held as an unpublished commit, made on the branch's head, which moves only
    /// when the commit is published; it can be dropped instead.
    Held,
    /// `as unattached branch`: an unattached snapshot, read by its id, which never publishes.
    Unattached,
}

impl Publication {
    fn of(mode: &MutationWriteMode) -> Self {
        match mode {
            MutationWriteMode::Publish => Self::Branch,
            MutationWriteMode::AsCommit => Self::Held,
            MutationWriteMode::Unattached { .. } => Self::Unattached,
        }
    }
}

/// Where the statement `source` puts what it writes, read from its parse alone: a `branch
/// merge … as commit` holds the merge as an unpublished commit; every other statement, and text
/// that does not parse, is [`Publication::Branch`].
pub fn statement_publication(source: &str) -> Publication {
    match parse_query_diagnostic(source).map(|file| file.body) {
        Ok(FileBody::Branch(BranchStmt::Write(BranchWrite::Merge {
            as_commit: true, ..
        }))) => Publication::Held,
        _ => Publication::Branch,
    }
}

/// What the declaration `name` in `source` does with what it writes, read from its parse alone:
/// where its result goes, and what it destroys that Studio cannot put back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeclaredWrite {
    pub publication: Publication,
    pub destroys: Option<Destroys>,
}

/// [`DeclaredWrite`] for the declaration `name` in `source`, for a source checked against no
/// schema yet: neither needs the schema, so a destructive write waits for its name typed
/// whether the schema is loaded or not. [`QueryInfo::publication`] and [`QueryInfo::destroys`]
/// say the same once it is checked. Text that does not parse, or holds no such declaration,
/// publishes on its branch and destroys nothing; the server refuses it before it writes.
pub fn declared_write(source: &str, name: &str) -> DeclaredWrite {
    match parse_query_diagnostic(source).map(|file| file.body) {
        Ok(FileBody::Queries(decls)) => decls.iter().find(|decl| decl.name == name).map_or_else(
            DeclaredWrite::default,
            |decl| DeclaredWrite {
                publication: Publication::of(&decl.write_mode),
                destroys: deletes(decl),
            },
        ),
        _ => DeclaredWrite::default(),
    }
}

/// What a write destroys that Studio cannot put back (constitution IV), so it is confirmed by
/// typing what it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destroys {
    /// `branch delete <name>`: the branch, and its commits no other branch holds.
    Branch(String),
    /// `commit drop "<id>"`: the unpublished commit.
    Commit(String),
    /// `load … mode overwrite`: every row of the type, replaced by the file's.
    Overwrite { type_name: String, edge: bool },
    /// A `delete` that publishes: what its `where` matches of each type it names, in order.
    Rows(Vec<String>),
}

/// What running the statement `source` destroys, read from its parse alone: a `branch delete`,
/// a `commit drop` or an overwriting `load`. `None` for any other statement, for declarations
/// (their [`QueryInfo::destroys`] says), and for text that does not parse, which the server
/// refuses before it writes.
pub fn statement_destroys(source: &str) -> Option<Destroys> {
    match parse_query_diagnostic(source).ok()?.body {
        FileBody::Branch(BranchStmt::Write(BranchWrite::Delete { name })) => {
            Some(Destroys::Branch(name))
        }
        FileBody::Commit(CommitWrite::Drop { graph_commit_id }) => {
            Some(Destroys::Commit(graph_commit_id))
        }
        FileBody::Load(load) if load.mode == LoadStmtMode::Overwrite => Some(match load.target {
            LoadTarget::Node(type_name) => Destroys::Overwrite {
                type_name,
                edge: false,
            },
            LoadTarget::Edge(type_name) => Destroys::Overwrite {
                type_name,
                edge: true,
            },
        }),
        _ => None,
    }
}

/// The branch the statement `source` writes to, read from its parse alone, as the server
/// resolves it: the branch a `branch create` makes or a `branch delete` deletes, a `branch
/// merge`'s target (`main` when it names none), and a `load`'s request branch, which is `main`
/// when the request names none. `None` for a `commit publish` or `commit drop`, whose target
/// the proposal recorded and only the server's answer names, for statements that write nothing,
/// for declarations, and for text that does not parse.
pub fn statement_branch(source: &str) -> Option<String> {
    match parse_query_diagnostic(source).ok()?.body {
        FileBody::Branch(BranchStmt::Write(
            BranchWrite::Create { name, .. } | BranchWrite::Delete { name },
        )) => Some(name),
        FileBody::Branch(BranchStmt::Write(BranchWrite::Merge { into, .. })) => {
            Some(into.unwrap_or_else(|| "main".to_owned()))
        }
        FileBody::Load(_) => Some("main".to_owned()),
        _ => None,
    }
}

/// The types a declaration's `delete`s remove rows from, when its result publishes: a write
/// `as commit` or `as unattached branch` moves no branch, so it deletes nothing there.
fn deletes(decl: &QueryDecl) -> Option<Destroys> {
    if decl.write_mode != MutationWriteMode::Publish {
        return None;
    }
    let mut types: Vec<String> = Vec::new();
    for mutation in &decl.mutations {
        if let Mutation::Delete(delete) = mutation
            && !types.contains(&delete.type_name)
        {
            types.push(delete.type_name.clone());
        }
    }
    (!types.is_empty()).then_some(Destroys::Rows(types))
}

/// What the compiler says about a `.gq` source.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryCheck {
    pub diagnostics: Vec<Diagnostic>,
    /// One per declaration; none for a statement (`branch …`, `show …`, `commit …`, `load …`,
    /// `explain load …`), which declares no query.
    pub queries: Vec<QueryInfo>,
}

/// Parses and type-checks `.gq` source against a schema.
pub fn check(schema: &SchemaModel, source: &str) -> QueryCheck {
    let mut result = QueryCheck::default();
    let file = match parse_query_diagnostic(source) {
        Ok(file) => file,
        Err(diagnostic) => {
            result
                .diagnostics
                .push(Diagnostic::from_compiler(&diagnostic, None));
            return result;
        }
    };
    let decls = match file.body {
        FileBody::Queries(decls) => decls,
        FileBody::Explain(decl) => vec![decl],
        FileBody::Branch(_)
        | FileBody::Show(_)
        | FileBody::Commit(_)
        | FileBody::Load(_)
        | FileBody::ExplainLoad(_) => return result,
    };
    // A compiler error as a problem of the declaration it is in.
    let problem = |error: &CompilerError, name: &str| match error.diagnostic() {
        Some(diagnostic) => Diagnostic::from_compiler(diagnostic, Some(name)),
        None => Diagnostic {
            code: "error".into(),
            message: error.to_string(),
            fix: None,
            line: None,
            column: None,
            byte: None,
            query: Some(name.to_owned()),
            edits: Vec::new(),
        },
    };
    for decl in &decls {
        let info = QueryInfo {
            name: decl.name.clone(),
            is_mutation: !decl.mutations.is_empty(),
            params: decl
                .params
                .iter()
                .map(|p| (p.name.clone(), p.type_name.clone(), p.nullable))
                .collect(),
            columns: Vec::new(),
            destroys: deletes(decl),
            publication: Publication::of(&decl.write_mode),
        };
        match typecheck_query_decl(&schema.catalog, decl) {
            Ok(checked) => {
                let mut info = info;
                // Columns that return a whole node, by name, with the node's type.
                let node_types: Vec<(String, String)> = match &checked {
                    CheckedQuery::Read(context) => decl
                        .return_clause
                        .iter()
                        .filter_map(|projection| {
                            let Expr::Variable(var) = &projection.expr else {
                                return None;
                            };
                            let Some(BoundVariable::Node { type_name }) =
                                context.bindings().get(var)
                            else {
                                return None;
                            };
                            let name = projection.alias.clone().unwrap_or_else(|| var.clone());
                            Some((name, type_name.clone()))
                        })
                        .collect(),
                    CheckedQuery::Mutation(_) => Vec::new(),
                };
                if !info.is_mutation {
                    match describe_query_operation(&schema.catalog, decl) {
                        Ok(operation) => {
                            // One descriptor per projection, in the `return`'s order; each
                            // knows its column in the server's answer too.
                            info.columns = operation
                                .result
                                .iter()
                                .zip(&decl.return_clause)
                                .map(|(field, projection)| {
                                    let column = executed_column_name(
                                        &projection.expr,
                                        projection.alias.as_deref(),
                                    );
                                    let node_type = node_types
                                        .iter()
                                        .find(|(name, _)| *name == column)
                                        .map(|(_, ty)| ty.clone());
                                    ResultColumn {
                                        name: field.name.clone(),
                                        kind: kind_label(field, node_type.as_deref()),
                                        nullable: field.nullable,
                                        node_type,
                                        column,
                                    }
                                })
                                .collect();
                        }
                        // Checked, but the compiler cannot describe what it returns (a traversal
                        // no binding starts, a list nested in a list): it would fail when run.
                        Err(error) => result.diagnostics.push(problem(&error, &decl.name)),
                    }
                }
                result.queries.push(info);
            }
            Err(error) => {
                result.diagnostics.push(problem(&error, &decl.name));
                result.queries.push(info);
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::tests::dev;

    #[test]
    fn a_valid_read_reports_its_columns() {
        let result = check(
            &dev(),
            "query q() { match { $p: Person } return { $p.name, $p.born, $p.tags, $p } limit 5 }",
        );
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let columns = &result.queries[0].columns;
        assert_eq!(columns.len(), 4);
        assert!(columns[1].nullable);
        // Each kind reads as a type: a list by its items, a whole node by its type.
        let kinds: Vec<&str> = columns.iter().map(|c| c.kind.as_str()).collect();
        assert_eq!(kinds, ["String", "Date", "[String]", "Person"]);
        // Each knows its column in the server's answer, which its inferred name (`name`) is
        // not.
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["name", "born", "tags", "p"]);
        let columns: Vec<&str> = columns.iter().map(|c| c.column.as_str()).collect();
        assert_eq!(columns, ["p.name", "p.born", "p.tags", "p"]);
        let aliased = check(
            &dev(),
            "query q() { match { $p: Person } return { $p.name as who, count($p) as n } }",
        );
        let columns: Vec<&str> = aliased.queries[0]
            .columns
            .iter()
            .map(|c| c.column.as_str())
            .collect();
        assert_eq!(columns, ["who", "n"]);
    }

    /// A statement's branch is the one the server writes: the branch made or deleted, a
    /// merge's target or main, and main for a load sent without a branch. A commit statement's
    /// target is only in the server's answer, and a read or a declaration writes no branch.
    #[test]
    fn a_statement_names_the_branch_it_writes() {
        for (source, branch) in [
            ("branch create feature from main", Some("feature")),
            ("branch delete feature", Some("feature")),
            ("branch merge feature into release", Some("release")),
            ("branch merge feature", Some("main")),
            (
                "branch merge feature into release as commit",
                Some("release"),
            ),
            (
                "load csv \"s3://b/p.csv\" into node:Person mode append header",
                Some("main"),
            ),
            ("commit publish \"01JCOMMIT\"", None),
            ("commit drop \"01JCOMMIT\"", None),
            ("branch list", None),
            ("branch merge feature into release dry run", None),
            (
                "query q() { match { $p: Person } return { $p.name } }",
                None,
            ),
            ("branch delete", None),
        ] {
            assert_eq!(statement_branch(source).as_deref(), branch, "{source}");
        }
    }

    /// A write says where its result goes: a declaration `as commit` is held, one `as
    /// unattached branch` is a snapshot, any other publishes on its branch; of the statements,
    /// only a merge `as commit` is held.
    #[test]
    fn a_write_says_where_its_result_goes() {
        for (source, publication) in [
            (
                "query q() { insert Person { slug: \"a\" } }",
                Publication::Branch,
            ),
            (
                "query q() as commit { insert Person { slug: \"a\" } }",
                Publication::Held,
            ),
            (
                "query q() as unattached branch { insert Person { slug: \"a\" } }",
                Publication::Unattached,
            ),
        ] {
            let checked = check(&dev(), source);
            assert_eq!(checked.queries[0].publication, publication, "{source}");
            assert_eq!(
                declared_write(source, "q").publication,
                publication,
                "{source}"
            );
        }
        assert_eq!(
            declared_write("query q() as commit { insert Person { slug: \"a\" } }", "r"),
            DeclaredWrite::default(),
            "no such declaration"
        );
        for (source, publication) in [
            (
                "branch merge feature into release as commit",
                Publication::Held,
            ),
            ("branch merge feature into release", Publication::Branch),
            ("branch delete feature", Publication::Branch),
            ("branch merge", Publication::Branch),
        ] {
            assert_eq!(statement_publication(source), publication, "{source}");
        }
    }

    /// What a write destroys that Studio cannot put back: a deleted branch, a dropped commit,
    /// an overwritten type, and the types a published `delete` removes rows from. A proposal's
    /// delete removes nothing from a branch.
    #[test]
    fn writes_say_what_they_destroy() {
        assert_eq!(
            statement_destroys("branch delete feature"),
            Some(Destroys::Branch("feature".into()))
        );
        assert_eq!(
            statement_destroys("commit drop \"01JCOMMIT\""),
            Some(Destroys::Commit("01JCOMMIT".into()))
        );
        assert_eq!(
            statement_destroys("load csv \"s3://b/p.csv\" into node:Person mode overwrite header"),
            Some(Destroys::Overwrite {
                type_name: "Person".into(),
                edge: false
            })
        );
        assert_eq!(
            statement_destroys("load csv \"s3://b/k.csv\" into edge:Knows mode overwrite header"),
            Some(Destroys::Overwrite {
                type_name: "Knows".into(),
                edge: true
            })
        );
        for kept in [
            "load csv \"s3://b/p.csv\" into node:Person mode append header",
            "branch create feature",
            "branch merge feature",
            "commit publish \"01JCOMMIT\"",
            "branch delete",
        ] {
            assert_eq!(statement_destroys(kept), None, "{kept}");
        }
        let deletes = check(
            &dev(),
            "query purge($s: String) {\n  delete Person where slug = $s\n  delete Company where slug = $s\n  delete Person where name = $s\n}",
        );
        assert!(deletes.diagnostics.is_empty(), "{:?}", deletes.diagnostics);
        let rows = Some(Destroys::Rows(vec!["Person".into(), "Company".into()]));
        assert_eq!(deletes.queries[0].destroys, rows);
        // Read from the parse alone, as for a source no schema has checked yet: the same.
        let purge = "query purge($s: String) {\n  delete Person where slug = $s\n  delete Company where slug = $s\n  delete Person where name = $s\n}";
        assert_eq!(declared_write(purge, "purge").destroys, rows);
        let proposed = check(
            &dev(),
            "query purge($s: String) as commit {\n  delete Person where slug = $s\n}",
        );
        assert_eq!(proposed.queries[0].destroys, None);
        assert_eq!(
            declared_write(
                "query purge($s: String) as commit {\n  delete Person where slug = $s\n}",
                "purge"
            ),
            DeclaredWrite {
                publication: Publication::Held,
                destroys: None
            }
        );
        let insert = check(
            &dev(),
            "query add($s: String) {\n  insert Company { slug: $s, name: $s }\n}",
        );
        assert_eq!(insert.queries[0].destroys, None);
    }

    #[test]
    fn whole_node_columns_carry_their_type() {
        let result = check(
            &dev(),
            "query q() { match { $p: Person  $p worksAt $c } return { $p, $c as employer, $p.name } }",
        );
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let types: Vec<Option<&str>> = result.queries[0]
            .columns
            .iter()
            .map(|c| c.node_type.as_deref())
            .collect();
        assert_eq!(types, [Some("Person"), Some("Company"), None]);
    }

    #[test]
    fn a_parse_error_has_a_position() {
        let result = check(&dev(), "query q() { match { $p: Person } return { $p.name ");
        assert_eq!(result.diagnostics.len(), 1);
        assert!(result.diagnostics[0].line.is_some());
        assert_eq!(result.diagnostics[0].code, "Q001");
    }

    #[test]
    fn a_type_error_names_the_query() {
        let result = check(
            &dev(),
            "query q() { match { $p: Person } return { $p.nope } }",
        );
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].query.as_deref(), Some("q"));
    }

    #[test]
    fn statements_are_recognised() {
        let result = check(&dev(), "branch list");
        assert!(result.diagnostics.is_empty() && result.queries.is_empty());
    }

    #[test]
    fn mutations_and_parameter_types_are_reported() {
        let result = check(
            &dev(),
            "query add($slug: String, $founded: I32?, $tags: [String]?) {\n  insert Company { slug: $slug, name: $slug, founded: $founded }\n}",
        );
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let query = &result.queries[0];
        assert!(query.is_mutation);
        assert!(query.columns.is_empty());
        assert_eq!(query.params[0], ("slug".into(), "String".into(), false));
        assert_eq!(query.params[1], ("founded".into(), "I32".into(), true));
        assert_eq!(query.params[2].1, "[String]");
    }

    #[test]
    fn a_missing_parameter_list_comes_with_its_edit() {
        let schema = crate::schema::tests::dev();
        let source = "query people { match { $p: Person } return { $p.name } }";
        let check = check(&schema, source);
        let problem = &check.diagnostics[0];
        assert_eq!(problem.code, "Q002");
        assert_eq!(problem.edits, [(12..12, "()".to_owned())]);
        let mut fixed = source.to_owned();
        for (range, text) in problem.edits.iter().rev() {
            fixed.replace_range(range.clone(), text);
        }
        assert!(super::check(&schema, &fixed).diagnostics.is_empty());
    }
}

//! Diagnostics and result types for `.gq` source, from `omnigraph-compiler` (spec 007).

use omnigraph_compiler::error::CompilerError;
use omnigraph_compiler::query::ast::Expr;
use omnigraph_compiler::query::ast::FileBody;
use omnigraph_compiler::query::diagnostic::QueryDiagnostic;
use omnigraph_compiler::query::parser::parse_query_diagnostic;
use omnigraph_compiler::query::typecheck::{BoundVariable, CheckedQuery, typecheck_query_decl};
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
    pub name: String,
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
                            info.columns = operation
                                .result
                                .iter()
                                .map(|field| {
                                    let node_type = node_types
                                        .iter()
                                        .find(|(name, _)| *name == field.name)
                                        .map(|(_, ty)| ty.clone());
                                    ResultColumn {
                                        name: field.name.clone(),
                                        kind: kind_label(field, node_type.as_deref()),
                                        nullable: field.nullable,
                                        node_type,
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

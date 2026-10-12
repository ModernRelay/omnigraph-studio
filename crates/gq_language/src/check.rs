//! Diagnostics and result types for `.gq` source, from `omnigraph-compiler` (spec 007).

use omnigraph_compiler::describe_query_operation;
use omnigraph_compiler::query::ast::FileBody;
use omnigraph_compiler::query::diagnostic::QueryDiagnostic;
use omnigraph_compiler::query::parser::parse_query_diagnostic;
use omnigraph_compiler::query::typecheck::typecheck_query_decl;

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
        }
    }
}

/// A result column a read query will return, known before it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultColumn {
    pub name: String,
    /// The compiler's value kind, for example `String`, `I64`, `Date`, `Node`.
    pub kind: String,
    pub nullable: bool,
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
    pub queries: Vec<QueryInfo>,
    /// The source is a single statement (`branch …`, `show …`, `explain …`) rather than
    /// query declarations.
    pub statement: bool,
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
        FileBody::Branch(_) | FileBody::Show(_) => {
            result.statement = true;
            return result;
        }
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
            Ok(_) => {
                let mut info = info;
                if !info.is_mutation
                    && let Ok(operation) = describe_query_operation(&schema.catalog, decl)
                {
                    info.columns = operation
                        .result
                        .iter()
                        .map(|field| ResultColumn {
                            name: field.name.clone(),
                            kind: format!("{:?}", field.kind),
                            nullable: field.nullable,
                        })
                        .collect();
                }
                result.queries.push(info);
            }
            Err(error) => {
                let diagnostic = match error.diagnostic() {
                    Some(diagnostic) => Diagnostic::from_compiler(diagnostic, Some(&decl.name)),
                    None => Diagnostic {
                        code: "error".into(),
                        message: error.to_string(),
                        fix: None,
                        line: None,
                        column: None,
                        byte: None,
                        query: Some(decl.name.clone()),
                    },
                };
                result.diagnostics.push(diagnostic);
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
            "query q() { match { $p: Person } return { $p.name, $p.born } limit 5 }",
        );
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let columns = &result.queries[0].columns;
        assert_eq!(columns.len(), 2);
        assert!(columns[1].nullable);
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
        assert!(result.statement);
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
}

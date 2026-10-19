//! What the editor knows about its text after a check: the problems in the whole source and,
//! for each declaration on its own, what the compiler says (mutation or read, parameters,
//! result columns). A broken declaration does not hide what the others declare.

use std::ops::Range;

use gq_language::{Diagnostic, Outline, QueryInfo, SchemaModel};

#[derive(Debug, Clone, Default)]
pub struct Analysis {
    pub text: String,
    pub outline: Outline,
    /// Problems in the whole source, with the range to underline when the position is known.
    pub problems: Vec<(Diagnostic, Option<Range<usize>>)>,
    /// One entry per declaration in `outline`, in order.
    pub queries: Vec<Option<QueryInfo>>,
}

impl Analysis {
    /// Checks `text` against `schema`. Runs off the main thread.
    pub fn new(schema: &SchemaModel, text: String) -> Self {
        let outline = gq_language::outline(&text);
        let checked = gq_language::check(schema, &text);
        let problems = checked
            .diagnostics
            .into_iter()
            .map(|diagnostic| {
                let byte = diagnostic
                    .byte
                    .map(|b| b as usize)
                    .or_else(|| {
                        Some(gq_language::offset_of(
                            &text,
                            diagnostic.line?,
                            diagnostic.column?,
                        ))
                    })
                    .or_else(|| locate(&text, &outline, &diagnostic));
                let range = byte.map(|byte| gq_language::problem_range(&text, byte));
                (diagnostic, range)
            })
            .collect();
        // The whole text's check describes each declaration it parsed; only text that does not
        // parse is checked again a declaration at a time, so one broken query leaves the others
        // runnable.
        let queries = outline
            .declarations
            .iter()
            .map(|declaration| {
                checked
                    .queries
                    .iter()
                    .find(|query| query.name == declaration.name)
                    .cloned()
                    .or_else(|| {
                        let own = outline.runnable_text(&text, declaration);
                        gq_language::check(schema, &own)
                            .queries
                            .into_iter()
                            .find(|query| query.name == declaration.name)
                    })
            })
            .collect();
        Self {
            text,
            outline,
            problems,
            queries,
        }
    }

    /// The declaration under `offset` and what the compiler says about it.
    pub fn query_at(
        &self,
        offset: usize,
    ) -> Option<(&gq_language::Declaration, Option<&QueryInfo>)> {
        let declaration = self.outline.declaration_at(offset)?;
        let index = self
            .outline
            .declarations
            .iter()
            .position(|d| d == declaration)?;
        Some((
            declaration,
            self.queries.get(index).and_then(Option::as_ref),
        ))
    }
}

/// Where a type error without a position probably is: the last name the message quotes
/// (`` type `Person` has no property `nmae` ``) that appears as a token in the declaration the
/// error names. Only an underline hint; the message stays the authority.
fn locate(text: &str, outline: &Outline, diagnostic: &Diagnostic) -> Option<usize> {
    let name = diagnostic.query.as_deref()?;
    let declaration = outline.declarations.iter().find(|d| d.name == name)?;
    let body = &text[declaration.range.clone()];
    let tokens = gq_language::tokenize(body);
    let quoted: Vec<&str> = diagnostic
        .message
        .split('`')
        .skip(1)
        .step_by(2)
        .filter(|quoted| !quoted.is_empty())
        .collect();
    quoted.into_iter().rev().find_map(|quoted| {
        tokens
            .iter()
            .find(|token| &body[token.range.clone()] == quoted)
            .map(|token| declaration.range.start + token.range.start)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEV_SCHEMA: &str = include_str!("../../../dev/cluster/knowledge.pg");

    #[test]
    fn a_broken_declaration_keeps_its_siblings_known() {
        let schema = SchemaModel::parse(DEV_SCHEMA).unwrap();
        let text = "query people() {\n  match { $p: Person }\n  return { $p.name }\n}\n\nquery add($slug: String) {\n  insert Company { slug: $slug, name: $slug }\n}\n\nquery broken() { match { $p: Person } return { $p.name ";
        let analysis = Analysis::new(&schema, text.to_owned());
        assert_eq!(analysis.outline.declarations.len(), 3);
        // The whole source does not parse, so there is one problem, at the end.
        assert_eq!(analysis.problems.len(), 1);
        assert!(analysis.problems[0].1.is_some());
        let people = analysis.queries[0].as_ref().unwrap();
        assert!(!people.is_mutation);
        assert_eq!(people.columns.len(), 1);
        let add = analysis.queries[1].as_ref().unwrap();
        assert!(add.is_mutation);
        assert_eq!(add.params[0].0, "slug");
        assert!(analysis.queries[2].is_none());
        let inside_add = text.find("insert").unwrap();
        let (declaration, info) = analysis.query_at(inside_add).unwrap();
        assert_eq!(declaration.name, "add");
        assert!(info.unwrap().is_mutation);
    }

    #[test]
    fn type_errors_are_underlined_where_they_are() {
        let schema = SchemaModel::parse(DEV_SCHEMA).unwrap();
        let text = "query q() { match { $p: Person } return { $p.nope } }";
        let analysis = Analysis::new(&schema, text.to_owned());
        assert_eq!(analysis.problems.len(), 1);
        let (diagnostic, range) = &analysis.problems[0];
        assert_eq!(diagnostic.query.as_deref(), Some("q"));
        // The compiler gives no position for this one; the quoted name locates it.
        assert_eq!(&text[range.clone().unwrap()], "nope");
    }

    /// Instrument for spike S4's budget (diagnostics < 100 ms after typing stops on a 500-line
    /// file): `cargo test -p query_editor --release -- --ignored --nocapture analysis_time`.
    #[test]
    #[ignore = "instrument: prints the time to analyse, and to complete in, a 500-line source"]
    fn analysis_time_on_a_500_line_source() {
        let schema = SchemaModel::parse(DEV_SCHEMA).unwrap();
        let one = "query q{n}($slug: String) {\n  match {\n    $p: Person { slug: $slug }\n    $p worksAt $c\n  }\n  return { $p.name, $c.name }\n  limit 10\n}\n";
        let text: String = (0..63)
            .map(|n| one.replace("{n}", &n.to_string()))
            .collect();
        assert!(text.lines().count() >= 500);
        let started = std::time::Instant::now();
        let runs = 10;
        for _ in 0..runs {
            let analysis = Analysis::new(&schema, text.clone());
            assert!(analysis.problems.is_empty());
            assert_eq!(analysis.queries.len(), 63);
        }
        println!(
            "analysis of {} lines: {:?} per run",
            text.lines().count(),
            started.elapsed() / runs
        );
        // A completion at the end of the source, as each key in an open menu asks.
        let started = std::time::Instant::now();
        for _ in 0..runs {
            let at = text.rfind("$c.name").unwrap() + 3;
            let (_, items) = gq_language::completions(&schema, &text, at);
            assert!(!items.is_empty());
        }
        println!(
            "completion in {} lines: {:?} per key",
            text.lines().count(),
            started.elapsed() / runs
        );
    }
}

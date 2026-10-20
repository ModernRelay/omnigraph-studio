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
        // The whole text's check describes each declaration it parsed. One it did not is
        // checked on its own, so one broken query leaves the others runnable, and its problems
        // are that check's: the whole text's stops at its first.
        let mut queries = Vec::new();
        let mut own_problems = Vec::new();
        let mut checked_alone: Vec<&gq_language::Declaration> = Vec::new();
        for declaration in &outline.declarations {
            if let Some(query) = checked.queries.iter().find(|q| q.name == declaration.name) {
                queries.push(Some(query.clone()));
                continue;
            }
            let own_text = outline.runnable_text(&text, declaration);
            let own = gq_language::check(schema, &own_text);
            let own_outline = gq_language::outline(&own_text);
            own_problems.extend(own.diagnostics.into_iter().filter_map(|diagnostic| {
                placed(
                    diagnostic,
                    &text,
                    (&own_text, &own_outline),
                    outline.prefix.end,
                    &declaration.range,
                )
            }));
            queries.push(
                own.queries
                    .into_iter()
                    .find(|query| query.name == declaration.name),
            );
            checked_alone.push(declaration);
        }
        let mut problems: Vec<(Diagnostic, Option<Range<usize>>)> = checked
            .diagnostics
            .into_iter()
            .map(|diagnostic| {
                // The compiler gives a problem's byte with its line and column, or none of them.
                let byte = diagnostic
                    .byte
                    .map(|b| b as usize)
                    .or_else(|| locate(&text, &outline, &diagnostic));
                let range = byte.map(|byte| gq_language::problem_range(&text, byte));
                (diagnostic, range)
            })
            .filter(|(diagnostic, range)| {
                !checked_alone.iter().any(|declaration| {
                    diagnostic.query.as_deref() == Some(declaration.name.as_str())
                        || range
                            .as_ref()
                            .is_some_and(|range| declaration.range.contains(&range.start))
                })
            })
            .collect();
        problems.extend(own_problems);
        // In the order of the text; problems with no place last.
        problems.sort_by_key(|(_, range)| range.as_ref().map_or(usize::MAX, |range| range.start));
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

/// A problem a declaration's own check found, moved from that check's text (`own`: the
/// settings prefix, `prefix` bytes, then the declaration) to where the declaration is in
/// `text`. One in the prefix is the whole text's check's to report. A problem at the end of
/// the declaration is placed on its last word, not on the declaration after it.
fn placed(
    mut diagnostic: Diagnostic,
    text: &str,
    own: (&str, &Outline),
    prefix: usize,
    declaration: &Range<usize>,
) -> Option<(Diagnostic, Option<Range<usize>>)> {
    // Its last word: an unclosed declaration runs on to the next, over blank lines and any
    // comment above it.
    let end = gq_language::tokenize(&text[declaration.clone()])
        .iter()
        .rev()
        .find(|token| token.kind != gq_language::TokenKind::Comment)
        .map_or(declaration.start, |token| {
            declaration.start + token.range.end
        });
    let moved = |at: usize| (at >= prefix).then(|| (declaration.start + at - prefix).min(end));
    let at = match diagnostic.byte {
        Some(byte) => {
            let at = moved(byte as usize)?;
            let before = &text[..at];
            diagnostic.byte = Some(at as u32);
            diagnostic.line = Some(before.matches('\n').count() as u32 + 1);
            diagnostic.column = Some(
                before
                    .rsplit('\n')
                    .next()
                    .map_or(0, |line| line.chars().count()) as u32
                    + 1,
            );
            Some(at)
        }
        None => match locate(own.0, own.1, &diagnostic) {
            Some(at) => Some(moved(at)?),
            None => None,
        },
    };
    // An edit that reaches into the prefix has no place in the declaration: none is offered.
    diagnostic.edits = std::mem::take(&mut diagnostic.edits)
        .into_iter()
        .map(|(range, with)| Some((moved(range.start)?..moved(range.end)?, with)))
        .collect::<Option<Vec<_>>>()
        .unwrap_or_default();
    let range = at.map(|at| gq_language::problem_range(&text[..end], at));
    Some((diagnostic, range))
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
        // The whole source does not parse; its one problem is the broken query's.
        assert_eq!(analysis.problems.len(), 1);
        let at = analysis.problems[0].1.as_ref().unwrap().start;
        assert!(at >= text.find("query broken").unwrap(), "{at}");
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

        // Nor does it hide the problems of the ones after it.
        let text = "query broken() { match { $p: Person } return { $p.name\n\n\
            query q() { match { $p: Person } return { $p.nope } }";
        let analysis = Analysis::new(&schema, text.to_owned());
        let at: Vec<usize> = analysis
            .problems
            .iter()
            .filter_map(|(_, range)| range.as_ref().map(|range| range.start))
            .collect();
        assert_eq!(at.len(), 2, "{:?}", analysis.problems);
        assert!(at[0] < text.find("query q").unwrap(), "{at:?}");
        assert_eq!(at[1], text.find("nope").unwrap());
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

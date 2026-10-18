//! The tree-sitter grammar for OmniGraph queries (`.gq`), with the queries Studio's editor uses
//! for highlighting, brackets, indentation and outline (spec 007). `src/parser.c` is generated
//! from `grammar.js` by the tree-sitter CLI; see `grammars/README.md`.

use tree_sitter_language::LanguageFn;

unsafe extern "C" {
    fn tree_sitter_gq() -> *const ();
}

/// The GQ language, for `tree_sitter::Language::new`.
pub const LANGUAGE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_gq) };

pub const HIGHLIGHTS_QUERY: &str = include_str!("../../queries/highlights.scm");
pub const BRACKETS_QUERY: &str = include_str!("../../queries/brackets.scm");
pub const INDENTS_QUERY: &str = include_str!("../../queries/indents.scm");
pub const OUTLINE_QUERY: &str = include_str!("../../queries/outline.scm");

#[cfg(test)]
mod tests {
    use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator as _};

    use super::*;

    fn parse(source: &str) -> tree_sitter::Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&Language::new(LANGUAGE))
            .expect("the GQ grammar loads");
        parser.parse(source, None).expect("a tree")
    }

    const EVERY_CLAUSE: &str = r#"
set engine = v2;
// a comment
query q($n: String, $v: Vector(3)?) @description("people") {
  match {
    $p: Person { name: $n }
    $o: @Operator
    $p $w:knows $f
    $p <knows | likes> $g
    $p (worksAt | owns) {1, 3} $c
    $p "in" $h
    $p <"in"> $i
    $p ("in" | knows) $j
    $f.age > 25 and not $f.retired
    count { $f knows $x } > 2
    not { $p owns $z }
    $f.@id in [date("2026-01-01"), "x"]
  }
  return distinct { $p.name as name, count(distinct $f) as n }
  order { nearest($p.embedding, $v), n desc }
  limit 10
}
query add($name: String) {
  insert Knows { from: $name, to: "b" }
  update Person set { age: 1 } where name = $name
  delete Person where @id = $name
}
query propose($name: String) as commit {
  insert Person { name: $name }
}
query child($base: String) as unattached branch on $base {
  insert Person { name: "c" }
}
"#;

    /// Every statement form, one after another; the server takes one per request.
    const EVERY_STATEMENT: &str = r#"
show all;
branch create feature from main;
branch delete "old";
branch merge dry into main
branch merge feature into main as commit
branch merge feature into main dry run;
branch merge-base feature into main;
branch diff feature into main;
branch diff "01J" to main after page "next" limit 10;
branch list;
commit publish "01J";
commit drop "01J"
load csv "s3://bucket/people.csv" into node:Person mode overwrite delimiter "|" header columns (id, name);
explain load csv "knows.csv" into edge:Knows mode append
explain query q() { match { $p: Person } return { $p.name } }
"#;

    #[test]
    fn every_clause_kind_parses_without_errors() {
        let tree = parse(EVERY_CLAUSE);
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        let sexp = tree.root_node().to_sexp();
        assert_eq!(sexp.matches("(traversal").count(), 8, "{sexp}");
        assert!(sexp.contains("(subquery_predicate"), "{sexp}");
        assert!(sexp.contains("(insert_statement"), "{sexp}");
        assert!(sexp.contains("result: (as_commit)"), "{sexp}");
        assert!(
            sexp.contains("result: (as_unattached_branch base: (variable))"),
            "{sexp}"
        );
    }

    #[test]
    fn every_statement_parses_without_errors() {
        let tree = parse(EVERY_STATEMENT);
        let sexp = tree.root_node().to_sexp();
        assert!(!tree.root_node().has_error(), "{sexp}");
        assert_eq!(sexp.matches("(branch_statement").count(), 9, "{sexp}");
        assert_eq!(sexp.matches("(commit_statement").count(), 2, "{sexp}");
        assert_eq!(sexp.matches("(load_statement").count(), 2, "{sexp}");
        assert_eq!(sexp.matches("(explain_statement").count(), 2, "{sexp}");
        assert!(
            sexp.contains("(as_commit)") && sexp.contains("(dry_run)"),
            "{sexp}"
        );
    }

    #[test]
    fn statement_and_clause_words_highlight_as_keywords() {
        let language = Language::new(LANGUAGE);
        let highlights = Query::new(&language, HIGHLIGHTS_QUERY).expect("the query compiles");
        let keyword = highlights.capture_index_for_name("keyword").unwrap();
        let mut words = Vec::new();
        for source in [EVERY_STATEMENT, EVERY_CLAUSE] {
            let tree = parse(source);
            let mut cursor = QueryCursor::new();
            let mut matches = cursor.matches(&highlights, tree.root_node(), source.as_bytes());
            while let Some(found) = matches.next() {
                for capture in found.captures.iter().filter(|c| c.index == keyword) {
                    words.push(
                        capture
                            .node
                            .utf8_text(source.as_bytes())
                            .unwrap()
                            .to_owned(),
                    );
                }
            }
        }
        for word in [
            "merge-base",
            "diff",
            "to",
            "after",
            "page",
            "dry",
            "run",
            "commit",
            "publish",
            "drop",
            "load",
            "csv",
            "node",
            "edge",
            "mode",
            "overwrite",
            "append",
            "delimiter",
            "header",
            "columns",
            "distinct",
            "unattached",
            "on",
        ] {
            assert!(words.iter().any(|w| w == word), "{word} in {words:?}");
        }
        // A branch named `dry` is a name, so `dry` is a keyword once: in `dry run`.
        assert_eq!(words.iter().filter(|w| *w == "dry").count(), 1, "{words:?}");
    }

    #[test]
    fn a_broken_query_keeps_its_good_parts() {
        let tree = parse("query q() { match { $p: Person $p. } return { $p.name } }");
        assert!(tree.root_node().has_error());
        assert!(tree.root_node().to_sexp().contains("(return_clause"));
    }

    #[test]
    fn the_editor_queries_compile_and_find_declarations() {
        let language = Language::new(LANGUAGE);
        for source in [HIGHLIGHTS_QUERY, BRACKETS_QUERY, INDENTS_QUERY] {
            Query::new(&language, source).expect("the query compiles");
        }
        let outline = Query::new(&language, OUTLINE_QUERY).expect("the outline compiles");
        let tree = parse(EVERY_CLAUSE);
        let name = outline.capture_index_for_name("name").unwrap();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&outline, tree.root_node(), EVERY_CLAUSE.as_bytes());
        let mut names = Vec::new();
        while let Some(found) = matches.next() {
            for capture in found.captures.iter().filter(|c| c.index == name) {
                names.push(
                    capture
                        .node
                        .utf8_text(EVERY_CLAUSE.as_bytes())
                        .unwrap()
                        .to_owned(),
                );
            }
        }
        assert_eq!(names, ["q", "add", "propose", "child"]);
    }
}

//! The tree-sitter grammar for OmniGraph schemas (`.pg`), with the queries Studio's schema view
//! uses for highlighting, brackets, indentation and outline (spec 007). `src/parser.c` is
//! generated from `grammar.js` by the tree-sitter CLI; see `grammars/README.md`.

use tree_sitter_language::LanguageFn;

unsafe extern "C" {
    fn tree_sitter_pg() -> *const ();
}

/// The schema language, for `tree_sitter::Language::new`.
pub const LANGUAGE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_pg) };

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
            .expect("the schema grammar loads");
        parser.parse(source, None).expect("a tree")
    }

    const EVERY_CONSTRUCT: &str = r#"
// A line comment.
/* A block
   comment. */
interface Named {
    name: String @key
}

node Person @description("someone") implements Named {
    age: I32? @index
    tags: [String]?
    status: enum(active, on-hold, done_2)
    embedding: Vector(1536)? @embed("bio", model="text-embedding-3-large")
    bio: String?
    photo: Blob?
    born: Date?
    seen: DateTime?
    score: F64?
    @unique(name, age)
    @range(age, 0..200)
    @range(score, -1.5..)
    @check(name, "^[A-Z]")
}

edge Knows: Person -> Person @card(0..1)

edge WorksAt: Person -> Company {
    since: Date?
    @key(@src, @dst)
}

node Company {
    name: String @key
}
"#;

    #[test]
    fn every_construct_parses_without_errors() {
        let tree = parse(EVERY_CONSTRUCT);
        let sexp = tree.root_node().to_sexp();
        assert!(!tree.root_node().has_error(), "{sexp}");
        assert_eq!(sexp.matches("(edge_declaration").count(), 2, "{sexp}");
        assert_eq!(sexp.matches("(meta_field)").count(), 2, "{sexp}");
        assert!(sexp.contains("(implements"), "{sexp}");
        assert!(sexp.contains("(keyword_argument"), "{sexp}");
    }

    #[test]
    fn a_broken_declaration_keeps_the_ones_after_it() {
        let tree = parse("node Person { name: }\nnode Company { name: String @key }");
        assert!(tree.root_node().has_error());
        let sexp = tree.root_node().to_sexp();
        assert_eq!(sexp.matches("(node_declaration").count(), 2, "{sexp}");
    }

    #[test]
    fn the_view_queries_compile_and_find_declarations() {
        let language = Language::new(LANGUAGE);
        for source in [HIGHLIGHTS_QUERY, BRACKETS_QUERY, INDENTS_QUERY] {
            Query::new(&language, source).expect("the query compiles");
        }
        let outline = Query::new(&language, OUTLINE_QUERY).expect("the outline compiles");
        let tree = parse(EVERY_CONSTRUCT);
        let name = outline.capture_index_for_name("name").unwrap();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&outline, tree.root_node(), EVERY_CONSTRUCT.as_bytes());
        let mut names = Vec::new();
        while let Some(found) = matches.next() {
            for capture in found.captures.iter().filter(|c| c.index == name) {
                names.push(
                    capture
                        .node
                        .utf8_text(EVERY_CONSTRUCT.as_bytes())
                        .unwrap()
                        .to_owned(),
                );
            }
        }
        assert_eq!(names, ["Named", "Person", "Knows", "WorksAt", "Company"]);
    }
}

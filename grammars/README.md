# Grammars

Tree-sitter grammars for OmniGraph's languages, used by the query editor for highlighting,
folding and outline (spec 007):

- `tree-sitter-gq`: queries, mutations and statements (`.gq`) at OmniGraph
  `edge-20261020-7fd9cc9`, the `omnigraph-compiler` tag Studio pins, where the corpus below was
  checked. Built: the query editor's buffer parses with it (`query_editor::syntax`).
- `tree-sitter-pg`: schemas (`.pg`) at OmniGraph v0.13.0. Built: the read-only schema view
  parses with it (`query_editor::schema_view`).

The source of truth for both languages is OmniGraph's pest grammars
(`crates/omnigraph-compiler/src/query/query.pest` and `schema/schema.pest` at the pinned tag).
Type checking, diagnostics and completion come from `omnigraph-compiler` itself, not from these
grammars.

These grammars live here until OmniGraph takes ownership of them together with an
`omnigraph lsp` server (`docs/deferred-engine-changes.md`).

## tree-sitter-gq

- `grammar.js` is the source. It accepts what the pest grammar accepts and recovers around the
  rest; it is for colour and structure, so it is more tolerant than the compiler (a list takes
  any expression, for instance).
- `src/parser.c`, `src/grammar.json` and `src/node-types.json` are generated; regenerate them
  after editing `grammar.js` with the tree-sitter CLI whose ABI Studio's `tree-sitter` reads
  (0.27, ABI 15):

  ```bash
  cd grammars/tree-sitter-gq
  npx --yes tree-sitter-cli@0.27.1 generate --abi 15
  ```

- `queries/` holds the editor's queries: `highlights.scm` (theme names as in
  `gq_language::TokenKind::syntax_name`), `brackets.scm`, `indents.scm` and `outline.scm`.
- `cargo test -p tree-sitter-gq` parses a file that uses every clause kind and one that uses
  every statement, compiles the queries and checks that the statement words highlight as
  keywords. Check a grammar change against OmniGraph's logic-test corpus too: extract the
  `--- query` and `--- mutate` steps of `crates/omnigraph-gqt/cases/**/*.gqt` at the pinned tag
  (leaving out steps whose next `--- expect` is an error) and run `tree-sitter parse --quiet
  --stat` over them with the repository's fixture queries (`crates/omnigraph/tests/fixtures/*.gq`
  at the tag); at `edge-20261020-7fd9cc9` all 1,636 parse without an error node.

## tree-sitter-pg

- `grammar.js` is the source, written from `schema/schema.pest`. A body constraint
  (`@unique(a, b)`, `@range(age, 0..200)`) and an annotation (`@key`, `@embed("text")`) read as
  one `annotation` rule; the compiler tells them apart. Regenerate `src/` as above, from
  `grammars/tree-sitter-pg`.
- `queries/` holds the view's queries, with the theme names the GQ grammar uses: a type, a
  property or a relationship reads the same colour in a schema as in a query.
- `cargo test -p tree-sitter-pg` parses a schema that uses every construct, keeps the
  declarations after a broken one, compiles the queries and checks the outline. Check a grammar
  change against OmniGraph's schemas too: the repository's `.pg` files and the `--- schema`
  sections of the `.gqt` corpus at the pinned tag; at v0.13.0 all 269 parse without an error
  node.

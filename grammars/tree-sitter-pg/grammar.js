/**
 * Tree-sitter grammar for OmniGraph schemas (`.pg`), at OmniGraph v0.13.0.
 *
 * Written from `crates/omnigraph-compiler/src/schema/schema.pest` at that tag, for colour and
 * outline in Studio's read-only schema view. A body constraint (`@unique(a, b)`) and an
 * annotation (`@key`, `@embed("text")`) read as one rule here; the compiler tells them apart.
 */

const commaSep1 = (rule) => seq(rule, repeat(seq(',', rule)));
const commaSep = (rule) => optional(commaSep1(rule));

module.exports = grammar({
  name: 'pg',

  extras: ($) => [/\s/, $.comment],

  word: ($) => $.identifier,

  rules: {
    source_file: ($) =>
      repeat(choice($.interface_declaration, $.node_declaration, $.edge_declaration)),

    comment: (_) =>
      token(choice(seq('//', /[^\n]*/), seq('/*', /[^*]*\*+([^/*][^*]*\*+)*/, '/'))),

    interface_declaration: ($) =>
      seq('interface', field('name', $.type_identifier), '{', repeat($.property), '}'),

    node_declaration: ($) =>
      seq(
        'node',
        field('name', $.type_identifier),
        repeat($.annotation),
        optional($.implements),
        '{',
        repeat(choice($.property, $.annotation)),
        '}',
      ),

    implements: ($) => seq('implements', commaSep1($.type_identifier)),

    edge_declaration: ($) =>
      seq(
        'edge',
        field('name', $.type_identifier),
        ':',
        field('from', $.type_identifier),
        '->',
        field('to', $.type_identifier),
        repeat($.annotation),
        optional(seq('{', repeat(choice($.property, $.annotation)), '}')),
      ),

    property: ($) =>
      prec.right(
        seq(field('name', $.identifier), ':', field('type', $.type), repeat($.annotation)),
      ),

    type: ($) =>
      seq(choice($.list_type, $.enum_type, $.vector_type, $.primitive_type), optional('?')),

    list_type: ($) => seq('[', $.primitive_type, ']'),

    enum_type: ($) => seq('enum', '(', commaSep1($.enum_value), ')'),

    enum_value: (_) => /[A-Za-z0-9_-]+/,

    vector_type: ($) => seq('Vector', '(', $.number, ')'),

    primitive_type: (_) =>
      choice('String', 'Blob', 'Bool', 'I32', 'I64', 'U32', 'U64', 'F32', 'F64', 'DateTime', 'Date'),

    // `@key`, `@embed("text")`, `@card(0..1)`, and body constraints such as `@unique(a, b)`,
    // `@range(age, 0..200)` or `@key(@src, @dst)`.
    annotation: ($) =>
      prec.right(
        seq(
          field('name', $.annotation_name),
          optional(seq('(', commaSep($._argument), ')')),
        ),
      ),

    annotation_name: (_) => token(seq('@', /[A-Za-z_][A-Za-z0-9_]*/)),

    _argument: ($) =>
      choice(
        $.range,
        $.keyword_argument,
        alias($.annotation_name, $.meta_field),
        $.string,
        $.number,
        $.boolean,
        $.identifier,
      ),

    keyword_argument: ($) =>
      seq(field('name', $.identifier), '=', choice($.string, $.number, $.boolean)),

    range: ($) => choice(seq($.number, '..', optional($.number)), seq('..', $.number)),

    type_identifier: (_) => /[A-Z][A-Za-z0-9_]*/,

    identifier: (_) => /[a-z_][A-Za-z0-9_]*/,

    string: (_) => token(seq('"', repeat(choice(/[^"\\]/, /\\./)), '"')),

    number: (_) => /-?\d+(\.\d+)?/,

    boolean: (_) => choice('true', 'false'),
  },
});

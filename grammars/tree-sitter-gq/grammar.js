/**
 * Tree-sitter grammar for OmniGraph queries (`.gq`), GQ at OmniGraph `edge-20261018-0a62bb7`.
 *
 * Written from `crates/omnigraph-compiler/src/query/query.pest` at that tag. It is for
 * highlighting, folding and outline in Studio's query editor: it accepts what the pest grammar
 * accepts and recovers around what it does not. Whether a query is valid is the compiler's
 * answer, never this grammar's.
 */

const PREC = {
  or: 1,
  and: 2,
  not: 3,
  compare: 4,
  // A name before `(` is a call, never a name followed by a parenthesised clause.
  call: 9,
  // A traversal reads `$a edge $b`, which the parser could also take as three filters.
  traversal: 10,
};

const commaSep1 = (rule) => seq(rule, repeat(seq(',', rule)));
const commaSep = (rule) => optional(commaSep1(rule));

module.exports = grammar({
  name: 'gq',

  extras: ($) => [/\s/, $.comment],

  word: ($) => $.identifier,

  conflicts: ($) => [[$._operand, $.traversal]],

  rules: {
    source_file: ($) =>
      repeat(
        choice(
          $.setting_statement,
          $.show_statement,
          $.branch_statement,
          $.commit_statement,
          $.load_statement,
          $.explain_statement,
          $.query_declaration,
        ),
      ),

    comment: (_) =>
      token(choice(seq('//', /[^\n]*/), seq('/*', /[^*]*\*+([^/*][^*]*\*+)*/, '/'))),

    // ---- statements ----

    setting_statement: ($) =>
      choice(
        seq('set', field('name', $.setting_name), '=', field('value', $._setting_value), ';'),
        seq('reset', choice('all', field('name', $.setting_name)), ';'),
      ),

    show_statement: ($) => seq('show', choice('all', field('name', $.setting_name)), ';'),

    setting_name: ($) => seq($.identifier, repeat(seq('.', $.identifier))),

    _setting_value: ($) => choice($.integer, $.identifier, $.string),

    branch_statement: ($) =>
      seq(
        'branch',
        choice(
          seq('create', $._branch_name, optional(seq('from', $._branch_name))),
          seq('delete', $._branch_name),
          seq(
            'merge',
            $._branch_name,
            optional(seq('into', $._branch_name)),
            optional(choice($.as_commit, $.dry_run)),
          ),
          // One word to the server: the lexer takes `merge-base` whole, never `merge` and `-`.
          seq('merge-base', $._branch_name, optional(seq('into', $._branch_name))),
          seq(
            'diff',
            $._branch_name,
            optional(seq(choice('into', 'to'), $._branch_name)),
            optional(seq('after', 'page', field('page', $.string))),
            optional(seq('limit', field('limit', $.integer))),
          ),
          'list',
        ),
        optional(';'),
      ),

    // A side is a branch or a quoted commit id. `dry` and `as` are names here: the clause
    // words are keywords only where the clause may start.
    _branch_name: ($) => choice(alias($.identifier, $.branch_name), alias($.string, $.branch_name)),

    dry_run: (_) => seq('dry', 'run'),

    commit_statement: ($) =>
      seq('commit', choice('publish', 'drop'), field('commit', $.string), optional(';')),

    load_statement: ($) => seq($._load, optional(';')),

    // `load csv "<uri>" into node:Person mode overwrite [delimiter "|"] [header] [columns (a, b)]`.
    _load: ($) =>
      seq(
        'load',
        'csv',
        field('source', $.string),
        'into',
        field('target', $.load_target),
        repeat(choice($.load_mode, $.load_delimiter, 'header', $.load_columns)),
      ),

    load_target: ($) => seq(choice('node', 'edge'), ':', field('type', $.type_identifier)),

    load_mode: (_) => seq('mode', choice('overwrite', 'append', 'merge')),

    load_delimiter: ($) => seq('delimiter', $.string),

    load_columns: ($) => seq('columns', '(', commaSep1($.identifier), ')'),

    explain_statement: ($) =>
      seq('explain', choice($.query_declaration, alias($._load, $.load_statement))),

    // ---- declarations ----

    query_declaration: ($) =>
      seq(
        'query',
        field('name', $.identifier),
        field('parameters', $.parameters),
        repeat($.annotation),
        optional(field('result', choice($.as_commit, $.as_unattached_branch))),
        field('body', $.query_body),
      ),

    // The write's result is an unpublished commit.
    as_commit: (_) => seq('as', 'commit'),

    // The write's result is an unattached snapshot, a root or a child of the named one.
    as_unattached_branch: ($) =>
      seq('as', 'unattached', 'branch', optional(seq('on', field('base', choice($.string, $.variable))))),

    parameters: ($) => seq('(', commaSep($.parameter), ')'),

    parameter: ($) => seq(field('name', $.variable), ':', field('type', $.type)),

    type: ($) => seq(choice($.list_type, $.vector_type, $.primitive_type), optional('?')),

    list_type: ($) => seq('[', $.primitive_type, ']'),

    vector_type: ($) => seq('Vector', '(', $.integer, ')'),

    primitive_type: (_) =>
      choice('String', 'Blob', 'Bool', 'I32', 'I64', 'U32', 'U64', 'F32', 'F64', 'DateTime', 'Date'),

    annotation: ($) => seq(choice('@description', '@instruction'), '(', $.string, ')'),

    query_body: ($) =>
      seq(
        '{',
        choice(
          seq($.match_clause, $.return_clause, optional($.order_clause), optional($.limit_clause)),
          repeat1($._mutation),
        ),
        '}',
      ),

    // ---- reads ----

    match_clause: ($) => seq('match', $.block),

    block: ($) => seq('{', repeat1($._clause), '}'),

    _clause: ($) =>
      choice(
        $.negation,
        $.subquery_predicate,
        $.exists_clause,
        $.binding,
        $.traversal,
        $._expression,
      ),

    binding: ($) =>
      seq(
        field('variable', $.variable),
        ':',
        field('type', $.type_identifier),
        optional(seq('{', commaSep1($.property_match), optional(','), '}')),
      ),

    property_match: ($) => seq(field('property', $.identifier), ':', field('value', $._expression)),

    traversal: ($) =>
      prec.dynamic(
        PREC.traversal,
        seq(
          field('from', $.variable),
          optional($.edge_binding),
          field(
            'edge',
            choice($.edge_name, $.string, $.undirected_edge, $.edge_alternation, $.edge_wildcard),
          ),
          optional($.hop_bounds),
          field('to', $.variable),
        ),
      ),

    edge_binding: ($) => seq($.variable, ':'),

    edge_name: ($) => $.identifier,

    undirected_edge: ($) =>
      seq(
        '<',
        choice($.edge_wildcard, $._edge_alternatives, $.edge_name, $.string),
        '>',
      ),

    edge_alternation: ($) => seq('(', $._edge_alternatives, ')'),

    _edge_alternatives: ($) =>
      seq(choice($.edge_name, $.string), repeat1(seq('|', choice($.edge_name, $.string)))),

    edge_wildcard: (_) => '*',

    hop_bounds: ($) => seq('{', $.integer, ',', optional($.integer), '}'),

    negation: ($) => seq('not', $.block),

    exists_clause: ($) => seq('exists', $.block),

    subquery_predicate: ($) =>
      seq(
        field('aggregate', $.aggregate_name),
        optional(seq('(', $._expression, ')')),
        $.block,
        $.comparison_operator,
        $._expression,
      ),

    aggregate_name: (_) => choice('count', 'sum', 'avg', 'min', 'max'),

    return_clause: ($) => seq('return', optional('distinct'), '{', repeat1($.projection), '}'),

    projection: ($) =>
      seq($._expression, optional(seq('as', field('alias', $.identifier))), optional(',')),

    order_clause: ($) => seq('order', '{', commaSep1($.ordering), '}'),

    ordering: ($) => seq($._expression, optional(choice('asc', 'desc'))),

    limit_clause: ($) => seq('limit', $.integer),

    // ---- writes ----

    _mutation: ($) => choice($.insert_statement, $.update_statement, $.delete_statement),

    insert_statement: ($) =>
      seq('insert', field('type', $.type_identifier), '{', repeat1($.assignment), '}'),

    update_statement: ($) =>
      seq(
        'update',
        field('type', $.type_identifier),
        'set',
        '{',
        repeat1($.assignment),
        '}',
        'where',
        field('predicate', $._expression),
      ),

    delete_statement: ($) =>
      seq('delete', field('type', $.type_identifier), 'where', field('predicate', $._expression)),

    assignment: ($) =>
      seq(field('property', $.identifier), ':', field('value', $._expression), optional(',')),

    // ---- expressions ----

    _expression: ($) =>
      choice(
        $.binary_expression,
        $.not_expression,
        $.comparison,
        $.membership,
        $.null_test,
        $._operand,
      ),

    binary_expression: ($) =>
      choice(
        prec.left(PREC.or, seq($._expression, 'or', $._expression)),
        prec.left(PREC.and, seq($._expression, 'and', $._expression)),
      ),

    not_expression: ($) => prec(PREC.not, seq('not', $._expression)),

    comparison: ($) =>
      prec.left(
        PREC.compare,
        seq(
          $._expression,
          choice($.comparison_operator, 'starts_with', 'contains'),
          $._expression,
        ),
      ),

    comparison_operator: (_) => choice('>=', '<=', '!=', '>', '<', '='),

    membership: ($) => prec.left(PREC.compare, seq($._expression, 'in', $._expression)),

    null_test: ($) => prec.left(PREC.compare, seq($._expression, 'is', optional('not'), 'null')),

    _operand: ($) =>
      choice(
        $.parenthesized,
        $.call,
        $.property_access,
        $.meta_field,
        $.variable,
        $._literal,
        $.identifier,
      ),

    parenthesized: ($) => seq('(', $._expression, ')'),

    // `count(distinct $x)`: only an aggregate takes `distinct`, so another call's argument
    // may still be a property named `distinct`.
    call: ($) =>
      prec(
        PREC.call,
        choice(
          seq(field('function', $.identifier), '(', commaSep($._expression), ')'),
          seq(
            field('function', alias($.aggregate_name, $.identifier)),
            '(',
            optional('distinct'),
            commaSep($._expression),
            ')',
          ),
        ),
      ),

    property_access: ($) =>
      seq(field('object', $.variable), '.', field('property', choice($.meta_field, $.identifier))),

    // ---- terminals ----

    meta_field: (_) => token(seq('@', /[A-Za-z_][A-Za-z0-9_]*/)),

    variable: (_) => token(seq('$', /[A-Za-z_][A-Za-z0-9_]*/)),

    identifier: (_) => /[a-z_][A-Za-z0-9_]*/,

    // A leading `@` names an engine catalogue type (`@Operator`).
    type_identifier: (_) => token(seq(optional('@'), /[A-Z][A-Za-z0-9_]*/)),

    _literal: ($) => choice($.list, $.string, $.float, $.integer, $.boolean),

    // pest admits literals here, `date("…")` among them; any expression reads the same.
    list: ($) => seq('[', commaSep($._expression), ']'),

    string: (_) => token(seq('"', repeat(choice(/[^"\\]/, /\\./)), '"')),

    float: (_) => /\d+\.\d+/,

    integer: (_) => /\d+/,

    boolean: (_) => choice('true', 'false'),
  },
});

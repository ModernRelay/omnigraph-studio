; Theme names match `gq_language::TokenKind::syntax_name`, so the editor and the read-only views
; of a query colour the same words the same way.

(comment) @comment
(string) @string
(integer) @number
(float) @number
(boolean) @boolean

(variable) @variable
(type_identifier) @type
(primitive_type) @type
"Vector" @type

(annotation ["@description" "@instruction"] @attribute)

(meta_field) @property
(property_access property: (identifier) @property)
(property_match property: (identifier) @property)
(assignment property: (identifier) @property)

(query_declaration name: (identifier) @function)
(call function: (identifier) @function)
(aggregate_name) @function

(edge_name) @label

[
  "query" "match" "return" "order" "limit" "insert" "update" "delete" "set" "where"
  "not" "exists" "and" "or" "in" "is" "null" "as" "asc" "desc" "reset" "show" "all"
  "branch" "create" "from" "merge" "into" "list" "explain" "starts_with" "contains"
] @keyword

(comparison_operator) @operator
(edge_wildcard) @operator
(type "?" @operator)

(undirected_edge ["<" ">"] @punctuation.bracket)
["(" ")" "[" "]" "{" "}"] @punctuation.bracket
["," ":" "." ";" "|"] @punctuation.delimiter

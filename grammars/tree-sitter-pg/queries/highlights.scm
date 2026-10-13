; Theme names are the ones the GQ grammar uses, so a type, a property or an edge reads the same
; colour in a schema as in a query.

(comment) @comment
(string) @string
(enum_value) @string
(number) @number
(boolean) @boolean

(type_identifier) @type
(primitive_type) @type
"Vector" @type

(edge_declaration name: (type_identifier) @label)

(property name: (identifier) @property)
(keyword_argument name: (identifier) @property)
(meta_field) @property
(annotation name: (annotation_name) @attribute)

["interface" "node" "edge" "implements" "enum"] @keyword

["->" ".." "=" "?"] @operator

["(" ")" "[" "]" "{" "}"] @punctuation.bracket
["," ":"] @punctuation.delimiter

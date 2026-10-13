; Each declaration is an outline item, named by its type.
(interface_declaration
  "interface" @context
  name: (type_identifier) @name) @item

(node_declaration
  "node" @context
  name: (type_identifier) @name) @item

(edge_declaration
  "edge" @context
  name: (type_identifier) @name) @item

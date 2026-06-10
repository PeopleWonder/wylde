; Outline items for TSX — `treesitter.outline`.
;
; Identical patterns to TypeScript (TSX is TS + JSX; JSX adds expression
; nodes, not new definition kinds — a function component outlines as its
; function declaration).

(function_declaration name: (_) @name) @item
(generator_function_declaration name: (_) @name) @item
(class_declaration name: (_) @name) @item
(abstract_class_declaration name: (_) @name) @item
(interface_declaration name: (_) @name) @item
(enum_declaration name: (_) @name) @item
(type_alias_declaration name: (_) @name) @item
(method_definition name: (_) @name) @item
(method_signature name: (_) @name) @item
(abstract_method_signature name: (_) @name) @item

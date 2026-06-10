; Outline items for TypeScript — `treesitter.outline`.
;
; All depths (class methods + interface signatures included); outline.rs
; nests the flat captures by byte containment.

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

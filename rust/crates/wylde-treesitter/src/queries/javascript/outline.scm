; Outline items for JavaScript — `treesitter.outline`.
;
; All depths (class methods included); outline.rs nests by containment.

(function_declaration name: (_) @name) @item
(generator_function_declaration name: (_) @name) @item
(class_declaration name: (_) @name) @item
(method_definition name: (_) @name) @item

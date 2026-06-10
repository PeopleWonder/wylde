; Outline items for Python — `treesitter.outline`.
;
; Unlike chunks.scm these patterns are NOT anchored to the module root:
; the outline wants every definition at every depth (methods, inner defs),
; and outline.rs nests the flat captures into a tree by byte containment.
; A decorated def matches via its inner definition node, so the outline
; entry starts at the `def`/`class` line, not the decorator.

(function_definition name: (_) @name) @item
(class_definition name: (_) @name) @item

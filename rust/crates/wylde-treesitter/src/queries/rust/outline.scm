; Outline items for Rust — `treesitter.outline`.
;
; All depths (impl/trait methods included); outline.rs nests by containment.
; `impl` has two patterns (chunks.scm precedent): the typed one names the
; entry after the implemented type when it's a plain `type_identifier`;
; generic impls fall through to the bare pattern. Both capture the same
; node — outline.rs dedups by node id and keeps the captured name.

(function_item name: (_) @name) @item
(function_signature_item name: (_) @name) @item
(struct_item name: (_) @name) @item
(enum_item name: (_) @name) @item
(union_item name: (_) @name) @item
(trait_item name: (_) @name) @item
(mod_item name: (_) @name) @item
(macro_definition name: (_) @name) @item

(impl_item type: (type_identifier) @name) @item
(impl_item) @item

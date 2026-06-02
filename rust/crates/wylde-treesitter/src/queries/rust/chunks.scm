; Top-level chunk boundaries for Rust.
;
; Each @chunk is a node that becomes its own AST-aligned chunk; @symbol_name
; (when present) names it. Anchoring under `(source_file ...)` keeps matches to
; *crate-top-level* items — methods inside an `impl`/`trait` stay inside that
; item's chunk. Leftover top-level statements (`use`, `const`, `static`, attrs)
; are not matched here; chunk.rs groups them into "module" filler chunks.

(source_file
  (function_item
    name: (identifier) @symbol_name) @chunk)

(source_file
  (struct_item
    name: (type_identifier) @symbol_name) @chunk)

(source_file
  (enum_item
    name: (type_identifier) @symbol_name) @chunk)

(source_file
  (union_item
    name: (type_identifier) @symbol_name) @chunk)

(source_file
  (trait_item
    name: (type_identifier) @symbol_name) @chunk)

(source_file
  (mod_item
    name: (identifier) @symbol_name) @chunk)

(source_file
  (macro_definition
    name: (identifier) @symbol_name) @chunk)

; `impl Type { … }` / `impl Trait for Type { … }`. Two patterns: the typed one
; names the chunk after the implemented type when it's a plain `type_identifier`
; (generic impls fall through to the bare pattern). Both capture the same node;
; chunk.rs dedups by node id and keeps the name when one was captured.
(source_file
  (impl_item
    type: (type_identifier) @symbol_name) @chunk)

(source_file
  (impl_item) @chunk)

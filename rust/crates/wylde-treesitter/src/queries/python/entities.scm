; Structural-entity captures for Python — the `treesitter.extract_entities` verb.
;
; The query captures *candidate* nodes; entities.rs classifies them (a
; `function_definition` is a top-level function, a method, or a nested closure
; depending on its enclosing definition; a `call`'s caller is its nearest
; enclosing function). Keeping the query flat — one capture per kind, no
; structural anchoring — means the Rust side owns scope/ownership decisions in
; one place instead of spreading them across many query patterns.
;
; Captures:
;   @function — every `def` (top-level, method, or nested); classified in Rust.
;   @class    — every `class`; methods/bases extracted by walking its body.
;   @import   — `import x` / `import x.y as z`.
;   @import   — `from x import y` (same capture name; both yield import edges).
;   @call     — every call expression; callee + caller resolved in Rust.

(function_definition) @function
(class_definition) @class
(import_statement) @import
(import_from_statement) @import
(call) @call

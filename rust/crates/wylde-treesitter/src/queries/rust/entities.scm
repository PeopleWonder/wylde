; Structural-entity captures for Rust — the `treesitter.extract_entities` verb.
;
; Captures are flat (one per kind, no structural anchoring); entities.rs
; classifies each via RUST_SPEC (a `function_item` inside an `impl`/`trait` is a
; method, not a top-level function; a call's caller is its enclosing `fn`).
;
;   @function — every `fn`; top-level vs method decided in Rust.
;   @class    — type-like items + `impl` blocks. struct/enum/trait declare the
;               type; `impl` attaches methods (and, for `impl Trait for T`, the
;               implemented trait as an INHERITS base).
;   @import   — `use` declarations → IMPORTS edges (module path).
;   @call     — call expressions; callee = the trailing identifier.

(function_item) @function

(struct_item) @class
(enum_item) @class
(trait_item) @class
(impl_item) @class

(use_declaration) @import

(call_expression) @call

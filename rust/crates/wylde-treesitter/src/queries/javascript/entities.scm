; Structural-entity captures for JavaScript — `treesitter.extract_entities`.
;
; Captures are flat; entities.rs classifies each via JS_SPEC (a method lives in
; a class body, not the top-level functions list; a call's caller is its
; enclosing function/method).
;
;   @function — top-level / nested function declarations.
;   @class    — class declarations; methods + `extends` base read in Rust.
;   @import   — ES `import` statements → IMPORTS edges (the source module).
;   @call     — call expressions; callee = the trailing identifier.

(function_declaration) @function
(generator_function_declaration) @function

(class_declaration) @class

(import_statement) @import

(call_expression) @call

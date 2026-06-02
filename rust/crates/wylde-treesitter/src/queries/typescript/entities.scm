; Structural-entity captures for TypeScript — `treesitter.extract_entities`.
;
; Like JavaScript, plus abstract classes and interfaces as @class candidates
; (entities.rs reads their method signatures + `extends`/`implements` bases via
; TS_SPEC).
;
;   @function — function declarations.
;   @class    — class / abstract class / interface declarations.
;   @import   — ES `import` statements → IMPORTS edges.
;   @call     — call expressions; callee = the trailing identifier.

(function_declaration) @function
(generator_function_declaration) @function

(class_declaration) @class
(abstract_class_declaration) @class
(interface_declaration) @class

(import_statement) @import

(call_expression) @call

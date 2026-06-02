; Top-level chunk boundaries for JavaScript.
;
; Each @chunk is a node that becomes its own AST-aligned chunk; @symbol_name
; names it. Anchoring under `(program ...)` keeps matches to *module-top-level*
; declarations — methods inside a class stay in the class chunk. `export`ed
; declarations are wrapped in an `export_statement`, so they get their own
; patterns that reach through to the inner declaration's name. Leftover
; top-level statements (imports, `const`/`let` bindings) coalesce into "module"
; filler chunks in chunk.rs.

(program
  (function_declaration
    name: (identifier) @symbol_name) @chunk)

(program
  (generator_function_declaration
    name: (identifier) @symbol_name) @chunk)

(program
  (class_declaration
    name: (identifier) @symbol_name) @chunk)

; `export function f() {}` / `export class C {}` / `export default …`.
(program
  (export_statement
    declaration: (function_declaration
      name: (identifier) @symbol_name)) @chunk)

(program
  (export_statement
    declaration: (generator_function_declaration
      name: (identifier) @symbol_name)) @chunk)

(program
  (export_statement
    declaration: (class_declaration
      name: (identifier) @symbol_name)) @chunk)

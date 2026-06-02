; Top-level chunk boundaries for TSX (TypeScript + JSX).
;
; Identical to the TypeScript query — TSX is the same grammar plus JSX, and the
; top-level declaration shapes (functions, classes, abstract classes,
; interfaces, enums, type aliases) are unchanged. JSX lives *inside* function
; component / class bodies, so it never opens a top-level chunk boundary; the
; component is captured by its enclosing `function_declaration`/`class_declaration`
; like any other definition. Names follow the TS grammar: class/interface/type
; names are `type_identifier`, function/enum names are `identifier`. `export`ed
; forms reach through `export_statement`.

(program
  (function_declaration
    name: (identifier) @symbol_name) @chunk)

(program
  (generator_function_declaration
    name: (identifier) @symbol_name) @chunk)

(program
  (class_declaration
    name: (type_identifier) @symbol_name) @chunk)

(program
  (abstract_class_declaration
    name: (type_identifier) @symbol_name) @chunk)

(program
  (interface_declaration
    name: (type_identifier) @symbol_name) @chunk)

(program
  (enum_declaration
    name: (identifier) @symbol_name) @chunk)

(program
  (type_alias_declaration
    name: (type_identifier) @symbol_name) @chunk)

; `export`ed top-level declarations.
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
      name: (type_identifier) @symbol_name)) @chunk)

(program
  (export_statement
    declaration: (abstract_class_declaration
      name: (type_identifier) @symbol_name)) @chunk)

(program
  (export_statement
    declaration: (interface_declaration
      name: (type_identifier) @symbol_name)) @chunk)

(program
  (export_statement
    declaration: (enum_declaration
      name: (identifier) @symbol_name)) @chunk)

(program
  (export_statement
    declaration: (type_alias_declaration
      name: (type_identifier) @symbol_name)) @chunk)

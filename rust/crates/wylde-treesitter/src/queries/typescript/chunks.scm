; Top-level chunk boundaries for TypeScript.
;
; Like the JavaScript query, plus TS-only top-level shapes: abstract classes,
; interfaces, enums, and type aliases. Class/interface names are
; `type_identifier` in the TS grammar (vs `identifier` in JS); function and enum
; names stay `identifier`. `export`ed forms reach through `export_statement`.

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

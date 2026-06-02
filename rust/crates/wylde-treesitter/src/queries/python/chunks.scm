; Top-level chunk boundaries for Python.
;
; Each @chunk capture is a node that becomes its own AST-aligned chunk; the
; @symbol_name capture (when present) names it. Anchoring every pattern under
; `(module ...)` keeps the match to *top-level* definitions only — methods
; inside a class are NOT separately captured, so a class and its body stay one
; chunk. Statements between definitions (imports, module-level assignments) are
; not matched here; chunk.rs groups those leftover top-level statements into
; their own "module" filler chunks.

(module
  (function_definition
    name: (identifier) @symbol_name) @chunk)

(module
  (class_definition
    name: (identifier) @symbol_name) @chunk)

; A decorated def (`@deco\ndef f(): ...`) is one chunk spanning the decorators
; plus the def; reach through `definition:` to name the inner function/class.
(module
  (decorated_definition
    definition: [
      (function_definition name: (identifier) @symbol_name)
      (class_definition    name: (identifier) @symbol_name)
    ]) @chunk)

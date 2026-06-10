; Top-level chunk boundaries for Bash.
;
; Function definitions are the only named top-level structure; everything
; between them (setup statements, exports) coalesces into chunk.rs's
; "module" filler chunks.

(program
  (function_definition
    name: (word) @symbol_name) @chunk)

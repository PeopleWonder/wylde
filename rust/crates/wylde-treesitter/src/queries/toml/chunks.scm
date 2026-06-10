; Top-level chunk boundaries for TOML.
;
; Tables (`[server]`) and array-of-table elements (`[[bin]]`) are the
; natural chunk units — each keeps its key/value pairs together. Root-table
; pairs before the first header coalesce into chunk.rs's "module" filler.
; The header key (bare / dotted / quoted) names the chunk; tables carry no
; `name` field in this grammar, so the key is matched as a direct child.

(document
  (table
    [(bare_key) (dotted_key) (quoted_key)] @symbol_name) @chunk)

(document
  (table_array_element
    [(bare_key) (dotted_key) (quoted_key)] @symbol_name) @chunk)

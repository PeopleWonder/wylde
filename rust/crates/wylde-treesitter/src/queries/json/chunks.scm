; Top-level chunk boundaries for JSON.
;
; A JSON file is typically ONE top-level value (object/array), so each
; document child becomes a chunk and chunk.rs's oversized-windowing splits
; a giant object into line-aligned shards. No symbol names — JSON values
; are anonymous at the top level.

(document (_) @chunk)

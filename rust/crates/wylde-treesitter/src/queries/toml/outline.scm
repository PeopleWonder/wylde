; Outline items for TOML — tables and array-of-table elements at every
; depth (nesting by containment is flat here — TOML tables don't nest
; syntactically — but dotted headers read naturally in a flat list).

(table
  [(bare_key) (dotted_key) (quoted_key)] @name) @item

(table_array_element
  [(bare_key) (dotted_key) (quoted_key)] @name) @item

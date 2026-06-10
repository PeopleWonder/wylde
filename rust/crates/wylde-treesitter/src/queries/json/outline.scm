; Outline items for JSON — every object key at every depth; outline.rs
; nests the flat captures by byte containment, reproducing the object
; structure. @name is the key's inner text (no quotes).

(pair
  key: (string (string_content) @name)) @item

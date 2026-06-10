; Outline items for Markdown (the tree-sitter-md *block* grammar) —
; `treesitter.outline`.
;
; Sections at EVERY depth (unlike chunks.scm, which only takes the
; document's direct children): the block grammar nests subsections inside
; their parent `section`, so byte-containment nesting in outline.rs
; reproduces the heading hierarchy. @name is the heading text — ATX
; (`# Title`) via the `heading_content` inline node, setext
; (`Title\n=====`) via the leading paragraph. A heading-less preamble
; section matches the bare pattern with no name.

(section
  (atx_heading
    heading_content: (inline) @name)) @item

(section
  (setext_heading
    heading_content: (paragraph) @name)) @item

(section) @item

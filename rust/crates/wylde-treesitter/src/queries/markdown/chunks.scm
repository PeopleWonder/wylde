; Top-level chunk boundaries for Markdown (the tree-sitter-md *block* grammar).
;
; The block grammar nests content under `section` nodes by heading level: a
; document's direct children are its highest-level sections, each holding a
; heading plus its body (and any nested subsections). Chunking on the
; document's top-level sections therefore keeps a heading together with the
; prose/code beneath it — the natural RAG unit for docs. (chunk.rs only treats
; *direct* children of the root as boundaries, so subsections ride inside their
; parent section's chunk; oversized sections still get windowed.)
;
; @symbol_name is the heading text: ATX headings (`# Title`) expose it as the
; `heading_content` inline node; setext headings (`Title\n=====`) carry it in
; the leading paragraph. A leading section with no heading (preamble before the
; first `#`) matches the bare pattern with no name.

(document
  (section
    (atx_heading
      heading_content: (inline) @symbol_name)) @chunk)

(document
  (section
    (setext_heading
      heading_content: (paragraph) @symbol_name)) @chunk)

(document
  (section) @chunk)

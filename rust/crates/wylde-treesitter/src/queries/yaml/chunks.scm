; Top-level chunk boundaries for YAML (the `stream` grammar root).
;
; A YAML file is a stream of documents (`---` separated); each document is
; the natural chunk — multi-doc manifests (k8s style) split per resource,
; single-doc files become one chunk that chunk.rs windows when oversized.

(stream (document) @chunk)

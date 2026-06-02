; Structural-entity captures for TSX (TypeScript + JSX) —
; `treesitter.extract_entities`.
;
; Identical to the TypeScript captures (entities.rs reads them via TS_SPEC —
; node kinds/fields are the same grammar), plus a JSX-only `@jsx_call` so a
; component reference in markup becomes a CALLS edge. A `<Component/>` /
; `<Component>…</Component>` tag is treated as a call to `Component` (caller =
; the enclosing function/method, i.e. the component that renders it). Host tags
; (`<div>`, `<span>` — lowercase) are filtered out in entities.rs so only React
; components land in `calls`; member tags (`<Foo.Bar/>`) are left for a later
; pass.
;
;   @function — function declarations.
;   @class    — class / abstract class / interface declarations.
;   @import   — ES `import` statements → IMPORTS edges.
;   @call     — call expressions; callee = the trailing identifier.
;   @jsx_call — JSX element tag name → a CALLS edge to the component.

(function_declaration) @function
(generator_function_declaration) @function

(class_declaration) @class
(abstract_class_declaration) @class
(interface_declaration) @class

(import_statement) @import

(call_expression) @call

(jsx_opening_element
  name: (identifier) @jsx_call)
(jsx_self_closing_element
  name: (identifier) @jsx_call)

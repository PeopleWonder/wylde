# Bundled-asset attribution — Wylde GUI

This directory carries the licence / NOTICE text for every asset Wylde ships
as a default. **Standing rule (Aaron): anything shipped as a default must be
free for commercial use.** Non-commercial or ShareAlike-encumbered assets are
rejected (e.g. vscode-icons is CC BY-SA — *not* used).

## File-tree icons (`assets/icons/file-tree/*.svg`)

**Currently shipped (audit D1, 2026-06-19):** the genuine upstream sets, both
permissive and commercial-use-OK:

- **Lucide (ISC)** — the category / UI / folder / generic glyphs (`file`,
  `folder`, `folder-open`, `code`, `doc`, `config`, `data`, `image`, `lock`,
  `git`, `package`, `book`). Sourced from
  <https://github.com/lucide-icons/lucide> (`icons/*.svg`, e.g.
  `file-code`→`code`, `file-text`→`doc`, `file-cog`→`config`,
  `database`→`data`, `git-branch`→`git`, `book-open`→`book`). Licence:
  **ISC** — see `LICENSE-Lucide-ISC.txt`.
- **Seti-UI (MIT)** — the per-language file-type glyphs (`rust`, `python`,
  `typescript`, `react`, `javascript`, `go`, `c`, `cpp`, `c-sharp`, `java`,
  `kotlin`, `swift`, `ruby`, `php`, `scala`, `clojure`, `elixir`, `haskell`,
  `lua`, `dart`, `r`, `vue`, `svelte`, `html`, `css`, `sass`, `shell`,
  `powershell`, `json`, `yaml`, `markdown`, `xml`, `zig`, `perl`, `ocaml`,
  `julia`, `nim`, `elm`, `graphql`, `terraform`, `docker`, `makefile`).
  Sourced from <https://github.com/jesseweed/seti-ui> (`icons/*.svg`).
  Licence: **MIT**, © 2014 Jesse Weed — see `LICENSE-Seti-MIT.txt`.

gpui's `svg()` paints every glyph as a single-colour **mask tinted by the
element's `text_color`**, so a Seti icon's source `fill="#…"` is ignored and
the per-file-type colour comes from the theme config (`file_tree_icons`
block / the row's white-font colour), not the SVG. Lucide's `currentColor`
strokes mask the same way. Mixed view-boxes (Lucide 24×24, Seti 32×32) scale
to the render box independently, so the two sets coexist cleanly.

The extension/filename → icon mapping lives in
`Core/GUI/Frontend/Panels/Workspaces/src/files/icon_map.rs`; the embed list
that makes each glyph available to `svg().path(...)` is `FILE_TREE_ICONS` in
`Core/GUI/Shell/src/assets.rs`. Recognised languages resolve to their Seti
glyph; anything without one falls through to a Lucide category, with `code`
(Lucide `file-code`) as the generic source-file fallback and `file` as the
final default.

**Superseded:** the original in-house CC0 placeholder set (12 category
glyphs) that shipped while the upstream sets were owed. Its dedication is
retained in `CC0-original-assets.txt` for provenance; those glyphs are no
longer on disk.

## Brand / tray icon (`assets/icons/icon.{png,ico,icns}`, `*.png`)

Pre-existing Wylde brand mark (relocated from the deleted `src-tauri/icons/`
at the gpui cutover). Wylde-owned.

## Fonts

No font is bundled today (the white-font pass is a colour change only). If a
face is later bundled for cross-OS consistency, **Inter (SIL OFL 1.1)** is the
drop-in (it's already the UI face); bundle its `OFL.txt` here at that time.

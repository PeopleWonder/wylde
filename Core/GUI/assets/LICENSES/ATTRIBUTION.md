# Bundled-asset attribution — Wylde GUI

This directory carries the licence / NOTICE text for every asset Wylde ships
as a default. **Standing rule (Aaron): anything shipped as a default must be
free for commercial use.** Non-commercial or ShareAlike-encumbered assets are
rejected (e.g. vscode-icons is CC BY-SA — *not* used).

## File-tree icons (`assets/icons/file-tree/*.svg`)

**Currently shipped:** an **original** Wylde set of monochrome, stroke-based
line glyphs (24×24, `stroke="currentColor"` idiom). They are authored
in-house and dedicated to the public domain (**CC0-1.0** — see
`CC0-original-assets.txt`), so they carry **no attribution burden** and are
unambiguously commercial-safe. gpui's `svg()` paints them as a single-colour
**mask tinted by the element's `text_color`**, so the per-file-type colour
comes from the theme config (`file_tree_icons` block), not the SVG.

The set is intentionally small and **category-based** (file, folder,
folder-open, code, doc, config, data, image, lock, git, package, book) rather
than per-language brand marks. The icon-config resolution maps file
extensions onto these categories (e.g. `rs`/`py`/`ts` → `code`, `toml`/`yaml`
→ `config`).

**Drop-in path for the polished upstream sets (owed / optional).** The plan's
recommended sets are **Lucide (ISC)** for UI/folder/git glyphs and **Seti
(MIT)** for per-file-type icons. Both are permissive, monochrome, and tint
cleanly through `svg()`. To adopt them:

1. Drop their `.svg` files into `assets/icons/file-tree/` (overwriting /
   adding names), keeping the `stroke`/`fill` monochrome convention.
2. Extend the `Assets` embed list in `Core/GUI/Shell/src/assets.rs` and the
   `file_tree_icons` config with any new names.
3. The licence texts are **already staged here** — `LICENSE-Lucide-ISC.txt`
   and `LICENSE-Seti-MIT.txt` — so adopting them is licence-complete the
   moment the files land.

Couldn't be fetched in the offline build session that created this slice;
the originals ship in the meantime so the file tree has icons today.

## Brand / tray icon (`assets/icons/icon.{png,ico,icns}`, `*.png`)

Pre-existing Wylde brand mark (relocated from the deleted `src-tauri/icons/`
at the gpui cutover). Wylde-owned.

## Fonts

No font is bundled today (the white-font pass is a colour change only). If a
face is later bundled for cross-OS consistency, **Inter (SIL OFL 1.1)** is the
drop-in (it's already the UI face); bundle its `OFL.txt` here at that time.

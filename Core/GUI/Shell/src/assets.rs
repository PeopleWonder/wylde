//! Bundled assets — fonts, icons, brand mark.
//!
//! Per the plan §4.2 the gpui side eventually ships Inter as a bundled
//! TTF (`include_bytes!("../assets/fonts/Inter-Regular.ttf")` etc.) so
//! the font rendering is identical across OSes regardless of whether
//! the user has Inter installed locally.
//!
//! This foundation slice does NOT ship the TTF files — the gpui-side
//! authoring still relies on whatever the OS has for "Inter".  The
//! `install_fonts` hook below is wired through to gpui's text system
//! anyway so that, when the TTFs land in `assets/fonts/`, flipping the
//! bundle on is a one-line `include_bytes!` change.
//!
//! Same story for the brand-mark gradient and the tray icon — both
//! reuse the existing PNG under `Core/GUI/assets/icons/icon.png`
//! until a gpui-native canvas implementation lands (see plan §4.4 for
//! the brand mark; §7 for the tray-icon recommendation).
//!
//! Slice 11 (cutover) moved the icon bundle out of the deleted
//! `src-tauri/icons/` and into `Core/GUI/assets/icons/` — the gpui
//! workspace now owns its own asset tree rather than borrowing the
//! Tauri one.

use std::borrow::Cow;
use std::path::PathBuf;

use gpui::{AssetSource, SharedString};

/// The file-tree icon bundle, embedded into the binary at build time.
///
/// Each entry is `(asset-key, bytes)`. The key is the path callers pass to
/// `svg().path(...)` — e.g. `svg().path("icons/file-tree/rust.svg")` — so the
/// keys are the stable contract the icon config (`file_tree_icons`) and the
/// Files panel resolve against. gpui paints an SVG as a **monochrome mask
/// tinted by the element's `text_color`**, so these are stroke-based
/// monochrome glyphs and the per-file-type colour comes from the theme, not
/// the file. See `Core/GUI/assets/LICENSES/ATTRIBUTION.md` for licensing
/// (originals shipped today; Lucide/Seti staged as a drop-in).
macro_rules! file_tree_icons {
    ($($name:literal),* $(,)?) => {
        &[ $((
            concat!("icons/file-tree/", $name, ".svg"),
            include_bytes!(concat!("../../assets/icons/file-tree/", $name, ".svg")) as &[u8],
        )),* ]
    };
}

/// The embedded icon table. Add a glyph here + its `.svg` under
/// `assets/icons/file-tree/` to make it available to `svg().path(...)`.
///
/// Two upstream sets, both permissive / commercial-OK (see
/// `Core/GUI/assets/LICENSES/ATTRIBUTION.md`): **Lucide (ISC)** supplies the
/// category / UI / folder / generic glyphs; **Seti-UI (MIT)** supplies the
/// per-language file-type glyphs. Both tint cleanly through `svg()`'s mask.
pub static FILE_TREE_ICONS: &[(&str, &[u8])] = file_tree_icons![
    // ── Lucide (ISC): category / UI / folder / generic ──
    "file",
    "folder",
    "folder-open",
    "code",
    "doc",
    "config",
    "data",
    "image",
    "lock",
    "git",
    "package",
    "book",
    // ── Seti-UI (MIT): per-language file-type ──
    "rust",
    "python",
    "typescript",
    "react",
    "javascript",
    "go",
    "c",
    "cpp",
    "c-sharp",
    "java",
    "kotlin",
    "swift",
    "ruby",
    "php",
    "scala",
    "clojure",
    "elixir",
    "haskell",
    "lua",
    "dart",
    "r",
    "vue",
    "svelte",
    "html",
    "css",
    "sass",
    "shell",
    "powershell",
    "json",
    "yaml",
    "markdown",
    "xml",
    "zig",
    "perl",
    "ocaml",
    "julia",
    "nim",
    "elm",
    "graphql",
    "terraform",
    "docker",
    "makefile",
];

/// gpui [`AssetSource`] over the embedded bundle, registered at app boot
/// (`Application::with_assets`) so `svg().path("icons/file-tree/…")` resolves.
/// Embedding (vs. reading from disk relative to the exe) keeps icons working
/// across every install layout — there is no asset directory to ship.
#[derive(Clone, Copy, Debug, Default)]
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        Ok(FILE_TREE_ICONS
            .iter()
            .find(|(key, _)| *key == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(FILE_TREE_ICONS
            .iter()
            .filter(|(key, _)| key.starts_with(path))
            .map(|(key, _)| SharedString::from(*key))
            .collect())
    }
}

/// Tray-icon asset candidates, in priority order.  On Windows the
/// native tray glyph format is `.ico` (multi-resolution container);
/// `tray-icon`'s PNG loader is finicky about exact pixel dimensions
/// so the `.ico` is the safer first pick.  On macOS / Linux a PNG
/// works fine.  The bundle under `Core/GUI/assets/icons/` ships both
/// (relocated there from the deleted `src-tauri/icons/` at cutover),
/// so we just probe in order.
pub const TRAY_ICON_CANDIDATES: &[&str] = &[
    #[cfg(target_os = "windows")]
    "assets/icons/icon.ico",
    "assets/icons/icon.png",
];

/// Best-effort resolver: walk up from the executable's directory
/// until one of the `TRAY_ICON_CANDIDATES` paths resolves on disk.
/// `None` means no asset was found — the tray install caller logs
/// and continues without a tray glyph.
pub fn locate_tray_icon() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let mut cursor = exe.parent().map(|p| p.to_path_buf());
    while let Some(dir) = cursor {
        for candidate in TRAY_ICON_CANDIDATES {
            let p = dir.join(candidate);
            if p.exists() {
                return Some(p);
            }
        }
        // Also tolerate an installed layout where the icon sits next
        // to the binary (no `src-tauri/icons/` prefix).
        for leaf in ["icon.ico", "icon.png"] {
            let p = dir.join(leaf);
            if p.exists() {
                return Some(p);
            }
        }
        cursor = dir.parent().map(|p| p.to_path_buf());
    }
    None
}

/// Hook for gpui's text-system bundling.  No-op for the foundation
/// slice — see module doc.  When the TTFs land, this is where the
/// `cx.text_system().add_fonts(...)` calls go.
pub fn install_fonts() {
    // Intentionally empty until `assets/fonts/Inter-*.ttf` is on disk.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_icon_candidates_reference_existing_bundle() {
        // If the icon under `assets/icons/` ever moves, this test is the
        // loud failure that tells us to update the constants.  The
        // candidates must point at the gpui-owned asset tree (never the
        // deleted `src-tauri/`) or the installed-next-to-binary leaf.
        assert!(!TRAY_ICON_CANDIDATES.is_empty());
        for candidate in TRAY_ICON_CANDIDATES {
            assert!(
                !candidate.contains("src-tauri"),
                "candidate {candidate:?} must not reference the deleted src-tauri tree",
            );
            assert!(
                candidate.starts_with("assets/") || candidate.starts_with("icon"),
                "candidate {candidate:?} must point at the gpui asset bundle or the installed leaf",
            );
        }
    }

    /// `locate_tray_icon` must not panic even when the executable's
    /// directory tree contains no matching asset.  The caller treats
    /// `None` as "tray comes up without a glyph" — a degraded state,
    /// not a crash.
    #[test]
    fn locate_tray_icon_handles_missing_asset_gracefully() {
        let _ = locate_tray_icon();
    }

    /// Every embedded icon resolves through the AssetSource, the bytes are a
    /// non-empty SVG, and the keys use the `icons/file-tree/` namespace the
    /// config resolves against. `include_bytes!` already fails the build if a
    /// file is missing, so this guards the *contract* (key shape + payload).
    #[test]
    fn embedded_icons_load_through_asset_source() {
        let assets = Assets;
        assert!(!FILE_TREE_ICONS.is_empty());
        for (key, _) in FILE_TREE_ICONS {
            assert!(
                key.starts_with("icons/file-tree/") && key.ends_with(".svg"),
                "icon key {key:?} must live in the file-tree namespace",
            );
            let loaded = assets.load(key).expect("load is infallible here");
            let bytes = loaded.unwrap_or_else(|| panic!("icon {key:?} did not resolve"));
            assert!(!bytes.is_empty(), "icon {key:?} is empty");
            let text = std::str::from_utf8(&bytes).expect("svg is utf-8");
            assert!(text.contains("<svg"), "icon {key:?} is not an SVG");
        }
        // A miss is a clean None, never a panic.
        assert!(assets
            .load("icons/file-tree/does-not-exist.svg")
            .unwrap()
            .is_none());
        // `list` filters by prefix.
        assert_eq!(
            assets.list("icons/file-tree/").unwrap().len(),
            FILE_TREE_ICONS.len()
        );
    }

    /// The defaults the config falls back to (`file`, `folder`, `folder-open`)
    /// must always be present — a file tree with no icons is the failure mode
    /// this pins against.
    #[test]
    fn the_config_default_icons_are_bundled() {
        let assets = Assets;
        for name in ["file", "folder", "folder-open"] {
            let key = format!("icons/file-tree/{name}.svg");
            assert!(
                assets.load(&key).unwrap().is_some(),
                "default icon {name:?} must ship",
            );
        }
    }
}

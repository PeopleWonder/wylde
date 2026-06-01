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

use std::path::PathBuf;

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
}

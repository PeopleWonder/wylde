//! ONNX Runtime / OpenVINO DLL bundle discovery (Slice 5 —
//! `docs/plans/voice-rust-port.md`, "DLL bundling decision").
//!
//! `ort` is built with `load-dynamic` (see `Cargo.toml`), so at runtime it
//! `dlopen`s `onnxruntime.dll` rather than linking it statically. The spike
//! (`docs/wylde-voice-npu-spike-findings.md` §"Build configuration that
//! works") established two packaging facts:
//!
//!   * `ORT_DYLIB_PATH=<binary_dir>/onnxruntime.dll` tells `ort-sys` which
//!     DLL to load. Without it the load fails on a vanilla machine.
//!   * The OpenVINO EP needs ~20 more DLLs (openvino 2025.4.x) **co-located
//!     so the Windows loader resolves them** — the spike worked precisely
//!     because the binary dir held every required DLL.
//!
//! Rather than push this into the (still-Python) lifecycle launcher, the
//! resolution happens here, inside the Rust binary, before any `ort`
//! session is built. That keeps packaging in the Rust ring (everything-Rust
//! rule) and makes a bare `wylde-voice.exe + DLLs in the same folder`
//! deployment self-configuring. An operator `ORT_DYLIB_PATH` always wins.

use std::path::{Path, PathBuf};

/// The DLL `ort-sys` dlopens. Windows-only filename; on other platforms
/// the soname differs but Wylde voice only ships on Windows, so we keep
/// the search Windows-shaped and no-op elsewhere.
const ORT_DLL: &str = "onnxruntime.dll";

/// Candidate directories (relative to the executable) that a bundled
/// `onnxruntime.dll` + OpenVINO DLLs may live in, most-specific first.
/// Pure so the search order is unit-testable without a real filesystem.
pub fn candidate_dirs(exe_dir: &Path) -> Vec<PathBuf> {
    vec![
        // 1. Beside the exe — the spike's layout (`target/release/*.dll`).
        exe_dir.to_path_buf(),
        // 2. A dedicated runtime subfolder — the "single resource bundle"
        //    packaging the spike recommended (#3) to avoid littering the
        //    bin dir with 23 DLLs.
        exe_dir.join("voice-runtime"),
        exe_dir.join("onnxruntime"),
    ]
}

/// First candidate dir that actually contains `onnxruntime.dll`, or `None`.
fn find_ort_dir(exe_dir: &Path) -> Option<PathBuf> {
    candidate_dirs(exe_dir)
        .into_iter()
        .find(|d| d.join(ORT_DLL).is_file())
}

/// Ensure `ORT_DYLIB_PATH` points at a real `onnxruntime.dll` before the
/// first `ort` session is built.
///
/// Returns:
///   * `Ok(Some(path))` — the var was set (or already pointed at a real
///     file) and resolves to an existing DLL.
///   * `Ok(None)`       — no bundled DLL found near the exe; left unset so
///     `ort` falls back to its own resolution and the action layer can
///     surface a clean `model_not_loaded`-shaped error. Non-fatal: the
///     pipe service still boots (matches the `Cargo.toml` default-build
///     contract).
///
/// Never overwrites an operator-provided `ORT_DYLIB_PATH`.
pub fn ensure_ort_dylib_path() -> std::io::Result<Option<PathBuf>> {
    if !cfg!(windows) {
        return Ok(None);
    }

    // Respect an explicit operator override — but report whether it's real.
    if let Some(existing) = std::env::var_os("ORT_DYLIB_PATH") {
        let p = PathBuf::from(existing);
        if p.is_file() {
            return Ok(Some(p));
        }
        tracing::warn!(
            "wylde-voice: ORT_DYLIB_PATH set to {} but no file there — \
             leaving it as-is (operator override)",
            p.display()
        );
        return Ok(None);
    }

    let exe = std::env::current_exe()?;
    let exe_dir = exe.parent().unwrap_or_else(|| Path::new("."));

    match find_ort_dir(exe_dir) {
        Some(dir) => {
            let dll = dir.join(ORT_DLL);
            // Set during single-threaded startup, before any ort session
            // (and thus any dlopen) runs. Same lifecycle point the spike
            // set it from its launcher env.
            std::env::set_var("ORT_DYLIB_PATH", &dll);
            prepend_to_path(&dir);
            Ok(Some(dll))
        }
        None => Ok(None),
    }
}

/// Prepend `dir` to the process `PATH` so the Windows loader resolves the
/// OpenVINO DLLs that `onnxruntime.dll` depends on from the same bundle.
/// No-op if `dir` is already first on `PATH`.
fn prepend_to_path(dir: &Path) {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let already_first = std::env::split_paths(&current)
        .next()
        .map(|first| first == dir)
        .unwrap_or(false);
    if already_first {
        return;
    }
    let mut entries = vec![dir.to_path_buf()];
    entries.extend(std::env::split_paths(&current));
    if let Ok(joined) = std::env::join_paths(entries) {
        // Startup, single-threaded, before any DLL load.
        std::env::set_var("PATH", joined);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_order_is_most_specific_dir_first() {
        let exe_dir = Path::new("C:\\opt\\wylde\\bin");
        let dirs = candidate_dirs(exe_dir);
        assert_eq!(dirs[0], exe_dir);
        assert_eq!(dirs[1], exe_dir.join("voice-runtime"));
        assert!(dirs.iter().all(|d| d.starts_with(exe_dir)));
    }

    #[test]
    fn find_ort_dir_locates_bundled_dll() {
        let tmp = tempfile::tempdir().unwrap();
        let exe_dir = tmp.path();
        // No DLL yet → no hit.
        assert!(find_ort_dir(exe_dir).is_none());
        // Drop the DLL into the dedicated runtime subfolder.
        let runtime = exe_dir.join("voice-runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::write(runtime.join(ORT_DLL), b"stub").unwrap();
        assert_eq!(find_ort_dir(exe_dir), Some(runtime));
    }

    #[test]
    fn find_ort_dir_prefers_exe_dir_over_subfolder() {
        let tmp = tempfile::tempdir().unwrap();
        let exe_dir = tmp.path();
        std::fs::create_dir_all(exe_dir.join("voice-runtime")).unwrap();
        std::fs::write(exe_dir.join("voice-runtime").join(ORT_DLL), b"stub").unwrap();
        std::fs::write(exe_dir.join(ORT_DLL), b"stub").unwrap();
        // Exe dir comes first in candidate order, so it wins.
        assert_eq!(find_ort_dir(exe_dir), Some(exe_dir.to_path_buf()));
    }
}

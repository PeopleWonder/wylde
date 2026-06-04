//! Embed an `asInvoker` application manifest on Windows.
//!
//! Windows' "installer detection" heuristic auto-elevates (UAC) any
//! executable whose filename contains words like `update`, `setup`,
//! `install`, or `patch` *unless* the binary carries a manifest that
//! explicitly declares a `requestedExecutionLevel`. Our crate is named
//! `wylde-updater`, so the produced binaries — including the `cargo test`
//! harness `wylde_updater-<hash>.exe` — match the heuristic and fail to
//! launch with `ERROR_ELEVATION_REQUIRED` (os error 740).
//!
//! Emitting an `asInvoker` manifest makes the heuristic stand down. This
//! crate is a library (no bin target), so the link args are scoped to its
//! own `cargo test` harness; they do not propagate to downstream crates
//! that merely depend on the library.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(windows)]
    {
        const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
</assembly>
"#;
        let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR set by cargo");
        let manifest_path = std::path::Path::new(&out_dir).join("wylde-updater.manifest");
        std::fs::write(&manifest_path, MANIFEST).expect("write manifest");

        // Only the MSVC linker understands /MANIFEST; the GNU toolchain
        // embeds via a .rc resource instead. We target MSVC (the shipped
        // toolchain), so guard on it and no-op elsewhere.
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            let input = manifest_path.display();
            // The generic `rustc-link-arg` covers every linked target of
            // *this* package — including the lib's unit-test harness, which
            // is the binary that trips the heuristic. (`-tests` only covers
            // `tests/` integration targets, of which this crate has none.)
            // It's inert downstream: dependents only link wylde-updater as
            // an rlib, which has no link step. Passed as single argv
            // tokens, so the space in the worktree path stays intact.
            println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
            println!("cargo:rustc-link-arg=/MANIFESTINPUT:{input}");
        }
    }
}

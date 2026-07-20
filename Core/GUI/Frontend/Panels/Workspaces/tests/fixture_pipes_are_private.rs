//! Guard: no GUI test may bind a **production** pipe name.
//!
//! ## Why this exists (#75, and #29 before it; the class is tracked on #83)
//!
//! A test that stands up a fixture server on `\\.\pipe\wylde-<service>` claims
//! the endpoint the *live* service owns. On a developer's machine running Wylde
//! the bind is refused (`ERROR_ACCESS_DENIED` / `ERROR_PIPE_BUSY`) and the test
//! fails deterministically; two such tests in one run also cross-talk (#29).
//!
//! The reason this class survives normal review is that **CI cannot observe
//! it**. CI never runs the Wylde stack, so the production name is always free
//! there and the test is green on every PR — red only on the machines actually
//! running the product. That is the inverse of a flake: the dynamic gate is
//! structurally blind, so a *static* check is the only enforcement available.
//! Hence this test rather than a runtime assertion.
//!
//! ## The rule
//!
//! A fixture server owns a *fixture* pipe. Use
//! `wylde_gui_pipe::test_backend::unique_pipe_name` + `PipeNameOverride`
//! (Core/GUI/Frontend/Pipe/src/test_backend.rs), which mints a per-process name
//! and re-points `wylde_gui_pipe::call` at it for the life of the guard. The
//! `rust/` workspace has followed the equivalent convention since #29
//! (`unique_service_name()` + the `WYLDE_*_PIPE_NAME` service overrides).
//!
//! ## Scope — and the half this guard cannot cover
//!
//! Scope is the `Core/GUI` tree; `rust/` is a separate cargo workspace with its
//! own gates and complies with the *pipe-name* convention (#29).
//!
//! **That is narrower than it sounds, and #80 proved it.** This guard is a scan
//! of source text, so it can only catch a production resource that appears *as a
//! literal in the test*. A pipe bind does. A resource resolved **inside
//! production code** does not:
//! `wylde-lifecycle`'s `shutdown_all_returns_structured_summary` asserted
//! `count == 0` and got `11` on any configured machine, because the root was
//! read from the ambient `WYLDE_ROOT` three layers down, in a process-global
//! `OnceLock`. Its test source contained no marker at all — the only
//! `WYLDE_ROOT` text was a comment, which `strip_line_comments` strips. A
//! textual gate for it would be permanently green: a required check that cannot
//! fail.
//!
//! So the class has two halves, enforced differently:
//!
//! | half | tell | enforcement | sightings |
//! |---|---|---|---|
//! | literal in the test | `\\.\pipe\wylde-x` | **this scan** | #47, #75 |
//! | resolved in production code | *(none — invisible)* | **hermetic `cfg(test)`** + a gate pinning it (`rust/crates/wylde-lifecycle/src/state/mod.rs`, `resolve_root_is_hermetic_under_cfg_test`) | #80 |
//!
//! Adding a resource? Ask which half it is before reaching for this file. If the
//! test never names it, extending this scan will buy a green check and no
//! safety — make the resolution hermetic under `cfg(test)` instead.

use std::fs;
use std::path::{Path, PathBuf};

/// The `Core/GUI` root, derived from this crate's manifest dir
/// (`Core/GUI/Frontend/Panels/Workspaces`) so it survives a checkout anywhere.
fn gui_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("manifest dir should be Core/GUI/Frontend/Panels/Workspaces")
        .to_path_buf()
}

/// Every `.rs` file under a `tests/` directory in the GUI tree.
fn gui_test_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            // Build output is not source; skip the expensive walk.
            if name == "target" || name == "target-dev" || name.starts_with('.') {
                continue;
            }
            gui_test_sources(&path, out);
        } else if name.ends_with(".rs")
            && path
                .components()
                .any(|c| c.as_os_str().to_string_lossy() == "tests")
        {
            out.push(path);
        }
    }
}

/// Strip `//` line comments so prose *describing* a production pipe (including
/// this file's own module docs, and the explanatory comment in
/// `integration_graph_ipc.rs`) is never mistaken for a bind.
fn strip_line_comments(src: &str) -> String {
    src.lines()
        .map(|line| match line.find("//") {
            Some(i) => &line[..i],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A production pipe literal is `\\.\pipe\wylde-<service>` where `<service>`
/// is a fixed name. Two forms are legitimate and must NOT trip the guard:
///   * an interpolated name (`{service_name}`) — the `rust/` convention;
///   * a minted fixture name, which always carries the `-test-` infix.
fn offending_literals(code: &str) -> Vec<String> {
    const MARKER: &str = r"\\.\pipe\wylde-";
    let mut found = Vec::new();
    for (idx, _) in code.match_indices(MARKER) {
        // Take the rest of the literal up to the closing quote.
        let tail = &code[idx + MARKER.len()..];
        let end = tail.find('"').unwrap_or(tail.len());
        let service = &tail[..end];
        if service.contains('{') || service.contains("-test-") {
            continue;
        }
        // A bare `wylde-` prefix with nothing after it is a format template.
        if service.is_empty() {
            continue;
        }
        found.push(format!("{MARKER}{service}"));
    }
    found
}

#[test]
fn no_gui_test_binds_a_production_pipe_name() {
    let mut sources = Vec::new();
    gui_test_sources(&gui_root(), &mut sources);

    assert!(
        !sources.is_empty(),
        "found no GUI test sources under {} — the walk is broken, not the tree clean",
        gui_root().display()
    );

    let mut offenders = Vec::new();
    for path in &sources {
        let Ok(src) = fs::read_to_string(path) else {
            continue;
        };
        for literal in offending_literals(&strip_line_comments(&src)) {
            offenders.push(format!("  {} binds {}", path.display(), literal));
        }
    }

    assert!(
        offenders.is_empty(),
        "test(s) bind a PRODUCTION pipe name — they will fail on any machine \
         running Wylde, and CI cannot catch that because CI never runs the \
         stack (#75).\n{}\n\nUse wylde_gui_pipe::test_backend::unique_pipe_name \
         + PipeNameOverride to own a private fixture pipe instead.",
        offenders.join("\n")
    );
}

#[test]
fn guard_flags_a_production_bind_but_allows_the_two_legitimate_forms() {
    // The guard is only worth having if it actually fires; pin its behaviour so
    // a future refactor can't quietly neuter it into an always-green check.
    //
    // Each fixture is assembled with `format!` rather than written out, so this
    // file contains no literal production pipe path and the guard's own walk
    // does not flag it — while the strings under test are still the real bytes.
    let prod = format!(r"\\.\pipe\wylde-{}", "workspaces");
    assert_eq!(
        offending_literals(&format!(r#"create(r"{prod}")"#)),
        vec![prod.clone()],
        "a literal production bind must be flagged"
    );

    let interpolated = format!(r"\\.\pipe\wylde-{}", "{service_name}");
    assert!(
        offending_literals(&format!(r#"create(&format!(r"{interpolated}"))"#)).is_empty(),
        "an interpolated service name is the rust/ convention — allowed"
    );

    let minted = format!("{prod}-test-4821-0");
    assert!(
        offending_literals(&format!(r#"create(r"{minted}")"#)).is_empty(),
        "a minted fixture name carries -test- — allowed"
    );
}

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
//! ## What the scan walks (#225)
//!
//! Two source classes, scanned with two different strictnesses:
//!
//! * **`tests/**` files** — a dedicated test file has no business naming a
//!   production pipe at all, so *any* production literal is the tell
//!   (`offending_literals`, whole file).
//! * **`src/**` files** — only their `#[cfg(test)]` regions
//!   (`cfg_test_regions`), and only for a fixture *bind*
//!   (`offending_binds`: a `create(<production literal>)`). Production code in
//!   `src` legitimately holds the real pipe literal (it *is* the service), and
//!   a `src` test module legitimately *names* it in a resolver assertion —
//!   e.g. `assert_eq!(pipe_name("lifecycle"), r"\\.\pipe\wylde-lifecycle")` —
//!   so the whole-file rule would false-positive here. The `src` half existed
//!   as a hole until #225: a fixture server stood up on a production name
//!   inside a `src/**` `#[cfg(test)]` block was invisible to the old walk,
//!   which only visited `tests/` directories.
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
//! | literal in the test (a `tests/**` file, or a `src/**` `#[cfg(test)]` bind) | `\\.\pipe\wylde-x` | **this scan** | #47, #75, #225 |
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

/// Classify and collect every `.rs` source in the GUI tree:
///   * `tests_out` — files under a `tests/` directory (a whole dedicated
///     test file; scanned in full).
///   * `src_out` — files under a `src/` directory (production crate source;
///     only their `#[cfg(test)]` regions are scanned — see #225).
///
/// A single descent fills both buckets so the tree is walked once.
fn gui_sources(dir: &Path, tests_out: &mut Vec<PathBuf>, src_out: &mut Vec<PathBuf>) {
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
            gui_sources(&path, tests_out, src_out);
        } else if name.ends_with(".rs") {
            let under = |seg: &str| {
                path.components()
                    .any(|c| c.as_os_str().to_string_lossy() == seg)
            };
            if under("tests") {
                tests_out.push(path);
            } else if under("src") {
                src_out.push(path);
            }
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

/// Concatenate the source text of every top-level `#[cfg(test)]`-guarded item
/// (a `mod` or `fn`) in a `src/**` file, and nothing else.
///
/// This is what makes the `src/**` scan safe (#225). Production code — the
/// real service binding its own production pipe — lives *outside* these
/// regions and is deliberately excluded, so it is never mistaken for a
/// fixture-test offender. Only test code compiled under `#[cfg(test)]` is
/// returned.
///
/// The extent of each region is `[the attribute … the next `}` at column 0]`.
/// rustfmt (gated by G6) puts every top-level item's closing brace at column
/// 0, so this needs no brace/string lexer. A `#[cfg(test)]` on a non-block
/// item (`use super::*;`) has a `;` before any `{` and contributes nothing.
fn cfg_test_regions(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim_start();
        // `#[cfg(test)]`, `#[cfg(all(test, …))]`, etc. — any cfg attr naming
        // the `test` predicate. Not the inner `#![…]` form.
        let is_cfg_test = trimmed.starts_with("#[cfg(") && trimmed.contains("test");
        if !is_cfg_test {
            i += 1;
            continue;
        }
        let start = i;
        // Walk forward to the block's opening `{`. If a `;` closes the item
        // first (a non-block `#[cfg(test)]` item), there is no region here.
        let mut j = i;
        let mut opened = false;
        while j < lines.len() {
            if lines[j].contains('{') {
                opened = true;
                break;
            }
            if lines[j].contains(';') {
                break;
            }
            j += 1;
        }
        if !opened {
            i = j + 1;
            continue;
        }
        // From just after the opening line, the region ends at the first
        // column-0 `}` (the top-level item's close).
        let mut k = j + 1;
        while k < lines.len() && !lines[k].starts_with('}') {
            k += 1;
        }
        let end = k.min(lines.len() - 1);
        for line in &lines[start..=end] {
            out.push_str(line);
            out.push('\n');
        }
        i = end + 1;
    }
    out
}

/// Like [`offending_literals`], but flags a production pipe literal ONLY when
/// it sits in *bind* position — an argument to a named-pipe server
/// `create(...)` — the fixture-server bind the #75 class is about.
///
/// This narrower rule is what the `src/**` `#[cfg(test)]` scan uses (#225). A
/// `src` test module legitimately *names* a production pipe without binding
/// it — e.g. `assert_eq!(pipe_name("lifecycle"), r"\\.\pipe\wylde-lifecycle")`
/// pins the resolver's output — so the whole-file [`offending_literals`] rule
/// (any literal is suspect), correct for a dedicated `tests/` file, would
/// false-positive here. A bind passes the literal to `ServerOptions::new()…
/// create(<literal>)`; a name-assertion does not.
fn offending_binds(code: &str) -> Vec<String> {
    const MARKER: &str = r"\\.\pipe\wylde-";
    let mut found = Vec::new();
    for (idx, _) in code.match_indices(MARKER) {
        let tail = &code[idx + MARKER.len()..];
        let end = tail.find('"').unwrap_or(tail.len());
        let service = &tail[..end];
        // Same carve-outs as offending_literals: interpolated names and minted
        // `-test-` fixtures are legitimate; a bare `wylde-` template is empty.
        if service.contains('{') || service.contains("-test-") || service.is_empty() {
            continue;
        }
        // Bind-scoped: the literal must be inside a `create(...)` call within
        // the current statement (back to the last `;` / `{` / `}`), not a bare
        // reference such as a `pipe_name(...)` assertion.
        let stmt_start = code[..idx]
            .rfind([';', '{', '}'])
            .map(|i| i + 1)
            .unwrap_or(0);
        if code[stmt_start..idx].contains("create(") {
            found.push(format!("{MARKER}{service}"));
        }
    }
    found
}

#[test]
fn no_gui_test_binds_a_production_pipe_name() {
    let mut test_sources = Vec::new();
    let mut src_sources = Vec::new();
    gui_sources(&gui_root(), &mut test_sources, &mut src_sources);

    assert!(
        !test_sources.is_empty(),
        "found no GUI test sources under {} — the walk is broken, not the tree clean",
        gui_root().display()
    );
    assert!(
        !src_sources.is_empty(),
        "found no GUI src sources under {} — the walk is broken, not the tree clean",
        gui_root().display()
    );

    let mut offenders = Vec::new();

    // Dedicated `tests/` files: any production pipe literal is the tell.
    for path in &test_sources {
        let Ok(src) = fs::read_to_string(path) else {
            continue;
        };
        for literal in offending_literals(&strip_line_comments(&src)) {
            offenders.push(format!("  {} binds {}", path.display(), literal));
        }
    }

    // `src/**` files: scan ONLY their `#[cfg(test)]` regions, and only for a
    // fixture *bind* (a `create(<production literal>)`) — production code
    // legitimately holds the real pipe literal, and a `src` test module
    // legitimately *names* it in a resolver assertion (#225).
    for path in &src_sources {
        let Ok(src) = fs::read_to_string(path) else {
            continue;
        };
        let test_only = cfg_test_regions(&strip_line_comments(&src));
        for literal in offending_binds(&test_only) {
            offenders.push(format!(
                "  {} (a src #[cfg(test)] block) binds {}",
                path.display(),
                literal
            ));
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

#[test]
fn guard_covers_a_src_cfg_test_bind_but_not_a_name_assertion() {
    // #225: a fixture pipe bind declared in a `src/**` `#[cfg(test)]` module
    // used to escape the guard entirely (the walk only visited `tests/`
    // files). Pin the tightened behaviour: the bind is caught, while the two
    // legitimate `src`-side uses of a production literal — production code
    // outside the test region, and a name-resolution assertion inside it —
    // are not.
    //
    // Literals are assembled with `format!` so this file carries no bare
    // production pipe path of its own.
    let prod = format!(r"\\.\pipe\wylde-{}", "workspaces");

    // A synthetic src file: production code binds the real pipe (legitimate —
    // it IS the service), a #[cfg(test)] module both asserts the resolver's
    // output (legitimate) AND stands up a fixture server on the production
    // name (the offender #225 adds coverage for).
    let offending_src = format!(
        "pub fn serve() {{\n\
         \x20   let s = ServerOptions::new().create(r\"{prod}\").unwrap();\n\
         }}\n\
         \n\
         #[cfg(test)]\n\
         mod tests {{\n\
         \x20   #[test]\n\
         \x20   fn name_resolves() {{\n\
         \x20       assert_eq!(pipe_name(\"workspaces\"), r\"{prod}\");\n\
         \x20   }}\n\
         \n\
         \x20   #[test]\n\
         \x20   fn fixture_uses_prod_name() {{\n\
         \x20       let _srv = ServerOptions::new().create(r\"{prod}\").unwrap();\n\
         \x20   }}\n\
         }}\n"
    );

    // The `#[cfg(test)]` region is extracted; production `serve()` is excluded.
    let region = cfg_test_regions(&strip_line_comments(&offending_src));
    assert!(
        region.contains("fixture_uses_prod_name") && !region.contains("pub fn serve"),
        "cfg_test_regions must capture the test module and exclude production code"
    );

    // Only the in-region BIND is flagged — once. The `pipe_name` assertion,
    // and the production `serve()` bind outside the region, are not.
    assert_eq!(
        offending_binds(&region),
        vec![prod.clone()],
        "the src #[cfg(test)] fixture bind must be flagged, and only it"
    );

    // Fail-before / pass-after: swap the offending bind for a minted fixture
    // name (the sanctioned form) and the same scan goes green.
    let fixed_src = offending_src.replace(
        &format!(r#"create(r"{prod}").unwrap();"#),
        &format!(r#"create(r"{prod}-test-4821-0").unwrap();"#),
    );
    // (Only the fixture bind carried a trailing `.unwrap();` twice — the
    // production `serve()` line matches too, but rewriting it to a minted name
    // is harmless for this assertion since we scan only the test region.)
    let fixed_region = cfg_test_regions(&strip_line_comments(&fixed_src));
    assert!(
        offending_binds(&fixed_region).is_empty(),
        "a minted -test- fixture name in a src #[cfg(test)] bind is allowed"
    );
}

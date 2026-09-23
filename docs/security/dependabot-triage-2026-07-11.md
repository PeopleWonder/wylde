# Dependabot alert triage — 2026-07-11

Triage of the five open Dependabot alerts on `PeopleWonder/wylde` (default
branch). Each alert is assessed for the vulnerable package, the manifest it
enters through, whether the vulnerable code path is reachable in the shipped
product, and the remediation actually available.

Context that drives most of the dispositions: Wylde completed a **full-Rust
cutover (R6, commit `2f5aa82`, "delete the Python runtime")** that removed
`uv.lock` and every Python runtime service. The only Python left in-tree is
stdlib-only dev tooling (`Core/harness/dev/wylde_check` + its tests and the N8N
tool stubs). `pyproject.toml` on the default branch declares
`dependencies = []`. The product ships **Windows-only** (`WyldeSetup.exe` /
`wylde-gui.exe`).

Summary:

| # | Package | Sev | Manifest | Disposition |
|---|---------|-----|----------|-------------|
| 17 | `transformers` (pip) | High | `uv.lock` | Stale — manifest & package removed in R6 |
| 19 | `soupsieve` (pip) | High | `uv.lock` | Stale — manifest & package removed in R6 |
| 20 | `soupsieve` (pip) | High | `uv.lock` | Stale — manifest & package removed in R6 |
| 1  | `glib` (Rust) | Moderate | `Core/GUI/Cargo.lock` | Not reachable (Linux-only, not built on Windows); upstream-pinned |
| 18 | `async-tar` (Rust) | Moderate | `Core/GUI/Cargo.lock` | Compiled but dormant (no untrusted-tar path); upstream-pinned |

---

## #17 — `transformers` remote code execution (High)

- **Advisory:** CVE-2026-4372 / GHSA-29pf-2h5f-8g72. Affected `< 5.3.0`,
  patched `5.3.0`. Malicious `config.json` `_attn_implementation_internal`
  field triggers download + execution of arbitrary code from an attacker Hub
  repo on `from_pretrained()`, bypassing `trust_remote_code`.
- **Entry:** transitive via `sentence-transformers 5.5.0 → transformers
  4.57.6`, recorded in `uv.lock`.
- **Reachability:** none. `uv.lock` was deleted in R6 (`2f5aa82`). No
  `transformers` / `sentence-transformers` importer remains; `pyproject.toml`
  has `dependencies = []`. The package is not installed or vendored anywhere in
  the tree.
- **Remediation:** already effected by the R6 Python deletion. Nothing to bump.
  Alert is stale against a removed manifest → **dismiss as "vulnerable code is
  not used."** (Dependabot did not auto-close it because the manifest was
  removed rather than the dependency re-pinned.)

## #19 — `soupsieve` memory exhaustion via large selector lists (High)

- **Advisory:** CVE-2026-49476 / GHSA-2wc2-fm75-p42x. Affected `<= 2.8.3`,
  patched `2.8.4`. Unbounded allocation compiling large comma-separated CSS
  selector lists (~488× amplification).
- **Entry:** transitive via `beautifulsoup4 4.14.3 → soupsieve 2.8.3` in
  `uv.lock`.
- **Reachability:** none — same basis as #17. No `beautifulsoup4` / `soupsieve`
  importer remains; `uv.lock` is gone.
- **Remediation:** already effected by R6. **Dismiss as stale.**

## #20 — `soupsieve` ReDoS via selector parser (High)

- **Advisory:** CVE-2026-49477 / GHSA-836r-79rf-4m37. Affected `<= 2.8.3`,
  patched `2.8.4`. Catastrophic regex backtracking on an unterminated quoted
  attribute-selector value (300-byte payload → multi-second hang).
- **Entry:** transitive via `beautifulsoup4 → soupsieve 2.8.3` in `uv.lock`.
- **Reachability:** none — same basis as #17/#19.
- **Remediation:** already effected by R6. **Dismiss as stale.**

---

## #1 — `glib` `VariantStrIter` unsoundness (Moderate)

- **Advisory:** GHSA-wrw7-89jp-8q8g (no CVE). Affected `>= 0.15.0, < 0.20.0`,
  patched `0.20.0`. An out-parameter pointer passed as `&p` instead of `&mut p`
  to the variadic `g_variant_get_child`; recent rustc drops the write, so
  `CStr::from_ptr` reads a NULL pointer → crash.
- **Current:** `glib 0.18.5` in `Core/GUI/Cargo.lock`.
- **Entry:** deep transitive in the GTK3 binding stack —
  `glib ← atk / gdk / gio / pango / cairo-rs / gdk-pixbuf / webkit2gtk ← gtk
  0.18.2 ← libappindicator 0.9.0 ← tray-icon 0.19.3` and `← wry 0.54.4`, both
  ultimately under `wylde-gui`.
- **Reachability:** **none in the shipped product.** The entire GTK3 stack is
  `cfg(target_os = "linux")`; `wry` and `tray-icon` use Win32 APIs on Windows.
  `cargo tree -i glib --target x86_64-pc-windows-msvc` returns *nothing* — glib
  is never compiled into the Windows build that Wylde ships. (It would compile,
  and the unsound iterator would be present, in a hypothetical Linux build.)
- **Remediation:** no clean bump. `cargo update -p glib --precise 0.20.0` is
  rejected — `libappindicator 0.9.0` requires `glib ^0.18` (there is no patched
  0.18.x/0.19.x; the fix ships only at 0.20.0). Reaching glib 0.20 requires a
  full gtk-rs 0.18 → 0.20 migration, which is blocked by `webkit2gtk`/`wry`
  pinning the GTK3 bindings at 0.18. **Deferred** (already listed under the prior
  advisory sweep). Given non-reachability on Windows, dismissing as "vulnerable
  code cannot be built/exploited on the shipped target" is defensible; left open
  and tracked pending a `wry`/gtk-rs major bump.

## #18 — `async-tar` PAX extension-header desync / entry smuggling (Moderate)

- **Advisory:** CVE-2026-53600 / GHSA-35rm-7j9c-2f7m. Affected `< 0.6.1`,
  patched `0.6.1`. A buffered PAX `size` record is mis-applied to an
  intermediary GNU longname header, desyncing the parser from a POSIX-correct
  reader → differential extraction (a scanner and the extractor see different
  files). Requires the consumer to extract an attacker-influenced tar stream.
- **Current:** `async-tar 0.5.1` in `Core/GUI/Cargo.lock` (GHSA range `< 0.6.1`
  includes 0.5.1; the described defect code is in the 0.6.0 line).
- **Entry:** `async-tar ← http_client ← gpui 0.2.2`, all from the pinned Zed git
  rev `b3d93d44`, under `wylde-gui`. It **is** in the
  `x86_64-pc-windows-msvc` graph, i.e. compiled into the Windows build.
- **Reachability:** effectively dormant. The only consumer is Zed's
  `http_client` (tarball extraction for e.g. language-server/extension
  downloads in upstream Zed). Wylde does not drive `http_client`'s tar
  extraction over any untrusted stream — the updater is a separate crate
  (`wylde-updater`, GitHub Releases + minisign, no tar). No code path feeds an
  attacker-controlled archive into `async-tar`.
- **Remediation:** no clean bump. `cargo update -p async-tar --precise 0.6.1` is
  rejected — `http_client` (the pinned gpui rev) requires `async-tar ^0.5.1`,
  and `0.5 → 0.6` is a semver-major change (also a runtime-feature-flag change),
  so a `[patch.crates-io]` to 0.6.1 would not satisfy the requirement or compile
  `http_client` unchanged. Clearing it needs bumping the `gpui`/Zed git rev to
  one whose `http_client` uses `async-tar 0.6.1`, or maintaining a fork — both
  out of scope for a minimal dependency fix. **Deferred**, tracked against the
  next `gpui`-rev bump.

---

## What was changed on the branch

Documentation only. No `Cargo.toml` / `Cargo.lock` / `pyproject.toml` change was
made:

- The pip alerts were already remediated by the R6 Python deletion — the
  manifest and packages are gone; there is nothing left to bump or remove.
- The two Rust alerts cannot be moved by `cargo update` (both blocked by
  upstream pins across a 0.x major boundary) and forcing them would require a
  cascading upstream migration that risks breaking the GUI build — worse than
  the (non-reachable / dormant) alerts they would address.

Recommended follow-up on GitHub: dismiss #17/#19/#20 as *vulnerable code not
used* (removed manifest); keep #1 and #18 tracked against a future `wry`/gtk-rs
and `gpui`-rev bump respectively (or dismiss #1 as not-buildable-on-target).

---

## Addendum — 2026-07-14 (hygiene pass)

Re-verified the two Rust alerts independently and stood up the automated
dependency machinery that was missing. See
[`dependency-hygiene-policy.md`](dependency-hygiene-policy.md).

**#1 `glib` — confirmed unfixable, hypothesis that `wry`/`tray-icon` are dead
Tauri deps is FALSE.** Both are live gpui-era code: `wry` is wrapped by the
`wylde-webview` crate and driven by `Core/GUI/Shell/src/shell_root.rs`
(`IframeHost`, `probe_url`, `slot_bounds`) for extension iframe panels;
`tray-icon` is wired by `Core/GUI/Shell/src/{main,tray,window}.rs` as the
`tauri::tray` replacement. Neither is removable, so `glib` can't be dropped that
way. It stays Linux-only / not-in-shipped-binary. **Accepted** in
`Core/GUI/deny.toml` (RUSTSEC-2024-0429) with a review date.

**#18 `async-tar` — confirmed unfixable, with new evidence.** Advancing the gpui
git rev cannot fix it: Zed's `http_client` still requires `async-tar ^0.5.1`, and
**even Zed `main` still pins `async-tar = "0.5.1"`** (checked 2026-07-14 against
`zed-industries/zed`), while Zed `main` has moved to edition 2024 (a large
breaking jump). No RUSTSEC id exists for it yet, so cargo-deny can't gate it;
**Dependabot remains the gate.** Documented in the hygiene policy.

**New advisories the fresh RustSec DB surfaced (post-dating this triage) and what
was done:**

| Crate | Advisory | Disposition |
|-------|----------|-------------|
| `crossbeam-epoch` 0.9.18 | RUSTSEC-2026-0204 (vuln) | **FIXED** — `cargo update` → 0.9.20 (both workspaces) |
| `anyhow` 1.0.102 | RUSTSEC-2026-0190 (unsound) | **FIXED** — `cargo update` → 1.0.103 (both workspaces) |
| `quick-xml` 0.30/0.39 | RUSTSEC-2026-0194/0195 (DoS) | Accepted — Linux-only (`xcb`/`zbus_xml`), not in shipped binary |
| `bincode` 1.3.3 | RUSTSEC-2025-0141 (unmaintained) | Accepted — direct dep, no safe upgrade (2.x rewrite) |

After the two `cargo update` fixes: `rust/` has **zero** outstanding
vulnerabilities; `Core/GUI` has only the Linux-only `quick-xml` entries left, all
allow-listed. `cargo deny check advisories` is green for both workspaces.

# Pre-Alpha Public-Release Preparation — 2026-05-31

> **Historical record — paths below are as-of 2026-05-31 and are deliberately NOT updated (#31).**
> This is a dated log of actions actually taken, so rewriting its paths would falsify the record.
>
> **Redaction note (2026-07-19).** The original version of this document quoted the removed
> identifiers verbatim in its "Before" column — including the maintainer's real WAN IP, real
> LAN subnet, personal email, and home directory paths. A scrub report that republishes what
> it scrubbed defeats itself, so those literal values are now redacted to descriptions.
> The *record* (what was found, how much, and what it became) is intact; only the raw
> personal values are gone. This is the one exception to the "not updated" rule above.
> The tree has since moved **out of the Obsidian vault** and is now a git repository; the
> `Obsidian Vault\Wylde` / `Obsidian Vault\Wylde-release` locations referenced below no longer
> exist. For current layout see [`../wylde-repo-organization.md`](../wylde-repo-organization.md).
> Do not use this document to locate anything today.

This document records every action taken to turn the private working repo into the
public-alpha copy at `Wylde-release/`. Three parts: **(1) copy**, **(2) scrub** (secrets /
personal info / dev artifacts), **(3) documentation audit**. All work happened in the copy;
the working repo was not modified.

- **Source (working copy, untouched):** `%USERPROFILE%\Documents\Obsidian Vault\Wylde`
- **Destination (this copy):** `%USERPROFILE%\Documents\Obsidian Vault\Wylde-release`
- **Date:** 2026-05-31

---

## Part 1 — Copy

`robocopy /MIR` mirror, excluding bulk/build/runtime junk so the copy is fast and clean:

- **Excluded dirs:** `target node_modules __pycache__ .venv venv .pytest_cache .mypy_cache
  .ruff_cache .git deps .wylde logs`
- **Excluded files:** `*.log *.bak *.swp .DS_Store Thumbs.db`
- **Result:** 1919 files / 625 MB copied, 0 failed (robocopy exit 1 = success).
- **Verified populated** three independent ways (`find`, PowerShell `Get-ChildItem`, Python
  `os.walk`) and by spot-checking key files (`launch_wylde.ps1`, `Core/GUI/Cargo.toml`,
  `rust/Cargo.toml`, `pyproject.toml`, `uv.lock`, `docs/`).
- **Working copy untouched** — confirmed at finish: source still contains the original
  maintainer-name (in `Core/GUI/Cargo.toml`) and real-LAN-subnet (in `VPN/`) values.

Note on layout: there is no Cargo workspace at the repo root. The two Rust workspaces are
`Core/GUI/` (the gpui desktop GUI, crate `wylde-gui`) and `rust/` (backend services).

---

## Part 2 — Scrub

### CRITICAL — secrets / credentials: **0 real secrets remain**

- **Secret-extension file scan** (`*.key *.pem *.p12 *.pfx *.crt *.cer id_rsa* id_ed25519*
  *.kdbx *.gpg *credentials* *.netrc wg*.conf`): only one hit —
  `Core/Memgraph/vendor/neo4j/neo4j.cer`. Inspected: it is a DER-encoded **public**
  DigiCert code-signing certificate (no `PRIVATE KEY`), part of the vendored Neo4j
  distribution. **KEPT** as a public vendor cert. *(Note: a `.cer` ships in the public repo
  by design.)*
- **No** real `.env` files, `.db`/`.sqlite` databases, private keys, PFX/P12, or WireGuard
  configs were present in the copy.
- **Hardcoded-secret content scan** (`sk-…`, `ghp_…`, `AKIA…`, JWT, quoted
  `password=`/`secret=` literals): **0 hits** in shipping code. The apr1 htpasswd hash in
  the device-gate tests is a synthetic fixture (password `letmein`); its username was
  scrubbed and it was retained as a test fixture.

### HIGH — personal info sanitized (verified 0 remaining, ripgrep + Python `os.walk`)

> **⚠️ This section's "0 remaining" was true on 2026-05-31 and DRIFTED.** By 2026-07-19 the
> tree had re-accumulated ~175 occurrences of the maintainer's first name across ~70 files
> (code comments, test fixtures, a benchmark rig label) and 11 home-directory paths across
> 8 files — reintroduced by ordinary day-to-day commits, because nothing enforced the
> guarantee after the one-time pass. A hand-audited number is not a gate.
>
> Both classes were re-scrubbed on 2026-07-19, and this time the guarantee is **enforced in
> CI** by `wylde_check` rule 55 (`no_personal_identifiers`), which fails the build if either
> pattern reappears. See [`../wylde_check_rules.md`](../wylde_check_rules.md). Treat the
> table below as a record of the original pass, not as a current-state assertion — the rule
> is the current-state assertion.

| Category | Before | After | Replacement |
|---|---|---|---|
| Maintainer's host name *(redacted)* | 0 (never in repo) | 0 | → `WYLDE_HOST` (rule applied; no occurrences) |
| Maintainer name (all forms) | ~334 occ / ~118 files | **0** | full name→"Wylde User"; possessive→"the Wylde user's"; bare→"the Wylde user"; test usernames→`wylde` |
| Maintainer personal email | 1 | **0** | → `user@example.com` (`Core/GUI/Cargo.toml`) |
| Real LAN subnet *(redacted)* | 68 occ / 13 files | **0** | → `192.0.2.x` (RFC 5737, last octet preserved) |
| Real WAN IP *(redacted)* | 183 occ / 1 file | **0** | → `203.0.113.1` (`VPN/data/wylde-link/endpoint-history.json`) |
| `10.x.x.x` | — | left as-is | RFC 1918 CIDR ranges / test & doc example values (per spec: leave test/example 10.x) |
| Maintainer home-directory paths | ~13 occ / 8 files | **0** | → `%USERPROFILE%\…` (or `C:\Users\<user>\…` in code comments) |
| `cloud.wylde.local:8443` | 0 in docs | 0 | overwritehost bug was config/code, not docs |
| mkcert CA subject | 0 | 0 | none present |

~120 files edited across `VPN/`, `rust/crates/wylde-*`, `Core/GUI/Frontend/Panels/*`,
`Core/harness/**`, `Core/Lifecycle/**`, `Voice/*`, `Extensions/**`, and ~35 docs.

**Test fixtures updated to stay green:** device-gate username (maintainer's)→`wylde` across 5
files (htpasswd seed line + every lookup/assert; the hash verifies on the password, not the
username); memgraph `actions.rs` and rag `search.rs` seed/query fixtures (maintainer's first
name)→`the Wylde user`; the `extending-wylde-services.md` tutorial sample; and markdown anchor
slugs paired with their headings.

### MEDIUM — dev artifacts: confirmed absent

- No `.pyc .swp .bak .log Thumbs.db .DS_Store`, no `.vscode/ .idea/ .wylde/ __pycache__/
  .pytest_cache/ .mypy_cache/ .ruff_cache/ node_modules/`.
- Runtime state removed from `data/`: deleted `data/state`, `data/tmp`, `data/model_registry`,
  `data/settings.json`, `data/.initialized`, and `data/manifests/*.json`. Kept the 10
  `data/contracts/actions/*.json` schemas and `data/manifests/.gitkeep`.
- `docs/_probe.txt` (scratch) removed.
- `.gitignore` rewritten to keep all of the above out long-term (target, caches, venv,
  node_modules, runtime data/state/logs, secrets/keys/env, editor/IDE, OS junk).
- The `Core/GUI/target/` produced by the build smoke-test (Part-3 verification) was removed
  afterward so the shipped copy carries no build artifacts (those also embedded the
  `C:\Users\<user>\…` build path).

---

## Part 3 — Documentation audit

- **Markdown total: 117** — 86 are bundled third-party JDK license texts under
  `Core/Memgraph/vendor/jdk/legal/**` (kept, untouched). The ~31 real Wylde docs were audited.

**Files changed / deleted:**

| File | Action | Summary |
|---|---|---|
| `README.md` | UPDATE | Full rewrite. Was Python-only/stale; now ships-ready public-alpha README: local-first + alpha disclaimer, Rust-first framing, native gpui `wylde-gui` GUI, service-architecture table, repo-layout table (notes `Gateway/` removed), build/run via `launch_wylde.ps1` + `cargo build`, security-model section, license note. |
| `WYLDE_ENDPOINTS.md` | UPDATE | Added current-implementation banner: Gateway is Rust `wylde-gateway` (Python `Gateway/` deleted); lifecycle/voice/vram-broker default to Rust; the `Gateway/app.py:NNN` citations are historical contract references. |
| `docs/wylde-repo-organization.md` | UPDATE | 3 current-state fixes: lifecycle default python→rust (crate + Core tables); `Core/Memgraph/` rewritten to "supervision-only, harness uses direct Bolt, Python pipe/IPC clone + graph_service removed". |
| `docs/diagnostic-archive/_bios_check_notes.md` | DELETE | Loose underscore-prefixed hardware/BIOS scratch note, referenced by nothing — not a product doc. |

**Broken internal links:** every `[](path)` / `![](path)` link resolved from its file's
location. 149 unresolved candidates, but **zero genuine broken doc links** — they are
root-relative source-code `file:line` citations (the design-doc house style, mostly in
`wylde-ollama-design.md` / `wylde-rust-migration-master-plan.md`), historical Svelte-file
references in the explicitly-archived `inference-bar-audit.md`, and intentional redacted
pointers into the maintainer's private memory store. No doc-to-doc link was broken; nothing
needed rewriting.

**Stale-concept fixes:** 5 edits across 3 files (above). Remaining Tauri/Svelte/Python-Gateway/
Memgraph-ipc/sync.py mentions live only in plan/archive/design docs as past-tense port
provenance (permitted) or in dated point-in-time reports (e.g. mypy baselines). Verified
`cloud.wylde.local` appears without `:8443` everywhere, `Core/Memgraph/` contains no Python
files, and `sync.py` appears in no markdown.

**Duplicates / loose docs:** the two GUI-extending docs (`extending-the-gui.md`,
`extending-the-wylde-gui.md`) are complementary (conceptual vs. recipe), not duplicates —
kept. One loose doc deleted (above).

**Placeholder / TODO / TBD:** scanned all docs; every hit is legitimate (tracked work-item
descriptions in plan/archive docs, honest "not-yet-wired stub" status such as
`Core/GUI/installer/README.md`, or template variables). None were fill-in-the-blank filler;
nothing removed (removing would misrepresent genuinely-unbuilt state).

---

## Verification (Definition of Done)

- ✅ `Wylde-release/` exists, structurally complete (1,927 files), source modulo excluded artifacts.
- ✅ **Zero CRITICAL secrets** (dual-engine grep; only public vendored `neo4j.cer` kept).
- ✅ **HIGH personal info sanitized** — re-confirmed maintainer-name=0, real-LAN-subnet=0, real-WAN-IP=0 across the whole copy.
- ✅ **MEDIUM dev artifacts gone**; `.gitignore` hardened; 0 `target/` dirs remain.
- ✅ Every `*.md` doc link resolves or is non-doc/historical (no genuine broken links).
- ✅ No deleted-concept prose describing current behavior; README current & ships-ready.
- ✅ `cd Wylde-release/Core/GUI ; cargo build --release -p wylde-gui` → **exit 0** ("Finished release in 2m 11s"; `wylde-gui.exe` produced).
- ✅ `wylde_check.run_all()` against the release root → **48/48 rules, 0 errors / 0 warnings / 0 info**.
- ✅ Working copy untouched (source still holds the original identifiers).

**Ready-to-upload path:** `%USERPROFILE%\Documents\Obsidian Vault\Wylde-release`

*Note: the build smoke-test `target/` was removed after verification, so a fresh `cargo build`
is required before running the GUI from the copy.*

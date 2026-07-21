# Changelog

All notable changes to Wylde are recorded here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org/), and pre-1.0 alphas may break between builds
(see [`docs/branch-and-release-policy.md`](docs/branch-and-release-policy.md) §3).

<!--
Maintenance: this changelog is hand-curated (deliberately richer than an
auto-generated bullet list). For any user-facing change, add an entry in the
matching section of the current unreleased version. `tools/changelog-draft.sh`
seeds a draft from Conventional Commits since the last tag — edit it into
narrative form.
Release lines: experimental builds ship 0.1.x (Beta channel); the stable gate is
0.2.0 (Stable channel), cut only on the maintainer's say-so.
State: the workspace version is now 0.2.0-beta.1, but it is NOT yet tagged/released
(that is #38, the maintainer's separate say-so). The section below is therefore
headed "[0.2.0-beta.1] — unreleased"; on release, replace "unreleased" with the tag
date and start a fresh [Unreleased] section above it for later work.
-->

## [0.2.0-beta.1] — unreleased

**Wylde 0.2.0-beta.1 is the first pre-release of the modern, all-Rust stack** (the stable
`0.2.0` cut remains gated on the maintainer's separate say-so, #38). The only
earlier tag, `v0.1.0-alpha.1` (2026-06-04, a GitHub *pre-release* on the Beta channel),
predates the full-Rust cutover entirely — it shipped the gpui desktop rebuild while the
runtime beneath it was still Python. Everything between that tag and this one was built in
the open on the `develop` line and is only now judged ready to carry a 0.2 version, so
this pre-release absorbs an unusually large body of work.

The headline changes: the **full-Rust cutover** (every Python runtime component ported to
Rust and its source deleted); a local-first **memory system** (short-term, long-term, and
reflection across the conversation, workspace, and long-term scopes); the **Thought Bubble
System** with pre-turn structural retrieval; a workspace **knowledge graph** with a native
gpui graph panel and an in-app IDE; **BM25 lexical retrieval + RRF fusion**; a definitional
**concept hierarchy** and a **concept-routing** decision layer (both isolated, default-off,
and byte-identical when disabled); and an **agentic reasoning tier** shipped `enabled:
false` as an opt-in experiment. Wrapping all of it is the **enforcement layer** whose
absence let the alpha ship broken — the GUI panel-walk (L7), the launch-and-verify preflight
and its commit-bound receipt, the benchmark regression gate, version-consistency (G7), and
the license/advisory gates — now wired so the class of defect that shipped before is blocked
rather than merely documented.

The entries below are long because the release is, and they are written to be read: each
says what changed and why it mattered. The release date is stamped when this version is
tagged on the maintainer's say-so (`docs/branch-and-release-policy.md` §5).

### Added

- **Green PRs into `develop` now merge themselves — no session left idle waiting to click merge.** With the
  strict up-to-date rule off on `develop`, a PR can merge the moment its checks pass, but nothing armed that
  merge, so the final step was still hand-babysat (a session opens a PR, sits waiting on CI, then needs a nudge
  to merge). A new `.github/workflows/auto-merge-develop.yml` runs on `pull_request: [opened, ready_for_review]`
  and calls `gh pr merge --auto --squash` for every qualifying PR, so GitHub completes the merge itself the
  instant the required checks go green (#189). This is **not** a gate bypass: native auto-merge respects the
  full `protect-develop` required-check set (backend build+test, GUI build, panel-walk L7, clippy/fmt, G7, the
  `personal-info scrub (G8)`, the cargo-deny advisory/license legs, branch target+name, conventional commits,
  changelog, and `linked issue`), so a PR that is red or unlinked simply never merges. It arms **only** PRs
  targeting `develop` (the `develop`→`main` and experimental promotions stay deliberately manual), skips drafts,
  honours a `no-auto-merge` label as an explicit hold, and excludes `dependabot[bot]` — those stay with the
  narrower, patch-only `dependabot-automerge.yml` (#68), which the two workflows partition cleanly by actor so
  the general one never loosens that stricter gating. Least-privilege `contents: write` + `pull-requests: write`
  on `GITHUB_TOKEN`; the arming job is advisory, not a required check, so it can never deadlock a branch.
- **Every PR now has to tie to a tracking issue, and every issue now gets a milestone automatically.** Two
  halves of one project rule — "every issue is attached to a milestone, every merge is tied to an issue" —
  turned from a norm into automation (#183). A new `linked issue` job in `.github/workflows/pr-checks.yml`
  fails any PR whose title, body, or introduced commits reference no issue (`#N`, or a
  `Closes/Fixes/Resolves/Refs #N` keyword), and it is a **required check** on both `protect-develop` and
  `protect-experimental`. The escape hatch is a `no-issue` label for a deliberate no-issue change; Dependabot
  PRs and the `develop`→`main` promotion are exempt by construction (they carry no single issue and must keep
  flowing — Dependabot auto-merge queues behind the required checks, so gating it would hang every bump). The
  label is evaluated at step level, not as a job-level `if:`, so the required context always reports a
  conclusion and can never leave the branch deadlocked on an "expected" check. On the issue side — which a
  required status check can't reach — a new `.github/workflows/issue-milestone.yml` auto-assigns the catch-all
  `0.x - backlog` milestone on `issues.opened`/`reopened` when none is set, with a weekly sweep for anything
  that slips through, under a least-privilege `issues: write` token (#177). `0.x - backlog` is a floor, not a
  verdict: triage still re-files into the right release milestone at will.
- **Wylde's updater now carries the whole stack, and the launcher always runs the current one.** Two
  halves of the same gap, fixed against one shared resolver. The self-updater was structurally
  GUI-only: it selected release assets by matching the literal `wylde-gui`, then `self_replace`d the
  running executable. The lifecycle daemon and every backend service were never fetched and never
  swapped — so because most of Wylde's logic lives in the backend, **a backend fix could not reach an
  installed user at all**, and a successful update left a new GUI sitting on top of a stale backend.
  Separately, the launcher resolved each binary independently, taking the first hit across
  `rust\bin` → `target\release` → `target\debug`; one stale artifact at an earlier candidate shadowed
  a fresh build indefinitely (the running stack had drifted days behind the tree with nothing saying
  so), and because the walk ran per binary, a single launch could mix binaries from profiles that
  have no version relationship to each other. Both now go through the new `wylde-stack` crate, which
  answers "what is the stack" by **discovery** — the in-tree core tier plus whatever the `Services/`
  bucket currently holds — and "where does it run from" by resolving that roster against **one**
  directory: the `current` pointer the updater maintains, or the build tree when no pointer exists.
  The updater fetches, individually verifies, and stages every member into a version directory before
  switching over with a single atomic pointer move, so "GUI new, daemon stale" is no longer a
  reachable state and a release missing a required binary is refused rather than half-installed.
  Desktop shortcuts now target the launcher rather than a build path, so they cannot go stale: they
  never name a version or a profile. The point of the shared resolver is that **adding the Nth
  service needs no edit to either the updater or the launcher** — a service dropped into `Services/`
  is picked up by both — and a coverage gate fails red if a daemon-managed service ever lacks a
  corresponding update/launch path, so the guarantee is checked rather than merely asserted.
  (#97, #92)

- **The updater no longer leaves a full copy of the stack on disk for every update it has ever
  applied.** Installs stage each release into its own `versions/<ver>/` directory and flip the
  `current` pointer to it — correct and atomic, but nothing ever removed the old directories, so an
  installed machine accumulated one entire stack per update, without bound, invisibly, and the leak
  worsened as the stack itself grew. Each successful install now prunes `versions/` down to a fixed
  retention window — the newly-installed **current** stack plus one previous, kept as a rollback
  fallback — so disk stays bounded by construction rather than by anyone remembering to clean up.
  The prune is deliberately ordered to be safe: it runs **after** the pointer has flipped, so the new
  stack is already live and its predecessor is still present the whole time — there is no instant in
  which the rollback fallback is gone but the new version is not yet committed — and it never touches
  the current stack or the retained previous. It enumerates `versions/` from disk and removes whole
  directories, so a new service's extra binaries under a version dir are covered with no edit, and a
  directory a previous run couldn't delete (a locked or in-use binary) is retried on the next install
  rather than leaking. A prune failure is logged and skipped: it can never fail an update that has
  already succeeded. (#139)

- **Wylde now reclaims disk when you switch the model behind a reasoning slot, instead of hoarding
  every model it ever pulled.** Until now the local model store had no bound and no cleanup: each
  time the default reasoner (or your chosen slot model) changed, the superseded model was left on
  disk forever — quietly growing into tens of gigabytes. A slot change now runs a *keep-only-
  referenced* pass wired directly to the change: the model the new configuration no longer
  references becomes eligible for reclaim, automatically, with no hand-maintained cleanup list — a
  future slot type inherits the same behaviour for free. Safety is deliberate and conservative: a
  model that is still referenced by any slot (reasoner / fast / embedder) or pinned is **never**
  touched, only the exact model a change *superseded* is ever considered (a model you pulled by hand
  and never assigned to a slot is never a candidate), and the pass is **announce-only by default** —
  it logs what could be reclaimed and its size but deletes nothing unless you opt in with
  `WYLDE_OLLAMA_RECLAIM_SUPERSEDED` (pin models to protect with `WYLDE_OLLAMA_GC_PINS`). New
  diagnostics surface the store's total and per-model on-disk size (`ollama.store_usage`) and the
  reclaim itself (`ollama.gc`).
- **The auto-updater's Settings controls now gate every outbound step behind an informed choice.**
  Wylde stays fully isolated by default (no update network call unless you turn updates on *and* opt
  into automatic checks); this pass adds the consent and acknowledgement surfaces around that default.
  Enabling **"Check automatically"** now opens a consent dialog that states plainly that Wylde will
  contact GitHub about once a week to check for a new version, and that nothing is downloaded or
  installed automatically — an available update always shows you its changelog and waits for you to
  **Accept** before any bytes are pulled (download-on-Accept; turning the option back off needs no
  dialog). When an update is found, the panel renders a **changelog card** with the release notes and
  two choices: **Accept** (download, verify, install) or **Decline — "Skip this version"**, which
  remembers that exact version so the weekly check stops re-offering it until a newer release appears
  (a manual "Check now" still surfaces it, so you can change your mind). Selecting the **Experimental**
  branch now raises a warning that it is for testing new features, may contain significant bugs, and
  that posting found bugs on GitHub helps development — shown only when switching *to* Experimental;
  switching back to Stable is immediate. The channel is now labelled **Stable / Experimental** in the
  UI (previously "Beta"; the on-disk value is unchanged). All controls are native gpui.

### Changed

- **The live-Memgraph/Neo4j tests now run against a real database in CI, so the
  graph layer's Cypher is exercised end-to-end instead of only against mocks
  (#121).** A new `live-graph (Neo4j Bolt) tests` CI leg installs the vendored,
  checksum-pinned Neo4j (`tools/install-neo4j.ps1`), boots it auth-off on
  `bolt://127.0.0.1:7687`, and runs the previously-dead `#[ignore]`d integration
  tests with `--ignored` (`wylde-workspaces` `integration_graph` +
  `integration_symbols_find`, `wylde-harness` `memgraph_live` +
  `memgraph_bolt_integration`). On its very first run the leg earned its keep —
  it caught a real live-DB bug the mocks never could (`symbol_context` returns
  zero callees against real Neo4j), now tracked as #203 and its test excluded
  from the leg with a pointer until fixed. The default
  `backend` job still runs DB-less and skips them, so the markers stay; a
  dedicated `--ignored` leg is what makes them live rather than dead. The leg is
  Windows + vendored Neo4j because the shipped DB is a JVM (not the Memgraph
  database) and the tests are `#![cfg(windows)]`, so a Linux service container
  would not compile them. It is intentionally not yet a required status check —
  it must be observed green on `develop` first, then added to the branch
  rulesets (the `gui-panel-walk` deadlock lesson).

- **Dependency bumps.** Routine updates carrying no API change and no code edit on our side.
  Kept as one list so the narrative entries below stay readable; each line is the dependency and
  the version span, newest wins where a crate was bumped in more than one manifest.
  CI actions first, then cargo crates.
  - `actions/checkout` v4 → v7 (#144)
  - `actions/setup-python` v5 → v7 (#144)
  - `dependabot/fetch-metadata` v2 → v3 (#144)
  - `anyhow` 1.0.103 → 1.0.104 (#145)
  - `async-trait` 0.1.89 → 0.1.91 (#145)
  - `chrono` 0.4.44 → 0.4.45 (#145)
  - `clap` 4.6.2 → 4.6.3 (#175)
  - `futures` 0.3.32 → 0.3.33 (#145)
  - `hyper` 1.9.0 → 1.11.0 (#145, #175)
  - `opener` 0.7.2 → 0.8.5 (#152)
  - `rfd` 0.15.4 → 0.17.2 (#147)
  - `scraper` 0.20.0 → 0.27.0 (#153)
  - `serde` 1.0.228 → 1.0.229 (#145)
  - `serde_json` 1.0.149 → 1.0.151 (#145)
  - `tokio` 1.52.3 → 1.53.1 (#145, #175)
  - `tray-icon` 0.19.3 → 0.24.1 (#150)
  - `unicode-segmentation` 1.13.2 → 1.13.3 (#145)
  - `uuid` 1.23.1 → 1.24.0 (#145)
  - `wry` 0.54.4 → 0.55.1 (#146)
- **`windows` crate 0.58 → 0.62.2 (breaking; required code changes).** Not a routine bump: the
  0.62 API reworked several Win32 wrappers to take `Option<…>` instead of a raw handle. `LocalFree`
  is now `LocalFree(Option<HLOCAL>)` and `SetNamedSecurityInfoW`'s owner/group parameters are
  `Option<PSID>`. Updated the DPAPI protect/unprotect paths (`wylde-shared/src/encryption.rs`) and
  the Windows owner-only ACL-hardening path (`wylde-shared/src/secure_file.rs`): the freed-buffer
  handles are wrapped in `Some(…)`, and the old null-`PSID` "leave owner/group unchanged" sentinel
  is now the clearer `None`. Behaviour is byte-for-byte unchanged. Consolidates and supersedes
  Dependabot #151, #155, #156 (one bump across the `rust/`, `Core/GUI/`, and `rust/tests/parity/`
  manifests). (#171)

- **`thiserror` 1 → 2 (major).** Bumped the single workspace pin (`rust/Cargo.toml`); all 34
  `#[derive(thiserror::Error)]` error enums across the backend crates compile unchanged — 2.0 is
  source-compatible with our derives (no `#[from]`, `#[error(transparent)]`, or display-attribute
  edits were required). Refreshed the `rust/`, `Core/GUI/`, `rust/tests/parity/`, and
  `tools/wylde-release/` lockfiles. Two transitive dependencies still pin `thiserror ^1`
  (`nvml-wrapper` in `wylde-vram-broker`, `neo4rs` in the release tool), so `thiserror` 1.0.69 and
  2.0.19 coexist in the graph — expected, not a conflict. Consolidates and supersedes Dependabot
  #149, #154, #157, #158. (#172)

- **`auto-launch` 0.5 → 0.6 (required a code edit).** The 0.6 release deprecated the boolean
  `AutoLaunchBuilder::set_use_launch_agent` in favour of an explicit `set_macos_launch_mode(MacOSLaunchMode)`
  enum, and under `-D warnings` the deprecation is a hard clippy error. Repointed the one call site
  (`Core/GUI/Frontend/Panels/Settings/src/ipc.rs`) to `MacOSLaunchMode::AppleScript` — the exact mode the old
  `false` mapped to. This is a macOS-only launch knob, inert on the Windows target the GUI ships to, so
  behaviour is byte-for-byte unchanged. (#148)

- **The NSIS installer has been removed from this repository.** It never produced a
  working install — the "Quick install" route documented in the README, and the
  `WyldeSetup-<version>.exe` asset attached to `v0.1.0-alpha.1`, do not work and
  should not be used. `tools/installer/`, `Core/GUI/installer/`, and
  `docs/installer.md` are gone; the work is parked at
  [PeopleWonder/wylde-installer](https://github.com/PeopleWonder/wylde-installer)
  (GPL-3.0-or-later, history preserved) and clearly marked non-functional planned
  future work. Two long-standing false claims are retracted there: that a pack +
  install + uninstall round-trip had been verified, and the pre-Rust-cutover
  description of bundling Python service trees. The only supported way to run Wylde
  is a development checkout — see [`docs/setup.md`](docs/setup.md).

- **`wylde_check` retired 22 dead rules (52 → 30 active) and repointed the file-size cap at Rust.** The #116
  fix made rules that couldn't fail go red instead of quiet; this is the cleanup that followed. A rule-by-rule
  audit against the real `develop` tree — not the synthetic fixtures the unit suite runs against — found 22 rules
  policing code the Rust cutover deleted. Fifteen were structurally dead: their target trees (`Core/Lifecycle/`,
  per-service `run.py`, `data/manifests/`, `Gateway/routes/`, `Core/harness/memory/`, the `tools/` manifest tree,
  the Python pipe modules) no longer exist, so each walked nothing and reported a clean pass — `manifest_paths`,
  `tool_id_regex`, `action_registry`, `gateway_scope`, `tool_docstring_required`, `spawn_paths_exist`,
  `run_py_entry_point`, `run_py_startup_sequence`, `shutdown_handler_marks_stopped`, `memory_layer_boundaries`,
  `action_docstring_required`, `manifest_sandbox_required`, `rest_routes_exist_in_service`,
  `every_service_has_manifest`, `service_manifest_schema`. Seven were Python-only linters whose last inputs were
  the checker's own tooling once every production `.py` was ported — `no_internal_http`, `import_paths`,
  `logging_setup_only`, `no_external_subprocess`, `test_init_present`, `no_bare_except`,
  `no_python_gateway_imports`; for the four of those with `*_rust` twins the coverage carries over, and the
  internal-port constants are retained in `_config.py` for a queued `no_internal_http_rust` (no Rust twin exists
  yet). Two rules were narrowed rather than dropped: `no_legacy_gui_imports_in_panels` lost its Svelte matcher
  (zero `.svelte`/`.js`/`.ts` files remain; its only finding was a false positive on a file-icon table row) and
  `first_party_manifest_must_be_gpui_view` lost its extension half (`Extensions/` is gone). `file_size_limit` —
  formerly a flat 700-line cap on Python files, of which almost none remained — now caps Rust sources
  (`rust/crates/*/src/**` + `Core/GUI/**`, excluding `/target/`) at the same 700 lines; the 91 files already over
  cap (worst: `Core/GUI/Frontend/Panels/Chat/src/chat_panel.rs` at 5298) are recorded as queued splits so the cap
  engages on new growth. The dispatcher's self-asserted count, module docstring, `docs/wylde_check_rules.md`, and
  `tools/preflight-function-test.ps1` are all reconciled to 30. This does not change the outstanding-findings
  backlog: every retired rule reported zero, so the 141 real findings (concentrated in `import_paths_rust` and
  `no_silent_error_swallow_rust`) are untouched and remain #114's scope.

- **`wylde-release publish` now refuses to cut a release without a real changelog.** Previously, when
  neither `--notes-file` nor `--notes` was supplied, publish fell back to a one-line auto-message
  ("Automated release X (channel).") — so a stable or experimental release could ship with no real
  release notes, and the updater's changelog card would then show that stub. The publish path now
  gates on the notes being present and non-placeholder (fail-closed, alongside the existing
  preflight-receipt gate); the auto-message is allowed only for a `--dry-run` rehearsal. A real
  release must pass `--notes-file` pointing at the version's `CHANGELOG.md` section. This makes the
  changelog a required, verifiable release gate rather than an optional courtesy.

### Fixed

- **Registering a 6th workspace no longer silently destroys the least-recently-used
  workspace's entire bundle (closes #133).** The MRU-5 window was a *disk cap*, not just
  a dropdown limit: promoting a 6th workspace `remove_dir_all`'d the LRU bundle — persona,
  `memory.jsonl`, RAG chunk store, conversations, and Memgraph nodes — with no prompt, no
  warning, and no undo, the exact inverse of the never-auto-delete decision taken for models
  (#120/#131). The window is now display-only: `WorkspaceState::promote` re-orders the `mru`
  list but never evicts, so the list is the full, unbounded enumeration of every workspace on
  disk. `promote_and_persist` no longer tears down anything; the sole bundle-destroying path is
  now explicit `delete`. A workspace pushed past the window stays fully on disk and enumerable
  (`persistence::load_all`); the dropdown still renders only the first `MRU_WINDOW`. Covered by
  a test that registers past the window and asserts the LRU's `definition.json`, `persona.md`,
  `memory.jsonl`, and `index/chunks.jsonl` all survive — and that no graph teardown is enqueued.

- **A lost, stale, or damaged workspace index can no longer silently orphan every
  bundle on disk (closes #134).** Every enumeration path read `index.json`'s `mru` list and
  nothing ever walked `<data_dir>/workspaces/`, so a bundle present on disk but absent from the
  index was invisible forever — nothing listed it, nothing could delete it. This lands the
  disk-walk half that complements the earlier fail-loud load guard (#140): a new
  `persistence::list_bundle_ids` walks the bundle directories, `registry::list_all` (exposed as
  the `workspaces.list_all` verb) enumerates every workspace on disk — MRU-ordered when the index
  is readable, recovered straight from disk when it is damaged (never folded to an empty list) —
  and reconciles stale `mru` ids (whose directory is gone) back out of the persisted index so
  they stop occupying a dropdown slot. Everything the walk surfaces is deletable through the same
  `delete` verb. Covered by tests for plant-and-find, corrupt-index recovery, stale-entry
  reconciliation, and orphan-is-deletable — each failing before this change.

- **A deleted workspace's memory sweep is now durable instead of fire-and-forget,
  so a down harness can no longer orphan `workspace_memories/` permanently
  (closes #166).** Deleting a workspace swept its durable memory tier (#135) and
  its bound flat-store conversations by firing two `tokio::spawn`ed IPC calls and
  only logging on failure — if the harness was down, slow, or restarting at that
  instant, the sweep was lost and `<data_dir>/workspace_memories/<id>/` orphaned
  forever. Because a workspace id derives from its folder (#28), re-registering
  the same folder later silently re-attached memories the user believed they had
  deleted — a privacy failure, not just stray disk. Rather than stand up a second
  bespoke queue, the durable pending-teardown queue #99 built for the graph
  cascade is now generalized from bare workspace ids to `(workspace id, teardown
  target)` pairs, with `target ∈ { graph, memory, conversations }`. The one drain
  dispatches per target and applies the same rule to all: dequeue only on
  `reply.ok`, leave queued (retry on the next create/activate/delete or at boot)
  on failure, and — critically for the memory tier — skip-and-dequeue without
  sweeping if the workspace is live again, so a delete-then-re-add can never wipe
  fresh memories. Since #133 the only teardown path is explicit `delete`
  (registering never evicts), so the memory + conversation sweeps are scoped to
  it and no non-delete path enqueues them. The old #99 bare-id queue file
  (`pending_graph_cleanup.json`) is migrated in place to the generalized
  `pending_teardown.json` on first read.

- **Convention-A data-root resolution now has ONE source of truth instead of
  seven copy-pasted `fn data_dir()`s, and the gate named for it can finally fire
  (closes #138).** The canonical `WYLDE_DATA_DIR` → `DATA_DIR` →
  `<WYLDE_ROOT>/.wylde/data` ladder — the root under which encryption prefs,
  graph profiles, `settings/*.json`, the memory tiers, and the workspace
  registry all live — was duplicated as a private resolver in six `rust/crates`
  modules (`wylde-shared/encryption`, `wylde-harness/memory/common` +
  `turn/reasoning/config`, `wylde-workspaces/common`, `wylde-concept-routing` and
  `wylde-concept-hierarchy` config), each free to drift, while the three tests
  named for the property asserted only that the resolved path was *non-empty* —
  green under any convention, including a regression to the process cwd. There is
  now one `wylde_shared::paths::data_dir` (with a pure, env-free
  `data_dir_under(root)` core); every other copy delegates via
  `pub use wylde_shared::paths::data_dir`. A new **required** backend test
  (`wylde-shared/tests/single_data_dir_resolver.rs`) walks every crate's `src/`
  and turns red if any file outside the canonical one reintroduces a
  convention-A `fn data_dir`, and the fake non-empty gates are replaced with
  assertions that pin the real `.wylde/data` shape. Scope: the genuinely
  different resolvers (`data/model_registry`, `device_gate/data`, `<ROOT>/data`)
  are not convention A and are unchanged; the GUI graph-settings panel keeps a
  sanctioned copy because it deliberately links no service crate (folding it in
  needs an approved dependency addition — see #138).

- **The L7 `panel-walk` gate's hand-kept crate list is now guarded against
  silent under-coverage (closes #95).** `cargo panel-walk` (the required `gui
  panel-walk (L7)` job) is a `-p`-scoped alias in `Core/GUI/.cargo/config.toml`
  listing today's nine panel crates. A tenth panel added without extending the
  alias would have its tests silently skipped by the gate while CI stayed green.
  A new static test in `wylde-panel-workspaces` asserts the alias's `-p` set
  covers every `Frontend/Panels/*` workspace member (excluding the `shared/*`
  helpers), turning silent under-coverage into a red that names the missing
  `-p`. It runs under the L7 gate itself and needs no `--workspace` (which would
  drag in the Shell's headless-unsafe `wry`/tray-icon graph the scoping avoids).

- **Every service's `ALL_ACTIONS` verb table is now asserted EQUAL to the live
  registry, both directions — and the reverse direction caught 11 verbs that had
  silently drifted (closes #130).** Each service's registration test asserted
  only `table ⊆ registry`; none asserted `registry ⊆ table`, the direction a
  developer trips (register a handler, forget the table). A missing entry leaks
  past `reset_for_tests` (which unregisters by iterating the table) and makes the
  gpui-contract lint flag correct callers as calling a nonexistent verb. A shared
  `assert_action_table_matches_registry(prefixes, all_actions)` helper now checks
  both directions and is wired into all eight services (`voice`, `lifecycle`,
  `n8n`, `treesitter`, `ollama`, `extension-bridge`, `lsp`, `workspaces`); the two
  hardcoded inline verb lists (`ollama`, `extension-bridge` — a third, already
  stale copy of the set) are deleted in favour of iterating `ALL_ACTIONS`, and
  `lsp` + `workspaces` gained the test they never had. Turning the reverse
  direction on immediately surfaced real drift in `wylde-workspaces`: ten
  `workspaces.hierarchy.*` verbs and `workspaces.conversations.refresh_summary`
  were registered and handled but absent from `ALL_ACTIONS` — now added.

- **The model-GC reference set now derives structurally from `ModelSlots`, so a
  new model slot cannot be silently unreferenced (closes #119).**
  `referenced_models` hardcoded a three-element array of slot fields
  (`reasoner`, `fast`, effective embedder). A fourth slot added later would not
  grow it — its model would be unreferenced by definition, and an operator-run
  sweep-mode `ollama.gc` (which makes every unreferenced model eligible) could
  delete a model a live slot needs. The set is now built from an **exhaustive
  destructure of `ModelSlots`** with no `..`, so adding a slot field fails to
  compile until it is explicitly classified as a reference root or excluded with
  a reason — the guarantee is enforced at compile time, not by a runtime test
  someone must remember. The `refs.len() == 2` count assertion (which only
  signalled "a number moved" and never fired for an empty-string slot) is
  replaced by a set-equality test asserting each slot is an independent root.

- **`wylde_check` rule 44's anti-pattern regex did not match the literal it
  exists to catch (closes #115).** Rule 44 (`boot_uses_daemon_managed_table`)
  forbids a hand-kept `const`/`static` SERVICES roster reappearing in the Rust
  boot path, but its regex had two blind spots: the prefix alternation covered
  only `SERVICES`/`ALL_SERVICES`, not a qualifier like `CORE_SERVICES` (the exact
  literal #101 deleted from `control.rs` — re-pasting it passed the gate clean);
  and it required an array type annotation `: [`, so every idiomatic slice-form
  roster `: &[&str] = &[` escaped regardless of name. The pattern now matches any
  uppercase-qualified `SERVICES` name in both array and slice forms, with
  regression tests asserting the previously-escaping cases fire and a scalar
  `SERVICE_*` const does not (the widened pattern must not over-match).

- **Two registered `conversations.*` verbs were missing from the harness pipe's
  `ALL_PIPE_ACTIONS` table, and no test guarded that direction (closes #142).**
  `conversations.get_active_for_workspace` and `set_active_for_workspace` are
  registered and work at runtime, but the verb table listed only eight of the
  ten. Because the `wylde_check` rules 38/48 treat that table as the registry's
  source of truth, every correct caller of the two verbs was flagged as calling
  a nonexistent verb; and because `reset_for_tests()` unregisters by iterating
  the table, the two verbs were never cleared between tests (registry leak). The
  two verbs are now listed, and `install_registers_every_action` asserts the
  table and the live registry are **equal** — the previous test only checked
  `table ⊆ registered`, so a verb registered but forgotten from the table (the
  direction a developer trips) passed. Removing any verb from the table now turns
  the test red and names it.

- **The `develop → main` promotion PR failed commit-lint on already-merged history.** The
  `conventional commits` check (`.github/workflows/pr-checks.yml`) linted the entire
  `origin/${BASE}..HEAD` range, so a merge-up PR re-linted every commit already vetted on
  `develop` and failed on one old Dependabot subject (`chore(deps)(deps): …`) that predates the
  rule and cannot be corrected without rewriting public history. The check now excludes commits
  already reachable from `origin/develop` (`git rev-list … "origin/${BASE}..HEAD" --not
  "origin/develop"`), so a promotion lints *nothing* pre-existing, while a feature PR into
  `develop` — and a `hotfix/* → main` that never went through `develop` — still lint their
  genuinely new commits, so a malformed *new* subject is still caught.

- **Deleting a workspace left its concepts in the graph forever.** The workspace-teardown cascade
  (`delete`, and MRU eviction) pruned a workspace's `Chunk` nodes and the `Entity` nodes left with no
  surviving mention — but never its `Concept` nodes. The `DELETE_WORKSPACE_CONCEPTS` statement existed
  and was wired into the *re-projection* path (a concept rebuild clears the prior set before writing the
  new one); teardown simply never ran it. So every deleted or evicted workspace left its whole concept
  layer — the `Concept` nodes plus the `CHILD_OF` edges between them — resident in Memgraph
  permanently, scoped to a workspace id that no longer exists. Worse, the orphan-entity prune
  `DETACH DELETE`s the entities those concepts pointed at, so the survivors were left holding `MEMBER`
  edges into deleted nodes: unreachable from the panel, never reclaimed by a later rebuild (which only
  clears the *current* workspace's set), and invisible to every other cleanup path. The graph accumulated
  one such island per workspace ever removed. Teardown now runs the concept sweep first — before the
  entity prune, so concepts are gone before the nodes they reference are — and reports a
  `concepts_deleted` count alongside the existing chunk and orphan counts (#117).

  The cascade's statement sequence is now declared once (`WORKSPACE_TEARDOWN_STEPS`) and consumed
  twice: the Bolt client executes it, and the unit-test graph mock replays it. That shared declaration
  is what makes the regression test real — the previous mock modelled only chunks and mentions, so a
  teardown that skipped concepts looked correct against a universe containing none. The mock now models
  `Concept`/`CHILD_OF`/`MEMBER` and panics on a cascade step it doesn't understand. Full proof against a
  live Memgraph is an `#[ignore]`d integration test pending the live-test work in #121.

- **Deleting a workspace left its durable memories on disk forever.**
  `<data_dir>/workspace_memories/<id>/` holds the curated, LLM-authored workspace memory tier. It lives
  outside the workspace bundle deliberately, so MRU eviction of a file index can never take the
  expensive-to-rebuild memories with it — but that also placed it outside the reach of *every* removal
  path, including explicit delete. The cleanup function (`delete_memory_dir`) was written, correct, and
  unit-tested, with **zero production callers**; its doc comment claimed it was "invoked on explicit
  user delete of a workspace", and nothing invoked it. Because a workspace id is derived from its
  folder path, re-registering the same folder re-derived the same id and silently re-attached memories
  the user believed they had deleted — a privacy consequence as much as a disk one. Explicit workspace
  delete now sweeps the tier via a new `memory.workspace.delete_all` verb. MRU eviction still does not,
  and must not: the sweep hangs off the delete verb, not the shared teardown primitive that eviction
  also funnels through (#135).

  The tier is owned by the harness while the delete verb lives in the workspaces service, so the sweep
  crosses a service boundary — best-effort and fire-and-forget, the same shape as the existing
  flat-store conversation sweep, because a Fast/Medium verb must not block on a peer service. It is
  therefore *not* durable: a harness that is down when a workspace is deleted logs a degraded sweep and
  the memories survive. The graph cascade solved the equivalent problem with a durable pending queue;
  this tier has no such queue yet.

- **`delete_memory_dir` would have obeyed an id that escaped its own directory tree.** It is a
  `remove_dir_all` over `workspace_memories_dir().join(workspace_id)`, and `Path::join` resolves an
  empty id to the tier **root** — every workspace's memories — while an absolute id (`C:\Windows`,
  `/etc`) discards the base entirely and a `../..` id walks out of the tier. Harmless while the
  function had no callers; a live hazard the moment one was added, since the tier is reachable over the
  pipe. The destructive path now validates the id and refuses all three, with the verb layer rejecting
  a blank id separately (defence in depth) (#135).

- **A damaged workspace index presented as "you have no workspaces" — and the next click made that
  true.** The registry's `index.json` (the active pointer plus the MRU list, which is also the
  authoritative set of workspaces the registry retains) was read by a loader that folded *every*
  failure — unreadable file, failed decrypt, unparseable JSON — into an empty `WorkspaceState`. A
  torn write or a decrypt failure was therefore indistinguishable from a brand-new install.

  Presenting empty is alarming but, by itself, recoverable: the bytes are still on disk. The
  destructive part was what came next. Every mutating path is load → mutate → save, so the first
  activate, create, or delete after a failed read would write the empty-plus-one state straight over
  the file that still held the real MRU — converting a recoverable file problem into permanent loss of
  every other workspace's registration.

  `load` now distinguishes *absent* (the legitimate first-run case, still an empty state) from
  *damaged*, and fails. Verbs answer a dedicated `index_damaged` error telling the user their
  workspaces have not been deleted and the file has been left alone, instead of rendering an empty
  list. As a second, independent guard, `save` refuses to overwrite a damaged index at all, so even a
  caller that wrongly defaulted a failed read cannot destroy the bytes. Read-only consumers that only
  want "which workspace is active" (the file watcher, the symbol index) opt into the old quiet
  degradation through an explicit `load_or_default`, which is documented as never safe on a path that
  writes state back (#140).

- **Changing the embedding dimension destroyed every stored memory vector, and the recovery function
  the destruction relied on did not exist.** The memory tiers' vector mirrors
  (`long_term.vec.bin`, each workspace's `memory.vec.bin`) recorded only a format version and a
  width. Loading one at a different width returned an empty store and left the old file in place —
  which sounds harmless, but the next write persisted that empty store straight over it. One `warn!`
  was the only trace. Worse, the mirrors carried **no embedding-model identity at all**, so swapping
  `WYLDE_EMBED_MODEL` at the same width kept every prior vector and silently compared it against
  vectors from a different model forever, degrading search quality with no signal anywhere. The
  workspaces RAG index already stamped its model and rebuilt on mismatch; the memory tiers did not,
  and that asymmetry was the bug.

  The on-disk envelope is now version 3 and stamps `embed_model`. An incompatible mirror — wrong
  width *or* wrong embedder — is moved aside to `<path>.incompatible` instead of being left to be
  overwritten, so the vectors survive. Version-2 files load transparently and adopt the current model
  on their next persist (#136).

- **`reindex` did not exist.** Three separate doc comments justified the behaviour above by claiming
  the mirrors were "rebuilt by `reindex` from the JSON if the two ever drift". There was no
  `reindex` — `git grep 'fn reindex'` over the harness returned nothing. The safety property the
  destructive path depended on was fictional, and the mirrors drifted permanently partial in ordinary
  operation too: whenever the embedder is down or over its 1.2 s budget the record saves JSON-only,
  and nothing ever revisited it, so semantic search quietly skipped those records for good.

  There is now a real rebuild, exposed as `memory.long_term.reindex` and `memory.workspace.reindex`.
  It re-embeds the authoritative JSON and writes a fresh, stamped mirror. Critically, a rebuild that
  embeds *nothing* — the embedder is down — refuses to persist and leaves the existing mirror
  untouched, rather than completing the destruction it was meant to repair; and a partial rebuild
  reports its shortfall instead of claiming success. The false doc claims have been replaced with
  what actually happens (#136).

- **A concept rebuild could silently spend your hand-authored relations, in two ways.** Typed concept
  relations (positive / "IS NOT" / dependency) are hand-authored and irreplaceable. The stable-id
  machinery that protects them across a rebuild is real and works — semantic ids are minted ordinals
  carried over by centroid similarity, never content-derived, never recycled, and authored edges are
  flagged rather than deleted. But it collapsed completely in two situations, both silent.

  **An empty or torn chunk index.** `build` chooses semantic clustering only when the index holds at
  least two usable vectors, and otherwise falls through to the directory-cluster fallback — which
  replaces the entire auto-generated concept set, keeping only manually-authored concepts. So a purge,
  an interrupted reindex, or a data directory resolved against the wrong working directory would drop
  every semantic concept. That is *unrecoverable*: because ordinals are never reused, a later rebuild
  over a restored index mints new ids that can never re-match the relations authored on the old ones.
  The edges survive on disk, permanently inert. The build now **refuses** in that situation, naming
  what would be lost, unless the workspace has no semantic concepts at risk or the caller passes
  `force`.

  **An embedding-width change.** Carry-over pairs prior centroids with new drafts only where the two
  vectors are the same length, so a change in embedding width makes carry-over arithmetically
  impossible — every concept is reminted and every authored relation dangles in a single build, with
  nothing but a non-zero count in the reply to say so. The build now detects a carry-over pool that
  cannot possibly match the incoming vectors and refuses, again overridable with `force` (#137).

- **The Relations editor showed broken relations as if they were fine.** When a rebuild drops a
  concept, the backend flags every relation pointing at it as `dangling` — retained on disk, excluded
  from routing — and has always sent that flag over the wire. The Relations sub-tab's view model did
  not deserialise it, and the row model dropped it again, so an edge that had gone inert rendered
  identically to a live one. The Hierarchy sub-tab badges the same flag correctly, so the two views
  told opposite stories about the same data. Dangling relations now carry a "dangling — re-point"
  badge, matching the Hierarchy treatment (#137).

- **A pre-manifest workspace index could have its vectors permanently mislabelled as compatible.**
  The RAG index records the embedding model and width it was built with, and forces a full rebuild
  when they no longer match. An **absent** manifest, though, was treated as "no rebuild needed", so a
  legacy index upgraded in place through the delta path's mtime fallback — deliberately, to avoid a
  mass re-embed. That is only safe if the stored vectors came from the current embedder, and an absent
  manifest is exactly the case where that cannot be known.

  Swapping the embedding model before the first post-manifest pass therefore kept every old vector
  verbatim and then wrote a fresh manifest naming the **new** model: a mixed, silently incomparable
  vector set, permanently blessed as compatible because every later compatibility check passed against
  a record that had never been true. A legacy index that actually holds embeddings now forces a
  rebuild, so its manifest describes its real contents; a never-indexed workspace, or one whose chunks
  carry no embeddings, is unaffected and still takes the cheap path (#136).

- **Four `wylde_check` rules could not fail, and one had been red and unnoticed for months.** The lint
  engine's rules 38 and 48 (`panel_verbs_exist_in_harness_registry`,
  `gateway_verbs_exist_in_harness_registry`) loaded their verb registry from two constants that both named
  files deleted or renamed by the Rust cutover — `rust/crates/wylde-harness/src/pipe.rs` (now
  `pipe/mod.rs`) and `Core/harness/pipe/__init__.py` (gone entirely). The registry came back empty, both
  rules hit an `if not registry: return out` bail, and an empty findings list is indistinguishable from a
  clean pass. They reported success for having checked nothing, leaving 46 Gateway `harness_dispatch`
  callsites across 8 route files and the whole panel→harness edge unguarded. Repointing them surfaced
  **8 real latent defects on live REST routes** — the Gateway still dispatches `workspaces.*` verbs the
  harness explicitly retired and now answers with `no_action`, plus two Chat-panel conversation verbs.
  Rule 31 (`shutdown_reaps_manifest_orphans`) was a different failure of the same family: correctly
  hardened to error on a missing target, but pointed at `Core/Lifecycle/daemon_state/__init__.py` in a
  tree where `Core/Lifecycle/` has zero files — so it was genuinely failing, and nothing was running the
  engine to notice (#114). It is repointed at the Rust lifecycle crate, following the guarantee to where
  it actually moved: teardown no longer reaps (Rust `stop_all_daemon_managed` only *halts* the sweep so an
  in-flight tick can't rewrite a manifest mid-teardown), and the boot path sweeps instead, before the
  first `start_<service>()`. Rules 44/45 gained comment-stripping — `_require_token` was a bare substring
  test, so deleting the real `boot_sequence()` call and leaving the doc comment that merely mentions it
  kept the rule green. An unloadable registry is now a hard `error` everywhere: a rule that cannot load
  its input has not passed, it has failed to run. (#116)
- **New meta-rule 51 `rule_targets_exist` stops a rule from silently going dead a third time.** A rule
  pointed at a deleted file does not go red, it goes *quiet* — the tree looks greener the more of the
  engine rots. This happened to rules 44/45 (#101) and then to 38/48 (#116), both caught by hand months
  late. The new rule asserts every path the engine is configured to inspect still exists, and fails the
  PR that deletes one, naming the rule it just disarmed. (#116)
- **Quit now actually stops the whole stack.** The GUI's shutdown carried two hand-typed arrays naming
  four of the eleven killable services, so `voice`, `extension-bridge`, `ollama`, `harness`, `treesitter`,
  `workspaces`, `n8n` and `vpn` survived Quit holding VRAM and named pipes. The failure was silent because
  the drain wait polled *the same four names*: once those exited it concluded the stack had drained,
  returned success, and the hard-kill fallback that would have caught the other eight was never reached —
  a clean-looking shutdown that wasn't one. Both sets now derive from the stack roster
  (`wylde_stack::shutdown_targets`), so a service is covered on both paths the moment it has a roster row,
  and the lifecycle daemon rides the roster's daemon tier instead of being retained by hand. Fixing only
  the kill list would have left the early exit in place, so both halves changed together. `wylde-stack` is
  dependency-lean and was already in the GUI's lock graph via `wylde-updater`, so this cost no new
  dependency — the tokio/anyhow objection that deferred the earlier attempt pointed at `wylde-lifecycle`,
  the wrong crate. A new counting gate,
  `rust/crates/wylde-stack/tests/shutdown_target_coverage.rs`, drops a synthetic service on disk and
  requires the real derivation to carry it onto both paths; it also reads `Core/GUI/Shell/src/shutdown.rs`
  across the workspace boundary and fails if a hand-typed image list reappears. It lives in the `rust/`
  workspace because that is the only one whose `cargo test` runs in CI. Docs corrected alongside: the
  `daemon_managed` module doc and #101's commit message both claimed the hard-kill list "derives from this
  table by construction", which was never true as shipped, and `wylde_check` rule 45's exemption for those
  constants is withdrawn. (#124)

- **Re-indexing a workspace no longer leaks orphaned graph chunks, and removing one now cascades to the
  graph.** A workspace's `Chunk` id embeds the file mtime, so any re-save re-keys every chunk — and two
  Memgraph write paths leaked as a result. A forced full re-index (embed model/dim/version change) was
  purely additive: it `MERGE`d a fresh chunk id for every mtime-drifted file while the superseded nodes
  stayed behind forever, so rebuilding N times grew the graph N×. Separately, graph teardown lived only
  in the explicit-delete handler, fire-and-forget — so every MRU-*evicted* workspace orphaned all its
  chunk and entity nodes with nothing to clean them up, and a transient graph blip during a delete
  orphaned silently. Full re-index now does a true replace (delete-then-write the workspace's chunks
  before the upsert, preserving authored entities and their relations), and both removal paths — explicit
  delete and MRU eviction — funnel through one durable teardown primitive that enqueues the workspace on
  an encrypt-at-rest pending queue and drains it against the graph, dequeuing only on success (a blip
  re-defers instead of orphaning) and skipping a re-created folder-derived id so fresh data is never
  wiped. The delta/watcher paths were already correct and are unchanged. Fixes #99.

- **A newly-added core service can no longer be silently skipped on shutdown.** The 12 in-tree
  daemon-managed services were enumerated by hand in five parallel places (boot, shutdown,
  `dispatch_start`, `dispatch_stop`, and the manageable-core set) with nothing keeping them in sync —
  so forgetting the shutdown line when adding a service orphaned it on quit with nothing red. And the
  static gate meant to catch this (wylde_check rules 44/45) pointed at `launcher.py`/`shutdown.py`,
  files the Rust cutover deleted, guarded by `if file.exists()`, so it ran over nothing and passed
  green — a dead gate. Boot, shutdown, and dispatch now all derive from one `DAEMON_MANAGED` source of
  truth (one row per service; the two deliberate asymmetries — the user-started VPN and the boot-only
  no-op memory scheduler — are typed flags, not silent omissions), so adding the 13th core service is a
  one-row change covered on every path by construction. A crate test asserts the boot/shutdown/dispatch
  sets agree and is proven able to fail (desync one path → red); wylde_check rules 44/45 are repointed
  at the live table so the gate actually fires. No user-visible behaviour change — the same services
  boot and drain in the same order. (#101)
- **Log files no longer grow without bound — every sink now inherits one rotation policy.** Wylde had
  no log rotation anywhere: every persistent log was opened append-only with no size and no age cap, so
  `ipc.jsonl` had quietly grown to ~179 MB (and climbing ~179 MB/month, per install), with the gateway
  audit logs (`gateway.jsonl`/`egress.jsonl`), the GUI error sink (`gui_errors.jsonl`), and the Neo4j
  console-capture log leaking the same way — a silent disk-filler with no crash to warn you. The central
  logging module now owns a shared rotating file sink that every Wylde-owned log routes through by
  construction: each file is capped (default 10 MiB) and a few rotated generations are kept (default 5),
  bounding any one log to ~60 MB instead of forever. Both limits are overridable via `WYLDE_LOG_MAX_BYTES`
  and `WYLDE_LOG_KEEP_FILES`, but the defaults bound growth out of the box. Because the policy lives at the
  chokepoint, any log a future service opens is bounded automatically, and a new architecture check turns
  an ad-hoc uncapped log-append red in CI. (The bundled Neo4j already rotates its own internal log via
  log4j2, so that one is left to it — Wylde only bounds the separate console-output capture.) Fixes #98.

- **`service.shutdown_all` no longer under-counts the vram-broker.** Its summary
  (`stopped`/`count`) omitted the broker even when it had just been stopped, because the teardown
  reporter `is_or_was_tracked` stat'd `wylde-vram-broker.json` — the broker's *pipe*-prefixed name —
  while the broker self-registers its manifest under its short name, `vram-broker.json`. So the
  predicate was unconditionally false for the broker and a real (non-nospawn) shutdown dropped it from
  the summary; the broker itself *did* stop (its stop keys off the process/pipe), the count just lied.
  The registry already worked around this exact quirk (`registry.rs` ~146) — one quirk, two consumers,
  only one patched (found via #80). The reporter now resolves the broker's short manifest alias, scoped
  to the broker alone and kept out of `manifest_path_for` so the daemon's manifest *writers* still
  derive the canonical path for every other service. A test drives the full real teardown through the
  `service.shutdown_all` action and is proven able to fail (reverting the fix → broker absent, count 0).
  (#84)
- **The Workspaces graph-IPC test no longer claims the live service's pipe.** `integration_graph_ipc`
  stood up its fixture server on the **production** endpoint (`\\.\pipe\wylde-workspaces`), which the
  real service already owns — so it failed with `ERROR_ACCESS_DENIED` / `ERROR_PIPE_BUSY` on any machine
  actually running Wylde, and blocked `cargo panel-walk` (the L7 gate's own invocation) on a live rig.
  It passed in CI throughout, because CI never runs the stack — the inverse of a flake, and the reason
  it survived review. The root cause was a missing seam rather than a bad constant: the GUI *client*
  (`wylde_gui_pipe`) resolved the pipe name itself with no injection point, while the service side
  (`WYLDE_WORKSPACES_PIPE_NAME`) and the whole `rust/` workspace already had one (#29). `pipe_name()`
  now consults a `test-support`-gated override, so a fixture server owns a private per-process pipe and
  the shipped Shell keeps **no** override path at all — deliberately not an env var, which would have
  been a live pipe-hijacking surface. A new static check (`fixture_pipes_are_private.rs`) scans the GUI
  tree for literal production binds inside the already-required `gui panel-walk (L7)` context; static
  because CI, having no live stack, can never observe this class at runtime (#75).
- **Three eval/bench targets no longer default to a folder that doesn't exist.** `lexical_eval.rs`,
  `live_eval.rs` (`live_data_dir()`) and `index_bench.rs` each fell back to a hardcoded
  `%USERPROFILE%\Documents\Obsidian Vault\Wylde-release` path when `WYLDE_ROOT` was unset. That vault
  is gone, so the fallback silently read a dead directory and the evals reported an empty corpus rather
  than a misconfiguration — the same flattering-green shape #28 was made of. They now **fail closed**
  with a message naming the variable to set (`WYLDE_EVAL_DATA_DIR` / `WYLDE_ROOT`); `index_bench` exits
  `2` with a usage line. The #31 scrub swept docs and missed these because they're Rust.
- **The three private plan docs that had no backup now have one.** `privacy-plan.md`,
  `wylde-android-app-plan.md` and `wylde-rust-migration-master-plan.md` (retired `legacy` — the
  full-Rust cutover it plans already happened) moved into the `wylde-planning` repo, reachable at
  `docs/plans/` through the junction, and their `.gitignore` entries are gone. That closes the
  one-disk-no-backup durability gap for them; the remaining entries in that list are still one-disk.
  Companion-doc links in `wylde-pairing-future-cd.md`, `wylde-passwords-self-healing-extension.md` and
  `wylde-phase5-cutover.md` were repointed at `plans/` so they don't dangle.
- **`docs/wylde-repo-organization.md` no longer tells you the repo isn't a repo.** The stale-vault-path
  scrub (#31) turned up one reference that was worse than a dead path: a doc marked
  `status: living reference` whose §1 stated the tree lived at `%USERPROFILE%\Documents\Obsidian
  Vault\Wylde\`, had no `.git/`, would make `git status` "refuse", and that version history was
  therefore implicit in progress-memory files with every file "authoritative current state". The tree
  is under git with `develop` as trunk, so a living reference was actively instructing readers to
  distrust git. §1 now describes the actual git layout, and §11's auto-memory path derives its slug
  from wherever the repo lives instead of hardcoding the vault one. Paths are repo-relative on purpose
  so they don't rot the same way twice. `WYLDE_ENDPOINTS.md:504` (`cwd=vault root` → `repo root`) also
  scrubbed.
  - **`docs/security/pre-alpha-release-2026-05-31.md` deliberately keeps its vault paths** — it's a
    dated log of actions actually taken, and rewriting it would falsify the record. It gets a header
    note (paths as-of that date, locations gone, don't navigate by it) instead of a scrub. Same call
    for `docs/mypy_baseline.txt`, whose paths are captured tool *stdout*; it's a Python-era artifact
    due for deletion with the Python scrub (T1.2), which is where that decision belongs.
- **The Dashboard's service-health strip now derives from the stack roster, so no daemon-managed
  service can silently lack a Stop button.** The strip rendered from a hand-kept nine-name array
  (`MONITORED_SERVICES`) that had already drifted once and was short by four — `wylde-workspaces`,
  `wylde-treesitter`, `wylde-n8n`, and `wylde-vpn` had no tile, and so no health dot and no #35 Stop
  control — while the test that appeared to cover it (`service_health.len() == MONITORED_SERVICES.len()`)
  compared the list to itself and was vacuously true for any content. The strip's membership now comes
  from `wylde_stack::roster()` — the same discovery the updater and launcher follow — so every service,
  in-tree or dropped into the `Services/` bucket, gets a chip and (where its roster `Tier` makes it
  daemon-managed) a Stop, with no list to edit. `offers_stop`'s exclusion likewise derives from the
  service's role (`Tier::Daemon`/`wylde-lifecycle`), not a name literal. `wylde-memgraph`, which is
  JVM-supervised and so carries no roster binary, stays on the strip via one typed, documented carve-out
  mirroring `wylde_stack::shutdown_targets::NON_ROSTER_GUI_IMAGES`. A falsification test drops a synthetic
  `Services/` service into a tempdir and asserts it appears on the strip with no code edit; reverting the
  derivation turns it red. (#123)

### Changed

- **The GPLv3 license gate is now a REQUIRED check, and the ruleset JSONs match live
  again.** #52 built the license gate but merged it reporting-only, so a PR introducing a
  GPL-incompatible dependency went red and stayed mergeable — a linter, not a gate.
  `cargo-deny (licenses) (rust/Cargo.toml)` and `cargo-deny (licenses) (Core/GUI/Cargo.toml)`
  are now required on both `protect-develop` and `protect-main`. Safe to require because
  `license-check.yml` is unfiltered and therefore always reports — the #49 lesson (**never
  require a path-filtered context**, or GitHub hangs every PR that touches none of those
  paths) held here rather than being relearned.
  - **Fixed live/file drift that would have silently un-required the advisory gate.** #49
    added its two `cargo-deny (advisories)` contexts to the *live* rulesets via `gh api` but
    never updated `.github/rulesets/*.json`, leaving the files listing 9 contexts while live
    carried 11. Since applying a ruleset is a **replace, not a merge**, the next apply from
    those files would have quietly dropped the advisory requirements. Both JSONs now carry
    the full **13** contexts, verified live after applying.
  - `docs/enforcement-matrix.md` rows 12/12c and the required-checks note were stale (they
    still described `cargo-deny` as path-filtered and deliberately not required); they now
    match reality and record both traps.

- **The GUI's required check now enforces 890 tests instead of 41.** The `gui panel-walk (L7)`
  gate ran through the `panel-walk` cargo alias, which carried `--test panel_walk` — meaning it
  ran `tests/panel_walk.rs` **and nothing else**. Roughly 130 behavioural windowed tests that
  already existed and already passed — chat workspace-id scoping on send
  (`Chat/tests/dock_scoping.rs`, `conversations.rs`), memory copy-in provenance
  (`Memory/tests/copy_in.rs`), workspace registry nav (`Workspaces/tests/registry_nav.rs`),
  settings prefs dispatch (`Settings/tests/prefs_dispatch.rs`), device pairing cancel
  (`Devices/tests/cancel_pairing.rs`), and the rest — were enforced by **nothing**. They ran only
  under a full local `cargo test`, which no required check performs, so a regression they would
  have caught turned nothing red and the coverage could rot silently. Dropping the `--test` filter
  collects coverage the project had already paid for. Verified green first: **890 passed / 0
  failed** via the exact CI invocation, ~1 min warm. (issue #56; enforcement-matrix row 4b.)
  - **The `-p` crate scoping is untouched and must stay** — NOT `--workspace`, so the gate still
    never links the Shell's tray-icon/wry graph or the `rust/` audio stack (`wylde-voice`/cpal,
    which segfaults headless). Widen test *targets*, never the crate set.
  - **The status-check context name is deliberately unchanged** even though the job now runs more
    than the panel-walk. Renaming a required context means the old one never reports again, and
    GitHub blocks every PR forever waiting for it — including the PR doing the rename, which then
    cannot merge to fix itself. Same family as the #49/#57 "never require a path-filtered context"
    lesson. The rename recipe (add the new context live first, merge, then drop the old) is
    recorded in `ci.yml` and the alias comment.

### Added

- **A one-click Stop control on the Dashboard service console.** The GUI could start and restart
  backend services from anywhere (decision 7) but had no way to *stop* one — the lifecycle
  `service.stop` verb existed and nothing drove it. Each running service chip now offers a Stop
  button (rendered only where a stop is a live action: the service is up and isn't
  `wylde-lifecycle` itself, which serves the request); clicking it dispatches `service.stop` and
  re-probes just that service so its chip flips without waiting for the 5 s refresh. No error
  banner by design — the console degrades per card, so a failed stop leaves the chip green, the
  honest signal. Closes the last named Tier-C control from #35; the new `service_control.rs` test
  drives it under the required `gui panel-walk (L7)` check and is proven able to fail (pointing the
  control at `start_service` turns it red).
- **Tier-C coverage for two critical-path controls: type-and-send, and happy-path device
  pairing.** Both are controls #35 names, and both were untested at the seam a user actually
  drives.
  - **`Chat/tests/type_and_send.rs`** enters at the *composer*, not at `send_user_message`.
    The turn dispatch itself was already covered — what nothing touched was everything
    upstream of it: the `prompt_input` → `InputEvent::Submit` → `submit_text` wiring that
    pressing Enter goes through. It asserts the typed text reaches the turn, the composer is
    cleared afterwards (or the user silently re-sends), a whitespace-only Enter starts no turn,
    and — the one with teeth — a **double Enter starts exactly one turn**. That last is the
    regression `starting` exists for: between Enter and `start_turn` returning, `active_turn_id`
    is still `None`, so a second Enter would slip past that guard and start a duplicate turn.
    Verified non-vacuous by deleting the `starting` guard and watching the test fail with a real
    double-send.
  - **`Devices/tests/complete_pairing.rs`** covers the path a user actually takes; its sibling
    `cancel_pairing.rs` only covered the abort. **No real peer device is needed** — the panel
    never talks to the phone, it polls `device_gate.get_pairing_status`, so "a phone completed"
    is just the server reporting `{pairing_active: false}`. Asserts the card closes itself and
    the new device lands in the list, and that a **transient** status failure keeps the card open
    (a blipping device-gate must not strand a user mid-pair against a code the server still
    considers live). Drives the poll loop with `advance_clock` rather than sleeping — the first
    use of it in the GUI suite; the loop waits on a gpui executor timer, so this is deterministic
    and runs in 0.04s.
  - Both run automatically: `tests/` targets auto-discover, and `cargo panel-walk` (the required
    `gui panel-walk (L7)` check) runs every test target in the 9 panel crates since #56 dropped
    its `--test` filter. Neither needed a `Cargo.toml` change — both panels already carry the
    test-support dev-dep block.

- **L5 shipped-config assertion — the experimental reasoning tier can no longer ship
  switched on.** The reasoning tier is a post-0.2 experiment that must ship
  `enabled:false`. `ReasoningConfig::default` said so and was unit-tested — but a unit test
  only ever proved the **fallback**. Nothing checked the config the shipped system actually
  obeys, so a `reasoning.json` shipping (or being written) with the tier on would have sailed
  through a fully green, launch-verified receipt. `preflight --launch` now runs
  `l5.reasoning_disabled` (issue #27), which folds into the receipt's `gates` map like every
  other check and counts toward `launch_verified`.
  - **Asks the running harness, not a file.** The check calls `settings.reasoning.get` and
    asserts `enabled:false`. `ReasoningConfig::current()` is the value the turn engine actually
    obeys, already resolved through the product's own `WYLDE_DATA_DIR`/`DATA_DIR`/`WYLDE_ROOT`
    chain — so one live read covers both a shipped file that enables the tier and an in-memory
    value that disagrees with the file. Reading the JSON ourselves would re-implement that
    resolution and could pass while the running system disagreed.
  - **Fails closed, and not skippable.** A missing or non-boolean `enabled`, or a harness that
    won't answer, is a FAIL — "couldn't determine" never counts as "it's off". Unlike the slow
    functional checks it is exempt from `--skip-functional` (it's a single cheap pipe read): a
    release-grade receipt should never be able to omit "did we ship the experiment switched
    on?". The verdict logic is split into a pure `reasoning_verdict` and unit-tested for the
    fail-closed contract without needing a live stack. (enforcement-matrix row 14;
    `release-checklist.md` L5 — previously a manual "also confirm".)

- **L2/L3 launch-and-verify preflight gate — the check that would have caught every
  "shipped broken" defect.** `wylde-release preflight --launch` (and the standalone
  `wylde-release smoke`) now *launch the shipped artifacts and exercise the assembled,
  running system*, feeding each result into the same commit-bound `preflight-receipt.json`
  that `publish` already gates on. Unit tests verify code; only launching verifies assembly.
  - **L2 cold-start** — spawns the real daemon (`wylde-lifecycle.exe`) and GUI
    (`wylde-gui.exe`) the way the launcher does, from a **neutral working directory** so a
    pass proves env-var resolution rather than cwd luck, and asserts each starts, stays up,
    and binds what it should (`\\.\pipe\wylde-lifecycle`; GUI = process-alive + no panic —
    window content stays the CI panel-walk's job). Attaches to an already-running daemon
    instead of spawning a sibling stack.
  - **L3 service-health** — discrete, individually-reported assertions: daemon pipe answers ·
    services discovered (`service.list`) + core services reachable on their own pipes · VRAM
    broker sees the GPU · Ollama has the reasoner + embed models · **Memgraph holds real
    data** (Bolt node counts > 0 — the empty-graph boot bug a port-liveness ping structurally
    cannot see) · RAG answers a fixture query · a chat turn completes · a memory round-trips.
  - **Un-skippable + fail-closed.** Every check fails closed (can't determine → FAIL); the
    receipt gains `launch_verified`, and `publish` now refuses a receipt that is green but not
    launch-verified (deliberate `--no-preflight-receipt` escape hatch unchanged). Everything
    spawned is torn down (graceful `service.shutdown_all` + `taskkill /T` backstop) — no
    orphan processes, no pipe collisions with a parallel session. New deps on the standalone
    `wylde-release` crate: `rmp-serde` (a tiny hand-rolled msgpack pipe client, wire-compatible
    with `wylde_shared::ipc`) and `neo4rs`/`tokio` (the Memgraph content query, same Bolt
    driver the product uses). (roadmap T0.1; enforcement-matrix row 14.)

- **GUI panel-walk test suite (L7) — the answer to "does every page load?"**
  Every one of the 9 panels (Chat, Dashboard, Memory, Workspaces, Models,
  Tools, Devices, Remote Access, Settings) — plus the Workspaces subtabs — now
  has a headless windowed `#[gpui::test]` panel-walk (`tests/panel_walk.rs`)
  that mounts the real view the way the Shell does and asserts it loads without
  panic and detects its error state, under four backend conditions: healthy,
  backend **down** (the daemon-in-no-spawn-mode case that shipped broken),
  backend **error envelope**, and **empty**. Closes the Dashboard / Models /
  Remote Access / Tools **zero-coverage gap**. Run the whole gate with
  **`cargo panel-walk`** (from `Core/GUI/`); it now runs **headless in CI** as
  the `gui panel-walk (L7)` job — the windowed gpui tests were verified to run
  on the CI runner with no desktop session (gpui's mock `TestPlatform`), so the
  suite gates **every PR**, not just the local preflight. `ScriptedBackend`
  gained path-based routing (`on_path` / `on_path_err`) for the action-less
  Remote Access panel. (issue #35, roadmap T0.1b; enforcement-matrix row 4b.)

- **Benchmark regression gate + preflight receipt.** Wylde had benchmark
  *harnesses* but no recorded baselines and no gate — a benchmark run by hand
  and eyeballed is an experiment, not enforcement. New `wylde-release bench`
  runs the eval harnesses against live Ollama, compares each metric to a
  committed baseline (`benchmarks/baselines/wylde-benchmarks.json`) with a
  **noise-calibrated per-metric threshold** — fail on a real regression, warn on
  a small one, flag an improvement to re-record — and appends every run to a
  trend history. The reasoning fast/think arms are baselined with real recorded
  numbers (fast 7.5 s median / think 30.9 s, success + token cost); retrieval
  (BM25/RRF invariants) is wired and gates once the tree is re-indexed. New
  `wylde-release preflight` runs the gate plus version-consistency (G7) and
  writes a **receipt bound to the commit**; `wylde-release publish` now refuses
  to ship without a green, current receipt (a stale or dirty-tree receipt can't
  validate a new build). See `benchmarks/README.md` for the design and the
  honest limitations. (roadmap T0.1 — the preflight receipt; enforcement-matrix
  rows 21–23.)

- **Indexer progress + ETA.** The workspace re-index now reports real, live
  progress instead of a bare "Indexing…". The indexer emits a structured
  snapshot — current phase (scanning / embedding / saving), files and chunks
  done vs total, a rolling throughput (chunks/sec), and a computed ETA
  (remaining ÷ rolling rate) — over the existing `RagState` → `list_mru`
  channel the GUI already polls (no new channel). The Workspaces card replaces
  the static "Indexing…" text with a live progress affordance: a status line
  (`Embedding · 46% · 612 / 1038 files · ~2m 30s remaining`) above a progress
  bar, and the Re-index button shows the percent. It stays graceful before the
  total is known — an indeterminate "Scanning files…" state during the walk —
  then switches to the determinate bar + ETA once counting is done. A dev
  bench (`examples/index_bench.rs`, isolated index dir, graph-write disabled)
  calibrates the ETA against measured throughput on the real repo.

- **Thought Bubble System — structural awareness for chat.** A floating
  Thought-Bubble composer layer over the chat input, with a unified
  `Ctrl+Z` undo timeline spanning both typed text and bubbles. The composer is
  symbol-aware (a `Ctrl+P` palette resolves code symbols) and, before each turn,
  an AI context-gather hook performs structural retrieval — it detects symbols
  and anchors in the prompt, pulls a bounded k-hop code neighbourhood
  (callers/callees/types), the user profile, short-term memory, and workspace
  notes, then evicts to a token budget and injects them as named prompt slots.
- **Anchors & Vocabulary.** A durable anchor/vocabulary layer (workspace + global
  stores, shared tokenizer, human-friendly aliases) with a Vocabulary tab, an
  LLM-proposal review queue, a composer "Anchor-this" action, recommended-cleanup
  / stale-mark / archive semantics, and a graph vocabulary overlay.
- **User profile.** A `user_profile` module (name / style / freeform rules) with
  an editable Settings section and an Accept / Edit / Reject queue for
  model-proposed profile changes; user edits always win, proposals are
  spam-gated and time-suppressed. Encrypted at rest (DPAPI).
- **Workspace knowledge-graph verbs.** New read/query surface over the workspace
  code graph: `workspaces.graph` (cached projection), `symbols.find` (in-memory
  fuzzy symbol index), `workspaces.symbol_context` (k-hop caller/callee/type
  neighbourhood with git-blame), `anchors.*`, and scoped chat-history search.
  A file watcher keeps the graph fresh via per-file delta-upsert (and graph-clean
  on delete).
- **Workspaces graph panel.** A native gpui graph visualization for the workspace
  code graph: force-directed layout (Barnes-Hut, off-thread physics worker),
  plus deterministic hierarchical and stable-grid layouts with animated 500 ms
  swaps; space-map navigation (zoom, breadcrumb, exit edges), auto-clustering
  with expand-in-place, a clusters-first "galaxy" tier with aggregate edges,
  viewport culling / LOD, fit-to-view, and a settings menu + per-workspace layout
  profile library. Every colour/size is read from the locked Visual Style v1
  theme.
- **Workspaces IDE.** An in-app IDE for the active workspace: jailed
  `workspaces.fs.*` verbs (read / write / list_dir), a Files + Editor tab shell,
  a from-scratch gpui code-editor element with syntax highlighting, a lazy
  file-tree, and cross-panel deep-links (vocab word → graph node, GraphView
  `focus_node`). An optional `wylde-lsp` service wraps rust-analyzer to provide
  in-editor diagnostics, completions, and hover.
- **Concept system + concept-routing (R0–R4).** A concept layer over the index
  (schema, directory-labeled cheap concepts, then semantic concepts via
  embedding clustering + centroids + curation) feeding concept-driven retrieval,
  a freshness signal, and an additive four-colour highlight. A browse surface
  (Concepts sub-tab, hybrid search, vocab hierarchy). Concept-**routing** ships
  as an isolated, **default-OFF, byte-identical-when-disabled** crate: toggle +
  route-and-log → typed-relation store + spreading-activation engine → relations
  authoring GUI → a curate-before-inject menu with Augment injection →
  scoped-lens narrowing + typed dependency-tree viz → an eval harness with
  calibrated thresholds. Augment is the default mode; Replace is opt-in.
- **Definitional concept hierarchy (H0–H6).** A navigable, drill-down DAG that
  unifies concepts + vocabulary anchors into one `{id, label, definition,
  parents, children}` node model — every node carries a definition; you drill
  until leaves are definition-only. Shipped as an isolated, **default-OFF,
  byte-identical-when-disabled** crate (`wylde-concept-hierarchy`) that
  *projects* the view read-only from the existing concept / anchor / relation
  stores (multi-parent preserved, diamonds/cycles guarded, the definitional
  ancestor-chain accessor), plus a thin additive `hierarchy.json` overlay for
  net-new authored data: authored/overriding definitions by a priority ladder, a
  never-reused `node:<n>` id allocator, authored containment edges, and node
  merges — all with the `Relation.dangling` retain-but-exclude rule. A deletable
  `workspaces.hierarchy.*` verb seam (`get_tree`, `get_node`, `set_definition`,
  `add_edge`, `remove_edge`, `merge_nodes`) maps the Core `Concept` into the
  crate's Core-free `ConceptView` so the crate never touches Core. A read-only
  **Hierarchy** sub-tab (in the isolated `hierarchy/` GUI folder, fourth tab of
  the Vocabulary tab) renders the DAG as a cycle-safe, indented drill-down —
  definitions shown at every level with a priority-ladder source badge, a "needs
  definition" badge on `Missing` nodes, "also under: …" for multi-parent nodes,
  a selected-node ancestor-chain breadcrumb, and a Graph deep-link via the focus
  bus. The sub-tab also **authors**: edit/override or clear a node's definition,
  mint brand-new authored nodes, add/remove containment edges, and merge/unmerge
  nodes (a target picker over the loaded universe) — with an "authored edges &
  merges" panel that surfaces dangling records for re-point/remove. Toggle OFF ⇒
  the verbs are inert, the sub-tab renders an inert disabled state, and the
  overlay is never written; deleting the crate + bridge + overlay + sub-tab
  folder reverts to today. **H5 retrieval injection** rides the existing
  `### Concepts` slot: for each curated concept it adds a high-signal
  definitional ancestor-chain line (`Label — definition — under Parent — under
  Root`), blurb-first so token-budget eviction sheds snippets before
  definitions, Augment-only, missing-definition nodes skipped — and gated
  identity-when-off (the block is never added unless the toggle is on, so today's
  prompt is byte-identical). **H6 containment-spread** wires the hierarchy's
  parent/child containment edges into the spreading-activation router as a
  *separate*, gated propagation channel (not a `RelationKind` — the
  `concept_relations.json` wire shape stays frozen): activation flows along
  containment with an asymmetric decay (child→parent strong, parent→child weak —
  both tunable knobs, conservative defaults), reusing the same Dijkstra
  relaxation + cycle guard as the dependency step, and slotted before the IS-NOT
  inhibition so a strong exclusion still has the last word. The channel is
  sourced at the workspaces wiring layer from the applied hierarchy graph and
  mapped into the router's node space, so the routing crate stays decoupled from
  hierarchy storage. **Doubly identity-safe:** the master toggle OFF ⇒ no
  containment adjacency is passed (built without touching the hierarchy stores),
  and even ON an empty adjacency is the spread step's identity — so routing is
  byte-identical to today unless containment edges actually exist and the toggle
  is on.
- **Tree-sitter expansion.** Code outline + highlight verbs (with a graph-panel
  outline card) and added JSON, TOML, YAML, and Bash grammars.
- **Conversation export / import.** An escape-hatch to move conversations in and
  out.
- **Out-of-tree runtime foundation.** Core's tracked tree stays "just Core"; three
  out-of-tree buckets (`Services/`, `Extensions/`, `Core/Plugins/`) ship empty
  and are populated out-of-band, each keeping its own `.git`. The lifecycle
  registry descends into `Services/*`, dynamically supervises siblings, resolves
  per-service data dirs (`service_paths.json` + `WYLDE_<SVC>_DATA_DIR`), and
  cleanly no-ops when a bucket is absent. Adds a `cargo xtask build-all`, a
  compiled-in plugin mechanism (`wylde-plugin-api` SDK + reference plugin), and
  N8N as a first-class Rust service (`wylde-n8n`).
- **GUI test harness.** A gpui windowed-test harness with dock-scoping tests
  across the Workspaces, Chat, Memory, Editor, Files, and Graph surfaces.
- **Lexical (BM25) retrieval + RRF fusion (default OFF).** A per-workspace
  pure-Rust [tantivy](https://github.com/quickwit-oss/tantivy) inverted index
  over the *same* chunk corpus the dense index already holds, fused with the
  existing cosine retrieval via Reciprocal Rank Fusion so an exact-token recall
  signal (rare identifiers, error codes, literal names the embedder blurs) sits
  alongside semantic relevance. Behind a `settings.lexical.*` master toggle that
  is **OFF by default** — OFF is byte-for-byte today's dense-only behaviour. The
  lexical index is built from the post-exclusion chunk set (never a fresh walk,
  so it can never drift from `chunks.jsonl`), holds term postings + chunk ids
  only (no second copy of chunk bodies), and stays in step via the existing
  content-hash manifest (full rebuild + cheap embed-free backfill + incremental
  watcher delta). Under fusion a strong BM25 hit at low cosine bypasses the
  absolute cosine floor (the recall win) while a query off-topic to both signals
  still injects nothing; the anchor-bias is reworked from a substring boost into
  an IDF-weighted, exact-token BM25 sub-query. A dense/lexical/fused eval harness
  (with a lexical gold class) proves the recall gain and the semantic
  no-regression guardrail.

### Changed

- **`tools/seed-github-project.sh` seeds the whole tracked backlog, not a frozen
  slice of it.** The script carried two hand-kept lists — an `ISSUE_TIER` map and a
  literal `for n in 25 … 40` loop — and every issue filed after the script was
  written (#41, #43, #44, #47, #49) was added to the board by hand and never made it
  back into either list. A board rebuilt from scratch would have silently come up
  five issues short. The loop now iterates the `ISSUE_TIER` map directly (numerically
  sorted), so the map is the single source of truth and adding an issue is a one-line
  change that cannot drift. The missing issues are now in the map with their Tiers,
  along with the newly-filed #55/#56/#57. Re-running remains a no-op against a
  fully-seeded board.

- **New issues (and same-repo PRs) now land on the Roadmap board automatically.**
  Milestoning was already automated (`issue-milestone.yml`) but board *membership*
  was not — an issue got a card only if a human remembered, and it had drifted (an
  audit found 31 milestoned issues off the board and 11 closed issues still sitting
  in Todo). A new `add-to-project.yml` workflow (`issues: [opened, reopened]`,
  `pull_request: [opened]`) adds each item to `projects/1` via
  `actions/add-to-project`, pinned to a commit SHA and authenticated with the same
  `PROJECT_TOKEN` classic PAT `roadmap-dates.yml` already uses — the default
  `GITHUB_TOKEN` cannot write a user-owned Project. It declares a least-privilege
  `contents: read` (the board write goes through the PAT, not `GITHUB_TOKEN`), so it
  stays clear of the `actions/missing-workflow-permissions` class fixed in #177, and
  it skips Dependabot and fork PRs, whose events carry no repo secrets, and it
  no-ops with a notice (never a red X) if `PROJECT_TOKEN` is unset. The one-time
  backlog gap was also backfilled by hand. **Two one-time manual steps are still
  required** before automation is live: (1) the `PROJECT_TOKEN` repo secret must be
  configured — it is currently unset, so both this workflow and `roadmap-dates.yml`
  no-op; and (2) status transitions need a manual enable — GitHub exposes no API to toggle a Project's built-in workflows
  (only `deleteProjectV2Workflow` exists), so *Item added → Todo*, *Item closed →
  Done*, and *Pull request merged → Done* must be switched on once in the Project's
  Workflows settings.

- **Clippy (G4) + fmt (G6) CI gates are now LIVE.** The two staged enforcement
  gates were armed: a new `clippy (G4) + fmt (G6)` CI job runs
  `cargo fmt --all -- --check` and
  `cargo clippy --workspace --all-targets --locked -- -D warnings` across every
  CI-built workspace (rust/, Core/GUI/, tools/xtask, tools/wylde-release) and
  **fails the build** on any warning or unformatted file. Getting there took a
  workspace-wide `cargo fmt` (its own `chore(fmt)` commit) and a behavior-neutral
  clippy cleanup — derivable `Default`s, `contains` over `iter().any(==)`,
  struct-init over `default()`-then-reassign in tests, scoping a cfg(test)
  `test_support` to `pub(crate)`, and a justified `await_holding_lock` allow on
  env-serializing async tests. Harness lib stays 1168/0. Enforcement-matrix rows
  10 + 11 move from ⏳-staged to ✅-live. (issue #32)
- **Full-Rust cutover.** Every remaining Python runtime component was ported
  to Rust and its source deleted (~350 files): the Lifecycle daemon +
  rollback path (`Core/Lifecycle/`), the Python harness runtime
  (`Core/harness/` — pipe verbs, memory layers, tooling, model registry,
  backend), the shared IPC helpers (`Core/shared/`), and the Memgraph
  Python wrapper (the lifecycle daemon now supervises the bundled Neo4j JVM
  directly). New in Rust with this wave: `memory.reflect` for all three
  scopes (conversation reflection, workspace curation, long-term
  consolidation) and the background memory scheduler (same
  `scheduler_state.json` + `WYLDE_SCHED_*` envs), now a tokio task inside
  `wylde-harness` gated on `WYLDE_HARNESS_SCHEDULER`.
- **Rust-only boot.** `launch_wylde.ps1` lost its Python daemon fallback and
  PYTHONPATH overlay; the per-service `WYLDE_<SERVICE>_IMPL=python`
  strangler flags now only log a warning. The kept Python — the
  `wylde_check` lint tool (`Core/harness/dev/`) and the stdlib N8N tool
  stubs — is dev-only; `pyproject.toml` carries no runtime dependencies and
  the stale `uv.lock` was removed.
- **Images extracted to a service.** The Images suite was lifted out of Core into
  a standalone `wylde-images` service (subtractive removal from Core, with a
  removability acceptance check).
- **Security boundary hardened (P1–P4).** The gateway gained an egress SSRF guard
  (deny-list + DNS-rebinding pin + host allowlist); the extension bridge gained a
  capability-checked inference gate (`inference.embed` / `inference.chat`
  forwarders) and a least-privilege, allowlist-scrubbed spawn environment; the
  webcrawler is now gateway-only for egress, and spawned-process cwd is
  placeholdered.
- **Prompt engineering (B-series).** Per-model `num_ctx` overrides now drive the
  slot budget; windowed conversation history is sent every turn; long-term memory
  and the auto-summary are injected into dedicated tiers; hardcoded prompts were
  migrated into a catalog (with a golden-snapshot harness and a lint rule banning
  literal prompts); a post-turn extraction pass assigns importance; the base
  instruction is capability-conditioned and the message layout is cache-aware;
  vectors use int8 scalar quantization.
- **RAG relevance levers (2.1–2.5).** MMR diversity rerank, dynamic
  where-warranted top-k, conversation-aware query construction, anchor-biased
  retrieval, and an active-file / current-focus boost.
- **Chat scoping.** The Workspaces dock no longer shares the global ChatPanel
  singleton; global Chat is strictly workspace-free, while in-workspace chat gets
  a per-workspace conversation list + switcher, create-and-bind, a last-open
  pointer, and a delete-time sweep of bound conversations.
- **Index hygiene (P1–P4).** Walk-time exclusion, filter-only purge, a
  content-hash manifest, and concept stability across re-indexes.
- **Memory subsystem (M-series).** Tier-7 fit guarantee, pressure-triggered
  consolidation, reflection dedup + recency-touch damping, slot-liveness net,
  server-side query embedding for graph queries, and retirement of the old
  harness RAG subsystem.
- **Accessibility / theme.** Text tokens lifted toward white (ladder preserved),
  non-colour cues for placeholder / disabled / ignored states, and real
  Lucide + Seti file-tree icons replacing the CC0 placeholders.
- **Dev environment.** Fast dev rebuild loop, theme hot-reload, desktop
  shortcuts, and a dev-only hot-reload path (`dev.restart_service` verb +
  backend watcher).

### Security

- **Every third-party GitHub Action is now pinned to a commit SHA, and a CI gate
  keeps it that way (closes #127).** Every `uses:` across the seven workflows was
  pinned to a *mutable major tag* (`actions/checkout@v7`, `dependabot/fetch-metadata@v3`,
  etc.) — a tag its upstream owner can repoint at any commit. The sharpest instance
  is `dependabot-automerge.yml`, the one workflow holding write scopes (`contents:
  write` + `pull-requests: write`) with a live `GITHUB_TOKEN`: there, a repointed
  `fetch-metadata` tag is direct repo write access with no code review in the path.
  All five distinct actions (`actions/checkout`, `Swatinem/rust-cache`,
  `dependabot/fetch-metadata`, `EmbarkStudios/cargo-deny-action`, `actions/setup-python`)
  are now pinned to the full 40-hex SHA their tag resolved to, with the tag kept as
  a trailing `# vN` comment so Dependabot's github-actions ecosystem still bumps them.
  A new `actions pinned to SHA` CI gate (`tools/check-actions-pinned.sh`, pure
  bash/grep with a `--selftest`) turns **red** with the tag→SHA fix command if any
  action is tag-pinned; both rulesets require its context.

- **The GUI voice preset lists can no longer silently drift from the service's
  accepted values (closes #129).** Four voice value-domain lists (PTT hotkeys,
  STT backends, VAD sensitivities, wake-word models) are duplicated across the
  `Core/GUI` ↔ `rust/` cargo-workspace boundary — the GUI can't `use`
  `wylde-voice` because its audio stack (cpal) segfaults in the headless
  panel-walk gate — and until now only a `/// Mirrors` doc comment held them in
  sync. A drifted list offers a picker value the service validator
  (`wylde-voice` `session.rs`) rejects, so the user selects a legal-looking
  option and the `voice.set_config` silently fails. New
  `tools/check-voice-presets-mirror.py` asserts each GUI list is **equal**
  (value and order) to the `config_persist` const its `/// Mirrors` comment
  names — deriving the pairing from that existing comment rather than a second
  hand-kept map — and runs as the required `voice presets mirror` CI gate.
  `VoiceSettings.mode` (`ALL_MODES`) is documented as deliberately unmirrored
  (a toggle, not a cycle-list). Appending or reordering a GUI list now turns the
  gate red and names both sides.

- **The cargo-deny advisory + license gates now cover every gated Cargo
  workspace, driven by one discovered list (closes #122).** The repo has four
  gated `[workspace]` roots (`rust/`, `Core/GUI/`, `tools/xtask`,
  `tools/wylde-release`) plus the deliberately-excluded `voice-npu-spike` spike,
  but three independent hand-kept lists — the two `cargo-deny` matrices and the
  G7 version check — each enumerated only the first *two*. So the two shipped
  release tools got **no vulnerability scan and no GPLv3 license scan**, and
  carried their own versions unchecked: a vulnerable or copyleft-incompatible
  dependency, or a version split, could land in a release tool with all required
  checks green. New `tools/list-workspaces.sh` discovers the workspace roots from
  the tree (with a documented exclusion list); `tools/check-versions.sh` now
  derives its set from it, the cargo-deny matrices cover all four (the two
  `tools/` workspaces share one `tools/deny.toml`, resolved by walking up), and
  both rulesets require the new `cargo-deny (advisories|licenses)
  (tools/xtask|tools/wylde-release/Cargo.toml)` contexts. A new `manifest
  coverage` CI gate (`tools/check-manifest-coverage.sh`) turns **red** with an
  actionable message if any of those enumerations drifts from the discovered set,
  so a forgotten edit fails loudly instead of shipping silently. `cargo deny
  check advisories`/`licenses` → `ok` on all four workspaces today.

- **CI workflows now declare a least-privilege `GITHUB_TOKEN` scope (CodeQL
  `actions/missing-workflow-permissions`, 9 alerts).** `ci.yml`,
  `license-check.yml`, and `security-audit.yml` had no explicit `permissions:`
  block, so their jobs inherited the repository-default token scope — broader
  than any of them use. Every job across the three only checks out, caches,
  builds/tests, or runs `cargo-deny`; none comment on PRs, upload SARIF, or push.
  Added a top-level `permissions: { contents: read }` to each (applies to all
  jobs), clearing the CodeQL hardening finding with no behavioural change.

- **`cargo-deny (advisories)` is now a blocking gate, not advisory-in-name-only
  (G5; closes #49).** The security-audit workflow's `pull_request` path filter was
  removed so both matrix legs — `cargo-deny (advisories) (rust/Cargo.toml)` and
  `… (Core/GUI/Cargo.toml)` — run on *every* PR (like `ci.yml`), and both contexts
  were added to the required-check list on the `protect-develop` and `protect-main`
  rulesets. Previously the check ran only when `Cargo.*`/`deny.toml` changed and was
  absent from the required set, so a PR that introduced a new advisory was still
  mergeable. Making a path-filtered check *required* would have silently blocked every
  Cargo-untouching PR forever (GitHub waits for a status the skipped workflow never
  reports); running it unconditionally is what makes it safe to require.

- **GPLv3 license compliance is now an enforced CI gate, not a norm.** Wylde Core
  is `GPL-3.0-or-later`; copyleft *inherits*, so every dependency compiled or linked
  into a Wylde binary must carry a GPLv3-**compatible** license — a single
  incompatible dep (SSPL/BUSL/CDDL/EPL, GPL-2.0-only, the historical OpenSSL license,
  or an unlicensed crate) is a real legal defect. Both `deny.toml` files already
  *defined* `[licenses]`, but CI only ran `check advisories`, so nothing enforced it.
  New `.github/workflows/license-check.yml` runs `cargo deny check licenses` on both
  workspaces, **unfiltered on every PR** (same path-filter-free mechanism as the
  advisory gate, so the `cargo-deny (licenses)` legs can be *required* without hanging
  Cargo-untouching PRs). The allow-list was rewritten as a real, FSF-matrix-vetted
  GPLv3-compatibility policy — including the fix that it previously allowed deprecated
  `GPL-3.0` but **not** `GPL-3.0-or-later` (the project's own license), so the gate
  would have rejected every first-party crate; `OpenSSL` was removed from the GUI list
  as FSF-incompatible with GPL and absent from the tree. **No GPL-incompatible
  dependency exists in either tree today** (`cargo deny check licenses` → `licenses ok`
  on both). Making the legs *required* is a one-line ruleset addition, to land the same
  way #49 added its advisory contexts to the `protect-develop`/`protect-main` rulesets.
- **Formally accepted the two unbumpable, gpui-pinned advisories in `deny.toml`
  with a documented review trigger (closes #30 / KI-3).** Both ride behind the
  pinned `gpui` git rev (`b3d93d44`), which Dependabot cannot bump: `glib` 0.18.5
  `VariantStrIter` unsoundness (RUSTSEC-2024-0429 / GHSA-wrw7-89jp-8q8g) — a
  GTK3-only transitive, `cfg(linux)`-gated and absent from the shipped Windows
  binary; and `async-tar` 0.5.1 PAX entry-smuggling (GHSA-35rm-7j9c-2f7m /
  CVE-2026-53600) — compiled but dormant (no untrusted-tar path; the self-updater
  is the separate, minisign-verified `wylde-updater`). The `glib` acceptance is
  recorded as an ignore in `Core/GUI/deny.toml`; `async-tar` still has no RUSTSEC
  id (re-verified 2026-07-15), so cargo-deny cannot ignore it and Dependabot
  remains its gate — its disposition is documented there in a comment. The real
  review trigger for both is the next deliberate `gpui`-rev bump (with a
  2026-10-14 quarterly backstop, adjustable); policy in
  `docs/security/dependency-hygiene-policy.md`. `cargo deny check advisories`
  passes green on both the `rust/` and `Core/GUI/` workspaces.

- **Dependency advisory sweep (RustSec / GitHub Dependabot).** Bumped two
  transitive crates to their patched releases across the affected lockfiles:
  `quinn-proto` 0.11.14 → 0.11.15 (RUSTSEC-2026-0185, HIGH — remote memory
  exhaustion from unbounded out-of-order QUIC stream reassembly; pulled via
  `reqwest`/`quinn` in the `rust/`, `Core/GUI/`, and `Services/wylde-images/`
  workspaces) and `memmap2` 0.9.10 → 0.9.11 (RUSTSEC-2026-0186, unsound —
  unchecked pointer offset; `Core/GUI/`). Lockfile-only patch bumps; no manifest
  or API changes. Remaining advisories are RustSec *unmaintained* notices with no
  patched release (`async-std`, the GTK3 `gtk`/`gdk`/`atk` binding family,
  `glib` unsoundness, `paste`, `instant`, `backoff`, `bincode`, `fxhash`,
  `proc-macro-error`/`proc-macro-error2`, `rustls-pemfile`); these are transitive
  and deferred — clearing them needs upstream/major migrations, not a bump.

- **GitHub Dependabot alert triage (5 open).** Reviewed and dispositioned the
  five open Dependabot alerts on the default branch. The three HIGH `pip`
  alerts — `transformers` remote code execution (CVE-2026-4372) and `soupsieve`
  ReDoS + memory-exhaustion (CVE-2026-49477 / CVE-2026-49476) — are all against
  `uv.lock`, the Python lockfile deleted in the R6 full-Rust cutover
  (`2f5aa82`). Those packages have no importer left in-tree (`pyproject.toml`
  now declares `dependencies = []`; the surviving Python is stdlib-only dev
  tooling), so the vulnerable code is not present and the alerts are stale
  against a removed manifest — dismissed as *vulnerable code not used*. The two
  remaining Moderate Rust alerts are transitive and upstream-pinned, with no
  clean bump: `glib` `VariantStrIter` unsoundness (GHSA-wrw7-89jp-8q8g) is
  pulled only through the GTK3 binding family (`gtk`/`gdk`/`atk` ← `wry` /
  `tray-icon`), which is `cfg(linux)`-gated and **not compiled into the shipped
  Windows build** (confirmed absent from the `x86_64-pc-windows-msvc` dependency
  graph); and `async-tar` PAX-header desync / entry-smuggling (CVE-2026-53600,
  patched 0.6.1) is required at `^0.5.1` by Zed's `http_client` (pinned `gpui`
  git rev `b3d93d44`) and Wylde exercises no untrusted-tarball extraction path
  through it. `cargo update` rejects both across the 0.x major boundary; forcing
  them needs a `gpui`-rev bump or a full gtk-rs major migration and is deferred.
  Full reachability write-up in `docs/security/dependabot-triage-2026-07-11.md`.

### Fixed

- **The reasoning planner and the executor spoke different tool vocabularies, so no plan step
  ever realised.** The planner's catalog (`reasoning::inputs::render_tool_catalog`) filtered only
  on `status == "active"` and rendered *every* active tool. But the verb-tool cutover
  (`WYLDE_HARNESS_VERB_TOOLS`, default **ON** since 2026-06-03) means the executor's turn
  advertises only the eight `wylde_*` verbs plus a small surviving tail. So the planner proposed
  `read_file`, the executor could only dispatch `wylde_get`, and `PlanState::finish_round` — which
  binds a step's result **only** to a dispatch of that step's own tool, matched by exact name —
  never fired. Steps never realised, `expected` / `on_surprise` / the whole surprise machinery
  never evaluated, and the tier was decorative on exactly the multi-step tasks it exists for.
  (issue #25 / KI-1; reasoning-v2 Slice B.)
  - **One filter, not two.** `turn::prompt::advertise` is now `pub(crate)` and is *the* definition
    of "what the model may name"; the planner applies it (and `MAX_CATALOG_TOOLS`) against the
    live `verb_mode`, off the same `catalog_payload`. Two catalogs was the bug. The struct's own
    doc already described `tool_catalog` as "verb — description" lines — the intent was always the
    advertised surface; the code had drifted from it.
  - **The planner is now told the legal `resource_type` values** (`PlanInputs::resource_catalog`,
    rendered into PLAN *and* REPLAN). Without this the fix would only have traded one failure for
    another: the executor discovers resource types by calling `wylde_describe` at runtime — the
    verb guidance says outright they are "NOT in this prompt" — but PLAN is a single call that must
    emit a complete DAG, so it would have named `wylde_get` correctly and then invented the
    `resource_type`. Uses the same no-arg `wylde_describe` payload (`summary_rows`), one compact
    line each; empty (and omitted) in legacy mode.
  - **The invariant is now pinned by a test that fails on the old code** —
    `planner_never_names_a_tool_the_executor_wont_advertise`, asserted in **both** modes against a
    real registry. It compares in the right identifier space: the executor advertises `name`
    (often dotted, e.g. `ollama.auto_evict_lru`), the model emits that, and `actions.rs` resolves
    it through `alias_map` to the canonical id before `round_results` — *"the plan stores canonical
    ids; the model emits dotted/aliased names"*. So a plan step's tool must be a canonical id some
    advertised name resolves to. Reverting the fix reproduces the exact failure.

- **The GUI error sink could silently lose the error it just told you it recorded.**
  `routes::dev::append_line` — the `POST /api/dev/gui_error` handler backing
  `logs/gui_errors.jsonl` — called `write_all()` on a `tokio::fs::File` and never flushed. Tokio
  buffers the write and hands it to a background blocking task; it does **not** guarantee a flush
  when the handle drops, and discards any drop-time error. So `write_all().await` returned
  `Ok(())`, the route answered `{"recorded": true}`, and the record could still never reach disk.
  **A silently-dropped error report is the worst failure mode an error sink has** — the one thing
  it exists to do, failing in the one way nobody notices. Now flushed explicitly.
  - Found via a **~3% flake** in `records_a_well_formed_event` (the file was created but empty).
    The test was right and the route was wrong — worth stating, because the tempting read of a
    rare red on an unrelated PR is "flaky test, re-run it".

- **A test race in `wylde-gateway`'s egress registry — two mutexes guarding one resource.**
  `egress::destinations`' tests took a private `REGISTRY_LOCK` while `egress::client`'s and
  `pipe`'s tests took `EGRESS_TEST_LOCK` — **for the same process-global destination registry**.
  Two different mutexes over one shared resource provide *no* mutual exclusion: each module was
  internally serialised and entirely unsynchronised against the other, so a `reload` in one wiped
  the registry out from under a request in the other. It surfaced as `forward_ssrf_blocks_private`
  / `forward_ssrf_blocks_metadata` failing with
  `Denied("caller \"Caller\" declares no egress destinations")` instead of the `Ssrf` they assert —
  an SSRF test failing for a reason that had nothing to do with SSRF.
  - The registry-touching `destinations` tests now take the same `EGRESS_TEST_LOCK` (and are
    `#[tokio::test]` accordingly); the pure parsing tests stay sync and lock-free.
  - **The measurement that proved it:** `egress::client` alone was **0 failures in 20 runs**, but
    `egress::` (client + destinations together) was **4 in 20**. That gap is the whole diagnosis —
    a race between modules, not within one. After the fix: **0 in 25** together, and **0 in 40**
    across the full `wylde-gateway --lib`.

- **A flaky env race in `wylde-extension-bridge`'s tests that red-walled CI on PRs touching no
  Rust at all.** `mcp::client::tests` mutated the process-global `WYLDE_BIN` / `WYLDE_ROOT` while
  `cargo test` ran them in **parallel threads**: `cwd_wylde_root_token_resolves_to_real_root` set
  `WYLDE_ROOT=/the/real/root` while `wylde_bin_token_falls_back_to_release_dir` was mid-assert
  against `/repo`, so the latter failed on a value it never set. Reproduced at **~8% (2 failures in
  25 local runs)**; **0 in 40** after the fix. Caught because it failed the `backend (rust/) build +
  test` required check on a **docs-and-ruleset-only PR** — an ~8% flake in a required check is a
  random tax on every PR and trains people to hit re-run instead of reading the failure.
  - Fixed with **`#[serial]`** (serial_test) on every env-mutating test in the module — the guard
    the rest of the tree already uses (`wylde-shared`, `wylde-harness`, `wylde-concept-routing`,
    `wylde-concept-hierarchy`).
  - The tests carried a comment asserting `// SAFETY: single-threaded test`. That was **false** —
    cargo is multi-threaded by default — and the wrong premise is precisely what let the race in.
    Removed rather than corrected in place, and replaced with a note that any new `set_var` /
    `remove_var` test here must be `#[serial]`.
  - Same shape as the `wylde-lifecycle` env-isolation bug already tracked in `known-issues.md`
    KI-6: **a test that pins one variable but depends on two.** KI-6 now records this one as the
    second confirmed instance, plus the method — enumerate the remaining failures with a repeat
    loop, since a single green run proves nothing about a race.

- **`docs/wylde-repo-organization.md` no longer tells you the repo isn't a repo.** The stale-vault-path
  scrub (#31) turned up one reference worse than a dead path: a doc marked `status: living reference`
  whose §1 stated the tree lived at `%USERPROFILE%\Documents\Obsidian Vault\Wylde\`, had no `.git/`,
  would make `git status` "refuse", and that version history was therefore implicit in progress-memory
  files with every file "authoritative current state". The tree is under git with `develop` as trunk —
  so a living reference was actively instructing readers to distrust git. §1 now describes the real
  git layout, and §11's auto-memory path derives its slug from wherever the repo lives instead of
  hardcoding the vault one. Paths are repo-relative on purpose, so they don't rot the same way twice.
  `WYLDE_ENDPOINTS.md:504` (`cwd=vault root` → `cwd=repo root`) scrubbed too.
  - **`docs/security/pre-alpha-release-2026-05-31.md` deliberately keeps its vault paths.** It is a
    dated log of actions actually taken; rewriting it would falsify the record. It gets a header note
    (paths as-of that date, those locations are gone, don't navigate by it) instead of a scrub. Same
    call for `docs/mypy_baseline.txt`, whose vault paths sit inside captured tool *stdout* — it's a
    Python-era artifact due for deletion with the Python scrub (T1.2), which is where that call belongs.

- **`preflight --launch` can now produce a launch-verified receipt — the gate no
  longer collides with its own running stack.** The launch checks shell out to
  `cargo`, but a Wylde crate can't be (re)built while its binary is running, so
  the gate structurally contradicted itself and `all_green`/`launch_verified`
  could never both be true — blocking `publish` (which refuses a non-launch-verified
  receipt) and the 0.2 preflight. Two complementary fixes:
  - **`wylde-prebuild-guard` now blocks only on the crate's *own* exe.** The
    guard's job is one question — "will the linker fail to overwrite the target
    `.exe`?" — and building crate `X` only ever relinks `X.exe`. It previously
    blocked on *any* live `wylde-*.exe`, so a running `wylde-release.exe` (the
    preflight tool itself — a standalone crate that isn't even a member of the
    `rust/` workspace, and which no build overwrites) false-positived and
    aborted the reasoning benchmark. Both the live-process and runtime-manifest
    signals are now narrowed to `<current_crate>.exe` before classification.
  - **`--launch` now builds the release artifacts up front and cold-starts the
    *release* stack.** `--launch` implies the L1 release build (a launch-verified
    receipt must certify what actually ships), then pre-builds the exact
    functional-check binaries (`reasoning_eval` example, `integration_rag_indexer`
    + `embed_live` test bins) while the stack is still down. The running services
    then live in `target/release/` while the debug/test-profile functional checks
    write `target/debug/` — disjoint paths, so the Windows exe file-lock that
    failed L3.8 (`Access is denied (os error 5)` relinking a running
    `wylde-harness.exe`) can no longer occur, and L2/L3 run only pre-built
    binaries. (fixes #47)
- **De-flaked the `wylde-workspaces` gather-prompt breaker integration test
  (a CI-red-training flake).** `gather_prompt_degrades_then_trips_breaker_when_service_dies`
  intermittently failed on PRs with no `rust/` changes, then passed on re-run.
  Root cause was **not** timing or test ordering: the file's two
  `#[tokio::test]`s run concurrently in one process and each minted its service
  (pipe) name from `pid + timestamp`. The pid is identical and the timestamp
  can resolve to the same tick when both tests start together, so the names
  could **collide** (measured ~0.009% for two simultaneous threads on an idle
  box — higher on a loaded runner). Because the IPC server binds pipes without
  `first_pipe_instance`, two services on one name coexist and **share** the
  pipe; the negative test then kills *its* service but its post-kill
  `gather_prompt` calls reach the still-alive positive-test service, succeed,
  and the circuit breaker never accrues the 5 failures the test asserts. Proven
  deterministically with a forced-collision repro (all 5 post-kill calls
  returned `Ok`, breaker never tripped). Fix: the integration tests now mint
  collision-proof names with a random `uuid` suffix, matching the convention
  already used by `integration_rag_indexer.rs` (why `uuid` is a dev-dependency).
  Applied to the four sibling integration tests sharing the same latent pattern
  (`verbs_roundtrip`, `pipe_roundtrip`, `fs_verbs`, `anchors`). (fixes #29)
- **Long-term memories saved outside the model now embed on write.** The
  `memory.long_term.save` / `memory.long_term.update` API/pipe handlers (the
  Settings-UI "add memory" path, extensions, N8N — anything that isn't the
  model tool) passed `None` for the vector and never read a caller-supplied
  one, so the record landed in `long_term.json` with no entry in
  `long_term.vec.bin`. Semantic search (`memory_search`, the per-turn gather
  long-term retrieval) therefore couldn't rank it — only the text fallback
  could — silently defeating cross-conversation recall for UI-curated
  memories. Both handlers now auto-embed the body (budgeted, fail-soft) when
  no vector is supplied, mirroring the model-tool and workspace-save paths;
  update re-embeds the effective new body. Verified live: a memory saved via
  the pipe verb now returns as the top semantic hit for a paraphrased query.
  (fixes #43)
- **Short-term memory store now honours encryption-at-rest (OI-14).** It
  used plain file IO on the same conversation documents the conversations
  store reads/writes encrypted; a lazy-migration read could flip a document
  to ciphertext mid-flow, after which the short-term store's plain reads
  saw an unreadable file and silently minted a stub over live data (losing
  the workspace binding and the working-memory list). Both stores now route
  through the same `wylde_shared::encryption` read/write path.
- **Re-index no longer exhausts the OS ephemeral-port pool.** Bolt
  connections are pooled and embed requests rate-capped, and graph
  upsert/relate calls are batched with their own timeout, so whole-repo
  indexing stops crashing the runner.
- **Dev stage deploy-gap.** `wylde-dev.ps1` re-seeds a stale `target-dev/stage`
  from the freshest build (fail-soft when the binary is locked), so newly-added
  verbs stop `no_action`-ing in dev.
- **Slow pipe verbs.** A `call_with_deadline` path gives long-running verbs
  (re-index, graph) a generous deadline instead of timing out.
- **GUI responsiveness.** Surfaced previously-swallowed failures across the
  memory / devices / images / shell / chat surfaces, made graph degrade-retry
  clickable, and wired the vocab undo chord. Shaped-text `TextInput` with real
  glyph metrics (in-input wavy underline).
- **Lifecycle / ollama robustness.** Memgraph/Neo4j spawn anchored to an absolute
  root; a staleness guard flags running services on a rebuilt binary; implicit
  `:latest` tags resolve in ollama `model_matches`; the Start-Ollama button now
  starts the upstream daemon; service-down is distinguished from out-of-date in
  `no_action`.

## [0.1.0-alpha.1] — 2026-06-04

First tagged alpha. Published as a GitHub **pre-release** (beta channel).

### Added

- **gpui-native desktop app.** The full UI was rebuilt on
  [gpui](https://github.com/zed-industries/zed/tree/main/crates/gpui),
  retiring the earlier Tauri + Svelte alpha. All panels (Chat, Models, Memory,
  Dashboard, Devices, Workspaces, Tools, Settings, Images, RemoteAccess) talk
  to the in-process Rust harness over named pipes — no web stack, no embedded
  browser.
- **On-device voice, in-process.** STT (Whisper) and TTS (Kokoro) run directly
  in the orchestrator (ONNX); the Python voice service was deleted. Settings
  gains a Voice section (input-device selection, mic test) and a live
  push-to-talk hotkey.
- **In-app self-updater.** Opt-in updates from this repo's GitHub Releases,
  verified against one embedded minisign/Ed25519 public key and fail-closed (an
  unsigned or mis-signed binary is never installed). Stable / Beta channels, a
  manual "Check now", and an optional background check on a chosen cadence. No
  telemetry; the only outbound call is an unauthenticated GitHub REST GET.
- **Per-user installer.** A no-UAC NSIS installer (`WyldeSetup`) that installs
  to `%LOCALAPPDATA%\Programs\Wylde`, with daemon-first Start-menu / desktop
  shortcuts and optional sign-in autostart.
- **Conversation switching.** Per-conversation working memory with a switcher
  UI and a cross-panel nav-bus, so the Memory panel mirrors the active chat's
  buffer; `conversations.*` and `memory.short_term.*` ported to Rust.

### Release assets

- `wylde-gui-x86_64-pc-windows-msvc.exe` (+ `.minisig`) — bare signed GUI
  binary consumed by the self-updater.
- `WyldeSetup-0.1.0-alpha.1.exe` (+ `.minisig`) — per-user installer.

Both signed with the production minisign key (ID `DA7E13F4E9F2ACB6`).

[0.2.0-beta.1]: https://github.com/PeopleWonder/wylde/releases/tag/v0.2.0-beta.1
[0.1.0-alpha.1]: https://github.com/PeopleWonder/wylde/releases/tag/v0.1.0-alpha.1

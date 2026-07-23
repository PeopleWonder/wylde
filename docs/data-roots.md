# Where Wylde keeps your data

One table, one root. Every persistent store below resolves through
`wylde_shared::paths::data_dir()` — **convention A**:

```
WYLDE_DATA_DIR  →  DATA_DIR  →  <WYLDE_ROOT>/.wylde/data
```

`WYLDE_ROOT` is the estate root `launch_wylde.ps1` exports, and the working
directory lifecycle spawns every service with (`state/services.rs`,
`cmd.current_dir(wylde_root())`). When `WYLDE_ROOT` is unset the resolver falls
back to the process cwd — the historical behaviour, and the reason a
mis-launched service resolves a cwd-relative data dir (#138 H6).

There is exactly one resolver, in `rust/crates/wylde-shared/src/paths.rs`. A
required backend test (`wylde-shared/tests/single_data_dir_resolver.rs`) walks
every crate's `src/` and fails if a second `fn data_dir` appears anywhere, if a
store root is built from a bare relative `"data"`, or if one of the migrated
stores stops referencing the canonical resolver.

## The stores

`<data>` below is whatever convention A resolved to. Every legacy path is
**read-only to Wylde**: the migration copies out of it and never writes to,
renames, or deletes it, so a downgrade to a pre-#250 build still finds its data.

| Store | Canonical path | Env override | Legacy path (pre-#250) |
|---|---|---|---|
| Encryption prefs, graph profiles, `settings/*.json`, memory tiers, workspace registry, conversations, `voice_config.json`, `scheduler_state.json` | `<data>/…` | `WYLDE_DATA_DIR`, `DATA_DIR` | — (always convention A) |
| Starred default model | `<data>/default_model.json` | `DEFAULT_MODEL_PATH`, then `DATA_DIR` | `<WYLDE_ROOT>/data/default_model.json` |
| Active model (inference bar) | `<data>/active_model.json` | `ACTIVE_MODEL_PATH`, then `DATA_DIR` | `<WYLDE_ROOT>/data/active_model.json` |
| Routing profiles / model registry | `<data>/model_registry/` | `MODEL_DATA_DIR` | `<WYLDE_ROOT>/data/model_registry/` |
| Per-model Ollama overrides | `<data>/settings/ollama/` | `WYLDE_DATA_DIR`, `DATA_DIR` | `<WYLDE_ROOT>/data/settings/ollama/` |
| Gateway's old flat Ollama file (import source only) | — | — | `<WYLDE_ROOT>/data/settings/ollama.json` |
| Device gate — paired devices, credentials, audit log | `<data>/device_gate/` | `DEVICE_GATE_DATA_DIR`, `DEVICE_GATE_HTPASSWD` | `<WYLDE_ROOT>/device_gate/data/` |

The env overrides are **test seams and operator escape hatches**, not legacy
compatibility. They still win outright, ahead of both the canonical fallback and
the migration; several suites depend on them.

## The migration

`wylde_shared::data_migration` adopts a legacy location on first touch of the
store. Its contract, exercised by unit tests in that module and by a
legacy-only-data test per store:

- **One-way.** Legacy → canonical. Bytes are *copied*, never moved.
- **Never clobbers.** Runs only when the canonical location does not exist (for
  a tree: does not exist, or exists but is empty — services routinely
  `create_dir_all` their root before their first write). Anything written since
  the move is newer by construction and always wins.
- **Idempotent.** The first successful copy creates the canonical location, so
  every later call no-ops. Running it on every path resolution — which is what
  the stores do, because a `OnceLock` latch would be wrong under env rebinding —
  costs one `exists()` stat.
- **Fail-soft.** A copy that fails leaves the legacy bytes untouched and the
  canonical location absent; the next resolution retries.

### Why it matters

Moving a resolver without moving the bytes is silent. There is no error: the
store reads an empty directory and reports "nothing configured". To a user that
is *Wylde forgot my settings* — the starred default gone, per-model inference
overrides reset, routing profiles cleared and every benchmark re-run from
scratch, every paired phone unpaired. That risk is why #138 deferred this and
why #250 is a migration rather than a find-and-replace.

## Update survival

None of these paths sits inside `%LOCALAPPDATA%\Wylde\versions\<v>\`, the tree
`wylde_updater::install_stack` stages and prunes. They hang off the estate root,
which an update never writes. `wylde-harness/tests/default_model_survives_update.rs`
(#243) holds that line: it asserts the store resolves *outside* the replaced
tree, and stays deliberately root-agnostic so it checks the updater's blast
radius rather than duplicating the convention-A gate.

## Not yet unified

Three stores still root at the legacy sibling `<WYLDE_ROOT>/data`. They are
**out of #250's scope by its own terms** — none appears in its table or
acceptance criteria — and neither of its two hazards applies to them: each is
already anchored on `WYLDE_ROOT` rather than the cwd, so no location depends on
the working directory.

| Store | Path | Defined in |
|---|---|---|
| Service manifests (runtime, regenerated at startup) | `<WYLDE_ROOT>/data/manifests/` | `wylde-shared/src/manifest.rs` |
| Tool-consent preferences | `<WYLDE_ROOT>/data/preferences/consent.json` | `wylde-harness/src/tooling/consent.rs` |
| VRAM-broker state | `<WYLDE_ROOT>/data/state/vram-broker.json` | `wylde-vram-broker/src/config.rs` |

They remain a real second backup/restore target, so folding them in is worth a
follow-up issue — the migration helper and the gate are both already in place
for it. Until then, "back up Wylde" means `<WYLDE_ROOT>/.wylde/data` **and**
`<WYLDE_ROOT>/data`.

## References

#138 (the original unification) · #250 (this one — the four deferred resolvers)
· #243 / #244 (update survival) · #235 (the persistent default)

# Out-of-tree runtime model (foundation)

Status: foundation shipped (2026-06-17). This is the durable in-repo record
of the single-root, git-ignored-bucket runtime model. The buildable plan it
realizes lives (untracked) at `outputs/wylde-out-of-tree-runtime-plan.md`.

> Scope: this is the **foundation only** — the buckets, discovery,
> supervision, build orchestration, and data-path contract. The Images
> service extraction and the WyldeStudy extension build ride on top of it
> later; neither is done here.
>
> **Update (2026-06-17):** the Images extraction is now **done** — it is
> the first inhabitant of the `Services/` bucket. `wylde-images` is its own
> GPL-3.0-or-later repo under `Services/wylde-images/`, discovered/spawned
> by this foundation with **zero new Core code**, and surfaced as a
> loopback iframe (no compiled-in panel). See
> [`docs/services/wylde-images.md`](services/wylde-images.md) — the worked
> example for the Services tier. WyldeStudy is still pending.

## 1. The single-root, git-ignored buckets

`Wylde-release/` is `<root>` (== `WYLDE_ROOT`). It contains `Core/` plus
three **buckets**, all git-ignored so Core's tracked tree stays just-Core
and the buckets **ship empty**:

```
Wylde-release/            <root> == WYLDE_ROOT  (the Core-owning repo)
├── Core/                 Core itself (+ Core/GUI/ gpui workspace)
├── rust/                 Core's backend Cargo workspace
├── Services/    [ignored] sibling full-tier service repos   (lifecycle-supervised)
├── Extensions/  [ignored] leave-the-ecosystem extensions    (extension-bridge)
└── Core/Plugins/[ignored] compiled-in plugin crates         (opt-in, built into Core)
```

- **Container-of-repos.** A bucket is a plain directory; each *item* inside
  keeps its own independent `.git`. Core ignores the bucket contents
  (`.gitignore`: `/Services/`, `/Extensions/`, `/Core/Plugins/`).
- **Ships empty.** A fresh Core checkout has no bucket contents; git does
  not track empty dirs, so the buckets may even be absent. Everything
  no-ops cleanly when they are (the removability contract, §6).
- **Plugins are different.** `Services/` and `Extensions/` items are
  *runtime-discovered*; `Core/Plugins/` items are *compiled into Core* and
  so are opt-in at build time (the four-step wiring in
  `rust/crates/wylde-harness/src/plugins/mod.rs`). The bucket ships empty —
  `installed()` returns `[]`, Core builds plugin-free — and `hello_wylde`
  stays on disk under the ignored bucket as the worked example. See
  `docs/plugins.md`.

## 2. Two discovery walks

- **Lifecycle registry** (`rust/crates/wylde-lifecycle/src/registry.rs`):
  the flat top-level walk plus a one-level descent into the
  `SERVICE_BUCKETS` (`["Services"]`) — `list_bucket_folders` reads each
  `Services/<name>/manifest.json` exactly like an in-tree folder.
  `discovered_bucket_services[_in]` returns the canonical
  `DiscoveredService { name, folder, enabled }` rows the daemon supervises.
  Absent/empty bucket ⇒ empty `Vec` (clean no-op).
- **Extension bridge** (`rust/crates/wylde-extension-bridge/`): already
  walks `Extensions/*` for `mcp-server.json`. Unchanged by this work; the
  `Extensions/` bucket is simply git-ignored now.

## 3. Dynamic supervision (launch AND shutdown)

Nothing about the buckets is hardcoded — the daemon supervises whatever
discovery returns, symmetrically on the way up and down.

- **Boot** (`daemon.rs`): after the core tier spawns, a loop starts every
  *enabled* discovered sibling via `start_discovered`. Non-fatal per item;
  skipped under no-spawn (parity).
- **Shutdown** (`state/mod.rs::stop_all_daemon_managed`): a symmetric loop
  drains discovered siblings first (they are leaf consumers of the core
  tier) via `stop_discovered`, before the core teardown.
- **`start_discovered`** (`state/services.rs`): a thin generalization of
  `start_strangler` — already-alive guard → resolve the binary *beside the
  manifest* (`sibling_binary_path`: `WYLDE_<NAME>_BIN` override, then
  `Services/<name>/<bin>.exe`, then the repo's own `target/`) →
  `spawn_rust_binary` (verbatim env + `kill_on_drop`) → record + track. A
  missing binary is non-fatal.
- **Accept-list is discovery-driven.** The old fixed
  `DAEMON_MANAGED_SERVICES` array is now `CORE_SERVICES` (just the core
  subset). `service.start` / `.stop` / `.wake` gate on `is_manageable(name)`
  = core OR discovered sibling, and `dispatch_start`/`dispatch_stop` have a
  generic arm routing a discovered name to `start_discovered` /
  `stop_discovered`. Drop a service into `Services/` and it is accepted —
  no code edit. (The no-spawn parity surface stays on `CORE_SERVICES`.)

## 4. Build orchestration — `cargo xtask build-all`

All-Rust multi-workspace builder (`tools/xtask/`, a standalone crate so its
`cargo` spawns dodge the spawn linter). `cargo xtask build-all` (alias in
`.cargo/config.toml`; thin `tools/build-all.ps1` wrapper):

1. builds Core's two workspaces (`rust/`, `Core/GUI/`);
2. for each populated bucket repo (`Services/*` / `Extensions/*` child with
   a `Cargo.toml`): `cargo build --release` in its own folder, then
   **stages the produced binary beside its `manifest.json`** — the exact
   drop location `sibling_binary_path` reads.

Build → drop → discover is the whole chain. Flags: `--debug`, `--skip-gui`,
`--buckets-only`, `--root`. Clean no-op when no buckets are populated.

**F1 tie (deploy-gap).** `service.list`'s staleness guard
(`binary_predates_process`) is now sibling-aware: `annotate_staleness`
resolves a sibling's beside-manifest binary, so a sibling whose staged
artifact post-dates its running process shows `stale:true` — the same
`stale:0` gate W0 uses for core services. `build-all` prints the bounce
reminder; the authoritative live `stale:0` assertion stays with the
redeploy/preflight step that queries a running daemon.

**Dev-stack stage refresh (deploy-gap, in-tree).** Two launch paths resolve
*in-tree* core binaries differently:

- **Live** (`launch_wylde.ps1`): the daemon reads `rust/target/release`
  directly (`rust_binary_path` order: `rust/bin` → `target/release` →
  `target/debug`). `build-all` rebuilds in-tree crates there, so Live is
  fresh after a build + bounce — there is no staging step for in-tree
  services, and nothing for `build-all` to stage into the launch dir.
- **Dev hot-reload** (`tools/dev/wylde-dev.ps1`): the daemon spawns each
  service from `rust/target-dev/stage/<svc>.exe` via `WYLDE_<NAME>_BIN`
  overrides, so the backend watcher can rebuild into `rust/target-dev/debug`
  without fighting the running `.exe`'s lock, then swap. The launcher
  **seeds** that stage dir. The original seed ran only when the stage file
  was *absent*, so a later `cargo build`/`build-all` updated `target/release`
  while the dev daemon kept spawning the stale staged binary — which
  `no_action`ed every verb minted after the stale build (the 2026-06-18
  `wylde-workspaces` fs/graph/vocabulary break). Fixed: `wylde-dev.ps1` now
  re-seeds a stage binary whenever the *freshest* source build is newer than
  the staged copy (`Copy-Item` preserves mtime, so it is idempotent), and
  leaves a newer watcher-built stage copy untouched. `build-all` is **not**
  the fix site: it is the release/Live build tool and deliberately never
  writes the dev-only `target-dev/` scratch tree.

## 5. Per-service user-data paths

User data never lives inside a service folder and must outlive the binary,
so the path is persisted in **Core** config, not the service
(`rust/crates/wylde-lifecycle/src/paths.rs`, cloned from `updater_prefs`):

- **Store**: `service_paths.json` (under `WYLDE_DATA_DIR` →
  `WYLDE_ROOT/.wylde/data/`), keyed by canonical service name →
  `{ data_dir }`. Atomic temp-write + rename; defaults on any read error.
- **Default**: `default_data_dir` = `<root-parent>/WyldeData/<svc>/` — a
  sibling of the Core repo, outside the tree, never inside `Services/<svc>/`.
- **Actions**: `paths.get` / `paths.set` (a `paths.set` with an empty
  `data_dir` clears the override → reverts to default). The GUI first-open
  picker writes via `paths.set`; if dismissed, the service falls back to the
  default so it is never blocked. (Flow only — no GUI here.)
- **Injection**: `spawn_rust_binary` injects `WYLDE_<SVC>_DATA_DIR`
  (`data_dir_env_name`, e.g. `WYLDE_IMAGES_DATA_DIR`) = `resolve_data_dir`
  on every child. A data-owning service reads that env var in place of any
  hardcoded path; a path change takes effect on the next bounce.

This contract is generic — any future data-owning service gets it for free;
the only per-service work is the one "read env-or-default" line (for Images,
the later extraction's edit to `routes/images.rs::library_dir`).

## 6. The removability contract

With **all three buckets absent/empty**, Core still builds and boots, with
no dangling pipe or `MissingFactory`:

- discovery returns nothing → the boot/shutdown loops iterate zero times;
- `Core/Plugins/` empty → `installed()` is `[]` → Core builds plugin-free,
  catalog carries no `plugin.*` tools;
- the registry/service walks are unchanged from a tree without the buckets.

Proven by the unit no-op tests (`absent_services_bucket_is_a_clean_noop`,
`empty_services_bucket_is_a_clean_noop`, …) and the acceptance test: move
`Services/`, `Extensions/`, `Core/Plugins/` aside, build `rust/`, run the
full lifecycle suite — green.

# Mypy baseline — M1 observability pass

> ⚠️ **ARCHIVED / STALE — describes the REMOVED Python runtime, NOT the current all-Rust stack. Kept for history.**
> This report enumerates per-file mypy type errors in Python source (`VPN/peers/pairing.py`, Flask handlers, `Core/shared/ipc.py`, etc.) that was **deleted in the full-Rust cutover (R6, commit `2f5aa82`, 2026-06-10)**. The only Python still in the repo is the dev-time `wylde_check` architecture linter under `Core/harness/dev/`; this mypy baseline does not describe the live (Rust) runtime. Kept for historical reference.
> *Banner added 2026-06-27 on branch `chore/structure-tidy` (structure-tidy pass).*

**Date:** 2026-05-16
**Mypy version:** 2.1.0 (compiled)
**Config:** `[tool.mypy]` in `pyproject.toml` (permissive — `ignore_missing_imports = true`, no `disallow_untyped_defs`, no `strict`).
**Raw output:** [`mypy_baseline.txt`](mypy_baseline.txt)

> **Post-M2-sweep addendum (2026-05-16):** the 222 `unused-ignore` errors were
> swept. Section 7 at the end of this file records the result; the rest of
> this document still reflects the pre-sweep state for historical context.

> The goal of M1 is observation only — see what mypy reports at permissive
> settings. No annotation work, no type-ignore additions, no fixes. M2-M6
> phases will act on the findings below.

---

## How the baseline was captured

Mypy was invoked in four parts because three service directories
(`Core/resource_monitor/`, `device_gate/`, `Extensions/extension_bridge/`)
contain a space in the directory name, and each has an `__init__.py`. Mypy
refuses to map a space-containing path to a Python package name and aborts
with `"<name>" contains __init__.py but is not a valid Python package name`.

| Part | Command (run from repo root unless noted)                                          |
|------|------------------------------------------------------------------------------------|
| 1    | `uv run mypy --explicit-package-bases Core Gateway Voice VPN Trainer N8N Extensions` |
| 2    | `cd "device_gate" && uv run mypy --explicit-package-bases .`                       |
| 3    | `cd "Core/resource_monitor" && uv run mypy --explicit-package-bases .`             |
| 4    | `cd "Extensions/extension_bridge" && uv run mypy --explicit-package-bases .`       |

The three problem dirs are listed in the `[tool.mypy]` exclude regex so
they do not interrupt the Part 1 walk. **M2 should decide** whether to
rename those dirs (drops the space, makes them addressable from any cwd),
delete the `__init__.py` files (makes them script-only, matches how
`device_gate` already imports — bare `from core import …`), or keep the
multi-part invocation pattern indefinitely.

---

## 1. Headline numbers

| Metric                                                  | Value           |
|---------------------------------------------------------|-----------------|
| Total errors                                            | **268**         |
| Total `note:` lines (mostly `annotation-unchecked`)     | 25              |
| Total source files checked                              | **390**         |
| Files with at least one error                           | **75**          |
| Average errors per affected file                        | ≈ 3.6           |
| Average errors per checked file                         | ≈ 0.69          |

**Reading:** errors are concentrated in a relatively small slice of the
codebase. 75 of 390 files (≈ 19 %) carry any error at all, and the
distribution is heavily skewed by a single category — see §2.

---

## 2. Error category breakdown

| Code                 | Count | One-line meaning                                                     |
|----------------------|-------|----------------------------------------------------------------------|
| `unused-ignore`      | **222** | A `# type: ignore[…]` comment is suppressing nothing.              |
| `assignment`         | 17    | RHS type does not match the declared/inferred LHS type.              |
| `arg-type`           | 11    | Argument type doesn't match the parameter's declared type.           |
| `return-value`       | 4     | Function returned a value with the wrong type (or one was unexpected). |
| `var-annotated`      | 3     | Mypy cannot infer a variable's type and asks for an explicit annotation. |
| `union-attr`         | 3     | Attribute access on a union member that doesn't have that attribute. |
| `attr-defined`       | 3     | Module/object accessed an attribute that doesn't exist on it.        |
| `no-redef`           | 2     | A name was rebound to a different definition.                        |
| `dict-item`          | 2     | A dict literal entry's value type clashes with the declared dict type. |
| `func-returns-value` | 1     | Code uses the return value of a function that always returns `None`. |
| `annotation-unchecked` (note) | 16 | Mypy skipped the body of an untyped function (informational).  |

### 2a. `unused-ignore` — 222 errors (~83 % of the baseline)

These come from `# type: ignore[…]` comments that don't suppress anything
under the current permissive config. Two likely reasons:

1. The ignore was added against an older mypy run with stricter settings,
   and at permissive settings there's no underlying error to suppress.
2. The library's stub coverage improved and the ignore is now stale.

These are **noise, not bugs.** They don't represent type errors in the
code — they're orphan suppressions that mypy reports because
`warn_unused_ignores = true` is in the config.

**Representative examples:**
- `Core/harness/turn.py:273` — `error: Unused "type: ignore" comment`
- `Core/shared/ipc.py:42` — same (13 instances in this one file)
- `Core/shared/discovery.py:50` — same (9 instances in this file)

**Concentration hotspots** (file → unused-ignore count, top 10):

| File                                              | Count |
|---------------------------------------------------|-------|
| `Core/harness/turn.py`                            | ~32   |
| `Core/harness/pipe.py`                            | ~26   |
| `Core/shared/ipc.py`                              | ~13   |
| `Core/Memgraph/ipc.py`                            | ~13   |
| `Core/shared/discovery.py`                        | ~9    |
| `Core/Lifecycle/control.py`                       | ~5    |
| `Core/Lifecycle/daemon_state.py`                  | ~5    |
| `Core/harness/tests/test_turn.py`                 | ~7    |
| `Core/harness/tests/test_workspaces.py`           | ~5    |
| `Voice/download_models.py`                        | ~4    |

### 2b. `assignment` — 17 errors

The largest "real" error category. Common pattern: a variable typed as
`Module` or a concrete class is initialized to `None`, then the real
binding happens at use time.

**Representative examples:**
- `Core/harness/memory/reflection.py:553` — `Incompatible types in
  assignment (expression has type "LongTermMemory", variable has type
  "WorkspaceMemory")`
- `Gateway/middleware/audit_log.py:47` — `Incompatible types in assignment
  (expression has type "TextIOWrapper[_WrappedBuffer]", variable has type
  "None")` (and then the dependent `attr-defined` at line 48: `"None"
  has no attribute "write"`)
- `Trainer/Caption/video.py:77` — `Incompatible default for parameter
  "mode" (default has type "None", parameter has type "str")` (implicit-
  Optional change in PEP 484 — recurs at lines 78, 79, 80).

### 2c. `arg-type` — 11 errors

Argument-type mismatches at call sites.

**Representative examples:**
- `VPN/peers/pairing.py:90` — `Incompatible return value type (got
  "tuple[dict[str, str], str, int]", expected "tuple[bytes | str, str,
  int]")` (also at 92 and 99 — three sister errors).
- `Core/harness/pipe.py:188` — `Argument "kind" to "list_models" has
  incompatible type "str"; expected "Literal['llm', 'stt', 'tts',
  'vision', 'embed'] | None"`
- `Trainer/Caption/video.py:180-183` — four calls passing `Optional`
  values into `extract_frames` whose params are non-Optional (downstream
  of the implicit-Optional defaults flagged above).

### 2d. `annotation-unchecked` (note) — 16 occurrences

These aren't errors — they're informational notes that mypy skipped
analysing the body of a function because the function isn't annotated.
At M6 (strict mode) these will all become real errors via
`--check-untyped-defs` / `--disallow-untyped-defs`.

Hotspots: `Core/Memgraph/graph_service.py` (5), `Core/harness/tests/
test_turn.py` (4), `Core/shared/ipc.py` + `Core/Memgraph/ipc.py` (1 each).

---

## 3. By-service breakdown

| Service                              | Errors | Source files checked | Notes |
|--------------------------------------|-------:|---------------------:|-------|
| **Core/** (excl. resource_monitor)   | 200    | (part of 364)        | Carries the bulk; almost entirely `unused-ignore` in `turn.py`, `pipe.py`, `ipc.py`, `discovery.py`. |
| **Voice/**                           | 21     | (part of 364)        | Mostly `unused-ignore`; one real `assignment` in `synthesize.py:183`. |
| **VPN/**                             | 8      | (part of 364)        | 3× `return-value` in `peers/pairing.py`, 3× wireguard dict-item/assignment, 2× unused-ignore. |
| **Trainer/**                         | 8      | (part of 364)        | All in `Caption/video.py` — implicit-Optional plus downstream `arg-type`. |
| **Gateway/**                         | 7      | (part of 364)        | 5× `unused-ignore`; 1× `assignment` + 1× `attr-defined` in `audit_log.py`. |
| **Extensions/** (excl. Bridge)       | 7      | (part of 364)        | 3× `assignment` + 1× `var-annotated` in `Webcrawler/extractor.py`; 3× `unused-ignore`. |
| **Core/resource_monitor/**           | 7      | 10                   | All `unused-ignore`; 2 `annotation-unchecked` notes in `broker/registry.py`. |
| **device_gate/**                     | 6      | 9                    | 5× `unused-ignore`; 1× **real** `attr-defined` on `crypt` module. |
| **Extensions/extension_bridge/**     | 4      | 7                    | All `unused-ignore`. |
| **N8N/**                             | 0      | (part of 364)        | Clean. |
| **Total**                            | **268**| **390**              | |

**Service-level reading:**
- **Core dominates** the error count (200 / 268 ≈ 75 %), almost entirely
  via stale `unused-ignore` comments.
- **N8N** is mypy-clean already — a candidate for early strict-mode
  adoption.
- **Trainer** has the highest density of *real* type errors (8 errors,
  all in a single 200-line file) — fixable in one PR.
- **VPN** has the most concerning real signal (3× `return-value` shape
  mismatch in pairing.py) given that VPN is the project's auth boundary
  (see memory: principle #16).

---

## 4. Notable "real bug" candidates (top 10 for M5)

These are errors where the mypy signal is unlikely to be just "missing
annotation" — the type system is flagging behaviour that probably
misbehaves at runtime. Listed in approximate severity order.

| # | Location                                                  | Error                                                                                                                                              | Why it looks like a real bug |
|---|-----------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------|------------------------------|
| 1 | `device_gate/auth.py:89`                                  | `Module has no attribute "crypt"` `[attr-defined]`                                                                                                | The stdlib `crypt` module was **removed in Python 3.13**. The project pins `python>=3.11`, but any user on 3.13+ will see an `ImportError` at first auth attempt. |
| 2 | `VPN/peers/pairing.py:90`, `:92`, `:99` (3 errors)       | `Incompatible return value type (got "tuple[dict[str,…], str, int]", expected "tuple[bytes \| str, str, int]")`                                  | Pairing endpoint promises a `bytes\|str` response body but returns a `dict` — Flask will TypeError or auto-jsonify silently depending on path. Either the signature or the returns are wrong. |
| 3 | `Core/harness/memory/reflection.py:553`                   | `Incompatible types in assignment (expression has type "LongTermMemory", variable has type "WorkspaceMemory")`                                    | Two memory types mixed up. Downstream code is likely calling the wrong-store API. |
| 4 | `Core/harness/memory/post_turn_extractor.py:652`          | mirror of #3 — `WorkspaceMemory` assigned where `LongTermMemory` expected                                                                          | Same swap, opposite direction. The pair suggests a refactor mid-flight. |
| 5 | `Core/harness/tooling/tools/memory/memory_update/memory_update.py:45` | `WorkspaceMemory \| None` assigned where `LongTermMemory \| None` expected                                                                      | Same memory-type confusion in the user-facing memory-update tool. |
| 6 | `Trainer/Caption/video.py:77-80` + `:180-183` (8 errors) | Implicit Optional defaults plus four `arg-type` errors downstream                                                                                  | If a caller relies on the `None` default, `extract_frames` will crash on the first attribute/arithmetic op. Calls at 180-183 already trip this. |
| 7 | `Gateway/middleware/audit_log.py:47-48` (2 errors)        | `TextIOWrapper` assigned to `None`-typed slot, then `"None" has no attribute "write"`                                                              | If the file-open path is ever skipped, the next `.write` will AttributeError. Worth confirming the guard is correct. |
| 8 | `Core/harness/pipe.py:887`                                | `"_register_actions" does not return a value (it only ever returns None)` `[func-returns-value]`                                                  | Caller is binding/using the return of a function that's only ever `None`. Either the call is dead or the function is missing a return. |
| 9 | `Core/shared/discovery.py:430`                            | `Item "ServiceListener" of "_CatalogListener \| ServiceListener" has no attribute "services"` `[union-attr]`                                      | If the union narrows to `ServiceListener` at runtime, this attribute access will AttributeError. |
| 10 | `Core/harness/model_registry/_routing/hf_search.py:22`   | `Module has no attribute "quote"` `[attr-defined]`                                                                                                 | Wrong module reference — `quote` lives in `urllib.parse`, not `urllib`. Probably crashes on first use of the search path. |

**Honorable mentions** (next tier to check during M5):

- `Core/harness/memory/retrieval.py:190` — `None` assigned where `Module` expected; mirrored in `write_file.py:63` and `edit_file.py:73`.
- `Core/shared/ipc.py:1284` / `Core/Memgraph/ipc.py:1285` — `TestClient.open` access on `Any | TestClient` union (test-only, lower priority).
- `Core/harness/tool_registry/__init__.py:351` + `code_search_files.py:40` — `[no-redef]` shadowing (could be intentional, could be a bug).
- `Core/shared/tool_interface.py:62` — `isinstance` second arg typed as `object` (the check may silently always pass/fail).
- `VPN/tunnel/wireguard.py:59,111,114` — `int` assigned to `bool|None` slot; works under truthiness but indicates muddled types.

---

## 5. Recommended phase sequencing

### M2 — Triage

1. **Decide the space-named-dir question first.** Either rename the three
   dirs (`resource_monitor` → `ResourceMonitor` or `resource_monitor`,
   `device_gate` → `device_gate`, `extension_bridge` → `extension_bridge`)
   *or* remove the offending `__init__.py` files. The current four-part
   mypy invocation works but is fragile and easy to forget. A rename is
   the cleaner answer; the `__init__.py` removal works if these are truly
   script-only (which `device_gate` already behaves like, given its bare
   `from core import …` imports).
2. **Sweep the 222 `unused-ignore` comments.** These are the single
   biggest noise source. Either (a) bulk-delete every `# type: ignore[…]`
   that's flagged, or (b) lower `warn_unused_ignores` to `false` until
   M6. Option (a) is the right answer — the ignores already aren't doing
   anything; removing them is mechanical and de-risks the M6 strict-mode
   ramp. **Hotspot files** to clear first: `Core/harness/turn.py`,
   `Core/harness/pipe.py`, `Core/shared/ipc.py`, `Core/Memgraph/ipc.py`,
   `Core/shared/discovery.py`. Those five files account for ~90 of the
   222.
3. **File the 10 candidate-bug locations** from §4 as M5 work items.

### M3 — Stubs

- **N8N is already clean**, so it can skip M3/M4 entirely and move
  directly to M6 strict mode as the first proof-of-concept service.
- For everything else, `ignore_missing_imports = true` is hiding a long
  list of missing third-party stubs. Worth `mypy --install-types` once
  to see what's available (`types-PyYAML`, `types-requests`,
  `types-pywin32` etc.) and add them as dev deps. This will surface
  *new* errors against now-typed boundaries (e.g. `requests.Response`).

### M4 — Annotations

Order services from cheapest to expensive:
1. **N8N** — already clean.
2. **Extensions/extension_bridge** (4 errors, all `unused-ignore`) — easy
   win after M2's sweep.
3. **device_gate** (6 errors, 5 `unused-ignore` + 1 real bug from §4 #1)
   — small file count.
4. **resource_monitor** (7 errors) — small, contained.
5. **VPN** (8 errors) — small but contains the real `return-value`
   cluster.
6. **Trainer** (8 errors, all in `Caption/video.py`) — one file's worth
   of annotation work.
7. **Gateway** (7 errors) — small.
8. **Voice** (21 errors) — medium.
9. **Core** (200 errors, mostly `unused-ignore`) — biggest, but most of
   it goes away in M2's sweep. After that sweep, real annotation work is
   probably <30 errors.

### M5 — Real type errors

Fix the §4 top-10 bug candidates plus the honorable mentions, in the
order they're listed. Several are likely 1-line fixes (`urllib` →
`urllib.parse`, swap two type annotations). The VPN `pairing.py`
return-shape cluster and the `crypt` import deserve dedicated PRs.

### M6 — Finalize / strict mode

Once M2-M5 are done, layer in strict-mode settings *one at a time*,
service by service, starting with N8N:

```toml
[[tool.mypy.overrides]]
module = "N8N.*"
disallow_untyped_defs = true
strict_optional = true
warn_return_any = true
```

This lets the project run "strict where ready, lenient elsewhere"
indefinitely, rather than a flag-day global flip.

---

## Appendix A — pyproject.toml mypy section added by M1

```toml
[tool.mypy]
python_version = "3.11"
ignore_missing_imports = true
follow_imports = "normal"
warn_unused_ignores = true
warn_redundant_casts = true
show_error_codes = true
exclude = [
    "_legacy/",
    "docs/refactor-archive/",
    "\\.venv/",
    "Core/resource_monitor/",
    "device_gate/",
    "Extensions/extension_bridge/",
]
```

## Appendix B — Reproducing the baseline

```powershell
# Part 1 — top-level walkable dirs
uv run mypy --explicit-package-bases Core Gateway Voice VPN Trainer N8N Extensions

# Part 2 — device_gate
Push-Location "device_gate"
uv run mypy --explicit-package-bases .
Pop-Location

# Part 3 — Core/resource_monitor
Push-Location "Core/resource_monitor"
uv run mypy --explicit-package-bases .
Pop-Location

# Part 4 — Extensions/extension_bridge
Push-Location "Extensions/extension_bridge"
uv run mypy --explicit-package-bases .
Pop-Location
```

Expected exit code: `1` from each part (mypy found errors). Exit code
`2` means mypy itself blew up — most likely on a new space-containing
dir that needs to be added to the exclude list.

---

## 7. Post-M2-sweep result (2026-05-16)

The 222 stale `unused-ignore` ignores were bulk-removed in one pass.
Every `# type: ignore[…]` (or bare `# type: ignore`) that mypy reported
as `unused-ignore` was deleted from its line — code, other inline
comments, and indentation were preserved.

**Headline numbers after the sweep:**

| Metric                         | Pre-sweep | Post-sweep | Δ        |
|--------------------------------|----------:|-----------:|---------:|
| Total errors                   | 268       | **46**     | −222     |
| `unused-ignore`                | 222       | **0**      | −222     |
| Files with at least one error  | 75        | 27         | −48      |
| Source files checked           | 390       | 390        | —        |

**Per-part breakdown (post-sweep):**

| Part | Scope                          | Errors | Notes                                                |
|------|--------------------------------|-------:|------------------------------------------------------|
| 1    | Core / Gateway / Voice / VPN / Trainer / N8N / Extensions | 45 | All real-bug categories; matches the §4 candidate list. |
| 2    | `device_gate/`                 | 1      | `auth.py:89` — `Module has no attribute "crypt"` (§4 #1). |
| 3    | `Core/resource_monitor/`       | 0      | Clean (2 `annotation-unchecked` notes remain).       |
| 4    | `Extensions/extension_bridge/` | 0      | Clean.                                               |

**Files touched:** 60 (across all services).
**Lines edited:** 222 (one `# type: ignore[…]` removed per line).
**Second pass:** not needed — `unused-ignore` is zero on first re-run.

**What's left (46 errors) maps cleanly to §4's candidate-bug list:**

- The full §4 top-10 candidate list now accounts for ~25 of the 46
  remaining errors (the implicit-Optional cluster in
  `Trainer/Caption/video.py` alone is 8, the memory-type swaps are 4,
  the `pairing.py` return-shape cluster is 3, etc.).
- The remainder is the §4 honorable-mentions tier — `discovery.py`
  union-attr, the `urllib.quote` attr-defined, `wireguard.py`
  bool/int/str confusions, plus three new minor `arg-type`/`var-annotated`
  surfaces that were previously masked by ignores
  (`Voice/audio_io.py:158`, `Core/harness/tooling/tools/dev/lint_all/lint_all.py:36`,
  `Core/harness/model_registry/_routing/benchmarks.py:159`,
  `Core/harness/backend/backend_routing.py:242`,
  `Core/harness/memory/graph_retrieval.py:116`,
  `Core/harness/memory/retrieval.py:232`,
  `Core/harness/turn.py:332`,
  `Core/shared/tool_interface.py:147`,
  `Core/harness/pipe.py:858`).

**Verification (all green):**

- `ruff check .` → clean.
- `ruff format --check .` → clean (every swept file already formatted).
- `pytest Core Gateway "device_gate" Voice VPN Trainer N8N` → **360 passed**, 7 warnings, 49.37s.
- `wylde_check.run_all()` → 8 rules, **0 findings**.

**M2 step 1 is complete.** Next step in M2 is the space-named-dir
decision (§5 / M2 item 1), then handing the remaining 46 errors to M5.

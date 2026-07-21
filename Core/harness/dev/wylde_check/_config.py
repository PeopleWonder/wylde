"""Configuration constants for the wylde_check rules.

Pure data — no imports from sibling submodules, no filesystem walks at
import time.  Split out from ``wylde_check.py`` so individual rules can
import only the constants they need.
"""

from __future__ import annotations

import re
from typing import Tuple


# Walk-time exclusions.  These never get inspected by any rule.
EXCLUDED_DIRS: Tuple[str, ...] = (
    "_legacy",
    "__pycache__",
    "vendor",
    ".venv",
    "venv",
    ".git",
    "target",  # Cargo build output (Core/GUI/target/, gpui workspace)
    "rust/target",  # Cargo build output for the backend Rust workspace
    "build",  # generic build output
    ".pytest_cache",
    "docs/refactor-archive",  # historical context only
)


# Retained for the queued no_internal_http_rust rule (see issue) — the
# Python rule was retired 2026-07-20 but no Rust counterpart exists yet.
#
# Wylde-internal ports + loopback hosts the rule scans for.
# ``11434`` (Ollama) and ``7687`` (Memgraph Bolt) are external from
# Wylde's perspective but listed for completeness.
INTERNAL_HOSTS: Tuple[str, ...] = (
    "127.0.0.1",
    "localhost",
    "0.0.0.0",
)
INTERNAL_PORTS: Tuple[str, ...] = (
    "8005",  # Gateway
    "8011",  # tool registry (legacy)
    "8013",  # Trainer
    "8014",  # VPN
    "8020",  # browser-extension ingress
    "5678",  # n8n
    "11434",  # Ollama
    "7687",  # Memgraph Bolt
)


# Known-dead service names.  Each entry is a literal substring; rule 6
# greps for any occurrence in active code.  Comments-only matches in
# documented archive files are still findings — operators can move them
# out of active code if they want.
DEAD_SERVICE_NAMES: Tuple[str, ...] = (
    "wylde-orchestrator",
    "wylde-graph",  # renamed to wylde-memgraph
    "security-api",
    "fletch-web",
    "wylde-rag",
    "VoiceAssistant",  # dissolved into Voice
    "wylde-base",
    "wylde-improve",
)


# Inline marker that suppresses a single line from rule 6.  Two forms so
# the marker can ride at the end of either a host-language comment
# (Python / JS / Rust / Svelte) or a markdown comment.
DEAD_REF_OK_MARKERS: Tuple[str, ...] = ("wylde-check: dead-ref-ok",)


# Files that are entirely intentional historical context for rule 6.
# JSON archives can't carry inline markers; doc/template files and
# legacy-origin module docstrings routinely reference dead names by
# design.  These are skipped wholesale.
DEAD_REF_ALLOWLISTED_FILES: Tuple[str, ...] = (
    # JSON / archive templates
    "N8N/workflow_templates/agent-orchestra.json",
    # The rename-plan tracking doc
    "WYLDE_ENDPOINTS.md",
    # Historical GUI audit / migration docs
    "Core/GUI/docs/inference-bar-audit.md",
    "Core/GUI/docs/inference-bar-migration-plan.md",
    "N8N/workflow_templates/README.md",
    # Gateway scope audit digest — intentionally names dead services
    "Gateway/_audit/gateway_scope_digest.md",
    # Seed-graph relational dump (historical service map)
    "Core/Memgraph/seed_graph.py",
    # Test fixtures that exist to test rule 6 itself (split into a per-
    # rule-group package; each module uses dead names as synthetic data).
    "Core/harness/dev/tests/wylde_check/test_arch.py",
    "Core/harness/dev/tests/wylde_check/test_gui.py",
    # Test files that use dead service names as priority / fixture data
    "Core/harness/tests/test_memory.py",
    "Core/harness/tooling/tests/test_smoke.py",
    "Core/harness/model_registry/tests/test_model_registry.py",
    "Core/shared/tests/test_vram_broker_client.py",
    "Core/resource_monitor/test_vram_broker.py",
    # Legacy-origin module docstrings ("Pulled forward from
    # _legacy/core/wylde-rag/..." etc.)
    "Core/harness/memory/_common.py",
    "Core/harness/memory/embeddings.py",
    "Core/harness/memory/memgraph.py",
    "Core/harness/memory/miss_log.py",
    "Core/harness/memory/rag.py",
    "Core/harness/memory/rag_cache.py",
    "Core/harness/memory/rag_decompose.py",
    "Core/harness/memory/rag_gate.py",
    "Core/harness/memory/rag_multihop.py",
    "Core/harness/memory/rag_pipeline.py",
    "Core/harness/tooling/tools/meta/graph_query/graph_query.py",
    "Core/harness/tooling/tools/rag/__init__.py",
    "Core/harness/tooling/tools/rag/_rag_lib.py",
    "Core/harness/tooling/tools/rag/rag_ask/rag_ask.py",
    "Core/harness/tooling/tools/rag/rag_chunk_usage/rag_chunk_usage.py",
    "Core/harness/tooling/tools/rag/rag_graph_stats/rag_graph_stats.py",
    "Core/harness/tooling/tools/rag/rag_index/rag_index.py",
    "Core/harness/tooling/tools/rag/rag_misses/rag_misses.py",
    "Core/harness/tooling/tools/rag/rag_prune/rag_prune.py",
    "Core/harness/tooling/tools/rag/rag_reindex/rag_reindex.py",
)


# Retained for the queued no_internal_http_rust rule (see issue) — the
# Python rule was retired 2026-07-20 but no Rust counterpart exists yet.
#
# Catches the common HTTP client entry-points.  We intentionally keep it
# stringy so a future client library doesn't slip through silently — add
# to this list.
HTTP_CLIENT_PATTERNS: Tuple[re.Pattern[str], ...] = (
    re.compile(r"\brequests\.(?:get|post|put|delete|patch|head|options|request)\s*\("),
    re.compile(
        r"\bhttpx\.(?:get|post|put|delete|patch|head|options|request|AsyncClient|Client)\s*\("
    ),
    re.compile(
        r"\baiohttp\.(?:get|post|put|delete|patch|head|options|request|ClientSession)\s*\("
    ),
    re.compile(r"\burllib3\."),
    re.compile(r"\bfetch\s*\("),  # JS / Svelte
    re.compile(r"\bnew\s+XMLHttpRequest\b"),  # JS legacy
    re.compile(r"\bnew\s+WebSocket\b"),  # JS WebSocket
)


# Active-code roots to walk.  All paths relative to WYLDE_ROOT.
ACTIVE_ROOTS: Tuple[str, ...] = (
    "Core",
    "Gateway",
    "device_gate",
    "Voice",
    "VPN",
    "N8N",
    "Trainer",
    "Extensions",
    "rust",  # Cargo workspace for the R-phase port; .rs sources live in rust/crates/*/src/
)


# Pipe-name convention regex (rule 17).  Two passes:
#
# * ``PIPE_NAME_REF_RE``: matches the canonical dash form anywhere in
#   the file — used to enforce lowercase / no-uppercase / no-trailing-
#   noise on every appearance.
# * ``PIPE_NAME_TYPO_RE``: matches the underscore form ONLY when it
#   immediately follows ``pipe`` in a Windows named-pipe path (``\\.\pipe\wylde_X``
#   in either single- or double-escaped form).  Without that anchor
#   we'd catch every Python identifier like ``wylde_root`` and
#   ``wylde_check``.
PIPE_NAME_REF_RE = re.compile(r"\bwylde-[A-Za-z0-9_\-]+")
PIPE_NAME_TYPO_RE = re.compile(r"pipe[\\/](wylde_[A-Za-z][A-Za-z0-9_]*)")
PIPE_NAME_GOOD_RE = re.compile(r"^wylde-[a-z][a-z0-9\-]*$")


# Rust crate root.  Rules 26-29 walk this tree.
RUST_CRATES_ROOT: str = "rust/crates"


# Rule 26: deep `super::super::*` chains are a code-smell — by the time
# you're traversing three module levels up, the module organisation is
# wrong.  Anchored regex used only against the part of the `use` line
# after the keyword.
RUST_DEEP_SUPER_RE = re.compile(r"\bsuper::super::")


# Rule 26: cross-crate Rust imports.  Wylde crates (other than
# ``wylde-shared``) may only depend on each other via the shared crate;
# anything else routes a service through another service's surface
# instead of using the pipe/IPC contract.  Imports of ``wylde_shared``
# are always allowed; imports of one's own crate name (path component) are
# allowed.  All other ``wylde_<name>`` use-paths are flagged.
RUST_USE_CRATE_RE = re.compile(r"\buse\s+(wylde_[A-Za-z0-9_]+)\b")


# Rule 27: Rust silent-Result-swallow patterns.  ``let _ = expr;`` and
# trailing ``.ok();`` are the two idiomatic ways to drop a Result.  An
# inline marker ``// wylde-check: discard-result-ok`` on the same line
# suppresses the rule.
RUST_LET_UNDERSCORE_RE = re.compile(r"^\s*let\s+_\s*=\s*(?P<expr>.+?);")
RUST_DOT_OK_RE = re.compile(r"\.ok\s*\(\s*\)\s*;")
RUST_DISCARD_RESULT_MARKER = "wylde-check: discard-result-ok"


# Rule 28: only ``wylde_shared::logging::configure_logging`` may init
# the global tracing subscriber.  These patterns flag the canonical
# subscriber-construction entry points; matching the start of a
# subscriber chain is more reliable than trying to track the trailing
# ``.try_init()`` across the multi-line builder.  Lines starting with
# ``//`` are skipped by the rule so doc references to these symbols
# (e.g. this docstring) don't false-fire.
RUST_LOGGING_INIT_PATTERNS: Tuple[re.Pattern[str], ...] = (
    re.compile(r"\btracing_subscriber::fmt\s*\("),
    re.compile(r"\btracing_subscriber::registry\s*\("),
    re.compile(r"\btracing::subscriber::set_global_default\s*\("),
    re.compile(r"\btracing::subscriber::with_default\s*\("),
)


# Rule 29: only specific crates may spawn external processes.  These
# patterns flag every ``Command::new`` call outside the allowlist.
# Sync and tokio variants both restricted.
#
# Allowlist:
#   * ``wylde-lifecycle`` — supervises every other long-running service.
#   * ``wylde-extension-bridge`` — MCP-server host (Phase 4): owns the
#     stdin/stdout of each child MCP server. Routing spawn through a
#     lifecycle pipe action is not workable for stdio MCP because the
#     bridge would lose direct access to the child's pipes. Scope is
#     narrow: only the MCP transport modules (``src/mcp/``) need to
#     spawn; the rule still fires for spawns elsewhere in the crate.
#   * ``wylde-lsp`` — rust-analyzer LSP host (IDE S8): an LSP client IS a
#     language-server supervisor — it owns the stdin/stdout of the
#     ``rust-analyzer`` child to speak JSON-RPC. Same principled exemption
#     as the extension bridge's stdio MCP transport: routing the spawn
#     through a lifecycle pipe action would lose the direct pipe access the
#     protocol needs. The service is OPTIONAL (core works without it) and
#     spawns exactly one well-known binary.
RUST_PROCESS_SPAWN_PATTERNS: Tuple[re.Pattern[str], ...] = (
    re.compile(r"\bstd::process::Command::new\s*\("),
    re.compile(r"\btokio::process::Command::new\s*\("),
    # `use std::process::Command;` followed by bare `Command::new(...)`.
    # We catch the bare form by anchoring on Command::new (capitalised).
    re.compile(r"(?<!::)\bCommand::new\s*\("),
)
RUST_PROCESS_SPAWN_ALLOWED_CRATES: Tuple[str, ...] = (
    "wylde-lifecycle",
    "wylde-extension-bridge",
    "wylde-lsp",
)
# Back-compat alias for callers that still import the singular name.
RUST_PROCESS_SPAWN_ALLOWED_CRATE: str = RUST_PROCESS_SPAWN_ALLOWED_CRATES[0]


# Rule 54: every persistent file log must inherit the shared rotation
# policy.  The canonical logging module
# (``rust/crates/wylde-shared/src/logging.rs``) owns the ONE append-only
# ``OpenOptions`` behind ``RotatingLog`` / ``open_rotating_append``; the
# rule skips that file and flags a raw ``.append(true)`` anywhere else —
# the tell-tale of an ad-hoc uncapped log sink that bypasses rotation.
# Matches both ``std::fs`` and ``tokio::fs`` OpenOptions builders.  A
# same-line ``// wylde-check: unbounded-append-ok`` marker suppresses the
# rule for a justified non-log append.
RUST_UNBOUNDED_APPEND_PATTERNS: Tuple[re.Pattern[str], ...] = (
    re.compile(r"\.append\(\s*true\s*\)"),
)
RUST_UNBOUNDED_APPEND_MARKER = "wylde-check: unbounded-append-ok"
# The single sanctioned home of an append-only open — the rotation
# factory itself.  Skipped wholesale (analogous to rule 28 skipping the
# canonical logging file for subscriber init).
RUST_LOG_ROTATION_FACTORY_FILE = "rust/crates/wylde-shared/src/logging.rs"


# ── Rules 44-47: boot / shutdown / service-manifest correctness ──
#
# Added at the slice-11 cutover (2026-05-29) so boot + shutdown stay
# driven by a single source (no hardcoded, hand-kept service roster) and
# every backend service carries a schema-valid manifest. the Wylde user's
# modularity directive: "give special attention that launcher and shutdown
# rules cover all services attached with modularity in mind."
#
# REPOINTED for issue #101 (0.2 stability audit, finding F): the original
# rules 44/45 targeted `Core/Lifecycle/launcher.py` / `shutdown.py`, which
# the full-Rust cutover DELETED. Guarded by `if <file>.exists()`, they ran
# over a missing file, found nothing, and passed green — a dead gate. They
# now target the LIVE Rust single source of truth: the `DAEMON_MANAGED`
# table (`rust/crates/wylde-lifecycle/src/daemon_managed.rs`) that drives
# boot, shutdown, dispatch, and the kill-image list from one row per
# service. The SEMANTIC set-equality gate (boot-set == shutdown-set ==
# dispatch-set, modulo the two typed exceptions) is the crate unit test
# `daemon_managed::tests::boot_shutdown_dispatch_sets_agree`, run in CI by
# `cargo test --workspace`; these static rules ensure that single source
# stays STRUCTURALLY in place — the table exists, boot + shutdown are
# derived from it, and no hand-kept `const SERVICES` roster returns.

RUST_LIFECYCLE_CRATE: str = "rust/crates/wylde-lifecycle"
GPUI_SHUTDOWN_RS: str = "Core/GUI/Shell/src/shutdown.rs"

# The single source of truth (issue #101). Rule 44 asserts this file
# declares the table; rules 44/45 assert boot + shutdown are derived from
# it (below). A missing token here means the single source was ripped out
# or bypassed — the gate fires.
RUST_DAEMON_MANAGED_FILE: str = "rust/crates/wylde-lifecycle/src/daemon_managed.rs"
RUST_DAEMON_MANAGED_TABLE_TOKEN: str = "DAEMON_MANAGED"

# Boot must be derived from the table (`daemon.rs` iterates `boot_sequence()`),
# not a hand-written run of `start_<name>()` calls.
RUST_BOOT_FILE: str = "rust/crates/wylde-lifecycle/src/daemon.rs"
RUST_BOOT_TABLE_TOKEN: str = "boot_sequence"

# Shutdown must be derived from the table (`state/mod.rs` iterates
# `shutdown_sequence()`), not a hand-kept `let steps: [_; N]` array.
RUST_SHUTDOWN_FILE: str = "rust/crates/wylde-lifecycle/src/state/mod.rs"
RUST_SHUTDOWN_TABLE_TOKEN: str = "shutdown_sequence"

# The anti-pattern rule 44 still forbids: a `const`/`static` SERVICES array
# (Rust) reintroducing a hand-kept roster. Case-sensitive so ordinary
# lowercase locals never match. (The `DAEMON_MANAGED` table is not a
# `SERVICES` array and is intentionally not matched.)
#
# Two blind spots were closed in #115:
#   * ANY uppercase qualifier prefix — `CORE_SERVICES`, `DAEMON_SERVICES`,
#     `WYLDE_SERVICES` — not just the bare/`ALL_` forms. `CORE_SERVICES`
#     (the exact literal #101 deleted from control.rs) escaped before.
#   * SLICE syntax `: &[&str] = &[`, not only the array-annotation form
#     `: [&str; N] = [`. The idiomatic slice table has a `&` before the `[`,
#     so every slice-form roster escaped regardless of name.
RUST_HARDCODED_SERVICE_ARRAY_RE = re.compile(
    r"\b(?:const|static)\s+_?(?:[A-Z][A-Z0-9]*_)*SERVICES?(?:_LIST|_NAMES)?\s*:\s*&?\[",
)

# The gpui-side graceful shutdown must delegate to the daemon drain via
# this action (rule 45). Rule 45 checks only that this token is present;
# it does NOT count service coverage of the GUI's hard-kill / drain-wait
# sets. It used to exempt the WYLDE_SERVICE_PROCESSES / WYLDE_KILL_TARGETS
# constants from any roster check — an exemption that hid issue #124.
# Those constants are gone (both sets now derive from
# wylde_stack::shutdown_targets) and the counting gate is a Rust test:
# rust/crates/wylde-stack/tests/shutdown_target_coverage.rs.
GPUI_SHUTDOWN_DELEGATE_TOKEN: str = "lifecycle.shutdown_all"

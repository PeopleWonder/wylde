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


# Files exempt from rule 1 (no_internal_http).  The Ollama / Memgraph
# clients talk to external systems on the local box; extensions may call
# the Gateway boundary.
NO_HTTP_EXEMPT_PREFIXES: Tuple[str, ...] = (
    "Core/harness/backend/ollama_client.py",  # external LLM daemon
    "Core/harness/model_registry/_routing/ollama_watcher.py",
    "Core/harness/backend/request_building.py",  # builds Ollama bodies
    "Core/harness/tooling/tools/ollama",  # /api/* helpers
    "Core/harness/tooling/tools/visual/browser_",  # Playwright HTTP
    "Core/Memgraph",  # Bolt (7687) is DB wire protocol
    "Extensions",  # extensions can call Gateway
    # (The old `Core/GUI/src-tauri/src` exemption was dropped at the
    # slice-11 cutover — that tree is deleted, and rule 1 walks only
    # .py/.svelte/.js/.ts, so the gpui Rust GUI isn't scanned here.)
    #
    # The `Gateway`, `VPN`, and `Core/resource_monitor` exemptions were
    # dropped once the strangler deleted their Python sources — Gateway
    # (boundary HTTP), VPN (WireGuard/STUN/TURN), and the vram-broker
    # (resource_monitor, deleted in 7072947). No .py/.svelte/.js/.ts
    # source remains under those prefixes, so the exemptions matched
    # nothing — same cleanup as the earlier device_gate / vram_broker
    # prunes.
)


# Wylde-internal ports + loopback hosts the rule scans for.
# ``11434`` (Ollama) and ``7687`` (Memgraph Bolt) are external from
# Wylde's perspective but listed for completeness — the exemption
# prefixes above cover the legitimate callers.
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


# Gateway route categories per the Wylde user's contract.  Any route handler
# whose path doesn't start with one of these prefixes gets flagged for
# review.  The list is intentionally generous — egress + the dozen
# inbound mobile-future routes + MCP + extensions.
GATEWAY_ROUTE_PREFIXES: Tuple[str, ...] = (
    "/api/egress",
    "/api/chat",
    "/api/conversations",  # chat-history CRUD (mobile-bound)
    "/api/prompts",  # system-prompt overrides + presets (mobile-bound)
    "/api/devices",
    "/api/link",
    "/api/settings",
    "/api/system",
    "/api/rag",
    "/api/images",
    "/api/models",
    "/api/tools",  # tool registry
    "/api/dev",  # local-only dev diagnostics (GUI error-capture sink)
    "/api/health",
    "/health",
    "/mcp",  # planned
    "/extensions",  # phase 7 contract
    "/__action__",  # internal action dispatch
)


# Canonical tool id / name regex.  Snake or dotted, lower-case, digits OK.
TOOL_ID_RE = re.compile(r"^[a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)*$")


# Pre-compiled patterns for rule 1.  Catches the common HTTP client
# entry-points.  We intentionally keep it stringy so a future client
# library doesn't slip through silently — add to this list.
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


# Service folders that are documented entry points (used by rules 16-19).
# Some entries (Trainer, N8N, Extensions/*) are library-style and don't
# host their own run.py — the rules tolerate the absence and only flag
# violations of the naming/contract when a run.py IS present.
SERVICE_FOLDERS: Tuple[str, ...] = (
    "Core/resource_monitor",
    "Core/Memgraph",
    "device_gate",
    "Gateway",
    "Voice",
    "VPN",
    "Trainer",
    "N8N",
    "Extensions/extension_bridge",
    "Extensions/Webcrawler",
    "Extensions/Wylde_Study",
)


# Subprocess-spawn callsites are restricted to these prefixes (rule 14).
# Lifecycle is the daemon's job; tool runtimes wrap external CLIs by
# design; Memgraph wraps the Neo4j JVM; VPN/tunnel runs wg/iptables;
# Voice/device_manager talks to the system audio stack.
SUBPROCESS_ALLOWED_PREFIXES: Tuple[str, ...] = (
    "Core/Lifecycle/",
    "Core/harness/dev/",
    "Core/harness/tooling/tools/",  # all tool runtimes
    "Core/Memgraph/",  # Neo4j JVM wrapper
    "Voice/device_manager.py",  # system audio device control
    "VPN/tunnel/",  # wg-quick / iptables shell-outs
)


# Subprocess-spawn patterns rule 14 catches.  Stringy on purpose so the
# rule stays diff-friendly when a new spawning API needs blocking.
SUBPROCESS_PATTERNS: Tuple[re.Pattern[str], ...] = (
    re.compile(r"\bsubprocess\.Popen\s*\("),
    re.compile(r"\bsubprocess\.run\s*\("),
    re.compile(r"\bsubprocess\.call\s*\("),
    re.compile(r"\bsubprocess\.check_call\s*\("),
    re.compile(r"\bsubprocess\.check_output\s*\("),
    re.compile(r"\bos\.spawnv\s*\("),
    re.compile(r"\bos\.spawnvp\s*\("),
    re.compile(r"\bos\.spawnvpe\s*\("),
    re.compile(r"\bos\.spawnl\s*\("),
    re.compile(r"\bos\.spawnle\s*\("),
    re.compile(r"\bos\.spawnlp\s*\("),
    re.compile(r"\bos\.spawnlpe\s*\("),
)


# Logging-setup patterns rule 13 catches outside Core/shared/logging_setup.py.
LOGGING_SETUP_PATTERNS: Tuple[re.Pattern[str], ...] = (
    re.compile(r"\blogging\.basicConfig\s*\("),
    re.compile(r"\blogging\.getLogger\(\s*\)\s*\.\s*addHandler\s*\("),
    re.compile(r"\blogging\.getLogger\(\s*\)\s*\.\s*setLevel\s*\("),
    re.compile(r"\blogging\.root\s*\.\s*addHandler\s*\("),
    re.compile(r"\blogging\.root\s*\.\s*setLevel\s*\("),
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


# Deprecated run.py naming variants (rule 16).  If any of these patterns
# matches a top-level file in a service folder, the convention is broken.
DEPRECATED_ENTRY_PATTERNS: Tuple[re.Pattern[str], ...] = (
    re.compile(r"^[A-Za-z0-9_-]+_run\.py$"),
    re.compile(r"^start_[A-Za-z0-9_-]+\.py$"),
    re.compile(r"^launcher[A-Za-z0-9_-]*\.py$"),
    re.compile(r"^main_[A-Za-z0-9_-]+\.py$"),
    re.compile(r"^server_[A-Za-z0-9_-]+\.py$"),
)


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


# ── Rules 44-47: launcher / shutdown / service-manifest correctness ──
#
# Added at the slice-11 cutover (2026-05-29) so the launcher + shutdown
# stay manifest-driven (no hardcoded service roster) and every backend
# service carries a schema-valid manifest. the Wylde user's modularity directive:
# "give special attention that launcher and shutdown rules cover all
# services attached with modularity in mind … make them reference the
# manifests."

# Canonical launcher + shutdown sources. The launcher is Python
# (WYLDE_LIFECYCLE_IMPL defaults to python); the Rust daemon is a parity
# port held to the same no-hardcoded-roster contract by the negative
# scan below + its own crate tests.
LAUNCHER_PY: str = "Core/Lifecycle/launcher.py"
SHUTDOWN_PY: str = "Core/Lifecycle/shutdown.py"
RUST_LIFECYCLE_CRATE: str = "rust/crates/wylde-lifecycle"
GPUI_SHUTDOWN_RS: str = "Core/GUI/Shell/src/shutdown.rs"

# A reference to any of these proves the launcher/shutdown builds its
# service set from the filesystem-as-registry (services.yaml + per-service
# manifest.json) rather than a hardcoded list.
LAUNCHER_MANIFEST_REFERENCES: Tuple[str, ...] = (
    "load_services",
    "load_manifest",
    "list_service_folders",
)
SHUTDOWN_ENUMERATION_REFERENCES: Tuple[str, ...] = (
    "get_running",
    "load_manifest",
    "shutdown_order",
    "_shutdown_sequence",
    "load_services",
)

# The anti-pattern rules 44/45 forbid: a module-level UPPERCASE service
# roster assigned a list/tuple literal (Python), or a `const`/`static`
# SERVICES array (Rust). Case-sensitive so ordinary lowercase locals like
# `services = load_services()` never match.
PY_HARDCODED_SERVICE_LIST_RE = re.compile(
    r"^\s*_?(?:ALL_)?SERVICES?(?:_LIST|_NAMES)?\s*(?::[^=]+)?=\s*[\[\(]"
)
RUST_HARDCODED_SERVICE_ARRAY_RE = re.compile(
    r"\b(?:const|static)\s+_?(?:ALL_)?SERVICES?(?:_LIST|_NAMES)?\s*:\s*\[",
)

# The gpui-side graceful shutdown must delegate to the manifest-driven
# Python drain via this action (rule 45). The hard-kill image-name
# fallback constants (WYLDE_SERVICE_PROCESSES / WYLDE_KILL_TARGETS) are a
# recognised last resort — image names for `taskkill`, not the service
# enumeration — so they are deliberately NOT treated as a hardcoded
# roster.
GPUI_SHUTDOWN_DELEGATE_TOKEN: str = "lifecycle.shutdown_all"

# Top-level dirs that are NOT discoverable services. Source of truth is
# Core/Lifecycle/_common.EXCLUDED_TOP_LEVEL — keep this in sync when that
# set changes (this module is deliberately import-free, so the mirror is
# manual). `Core` holds a legitimate infra rollup manifest
# (Core/manifest.json); `data`/`logs`/`docs` are runtime/archive dirs and
# `rust`/`tools` are build/dev folders — none may carry a service manifest.
SERVICE_MANIFEST_EXCLUDED_TOP_LEVEL: Tuple[str, ...] = (
    "Core",
    "data",
    "logs",
    "docs",
    "rust",
    "tools",
)
SERVICE_MANIFEST_NONSERVICE_DIRS: Tuple[str, ...] = ("data", "logs", "docs")

# Required keys on a top-level service manifest (rule 47). `entry_point`
# is the canonical launch command / binary (may be null for an in-process
# / library / pipe-only service); there is deliberately no separate
# `binary` key — one field, one source of truth.
SERVICE_MANIFEST_REQUIRED_KEYS: Tuple[str, ...] = (
    "name",
    "entry_point",
    "shutdown_order",
)

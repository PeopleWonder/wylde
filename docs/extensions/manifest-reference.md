---
title: Extension manifest reference (mcp-server.json)
audience: extension authors
updated: 2026-06-02
status: reference; authoritative parser is rust/crates/wylde-extension-bridge/src/manifest.rs
---

# `mcp-server.json` reference

Every extension is a directory under `Extensions/<Name>/` with one
`mcp-server.json`. The bridge deserializes it into `McpServerManifest`
(`rust/crates/wylde-extension-bridge/src/manifest.rs`). Fields below are exactly
that struct — when this doc and the struct disagree, the struct wins.

## Top-level fields

| Field | Type | Required | Default | Notes |
| --- | --- | --- | --- | --- |
| `name` | string | **yes** | — | Dispatcher namespace prefix. Exact, case-sensitive. Must be non-empty. |
| `description` | string | no | `""` | Shown in `ext.list`. |
| `version` | string | no | `"1.0"` | Free-form. |
| `enabled` | bool | no | `false` | `ext.enable`/`ext.disable` persist by rewriting this field. |
| `transport` | enum | **yes** | — | `"stdio"` only. `"http"` parses but is rejected at validation. |
| `command` | string[] | yes for stdio | `[]` | Argv for the child. First element = program, rest = args. Must be non-empty for stdio. |
| `cwd` | string | no | extension dir | Working directory. Relative paths resolve against the manifest's parent. |
| `env` | map | no | `{}` | Extra env vars, merged onto the parent process env. |
| `url` | string | no | — | Reserved for `http` transport (deferred). |
| `capabilities` | string[] | no | `[]` | Declared capabilities (see vocabulary below). |
| `tools` | DeclaredTool[] | no | `[]` | Optional static catalog — lets `ext.tools.list` answer for a **disabled** extension without spawning it. The live `tools/list` from the running server is authoritative when enabled. |
| `ui_panels` | UiPanel[] | no | `[]` | GUI Tools-tab panels. |
| `resources` | ResourceDeclaration[] | no | `[]` | Declare tools as harness **resources** dispatched via the `wylde_*` verb layer (Slice 5a). Absent ⇒ today's named-tool behaviour. See below. |
| `health` | HealthConfig | no | see below | Liveness probe. |

### Placeholder tokens (expanded in `command`, `cwd`, `env`)

Resolved at spawn time in `wylde-extension-bridge/src/mcp/client.rs`:

| Token | Resolves to | Override env |
| --- | --- | --- |
| `${WYLDE_ROOT}` | repo root (`.` if unset) | `WYLDE_ROOT` |
| `${WYLDE_BIN}` | `<root>/rust/target/release` | `WYLDE_BIN` |
| `${WYLDE_PYTHON}` | `<root>/.venv/Scripts/python.exe` (Win) / `.venv/bin/python3` (Unix) | `WYLDE_PYTHON` |

Rust extensions use `${WYLDE_BIN}/wylde-ext-<name>`; legacy Python uses
`${WYLDE_PYTHON} -m Extensions._shim.server --extension <Name>`.

### Capabilities vocabulary

Declared, not transport-enforced — consumed by the gateway egress allowlist and
tier/consent gates (`Extensions/extension_bridge/contract.py` `LeavesSystem`):

`egress.web` · `egress.browser` · `egress.native` · `ingress.http` ·
`ingress.browser` · `inference.local`

Webcrawler declares `egress.web`; Wylde_Study declares
`ingress.browser`, `egress.browser`. Declare honestly — see
[harness-api-reference.md](./harness-api-reference.md) for what they actually gate.

#### `inference.local` — the bridge inference gate

`inference.local` is **enforced** (not just metadata): it is the capability the
extension-bridge checks before it will broker an inference call for your
extension. An extension that declares it may call the bridge's
`inference.embed` / `inference.chat` verbs, which forward — capability-checked,
rate-limited, and audited — to `wylde-ollama` (VRAM-broker lease + resident
keep-alive'd model reuse + per-request model swap). An extension **without**
`inference.local` calling those verbs is rejected with `capability_denied`.

This is the supported path for extension inference — do **not** POST directly to
Ollama's `127.0.0.1:11434`: a direct call bypasses the VRAM broker lease, the
connection-resilience layer, and the policy gate. See
[harness-api-reference.md](./harness-api-reference.md#bridge-inference-gate) for
the request/response shapes and rate-limit knobs. (The short form `inference`
is also accepted for back-compat, but `inference.local` is canonical.)

## `DeclaredTool` (entries in `tools[]`)

| Field | Type | Notes |
| --- | --- | --- |
| `tool_id` | string | **required**; the tool name. |
| `description` | string | shown to the model. |
| `endpoint` | string? | handler function (legacy/Python); defaults to `tool_id`. |
| `group` | string? | catalog grouping. |
| `tags` | string[] | searchable labels. |
| `parameters` | JSON | parameter schema (legacy array form, or JSON Schema). |
| `version` | string? | per-tool version. |

The *live* MCP `tools/list` your server returns uses MCP shape instead:
`{name, description, inputSchema}` where `inputSchema` is a JSON Schema object
(see `wylde-ext-webcrawler/src/mcp.rs:125`). `tools[]` in the manifest is the
offline mirror; the server's response is canonical when running.

## `resources[]` (ResourceDeclaration) — Slice 5a

`resources[]` lets an extension expose its tools as **resources** the harness
verb layer (`wylde_list/get/create/update/delete/search/execute`) dispatches,
instead of (or alongside) the flat `ext.tools.*` named-tool surface. The model
learns one resource and the seven verbs it already knows, not N tool names.
Parsed and validated by `manifest.rs` (structs `ResourceDeclaration` /
`OperationDeclaration` / `ActionDeclaration`); registered into the harness
`ResourceRegistry` overlay, namespaced to `ext:<Extension>:<resource_type>` so
it can never shadow a built-in or collide across extensions.

**Flag-gated.** The cutover flag `WYLDE_HARNESS_VERB_TOOLS` (read by
`wylde-extension-bridge::verb_mode_active`) is **dark until the Slice-6 cutover**.
With it **off**, declarations still parse and are exposed via `ext.resources.list`,
but the overlay is not populated and your named tools flow unchanged (a no-op for
the model). With it **on**, every tool a resource op names is **claimed** — hidden
from the named catalog and reachable only through the verb layer. A tool is either
claimed by exactly one resource op *or* stays named — never both.

### `ResourceDeclaration` (entries in `resources[]`)

| Field | Type | Required | Default | Notes |
| --- | --- | --- | --- | --- |
| `resource_type` | string | **yes** | — | Bare slug (`"url"`, `"todo"`). Namespaced to `ext:<Extension>:<slug>` at registration. Non-empty; unique within the extension. |
| `display_name` | string | no | `""` | Human label. |
| `description` | string | no | `""` | Shown to the model via `wylde_describe`. |
| `scope` | string | no | `"global"` | One of `global` \| `workspace` \| `conversation`. |
| `identifier_fields` | string[] | no | `[]` | Fields that identify one instance (e.g. `["id"]`). |
| `filter_fields` | string[] | no | `[]` | Fields a `list`/`search` may filter on. |
| `schema_version` | u32 | no | `1` | Must be `1`; the bridge rejects any other major at load. |
| `operations` | map | **yes** | — | Keyed by verb (`list`/`get`/`create`/`update`/`delete`/`search`/`execute`). Must be non-empty. |

### `OperationDeclaration` (values in `operations`)

| Field | Type | Default | Notes |
| --- | --- | --- | --- |
| `description` | string | `""` | Per-op description for `wylde_describe`. |
| `mcp_tool` | string | `""` | The MCP tool (from your `tools/list`) this verb calls. May be empty **only** for an `execute` op where every action supplies its own `mcp_tool`. |
| `destructive` | bool | `false` | `true` ⇒ `destructive_tool_access` tier + consent prompt; `false` ⇒ read, no consent. |
| `tier` | string | `"read"` | Advisory/reserved. Only `destructive` is enforced today; the string is kept for a richer future tier model. |
| `actions` | ActionDeclaration[] | `[]` | For `execute` ops: the legal `action` values. |
| `args_schema` | JSON | `{}` | Opaque JSON Schema surfaced to the LLM. **Not** validated against model args — the extension owns its own validation. |
| `response_schema` | JSON | `{}` | Opaque; same treatment. |

### `ActionDeclaration` (entries in an execute op's `actions[]`)

| Field | Type | Default | Notes |
| --- | --- | --- | --- |
| `name` | string | — | **Required**; the `action` value. Non-empty; unique within the op. |
| `description` | string | `""` | Shown to the model. |
| `mcp_tool` | string? | op's `mcp_tool` | Per-action tool override; falls back to the op's `mcp_tool`. |
| `destructive` | bool | `false` | Per-action destructive flag. |

**Validation** (`manifest.rs::validate_resources`): non-empty `resource_type`,
unique within the extension; `scope` in the three-value set; `schema_version == 1`;
non-empty `operations`; every key a known verb; every op bound to *some* `mcp_tool`
(or, for `execute`, per-action overrides covering all actions); action names
non-empty and unique. When the static `tools[]` mirror is present, every
`mcp_tool` is cross-checked against it at load; when absent (Rust extensions often
omit the mirror), the check defers to a runtime warn-once on first dispatch.

### Live example — Webcrawler

`Extensions/Webcrawler/mcp-server.json` declares its three read-only fetch tools
as one `execute` resource (`resource_type:"url"` → `ext:Webcrawler:url`):

```jsonc
"resources": [
  {
    "resource_type": "url",
    "display_name": "Web URL",
    "scope": "global",
    "schema_version": 1,
    "operations": {
      "execute": {
        "description": "Run a web action against a URL. 'action' selects fetch | scrape | extract.",
        "destructive": false,
        "tier": "read",
        "actions": [
          { "name": "fetch",   "mcp_tool": "fetch" },
          { "name": "scrape",  "mcp_tool": "scrape" },
          { "name": "extract", "mcp_tool": "extract" }
        ]
      }
    }
  }
]
```

The model then calls `wylde_execute("ext:Webcrawler:url", "fetch", {url:"…"})`,
which the harness reshapes into one `ext.tools.call{extension:"Webcrawler",
tool:"fetch", arguments:{…}}` hop to the running server. How that verb call
threads consent and the gateway egress allowlist:
[harness-api-reference.md](./harness-api-reference.md).

## `UiPanel` (entries in `ui_panels[]`)

| Field | Type | Notes |
| --- | --- | --- |
| `id` | string | **required**; unique within the extension. |
| `title` | string | **required**; tab label. |
| `icon` | string? | icon name or emoji. |
| `source` | PanelSource | **required**; `{ "kind": "iframe", "url": "…" }`. |

`source.kind` is only `"iframe"` today (`native_view` is deferred). **URLs must
be loopback** (`127.0.0.1`, `localhost`, `::1`) — anything else is rejected at
manifest load. Panel-only extensions (e.g. `Extensions/N8N/`) declare a no-op
`command`, `enabled:false`, and a panel; the bridge surfaces the panel without
spawning. Deep dive:
[../extending-wylde-extensions.md](../extending-wylde-extensions.md#ui-panels).

## `HealthConfig` (`health`)

| Field | Type | Default | Notes |
| --- | --- | --- | --- |
| `method` | string | `"ping"` | MCP method sent as the liveness probe. |
| `interval_s` | u64 | `30` | Seconds between probes. |
| `timeout_s` | u64 | `5` | Probe timeout; exceeding it marks the extension unhealthy. |

## Legacy `manifest.json` (Python extensions only)

Pre-MCP extensions also carry a `manifest.json` read by the Python bridge
(`Extensions/extension_bridge/contract.py`). It adds `handler` (module name,
default `handler`), `browser_extension_path`, a verbose `tools[]` with
`parameters[]` arrays, and per-tool overlays under `tools/<id>/manifest.json`.
The shim translates these into MCP. New Rust extensions **do not need it** —
`mcp-server.json` alone is the contract.

## Validation (what fails at load)

`manifest.rs` rejects: empty `name`; empty `command` for stdio transport;
`http` transport; non-loopback panel URLs; duplicate tool or panel ids; and the
`resources[]` faults listed above (bad scope, unknown verb, `schema_version != 1`,
unbound op, duplicate `resource_type`, `mcp_tool` absent from a present `tools[]`
mirror). Run `cargo test -p wylde-extension-bridge manifest` before shipping.

## Cross-links

- [writing-an-extension.md](./writing-an-extension.md) — quickstart; the 5-method MCP contract.
- [harness-api-reference.md](./harness-api-reference.md) — what capabilities gate, and how a `resources[]` verb call reaches your tool.
- [../extending-wylde-extensions.md](../extending-wylde-extensions.md) — UI-panel deep dive, full bridge action surface, extensions-vs-services.
- `rust/crates/wylde-extension-bridge/src/manifest.rs` — the authoritative parser.

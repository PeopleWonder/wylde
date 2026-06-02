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
`ingress.browser`

Webcrawler declares `egress.web`; Wylde_Study declares
`ingress.browser`, `egress.browser`. Declare honestly — see
[harness-api-reference.md](./harness-api-reference.md) for what they actually gate.

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
`http` transport; non-loopback panel URLs; duplicate tool or panel ids. Run
`cargo test -p wylde-extension-bridge manifest` before shipping.

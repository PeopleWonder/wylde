---
title: Harness API for extensions (calling back into Wylde)
audience: extension authors who need memory / chat / egress / consent
updated: 2026-06-02
status: reference; corrects the "extensions only call out, never in" claim in older docs
---

# Harness API for extensions

The older docs say extensions are sandboxed with "no direct IPC into other Wylde
services." **That is no longer true.** The canonical Rust extension
(`wylde-ext-webcrawler`) reaches the gateway over a named pipe to make its web
requests. The real model is: the bridge owns your *spawn and lifecycle*, but
nothing stops a Rust extension from opening a pipe to another Wylde service. The
actual guardrails are the **gateway egress allowlist** and the **per-tool
consent gate**, not transport isolation.

## The one call you need

A Rust extension links `wylde-shared` and calls any service action with:

```rust
use wylde_shared::ipc;

// signature: rust/crates/wylde-shared/src/ipc/client.rs:314
let data: serde_json::Value =
    ipc::call_action(service, action, payload).await?;   // Ok(data) | Err(IpcError{code,message})
```

`service` is a Wylde service name (the pipe is derived from it). The three
services an extension cares about:

| Service | Pipe | Use it for |
| --- | --- | --- |
| `wylde-gateway` | gateway pipe | outbound HTTP through the allowlist + audit log |
| `wylde-harness` | harness pipe | memory, chat, tool dispatch, consent |
| `wylde-extension-bridge` | `\\.\pipe\wylde-extension-bridge` | the `ext.*` admin surface (rarely from inside an extension) |

## Gateway egress — `wylde-gateway`

Don't open raw sockets. Forward through the gateway so the allowlist, kill
switch, and audit log apply (`wylde-ext-webcrawler/src/egress.rs:79`):

```rust
let payload = json!({
    "caller": "Webcrawler",        // MUST match the name that declared the egress key
    "dest": "web",                 // egress destination key from the manifest
    "method": "GET",
    "path": url,                   // full URL for wildcard destinations
    "headers": { "User-Agent": "Wylde-Webcrawler/1.0" },
    "timeout": 10.0
});
let resp = ipc::call_action("wylde-gateway", "egress.forward", payload).await?;
// resp: { status, body, headers }
```

A reachable gateway returning a **policy** denial (`egress_blocked`,
`egress_denied`) or an **upstream** failure must surface as an error — do not
retry with a direct request. Only a *transport* failure (gateway pipe down)
justifies a fallback, and log it loudly. `egress.rs` is the reference for this
classification.

## Bridge inference gate — `wylde-extension-bridge`

Don't POST to Ollama's `127.0.0.1:11434` directly. Route inference through the
**bridge**, which forwards — capability-checked, rate-limited, and audited — to
`wylde-ollama` (VRAM-broker lease + resident keep-alive'd model reuse +
per-request model swap). A direct Ollama call bypasses the broker lease, the
suspend/resume connection-resilience layer, and the policy gate.

**Capability:** declare `inference.local` in your `mcp-server.json`
`capabilities[]` (see [manifest-reference.md](./manifest-reference.md#inferencelocal--the-bridge-inference-gate)).
Without it, these calls return `capability_denied`.

```rust
// Embed — {extension, model?, input} -> {embeddings, model}
let payload = json!({
    "extension": "Wylde_Study",      // self-asserted; MUST be your extension name
    "model": "nomic-embed-text",     // omit to use the bridge's configured default
    "input": ["text to embed", "..."] // string or array of strings
});
let resp = ipc::call_action("wylde-extension-bridge", "inference.embed", payload).await?;
// resp: { embeddings: [[f32, ...], ...], model }

// Chat — {extension, model?, messages, options?, format?, keep_alive?}
let payload = json!({
    "extension": "Wylde_Study",
    "model": "llama3.2",
    "messages": [{"role": "user", "content": "..."}],
    "options": { "temperature": 0.2 },
    "format": "json"                 // optional; pass-through to Ollama
});
let resp = ipc::call_action("wylde-extension-bridge", "inference.chat", payload).await?;
// resp: { message: { role, content }, model, done, ... }
```

**Model selection / swap:** pass `model` per call. The bridge forwards it
verbatim; Ollama's keep-alive keeps the model resident and the broker accounts
for the lease, so swapping is just "ask for a different `model`". Omit `model`
to fall back to the bridge default (`WYLDE_BRIDGE_INFERENCE_{EMBED,CHAT}_MODEL`).

**Errors:** `capability_denied` (no `inference.local`), `inference_rate_limited`
(`details.retry_after_ms` says when to retry), `bad_request` (missing
`extension` / `input` / `messages`). Upstream errors pass through with their
original codes (`model_not_found`, `ollama_unreachable`, `vram_admission_denied`).

**Rate limit:** per-extension token bucket — `WYLDE_BRIDGE_INFERENCE_BURST`
(capacity, default 30) and `WYLDE_BRIDGE_INFERENCE_RPS` (refill/sec, default 5).

## Harness verbs — `wylde-harness`

The full set the harness pipe accepts (`rust/crates/wylde-harness/src/pipe.rs`,
`ALL_PIPE_ACTIONS`). Call them with `ipc::call_action("wylde-harness", <verb>, <payload>)`.

**Chat** — drive the model:
`chat.run_turn` (sync, returns final message) · `chat.start_turn` (async, returns
`turn_id`) · `chat.cancel` (`{turn_id}`) · `chat.stream_turn` · `chat.stream_tools`
(streaming).

**Tools** — reach the harness registry:
`tools.list` (live catalog) · `tools.run` (`{name, args?, device_tier?}`).

**Long-term memory:**
`memory.long_term.list` · `.save` (`{body, source?, importance?, tags?}`) ·
`.update` (`{id, …}`) · `.delete` (`{id}`) · `.history` (`{id}`) ·
`.search` (`{query, limit?, decay_days?}`).

**Workspaces:**
`memory.workspaces.list` · `.recent` (`{limit?}`) · `.get` (`{workspace_id}`) ·
`.get_mru_limit` · `.set_mru_limit` (`{limit}`) · `.get_persona`
(`{workspace_id}`) · `.set_persona` (`{workspace_id, text?}`) · `.delete`
(`{workspace_id}`).

**Consent** (see next section):
`consent.list` · `.set` (`{tool_id, decision}`) · `.respond` · `.clear`
(`{tool_id}`) · `.set_no_auth` (`{enabled}`) · `.reset` · `.stream_pending`.

The Python `Wylde_Study` handler reaches the *same* capabilities by importing
`Core.harness.memory.rag` directly in-process (it runs inside the venv with
`PYTHONPATH=${WYLDE_ROOT}`). That import path is legacy; new Rust extensions use
the pipe verbs above.

## Consent & tier gating

Tool dispatch (yours included, when invoked via `tools.run`) passes three gates
in `rust/crates/wylde-harness/src/tooling/`:

1. **Registry** — is the tool resolvable?
2. **Tier** — does the turn's `device_tier` permit a `destructive` tool?
3. **Consent** — has the user approved this tool? (`consent.rs`)

Consent outcomes are `Allow` / `Deny{reason}` / `Pending{prompt}`. Decisions
persist to `data/preferences/consent.json`:

```json
{ "no_auth": false, "tools": { "fs.write_file": "approved", "memory.long_term.delete": "denied" } }
```

`no_auth: true` is the power-user escape hatch (approve everything, no prompts);
default is `false`. One-time grants ("allow once") are honored but not persisted.
An extension that wraps a destructive capability should expect `Pending` on
first use and surface the prompt rather than treating it as failure.

## What the bridge does NOT give you

- No filesystem sandbox — a Rust extension has the process's full FS access.
  Capability declarations are advisory until a tier gate consumes them.
- No inbound calls from the harness beyond `tools/call` + `ping`.
- No `http` transport, resources, or prompts yet (MCP-roadmapped, not consumed).

## Cross-links

- [writing-an-extension.md](./writing-an-extension.md) — quickstart.
- [manifest-reference.md](./manifest-reference.md) — capabilities vocabulary.
- `rust/crates/wylde-shared/src/ipc/client.rs` — `call_action`.
- `rust/crates/wylde-harness/src/pipe.rs` — verb list + payloads.
- `rust/crates/wylde-ext-webcrawler/src/egress.rs` — egress reference impl.

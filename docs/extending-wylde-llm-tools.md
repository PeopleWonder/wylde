---
title: Extending Wylde — adding LLM-callable tools
audience: contributors adding new actions to the harness registry
authored: 2026-05-27
status: living reference
---

# Adding LLM-callable tools

## Executive summary

When you chat with Wylde's assistant, the language model can do more than
just talk back — it can use "tools" to take action: read a file, search
your notes, fetch a webpage, look something up in your graph. Each tool
is a small named function the model can call. The model decides when to
call it, picks the arguments, reads the result, and uses that to answer
your question. The catalogue of available tools is a plain Rust list that
sits next to the chat code; adding a new one is roughly thirty lines of
code and the model sees it immediately.

This is the easiest way to extend Wylde. It doesn't require touching the
GUI, doesn't require running a separate process, and doesn't require any
extension or plugin discovery. You write one file, register it in one
spot, and the same tool becomes callable by the LLM, by the GUI (through
the `tools.run` action), and — eventually — by external MCP clients.
The tier-gate (read-only vs. tool-use vs. destructive) is enforced for
you, the alias map resolves the model's free-form name choices, and a
deferred-stub mechanism lets you advertise tools that aren't built yet
without confusing the model.

This doc walks through what each piece of metadata means, gives you a
hello-world tool you can lift verbatim, and lists the anti-patterns that
have bitten contributors before. It assumes you've read
[extending-wylde.md](./extending-wylde.md) for the audience model and
have a working Rust toolchain.

## How it works

### What you'll be touching

Three places:

* `rust/crates/wylde-harness/src/tooling/tools/<your-group>.rs` — your new file.
* `rust/crates/wylde-harness/src/tooling/tools/mod.rs` — add `pub mod
  your_group;` and a `your_group::register(reg);` line in `register_all`.
* `rust/crates/wylde-harness/Cargo.toml` — only if you pull in a new dep.

That's it. The pipe surface (`tools.list` / `tools.run`) re-reads the registry
on every call, so a new tool is callable from the GUI as soon as the harness
restarts. The model sees it on the next turn because the turn loop pulls the
catalog from the registry per turn.

### Anatomy of a tool entry

```rust
use serde_json::{json, Value};
use wylde_shared::ipc::IpcError;
use crate::tooling::registry::{entry_active, param, param_default, Registry};

pub fn register(reg: &mut Registry) {
    reg.insert(entry_active(
        "echo",                         // canonical id (snake_case)
        "demo.echo",                    // dotted name (model sees this)
        "demo",                         // group label
        "Echo the input string back unchanged. \
         Useful for verifying the tool plumbing without side effects.",
        vec![                           // parameter schema
            param("text", "string", true, "the text to echo"),
            param_default("upper", "boolean", "uppercase the result",
                          json!(false)),
        ],
        false,                          // destructive: false → read-only-safe
        |args, _cfg| async move {
            let text = args.get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| IpcError::new("bad_request", "'text' is required"))?;
            let upper = args.get("upper")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let out = if upper { text.to_uppercase() } else { text.to_string() };
            Ok(json!({ "status": "success", "echo": out }))
        },
    ));
}
```

Every field matters. Let's go through them.

### `id` and `name`

The canonical id is `snake_case`. The dotted name is `group.tool_id`. The
registry's alias map auto-generates the inverses (`demo_echo` and `demo.echo`)
so the model can emit any reasonable shape and the salvage parser resolves it
to the canonical id. Don't try to outsmart the alias map — pick one canonical
`snake_case` id, one dotted name, and let the registry do the rest.

### `group`

A free-form string. The model sees it in the catalog as a coarse classifier
(`fs`, `memory`, `time`, `rag`, …). Pick an existing group if your tool fits
one; only create a new group if you're adding ≥2 tools that genuinely share a
concern.

### `description`

This is shown to the model as part of the tool catalog. It should be one
paragraph, written in the imperative, that tells the model **what the tool
does and when to use it**. Skip the implementation details. Wording matters:
"Echo the input string back unchanged" tells the model "this is a no-op verifier"
better than "calls a function that returns its argument."

Audience-shaped wording: today every audience sees the same description. When
per-audience overrides land, you'll add `gui_description` and `mcp_description`
fields. For now, optimise the wording for the model.

### `parameters`

A JSON-Schema-ish list. Use the helpers:

* `param(name, type, required, description)` for required-or-by-default fields.
* `param_default(name, type, description, default_value)` for fields with a
  default.

Types: `"string"`, `"number"`, `"integer"`, `"boolean"`, `"object"`, `"array"`.
The shape is wire-compatible with the Python `tool_registry` manifests for
parity.

### `destructive: bool`

The most important metadata field. `true` means the tool is **denied** on the
`tool_use` device tier — it requires `destructive_tool_access`. Use `true` for
anything that writes files, sends network requests, mutates persistent state,
or triggers external side effects. `false` is the right default for pure reads.

The tier gate (`tooling/runner.rs::check_registry_tier`) enforces this. The
GUI hides destructive tools from non-elevated UIs. Extensions that want to
call destructive Wylde actions need their own elevated capability declaration.

### The handler closure

Signature: `Fn(Value, &'static Config) -> Future<Output = Result<Value,
IpcError>>`. Take the JSON args, the harness `Config`, return either an
`Ok(Value)` (success) or an `Err(IpcError)` (handler-level failure — wire
shape error or downstream service unreachable).

**Soft errors go in the `Ok` body**, not the `Err`. The convention is
`{status: "success", …}` or `{status: "error", error: "..."}`. Look at
`time_tools::run_time_format` for a clean example — it returns `Ok` on bad
input with `status: "error"` so the model can correct itself, rather than
escalating to a wire-level failure.

`Err(IpcError)` is for the cases the model can't recover from on its own —
the upstream service is down, the registry is missing the tool, the wire format
is corrupt. The tier gate also returns `Err` because tier denials surface as
`ToolErrorReason::TierReadOnly` to the turn loop.

## How to extend

### Hello world, end to end

Create `rust/crates/wylde-harness/src/tooling/tools/demo.rs`:

```rust
//! `demo.*` — minimal tool group for the extending-wylde tutorial.

use serde_json::{json, Value};
use wylde_shared::ipc::IpcError;
use crate::tooling::registry::{entry_active, param, Registry};

pub fn register(reg: &mut Registry) {
    reg.insert(entry_active(
        "echo",
        "demo.echo",
        "demo",
        "Echo the input string back unchanged.",
        vec![param("text", "string", true, "the text to echo")],
        false,
        |args, _| async move { run_echo(args).await },
    ));
}

async fn run_echo(args: Value) -> Result<Value, IpcError> {
    let Some(text) = args.get("text").and_then(Value::as_str) else {
        return Ok(json!({
            "status": "error",
            "error": "'text' is required (string)",
        }));
    };
    Ok(json!({ "status": "success", "echo": text }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn echo_round_trips() {
        let out = run_echo(json!({"text": "hi"})).await.unwrap();
        assert_eq!(out["status"], "success");
        assert_eq!(out["echo"], "hi");
    }

    #[tokio::test]
    async fn echo_rejects_missing_text() {
        let out = run_echo(json!({})).await.unwrap();
        assert_eq!(out["status"], "error");
    }

    #[test]
    fn register_inserts_demo_echo() {
        let mut reg = Registry::empty();
        register(&mut reg);
        assert!(reg.lookup("demo.echo").is_some());
        assert!(reg.lookup("demo_echo").is_some());  // alias map
        assert!(reg.lookup("echo").is_some());        // canonical id
    }
}
```

Then in `rust/crates/wylde-harness/src/tooling/tools/mod.rs`, add:

```rust
pub mod demo;
```

and inside `register_all`:

```rust
pub fn register_all(reg: &mut Registry) {
    demo::register(reg);          // <-- new
    diff::register(reg);
    fs::register(reg);
    // ... etc
}
```

Insertion order doesn't affect correctness — the alias map prefers canonical
ids over aliases, and `canonical_ids()` returns sorted output regardless. Keep
the list alphabetical for diff readability.

### Run the gates

```
cargo test -p wylde-harness tooling::tools::demo
cargo clippy -p wylde-harness --all-targets -- -D warnings
```

If both pass, you're done. The next `chat.run_turn` will see `demo.echo` in
the catalog. The GUI's `tools.list` pipe verb returns it. The model can call
it as `demo.echo`, `demo_echo`, or `echo` — all three resolve.

### Deferred handlers

If you want to catalogue a tool but the implementation isn't ready, use
`entry_deferred` instead of `entry_active`:

```rust
use crate::tooling::registry::entry_deferred;

reg.insert(entry_deferred(
    "long_form_summarise",
    "summarise.long_form",
    "summarise",
    "Summarise a long document using a chunked map-reduce strategy.",
    vec![param("text", "string", true, "document text")],
    false,
    "11",                               // phase tag
    "depends on the visual layer port", // human-readable reason
));
```

The model sees the tool in the catalog. When it calls, the dispatcher returns
`phase_11_deferred` with the reason — the model interprets that as "registered
but not yet implemented" and picks a different tool. This is **always** better
than letting the model emit a tool name that resolves to `unknown_tool`.

## Gotchas

### Anti-patterns

* **Don't bypass the registry.** If your action needs to be invocable, register
  it. Don't add a side door through `dispatch.rs` or the pipe layer that
  short-circuits the catalog. The tier gate, alias map, and audience metadata
  all live in the registry; bypassing it breaks all three.
* **Don't reach into other crates' state.** Tool handlers get a `&'static
  Config` and their args. If you need memgraph data, call the registered
  `meta.graph_query` tool (or the in-process module via the harness's memory
  layer). Don't open a second pipe to `wylde-memgraph` from a tool handler —
  that path duplicates state ownership.
* **Don't use `OnceLock` for env-driven config.** See
  `~/.claude/projects/.../memory/feedback_avoid_oncelock_for_test_env.md`. Tests
  override `WYLDE_DATA_DIR` per test; caching it the first time sticks the
  wrong value for the rest of the suite. Re-read env per call.
* **Don't log secrets.** Tool handlers run with the harness's full
  capabilities. The model can pass arbitrary strings as args. Log args at
  `debug!` only, and never log full file contents at `info!`. The voice
  pipeline has the lowest tolerance for this — keep transcripts out of logs
  by default.

### What you got for free

* **GUI invocation** via `tools.run` over `\\.\pipe\wylde-harness`.
* **MCP exposure** if/when the bridge starts re-exporting internal tools
  (designed, not built — see the extensions doc).
* **Tier gate enforcement** on every call.
* **Alias resolution** for the model's free-form name choices.
* **Catalogue inspection** via `tools.list` and the `tool_search` meta tool.
* **A deferred stub** if you register the tool name before the handler exists.

## Cross-links

* [extending-wylde.md](./extending-wylde.md) — overview, audience model.
* [extending-the-gui.md](./extending-the-gui.md) — how the GUI invokes tools.
* `rust/crates/wylde-harness/src/tooling/tools/time_tools.rs` — the smallest
  active tool group in the tree; lift its shape.
* `rust/crates/wylde-harness/src/tooling/registry.rs` — the data model.

---

*If this doc is your starting point, the next step is to write the new tool
and put it through `cargo test -p wylde-harness`. Don't write a doc, don't
write a design memo — write the handler, write the tests, watch them pass.*

---
title: Extending Wylde — adding an in-box service
audience: contributors building net-new Wylde services
authored: 2026-05-27
status: living reference
---

# Adding a Wylde service

## Executive summary

Wylde is built out of a handful of small programs — one for the chat
brain, one for the local LLM, one for voice, one for the network gateway,
and so on. Each one runs as its own process, has its own job, and talks
to the others through Windows named pipes (the local equivalent of a
network socket). When you add a "service," you're adding a new program
of this kind: it gets a name, a pipe, gets started automatically when
Wylde boots, gets watched for crashes, and gets shut down cleanly when
the user quits.

This is the heaviest extension surface in Wylde. A service makes sense
when you're wrapping a long-running runtime (like the way Memgraph wraps
Neo4j), brokering a shared resource (like the VRAM broker), enforcing a
trust boundary (like the gateway), or isolating a crash domain so a JVM
or GPU driver going sideways doesn't take the rest of Wylde down. For
most "I want to add capability X" requests, the right answer is a tool
or an extension — not a service. Tools live inside the chat brain;
extensions live in their own sandboxed process and can be written in any
language. Services are reserved for the trusted middle.

This doc explains what a service consists of (a crate, a manifest, an
action contract, a pipe, a lifecycle slot, the four-phase startup
sequence), walks you through building a minimal `wylde-hello` service
end to end, and lists the conventions that the `wylde_check` linter
enforces so you don't have to remember them. If you're not sure you
need a service, you probably don't — start with
[extending-wylde-llm-tools.md](./extending-wylde-llm-tools.md) and
escalate if it's not enough.

## How it works

### When you actually want a service

Some heuristics:

* **You're wrapping a runtime.** Memgraph wraps Neo4j. Ollama wraps the
  Ollama HTTP runtime. The Trainer wraps LLaMA-Factory. If you're adding a
  long-running external dependency, it gets a service.
* **You're brokering a resource.** The VRAM broker arbitrates GPU memory
  across the harness, voice, and trainer. A new physical resource
  (USB peripheral, GPU bank, etc.) is a candidate.
* **You're enforcing a boundary.** The Gateway terminates external HTTP.
  The device-gate gates authentication. VPN gates network identity. New
  cross-cutting boundaries are service-shaped.
* **You're isolating failure.** When a crash should not take the harness
  down, splitting into a service is the right move. The Memgraph supervisor
  is in Python specifically so a JVM crash doesn't bring the Rust harness
  with it.

When **none** of those apply, you probably want a tool or an extension.

### What a service consists of

Every Wylde service has the same shape. `wylde_check` rules enforce all of
this; deviations are bugs.

1. A **crate** at `rust/crates/wylde-<service>/` with `lib.rs` + `main.rs`.
2. A **manifest** at `data/manifests/wylde-<service>.json`, written at
   startup by `ManifestWriter::write` and heartbeated every 60 s.
3. An **action contract** at `data/contracts/actions/<service>.json` declaring
   the verbs the service registers on its pipe. The GUI's `pipeAction`
   call sites are linted against it (rule 9).
4. A **pipe** at `\\.\pipe\wylde-<service>` opened by
   `wylde_shared::ipc::serve`. Msgpack-over-pipe envelope, v1 wire shape.
5. **Lifecycle integration** — a slot in
   `Core/Lifecycle/daemon_state/_services_*.py` that spawns the binary and
   tracks its PID.
6. The four-phase startup sequence: `configure_logging` → `write_manifest` →
   `start_heartbeat` → `serve_loop` (with `mark_serve_loop_entered()`
   attestation).
7. A SIGTERM/Ctrl-C shutdown handler that marks the manifest stopped.

## How to extend

### Walkthrough: hello-world-service

Let's build `wylde-hello`, a service that exposes `hello.greet` on
`\\.\pipe\wylde-hello`. ~50 lines total.

### 1. Create the crate

```
rust/crates/wylde-hello/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── main.rs
    └── pipe.rs
```

`Cargo.toml`:

```toml
[package]
name = "wylde-hello"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[[bin]]
name = "wylde-hello"
path = "src/main.rs"

[lib]
path = "src/lib.rs"

[build-dependencies]
wylde-prebuild-guard = { path = "../../build-support/wylde-prebuild-guard" }

[dependencies]
wylde-shared = { path = "../wylde-shared" }
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "signal"] }
serde_json.workspace = true
tracing.workspace = true
anyhow.workspace = true
```

`build.rs` at the crate root:

```rust
fn main() {
    wylde_prebuild_guard::run();
}
```

### 2. The library

`src/lib.rs`:

```rust
pub mod pipe;
```

`src/pipe.rs`:

```rust
//! `hello.*` actions on `\\.\pipe\wylde-hello`.

use serde_json::{json, Value};
use wylde_shared::ipc::{register_action_with_meta, Reply};

const HANDLER_MODULE: &str = "wylde_hello::pipe";

pub fn install() {
    register_action_with_meta(
        "hello.greet",
        |payload: Value| async move { handle_greet(payload).await },
        "Return a greeting. Payload {name?}; default greets the world.",
        HANDLER_MODULE,
    );
}

async fn handle_greet(payload: Value) -> Reply {
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("world");
    Reply::ok(json!({ "greeting": format!("hello, {name}") }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn greets_default_world() {
        let reply = handle_greet(json!({})).await;
        assert!(reply.ok);
        assert_eq!(reply.data["greeting"], "hello, world");
    }

    #[tokio::test]
    async fn greets_supplied_name() {
        let reply = handle_greet(json!({"name": "wylde"})).await;
        assert!(reply.ok);
        assert_eq!(reply.data["greeting"], "hello, wylde");
    }
}
```

### 3. The binary

`src/main.rs`:

```rust
use std::time::Duration;
use anyhow::Result;
use serde_json::json;
use tracing::Level;
use wylde_shared::ipc;
use wylde_shared::logging::configure_logging;
use wylde_shared::manifest::ManifestWriter;

const SERVICE_NAME: &str = "wylde-hello";

#[tokio::main]
async fn main() -> Result<()> {
    configure_logging(Some(SERVICE_NAME), Level::INFO);
    tracing::info!("wylde-hello: starting");

    let manifest = ManifestWriter::write(
        SERVICE_NAME,
        Some(0),
        "demo",
        "Greeting service — minimal example for the extending-wylde-services tutorial.",
        json!({
            "dashboard": { "label": "hello", "icon": "smile", "color": "green" },
        }),
        Some("rust:wylde-hello"),
    )?;
    let _heartbeat = manifest.start_heartbeat(Duration::from_secs(60));

    wylde_hello::pipe::install();
    tracing::info!("wylde-hello: actions registered; opening pipe at \\\\.\\pipe\\wylde-hello");

    let serve_fut = ipc::serve(SERVICE_NAME, None);
    tokio::select! {
        result = serve_fut => {
            if let Err(e) = result {
                tracing::error!("wylde-hello: serve() exited: {e}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("wylde-hello: ctrl-c received, shutting down");
        }
    }

    if let Err(e) = manifest.mark_stopped() {
        tracing::warn!("wylde-hello: mark_stopped failed: {e}");
    }
    Ok(())
}
```

### 4. Add to the workspace

In `rust/Cargo.toml`, under `members`:

```toml
members = [
    # ... existing entries ...
    "crates/wylde-hello",
]
```

### 5. Lifecycle integration

Add a slot in `Core/Lifecycle/daemon_state/_services_basic.py` (or the
appropriate `_services_*.py` for your service's category). The pattern,
abbreviated:

```python
def start_hello(state: DaemonState) -> None:
    impl = resolve_impl("hello", default="rust")
    if impl == "rust":
        binary = state.rust_root / "target" / "release" / "wylde-hello.exe"
        proc = _spawn_rust(state, binary)
    else:
        # No Python forward shim for this hello-world example — Rust only.
        raise NotImplementedError("wylde-hello has no Python impl")
    state._hello_proc = proc
```

…plus matching `stop_hello`, plus a slot registration in `_services.py`.
Look at `_services_basic.py::start_device_gate` for the canonical pattern;
copy it.

### 6. Manifest action contract

Write `data/contracts/actions/wylde-hello.json`:

```json
{
  "service": "wylde-hello",
  "version": 1,
  "actions": [
    {
      "name": "hello.greet",
      "handler": "wylde_hello::pipe::handle_greet",
      "description": "Return a greeting. Payload {name?}; default greets the world."
    }
  ]
}
```

The harness writes its own contract programmatically via
`ipc::write_action_contract`; for new services the JSON-by-hand path is the
norm until the helper generalises.

### 7. Run the gates

```
cargo build -p wylde-hello --release
cargo test -p wylde-hello
cargo clippy -p wylde-hello --all-targets -- -D warnings
uv run python -m Core.harness.dev.wylde_check
```

`wylde_check` will scream if you missed anything — pipe name convention
(rule 17), startup sequence (rule 18), shutdown handler (rule 19), action
contract present (rule 9 from the GUI side once a GUI consumer lands).

### Strangler-fig (historical)

The strangler-fig migration pattern carried every service from Python to
Rust between 2026-05 and 2026-06; the full-Rust cutover (R6, 2026-06-10)
deleted the last Python trees, so there is nothing left to strangle. The
pattern is kept in `wylde-repo-organization.md` §8 for the record —
byte-shape parity tests (`rust/tests/parity/`), env-var-gated defaults,
soak, then delete. The Phase 5.D parity gate catching a salvage-parser
edge case the unit tests missed is the cautionary tale worth remembering.

New services are net-new Rust: go straight to `rust:wylde-<name>` in the
entry_point. The `WYLDE_<SERVICE>_IMPL` env vars are still parsed for
shape consistency, but `=python` only logs a warning.

## Gotchas

### Conventions that catch you if you forget

* **Pipe name pattern** (`wylde_check` rule 17): `^wylde-[a-z][a-z0-9-]*$`.
* **One manifest write per service** (rule 2): `ManifestWriter::write` is
  the single entry. Don't `update_manifest` from inside the service —
  heartbeats handle it.
* **`mark_serve_loop_entered()` after pipe open** (rule 18): the four-phase
  sequence. The harness and Memgraph manifests have known-deferred
  warnings on this — clearing them is on the cleanup-slice punch list.
* **No external subprocesses from `src/`** (rule 14): if you need to spawn
  a sidecar (e.g. wrapping a JVM), put it in `build-support/` or use a
  thin Python supervisor (like `Core/Memgraph/`). Direct `std::process::Command`
  calls from inside `rust/crates/<crate>/src/` are denied.
* **Shutdown handler updates the manifest** (rule 19): `manifest.mark_stopped()`
  in the `ctrl_c` arm of the select loop. Don't trust the orphan reaper to
  catch a clean shutdown — the reaper is the safety net, not the path.
* **Lifecycle test sandbox** (rule 32): if you write tests under
  `Core/Lifecycle/tests/` that interact with manifests, inherit the autouse
  sandbox fixture. Don't read `data/manifests/` directly. Synthetic test
  PIDs have hit live `wylde-gateway.exe` PIDs before; the watchdog catches
  it now but rule 32 prevents the regression at lint time.
* **`OnceLock` for env-driven paths is a trap.** See
  `~/.claude/projects/.../memory/feedback_avoid_oncelock_for_test_env.md`.
  Tests override `WYLDE_DATA_DIR`; cached paths stick. Re-read env per
  call.

### How services and tools relate

A service exposes pipe verbs. A tool entry in the harness registry can
delegate to a service via `wylde_shared::ipc::call_action`:

```rust
// inside a tooling/tools/hello.rs file in the harness:
use crate::tooling::registry::{entry_active, param, Registry};

pub fn register(reg: &mut Registry) {
    reg.insert(entry_active(
        "hello_greet",
        "hello.greet",
        "hello",
        "Greet a name. Calls the wylde-hello service.",
        vec![param("name", "string", false, "who to greet (default: world)")],
        false,
        |args, cfg| async move {
            wylde_shared::ipc::call_action(&cfg.hello_service, "hello.greet", args).await
        },
    ));
}
```

This is the pattern for "service + tool" parity: the service owns the verb;
the tool re-exposes it to the LLM and GUI via the registry. The harness
config holds the service-name lookup (e.g. `cfg.hello_service =
"wylde-hello"`) so tests can override it.

Not every service needs a corresponding tool — `wylde-vram-broker` doesn't
(the model never directly leases VRAM; the ollama service does that on its
behalf). But for anything user-facing, the tool wrapper is what makes it
discoverable.

### What you got for free

* **Lifecycle supervision** — start at boot, restart on crash (configurable),
  reap on shutdown.
* **Manifest dashboard entry** — appears in the GUI's `SystemHealth` panel.
* **Pipe enumeration** — `list_pipes` in the GUI's `pipe.rs` includes you.
* **Orphan reaping** — if the service crashes and leaves a manifest behind,
  the next daemon shutdown's reaper will clean it up.
* **Test sandboxing** — once you add `Core/<service>/tests/conftest.py`
  inheriting the autouse fixture (or use the rust-side `#[cfg(test)]` blocks).

## Cross-links

* [extending-wylde.md](./extending-wylde.md) — overview, three pillars.
* [extending-wylde-llm-tools.md](./extending-wylde-llm-tools.md) — when
  a tool would be enough.
* [extending-wylde-extensions.md](./extending-wylde-extensions.md) — when
  the work belongs out-of-box.
* `docs/wylde-repo-organization.md` — full conventions reference.
* `docs/manifest_ownership.md` — manifest semantics.
* `docs/wylde_check_rules.md` — rule list.

---

*A new service is real architectural surface. Default to a tool; promote to
a service only when the heuristics above push you that way.*

//! Gateway HTTP parity: Python `Gateway.run` vs `wylde-gateway.exe`.
//!
//! Both gateways are spun up simultaneously on different ports
//! (`WYLDE_GATEWAY_PORT`); each case is fired at both and the responses are
//! diffed.
//!
//! Cases are either `gate: true` (a divergence fails the test — this route
//! is claimed to be at parity) or `gate: false` (a divergence is reported
//! as an informational finding only).
//!
//! ## What is gated
//!
//! The gate set is the **route intersection** — every route that exists on
//! *both* sides. The Python HTTP surface is exactly `Gateway/routes/*.py`;
//! the Rust port mirrors each of those under
//! `rust/crates/wylde-gateway/src/routes/*.rs` and additionally exposes a
//! couple of Rust-only routes (`/api/memory`, `/api/workspaces`). Those
//! Rust-only routes are intentional asymmetry — see `README.md`
//! ("Rust-only surface") — and are NOT gated; gating a route Python never
//! serves would always fail.
//!
//! Gated here: `/health`, the chat surface (`POST /api/chat`,
//! `POST /api/chat/generate`, `POST /api/chat/run_turn`), `/api/models`,
//! `/api/voice`, `/api/devices`, `/api/push`, `/api/link`, `/api/images`,
//! `/api/settings`, `/api/egress`, `/api/rag`, `/api/tools`,
//! `/extensions`, `/api/conversations`, `/api/prompts`, and the MCP
//! endpoint `/mcp` — the full route intersection. `/api/rag`,
//! `/api/tools` and `/extensions` were promoted from informational
//! probes once the Python side was brought onto the same transport /
//! envelope as the Rust port; `/api/conversations` and `/api/prompts`
//! followed once Python's Gateway grew the matching routers (see
//! `README.md`). The remaining probes are the framework-default 404
//! (`unknown_route`) and the GUI error-capture sink (`dev_gui_error`)
//! — the latter is ungated because a valid event triggers a filesystem
//! append to `logs/gui_errors.jsonl`, a write side-effect the probe
//! reports informationally rather than gating on.
//!
//! The four `mcp_*` cases fire `POST /mcp` with no Bearer token: both
//! implementations gate the MCP endpoint behind `require_device`, so
//! each rejects with an identical `401 missing_token` envelope before
//! any harness pipe call — the same path `chat_run_turn` exercises.
//!
//! SSE routes (`/api/chat`, `/api/chat/generate`) are diffed on their
//! event sequence after the volatile upstream-error `message` is
//! normalized out — see [`COMMON_VOLATILE`].

#![cfg(feature = "parity")]

use std::net::TcpListener;
use std::time::Duration;

use wylde_parity::diff;
use wylde_parity::http::{self, HttpCase};
use wylde_parity::paths;
use wylde_parity::proc;

/// A gateway parity case: the request, the volatile fields to ignore, and
/// whether a divergence should fail the test.
struct GwCase {
    http: HttpCase,
    volatile: &'static [&'static str],
    gate: bool,
}

/// Volatile fields present across the gateway response shapes.
///
/// A response can land in any of four shapes, and a path that does not
/// resolve is a no-op (see `diff::normalize`), so one list covers them
/// all:
///
/// * success envelope — `{ok: true, data: {...}}` (health carries a `ts`).
/// * canonical nested failure — `{ok: false, error: {code, message}}`
///   (Rust `failure()`; also the shape Wylde's auth dependencies build).
/// * Python `proxy_core` flat failure — `{ok, error, message, code}`,
///   where `message` sits at the top level.
/// * FastAPI's default `HTTPException` handler — `{detail: <envelope>}`.
///
/// SSE error frames (`event: error`) carry a volatile upstream-connection
/// `message`; the image-library listing carries a per-file `created_at`
/// (filesystem mtime — stable between the two reads, but a float whose
/// last digits can differ between Python's `st_mtime` and Rust's
/// `as_secs_f64()`).
const COMMON_VOLATILE: &[&str] = &[
    "body.data.ts",
    "body.error.message",
    "body.error.details",
    "body.message",
    "body.detail.error.message",
    "body.detail.error.details",
    "body.<sse-events>.*.data.message",
    "body.data.images.*.created_at",
];

fn cases() -> Vec<GwCase> {
    vec![
        // ── Gated: the route intersection (peripheral + chat surface) ───
        //
        // `/health` is public on both implementations and carries no
        // external dependency.
        GwCase {
            http: HttpCase {
                name: "health",
                method: "GET",
                path: "/health",
                body: None,
                headers: &[],
            },
            volatile: COMMON_VOLATILE,
            gate: true,
        },
        // `POST /api/chat` — Ollama SSE proxy. Whether Ollama is up or
        // down, both implementations re-emit a single `event: error`
        // frame (`http_<status>` if up + model unknown, `transport` if
        // down); the volatile connection `message` is normalized out.
        GwCase {
            http: HttpCase {
                name: "chat",
                method: "POST",
                path: "/api/chat",
                body: Some(serde_json::json!({ "model": "wylde-parity-probe" })),
                headers: &[],
            },
            volatile: COMMON_VOLATILE,
            gate: true,
        },
        // `POST /api/chat/generate` — the raw-prompt Ollama SSE proxy.
        GwCase {
            http: HttpCase {
                name: "chat_generate",
                method: "POST",
                path: "/api/chat/generate",
                body: Some(serde_json::json!({
                    "model": "wylde-parity-probe",
                    "prompt": "parity",
                })),
                headers: &[],
            },
            volatile: COMMON_VOLATILE,
            gate: true,
        },
        // `POST /api/chat/run_turn` — harness-pipe driver, device-tier
        // gated. Fired with no Bearer token: both sides must reject at
        // the auth layer before touching the harness pipe.
        GwCase {
            http: HttpCase {
                name: "chat_run_turn",
                method: "POST",
                path: "/api/chat/run_turn",
                body: Some(serde_json::json!({
                    "user_message": "parity",
                    "conversation_id": "parity-conv",
                })),
                headers: &[],
            },
            volatile: COMMON_VOLATILE,
            gate: true,
        },
        // `GET /api/models` — Ollama installed-model proxy.
        GwCase {
            http: HttpCase {
                name: "models_list",
                method: "GET",
                path: "/api/models",
                body: None,
                headers: &[],
            },
            volatile: COMMON_VOLATILE,
            gate: true,
        },
        // `GET /api/voice/health` — proxies the Voice service pipe.
        GwCase {
            http: HttpCase {
                name: "voice_health",
                method: "GET",
                path: "/api/voice/health",
                body: None,
                headers: &[],
            },
            volatile: COMMON_VOLATILE,
            gate: true,
        },
        // `GET /api/devices` — device-gate management surface.
        GwCase {
            http: HttpCase {
                name: "devices_list",
                method: "GET",
                path: "/api/devices",
                body: None,
                headers: &[],
            },
            volatile: COMMON_VOLATILE,
            gate: true,
        },
        // `GET /api/push/pending` — peer push drain via the VPN pipe.
        GwCase {
            http: HttpCase {
                name: "push_pending",
                method: "GET",
                path: "/api/push/pending?public_key=wylde-parity-key",
                body: None,
                headers: &[],
            },
            volatile: COMMON_VOLATILE,
            gate: true,
        },
        // `GET /api/link/status` — WyldeLink tunnel state proxy.
        GwCase {
            http: HttpCase {
                name: "link_status",
                method: "GET",
                path: "/api/link/status",
                body: None,
                headers: &[],
            },
            volatile: COMMON_VOLATILE,
            gate: true,
        },
        // `GET /api/images/library` — local image library listing (no
        // image-gen service dependency; reads `data/images/` on disk).
        GwCase {
            http: HttpCase {
                name: "images_library",
                method: "GET",
                path: "/api/images/library",
                body: None,
                headers: &[],
            },
            volatile: COMMON_VOLATILE,
            gate: true,
        },
        // `GET /api/settings/ollama` — Ollama runtime config (local file).
        GwCase {
            http: HttpCase {
                name: "settings_ollama",
                method: "GET",
                path: "/api/settings/ollama",
                body: None,
                headers: &[],
            },
            volatile: COMMON_VOLATILE,
            gate: true,
        },
        // `GET /api/egress/destinations` — configured egress upstreams.
        GwCase {
            http: HttpCase {
                name: "egress_destinations",
                method: "GET",
                path: "/api/egress/destinations",
                body: None,
                headers: &[],
            },
            volatile: COMMON_VOLATILE,
            gate: true,
        },
        // ── Gated: both-sides routes promoted from informational probes ─
        //
        // `/api/rag`, `/api/tools` and `/extensions` exist on both sides
        // and are now gated. `rag` proxies the harness pipe on both
        // ports — an unreachable harness yields the same 503 envelope.
        // `tools` reached parity once Python's `GET /api/tools` was moved
        // off its in-process registry import onto the harness `tools.list`
        // pipe action (the transport the Rust port already uses).
        // `/extensions` reached parity once both ports were moved onto
        // the `wylde-extension-bridge` pipe (wave 2i): that service is
        // not spawned in the sandbox, so each folds the pipe-transport
        // fault onto the same `503 extension_bridge_unavailable`.
        GwCase {
            http: HttpCase {
                name: "rag_collections",
                method: "GET",
                path: "/api/rag/collections",
                body: None,
                headers: &[],
            },
            volatile: COMMON_VOLATILE,
            gate: true,
        },
        GwCase {
            http: HttpCase {
                name: "tools_list",
                method: "GET",
                path: "/api/tools",
                body: None,
                headers: &[],
            },
            volatile: COMMON_VOLATILE,
            gate: true,
        },
        GwCase {
            http: HttpCase {
                name: "extension_call",
                method: "GET",
                path: "/extensions/parity/probe",
                body: None,
                headers: &[],
            },
            volatile: COMMON_VOLATILE,
            gate: true,
        },
        // ── Gated: the conversations + prompts ports ────────────────────
        //
        // `/api/conversations` and `/api/prompts` were Rust-only until
        // Python's Gateway grew matching routers (`Gateway/routes/
        // conversations.py`, `prompts.py`). Every verb gates on a
        // device-gate Bearer token; fired with NO token here so each side
        // rejects identically at the auth layer (`401 missing_token`)
        // without depending on a live device-gate pipe — the same path
        // `chat_run_turn` exercises. The token-*present* path is NOT
        // byte-equivalent and is deliberately left ungated: Python's
        // `require_device` collapses any device-gate fault into a single
        // `503 device_gate_unavailable`, while the Rust `authorize` passes
        // the raw device-gate pipe error straight through (HTTP 502 + the
        // pipe's own error code). That divergence lives in the shared auth
        // layer, not these routers.
        GwCase {
            http: HttpCase {
                name: "conversations_list",
                method: "GET",
                path: "/api/conversations",
                body: None,
                headers: &[],
            },
            volatile: COMMON_VOLATILE,
            gate: true,
        },
        GwCase {
            http: HttpCase {
                name: "prompts_list",
                method: "GET",
                path: "/api/prompts",
                body: None,
                headers: &[],
            },
            volatile: COMMON_VOLATILE,
            gate: true,
        },
        // ── Gated: the MCP server surface ───────────────────────────────
        //
        // `POST /mcp` is the v1 Model Context Protocol endpoint (Streamable
        // HTTP transport). It gates on `require_device`, so a request with
        // no Bearer token is rejected at the auth layer — identically on
        // both implementations — before any harness pipe call. Each case
        // carries a well-formed JSON-RPC body so the request is correct in
        // every respect except the missing credential; the four methods
        // (`initialize`, `tools/list`, `tools/call`, `resources/list`)
        // mirror the surface a real MCP client drives once authenticated.
        GwCase {
            http: HttpCase {
                name: "mcp_initialize",
                method: "POST",
                path: "/mcp",
                body: Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                })),
                headers: &[],
            },
            volatile: COMMON_VOLATILE,
            gate: true,
        },
        GwCase {
            http: HttpCase {
                name: "mcp_tools_list",
                method: "POST",
                path: "/mcp",
                body: Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/list",
                })),
                headers: &[],
            },
            volatile: COMMON_VOLATILE,
            gate: true,
        },
        GwCase {
            http: HttpCase {
                name: "mcp_tools_call_smoke",
                method: "POST",
                path: "/mcp",
                body: Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "tools/call",
                    "params": { "name": "git_status", "arguments": {} },
                })),
                headers: &[],
            },
            volatile: COMMON_VOLATILE,
            gate: true,
        },
        GwCase {
            http: HttpCase {
                name: "mcp_resources_list",
                method: "POST",
                path: "/mcp",
                body: Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 4,
                    "method": "resources/list",
                })),
                headers: &[],
            },
            volatile: COMMON_VOLATILE,
            gate: true,
        },
        // `POST /api/dev/gui_error` — the Tauri-GUI error-capture sink.
        // Ungated probe: a valid event makes both gateways append a line
        // to `logs/gui_errors.jsonl` (a filesystem write side-effect), so
        // a divergence is reported informationally rather than failing
        // the suite. Both implementations validate the same normalized
        // shape and return the flat `{ok: true, recorded: true}` body.
        GwCase {
            http: HttpCase {
                name: "dev_gui_error",
                method: "POST",
                path: "/api/dev/gui_error",
                body: Some(serde_json::json!({
                    "timestamp_iso": "2026-05-22T10:00:00.000Z",
                    "source": "manual",
                    "message": "wylde-parity-probe gui error",
                    "route": "parity",
                    "severity": "error",
                })),
                headers: &[],
            },
            volatile: COMMON_VOLATILE,
            gate: false,
        },
        // Unknown route — exercises each framework's 404 handler. Probe:
        // FastAPI and axum render 404 bodies differently by default.
        GwCase {
            http: HttpCase {
                name: "unknown_route",
                method: "GET",
                path: "/api/parity-nonexistent",
                body: None,
                headers: &[],
            },
            volatile: COMMON_VOLATILE,
            gate: false,
        },
    ]
}

/// Two distinct free TCP ports, both reserved at once so they cannot
/// collide, then released for the gateways to bind.
fn two_free_ports() -> (u16, u16) {
    let a = TcpListener::bind("127.0.0.1:0").expect("reserve port a");
    let b = TcpListener::bind("127.0.0.1:0").expect("reserve port b");
    let pa = a.local_addr().unwrap().port();
    let pb = b.local_addr().unwrap().port();
    (pa, pb)
}

#[test]
fn gateway_http_parity() {
    let py_bin = paths::venv_python();
    paths::require_artifact(
        &py_bin,
        "create the Wylde virtualenv (.venv) with the service dependencies",
    );
    let rs_bin = paths::rust_release_bin("wylde-gateway");
    paths::require_artifact(
        &rs_bin,
        "run `cargo build --release` in the rust/ workspace",
    );

    let (py_port, rs_port) = two_free_ports();
    let py_url = format!("http://127.0.0.1:{py_port}");
    let rs_url = format!("http://127.0.0.1:{rs_port}");

    let mut py_cmd = proc::python_module("Gateway.run");
    py_cmd.env("WYLDE_GATEWAY_PORT", py_port.to_string());
    let mut python = proc::Service::spawn("python gateway", py_cmd).expect("spawn python gateway");

    let mut rs_cmd = proc::rust_binary("wylde-gateway");
    rs_cmd.env("WYLDE_GATEWAY_PORT", rs_port.to_string());
    let mut rust = proc::Service::spawn("rust gateway", rs_cmd).expect("spawn rust gateway");

    let client = http::client();
    let ready_timeout = Duration::from_secs(45);

    assert!(
        http::wait_ready(&client, &py_url, ready_timeout),
        "python gateway never became ready on {py_url} (exited early: {})",
        python.has_exited(),
    );
    assert!(
        http::wait_ready(&client, &rs_url, ready_timeout),
        "rust gateway never became ready on {rs_url} (exited early: {})",
        rust.has_exited(),
    );

    let mut gate_failures: Vec<String> = Vec::new();
    let mut gate_failure_names: Vec<&str> = Vec::new();
    let mut probe_findings: Vec<String> = Vec::new();
    let mut gated_pass: Vec<&str> = Vec::new();
    let mut skipped: Vec<&str> = Vec::new();

    // Pre-flight: if the Wylde user's live Wylde stack is running, certain routes
    // proxy to real backends on the Python side and to nothing on the
    // Rust side (we spawn a fresh `wylde-gateway.exe` but no live Voice
    // / etc.). Skip those cases with a clear log line — same pattern
    // lifecycle parity uses for the live-daemon guard. Re-runs once the
    // live stack is stopped will gate them normally.
    let live_voice_bound = {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime for pre-flight probe");
        rt.block_on(wylde_parity::pipe::pipe_in_use("wylde-voice", "voice.get_status"))
    };

    for case in cases() {
        if case.http.name == "voice_health" && live_voice_bound {
            skipped.push(case.http.name);
            eprintln!(
                "[skip] {} : live `wylde-voice` pipe bound — stop the launcher to gate this case",
                case.http.name,
            );
            continue;
        }
        let py = http::fire(&client, &py_url, &case.http).to_json();
        let rs = http::fire(&client, &rs_url, &case.http).to_json();
        match diff::compare(case.http.name, &py, &rs, case.volatile) {
            Ok(()) => {
                if case.gate {
                    gated_pass.push(case.http.name);
                } else {
                    eprintln!("[probe] {} : PARITY", case.http.name);
                }
            }
            Err(report) => {
                if case.gate {
                    gate_failure_names.push(case.http.name);
                    gate_failures.push(report);
                } else {
                    probe_findings.push(report);
                }
            }
        }
    }

    eprintln!("\n=== Gateway parity ===");
    eprintln!(
        "gated routes at parity ({}): {gated_pass:?}",
        gated_pass.len()
    );
    if !skipped.is_empty() {
        eprintln!(
            "skipped (live-backend guard) ({}): {skipped:?}",
            skipped.len()
        );
    }
    if gate_failure_names.is_empty() {
        eprintln!("gated routes diverged: none");
    } else {
        eprintln!(
            "gated routes diverged ({}): {gate_failure_names:?}",
            gate_failure_names.len()
        );
    }
    if probe_findings.is_empty() {
        eprintln!("probed routes: all at parity");
    } else {
        eprintln!(
            "probed routes with divergences ({}): see below",
            probe_findings.len()
        );
        for finding in &probe_findings {
            eprintln!("\n--- [probe finding] ---\n{finding}");
        }
    }

    // Drop kills both gateways.
    drop(python);
    drop(rust);

    assert!(
        gate_failures.is_empty(),
        "{} gated gateway route(s) diverged ({:?}):\n\n{}",
        gate_failures.len(),
        gate_failure_names,
        gate_failures.join("\n\n"),
    );
}

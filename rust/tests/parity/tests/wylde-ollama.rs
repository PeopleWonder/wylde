//! wylde-ollama pipe parity: live-Ollama record/replay smoke.
//!
//! Phase 1 of the migration is greenfield Rust — there is no Python
//! counterpart to diff against. Per `docs/wylde-rust-migration-master-plan.md`
//! §5.1, the right shape for parity here is **record/replay against
//! real Ollama**: spin up `wylde-ollama.exe`, fire canonical requests
//! through the pipe, and assert the responses match a fixture corpus
//! captured against the real daemon.
//!
//! The full corpus capture is a follow-up — it needs a live Ollama
//! session and stable model availability across runs. For now this file
//! provides a minimal smoke that:
//!
//!   1. Spawns the Rust `wylde-ollama` binary against the live Ollama
//!      at `OLLAMA_URL` (default `http://127.0.0.1:11434`).
//!   2. Round-trips `ollama.health` + `ollama.list_models` via the
//!      pipe.
//!   3. Asserts both succeed with non-null data.
//!
//! Why so minimal: parity at this layer is *behavioural correctness*,
//! not byte-equality. The unit-test suite in
//! `wylde-ollama/src/actions/*` covers every action against wiremock;
//! the live-Ollama gate here just confirms the pipe → reqwest →
//! upstream wiring works end-to-end.
//!
//! Opt-in. Build the binary, ensure Ollama is running, then:
//! ```
//! WYLDE_OLLAMA_PARITY_LIVE=1 cargo test --features parity \
//!     --test wylde-ollama
//! ```

#![cfg(feature = "parity")]

use std::process::Command;
use std::time::Duration;

use serde_json::json;
use wylde_parity::{paths, pipe, proc};

const SERVICE: &str = "wylde-ollama";

fn live_mode() -> bool {
    std::env::var("WYLDE_OLLAMA_PARITY_LIVE")
        .map(|v| matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

#[tokio::test]
async fn health_and_list_models_via_pipe() {
    if !live_mode() {
        eprintln!("SKIP: WYLDE_OLLAMA_PARITY_LIVE not set");
        return;
    }

    let bin = paths::rust_release_bin("wylde-ollama");
    let cmd = Command::new(bin);
    let mut svc = proc::Service::spawn("wylde-ollama", cmd).expect("spawn wylde-ollama");

    // Give the service a generous window to bind the pipe and finish its
    // startup probe of upstream Ollama.
    let ready = pipe::wait_ready(SERVICE, "ollama.health", Duration::from_secs(15)).await;
    assert!(
        ready,
        "wylde-ollama pipe never became ready (process exited early: {})",
        svc.has_exited(),
    );

    let health = pipe::capture(SERVICE, "ollama.health", json!({})).await;
    assert_eq!(
        health["ok"], true,
        "health round-trip failed: {health:#}"
    );
    assert_eq!(
        health["data"]["ok"], true,
        "health body missing ok: {health:#}"
    );

    let list = pipe::capture(SERVICE, "ollama.list_models", json!({})).await;
    assert_eq!(
        list["ok"], true,
        "list_models round-trip failed: {list:#}"
    );
    assert!(
        list["data"]["models"].is_array(),
        "list_models body missing 'models' array: {list:#}"
    );

    // `Service`'s Drop kills the child process on teardown.
    drop(svc);
}

// TODO(follow-up): expand to a record/replay corpus once a live Ollama
// session can be reserved for capture. Suggested coverage:
//   * Each of the 10 actions × happy + sad path
//   * Streaming: chat_stream emits N frames in order, all parseable
//   * Streaming: pull surfaces success/progress correctly
//   * VRAM lease lifecycle: chat acquires + releases against a real broker
// See `docs/wylde-ollama-design.md §8` for the recommended corpus shape.

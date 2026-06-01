//! Cancellation propagation spike — design doc Q2.
//!
//! Asks: does dropping `reqwest::Response::bytes_stream()` propagate the
//! cancellation upstream to Ollama? Or do we leave ghost generates
//! running on the GPU?
//!
//! Procedure (per the task brief):
//!   1. Start a streaming `/api/chat` request to Ollama with a long
//!      prompt that will generate for ≥30 seconds.
//!   2. After 2s, drop the stream.
//!   3. Wait 5s, then poll `/api/ps` to see if the model is still
//!      loaded/busy.
//!   4. Wait another 30s and assert that either Ollama-side load
//!      drops, OR a fresh `/api/chat` to the same model returns
//!      without queue-induced latency.
//!
//! Why `#[ignore]`: requires a live Ollama daemon at OLLAMA_URL with
//! `qwen2.5:0.5b` (or any small model) already pulled. Default test
//! runs skip it. To run:
//!
//! ```
//! WYLDE_OLLAMA_LIVE=1 cargo test -p wylde-ollama \
//!     --test cancellation_spike -- --ignored --nocapture
//! ```
//!
//! Spike result (when run): record in [docs/wylde-ollama-design.md §9
//! Q2] whether drop sufficed, or whether the explicit `keep_alive=0`
//! evict in `chat::handle_chat_stream` is load-bearing.

use std::time::{Duration, Instant};

use futures::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};

const SPIKE_MODEL: &str = "qwen2.5:0.5b";

fn ollama_url() -> String {
    std::env::var("OLLAMA_URL").unwrap_or_else(|_| "http://127.0.0.1:11434".into())
}

fn live_mode_enabled() -> bool {
    std::env::var("WYLDE_OLLAMA_LIVE")
        .map(|v| matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

#[tokio::test]
#[ignore = "live Ollama required; run with WYLDE_OLLAMA_LIVE=1 --ignored"]
async fn drop_propagates_upstream_stop() {
    if !live_mode_enabled() {
        eprintln!("SKIP: WYLDE_OLLAMA_LIVE not set");
        return;
    }
    let url = ollama_url();
    let client = Client::builder()
        .build()
        .expect("reqwest client");

    // Probe — bail with diagnostic if Ollama isn't actually reachable.
    let probe = client
        .get(format!("{url}/"))
        .timeout(Duration::from_secs(3))
        .send()
        .await;
    match probe {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => {
            panic!("SPIKE PRECHECK FAIL: Ollama at {url}/ returned {}", r.status());
        }
        Err(e) => {
            panic!("SPIKE PRECHECK FAIL: cannot reach Ollama at {url}/: {e}");
        }
    }

    // Verify the spike model is installed.
    let tags: Value = client
        .get(format!("{url}/api/tags"))
        .send()
        .await
        .expect("/api/tags")
        .json()
        .await
        .expect("decode tags");
    let installed: Vec<String> = tags["models"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|m| m["name"].as_str().map(str::to_owned))
        .collect();
    assert!(
        installed.iter().any(|n| n == SPIKE_MODEL || n.starts_with(SPIKE_MODEL)),
        "spike model {SPIKE_MODEL} not installed (installed: {installed:?})"
    );

    let body = json!({
        "model": SPIKE_MODEL,
        "messages": [
            {"role": "user",
             "content": "Write a 5000-word essay on the history of the bicycle, \
                        starting from the early 1800s and going to present day. Be \
                        detailed and include many examples."}
        ],
        "stream": true,
    });

    // Step 1+2: open the stream, read for 2s, drop it.
    let start = Instant::now();
    {
        let resp = client
            .post(format!("{url}/api/chat"))
            .json(&body)
            .send()
            .await
            .expect("open chat stream");
        assert!(resp.status().is_success(), "chat HTTP {}", resp.status());

        let mut stream = resp.bytes_stream();
        let mut bytes_seen = 0usize;
        let read_until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < read_until {
            match tokio::time::timeout(Duration::from_millis(500), stream.next()).await {
                Ok(Some(Ok(chunk))) => bytes_seen += chunk.len(),
                Ok(Some(Err(e))) => panic!("stream errored: {e}"),
                Ok(None) => panic!("stream ended early (only {bytes_seen} bytes)"),
                Err(_) => continue,
            }
        }
        assert!(bytes_seen > 0, "no tokens streamed in 2s");
        eprintln!("SPIKE: read {bytes_seen} bytes before drop");
        // Stream + Response drop here — this is the cancellation point.
    }
    eprintln!("SPIKE: stream dropped at t={:?}", start.elapsed());

    // Step 3: wait 5s, then poll /api/ps.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let ps_after_drop: Value = client
        .get(format!("{url}/api/ps"))
        .send()
        .await
        .expect("/api/ps")
        .json()
        .await
        .expect("decode ps");
    eprintln!("SPIKE: /api/ps 5s after drop = {ps_after_drop:#}");

    // Step 4: time a fresh non-streaming chat to the same model.
    // If the prior generation is still running, this either blocks
    // (queue-induced) or comes back immediately depending on Ollama's
    // version. We measure the wall-clock of a tiny prompt.
    let probe_start = Instant::now();
    let probe_body = json!({
        "model": SPIKE_MODEL,
        "messages": [{"role": "user", "content": "Say hi."}],
        "stream": false,
    });
    let probe_resp = client
        .post(format!("{url}/api/chat"))
        .json(&probe_body)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .expect("probe chat");
    let probe_elapsed = probe_start.elapsed();
    assert!(probe_resp.status().is_success(), "probe HTTP");
    eprintln!("SPIKE: post-drop probe round-trip = {probe_elapsed:?}");

    // Verdict heuristic:
    //   < 5s: previous generation was cancelled (drop propagated).
    //   ≥ 10s: previous generation likely still running (drop did NOT
    //          propagate; need explicit cancel).
    // Threshold is conservative — tune per machine.
    if probe_elapsed >= Duration::from_secs(10) {
        panic!(
            "SPIKE FAIL: probe took {probe_elapsed:?} (≥10s) — drop did not \
             propagate upstream. The explicit keep_alive=0 evict in \
             chat::handle_chat_stream is LOAD-BEARING. Document in design doc Q2."
        );
    }
    eprintln!(
        "SPIKE PASS: probe took {probe_elapsed:?}; reqwest drop appears to \
         propagate stop to Ollama. The explicit eject in chat_stream is \
         belt-and-suspenders and can be removed in a follow-up."
    );
}

//! Live, machine-touching proof that `spawn_ollama_serve` actually starts
//! the real upstream Ollama daemon. `#[ignore]` by default — it spawns a
//! real `ollama serve` and binds 127.0.0.1:11434, so it only runs on a box
//! with Ollama installed and the daemon NOT already up.
//!
//! Run it explicitly (after confirming nothing is on :11434):
//!   cargo test -p wylde-lifecycle --test ollama_upstream_spawn_live -- --ignored --nocapture
//!
//! This is the "exercise the real path" check for the Start-Ollama-button
//! fix: locate the real ollama binary, detach-spawn `ollama serve`, and
//! confirm the daemon comes up — the exact mechanism `service.start
//! wylde-ollama` now drives. The spawned daemon is intentionally left
//! running (it is the user's external service; the fix never reaps it).

use std::net::TcpStream;
use std::time::{Duration, Instant};

use wylde_lifecycle::state::services::{locate_ollama_binary, spawn_ollama_serve};

fn upstream_listening() -> bool {
    TcpStream::connect_timeout(
        &"127.0.0.1:11434".parse().unwrap(),
        Duration::from_millis(300),
    )
    .is_ok()
}

#[test]
#[ignore = "spawns a real ollama serve; run manually on a host with Ollama installed"]
fn spawn_ollama_serve_brings_the_daemon_up() {
    // Precondition: Ollama must be installed on this host.
    let bin = locate_ollama_binary().expect(
        "ollama not found — install it or set WYLDE_OLLAMA_SERVE_BIN before running this test",
    );
    eprintln!("located ollama at {}", bin.display());

    // Precondition: the daemon must NOT already be up, or we'd be testing a
    // no-op. Stop any running `ollama serve` first.
    assert!(
        !upstream_listening(),
        "127.0.0.1:11434 is already serving — stop the running ollama first so this test \
         exercises the spawn path"
    );

    let launched = spawn_ollama_serve().expect("spawn_ollama_serve should succeed");
    eprintln!("spawned `{} serve`", launched.display());

    // Poll for the daemon to bind, same as the lifecycle ensure path.
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut up = false;
    while Instant::now() < deadline {
        if upstream_listening() {
            up = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    assert!(
        up,
        "ollama daemon did not start listening on 127.0.0.1:11434 within 15s"
    );
    eprintln!("upstream is now serving on 127.0.0.1:11434 — left running intentionally");
}

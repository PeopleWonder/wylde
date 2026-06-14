//! Live, machine-touching proof that the shared `pull_model` helper drives
//! the real streaming `ollama.pull` verb end-to-end. `#[ignore]` by default
//! — it talks to the running `wylde-ollama` wrapper + the Ollama daemon and
//! streams a real `/api/pull`.
//!
//! Run it explicitly (stack up, Ollama daemon up):
//!   cargo test -p wylde-gui-pipe --test model_pull_live -- --ignored --nocapture
//!
//! This is the "exercise the real verb path" check for the Download-model
//! feature: it does NOT trust that `pull_model` "looks wired" — it opens the
//! actual stream and asserts progress frames flow and the pull reaches
//! `success`. Pulling an already-installed model (the default
//! `nomic-embed-text`) makes it fast: Ollama re-verifies cached layers and
//! emits `success` without a multi-hundred-MB download.

use wylde_gui_pipe::{pull_model, PullProgress};

#[tokio::test]
#[ignore = "streams a real ollama.pull against the live wrapper + Ollama daemon"]
async fn pull_model_streams_progress_to_success() {
    let model = std::env::var("WYLDE_PULL_TEST_MODEL")
        .unwrap_or_else(|_| "nomic-embed-text".to_owned());

    let mut stream = pull_model(&model).expect("open ollama.pull stream");

    let mut saw_status = false;
    let mut saw_success = false;
    // Generous frame budget; a cached re-verify finishes in a handful of
    // frames, a cold pull in a few thousand. Either way we stop on success.
    for _ in 0..50_000 {
        match stream.recv().await {
            Some(Ok(v)) => {
                let p = PullProgress::from_value(&v);
                if !p.status.is_empty() {
                    saw_status = true;
                    eprintln!("pull frame: {}", p.label());
                }
                if p.is_success() {
                    saw_success = true;
                    break;
                }
            }
            Some(Err(e)) => panic!("ollama.pull stream errored: {e}"),
            None => break,
        }
    }

    assert!(saw_status, "expected at least one progress frame carrying a status");
    assert!(
        saw_success,
        "expected the pull of {model:?} to reach a success frame"
    );
}

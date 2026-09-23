//! Live, machine-touching proof that `pull_model` drives the real streaming
//! `ollama.pull` verb end-to-end **through the production bridge path** —
//! the exact code path the GUI uses, not the in-runtime shortcut a
//! `#[tokio::test]` would take. `#[ignore]` by default (talks to the live
//! `wylde-ollama` wrapper + Ollama daemon and streams a real `/api/pull`).
//!
//! Run it explicitly (stack up, Ollama daemon up):
//!   cargo test -p wylde-gui-pipe --test model_pull_live -- --ignored --nocapture
//!
//! Why a plain `#[test]` with hand-built runtimes instead of `#[tokio::test]`:
//! the GUI (`Shell/src/main.rs`) builds a multi-thread tokio runtime and
//! registers its handle via `install_runtime`; gpui then opens + drives pipe
//! streams from threads that have NO current tokio runtime. So `stream_call`
//! takes its `TOKIO_HANDLE.get()` + `handle.spawn(...)` branch. A
//! `#[tokio::test]` would instead hit the `Handle::try_current()` shortcut and
//! never exercise that branch — i.e. "looks wired" without proving the path
//! the Download button actually runs on. This test reproduces the real path.

use wylde_gui_pipe::{install_runtime, pull_model, PullAggregate, PullProgress};

#[test]
#[ignore = "streams a real ollama.pull over the production bridge path"]
fn pull_model_streams_progress_to_success_via_bridge() {
    let model =
        std::env::var("WYLDE_PULL_TEST_MODEL").unwrap_or_else(|_| "nomic-embed-text".to_owned());

    // 1) Stand up the bridge exactly like the GUI does and register it.
    let bridge = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build bridge runtime");
    install_runtime(bridge.handle().clone());

    // 2) This libtest thread has NO current tokio runtime, so this call hits
    //    the production `TOKIO_HANDLE` branch and spawns the reader on the
    //    bridge — the same as a gpui task opening the stream.
    let mut stream = pull_model(&model).expect("open ollama.pull stream via bridge");

    // 3) Drive recv() from a context that is NOT the bridge (a small
    //    current-thread runtime), mirroring gpui parking the UI task while the
    //    bridge-spawned reader fills the channel.
    let driver = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build driver runtime");

    let (saw_status, saw_overall_percent, saw_success) = driver.block_on(async move {
        // The same aggregate the GUI progress bar renders, fed by REAL frames.
        let mut agg = PullAggregate::default();
        let (mut status, mut pct, mut ok) = (false, false, false);
        for _ in 0..50_000 {
            match stream.recv().await {
                Some(Ok(v)) => {
                    let p = PullProgress::from_value(&v);
                    agg.update(&p);
                    if !p.status.is_empty() {
                        status = true;
                        eprintln!("bar: {} (overall {:?})", agg.label(), agg.percent());
                    }
                    if agg.percent().is_some() {
                        pct = true;
                    }
                    if p.is_success() {
                        ok = true;
                        break;
                    }
                }
                Some(Err(e)) => panic!("ollama.pull stream errored over the bridge: {e}"),
                None => break,
            }
        }
        (status, pct, ok)
    });

    assert!(
        saw_status,
        "expected at least one progress frame carrying a status"
    );
    assert!(
        saw_overall_percent,
        "expected the aggregate to yield an overall percent for the bar"
    );
    assert!(
        saw_success,
        "expected the pull of {model:?} to reach a success frame over the bridge path"
    );
}

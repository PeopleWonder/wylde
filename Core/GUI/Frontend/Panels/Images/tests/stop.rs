//! Windowed gpui tests for the Images "Stop" control (GUI-responsiveness pass —
//! category c). A ComfyUI generate can't be cancelled mid-run and was
//! dispatched onto tokio via the bridge, so dropping the gpui task would only
//! detach it — leaving the GPU job running while re-enabling Generate (→
//! overlapping jobs + a false "cancelled"). The fix keeps the run guard engaged
//! and turns Stop into an honest "Finishing…" (detached) marker.
//!
//! These assert the SYNCHRONOUS state transitions of submit/stop — the
//! behaviour under test is local state, and the generate request itself goes
//! over the gateway's HTTP route (not the `wylde_gui_pipe::call` seam), so we
//! deliberately do NOT drive the executor (no run_until_parked): the guard and
//! the detached flag are set inline, before any request could resolve.

use gpui::TestAppContext;

use wylde_panel_images::ImagesPanel;

#[gpui::test]
fn stop_keeps_the_guard_engaged_and_marks_finishing(cx: &mut TestAppContext) {
    let window = cx.add_window(|_w, cx| ImagesPanel::new(cx));

    // Submit engages the run guard immediately (so a second submit is blocked).
    window
        .update(cx, |p, _w, cx| {
            p.submit_generate("a red cube on a table".to_owned(), cx);
            assert!(p.generate_running, "submitting engages the run guard");
            assert!(!p.generate_detached, "a fresh generation is not yet detached");
        })
        .unwrap();

    // Stop marks the job detached but KEEPS the guard — the GPU job runs to
    // completion and Generate stays disabled (no overlapping jobs).
    window
        .update(cx, |p, _w, cx| {
            p.cancel_generate(cx);
            assert!(
                p.generate_detached,
                "Stop marks the job detached (honest 'Finishing…', not a false cancel)"
            );
            assert!(
                p.generate_running,
                "the run guard stays engaged so no overlapping generation can start"
            );
        })
        .unwrap();
}

#[gpui::test]
fn stop_is_a_noop_when_nothing_is_generating(cx: &mut TestAppContext) {
    let window = cx.add_window(|_w, cx| ImagesPanel::new(cx));
    window
        .update(cx, |p, _w, cx| {
            assert!(!p.generate_running);
            p.cancel_generate(cx);
            assert!(!p.generate_detached, "Stop with nothing in flight changes nothing");
        })
        .unwrap();
}

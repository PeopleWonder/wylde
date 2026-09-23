//! L7 **control**-walk: Tabulate (the #247 standard, for #39).
//!
//! Harness: `wylde_gui_test_support::control_walk`. Every clickable control is
//! built with `control(div(), id)`, so the walk enumerates and clicks each one,
//! and a click must move at least one observable channel (a backend call or the
//! panel fingerprint).
//!
//! The reset frame puts a real path in the input so Probe and Extract prove the
//! full `tabulate.probe` / `tabulate.extract` round-trip, not just the
//! empty-path guard.
//!
//! `fmt-xlsx` is the output-format radio's active segment in the reset frame,
//! so clicking it is a deliberate no-op. It is declared `external_effect`, the
//! same treatment every other radio's active segment gets; `fmt-csv` proves the
//! wiring.

use std::sync::Arc;

use gpui::TestAppContext;
use serde_json::json;

use wylde_gui_test_support::control_walk::{ControlWalk, WalkReport};
use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_tabulate::tabulate_panel::OutputFormat;
use wylde_panel_tabulate::TabulatePanel;

fn backend() -> Arc<ScriptedBackend> {
    ScriptedBackend::new()
        .on(
            "tabulate.probe",
            json!({
                "file_type": "csv",
                "mime": "text/csv",
                "tables_detected": 1,
                "tables": [{
                    "rows": 3, "cols": 2, "header_inferred": true,
                    "columns": [
                        { "header": "name", "kind": "text" },
                        { "header": "age", "kind": "integer" }
                    ]
                }],
                "redaction_warning": "",
                "note": null,
                "audit_id": "audit-1"
            }),
        )
        .on(
            "tabulate.extract",
            json!({
                "output_path": "C:/intake/sample.xlsx",
                "output_format": "xlsx",
                "tier_used": "deterministic",
                "format": "csv",
                "tables": [{ "name": "Sheet1", "rows": 3, "cols": 2 }]
            }),
        )
}

/// Everything a control can change: the format picker and the probe/extract
/// results and status lines the async calls land in.
fn fingerprint(p: &TabulatePanel) -> String {
    format!(
        "fmt={:?} probe={} extract={} loading={} err={:?} status={:?}",
        p.output_format,
        p.probe.is_some(),
        p.extract.is_some(),
        p.loading,
        p.error,
        p.status,
    )
}

/// Back to a fresh panel with a real input path before every click.
fn reset(p: &mut TabulatePanel, _w: &mut gpui::Window, cx: &mut gpui::Context<TabulatePanel>) {
    p.output_format = OutputFormat::Xlsx;
    p.probe = None;
    p.extract = None;
    p.loading = false;
    p.error = None;
    p.status = None;
    p.input_path
        .update(cx, |i, cx| i.set_text("C:/intake/sample.csv", cx));
    cx.notify();
}

fn mount(cx: &mut TestAppContext) -> gpui::WindowHandle<TabulatePanel> {
    let window = cx.add_window(|_w, cx| TabulatePanel::new(cx));
    cx.run_until_parked();
    window
}

fn walk(
    cx: &mut TestAppContext,
    window: gpui::WindowHandle<TabulatePanel>,
    fake: &Arc<ScriptedBackend>,
) -> WalkReport {
    ControlWalk::new(window, fake)
        .fingerprint(fingerprint)
        .reset(reset)
        .external_effect(&["fmt-xlsx"])
        .sources(&[include_str!("../src/tabulate_panel.rs")])
        .run(cx)
}

#[gpui::test]
fn every_tabulate_control_does_something_when_clicked(cx: &mut TestAppContext) {
    let fake = backend();
    let _guard = fake.clone().install();
    let window = mount(cx);

    walk(cx, window, &fake)
        .assert_every_control_lives()
        .assert_covers_every_literal_id();
}

#[gpui::test]
fn the_walk_reaches_probe_extract_and_the_format_picker(cx: &mut TestAppContext) {
    let fake = backend();
    let _guard = fake.clone().install();
    let window = mount(cx);

    let report = walk(cx, window, &fake);
    let painted = report.painted_ids();
    for id in ["tabulate-probe", "tabulate-extract", "fmt-csv"] {
        assert!(
            painted.contains(&id),
            "{id} painted and was walked; got {painted:?}"
        );
    }
}

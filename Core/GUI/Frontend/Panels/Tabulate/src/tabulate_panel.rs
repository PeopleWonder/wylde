//! Tabulate panel View — the native gpui cockpit over `wylde-tabulate`.
//!
//! Flow, top → bottom:
//!   * Header — title + a subtle safety-posture chip (from
//!     `tabulate.capabilities`): local-only, encrypted at rest, audit on.
//!   * Input — a file-path field + an output-format toggle (`.xlsx` / `.csv`),
//!     a **Probe** button and an **Extract** button.
//!   * Probe result — file type + per-table shape and per-column header +
//!     inferred type (NEVER a cell value), plus the redaction-review gate.
//!   * Extract result — the absolute output path + a one-line summary and the
//!     "needs human validation" note.
//!
//! The panel greys out (ServiceUnavailable stub) when `wylde-tabulate` is down —
//! declared via `required_services` in the manifest, handled by the Shell.

use gpui::{
    div, prelude::*, px, rgb, AnyView, App, AppContext, AsyncApp, Context, ElementId, Entity,
    FontWeight, IntoElement, Render, SharedString, Window,
};
use wylde_gpui_input::TextInput;
use wylde_theme::colors::{
    BORDER_DEFAULT, BORDER_SUBTLE, BRAND, BRAND_DIM, DANGER, SURFACE_700, SURFACE_800, SURFACE_900,
    TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY, WARNING,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::ipc::{self, Capabilities, ExtractView, ProbeView};

/// The two output formats the v1 writer can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Xlsx,
    Csv,
}

impl OutputFormat {
    fn wire(self) -> &'static str {
        match self {
            OutputFormat::Xlsx => "xlsx",
            OutputFormat::Csv => "csv",
        }
    }
    fn label(self) -> &'static str {
        match self {
            OutputFormat::Xlsx => ".xlsx",
            OutputFormat::Csv => ".csv",
        }
    }
}

pub struct TabulatePanel {
    /// The input file path (`tabulate.*` reads it locally — PHI never leaves
    /// the box).
    pub input_path: Entity<TextInput>,
    /// Output spreadsheet format for Extract.
    pub output_format: OutputFormat,

    /// Service capabilities + HIPAA posture, read once on open (drives the
    /// safety chip). `None` until the first `tabulate.capabilities` returns.
    pub capabilities: Option<Capabilities>,
    /// The last `tabulate.probe` result (structure only).
    pub probe: Option<ProbeView>,
    /// The last `tabulate.extract` result.
    pub extract: Option<ExtractView>,

    pub loading: bool,
    pub error: Option<String>,
    /// Transient status line.
    pub status: Option<String>,
}

impl TabulatePanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let input_path = cx.new(|input_cx| {
            TextInput::single_line(input_cx).with_placeholder(
                "Path to a file (e.g. a .csv / .xlsx / .json intake export)",
            )
        });
        Self {
            input_path,
            output_format: OutputFormat::Xlsx,
            capabilities: None,
            probe: None,
            extract: None,
            loading: false,
            error: None,
            status: None,
        }
    }

    /// Factory entry — matches the manifest factory string
    /// (`wylde_panel_tabulate::TabulatePanel::view`).
    pub fn view(_window: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|cx| {
            let panel = Self::new(cx);
            // Read the safety posture once so the chip can render; the panel is
            // fully usable even if this is still in flight.
            Self::spawn_capabilities(cx);
            panel
        })
        .into()
    }

    /// Load `tabulate.capabilities` into `self.capabilities` (safety chip).
    fn spawn_capabilities(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let result = ipc::capabilities().await;
            let _ = this.update(app_cx, |panel, cx| {
                if let Ok(caps) = result {
                    panel.capabilities = Some(caps);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub fn set_format(&mut self, fmt: OutputFormat, cx: &mut Context<Self>) {
        self.output_format = fmt;
        cx.notify();
    }

    /// The trimmed input path, or `None` when the field is empty.
    fn current_path(&self, cx: &Context<Self>) -> Option<String> {
        let p = self.input_path.read(cx).text().trim().to_owned();
        (!p.is_empty()).then_some(p)
    }

    /// Fire `tabulate.probe` and render the returned structure-only view.
    pub fn run_probe(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.current_path(cx) else {
            self.error = Some("Enter a file path first.".to_owned());
            cx.notify();
            return;
        };
        self.loading = true;
        self.error = None;
        self.status = None;
        self.probe = None;
        cx.notify();

        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let result = ipc::probe(path).await;
            let _ = this.update(app_cx, |panel, cx| {
                panel.loading = false;
                match result {
                    Ok(p) => {
                        let detected = p
                            .tables_detected
                            .map(|n| format!("{n} table(s)"))
                            .unwrap_or_else(|| "no tabular structure".to_owned());
                        panel.status = Some(format!("Probed {} — {}.", p.file_type, detected));
                        panel.probe = Some(p);
                    }
                    Err(e) => panel.error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Fire `tabulate.extract`; the service writes the spreadsheet and reports
    /// where it landed.
    pub fn run_extract(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.current_path(cx) else {
            self.error = Some("Enter a file path first.".to_owned());
            cx.notify();
            return;
        };
        let fmt = self.output_format.wire();
        self.loading = true;
        self.error = None;
        self.status = None;
        self.extract = None;
        cx.notify();

        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let result = ipc::extract(path, fmt).await;
            let _ = this.update(app_cx, |panel, cx| {
                panel.loading = false;
                match result {
                    Ok(out) => {
                        panel.status = Some(format!(
                            "Extracted {} table(s) via the {} tier.",
                            out.tables.len(),
                            out.tier_used
                        ));
                        panel.extract = Some(out);
                    }
                    Err(e) => panel.error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

// ── rendering ─────────────────────────────────────────────────────────

impl Render for TabulatePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .size_full()
            .bg(rgb(pack(SURFACE_900)))
            .font_family(FAMILY_INTER)
            .text_color(rgb(pack(TEXT_PRIMARY)))
            .child(self.header())
            .child(self.input_section(cx));

        if let Some(status) = &self.status {
            root = root.child(strip(status, TEXT_SECONDARY));
        }
        if let Some(err) = &self.error {
            root = root.child(strip(&format!("Error: {err}"), DANGER));
        }
        if self.loading {
            root = root.child(strip("Working…", TEXT_MUTED));
        }
        if self.probe.is_some() {
            root = root.child(self.probe_view());
        }
        if self.extract.is_some() {
            root = root.child(self.extract_view());
        }
        root
    }
}

impl TabulatePanel {
    fn header(&self) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_size(px(size::LG))
                    .font_weight(FontWeight(weight::SEMIBOLD as f32))
                    .child(SharedString::from("Tabulate")),
            )
            .child(hint(
                "Turn a file into a spreadsheet — runs entirely on this machine.",
            ))
            .child(self.safety_chip())
    }

    /// A subtle one-line posture chip from `tabulate.capabilities`. Renders a
    /// neutral "checking…" line until the first capabilities reply lands.
    fn safety_chip(&self) -> gpui::Div {
        let Some(caps) = self.capabilities.as_ref() else {
            return chip("Safety: checking service…", TEXT_MUTED, BORDER_SUBTLE);
        };
        let h = &caps.hipaa;
        let mut parts: Vec<&str> = Vec::new();
        if h.network_blocked {
            parts.push("local-only (no network)");
        }
        if h.at_rest.app_level_encryption {
            parts.push("encrypted at rest");
        }
        if h.audit {
            parts.push("audit log on");
        }
        let body = if parts.is_empty() {
            format!("Safety posture: {}", caps.status)
        } else {
            format!("Safety: {}", parts.join(" · "))
        };
        chip(&body, BRAND, BORDER_DEFAULT)
    }

    fn input_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Output-format toggle.
        let mut fmt_row = div().flex().flex_row().gap_2().items_center().child(label("Output"));
        for f in [OutputFormat::Xlsx, OutputFormat::Csv] {
            fmt_row = fmt_row.child(format_button(f, self.output_format == f, cx));
        }

        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded(px(6.0))
            .bg(rgb(pack(SURFACE_800)))
            .child(label("Input file"))
            .child(self.input_path.clone())
            .child(fmt_row)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(button(
                        "tabulate-probe",
                        "Probe structure",
                        BORDER_DEFAULT,
                        cx.listener(|this: &mut TabulatePanel, _ev, _w, cx| this.run_probe(cx)),
                    ))
                    .child(button(
                        "tabulate-extract",
                        "Extract to spreadsheet",
                        BRAND,
                        cx.listener(|this: &mut TabulatePanel, _ev, _w, cx| this.run_extract(cx)),
                    )),
            )
            .child(hint(
                "Probe shows structure only — never a cell value. Extract writes a new file.",
            ))
    }

    fn probe_view(&self) -> gpui::Div {
        // Render path only reached when a probe exists; let-else keeps it
        // panic-free regardless.
        let Some(p) = self.probe.as_ref() else {
            return div();
        };

        let detected = p
            .tables_detected
            .map(|n| n.to_string())
            .unwrap_or_else(|| "—".to_owned());

        let mut col = div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded(px(6.0))
            .bg(rgb(pack(SURFACE_800)))
            .child(label(&format!(
                "Structure · {} ({}) · {} table(s)",
                p.file_type, p.mime, detected
            )));

        for (i, t) in p.tables.iter().enumerate() {
            col = col.child(
                div()
                    .text_size(px(size::SM))
                    .font_weight(FontWeight(weight::SEMIBOLD as f32))
                    .text_color(rgb(pack(TEXT_SECONDARY)))
                    .child(SharedString::from(format!(
                        "Table {}: {} row(s) × {} col(s){}",
                        i + 1,
                        t.rows,
                        t.cols,
                        if t.header_inferred { " · header detected" } else { "" }
                    ))),
            );
            for c in &t.columns {
                let header = if c.header.is_empty() { "(unnamed)" } else { c.header.as_str() };
                col = col.child(
                    div()
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .child(SharedString::from(format!("• {header}  —  {}", c.kind))),
                );
            }
        }

        // A deferred / unparseable note, when present.
        if let Some(note) = &p.note {
            col = col.child(hint(note));
        }
        // The mandatory redaction-review gate (always present on a real probe).
        if !p.redaction_warning.is_empty() {
            col = col.child(strip(&p.redaction_warning, WARNING));
        }
        if !p.audit_id.is_empty() {
            col = col.child(hint(&format!("Audit id: {}", p.audit_id)));
        }
        col
    }

    fn extract_view(&self) -> gpui::Div {
        let Some(e) = self.extract.as_ref() else {
            return div();
        };

        let mut col = div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded(px(6.0))
            .bg(rgb(pack(SURFACE_800)))
            .child(label("Extract complete"))
            .child(
                div()
                    .text_size(px(size::SM))
                    .text_color(rgb(pack(TEXT_PRIMARY)))
                    .child(SharedString::from(format!("Wrote: {}", e.output_path))),
            )
            .child(hint(&format!(
                "{} table(s) · {} tier · source format {}",
                e.tables.len(),
                e.tier_used,
                e.format
            )));

        for t in &e.tables {
            col = col.child(
                div()
                    .text_size(px(size::XS))
                    .text_color(rgb(pack(TEXT_MUTED)))
                    .child(SharedString::from(format!(
                        "• {}: {} row(s) × {} col(s)",
                        t.name, t.rows, t.cols
                    ))),
            );
        }

        for w in &e.warnings {
            col = col.child(strip(w, WARNING));
        }
        if e.needs_validation {
            col = col.child(strip(
                "Extraction is assistive — review the output against the source before relying on it.",
                WARNING,
            ));
        }
        if !e.audit_id.is_empty() {
            col = col.child(hint(&format!("Audit id: {}", e.audit_id)));
        }
        col
    }
}

// ── small element helpers ────────────────────────────────────────────

fn button(
    id: &'static str,
    text: &str,
    border: gpui::Rgba,
    on_click: impl Fn(&gpui::MouseDownEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(ElementId::Name(id.into()))
        .px_3()
        .py_2()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(pack(border)))
        .cursor_pointer()
        .text_size(px(size::SM))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .hover(|s| s.bg(rgb(pack(SURFACE_700))))
        .on_mouse_down(gpui::MouseButton::Left, on_click)
        .child(SharedString::from(text.to_owned()))
}

fn format_button(f: OutputFormat, selected: bool, cx: &mut Context<TabulatePanel>) -> impl IntoElement {
    div()
        .id(ElementId::Name(format!("fmt-{}", f.wire()).into()))
        .px_3()
        .py_1()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(pack(if selected { BRAND } else { BORDER_DEFAULT })))
        .bg(rgb(pack(if selected { BRAND_DIM } else { SURFACE_700 })))
        .cursor_pointer()
        .text_size(px(size::SM))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this: &mut TabulatePanel, _ev, _w, cx| this.set_format(f, cx)),
        )
        .child(SharedString::from(f.label()))
}

fn label(text: &str) -> impl IntoElement {
    div()
        .text_size(px(size::SM))
        .font_weight(FontWeight(weight::SEMIBOLD as f32))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .child(SharedString::from(text.to_owned()))
}

fn hint(text: &str) -> impl IntoElement {
    div()
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_MUTED)))
        .child(SharedString::from(text.to_owned()))
}

fn strip(text: &str, color: gpui::Rgba) -> impl IntoElement {
    div()
        .p_2()
        .rounded(px(4.0))
        .bg(rgb(pack(SURFACE_800)))
        .text_size(px(size::SM))
        .text_color(rgb(pack(color)))
        .child(SharedString::from(text.to_owned()))
}

/// A compact inline status pill (the safety posture line).
fn chip(text: &str, fg: gpui::Rgba, border: gpui::Rgba) -> gpui::Div {
    div()
        .self_start()
        .px_2()
        .py_1()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(pack(border)))
        .text_size(px(size::XS))
        .text_color(rgb(pack(fg)))
        .child(SharedString::from(text.to_owned()))
}

pub(crate) fn pack(c: gpui::Rgba) -> u32 {
    let r = (c.r.clamp(0.0, 1.0) * 255.0).round() as u32;
    let g = (c.g.clamp(0.0, 1.0) * 255.0).round() as u32;
    let b = (c.b.clamp(0.0, 1.0) * 255.0).round() as u32;
    (r << 16) | (g << 8) | b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_signature_compiles() {
        fn assert_render<T: Render>() {}
        assert_render::<TabulatePanel>();
    }

    #[test]
    fn format_wire_strings_match_service() {
        assert_eq!(OutputFormat::Xlsx.wire(), "xlsx");
        assert_eq!(OutputFormat::Csv.wire(), "csv");
        assert_eq!(OutputFormat::Xlsx.label(), ".xlsx");
    }

    #[test]
    fn pack_round_trips_known_colors() {
        // White packs to 0xffffff; a known brand value stays in range.
        assert_eq!(pack(gpui::rgb(0xffffff)), 0xffffff);
        assert!(pack(BRAND) <= 0xffffff);
    }
}

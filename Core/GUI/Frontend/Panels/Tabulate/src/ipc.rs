//! Per-panel IPC helpers for the Tabulate panel.
//!
//! Every call goes through `wylde-tabulate`'s `/__action__` pipe envelope
//! (the same shape `wylde_shared::ipc::dispatch_action` consumes). Helpers
//! translate the JSON reply into small Rust view-structs the View consumes,
//! so the rendering layer never sees `serde_json::Value` directly.
//!
//! Three verbs are used:
//!   * `tabulate.capabilities` — read once at open, to render the safety
//!     posture chip and gate the format picker.
//!   * `tabulate.probe`        — PHI-safe structure probe (shape only, never
//!     a cell value).
//!   * `tabulate.extract`      — write the spreadsheet and report where it
//!     landed.

use serde::Deserialize;
use serde_json::{json, Value};

pub const SVC_TABULATE: &str = "wylde-tabulate";

// ── capabilities (safety posture + format picker) ─────────────────────────

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct AtRest {
    #[serde(default)]
    pub app_level_encryption: bool,
    #[serde(default)]
    pub volume_fde_attested: bool,
    #[serde(default)]
    pub fde_required: bool,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Hipaa {
    #[serde(default)]
    pub network_blocked: bool,
    #[serde(default)]
    pub audit: bool,
    #[serde(default)]
    pub at_rest: AtRest,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Capabilities {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub llm_enabled: bool,
    #[serde(default)]
    pub output_formats: Vec<String>,
    #[serde(default)]
    pub hipaa: Hipaa,
}

// ── probe (STRUCTURE ONLY — never a cell value) ───────────────────────────

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct ProbeColumn {
    #[serde(default)]
    pub header: String,
    /// Inferred column type (`integer` / `number` / `date` / `bool` /
    /// `string` / `empty`). `type` is a keyword, so it is renamed.
    #[serde(rename = "type", default)]
    pub kind: String,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct ProbeTable {
    #[serde(default)]
    pub rows: u64,
    #[serde(default)]
    pub cols: u64,
    #[serde(default)]
    pub header_inferred: bool,
    #[serde(default)]
    pub columns: Vec<ProbeColumn>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct ProbeView {
    #[serde(default)]
    pub file_type: String,
    #[serde(default)]
    pub mime: String,
    /// `null` for deferred (semi/unstructured) tiers; a count otherwise.
    #[serde(default)]
    pub tables_detected: Option<u64>,
    #[serde(default)]
    pub tables: Vec<ProbeTable>,
    #[serde(default)]
    pub redaction_warning: String,
    /// Present for deferred / unparseable inputs.
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub audit_id: String,
}

// ── extract ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct ExtractTable {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub rows: u64,
    #[serde(default)]
    pub cols: u64,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct ExtractView {
    #[serde(default)]
    pub output_path: String,
    #[serde(default)]
    pub output_format: String,
    #[serde(default)]
    pub tier_used: String,
    #[serde(default)]
    pub format: String,
    #[serde(default)]
    pub tables: Vec<ExtractTable>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub needs_validation: bool,
    #[serde(default)]
    pub audit_id: String,
}

// ── verb helpers ──────────────────────────────────────────────────────────

async fn action(verb: &str, payload: Value) -> Result<Value, String> {
    wylde_gui_pipe::call(
        SVC_TABULATE,
        "POST",
        "/__action__",
        Some(json!({ "action": verb, "payload": payload })),
    )
    .await
}

/// Read the service capabilities (safety posture + output formats).
pub async fn capabilities() -> Result<Capabilities, String> {
    let v = action("tabulate.capabilities", json!({})).await?;
    serde_json::from_value(v).map_err(|e| format!("malformed capabilities: {e}"))
}

/// PHI-safe structure probe of the file at `input_path`.
pub async fn probe(input_path: String) -> Result<ProbeView, String> {
    let v = action("tabulate.probe", json!({ "input_path": input_path })).await?;
    serde_json::from_value(v).map_err(|e| format!("malformed probe: {e}"))
}

/// Extract the file at `input_path` into a spreadsheet of `output_format`
/// (`xlsx` / `csv`). No `output_path` is sent, so the service writes into its
/// own configured output dir and reports the absolute path back.
pub async fn extract(input_path: String, output_format: &str) -> Result<ExtractView, String> {
    let v = action(
        "tabulate.extract",
        json!({ "input_path": input_path, "output_format": output_format }),
    )
    .await?;
    serde_json::from_value(v).map_err(|e| format!("malformed extract outcome: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_parses_posture() {
        let v = json!({
            "status": "v1",
            "llm_enabled": false,
            "output_formats": ["xlsx", "csv"],
            "hipaa": {
                "network_blocked": true,
                "audit": true,
                "at_rest": {
                    "app_level_encryption": true,
                    "volume_fde_attested": false,
                    "fde_required": false,
                    "detail": "app-level DPAPI encryption ON; volume FDE not attested"
                }
            }
        });
        let c: Capabilities = serde_json::from_value(v).unwrap();
        assert_eq!(c.status, "v1");
        assert!(c.hipaa.network_blocked);
        assert!(c.hipaa.audit);
        assert!(c.hipaa.at_rest.app_level_encryption);
        assert!(!c.hipaa.at_rest.fde_required);
        assert_eq!(c.output_formats, vec!["xlsx", "csv"]);
    }

    #[test]
    fn probe_parses_structure_only() {
        let v = json!({
            "file_type": "csv",
            "mime": "text/csv",
            "tables_detected": 1,
            "tables": [{
                "rows": 3, "cols": 2, "header_inferred": true,
                "columns": [
                    { "header": "Patient Name", "type": "string" },
                    { "header": "DOB", "type": "date" }
                ]
            }],
            "redaction_warning": "STRUCTURE ONLY … REDACT …",
            "audit_id": "abc123"
        });
        let p: ProbeView = serde_json::from_value(v).unwrap();
        assert_eq!(p.file_type, "csv");
        assert_eq!(p.tables_detected, Some(1));
        assert_eq!(p.tables[0].cols, 2);
        assert_eq!(p.tables[0].columns[1].header, "DOB");
        assert_eq!(p.tables[0].columns[1].kind, "date");
        assert_eq!(p.audit_id, "abc123");
    }

    #[test]
    fn probe_handles_deferred_null_tables_detected() {
        let v = json!({
            "file_type": "pdf",
            "mime": "application/pdf",
            "tables_detected": null,
            "redaction_warning": "…",
            "note": "pdf is a deferred tier in v1",
            "audit_id": "z"
        });
        let p: ProbeView = serde_json::from_value(v).unwrap();
        assert_eq!(p.tables_detected, None);
        assert!(p.tables.is_empty());
        assert!(p.note.unwrap().contains("deferred"));
    }

    #[test]
    fn extract_parses_outcome() {
        let v = json!({
            "output_path": "C:/Users/x/WyldeData/tabulate/intake.xlsx",
            "output_format": "xlsx",
            "tier_used": "structured",
            "format": "csv",
            "tables": [{ "name": "Sheet1", "rows": 3, "cols": 5 }],
            "warnings": [],
            "needs_validation": true,
            "audit_id": "deadbeef"
        });
        let e: ExtractView = serde_json::from_value(v).unwrap();
        assert!(e.output_path.ends_with("intake.xlsx"));
        assert_eq!(e.tier_used, "structured");
        assert!(e.needs_validation);
        assert_eq!(e.tables[0].rows, 3);
    }
}

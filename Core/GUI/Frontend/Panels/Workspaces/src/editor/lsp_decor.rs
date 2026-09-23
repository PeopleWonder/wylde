//! LSP diagnostics → editor decorations (IDE S9).
//!
//! Converts `lsp.diagnostics` ranges (0-based `{line, character}`) into byte
//! ranges in the editor buffer and builds wavy-underline [`Decoration`]s
//! coloured by severity. Pure + tested. `character` is treated as a Unicode
//! scalar count (matching the editor's `cursor_position`), an exact match for
//! the BMP/ASCII that dominates code and a close approximation elsewhere.

use gpui::Rgba;
use serde_json::Value;
use wylde_gpui_code_editor::Decoration;
use wylde_theme::colors::{DANGER, TEXT_MUTED, WARNING};

/// Byte offset of 0-based `(line, character)` in `text`. Clamps gracefully
/// past the end of a line / the buffer.
pub fn byte_offset(text: &str, line: u32, character: u32) -> usize {
    // Find the start byte of `line`.
    let mut start = 0usize;
    let mut seen = 0u32;
    if line > 0 {
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                seen += 1;
                if seen == line {
                    start = i + 1;
                    break;
                }
            }
        }
        if seen < line {
            return text.len(); // line past EOF
        }
    }
    // Advance `character` Unicode scalars within the line (stop at newline/EOF).
    let mut off = start;
    for (idx, c) in text[start..].chars().enumerate() {
        if idx as u32 >= character || c == '\n' {
            break;
        }
        off += c.len_utf8();
    }
    off
}

/// Colour for an LSP severity (1=Error, 2=Warning, 3=Info, 4=Hint).
fn severity_color(severity: u64) -> Rgba {
    match severity {
        1 => DANGER,
        2 => WARNING,
        _ => TEXT_MUTED,
    }
}

/// Convert a `lsp.diagnostics` reply (`{diagnostics:[{range, severity,
/// message}]}`) into wavy-underline decorations over `text`. Underline-only so
/// the underlying syntax colour shows through.
pub fn diagnostics_to_decorations(text: &str, reply: &Value) -> Vec<Decoration> {
    let Some(diags) = reply.get("diagnostics").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(diags.len());
    for d in diags {
        let Some(range) = d.get("range") else {
            continue;
        };
        let (Some(s), Some(e)) = (range.get("start"), range.get("end")) else {
            continue;
        };
        let (Some(sl), Some(sc)) = (
            s.get("line").and_then(Value::as_u64),
            s.get("character").and_then(Value::as_u64),
        ) else {
            continue;
        };
        let (Some(el), Some(ec)) = (
            e.get("line").and_then(Value::as_u64),
            e.get("character").and_then(Value::as_u64),
        ) else {
            continue;
        };
        let start = byte_offset(text, sl as u32, sc as u32);
        let mut end = byte_offset(text, el as u32, ec as u32);
        // Zero-width diagnostics (e.g. at EOF) get a 1-char underline so they
        // are still visible.
        if end <= start {
            end = (start + 1).min(text.len());
        }
        if end <= start {
            continue;
        }
        let severity = d.get("severity").and_then(Value::as_u64).unwrap_or(1);
        out.push(Decoration::underline_only(
            start..end,
            severity_color(severity),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn byte_offset_maps_line_and_char() {
        let t = "fn main() {}\nlet x = 1;\n";
        assert_eq!(byte_offset(t, 0, 0), 0);
        assert_eq!(byte_offset(t, 0, 3), 3); // "fn "
        assert_eq!(byte_offset(t, 1, 0), 13); // start of line 1 (after the \n)
        assert_eq!(byte_offset(t, 1, 4), 17); // "let "
                                              // Past EOF clamps.
        assert_eq!(byte_offset(t, 99, 0), t.len());
        // Past end of line clamps to the newline.
        assert_eq!(byte_offset(t, 0, 999), 12);
    }

    #[test]
    fn byte_offset_handles_multibyte() {
        let t = "héllo"; // é is 2 bytes
        assert_eq!(byte_offset(t, 0, 1), 1); // after 'h'
        assert_eq!(byte_offset(t, 0, 2), 3); // after 'é' (2 bytes)
    }

    #[test]
    fn diagnostics_build_underline_decorations() {
        let text = "fn main() {\n  let x = 1;\n}\n";
        let reply = json!({ "diagnostics": [
            { "severity": 2, "message": "unused `x`",
              "range": { "start": {"line":1,"character":6}, "end": {"line":1,"character":7} } },
            { "severity": 1, "message": "boom",
              "range": { "start": {"line":0,"character":0}, "end": {"line":0,"character":2} } },
        ]});
        let decos = diagnostics_to_decorations(text, &reply);
        assert_eq!(decos.len(), 2);
        // Each is an underline-only decoration (no fill colour).
        assert!(decos
            .iter()
            .all(|d| d.color.is_none() && d.underline.is_some()));
    }

    #[test]
    fn empty_diagnostics_is_empty() {
        assert!(diagnostics_to_decorations("x", &json!({ "diagnostics": [] })).is_empty());
        assert!(diagnostics_to_decorations("x", &json!({})).is_empty());
    }
}

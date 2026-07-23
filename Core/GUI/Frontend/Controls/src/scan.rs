//! Static scan of a panel source for the control ids it builds.
//!
//! Dev-only, like [`crate::registry`] — the shipped Shell links neither.
//!
//! This is the static half of the "no falsely-covered control" guarantee
//! (#247). The registry tells a walk what *painted*; this tells it what the
//! source *claims to build*. A control whose id appears here but which no
//! walked state ever painted is one the walk silently missed — typically a
//! modal-gated control — and
//! `wylde_gui_test_support::control_walk::WalkReport::assert_covers_every_literal_id`
//! turns that into a red test naming the id.
//!
//! It lives beside the constructor rather than in the test-support crate on
//! purpose: that crate is EXCLUDED from the workspace and has no lock file, so
//! nothing there can be `cargo test`-ed in CI. A scanner whose own tests never
//! run is the #56 shape — enforcement enforced by nothing. Here it rides
//! `cargo panel-walk`, which builds and tests this crate.

/// Every string literal that appears as the id argument of a `control(` call.
///
/// Ids built at runtime (`format!("row::{}", id)`) are invisible here and are
/// covered by the per-item rows the fixture actually renders — see
/// [`WalkReport::assert_covers_every_literal_id`].
pub fn literal_control_ids(source: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let bytes = source.as_bytes();
    let mut i = 0usize;
    while let Some(pos) = source[i..].find("control(") {
        let start = i + pos;
        // Require a call, not a substring of a longer identifier
        // (`spawn_control(`, `my_control(`).
        let ok_prefix = start == 0 || {
            let c = bytes[start - 1] as char;
            !(c.is_alphanumeric() || c == '_')
        };
        i = start + "control(".len();
        if !ok_prefix {
            continue;
        }
        // Walk the argument list to its closing paren, capturing string
        // literals at depth 1. The id is the last one before the call closes.
        let mut depth = 1usize;
        let mut j = i;
        let mut last_literal: Option<String> = None;
        while j < bytes.len() && depth > 0 {
            match bytes[j] as char {
                '(' => depth += 1,
                ')' => depth -= 1,
                '"' => {
                    let mut k = j + 1;
                    let mut lit = String::new();
                    while k < bytes.len() && bytes[k] as char != '"' {
                        if bytes[k] as char == '\\' {
                            k += 1;
                        }
                        if k < bytes.len() {
                            lit.push(bytes[k] as char);
                        }
                        k += 1;
                    }
                    if depth == 1 {
                        last_literal = Some(lit);
                    }
                    j = k;
                }
                _ => {}
            }
            j += 1;
        }
        if let Some(lit) = last_literal {
            if !out.contains(&lit) {
                out.push(lit);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::literal_control_ids;

    #[test]
    fn finds_a_simple_literal_id() {
        assert_eq!(
            literal_control_ids(r#"control(div(), "tools-refresh")"#),
            vec!["tools-refresh".to_string()]
        );
    }

    #[test]
    fn finds_ids_across_nested_calls() {
        let src = r#"
            control(div().px_2().child(label("Save")), "settings-save")
            control(div(), "settings-reset")
        "#;
        assert_eq!(
            literal_control_ids(src),
            vec!["settings-save".to_string(), "settings-reset".to_string()]
        );
    }

    #[test]
    fn a_runtime_built_id_yields_no_literal() {
        // The row-per-item shape. Nothing to assert statically, so the scan
        // must report nothing rather than guess at a prefix.
        assert!(literal_control_ids(r#"control(div(), format!("ext::{}", n))"#).is_empty());
    }

    #[test]
    fn ignores_identifiers_that_merely_end_in_control() {
        // `spawn_control(` / `my_control(` are not the constructor.
        assert!(literal_control_ids(r#"spawn_control(cx, "not-a-control")"#).is_empty());
    }

    #[test]
    fn deduplicates_repeated_ids() {
        let src = r#"control(div(), "a") control(div(), "a") control(div(), "b")"#;
        assert_eq!(
            literal_control_ids(src),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn a_commented_out_control_is_still_reported() {
        // Deliberate: the scan is a cheap lexical net, not a parser. A false
        // "you never walked this" is loud and one edit to fix; a false clean
        // pass is the failure this whole mechanism exists to prevent.
        assert_eq!(
            literal_control_ids(r#"// control(div(), "old-button")"#),
            vec!["old-button".to_string()]
        );
    }
}

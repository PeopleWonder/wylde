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

/// Every static string id that a `control(` call in `source` builds.
///
/// The id is a `control()` call's **last argument**. This recognises the two
/// static shapes the codebase uses:
///
/// * a bare string literal — `control(div(), "models-refresh")`; and
/// * the wrapped form the migration left behind —
///   `control(div(), ElementId::Name("models-hf-close".into()))`.
///
/// Both yield the same rendered id (`ElementId::Name` renders to its inner
/// string), so both must be extracted — an earlier version only saw the bare
/// form and let every `ElementId::Name("…")` control escape the coverage
/// guard, which is how a modal control could go unwalked while the walk
/// reported success.
///
/// Runtime ids carry no static string and are deliberately NOT returned:
/// `format!("row::{}", id)` and the tuple form `("row", i)` (which renders to
/// `"row-{i}"`, not `"row"`). Those are covered by the per-item rows the
/// fixture actually renders — see
/// `crate`'s downstream `assert_covers_every_literal_id`.
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
        let open = start + "control(".len() - 1; // index of the '('
        i = start + "control(".len();
        if !ok_prefix {
            continue;
        }
        // Find the matching close paren and the last top-level comma, so the
        // id argument is `source[last_comma+1 .. close]`.
        let mut depth = 0usize;
        let mut j = open;
        let mut last_comma: Option<usize> = None;
        let mut close = None;
        while j < bytes.len() {
            match bytes[j] as char {
                '"' => {
                    j += 1;
                    while j < bytes.len() && bytes[j] as char != '"' {
                        if bytes[j] as char == '\\' {
                            j += 1;
                        }
                        j += 1;
                    }
                }
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(j);
                        break;
                    }
                }
                ',' if depth == 1 => last_comma = Some(j),
                _ => {}
            }
            j += 1;
        }
        let Some(close) = close else { continue };
        let arg_start = last_comma.map(|c| c + 1).unwrap_or(open + 1);
        if let Some(id) = static_id(source[arg_start..close].trim()) {
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    out
}

/// The static string id of a `control()` last-argument expression, if it has
/// one. `"x"` and `ElementId::Name("x".into())` both yield `x`; anything else
/// (a tuple, a `format!`, a bound variable) yields `None`.
fn static_id(arg: &str) -> Option<String> {
    let inner = arg
        .strip_prefix("ElementId::Name(")
        .map(|s| s.trim())
        .unwrap_or(arg);
    let inner = inner.strip_prefix('"')?;
    let end = inner.find('"')?;
    Some(inner[..end].to_string())
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
    fn finds_the_wrapped_element_id_name_form() {
        // The shape the migration left behind. The id literal sits at depth 2,
        // inside `ElementId::Name(...)`; an earlier scanner only saw depth-1
        // literals and let every one of these escape the coverage guard.
        assert_eq!(
            literal_control_ids(r#"control(div(), ElementId::Name("models-hf-close".into()))"#),
            vec!["models-hf-close".to_string()]
        );
    }

    #[test]
    fn takes_the_id_arg_not_a_child_label() {
        // The id is the LAST argument. A string literal in an earlier argument
        // (a child label) must not be mistaken for the id.
        assert_eq!(
            literal_control_ids(r#"control(div().child("Save"), "settings-save")"#),
            vec!["settings-save".to_string()]
        );
    }

    #[test]
    fn a_runtime_built_id_yields_no_literal() {
        // The row-per-item shape. Nothing to assert statically, so the scan
        // must report nothing rather than guess at a prefix.
        assert!(literal_control_ids(r#"control(div(), format!("ext::{}", n))"#).is_empty());
    }

    #[test]
    fn a_tuple_id_yields_no_literal() {
        // `("row", i)` renders to `"row-{i}"`, a runtime id — NOT "row". The
        // scan must not capture the tuple's name part, or it would demand
        // coverage of an id that never exists.
        assert!(literal_control_ids(r#"control(div(), ("row", i))"#).is_empty());
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

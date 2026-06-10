//! Vocabulary editor models (Slice N, Build Order §4 `vocabulary/editor`):
//! the pure halves of the edit + promote flows. gpui-free and unit-tested;
//! the tab renders them.
//!
//! * [`parse_aliases`] — the alias field is a comma-separated input; parsing
//!   normalises whitespace and drops empties/dupes (server-side
//!   `validate_aliases` remains the authority — this is the friendly local
//!   pass).
//! * [`PromotionDialog`] — the OI-5 collision flow (Plan §4.4): promoting
//!   `{{X}}` when `{{X}}` already exists globally surfaces rename-with-suffix
//!   (default) / keep-workspace-only / replace-with-explicit-confirm. No
//!   silent overwrites; the data layer never decides.

/// Parse the editor's comma-separated alias field: trim, collapse inner
/// whitespace, drop empties and case-insensitive duplicates (first wins).
pub fn parse_aliases(input: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in input.split(',') {
        let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() {
            continue;
        }
        if out.iter().any(|e| e.eq_ignore_ascii_case(&collapsed)) {
            continue;
        }
        out.push(collapsed);
    }
    out
}

/// The OI-5 promotion-collision dialog state.
#[derive(Clone, Debug, PartialEq)]
pub enum PromotionDialog {
    /// No promotion in flight.
    Idle,
    /// The global store reported `already_exists_global`: the user picks.
    /// `rename_to` is pre-filled with the suffix default and editable.
    Collision {
        identifier: String,
        existing_definition: String,
        rename_to: String,
    },
    /// The user chose Replace — one more explicit confirmation (Plan §4.4:
    /// "Replace requires explicit confirmation").
    ConfirmReplace { identifier: String },
}

impl PromotionDialog {
    /// Build the collision state with the spec's default rename
    /// (`{{X_<workspace_suffix>}}`). The suffix is the workspace id's last
    /// path-ish segment, sanitised to identifier characters.
    pub fn collision(identifier: &str, existing_definition: &str, workspace_id: &str) -> Self {
        PromotionDialog::Collision {
            identifier: identifier.to_owned(),
            existing_definition: existing_definition.to_owned(),
            rename_to: format!("{identifier}_{}", workspace_suffix(workspace_id)),
        }
    }
}

/// A workspace id reduced to an identifier-safe suffix: the last `/`/`\`
/// segment with every non-`[A-Za-z0-9_]` byte mapped to `_`, lowercased.
pub fn workspace_suffix(workspace_id: &str) -> String {
    let last = workspace_id
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(workspace_id);
    let mut s: String = last
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        s.push_str("ws");
    }
    s
}

/// Extract the existing global definition out of an `already_exists_global`
/// error string (the verb embeds the existing record's description in its
/// details). Falls back to the whole message — the dialog always has
/// *something* to show.
pub fn existing_definition_from_error(err: &str) -> String {
    // The error detail convention: `… existing definition: <text>`.
    err.split("definition:")
        .nth(1)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(err)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_parse_trim_collapse_dedupe() {
        assert_eq!(
            parse_aliases("wire format,  set   active , Wire Format, ,pipe"),
            vec!["wire format", "set active", "pipe"]
        );
        assert!(parse_aliases("  ,  ,").is_empty());
        assert!(parse_aliases("").is_empty());
    }

    #[test]
    fn collision_prefills_the_spec_default_rename() {
        let d = PromotionDialog::collision("the_pipe", "old def", "C:/ws/Wylde-release");
        match d {
            PromotionDialog::Collision {
                identifier,
                existing_definition,
                rename_to,
            } => {
                assert_eq!(identifier, "the_pipe");
                assert_eq!(existing_definition, "old def");
                assert_eq!(rename_to, "the_pipe_wylde_release");
            }
            _ => panic!("expected Collision"),
        }
    }

    #[test]
    fn workspace_suffix_sanitises() {
        assert_eq!(workspace_suffix("C:/dev/My Project!"), "my_project_");
        assert_eq!(workspace_suffix("plain_ws"), "plain_ws");
        assert_eq!(workspace_suffix(""), "ws");
    }

    #[test]
    fn existing_definition_extracts_or_falls_back() {
        assert_eq!(
            existing_definition_from_error(
                "already_exists_global: {{x}} exists; existing definition: the old text"
            ),
            "the old text"
        );
        let opaque = "already_exists_global";
        assert_eq!(existing_definition_from_error(opaque), opaque);
    }
}

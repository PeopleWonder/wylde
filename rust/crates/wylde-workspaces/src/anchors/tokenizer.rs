//! `{{identifier}}` tokenizing for the workspace anchor surface.
//!
//! The grammar + parsing live in [`wylde_shared::anchor_tokenizer`] (lifted
//! there so the harness chat input and this service share one implementation).
//! This module re-exports those primitives and adds the workspace-specific
//! step: resolving each parsed token against the workspace's anchor store.

pub use wylde_shared::anchor_tokenizer::{
    is_valid_identifier, normalize_lookup_token, normalize_token, parse_anchors, parse_qualifiers,
    QualifierSpan, TokenSpan,
};

use super::anchor::Anchor;
use super::store;

/// One recognised token in a piece of text plus the workspace anchors it
/// resolves to (0, 1, or — across future merges — many).
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedToken {
    pub span: TokenSpan,
    pub anchors: Vec<Anchor>,
}

/// Parse every `{{token}}` in `text` and resolve each against `workspace_id`'s
/// anchor store. The basis for verb routing and (later) composer highlighting.
pub fn resolve_tokens(workspace_id: &str, text: &str) -> Vec<ResolvedToken> {
    parse_anchors(text)
        .into_iter()
        .map(|span| {
            let anchors = store::find_by_token(workspace_id, &span.identifier);
            ResolvedToken { span, anchors }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchors::anchor::{workspace_anchor, AnchorKind, AnchorTarget};
    use crate::test_support::TestEnv;

    #[test]
    fn resolve_tokens_links_to_stored_anchors() {
        let _env = TestEnv::new();
        let ws = "ws-resolve-000000";
        store::create(
            ws,
            workspace_anchor(
                ws,
                "set_active",
                AnchorKind::Concept,
                AnchorTarget::Concept { text: "t".into() },
                "d",
            ),
        )
        .unwrap();

        let resolved = resolve_tokens(ws, "call {{set_active}} then {{unknown_one}}");
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].span.identifier, "set_active");
        assert_eq!(resolved[0].anchors.len(), 1, "stored token resolves");
        assert_eq!(resolved[1].span.identifier, "unknown_one");
        assert!(
            resolved[1].anchors.is_empty(),
            "unknown token has no anchor"
        );
    }
}

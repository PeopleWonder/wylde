//! The workspace-scoped anchor surface.
//!
//! The [`Anchor`] data model itself lives in [`wylde_shared::anchor`] (see
//! that module for *why* — the harness global store can't depend on the
//! `wylde-workspaces` service lib, so the type sits in the crate both sides
//! share). This module re-exports those names so the Build Order's file path
//! (`wylde-workspaces/src/anchors/anchor.rs`) stays meaningful, and adds the
//! workspace-scope constructor.

pub use wylde_shared::anchor::{
    already_exists_global_details, epoch_now, validate_aliases, AliasError, Anchor, AnchorKind,
    AnchorScope, AnchorTarget, SymbolId,
};

/// Build a workspace-scoped anchor — stamps `scope = Workspace(workspace_id)`
/// so the per-workspace store always owns a correctly-scoped record regardless
/// of what the caller passed.
pub fn workspace_anchor(
    workspace_id: &str,
    identifier: impl Into<String>,
    kind: AnchorKind,
    target: AnchorTarget,
    description: impl Into<String>,
) -> Anchor {
    Anchor::new(
        identifier,
        kind,
        target,
        AnchorScope::Workspace {
            workspace_id: workspace_id.to_owned(),
        },
        description,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_anchor_is_workspace_scoped() {
        let a = workspace_anchor(
            "ws-9",
            "thing",
            AnchorKind::Concept,
            AnchorTarget::Concept { text: "t".into() },
            "d",
        );
        assert_eq!(a.scope.workspace_id(), Some("ws-9"));
        assert_eq!(a.identifier, "thing");
    }
}

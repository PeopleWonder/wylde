//! The anchor data model — the fundamental unit of attention in the
//! Thought Bubble System (Plan v2 §4.3).
//!
//! An **anchor** is a named handle (`{{identifier}}`) on a code symbol, a
//! concept, a convention, or a person. Symbols, persistent vocabulary, and
//! ephemeral context are all anchors in different *states*.
//!
//! ## Why this lives in `wylde-shared`
//!
//! The Build Order struct index names `wylde-workspaces` as the host crate
//! for [`Anchor`], and the *workspace*-scoped store does live there
//! (`wylde-workspaces/src/anchors/`). But the **global** anchor store lives
//! in the harness (`wylde-harness/src/global_anchors/`), and the harness is
//! a *pure consumer* of the workspaces service — it depends only on
//! `wylde-workspaces-client`, never on the `wylde-workspaces` service lib.
//! For both sides to return **byte-identical shapes** (a hard requirement of
//! Slice N-data) the type has to sit in a crate they both already depend on:
//! `wylde-shared`. `wylde-workspaces::anchors::anchor` re-exports these names
//! so the spec's file path stays meaningful.
//!
//! Timestamps follow the harness convention (`f64` epoch seconds, the same as
//! `NoteEntry` / `registry::epoch_now`), not a `chrono::DateTime`, so anchors
//! round-trip through JSON exactly and need no new dependency.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::anchor_tokenizer::is_valid_identifier;

/// A stable symbol identifier. In the v1 code graph this is the entity name
/// (which is also the graph node id — see
/// `wylde_workspaces::graph::symbol_index::SymbolEntry::id`). Kept as a type
/// alias rather than a newtype so it stays wire-transparent and the workspace
/// symbol index can feed it straight through.
pub type SymbolId = String;

/// What *kind* of thing an anchor names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchorKind {
    /// A code entity (function, class, module) identified by a [`SymbolId`].
    CodeSymbol,
    /// A free-text idea with no single code target (e.g. "the pipe protocol").
    Concept,
    /// A team/project convention (e.g. "we always atomic-write JSON stores").
    Convention,
    /// A person (collaborator, reviewer, the user themself).
    Person,
}

/// What an anchor *points at*. A [`AnchorKind::CodeSymbol`] anchor targets a
/// symbol; everything else targets a textual definition.
///
/// Serialised internally-tagged so the wire shape is self-describing and
/// stable across the workspace + global stores:
/// `{"type":"code_symbol","symbol_id":"…"}` or
/// `{"type":"concept","text":"…"}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnchorTarget {
    /// Points at a code symbol by its stable id.
    CodeSymbol { symbol_id: SymbolId },
    /// Points at a free-text definition (concepts/conventions/people).
    Concept { text: String },
}

impl AnchorTarget {
    /// The [`SymbolId`] this target references, if it is a code symbol.
    /// Used by the inverse lookup (`find_by_target`, OI-20).
    pub fn symbol_id(&self) -> Option<&str> {
        match self {
            AnchorTarget::CodeSymbol { symbol_id } => Some(symbol_id),
            AnchorTarget::Concept { .. } => None,
        }
    }
}

/// The persistence/visibility boundary of an anchor.
///
/// Serialised internally-tagged: `{"scope":"workspace","workspace_id":"…"}`
/// or `{"scope":"global"}`. The store that owns an anchor sets this — the
/// per-workspace store stamps [`Workspace`](AnchorScope::Workspace), the
/// harness global store stamps [`Global`](AnchorScope::Global).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum AnchorScope {
    /// Saved to one workspace's store (`wylde-workspaces`).
    Workspace { workspace_id: String },
    /// Promoted to the global store (`wylde-harness/global_anchors`).
    Global,
}

impl AnchorScope {
    /// The owning workspace id, if this is a workspace-scoped anchor.
    pub fn workspace_id(&self) -> Option<&str> {
        match self {
            AnchorScope::Workspace { workspace_id } => Some(workspace_id),
            AnchorScope::Global => None,
        }
    }

    /// Whether this is the global scope.
    pub fn is_global(&self) -> bool {
        matches!(self, AnchorScope::Global)
    }
}

/// The fundamental unit of attention (Plan v2 §4.3). One named, persisted
/// handle the LLM and the user co-author.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Anchor {
    /// The `{{identifier}}` token — alphanumeric + underscore, no spaces.
    /// e.g. `"set_active_graph_view"`.
    pub identifier: String,

    /// Symbol / concept / convention / person.
    pub kind: AnchorKind,

    /// What the anchor points at.
    pub target: AnchorTarget,

    /// Persistence/visibility boundary (workspace vs global).
    pub scope: AnchorScope,

    /// Human-readable definition shown in bubbles / the Vocabulary tab.
    pub description: String,

    /// Semantic-graph edges to other anchors (their identifiers). The
    /// peer-to-peer connection flow (OI-22) appends here.
    #[serde(default)]
    pub related_to: Vec<String>,

    /// Taxonomy parent (anchor hierarchy, OI-19). `None` at the root.
    #[serde(default)]
    pub parent_anchor: Option<String>,

    /// Free-text domain tag with a suggested vocabulary (OI-23): Networking,
    /// UI, Storage, Auth, ….
    #[serde(default)]
    pub domain: Option<String>,

    /// Creation time (epoch seconds, harness convention).
    #[serde(default)]
    pub created_at: f64,

    /// Last time the anchor was surfaced/used — feeds the Recommended Cleanup
    /// surface (OI-21) and the promotion heuristic.
    #[serde(default)]
    pub last_used_at: f64,

    /// How many times the anchor has been used; input to the workspace→global
    /// promotion-prompt heuristic (Plan v2 §4.4).
    #[serde(default)]
    pub usage_count: u32,
}

impl Anchor {
    /// Build a fresh anchor, stamping `created_at`/`last_used_at` to now.
    /// `identifier` is **not** validated here — callers (the verb handlers)
    /// validate via [`is_valid_identifier`] and return `bad_request` on a bad
    /// token, so a construction never silently accepts garbage.
    pub fn new(
        identifier: impl Into<String>,
        kind: AnchorKind,
        target: AnchorTarget,
        scope: AnchorScope,
        description: impl Into<String>,
    ) -> Self {
        let now = epoch_now();
        Self {
            identifier: identifier.into(),
            kind,
            target,
            scope,
            description: description.into(),
            related_to: Vec::new(),
            parent_anchor: None,
            domain: None,
            created_at: now,
            last_used_at: now,
            usage_count: 0,
        }
    }

    /// Whether this anchor's `identifier` is a well-formed token.
    pub fn has_valid_identifier(&self) -> bool {
        is_valid_identifier(&self.identifier)
    }

    /// Record one use: bump `usage_count` and re-stamp `last_used_at`.
    pub fn record_use(&mut self) {
        self.usage_count = self.usage_count.saturating_add(1);
        self.last_used_at = epoch_now();
    }

    /// The JSON wire shape returned by every `anchors.*` verb (workspace and
    /// global). This is exactly the serde representation — the verbs use it so
    /// both stores are guaranteed identical.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// Unix epoch seconds as `f64`, rounded to milliseconds so values round-trip
/// through a JSON serialize→parse exactly (full nanosecond `as_secs_f64`
/// precision sits at the f64 significand boundary for epoch-scale values and
/// can shift 1 ULP). Mirrors `wylde_workspaces::registry::epoch_now`.
pub fn epoch_now() -> f64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    (secs * 1000.0).round() / 1000.0
}

/// Build the structured `details` payload for an `already_exists_global`
/// collision error (OI-5). The data layer returns this; the GUI Vocabulary tab
/// renders the rename / keep-workspace-only / replace dialog from it.
pub fn already_exists_global_details(existing: &Anchor) -> Value {
    json!({
        "identifier": existing.identifier,
        "existing_definition": existing.description,
        "existing": existing.to_value(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Anchor {
        Anchor::new(
            "set_active_graph_view",
            AnchorKind::CodeSymbol,
            AnchorTarget::CodeSymbol {
                symbol_id: "set_active_graph_view".into(),
            },
            AnchorScope::Workspace {
                workspace_id: "ws-123".into(),
            },
            "Switches the graph panel to the active workspace view.",
        )
    }

    #[test]
    fn json_round_trips_exactly() {
        let a = sample();
        let raw = serde_json::to_string(&a).unwrap();
        let back: Anchor = serde_json::from_str(&raw).unwrap();
        assert_eq!(a, back);
    }

    #[test]
    fn target_wire_shape_is_tagged() {
        let a = sample();
        let v = a.to_value();
        assert_eq!(v["target"]["type"], "code_symbol");
        assert_eq!(v["target"]["symbol_id"], "set_active_graph_view");
        assert_eq!(v["scope"]["scope"], "workspace");
        assert_eq!(v["scope"]["workspace_id"], "ws-123");
        assert_eq!(v["kind"], "code_symbol");
    }

    #[test]
    fn concept_target_serialises_text() {
        let a = Anchor::new(
            "the_pipe_protocol",
            AnchorKind::Concept,
            AnchorTarget::Concept {
                text: "msgpack-framed named-pipe IPC".into(),
            },
            AnchorScope::Global,
            "How services talk.",
        );
        let v = a.to_value();
        assert_eq!(v["target"]["type"], "concept");
        assert_eq!(v["target"]["text"], "msgpack-framed named-pipe IPC");
        assert_eq!(v["scope"]["scope"], "global");
    }

    #[test]
    fn symbol_id_inverse_accessor() {
        let a = sample();
        assert_eq!(a.target.symbol_id(), Some("set_active_graph_view"));
        let c = AnchorTarget::Concept { text: "x".into() };
        assert_eq!(c.symbol_id(), None);
    }

    #[test]
    fn record_use_bumps_count_and_stamp() {
        let mut a = sample();
        assert_eq!(a.usage_count, 0);
        a.record_use();
        a.record_use();
        assert_eq!(a.usage_count, 2);
        assert!(a.last_used_at >= a.created_at);
    }

    #[test]
    fn defaults_fill_missing_optional_fields() {
        // A minimal stored record (pre-hierarchy/domain) still loads.
        let raw = r#"{
            "identifier":"x",
            "kind":"concept",
            "target":{"type":"concept","text":"t"},
            "scope":{"scope":"global"},
            "description":"d"
        }"#;
        let a: Anchor = serde_json::from_str(raw).unwrap();
        assert!(a.related_to.is_empty());
        assert!(a.parent_anchor.is_none());
        assert!(a.domain.is_none());
        assert_eq!(a.usage_count, 0);
    }

    #[test]
    fn collision_details_carry_existing_definition() {
        let a = sample();
        let d = already_exists_global_details(&a);
        assert_eq!(d["identifier"], "set_active_graph_view");
        assert_eq!(
            d["existing_definition"],
            "Switches the graph panel to the active workspace view."
        );
        assert_eq!(d["existing"]["identifier"], "set_active_graph_view");
    }

    #[test]
    fn scope_accessors() {
        assert_eq!(
            AnchorScope::Workspace {
                workspace_id: "w".into()
            }
            .workspace_id(),
            Some("w")
        );
        assert!(AnchorScope::Global.is_global());
        assert_eq!(AnchorScope::Global.workspace_id(), None);
    }
}

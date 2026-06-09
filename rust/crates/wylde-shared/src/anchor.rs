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

use crate::anchor_tokenizer::{collapse_whitespace, is_valid_identifier};
use crate::ipc::IpcError;

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

    /// Human-friendly alternate names that also resolve to this anchor in
    /// [`find_by_token`](crate)-style lookups (Slice N-data-aliases). Unlike
    /// [`identifier`](Anchor::identifier), an alias **may contain spaces** —
    /// e.g. `"set active"` aliasing `"set_active_graph_view"`. Aliases are
    /// stored whitespace-normalised ([`validate_aliases`]) and are *alternate
    /// lookup keys only*: the canonical [`identifier`](Anchor::identifier) is
    /// always what gets rendered/returned as the match name. Defaults to empty
    /// for anchors written before this field existed (no migration needed).
    #[serde(default)]
    pub aliases: Vec<String>,

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
            aliases: Vec::new(),
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

    /// Whether `normalized_token` resolves to this anchor — matching either its
    /// canonical [`identifier`](Anchor::identifier) or one of its
    /// [`aliases`](Anchor::aliases) (Slice N-data-aliases). The caller must pass
    /// an already-normalised token ([`crate::anchor_tokenizer::normalize_lookup_token`]);
    /// stored aliases are normalised at write time, so a direct equality is
    /// correct.
    pub fn matches_token(&self, normalized_token: &str) -> bool {
        self.identifier == normalized_token
            || self.aliases.iter().any(|a| a == normalized_token)
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

/// Maximum length (in characters) of a single normalised alias. Caps abuse
/// (a pathological multi-kilobyte "alias" that would bloat the store and every
/// lookup scan) while staying comfortably above any human-friendly name.
pub const MAX_ALIAS_LEN: usize = 64;

/// Why a candidate alias set was rejected (Slice N-data-aliases). The data
/// layer ([`validate_aliases`]) returns this; the verb handlers turn it into an
/// [`IpcError`] via [`AliasError::into_ipc`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AliasError {
    /// An alias was empty or whitespace-only after normalisation.
    Empty,
    /// An alias exceeded [`MAX_ALIAS_LEN`] characters.
    TooLong { alias: String },
    /// An alias equalled the anchor's own `identifier` (redundant — it would
    /// just resolve as the identifier).
    SelfCollision { alias: String },
    /// An alias collided with another anchor's `identifier` or one of its
    /// `aliases` within the same scope. `owned_by` is that anchor's identifier
    /// (an anchor's identity *is* its identifier — there is no separate id).
    Collision {
        conflicting_alias: String,
        owned_by: String,
    },
}

impl AliasError {
    /// The IPC error code: `alias_collision` for a cross-anchor collision (so
    /// callers/GUI can special-case it), `bad_request` for the input-shape
    /// rejections (empty / too long / self-collision).
    pub fn code(&self) -> &'static str {
        match self {
            AliasError::Collision { .. } => "alias_collision",
            _ => "bad_request",
        }
    }

    /// A human-readable message for the error.
    pub fn message(&self) -> String {
        match self {
            AliasError::Empty => "alias must not be empty or whitespace-only".to_owned(),
            AliasError::TooLong { alias } => format!(
                "alias {alias:?} exceeds the {MAX_ALIAS_LEN}-character limit"
            ),
            AliasError::SelfCollision { alias } => format!(
                "alias {alias:?} must not equal the anchor's own identifier"
            ),
            AliasError::Collision {
                conflicting_alias,
                owned_by,
            } => format!(
                "alias {conflicting_alias:?} already belongs to anchor {owned_by:?} in this scope"
            ),
        }
    }

    /// The structured `details` payload (only the cross-anchor collision carries
    /// one — `{conflicting_alias, owned_by}` — matching the brief's
    /// `AliasCollision` shape).
    pub fn details(&self) -> Option<Value> {
        match self {
            AliasError::Collision {
                conflicting_alias,
                owned_by,
            } => Some(json!({
                "conflicting_alias": conflicting_alias,
                "owned_by": owned_by,
            })),
            _ => None,
        }
    }

    /// Convert to the IPC error the verb handlers reply with.
    pub fn into_ipc(self) -> IpcError {
        IpcError {
            code: self.code().to_owned(),
            message: self.message(),
            details: self.details(),
        }
    }
}

/// Normalise + validate a candidate alias list for an anchor against the rest
/// of its scope (Slice N-data-aliases). Returns the cleaned, de-duplicated
/// alias list on success, or the first [`AliasError`].
///
/// Rules (in order, per alias):
/// 1. **Normalise** whitespace ([`collapse_whitespace`]): trim, collapse
///    internal runs to a single space.
/// 2. **Reject empty / whitespace-only** → [`AliasError::Empty`].
/// 3. **Length cap** [`MAX_ALIAS_LEN`] → [`AliasError::TooLong`].
/// 4. **Self-collision** (equals `own_identifier`) → [`AliasError::SelfCollision`].
/// 5. **Cross-anchor collision** against any *other* anchor's `identifier` or
///    `aliases` in `existing` → [`AliasError::Collision`]. The anchor being
///    edited is identified by `own_identifier` and skipped, so re-saving an
///    anchor's own aliases never trips on itself (covers create *and* update).
///
/// Duplicates **within** the candidate list collapse silently (first-seen
/// wins).
pub fn validate_aliases(
    own_identifier: &str,
    raw_aliases: &[String],
    existing: &[Anchor],
) -> Result<Vec<String>, AliasError> {
    let mut out: Vec<String> = Vec::new();
    for raw in raw_aliases {
        let alias = collapse_whitespace(raw);
        if alias.is_empty() {
            return Err(AliasError::Empty);
        }
        if alias.chars().count() > MAX_ALIAS_LEN {
            return Err(AliasError::TooLong { alias });
        }
        if alias == own_identifier {
            return Err(AliasError::SelfCollision { alias });
        }
        // Cross-anchor collision: scan every *other* anchor's identifier + its
        // aliases.
        for other in existing {
            if other.identifier == own_identifier {
                continue; // skip self (the update case)
            }
            if other.identifier == alias || other.aliases.iter().any(|a| a == &alias) {
                return Err(AliasError::Collision {
                    conflicting_alias: alias,
                    owned_by: other.identifier.clone(),
                });
            }
        }
        // De-dupe within the candidate list (first-seen wins).
        if !out.contains(&alias) {
            out.push(alias);
        }
    }
    Ok(out)
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
        // A minimal stored record (pre-hierarchy/domain/aliases) still loads.
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
        assert!(a.aliases.is_empty(), "aliases default to empty (backward compat)");
        assert_eq!(a.usage_count, 0);
    }

    #[test]
    fn aliases_round_trip_through_json() {
        let mut a = sample();
        a.aliases = vec!["set active".into(), "graph view".into()];
        let raw = serde_json::to_string(&a).unwrap();
        assert!(raw.contains("set active"), "aliases serialise: {raw}");
        let back: Anchor = serde_json::from_str(&raw).unwrap();
        assert_eq!(a, back);
        assert_eq!(back.aliases, vec!["set active", "graph view"]);
    }

    #[test]
    fn matches_token_hits_identifier_or_alias() {
        let mut a = sample();
        a.aliases = vec!["set active".into()];
        assert!(a.matches_token("set_active_graph_view"), "identifier");
        assert!(a.matches_token("set active"), "alias");
        assert!(!a.matches_token("nope"));
    }

    #[test]
    fn validate_aliases_normalizes_and_dedupes() {
        // Whitespace collapses; an exact duplicate (after normalisation) drops.
        let norm = validate_aliases(
            "the_anchor",
            &["  set   active ".into(), "set active".into(), "other".into()],
            &[],
        )
        .expect("valid");
        assert_eq!(norm, vec!["set active", "other"]);
    }

    #[test]
    fn validate_aliases_rejects_empty_and_overlong() {
        assert_eq!(
            validate_aliases("a", &["   ".into()], &[]),
            Err(AliasError::Empty)
        );
        let long = "x".repeat(MAX_ALIAS_LEN + 1);
        assert!(matches!(
            validate_aliases("a", &[long], &[]),
            Err(AliasError::TooLong { .. })
        ));
        // Exactly the cap is allowed.
        let at_cap = "y".repeat(MAX_ALIAS_LEN);
        assert!(validate_aliases("a", &[at_cap], &[]).is_ok());
    }

    #[test]
    fn validate_aliases_rejects_self_collision() {
        assert_eq!(
            validate_aliases("set_active", &["set_active".into()], &[]),
            Err(AliasError::SelfCollision {
                alias: "set_active".into()
            })
        );
    }

    #[test]
    fn validate_aliases_rejects_collision_with_other_identifier_or_alias() {
        let mut other = sample();
        other.identifier = "existing_anchor".into();
        other.aliases = vec!["already taken".into()];

        // Collision with another anchor's identifier.
        assert_eq!(
            validate_aliases("mine", &["existing_anchor".into()], std::slice::from_ref(&other)),
            Err(AliasError::Collision {
                conflicting_alias: "existing_anchor".into(),
                owned_by: "existing_anchor".into(),
            })
        );
        // Collision with another anchor's alias (whitespace-normalised match).
        assert_eq!(
            validate_aliases("mine", &["already   taken".into()], std::slice::from_ref(&other)),
            Err(AliasError::Collision {
                conflicting_alias: "already taken".into(),
                owned_by: "existing_anchor".into(),
            })
        );
    }

    #[test]
    fn validate_aliases_skips_self_so_update_can_resave() {
        // An anchor already in the store re-validating its own aliases must not
        // collide with itself (the update case).
        let mut me = sample();
        me.identifier = "set_active_graph_view".into();
        me.aliases = vec!["set active".into()];
        let norm = validate_aliases(
            "set_active_graph_view",
            &["set active".into(), "graph".into()],
            std::slice::from_ref(&me),
        )
        .expect("re-saving own aliases is fine");
        assert_eq!(norm, vec!["set active", "graph"]);
    }

    #[test]
    fn alias_error_into_ipc_carries_collision_details() {
        let e = AliasError::Collision {
            conflicting_alias: "foo".into(),
            owned_by: "bar".into(),
        };
        let ipc = e.into_ipc();
        assert_eq!(ipc.code, "alias_collision");
        let d = ipc.details.expect("collision details");
        assert_eq!(d["conflicting_alias"], "foo");
        assert_eq!(d["owned_by"], "bar");
        // The shape rejections are bad_request with no details.
        assert_eq!(AliasError::Empty.into_ipc().code, "bad_request");
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

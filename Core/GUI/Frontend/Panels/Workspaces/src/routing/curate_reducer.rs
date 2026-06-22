//! Pure view-model for the curate-before-inject menu (concept-routing **R2**,
//! plan §4; relation-model addendum §4.1) — the testable logic the gpui
//! [`super::CurateMenuView`] renders, kept gpui-free so it unit-tests without a
//! window. Mirrors the crate's `curation::candidate`/`apply` shaping (the GUI
//! doesn't link the service crate) plus the per-conversation **cadence**:
//! interactive on the first turn, auto-reuse after, with a re-open control.

use super::curate_ipc::{CandidateSetView, ProvenanceView, RoutedConceptView};

/// Estimated tokens one injected concept costs (blurb + member-snippet share) —
/// mirrors `wylde_concept_routing::curation::candidate::PER_CONCEPT_INJECT_TOKENS`
/// so the menu's budget indicator agrees with the server's eviction.
pub const PER_CONCEPT_INJECT_TOKENS: usize = 230;

/// How a menu row reads — the explainable annotation (glyph + greying).
/// Mirrors `wylde_concept_routing::MenuAnnotation`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CurateAnnotation {
    /// Cleared the cutoff on its own / via the graph — checked by default (`★`).
    Activated,
    /// Pulled in via a dependency edge — auto-pulled, checked by default (`↳`).
    Dependency,
    /// Lifted by a matched vocab term (`↑`).
    SeedLift,
    /// Co-activated by a positive edge (`+`).
    Positive,
    /// Suppressed by an exclusion edge — greyed, unchecked, re-addable (`⊘`).
    Excluded,
    /// Below the cutoff, no relation reshaping — unchecked, addable (`·`).
    Suppressed,
}

impl CurateAnnotation {
    pub fn glyph(self) -> &'static str {
        match self {
            CurateAnnotation::Activated => "★",
            CurateAnnotation::Dependency => "↳",
            CurateAnnotation::SeedLift => "↑",
            CurateAnnotation::Positive => "+",
            CurateAnnotation::Excluded => "⊘",
            CurateAnnotation::Suppressed => "·",
        }
    }
    /// Whether this row reads greyed/suppressed (exclusions).
    pub fn is_greyed(self) -> bool {
        matches!(self, CurateAnnotation::Excluded)
    }
}

/// Whether a row is a routed concept (drives injection) or a matched vocab term
/// (shown for transparency; injected via the existing anchors slot).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    Concept,
    Vocab,
}

/// One menu row.
#[derive(Clone, Debug, PartialEq)]
pub struct CurateRow {
    pub kind: RowKind,
    /// Concept id (or vocab identifier) — the key carried back in the curated
    /// list to `chat.run_turn`.
    pub key: String,
    pub label: String,
    pub score: f32,
    pub annotation: CurateAnnotation,
    /// The other endpoint a relation-driven row activated via (label form).
    pub via: Option<String>,
    /// Current checked state (concepts only meaningfully; vocab is display).
    pub checked: bool,
    /// Estimated injection token cost (`0` for vocab).
    pub est_tokens: usize,
}

impl CurateRow {
    pub fn is_concept(&self) -> bool {
        matches!(self.kind, RowKind::Concept)
    }
}

/// The menu model — rows + the token budget the checked set is measured
/// against. Concepts first (settled-score desc, the wire order), then vocab.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CurateMenuModel {
    pub query_echo: String,
    pub rows: Vec<CurateRow>,
    pub token_budget: usize,
}

impl CurateMenuModel {
    /// Build the menu from a routed candidate set. Pre-checks the activated set
    /// (which already folds in the dependency / seed-lift that cleared the
    /// cutoff); exclusions + below-cutoff seeds start unchecked but present.
    pub fn from_candidates(set: &CandidateSetView, token_budget: usize) -> Self {
        let mut rows: Vec<CurateRow> = set
            .concepts
            .iter()
            .map(|c| {
                let annotation = annotate(c);
                CurateRow {
                    kind: RowKind::Concept,
                    key: c.id.clone(),
                    label: c.label.clone(),
                    score: c.score,
                    annotation,
                    via: c.provenance.via().map(via_label),
                    checked: c.activated,
                    est_tokens: PER_CONCEPT_INJECT_TOKENS,
                }
            })
            .collect();
        rows.extend(set.vocabulary.iter().map(|v| CurateRow {
            kind: RowKind::Vocab,
            key: v.identifier.clone(),
            label: format!("{{{{{}}}}}", v.identifier),
            score: v.score,
            annotation: CurateAnnotation::Activated,
            via: None,
            checked: false,
            est_tokens: 0,
        }));
        CurateMenuModel {
            query_echo: set.query_echo.clone(),
            rows,
            token_budget,
        }
    }

    /// Toggle a concept row's checked state by key (no-op for vocab / unknown).
    pub fn toggle(&mut self, key: &str) {
        if let Some(r) = self
            .rows
            .iter_mut()
            .find(|r| r.is_concept() && r.key == key)
        {
            r.checked = !r.checked;
        }
    }

    /// The checked concept keys — the curated list sent on `chat.run_turn`.
    pub fn checked_concepts(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter(|r| r.is_concept() && r.checked)
            .map(|r| r.key.clone())
            .collect()
    }

    /// Estimated token cost of the currently-checked set (the budget indicator).
    pub fn estimated_tokens(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r.is_concept() && r.checked)
            .map(|r| r.est_tokens)
            .sum()
    }

    /// Whether the checked set is over the injection token budget (the menu's
    /// warning). The server's apply step will then evict the lowest-activation
    /// concept first — the warning tells the user before that happens.
    pub fn over_budget(&self) -> bool {
        self.token_budget > 0 && self.estimated_tokens() > self.token_budget
    }

    /// True when no concept can be injected at all (an all-vocab / empty menu) —
    /// the GUI then offers "Skip" only.
    pub fn has_injectable(&self) -> bool {
        self.rows.iter().any(CurateRow::is_concept)
    }
}

/// Map a routed concept's (activated, provenance) → the menu annotation.
fn annotate(c: &RoutedConceptView) -> CurateAnnotation {
    match &c.provenance {
        ProvenanceView::Inhibited { .. } => CurateAnnotation::Excluded,
        ProvenanceView::Dependency { .. } if c.activated => CurateAnnotation::Dependency,
        ProvenanceView::SeedLift { .. } if c.activated => CurateAnnotation::SeedLift,
        ProvenanceView::Positive { .. } if c.activated => CurateAnnotation::Positive,
        _ if c.activated => CurateAnnotation::Activated,
        _ => CurateAnnotation::Suppressed,
    }
}

/// A friendly label for a provenance via-node (strip the `concept:`/`vocab:`
/// tag the crate's `NodeRef::label()` adds; show `{{vocab}}` for vocab nodes).
fn via_label(node: &super::ipc::NodeRefView) -> String {
    match node {
        super::ipc::NodeRefView::Concept { id } => id.clone(),
        super::ipc::NodeRefView::Vocab { identifier } => format!("{{{{{identifier}}}}}"),
    }
}

// ── cadence (plan §4 #4) ──────────────────────────────────────────────────

/// Per-conversation curate cadence: **interactive on the first turn, auto-reuse
/// the remembered selection after, with a re-open control** (never silent — the
/// first turn always shows the menu). In-memory, session-scoped; lives on the
/// GUI panel, modelled here so the rule is tested without a window.
#[derive(Clone, Debug, Default)]
pub struct CurateCadence {
    /// conversation_id → the last confirmed curated concept ids.
    confirmed: std::collections::HashMap<String, Vec<String>>,
    /// conversation_ids the user asked to re-open (force the menu next turn even
    /// though a selection is remembered).
    reopen: std::collections::HashSet<String>,
    /// conversation_ids the user opted into auto-apply for ("⟳ auto next time")
    /// — a per-conversation override of the global `curate_before_inject`.
    auto: std::collections::HashSet<String>,
}

impl CurateCadence {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether to show the (blocking) menu for `conversation_id` this turn.
    /// `curate_before_inject` is the config flag (when `false` the user opted
    /// into auto-apply) — but the **first turn always prompts** regardless, and
    /// an explicit re-open always prompts. After the first confirmation, later
    /// turns auto-apply the remembered selection.
    pub fn should_prompt(&self, conversation_id: &str, curate_before_inject: bool) -> bool {
        if self.reopen.contains(conversation_id) {
            return true;
        }
        match self.confirmed.get(conversation_id) {
            // First turn (no confirmed selection yet) → always prompt.
            None => true,
            // Later turns → auto-apply if the user opted this conversation into
            // auto, else honour the global interactive flag.
            Some(_) => curate_before_inject && !self.auto.contains(conversation_id),
        }
    }

    /// The remembered selection to auto-apply when not prompting.
    pub fn remembered(&self, conversation_id: &str) -> Option<&Vec<String>> {
        self.confirmed.get(conversation_id)
    }

    /// Record the user's confirmed selection for a conversation (clears any
    /// pending re-open). Called when the user hits "Inject selected" / "Skip".
    pub fn confirm(&mut self, conversation_id: &str, selection: Vec<String>) {
        self.confirmed.insert(conversation_id.to_owned(), selection);
        self.reopen.remove(conversation_id);
    }

    /// Re-open the menu for a conversation that had auto-applied — the re-open
    /// control. The next [`should_prompt`](Self::should_prompt) returns `true`.
    /// Clears any auto opt-out so re-opening genuinely re-prompts.
    pub fn reopen(&mut self, conversation_id: &str) {
        self.reopen.insert(conversation_id.to_owned());
        self.auto.remove(conversation_id);
    }

    /// Opt a conversation into auto-apply ("⟳ auto next time") — later turns
    /// reuse the remembered selection without prompting. Still never silent: the
    /// first turn already prompted (this is set *from* that menu).
    pub fn set_auto(&mut self, conversation_id: &str) {
        self.auto.insert(conversation_id.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::curate_ipc::{ProvenanceView, VocabMatchView};
    use crate::routing::ipc::NodeRefView;

    fn concept(id: &str, score: f32, activated: bool, prov: ProvenanceView) -> RoutedConceptView {
        RoutedConceptView {
            id: id.into(),
            label: id.to_uppercase(),
            score,
            seed_score: score,
            provenance: prov,
            activated,
        }
    }

    fn set(concepts: Vec<RoutedConceptView>, vocab: Vec<VocabMatchView>) -> CandidateSetView {
        let activated_count = concepts.iter().filter(|c| c.activated).count();
        CandidateSetView {
            query_echo: "q".into(),
            concepts,
            vocabulary: vocab,
            abs_threshold: 0.5,
            chosen_cutoff: 0.5,
            activated_count,
            max_concepts: 3,
        }
    }

    #[test]
    fn activated_pre_checked_dependency_glyphed_exclusion_greyed() {
        let cs = set(
            vec![
                concept("auth", 0.71, true, ProvenanceView::Seed),
                concept(
                    "ddns",
                    0.32,
                    true,
                    ProvenanceView::Dependency {
                        from: NodeRefView::concept("auth"),
                        hops: 1,
                    },
                ),
                concept(
                    "wylde",
                    0.30,
                    false,
                    ProvenanceView::Inhibited {
                        by: NodeRefView::concept("auth"),
                        raw: 0.62,
                    },
                ),
            ],
            vec![],
        );
        let menu = CurateMenuModel::from_candidates(&cs, 1500);
        // Pre-checked = activated (auth + auto-pulled ddns).
        assert_eq!(
            menu.checked_concepts(),
            vec!["auth".to_owned(), "ddns".to_owned()]
        );
        // Dependency glyph + via.
        let ddns = &menu.rows[1];
        assert_eq!(ddns.annotation, CurateAnnotation::Dependency);
        assert_eq!(ddns.annotation.glyph(), "↳");
        assert_eq!(ddns.via.as_deref(), Some("auth"));
        // Exclusion greyed, unchecked, present.
        let wylde = &menu.rows[2];
        assert!(!wylde.checked && wylde.annotation.is_greyed());
        assert_eq!(wylde.annotation.glyph(), "⊘");
    }

    #[test]
    fn toggle_adds_and_removes_a_concept() {
        let cs = set(
            vec![
                concept("a", 0.7, true, ProvenanceView::Seed),
                concept("b", 0.4, false, ProvenanceView::Seed),
            ],
            vec![],
        );
        let mut menu = CurateMenuModel::from_candidates(&cs, 1500);
        assert_eq!(menu.checked_concepts(), vec!["a".to_owned()]);
        // Add the below-threshold concept.
        menu.toggle("b");
        assert_eq!(
            menu.checked_concepts(),
            vec!["a".to_owned(), "b".to_owned()]
        );
        // Remove the activated one.
        menu.toggle("a");
        assert_eq!(menu.checked_concepts(), vec!["b".to_owned()]);
    }

    #[test]
    fn vocab_rows_shown_not_injectable_zero_cost() {
        let cs = set(
            vec![concept("a", 0.7, true, ProvenanceView::Seed)],
            vec![VocabMatchView {
                identifier: "the_pipe".into(),
                score: 1.0,
            }],
        );
        let menu = CurateMenuModel::from_candidates(&cs, 1500);
        let vocab = menu.rows.iter().find(|r| !r.is_concept()).unwrap();
        assert_eq!(vocab.label, "{{the_pipe}}");
        assert_eq!(vocab.est_tokens, 0);
        // Toggling a vocab key is a no-op for the curated set.
        let mut menu = menu;
        menu.toggle("the_pipe");
        assert_eq!(menu.checked_concepts(), vec!["a".to_owned()]);
    }

    #[test]
    fn budget_indicator_warns_when_over() {
        let cs = set(
            vec![
                concept("a", 0.7, true, ProvenanceView::Seed),
                concept("b", 0.65, true, ProvenanceView::Seed),
            ],
            vec![],
        );
        // Budget fits one (230) but not two (460).
        let menu = CurateMenuModel::from_candidates(&cs, 300);
        assert!(menu.over_budget(), "two checked (460) over 300");
        let mut menu = menu;
        menu.toggle("b"); // uncheck one
        assert!(!menu.over_budget(), "one checked (230) fits 300");
    }

    // ── cadence ───────────────────────────────────────────────────────

    #[test]
    fn first_turn_always_prompts_then_auto_reuses() {
        let mut cad = CurateCadence::new();
        // First turn: prompt regardless of the curate flag.
        assert!(cad.should_prompt("conv1", false));
        assert!(cad.should_prompt("conv1", true));
        // User confirms a selection.
        cad.confirm("conv1", vec!["auth".into()]);
        // Later turn with auto (curate=false): no prompt, reuse remembered.
        assert!(!cad.should_prompt("conv1", false));
        assert_eq!(cad.remembered("conv1"), Some(&vec!["auth".to_owned()]));
        // Later turn with curate still on: prompt again (interactive every turn).
        assert!(cad.should_prompt("conv1", true));
    }

    #[test]
    fn reopen_forces_the_menu_after_auto() {
        let mut cad = CurateCadence::new();
        cad.confirm("conv1", vec!["auth".into()]);
        assert!(!cad.should_prompt("conv1", false), "auto-applies");
        // Re-open control → next turn prompts even in auto mode.
        cad.reopen("conv1");
        assert!(cad.should_prompt("conv1", false));
        // Confirming again clears the re-open.
        cad.confirm("conv1", vec![]);
        assert!(!cad.should_prompt("conv1", false));
    }

    #[test]
    fn auto_next_opts_a_conversation_out_of_prompting() {
        let mut cad = CurateCadence::new();
        cad.confirm("conv1", vec!["a".into()]);
        // Global curate stays ON, but the user chose "auto next time".
        cad.set_auto("conv1");
        assert!(
            !cad.should_prompt("conv1", true),
            "auto override beats curate=on"
        );
        // Re-open clears the auto opt-out.
        cad.reopen("conv1");
        assert!(cad.should_prompt("conv1", true));
    }

    #[test]
    fn cadence_is_per_conversation() {
        let mut cad = CurateCadence::new();
        cad.confirm("conv1", vec!["a".into()]);
        // A different conversation is still on its first turn.
        assert!(cad.should_prompt("conv2", false));
        assert_eq!(cad.remembered("conv2"), None);
    }
}

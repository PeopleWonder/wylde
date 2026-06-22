//! The curate-before-inject **menu payload** (concept-routing plan §4,
//! requirement 2; relation-model addendum §4.1) — the explainable list the user
//! curates before anything is injected.
//!
//! Pure: [`CuratedMenu::from_candidate_set`] reshapes a settled
//! [`CandidateSet`](crate::router::CandidateSet) (concepts + matched vocabulary,
//! scores, provenance) into one flat, sorted, annotated item list:
//!
//! * **pre-check** = the activated set (the router's settled-activation cutoff)
//!   — the common case is one click;
//! * **dependencies** (`Provenance::Dependency` / `SeedLift`) are shown
//!   auto-pulled (`↳`) and default-checked, so a flat-cosine dependency the seed
//!   alone would miss rides in;
//! * **exclusions** (`Provenance::Inhibited`) are shown greyed (`⊘`) and
//!   unchecked — *not hidden*, because inhibition is soft (the user can still
//!   re-add an overwhelming one);
//! * everything else is shown unchecked-but-addable, sorted by settled score.
//!
//! Each item carries an estimated token cost so the menu can show a budget
//! indicator and warn when the checked set exceeds [`RoutingConfig::inject_token_budget`](crate::config::RoutingConfig::inject_token_budget);
//! the actual eviction is [`apply`](super::apply).

use serde::{Deserialize, Serialize};

use crate::router::spread::Provenance;
use crate::router::CandidateSet;

/// What a menu row represents — a routed concept or a matched vocabulary term.
/// Both appear in one concatenated list (plan §4 "concepts + matched
/// dictionary/vocabulary words").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MenuItemKind {
    Concept,
    Vocab,
}

/// How a concept row should read in the menu, derived from its provenance — the
/// explainable annotation (`★` activated / `↳` pulled-in dependency / `⊘`
/// excluded / `+` co-activated / `↑` lifted by a vocab term / `·` suppressed).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MenuAnnotation {
    /// Cleared the cutoff on its own / via the graph — checked by default.
    Activated,
    /// Pulled in via a dependency edge (`↳`) — auto-pulled, checked by default.
    Dependency,
    /// Lifted by a matched vocab term (`↑`).
    SeedLift,
    /// Co-activated by a positive edge (`+`).
    Positive,
    /// Suppressed by an exclusion edge (`⊘`) — greyed, unchecked, re-addable.
    Excluded,
    /// Below the cutoff, no relation reshaping (`·`) — unchecked, addable.
    Suppressed,
}

impl MenuAnnotation {
    /// The single-glyph badge for the row (matches the calibration-log glyphs).
    pub fn glyph(self) -> &'static str {
        match self {
            MenuAnnotation::Activated => "★",
            MenuAnnotation::Dependency => "↳",
            MenuAnnotation::SeedLift => "↑",
            MenuAnnotation::Positive => "+",
            MenuAnnotation::Excluded => "⊘",
            MenuAnnotation::Suppressed => "·",
        }
    }
    /// Whether this row reads as "greyed / suppressed" in the menu (exclusions).
    pub fn is_greyed(self) -> bool {
        matches!(self, MenuAnnotation::Excluded)
    }
}

/// One row in the curate menu.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MenuItem {
    pub kind: MenuItemKind,
    /// The concept id (or vocab identifier) — the key carried in the curated
    /// list back to `chat.run_turn`.
    pub key: String,
    /// Display label (concept label, or the bare vocab identifier).
    pub label: String,
    /// Settled activation (concepts) or match strength (vocab).
    pub score: f32,
    /// The raw seed cosine before relation reshaping (concepts only; `0.0` for
    /// vocab) — lets the menu show before→after if it wants.
    pub seed_score: f32,
    /// The explainable annotation driving the glyph + greying.
    pub annotation: MenuAnnotation,
    /// If this row was pulled in / suppressed by a relation, the other node's
    /// label (for "↳ depends on X" / "⊘ excluded by Y").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// Pre-checked when the menu opens (the activated set + auto-pulled
    /// dependencies). The user can still toggle it.
    pub default_checked: bool,
    /// Estimated token cost of injecting this concept's context (blurb +
    /// member-snippet share) — feeds the budget indicator. `0` for vocab (vocab
    /// rides the existing `### Vocabulary` slot, not the concept budget).
    pub est_tokens: usize,
}

impl MenuItem {
    /// Whether this row drives injection (only concepts do in Augment mode;
    /// vocab is shown for transparency but injected via the anchors slot).
    pub fn is_injectable(&self) -> bool {
        matches!(self.kind, MenuItemKind::Concept)
    }
}

/// The whole menu — the concatenated, annotated, sorted item list plus the
/// budget the checked set is measured against.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CuratedMenu {
    /// The (conversation-composed) query that was routed — echoed for context.
    pub query_echo: String,
    /// Concepts first (by settled score desc), then vocabulary (by match order).
    pub items: Vec<MenuItem>,
    /// The token cap the checked set is measured against (config
    /// `inject_token_budget`).
    pub token_budget: usize,
}

impl CuratedMenu {
    /// Build the menu from a settled [`CandidateSet`]. `inject_token_budget` is
    /// the config cap shown by the budget indicator. A per-concept token
    /// estimate is split as a flat blurb cost plus an even share of the snippet
    /// budget across the *activated* set (the set that would actually inject
    /// snippets); it's an indicator, not the authoritative count (that's the
    /// server's render).
    pub fn from_candidate_set(set: &CandidateSet, inject_token_budget: usize) -> Self {
        let mut items: Vec<MenuItem> = set
            .concepts
            .iter()
            .map(|c| {
                let annotation = annotate(c.activated, &c.provenance);
                MenuItem {
                    kind: MenuItemKind::Concept,
                    key: c.id.clone(),
                    label: c.label.clone(),
                    score: c.score,
                    seed_score: c.seed_score,
                    annotation,
                    via: provenance_via(&c.provenance),
                    // Pre-check the activated set (which already folds in the
                    // dependency/seed-lift that cleared the cutoff). Exclusions
                    // and below-cutoff seeds start unchecked but addable.
                    default_checked: c.activated,
                    est_tokens: PER_CONCEPT_INJECT_TOKENS,
                }
            })
            .collect();

        // Vocabulary rows: shown for transparency (the matched dictionary words),
        // never drive injection — no token cost against the concept budget.
        items.extend(set.vocabulary.iter().map(|v| MenuItem {
            kind: MenuItemKind::Vocab,
            key: v.identifier.clone(),
            label: format!("{{{{{}}}}}", v.identifier),
            score: v.score,
            seed_score: 0.0,
            annotation: MenuAnnotation::Activated,
            via: None,
            default_checked: false,
            est_tokens: 0,
        }));

        CuratedMenu {
            query_echo: set.query_echo.clone(),
            items,
            token_budget: inject_token_budget,
        }
    }

    /// The keys pre-checked when the menu opens (the default curated set).
    pub fn default_checked_concepts(&self) -> Vec<String> {
        self.items
            .iter()
            .filter(|i| i.is_injectable() && i.default_checked)
            .map(|i| i.key.clone())
            .collect()
    }

    /// Estimated token cost of a given checked concept key set — the budget
    /// indicator. Sums the per-item estimate of the matching concept rows.
    pub fn estimated_tokens(&self, checked: &[String]) -> usize {
        self.items
            .iter()
            .filter(|i| i.is_injectable() && checked.iter().any(|k| k == &i.key))
            .map(|i| i.est_tokens)
            .sum()
    }

    /// Whether a given checked set is over the budget (the menu's warning).
    pub fn over_budget(&self, checked: &[String]) -> bool {
        self.estimated_tokens(checked) > self.token_budget
    }
}

/// Estimated tokens one injected concept costs: its boundary blurb line
/// (~30 tokens) plus its share of the member-snippet fill (~200 tokens). A flat
/// per-concept estimate keeps the menu indicator and the server-side eviction
/// ([`super::apply`]) agreeing without re-running retrieval — it's an indicator,
/// not the authoritative render count.
pub(crate) const PER_CONCEPT_INJECT_TOKENS: usize = 230;

/// Map (activated, provenance) → the menu annotation.
fn annotate(activated: bool, prov: &Provenance) -> MenuAnnotation {
    match prov {
        Provenance::Inhibited { .. } => MenuAnnotation::Excluded,
        Provenance::Dependency { .. } if activated => MenuAnnotation::Dependency,
        Provenance::SeedLift { .. } if activated => MenuAnnotation::SeedLift,
        Provenance::Positive { .. } if activated => MenuAnnotation::Positive,
        _ if activated => MenuAnnotation::Activated,
        _ => MenuAnnotation::Suppressed,
    }
}

/// The "via" label for a relation-driven row (the other endpoint).
fn provenance_via(prov: &Provenance) -> Option<String> {
    match prov {
        Provenance::Seed => None,
        Provenance::SeedLift { from } => Some(from.label()),
        Provenance::Dependency { from, .. } => Some(from.label()),
        Provenance::Positive { from } => Some(from.label()),
        Provenance::Inhibited { by, .. } => Some(by.label()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::{RoutedConcept, VocabMatch};
    use crate::NodeRef;

    fn concept(id: &str, score: f32, activated: bool, prov: Provenance) -> RoutedConcept {
        RoutedConcept {
            id: id.into(),
            label: id.to_uppercase(),
            score,
            seed_score: score,
            provenance: prov,
            activated,
        }
    }

    fn set(concepts: Vec<RoutedConcept>, vocab: Vec<VocabMatch>) -> CandidateSet {
        let activated_count = concepts.iter().filter(|c| c.activated).count();
        CandidateSet {
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
    fn activated_concepts_are_pre_checked() {
        let cs = set(
            vec![
                concept("a", 0.7, true, Provenance::Seed),
                concept("b", 0.3, false, Provenance::Seed),
            ],
            vec![],
        );
        let menu = CuratedMenu::from_candidate_set(&cs, 1500);
        assert_eq!(menu.default_checked_concepts(), vec!["a".to_owned()]);
        let a = &menu.items[0];
        assert!(a.default_checked && a.annotation == MenuAnnotation::Activated);
        let b = &menu.items[1];
        assert!(!b.default_checked && b.annotation == MenuAnnotation::Suppressed);
    }

    #[test]
    fn dependency_pulled_in_is_auto_checked_and_glyphed() {
        let cs = set(
            vec![concept(
                "ddns",
                0.32,
                true,
                Provenance::Dependency {
                    from: NodeRef::concept("nextcloud"),
                    hops: 1,
                },
            )],
            vec![],
        );
        let menu = CuratedMenu::from_candidate_set(&cs, 1500);
        let item = &menu.items[0];
        assert!(
            item.default_checked,
            "auto-pulled dependency is pre-checked"
        );
        assert_eq!(item.annotation, MenuAnnotation::Dependency);
        assert_eq!(item.annotation.glyph(), "↳");
        assert_eq!(item.via.as_deref(), Some("concept:nextcloud"));
    }

    #[test]
    fn exclusion_is_greyed_unchecked_but_present() {
        let cs = set(
            vec![concept(
                "wylde",
                0.30,
                false,
                Provenance::Inhibited {
                    by: NodeRef::concept("nextcloud"),
                    raw: 0.62,
                },
            )],
            vec![],
        );
        let menu = CuratedMenu::from_candidate_set(&cs, 1500);
        let item = &menu.items[0];
        assert!(!item.default_checked, "exclusions start unchecked");
        assert!(item.annotation.is_greyed(), "but shown greyed, not hidden");
        assert_eq!(item.annotation.glyph(), "⊘");
        assert_eq!(item.via.as_deref(), Some("concept:nextcloud"));
    }

    #[test]
    fn vocab_rows_are_shown_but_not_injectable() {
        let cs = set(
            vec![concept("a", 0.7, true, Provenance::Seed)],
            vec![VocabMatch {
                identifier: "the_pipe".into(),
                score: 1.0,
            }],
        );
        let menu = CuratedMenu::from_candidate_set(&cs, 1500);
        let vocab = menu
            .items
            .iter()
            .find(|i| i.kind == MenuItemKind::Vocab)
            .unwrap();
        assert_eq!(vocab.label, "{{the_pipe}}");
        assert!(!vocab.is_injectable());
        assert_eq!(vocab.est_tokens, 0, "vocab doesn't cost the concept budget");
        // The default curated set is concepts only.
        assert_eq!(menu.default_checked_concepts(), vec!["a".to_owned()]);
    }

    #[test]
    fn budget_indicator_flags_when_over() {
        let cs = set(
            vec![
                concept("a", 0.7, true, Provenance::Seed),
                concept("b", 0.65, true, Provenance::Seed),
            ],
            vec![],
        );
        // A tiny budget: even one concept is over.
        let menu = CuratedMenu::from_candidate_set(&cs, 10);
        assert!(menu.over_budget(&["a".into()]));
        // A generous budget: both fit.
        let menu = CuratedMenu::from_candidate_set(&cs, 5000);
        assert!(!menu.over_budget(&["a".into(), "b".into()]));
        assert!(menu.estimated_tokens(&["a".into()]) > 0);
    }
}

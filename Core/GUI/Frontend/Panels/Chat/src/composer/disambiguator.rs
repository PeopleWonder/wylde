//! Disambiguation (Slice F, Build Order §5): a `?N` word's dropdown —
//! candidate rows the user picks from, plus the "Anchor this?" affordance
//! hook (the *flow* behind it is Slice N's; the hook is shaped here so N
//! only wires a handler).
//!
//! Pure model — the panel renders [`DisambiguationView`] rows and routes a
//! click to `ComposerState::resolve`.

use super::{SymbolCandidate, WordRecognition};

/// One dropdown row.
#[derive(Clone, Debug, PartialEq)]
pub struct CandidateRow {
    pub id: String,
    /// Primary line: the symbol name + kind (`set_active · Function`).
    pub title: String,
    /// Secondary line: where it lives (`wylde-workspaces · src/registry.rs:42`).
    pub detail: String,
}

/// The dropdown model for one ambiguous word.
#[derive(Clone, Debug, PartialEq)]
pub struct DisambiguationView {
    pub word: String,
    pub rows: Vec<CandidateRow>,
    /// Slice N affordance: offer "Anchor this?" after a pick when the word
    /// has no anchors yet (creating one would make the choice durable).
    pub offer_anchor: bool,
}

/// Build the dropdown for `word`, best candidate first. `None` when the
/// word isn't actually ambiguous (nothing to disambiguate).
pub fn view_for(word: &WordRecognition) -> Option<DisambiguationView> {
    if !word.is_ambiguous() {
        return None;
    }
    let mut rows: Vec<CandidateRow> = word.candidates.iter().map(row).collect();
    // Best-first by score (the service already ranks; pin it locally so a
    // re-serialized set can't quietly reorder).
    rows.sort_by(|a, b| {
        let score = |id: &str| {
            word.candidates
                .iter()
                .find(|c| c.id == id)
                .map(|c| c.score)
                .unwrap_or(0.0)
        };
        score(&b.id)
            .partial_cmp(&score(&a.id))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    Some(DisambiguationView {
        word: word.token.text.clone(),
        rows,
        offer_anchor: word.anchor_count == 0,
    })
}

fn row(c: &SymbolCandidate) -> CandidateRow {
    let title = format!("{} · {}", c.name, c.kind);
    let place = if c.module_path.is_empty() {
        format!("{}:{}", c.file, c.line)
    } else {
        format!("{} · {}:{}", c.module_path, c.file, c.line)
    };
    CandidateRow {
        id: c.id.clone(),
        title,
        detail: place,
    }
}

#[cfg(test)]
#[path = "tests/disambiguator_tests.rs"]
mod disambiguator_tests;

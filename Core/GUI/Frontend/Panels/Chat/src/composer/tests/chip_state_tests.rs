//! Context-chip state machine suite (Build Order §5 file tree).

use super::ChipState;
use crate::composer::{SymbolCandidate, TokenKind, TokenSpan, WordRecognition};

fn word(text: &str, candidates: usize, anchors: usize) -> WordRecognition {
    let mut w = WordRecognition::new(TokenSpan {
        text: text.to_owned(),
        start: 0,
        end: text.len(),
        kind: TokenKind::Identifier,
    });
    w.candidates = (0..candidates)
        .map(|i| SymbolCandidate {
            id: format!("{text}-{i}"),
            name: text.to_owned(),
            kind: "Function".to_owned(),
            file: format!("src/{text}.rs"),
            line: 1,
            module_path: String::new(),
            score: 1.0,
        })
        .collect();
    w.anchor_count = anchors;
    w
}

#[test]
fn empty_words_hide_the_chip() {
    assert_eq!(ChipState::derive(&[]), ChipState::Hidden);
    assert_eq!(ChipState::Hidden.label(), None);
}

#[test]
fn any_ambiguity_wins_the_chip() {
    let words = vec![word("clear", 1, 0), word("ambig", 3, 0), word("also", 2, 1)];
    let chip = ChipState::derive(&words);
    assert_eq!(chip, ChipState::Unresolved { ambiguous: 2 });
    assert_eq!(chip.label().as_deref(), Some("?2"));
    assert!(chip.is_ambiguous());
}

#[test]
fn all_resolved_counts_ready_items() {
    let mut ambig = word("picked", 3, 0);
    ambig.resolved = Some("picked-1".to_owned());
    let words = vec![word("one", 1, 0), word("anchored", 0, 2), ambig];
    let chip = ChipState::derive(&words);
    assert_eq!(chip, ChipState::Ready { items: 3 });
    assert_eq!(chip.label().as_deref(), Some("3 ▸"));
    assert!(!chip.is_ambiguous());
}

#[test]
fn excluded_words_resolve_but_do_not_count() {
    let mut excluded = word("skipme", 1, 0);
    excluded.excluded = true;
    let words = vec![word("keep", 1, 0), excluded];
    assert_eq!(ChipState::derive(&words), ChipState::Ready { items: 1 });
}

#[test]
fn all_unrecognized_or_excluded_hides_the_chip() {
    // Unrecognized words only → nothing to show.
    assert_eq!(ChipState::derive(&[word("prose", 0, 0)]), ChipState::Hidden);
    // Everything excluded → nothing rides along → hidden.
    let mut w = word("gone", 1, 0);
    w.excluded = true;
    assert_eq!(ChipState::derive(&[w]), ChipState::Hidden);
}

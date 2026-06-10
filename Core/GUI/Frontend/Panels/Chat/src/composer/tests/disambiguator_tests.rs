//! Disambiguator suite (Build Order §5 file tree).

use super::*;
use crate::composer::{SymbolCandidate, TokenKind, TokenSpan, WordRecognition};

fn candidate(id: &str, score: f32, module: &str) -> SymbolCandidate {
    SymbolCandidate {
        id: id.to_owned(),
        name: id.split('-').next().unwrap_or(id).to_owned(),
        kind: "Function".to_owned(),
        file: format!("src/{id}.rs"),
        line: 7,
        module_path: module.to_owned(),
        score,
    }
}

fn ambiguous_word(anchors: usize) -> WordRecognition {
    let mut w = WordRecognition::new(TokenSpan {
        text: "set_active".to_owned(),
        start: 0,
        end: 10,
        kind: TokenKind::Identifier,
    });
    w.candidates = vec![
        candidate("set_active-low", 0.4, ""),
        candidate("set_active-top", 1.0, "wylde-workspaces::registry"),
    ];
    w.anchor_count = anchors;
    w
}

#[test]
fn unambiguous_words_have_no_dropdown() {
    let mut w = ambiguous_word(0);
    w.candidates.truncate(1);
    assert!(view_for(&w).is_none());
    let mut resolved = ambiguous_word(0);
    resolved.resolved = Some("set_active-top".to_owned());
    assert!(view_for(&resolved).is_none());
}

#[test]
fn rows_are_best_first_with_place_details() {
    let view = view_for(&ambiguous_word(0)).expect("ambiguous → dropdown");
    assert_eq!(view.word, "set_active");
    assert_eq!(view.rows.len(), 2);
    assert_eq!(view.rows[0].id, "set_active-top", "score 1.0 first");
    assert!(view.rows[0].title.contains("Function"));
    assert!(view.rows[0].detail.contains("wylde-workspaces::registry"));
    assert!(view.rows[0].detail.contains(":7"));
    // No module path → file:line only.
    assert!(!view.rows[1].detail.contains(" · src/"));
}

#[test]
fn anchor_offer_only_when_word_has_no_anchors() {
    assert!(view_for(&ambiguous_word(0)).unwrap().offer_anchor);
    assert!(!view_for(&ambiguous_word(2)).unwrap().offer_anchor);
}

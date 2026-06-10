//! Curate-before-send (Slice F, Build Order §5).
//!
//! The `N ▸` context chip opens this list: every context item riding along
//! with the upcoming send, each toggleable. Exclusions are **per-message**
//! (Plan §5.8: exclude ≠ ignore — the durable ignore tiers are Slice M).
//!
//! **Re-scope note (documented for the slice report):** the Build Order's
//! one-liner has curation "open the graph panel in focused mode" and receive
//! a curated subgraph back. There is no cross-panel routing surface in the
//! Shell yet (panels are independent gpui entities), so v1 curates **in
//! place** — an inline popover over the composer with the same
//! include/exclude semantics. The graph-panel handoff slots in behind
//! [`CurationView`] unchanged once the Shell grows panel-to-panel focus
//! routing; the curated-state model is identical either way.

use super::WordRecognition;

/// One curate-list row.
#[derive(Clone, Debug, PartialEq)]
pub struct CurationItem {
    /// Index into the composer's word set (the toggle target).
    pub word_idx: usize,
    /// The recognized word.
    pub word: String,
    /// What rides along: `2 anchors`, `symbol set_active`, `2 anchors + symbol`.
    pub summary: String,
    pub included: bool,
}

/// The curate-before-send popover model.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CurationView {
    pub items: Vec<CurationItem>,
}

impl CurationView {
    /// How many items remain included.
    pub fn included_count(&self) -> usize {
        self.items.iter().filter(|i| i.included).count()
    }
}

/// Build the curation list from the current recognition set: every
/// *recognized* word (ambiguous ones too — the user may exclude instead of
/// disambiguating).
pub fn view_for(words: &[WordRecognition]) -> CurationView {
    let items = words
        .iter()
        .enumerate()
        .filter(|(_, w)| w.is_recognized())
        .map(|(i, w)| CurationItem {
            word_idx: i,
            word: w.token.text.clone(),
            summary: summary(w),
            included: w.is_included(),
        })
        .collect();
    CurationView { items }
}

fn summary(w: &WordRecognition) -> String {
    let mut parts: Vec<String> = Vec::new();
    match w.anchor_count {
        0 => {}
        1 => parts.push("1 anchor".to_owned()),
        n => parts.push(format!("{n} anchors")),
    }
    if let Some(sym) = w.effective_symbol() {
        parts.push(format!("symbol {}", sym.name));
    } else if w.is_ambiguous() {
        parts.push(format!("?{} symbols", w.candidates.len()));
    }
    let mut s = if parts.is_empty() {
        "no context".to_owned()
    } else {
        parts.join(" + ")
    };
    // Surface the durable ignore so the list explains the unchecked default.
    if let Some(tier) = w.ignored_tiers.first() {
        s.push_str(&format!(" · ignored ({})", tier.label()));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composer::{SymbolCandidate, TokenKind, TokenSpan};

    fn word(text: &str, candidates: usize, anchors: usize, excluded: bool) -> WordRecognition {
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
        w.excluded = excluded;
        w
    }

    #[test]
    fn list_covers_recognized_words_only() {
        let words = vec![
            word("plain", 0, 0, false), // unrecognized → not listed
            word("sym", 1, 0, false),
            word("anchored", 0, 2, true), // excluded → listed, unchecked
        ];
        let view = view_for(&words);
        assert_eq!(view.items.len(), 2);
        assert_eq!(view.items[0].word, "sym");
        assert_eq!(view.items[0].word_idx, 1, "indices point into the word set");
        assert!(view.items[0].included);
        assert!(!view.items[1].included);
        assert_eq!(view.included_count(), 1);
    }

    #[test]
    fn summaries_read_naturally() {
        assert_eq!(
            view_for(&[word("a", 1, 0, false)]).items[0].summary,
            "symbol a"
        );
        assert_eq!(
            view_for(&[word("b", 0, 1, false)]).items[0].summary,
            "1 anchor"
        );
        assert_eq!(
            view_for(&[word("c", 1, 2, false)]).items[0].summary,
            "2 anchors + symbol c"
        );
        assert_eq!(
            view_for(&[word("d", 3, 0, false)]).items[0].summary,
            "?3 symbols"
        );
    }
}

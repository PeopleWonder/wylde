//! The message-level context chip (Slice F, Build Order §5): **one chip, two
//! states** — `?N` while N recognized words are still ambiguous, `N ▸` once
//! everything is resolved (N context items; the `▸` opens curate-before-send).
//!
//! Pure state machine over the per-word recognition results; the panel
//! renders it and routes clicks (ambiguous → first unresolved word's
//! disambiguation, ready → curation popover).

use super::WordRecognition;

/// The chip's two states (plus hidden when there's nothing to show).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChipState {
    /// No recognized words — render no chip.
    Hidden,
    /// `?N`: N words have ambiguous matches awaiting disambiguation.
    Unresolved { ambiguous: usize },
    /// `N ▸`: all words resolved; N context items will accompany the send.
    Ready { items: usize },
}

impl ChipState {
    /// Derive the chip from the current recognition set. Excluded items
    /// still count as "resolved" words (they're curated, not ambiguous) but
    /// don't count toward the ready-item total.
    pub fn derive(words: &[WordRecognition]) -> ChipState {
        if words.is_empty() {
            return ChipState::Hidden;
        }
        let ambiguous = words.iter().filter(|w| w.is_ambiguous()).count();
        if ambiguous > 0 {
            return ChipState::Unresolved { ambiguous };
        }
        let items = words
            .iter()
            .filter(|w| w.is_recognized() && !w.excluded)
            .count();
        if items == 0 {
            return ChipState::Hidden;
        }
        ChipState::Ready { items }
    }

    /// The chip label (`?2` / `3 ▸`), `None` when hidden.
    pub fn label(&self) -> Option<String> {
        match self {
            ChipState::Hidden => None,
            ChipState::Unresolved { ambiguous } => Some(format!("?{ambiguous}")),
            ChipState::Ready { items } => Some(format!("{items} ▸")),
        }
    }

    pub fn is_ambiguous(&self) -> bool {
        matches!(self, ChipState::Unresolved { .. })
    }
}

#[cfg(test)]
#[path = "tests/chip_state_tests.rs"]
mod chip_state_tests;

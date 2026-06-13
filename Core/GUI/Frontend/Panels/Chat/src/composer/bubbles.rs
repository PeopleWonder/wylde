//! The floating Thought-Bubble layer — pure core (Plan §5.2–5.5, OI-17).
//!
//! State machine + layout math + card-data parsing for the bubbles that
//! materialise above the composer when a highlighted word is clicked. The
//! rendering (composer_ui) and IPC (chat_panel) consume this; everything
//! here is testable without gpui.
//!
//! Spec anchors:
//!   * §5.2 — ONE word's bubble set open at a time (OI-17): clicking a
//!     second word's chip swaps sets; re-clicking the open word collapses.
//!   * §5.4 — one bubble expanded at a time; ✕/↺ rides the word's exclude
//!     state (the same Plan §5.8 semantics the chip strip uses); 📌 pins
//!     persist across messages within the conversation.
//!   * §5.3 — sizes/colours come from the locked `chat_composer` theme at
//!     render time, never from here.

use std::collections::BTreeSet;

use serde_json::Value;

use super::WordRecognition;

/// What one bubble represents.
#[derive(Clone, Debug, PartialEq)]
pub enum BubbleKind {
    /// An anchor matching the word (`{{identifier}}`).
    Anchor { description: String },
    /// The word's effective code symbol (file:line) — present so a
    /// recognized-but-unanchored word still drills into a card.
    Symbol { id: String, file: String, line: u32 },
}

/// One bubble in the open set.
#[derive(Clone, Debug, PartialEq)]
pub struct Bubble {
    /// Display label (the anchor identifier / symbol name).
    pub label: String,
    pub kind: BubbleKind,
}

/// The expanded card's drill-in data, parsed from a
/// `workspaces.symbol_context` reply (callers/callees/types + body preview
/// + the Slice L blame line).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CardContext {
    pub callers: Vec<String>,
    pub callees: Vec<String>,
    pub types_used: Vec<String>,
    /// First line of the symbol body, as a peek.
    pub body_preview: Option<String>,
    /// "edited by <author> — <summary>" from the newest blame entry.
    pub blame_line: Option<String>,
}

impl CardContext {
    /// Parse the `symbol_context` reply payload. Lenient — missing pieces
    /// just stay empty (the card renders what it has).
    pub fn from_reply(v: &Value) -> Self {
        fn names(v: &Value, key: &str) -> Vec<String> {
            v.get(key)
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|r| r.get("name").and_then(Value::as_str))
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        }
        let symbol = v.get("symbol").cloned().unwrap_or(Value::Null);
        let body_preview = symbol
            .get("body")
            .and_then(Value::as_str)
            .and_then(|b| b.lines().next())
            .map(str::to_owned);
        let blame_line = symbol
            .get("blame")
            .and_then(Value::as_array)
            .and_then(|entries| entries.first())
            .map(|e| {
                let author = e.get("author").and_then(Value::as_str).unwrap_or("?");
                let summary = e.get("summary").and_then(Value::as_str).unwrap_or("");
                format!("edited by {author} — {summary}")
            });
        CardContext {
            callers: names(v, "callers"),
            callees: names(v, "callees"),
            types_used: names(v, "types_used"),
            body_preview,
            blame_line,
        }
    }
}

/// The layer's interaction state (panel-owned; one per conversation).
#[derive(Debug, Default)]
pub struct BubbleLayer {
    /// Which word's set is open (`composer.words` index). `None` = compact.
    pub word_idx: Option<usize>,
    /// The fetched bubbles for the open set.
    pub bubbles: Vec<Bubble>,
    /// Expanded bubble (index into `bubbles`) — one at a time (§5.4).
    pub expanded: Option<usize>,
    /// Drill-in context for the expanded bubble, when fetched.
    pub context: Option<CardContext>,
    /// Set when the `symbol_context` fetch failed, so the card can show
    /// "context unavailable" instead of an eternal "loading context…".
    pub context_failed: bool,
    /// Right-click menu open on a bubble (index into `bubbles`).
    pub menu: Option<usize>,
    /// 📌 pinned bubble labels — persist across messages within this
    /// conversation (in-memory v1; conversation switches clear it).
    pub pinned: BTreeSet<String>,
}

impl BubbleLayer {
    /// Click a word's chip/word (§5.2): open its set, swap from another
    /// word's set, or collapse when it's already open. Returns `true` when
    /// the set is now open (the caller fetches bubbles).
    pub fn open(&mut self, word_idx: usize) -> bool {
        if self.word_idx == Some(word_idx) {
            self.collapse();
            return false;
        }
        self.word_idx = Some(word_idx);
        self.bubbles.clear();
        self.expanded = None;
        self.context = None;
        self.context_failed = false;
        self.menu = None;
        true
    }

    /// Collapse everything back to compact (§5.4 double-click-outside /
    /// Esc). Pins survive — they're per-conversation, not per-set.
    pub fn collapse(&mut self) {
        self.word_idx = None;
        self.bubbles.clear();
        self.expanded = None;
        self.context = None;
        self.context_failed = false;
        self.menu = None;
    }

    pub fn is_open(&self) -> bool {
        self.word_idx.is_some()
    }

    /// Expand one bubble's card (one at a time); re-click collapses it.
    /// Returns `true` when newly expanded (the caller fetches context).
    pub fn toggle_expanded(&mut self, ix: usize) -> bool {
        self.menu = None;
        if self.expanded == Some(ix) {
            self.expanded = None;
            self.context = None;
            self.context_failed = false;
            return false;
        }
        self.expanded = Some(ix);
        self.context = None;
        self.context_failed = false;
        ix < self.bubbles.len()
    }

    /// 📌 toggle. Returns the new pinned state.
    pub fn toggle_pin(&mut self, label: &str) -> bool {
        if self.pinned.remove(label) {
            false
        } else {
            self.pinned.insert(label.to_owned());
            true
        }
    }

    /// The label of the bubble the right-click menu is open on.
    pub fn menu_target_label(&self) -> Option<String> {
        self.menu
            .and_then(|i| self.bubbles.get(i))
            .map(|b| b.label.clone())
    }

    /// A new scan replaced `composer.words` — drop the open set if its
    /// word index no longer exists (stale indices must never dangle).
    pub fn on_words_changed(&mut self, new_len: usize) {
        if self.word_idx.is_some_and(|i| i >= new_len) {
            self.collapse();
        }
    }

    /// A conversation switch: everything resets, pins included.
    pub fn on_conversation_changed(&mut self) {
        self.collapse();
        self.pinned.clear();
    }
}

/// One §5.9-undoable bubble operation, inverse baked in. Toggles are
/// self-inverse; the set-open transition carries both endpoints so undo
/// replays `to → from` and redo `from → to`.
#[derive(Clone, Debug, PartialEq)]
pub enum BubbleOp {
    /// The word's ✕/↺ flip (Plan §5.8 exclude/restore).
    ToggleExclude { word_idx: usize },
    /// 📌 flip.
    TogglePin { label: String },
    /// Spawn / swap / collapse of the open bubble set (§5.2).
    SetOpenWord {
        from: Option<usize>,
        to: Option<usize>,
    },
}

/// Which sibling stack owns the next unified undo/redo step (§5.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UndoSide {
    Text,
    Bubble,
    Neither,
}

/// The merge rule: compare timeline stamps, newest wins. One shared clock
/// means stamps never tie across stacks; `>=` keeps the text side stable
/// if a zero-stamped legacy entry ever appears.
pub fn newer_side(text_top: Option<u64>, bubble_top: Option<u64>) -> UndoSide {
    match (text_top, bubble_top) {
        (None, None) => UndoSide::Neither,
        (Some(_), None) => UndoSide::Text,
        (None, Some(_)) => UndoSide::Bubble,
        (Some(t), Some(b)) => {
            if t >= b {
                UndoSide::Text
            } else {
                UndoSide::Bubble
            }
        }
    }
}

/// Build the bubble list for a word: one bubble per matching anchor, plus
/// the effective symbol's bubble when the word resolves to code. Pure —
/// `anchors` comes from the panel's fetch.
pub fn bubbles_for(word: &WordRecognition, anchors: &[(String, String)]) -> Vec<Bubble> {
    let mut out: Vec<Bubble> = anchors
        .iter()
        .map(|(identifier, description)| Bubble {
            label: identifier.clone(),
            kind: BubbleKind::Anchor {
                description: description.clone(),
            },
        })
        .collect();
    if let Some(sym) = word.effective_symbol() {
        out.push(Bubble {
            label: sym.name.clone(),
            kind: BubbleKind::Symbol {
                id: sym.id.clone(),
                file: sym.file.clone(),
                line: sym.line,
            },
        });
    }
    out
}

/// Horizontal slot positions for `n` bubbles clustered around the word's
/// x (strip-relative), spread by `gap`, clamped into `[0, width - gap]`.
/// Pure layout math for the absolutely-positioned bubble divs + tether
/// endpoints (both sides use the same slots, so tethers always meet their
/// bubbles).
pub fn slot_xs(n: usize, word_x: f32, gap: f32, width: f32) -> Vec<f32> {
    if n == 0 {
        return Vec::new();
    }
    let total = gap * (n as f32 - 1.0);
    let start = (word_x - total / 2.0)
        .max(0.0)
        .min((width - gap).max(0.0) - total.min(width));
    (0..n).map(|i| (start + gap * i as f32).max(0.0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composer::{SymbolCandidate, TokenKind, TokenSpan};
    use serde_json::json;

    fn word_with_symbol() -> WordRecognition {
        let mut w = WordRecognition::new(TokenSpan {
            text: "set_active".to_owned(),
            start: 0,
            end: 10,
            kind: TokenKind::Identifier,
        });
        w.candidates = vec![SymbolCandidate {
            id: "set_active".to_owned(),
            name: "set_active".to_owned(),
            kind: "Function".to_owned(),
            file: "src/registry.rs".to_owned(),
            line: 42,
            module_path: String::new(),
            score: 1.0,
        }];
        w
    }

    #[test]
    fn one_set_at_a_time_swap_and_collapse() {
        let mut l = BubbleLayer::default();
        assert!(l.open(0), "first open");
        l.bubbles = vec![Bubble {
            label: "a".into(),
            kind: BubbleKind::Anchor {
                description: String::new(),
            },
        }];
        l.expanded = Some(0);
        // Another word → swap: set cleared, expansion dropped (OI-17).
        assert!(l.open(2), "swap opens the new set");
        assert_eq!(l.word_idx, Some(2));
        assert!(l.bubbles.is_empty() && l.expanded.is_none());
        // Same word again → collapse.
        assert!(!l.open(2));
        assert!(!l.is_open());
    }

    #[test]
    fn pins_survive_collapse_but_not_conversation_switch() {
        let mut l = BubbleLayer::default();
        l.open(0);
        assert!(l.toggle_pin("the_pipe"), "pinned");
        l.collapse();
        assert!(l.pinned.contains("the_pipe"), "pin outlives the set");
        assert!(!l.toggle_pin("the_pipe"), "unpinned on re-toggle");
        l.toggle_pin("x");
        l.on_conversation_changed();
        assert!(l.pinned.is_empty(), "conversation switch clears pins");
    }

    #[test]
    fn stale_word_indices_collapse_on_rescan() {
        let mut l = BubbleLayer::default();
        l.open(3);
        l.on_words_changed(2);
        assert!(!l.is_open());
        l.open(1);
        l.on_words_changed(2);
        assert!(l.is_open(), "still-valid index survives");
    }

    #[test]
    fn bubbles_for_lists_anchors_then_the_symbol() {
        let w = word_with_symbol();
        let anchors = vec![("the_registry".to_owned(), "MRU registry".to_owned())];
        let bs = bubbles_for(&w, &anchors);
        assert_eq!(bs.len(), 2);
        assert_eq!(bs[0].label, "the_registry");
        assert!(matches!(bs[0].kind, BubbleKind::Anchor { .. }));
        assert_eq!(bs[1].label, "set_active");
        assert!(matches!(bs[1].kind, BubbleKind::Symbol { line: 42, .. }));
        // No anchors + no resolution → no bubbles.
        let bare = WordRecognition::new(TokenSpan {
            text: "x".into(),
            start: 0,
            end: 1,
            kind: TokenKind::Identifier,
        });
        assert!(bubbles_for(&bare, &[]).is_empty());
    }

    #[test]
    fn card_context_parses_symbol_context_reply() {
        let v = json!({
            "symbol": {
                "name": "set_active",
                "body": "fn set_active() {\n    …\n}",
                "blame": [
                    {"author": "Aaron", "summary": "fix MRU ordering", "author_time": 200, "commit": "abc", "lines": 3}
                ]
            },
            "callers": [{"name": "activate"}, {"name": "boot"}],
            "callees": [{"name": "persist"}],
            "types_used": [{"name": "Registry"}],
        });
        let c = CardContext::from_reply(&v);
        assert_eq!(c.callers, vec!["activate", "boot"]);
        assert_eq!(c.callees, vec!["persist"]);
        assert_eq!(c.types_used, vec!["Registry"]);
        assert_eq!(c.body_preview.as_deref(), Some("fn set_active() {"));
        assert_eq!(
            c.blame_line.as_deref(),
            Some("edited by Aaron — fix MRU ordering")
        );
        // Lenient on junk.
        assert_eq!(CardContext::from_reply(&json!({})), CardContext::default());
    }

    #[test]
    fn newer_side_picks_by_recency_across_stacks() {
        use UndoSide::*;
        assert_eq!(newer_side(None, None), Neither);
        assert_eq!(newer_side(Some(5), None), Text);
        assert_eq!(newer_side(None, Some(5)), Bubble);
        assert_eq!(newer_side(Some(7), Some(3)), Text, "text is newer");
        assert_eq!(newer_side(Some(3), Some(7)), Bubble, "bubble is newer");
        // The interleave that motivated the design: word(1) → bubble(2) →
        // word(3) unwinds Text, Bubble, Text.
        let mut text = vec![1u64, 3];
        let mut bubble = vec![2u64];
        let mut order = Vec::new();
        loop {
            match newer_side(text.last().copied(), bubble.last().copied()) {
                Text => {
                    text.pop();
                    order.push("text");
                }
                Bubble => {
                    bubble.pop();
                    order.push("bubble");
                }
                Neither => break,
            }
        }
        assert_eq!(order, vec!["text", "bubble", "text"]);
    }

    #[test]
    fn slots_cluster_around_the_word_and_clamp() {
        let xs = slot_xs(3, 200.0, 30.0, 800.0);
        assert_eq!(xs.len(), 3);
        // Centred: middle slot at the word.
        assert!((xs[1] - 200.0).abs() < 1.0, "{xs:?}");
        assert!((xs[1] - xs[0] - 30.0).abs() < 0.01);
        // Near the left edge: clamped non-negative, still spread.
        let xs = slot_xs(3, 0.0, 30.0, 800.0);
        assert!(xs[0] >= 0.0);
        assert!(xs[2] > xs[1] && xs[1] > xs[0]);
        assert!(slot_xs(0, 100.0, 30.0, 800.0).is_empty());
    }
}

//! Symbol-aware chat composer (Slice F, Plan v2 §5.1 / Build Order §5).
//!
//! As the user types, the composer recognizes symbol-shaped words and
//! `{{anchor}}` references ([`tokenizer`]), resolves them against the active
//! workspace over the pipe ([`ipc_to_workspaces`]:
//! `workspaces.symbols.find` and `workspaces.anchors.find_by_token`), and
//! surfaces the results as per-word chips (count / `?N` ambiguous, Theme
//! `chat_composer.per_word_chip`) plus the message-level context chip
//! (`?N` or `N ▸`, [`context_chip`]). `?N` words open a [`disambiguator`]
//! dropdown; the `▸` opens the curate-before-send list ([`curation`]); and
//! `Ctrl+P` opens the symbol palette (search, then insert an `@symbol`
//! reference at the cursor).
//!
//! **Presentation note (documented deviation):** the spec's IDE-style wavy
//! underline *inside* the input ([`highlight`]) needs glyph-position
//! metrics the shared `TextInput` doesn't expose at this gpui rev — the
//! highlight model (spans + colours + tooltips) is built and tested, and the
//! chip strip is the visible recognition surface until the input grows a
//! decoration API. Recognition is *display-side only* either way: the turn
//! driver (Slice G) re-resolves symbols server-side at send time.

pub mod bubbles;
pub mod context_chip;
pub mod curation;
pub mod disambiguator;
pub mod highlight;
pub mod input;
pub mod ipc_to_workspaces;
pub mod tokenizer;

pub use context_chip::ChipState;
pub use tokenizer::{TokenKind, TokenSpan};

/// One symbol candidate for a recognized word (the GUI mirror of the
/// `workspaces.symbols.find` match entry).
#[derive(Clone, Debug, PartialEq)]
pub struct SymbolCandidate {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: u32,
    pub module_path: String,
    pub score: f32,
}

/// Which durable ignore tier covers a token (Slice M; Plan §5.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IgnoreTierTag {
    Conversation,
    Workspace,
    Global,
}

impl IgnoreTierTag {
    pub fn label(self) -> &'static str {
        match self {
            IgnoreTierTag::Conversation => "conversation",
            IgnoreTierTag::Workspace => "workspace",
            IgnoreTierTag::Global => "global",
        }
    }
}

/// The recognition state of one composer token: what the workspace said
/// about it plus the user's choices (disambiguation pick, curation
/// exclusion, ignore reactivation).
#[derive(Clone, Debug, PartialEq)]
pub struct WordRecognition {
    pub token: TokenSpan,
    /// Symbol candidates (best-first). Empty when the workspace knows no
    /// such symbol.
    pub candidates: Vec<SymbolCandidate>,
    /// How many anchors match this token (`anchors.find_by_token`).
    pub anchor_count: usize,
    /// The user's disambiguation pick (a candidate `id`), if any.
    pub resolved: Option<String>,
    /// Curated out of the upcoming send (the ✕ exclude — this message only,
    /// Plan §5.8).
    pub excluded: bool,
    /// Durable ignore tiers covering this token (Slice M). A covered token
    /// still highlights and counts, but rides along deselected…
    pub ignored_tiers: Vec<IgnoreTierTag>,
    /// …unless reactivated for this message (the ↺, Plan §5.8).
    pub reactivated: bool,
}

impl WordRecognition {
    pub fn new(token: TokenSpan) -> Self {
        WordRecognition {
            token,
            candidates: Vec::new(),
            anchor_count: 0,
            resolved: None,
            excluded: false,
            ignored_tiers: Vec::new(),
            reactivated: false,
        }
    }

    /// Is any durable ignore tier covering this token?
    pub fn is_ignored(&self) -> bool {
        !self.ignored_tiers.is_empty()
    }

    /// Will this word's context actually ride along with the send?
    /// (Recognized, not ✕-excluded, and not ignore-deselected without a ↺.)
    pub fn is_included(&self) -> bool {
        self.is_recognized() && !self.excluded && (!self.is_ignored() || self.reactivated)
    }

    /// Did the workspace recognize this token at all?
    pub fn is_recognized(&self) -> bool {
        self.anchor_count > 0 || !self.candidates.is_empty()
    }

    /// Multiple symbol candidates and no user pick yet → `?N`.
    pub fn is_ambiguous(&self) -> bool {
        self.resolved.is_none() && self.candidates.len() > 1
    }

    /// The symbol this word ultimately refers to (the user's pick, or the
    /// single unambiguous candidate).
    pub fn effective_symbol(&self) -> Option<&SymbolCandidate> {
        if let Some(id) = &self.resolved {
            return self.candidates.iter().find(|c| &c.id == id);
        }
        if self.candidates.len() == 1 {
            return self.candidates.first();
        }
        None
    }

    /// The per-word chip label (Plan §5.1): `?N` while ambiguous, else the
    /// count of associated context items (anchors + the resolved symbol).
    /// `None` → no chip (unrecognized word).
    pub fn chip_label(&self) -> Option<String> {
        if self.is_ambiguous() {
            return Some(format!("?{}", self.candidates.len()));
        }
        let count = self.anchor_count + usize::from(self.effective_symbol().is_some());
        (count > 0).then(|| format!("{count}"))
    }
}

/// The composer's recognition state, owned by the Chat panel: the latest
/// scan results plus which popover (disambiguation / curation / palette) is
/// open. A monotonically increasing `generation` discards stale async
/// lookups (the user kept typing).
#[derive(Default)]
pub struct ComposerState {
    pub words: Vec<WordRecognition>,
    /// Bumped on every text change; lookup replies carrying an older
    /// generation are dropped.
    pub generation: u64,
    /// Open disambiguation dropdown: index into `words`.
    pub disambiguating: Option<usize>,
    /// "Anchor this?" offer after a disambiguation pick (Slice N): index
    /// into `words`.
    pub anchor_offer: Option<usize>,
    /// Open right-click ignore menu: index into `words` (Slice M).
    pub ignore_menu: Option<usize>,
    /// Curate-before-send popover open?
    pub curating: bool,
    /// `Ctrl+P` symbol palette state.
    pub palette: Option<PaletteState>,
    /// The workspaces service was unreachable on the last scan (recognition
    /// degraded — tooltip/strip hint, never an error wall).
    pub degraded: bool,
}

impl ComposerState {
    /// The message-level chip for the current words.
    pub fn chip(&self) -> ChipState {
        ChipState::derive(&self.words)
    }

    /// Begin a new scan generation (text changed): bump + close stale
    /// popovers that index into the old word set.
    pub fn begin_scan(&mut self) -> u64 {
        self.generation += 1;
        self.disambiguating = None;
        self.anchor_offer = None;
        self.ignore_menu = None;
        self.curating = false;
        self.generation
    }

    /// Install lookup results if they're still current.
    pub fn install(
        &mut self,
        generation: u64,
        words: Vec<WordRecognition>,
        degraded: bool,
    ) -> bool {
        if generation != self.generation {
            return false;
        }
        self.words = words;
        self.degraded = degraded;
        true
    }

    /// Resolve an ambiguous word to one of its candidates (disambiguator
    /// pick). Returns whether anything changed.
    pub fn resolve(&mut self, word_idx: usize, candidate_id: &str) -> bool {
        let Some(w) = self.words.get_mut(word_idx) else {
            return false;
        };
        if !w.candidates.iter().any(|c| c.id == candidate_id) {
            return false;
        }
        w.resolved = Some(candidate_id.to_owned());
        self.disambiguating = None;
        true
    }

    /// Toggle a word in/out of the upcoming send. For an ignored word this
    /// flips the per-message ↺ reactivation (Plan §5.8: ignored = default
    /// inactive, reactivate per message); otherwise it flips the ✕ exclude.
    pub fn toggle_excluded(&mut self, word_idx: usize) -> bool {
        match self.words.get_mut(word_idx) {
            Some(w) => {
                if w.is_ignored() {
                    w.reactivated = !w.reactivated;
                } else {
                    w.excluded = !w.excluded;
                }
                true
            }
            None => false,
        }
    }

    /// The per-message token lists the send carries (Slices F + M):
    /// `(excluded_tokens, reactivated_tokens)`.
    pub fn send_overrides(&self) -> (Vec<String>, Vec<String>) {
        let excluded = self
            .words
            .iter()
            .filter(|w| w.excluded)
            .map(|w| w.token.text.clone())
            .collect();
        let reactivated = self
            .words
            .iter()
            .filter(|w| w.is_ignored() && w.reactivated)
            .map(|w| w.token.text.clone())
            .collect();
        (excluded, reactivated)
    }

    /// The first still-ambiguous word (the `?N` context chip's click
    /// target).
    pub fn first_ambiguous(&self) -> Option<usize> {
        self.words.iter().position(WordRecognition::is_ambiguous)
    }
}

/// Can this word's text be an anchor `{{identifier}}`? (alphanumeric +
/// underscore — `path::segments` and `dotted.names` can't.)
pub fn is_anchorable_identifier(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `Ctrl+P` symbol palette state: a query + its hits + keyboard selection.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PaletteState {
    pub query: String,
    pub hits: Vec<SymbolCandidate>,
    pub selected: usize,
    /// Bumped per palette query; stale replies dropped (same pattern as the
    /// scan generation).
    pub generation: u64,
}

impl PaletteState {
    pub fn select_next(&mut self) {
        if !self.hits.is_empty() {
            self.selected = (self.selected + 1) % self.hits.len();
        }
    }

    pub fn select_prev(&mut self) {
        if !self.hits.is_empty() {
            self.selected = (self.selected + self.hits.len() - 1) % self.hits.len();
        }
    }

    pub fn selection(&self) -> Option<&SymbolCandidate> {
        self.hits.get(self.selected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(text: &str) -> TokenSpan {
        TokenSpan {
            text: text.to_owned(),
            start: 0,
            end: text.len(),
            kind: TokenKind::Identifier,
        }
    }

    fn candidate(id: &str) -> SymbolCandidate {
        SymbolCandidate {
            id: id.to_owned(),
            name: id.to_owned(),
            kind: "Function".to_owned(),
            file: format!("src/{id}.rs"),
            line: 1,
            module_path: String::new(),
            score: 1.0,
        }
    }

    fn word(text: &str, candidates: usize, anchors: usize) -> WordRecognition {
        let mut w = WordRecognition::new(token(text));
        w.candidates = (0..candidates)
            .map(|i| candidate(&format!("{text}-{i}")))
            .collect();
        w.anchor_count = anchors;
        w
    }

    #[test]
    fn chip_labels_follow_plan_5_1() {
        assert_eq!(word("plain", 0, 0).chip_label(), None);
        assert_eq!(word("one", 1, 0).chip_label().as_deref(), Some("1"));
        assert_eq!(word("anchored", 0, 3).chip_label().as_deref(), Some("3"));
        assert_eq!(word("both", 1, 2).chip_label().as_deref(), Some("3"));
        assert_eq!(word("ambig", 4, 1).chip_label().as_deref(), Some("?4"));
    }

    #[test]
    fn resolving_clears_ambiguity_and_counts_the_pick() {
        let mut state = ComposerState {
            words: vec![word("ambig", 3, 0)],
            ..Default::default()
        };
        assert!(state.words[0].is_ambiguous());
        assert!(state.resolve(0, "ambig-1"));
        assert!(!state.words[0].is_ambiguous());
        assert_eq!(state.words[0].chip_label().as_deref(), Some("1"));
        assert_eq!(state.words[0].effective_symbol().unwrap().id, "ambig-1");
        // Unknown candidate / index rejected.
        assert!(!state.resolve(0, "nope"));
        assert!(!state.resolve(9, "ambig-1"));
    }

    #[test]
    fn generation_guard_drops_stale_lookups() {
        let mut state = ComposerState::default();
        let g1 = state.begin_scan();
        let g2 = state.begin_scan();
        assert!(!state.install(g1, vec![word("old", 1, 0)], false), "stale");
        assert!(state.words.is_empty());
        assert!(state.install(g2, vec![word("new", 1, 0)], true));
        assert_eq!(state.words.len(), 1);
        assert!(state.degraded);
    }

    #[test]
    fn begin_scan_closes_popovers() {
        let mut state = ComposerState {
            words: vec![word("a", 2, 0)],
            disambiguating: Some(0),
            curating: true,
            ..Default::default()
        };
        state.begin_scan();
        assert!(state.disambiguating.is_none() && !state.curating);
    }

    #[test]
    fn first_ambiguous_finds_the_chip_target() {
        let mut state = ComposerState {
            words: vec![word("clear", 1, 0), word("ambig", 2, 0)],
            ..Default::default()
        };
        assert_eq!(state.first_ambiguous(), Some(1));
        state.resolve(1, "ambig-0");
        assert_eq!(state.first_ambiguous(), None);
    }

    #[test]
    fn ignore_semantics_follow_plan_5_8() {
        // Ignored: still recognized + chip still shows, but not included…
        let mut w = word("muted", 1, 0);
        w.ignored_tiers = vec![IgnoreTierTag::Workspace];
        assert!(w.is_recognized());
        assert!(w.chip_label().is_some(), "still highlights + counts");
        assert!(!w.is_included(), "default-inactive");

        // …until reactivated for this message (↺).
        w.reactivated = true;
        assert!(w.is_included());

        // toggle_excluded on an ignored word flips ↺, not ✕.
        let mut state = ComposerState {
            words: vec![w],
            ..Default::default()
        };
        assert!(state.toggle_excluded(0));
        assert!(!state.words[0].reactivated);
        assert!(!state.words[0].excluded, "✕ untouched for ignored words");

        // send_overrides carries the two lists.
        state.words[0].reactivated = true;
        state.words.push({
            let mut x = word("dropped", 1, 0);
            x.excluded = true;
            x
        });
        let (excluded, reactivated) = state.send_overrides();
        assert_eq!(excluded, vec!["dropped"]);
        assert_eq!(reactivated, vec!["muted"]);
    }

    #[test]
    fn palette_selection_wraps() {
        let mut p = PaletteState {
            hits: vec![candidate("a"), candidate("b"), candidate("c")],
            ..Default::default()
        };
        p.select_prev();
        assert_eq!(p.selection().unwrap().id, "c");
        p.select_next();
        p.select_next();
        assert_eq!(p.selection().unwrap().id, "b");
        let empty = PaletteState::default();
        assert!(empty.selection().is_none());
    }
}

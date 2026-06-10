//! Composer tokenizer (Slice F, Build Order §5) — detect symbol-like
//! identifiers and `{{anchor}}` references as the user types.
//!
//! Pure + gpui-free. The scan is deliberately conservative: every
//! `{{anchor}}` span is always a candidate, while a bare word is only a
//! candidate when it *looks like code* (`snake_case`, `CamelCase`,
//! `path::segments`, or a `dotted.path`) — plain prose words are skipped so
//! the composer isn't firing pipe lookups for "the" and "should". The
//! server-side context-gather (Slice G) still resolves everything at send
//! time; composer recognition is the *visible* subset.

/// What a token claims to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    /// A bare identifier-looking word (`set_active`, `GraphView`,
    /// `turn::driver`, `chat_panel.rs`).
    Identifier,
    /// A `{{…}}` anchor reference (spaces allowed inside, Slice N-data
    /// alias rules).
    AnchorRef,
}

/// One recognized span in the composer text. `start..end` are byte offsets
/// into the scanned string (the future in-input underline consumes them; the
/// chip strip only needs `text`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenSpan {
    pub text: String,
    pub start: usize,
    pub end: usize,
    pub kind: TokenKind,
}

/// Hard cap on candidates per scan — a pasted code block shouldn't fan out
/// into hundreds of pipe lookups. First-seen wins (reading order), duplicate
/// words collapse to the first occurrence.
pub const MAX_CANDIDATES: usize = 12;

/// Scan `text` for candidate tokens (see module docs for the heuristic).
pub fn scan(text: &str) -> Vec<TokenSpan> {
    let mut out: Vec<TokenSpan> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let bytes = text.as_bytes();
    let mut i = 0;

    while i < bytes.len() && out.len() < MAX_CANDIDATES {
        // `{{anchor token}}` — spaces allowed inside (alias lookups).
        if bytes[i] == b'{' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            if let Some(close) = text[i + 2..].find("}}") {
                let inner = text[i + 2..i + 2 + close].trim();
                let end = i + 2 + close + 2;
                if !inner.is_empty() && seen.insert(inner.to_owned()) {
                    out.push(TokenSpan {
                        text: inner.to_owned(),
                        start: i,
                        end,
                        kind: TokenKind::AnchorRef,
                    });
                }
                i = end;
                continue;
            }
        }

        // A word: ASCII identifier chars plus `:` and `.` joiners.
        if is_word_start(bytes[i]) {
            let start = i;
            while i < bytes.len() && is_word_char(bytes[i]) {
                i += 1;
            }
            // Trim trailing joiners (sentence punctuation: "driver.").
            let mut end = i;
            while end > start && matches!(bytes[end - 1], b':' | b'.') {
                end -= 1;
            }
            let word = &text[start..end];
            if looks_like_code(word) && seen.insert(word.to_owned()) {
                out.push(TokenSpan {
                    text: word.to_owned(),
                    start,
                    end,
                    kind: TokenKind::Identifier,
                });
            }
            continue;
        }

        // Skip ahead to the next ASCII boundary (UTF-8-safe: continuation
        // bytes never match the start tests above).
        i += 1;
    }
    out
}

fn is_word_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b':' | b'.')
}

/// The code-shaped heuristic: at least 3 chars AND one of `_`, `::`, an
/// interior `.` path, or mixed case (`CamelCase` / `mixedCase`).
fn looks_like_code(word: &str) -> bool {
    if word.len() < 3 {
        return false;
    }
    if word.contains('_') || word.contains("::") {
        return true;
    }
    // Interior dot (path / method): "chat_panel.rs", "self.layout" — but not
    // a sentence-final period (already trimmed) or a leading dot.
    if word[1..word.len() - 1].contains('.') {
        return true;
    }
    // Mixed case after the first char (CamelCase, mixedCase) — but not
    // ALL-CAPS prose like "TODO" (still allowed: 3+ caps with a lowercase
    // tail like "GraphView" hits the mixed test).
    let has_upper_tail = word.chars().skip(1).any(|c| c.is_ascii_uppercase());
    let has_lower = word.chars().any(|c| c.is_ascii_lowercase());
    has_upper_tail && has_lower
}

#[cfg(test)]
#[path = "tests/tokenizer_tests.rs"]
mod tokenizer_tests;

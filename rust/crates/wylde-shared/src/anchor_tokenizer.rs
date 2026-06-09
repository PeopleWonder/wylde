//! The `{{identifier}}` anchor tokenizer — shared by `wylde-workspaces` (verb
//! routing / token resolution) and `wylde-harness` (the chat input), so both
//! sides recognise the exact same grammar.
//!
//! Two patterns (Plan v2 §4.2):
//!
//!   * **Anchor token** — `{{identifier}}`. Roam-style, deliberately avoids
//!     `#hashtag` / markdown collisions. Saved anchors are referenced by
//!     typing `{{name}}` directly in chat.
//!   * **Qualifier** — `symbol{{qualifier}}`. A bare word immediately followed
//!     by a `{{…}}` token, used by the composer to disambiguate which sense of
//!     an ambiguous `symbol` the user means.
//!
//! Hand-rolled (no `regex` dependency — `wylde-shared` deliberately stays
//! lean) and byte-position accurate: every span carries the half-open
//! `[start, end)` byte range of the **whole** match in the source `text`, so a
//! composer can map a hit back to a highlight range. The inner `identifier`
//! must be a valid token ([`is_valid_identifier`]); `{{ }}` / `{{ bad name }}`
//! / unterminated `{{` are not matches.

/// A matched `{{identifier}}` token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenSpan {
    /// The inner identifier, without the braces (e.g. `set_active`).
    pub identifier: String,
    /// Byte offset of the opening `{` in the source text.
    pub start: usize,
    /// Byte offset just past the closing `}` in the source text.
    pub end: usize,
}

/// A matched `symbol{{qualifier}}` pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QualifierSpan {
    /// The bare leading word (e.g. `load` in `load{{config}}`).
    pub symbol: String,
    /// The qualifier inside the braces (e.g. `config`).
    pub qualifier: String,
    /// Byte offset of the first char of `symbol`.
    pub start: usize,
    /// Byte offset just past the closing `}`.
    pub end: usize,
}

/// Whether `s` is a well-formed anchor identifier: non-empty, ASCII
/// alphanumeric + underscore only (no spaces, no punctuation, no braces).
///
/// This is the single source of truth for identifier validity — the tokenizer
/// rejects non-conforming inner text, and the `anchors.create` verbs validate
/// the requested identifier against it before persisting.
pub fn is_valid_identifier(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Whether `b` may appear in an identifier (ASCII alnum or `_`).
fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Parse every `{{identifier}}` token in `text`, in source order.
///
/// Only well-formed tokens match: the inner text must be a valid identifier
/// ([`is_valid_identifier`]). `{{}}`, `{{ spaced }}`, `{{a-b}}`, and an
/// unterminated `{{` are skipped. Triple braces like `{{{x}}}` match the inner
/// `{{x}}` (the leading `{` is left as ordinary text).
pub fn parse_anchors(text: &str) -> Vec<TokenSpan> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let inner_start = i + 2;
            if let Some(rel) = find_close(&bytes[inner_start..]) {
                let inner_end = inner_start + rel;
                let inner = &text[inner_start..inner_end];
                if is_valid_identifier(inner) {
                    out.push(TokenSpan {
                        identifier: inner.to_owned(),
                        start: i,
                        end: inner_end + 2, // past the closing `}}`
                    });
                    i = inner_end + 2;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

/// Parse every `symbol{{qualifier}}` pattern in `text`, in source order.
///
/// The leading `symbol` is the run of identifier bytes immediately before a
/// `{{qualifier}}` token, with no separator between them. A `{{token}}` not
/// preceded by an identifier byte yields no qualifier (it's a plain anchor).
pub fn parse_qualifiers(text: &str) -> Vec<QualifierSpan> {
    let bytes = text.as_bytes();
    parse_anchors(text)
        .into_iter()
        .filter_map(|tok| {
            // The byte just before the opening `{{` must be an identifier byte.
            if tok.start == 0 || !is_ident_byte(bytes[tok.start - 1]) {
                return None;
            }
            // Walk back over the leading symbol's bytes.
            let mut s = tok.start;
            while s > 0 && is_ident_byte(bytes[s - 1]) {
                s -= 1;
            }
            Some(QualifierSpan {
                symbol: text[s..tok.start].to_owned(),
                qualifier: tok.identifier,
                start: s,
                end: tok.end,
            })
        })
        .collect()
}

/// Find the byte offset (relative to the slice start) of the `}}` that closes a
/// token whose inner text began at the slice start. Scans only identifier-legal
/// bytes; the first non-identifier byte that isn't the closing `}` aborts the
/// match (so `{{a b}}` doesn't match). Returns the offset of the first `}`.
fn find_close(inner: &[u8]) -> Option<usize> {
    let mut j = 0;
    while j + 1 < inner.len() {
        match inner[j] {
            b'}' if inner[j + 1] == b'}' => return Some(j),
            b if is_ident_byte(b) => j += 1,
            _ => return None,
        }
    }
    None
}

/// Strip the surrounding `{{ }}` from a token if present, returning the inner
/// identifier; otherwise return the input trimmed. Lets a verb accept either
/// `{{name}}` or bare `name` from a caller. Returns `None` if the result isn't
/// a valid identifier.
pub fn normalize_token(token: &str) -> Option<String> {
    let t = token.trim();
    let inner = t
        .strip_prefix("{{")
        .and_then(|s| s.strip_suffix("}}"))
        .unwrap_or(t);
    if is_valid_identifier(inner) {
        Some(inner.to_owned())
    } else {
        None
    }
}

/// Collapse a string's whitespace for stable comparison: trim the ends and
/// replace every run of ASCII whitespace with a single space. The single
/// source of truth for how an **alias** (which may contain spaces) is
/// normalised on both the write side ([`crate::anchor::validate_aliases`]) and
/// the lookup side ([`normalize_lookup_token`]), so a stored alias `"set
/// active"` matches a typed `"set   active"`.
pub fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Normalize a **lookup** token for `find_by_token` resolution: strip an
/// optional surrounding `{{ }}`, trim, and collapse internal whitespace
/// ([`collapse_whitespace`]).
///
/// Unlike [`normalize_token`], this deliberately **permits spaces** so a
/// human-friendly anchor *alias* (`"set active"`) resolves the same way a
/// canonical identifier (`"set_active_graph_view"`) does — the alias lookup
/// layer (Slice N-data-aliases) compares the result against both an anchor's
/// `identifier` and its `aliases`. Returns `None` only when the token is empty
/// after trimming. (The `{{identifier}}` *tokenizer* grammar is unchanged:
/// `parse_anchors` still only spans alphanumeric-+-underscore tokens; this is
/// the resolver layer the brief refers to, accepting whatever a caller passes
/// to the verb.)
pub fn normalize_lookup_token(token: &str) -> Option<String> {
    let t = token.trim();
    let inner = t
        .strip_prefix("{{")
        .and_then(|s| s.strip_suffix("}}"))
        .unwrap_or(t);
    let collapsed = collapse_whitespace(inner);
    if collapsed.is_empty() {
        None
    } else {
        Some(collapsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_token() {
        let spans = parse_anchors("see {{set_active}} now");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].identifier, "set_active");
        assert_eq!(
            &"see {{set_active}} now"[spans[0].start..spans[0].end],
            "{{set_active}}"
        );
    }

    #[test]
    fn parses_multiple_tokens_in_order() {
        let spans = parse_anchors("{{a}} and {{b_2}} and {{c}}");
        let ids: Vec<&str> = spans.iter().map(|s| s.identifier.as_str()).collect();
        assert_eq!(ids, ["a", "b_2", "c"]);
    }

    #[test]
    fn rejects_malformed_tokens() {
        assert!(parse_anchors("{{}}").is_empty(), "empty");
        assert!(parse_anchors("{{ spaced }}").is_empty(), "internal spaces");
        assert!(parse_anchors("{{a-b}}").is_empty(), "hyphen");
        assert!(parse_anchors("{{unterminated").is_empty(), "no close");
        assert!(parse_anchors("{single}").is_empty(), "single braces");
        assert!(parse_anchors("plain text").is_empty());
    }

    #[test]
    fn triple_braces_match_inner() {
        let spans = parse_anchors("{{{x}}}");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].identifier, "x");
        // The inner {{x}} matched; leading `{` and trailing `}` are text.
        assert_eq!(&"{{{x}}}"[spans[0].start..spans[0].end], "{{x}}");
    }

    #[test]
    fn adjacent_tokens() {
        let spans = parse_anchors("{{a}}{{b}}");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].identifier, "a");
        assert_eq!(spans[1].identifier, "b");
        assert_eq!(spans[1].start, spans[0].end);
    }

    #[test]
    fn qualifier_requires_leading_symbol() {
        let q = parse_qualifiers("load{{config}}");
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].symbol, "load");
        assert_eq!(q[0].qualifier, "config");
        assert_eq!(&"load{{config}}"[q[0].start..q[0].end], "load{{config}}");
    }

    #[test]
    fn plain_anchor_is_not_a_qualifier() {
        // A space before `{{` means it's a standalone anchor, not a qualifier.
        assert!(parse_qualifiers("load {{config}}").is_empty());
        // Same token still parses as a plain anchor.
        assert_eq!(parse_anchors("load {{config}}").len(), 1);
    }

    #[test]
    fn qualifier_walks_back_full_symbol() {
        let q = parse_qualifiers("xs.parse_value{{int}} done");
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].symbol, "parse_value", "stops at the dot");
        assert_eq!(q[0].qualifier, "int");
    }

    #[test]
    fn identifier_validation_rules() {
        assert!(is_valid_identifier("a"));
        assert!(is_valid_identifier("set_active_graph_view"));
        assert!(is_valid_identifier("X123_y"));
        assert!(!is_valid_identifier(""));
        assert!(!is_valid_identifier("has space"));
        assert!(!is_valid_identifier("dash-no"));
        assert!(!is_valid_identifier("braces{{"));
        assert!(!is_valid_identifier("emoji😀"));
    }

    #[test]
    fn normalize_accepts_both_forms() {
        assert_eq!(normalize_token("{{name}}").as_deref(), Some("name"));
        assert_eq!(normalize_token("  name  ").as_deref(), Some("name"));
        assert_eq!(normalize_token("{{a_b}}").as_deref(), Some("a_b"));
        assert_eq!(normalize_token("{{bad name}}"), None);
        assert_eq!(normalize_token(""), None);
    }

    #[test]
    fn collapse_whitespace_trims_and_collapses_runs() {
        assert_eq!(collapse_whitespace("  set   active  "), "set active");
        assert_eq!(collapse_whitespace("set\tactive\nview"), "set active view");
        assert_eq!(collapse_whitespace("solo"), "solo");
        assert_eq!(collapse_whitespace("   "), "");
    }

    #[test]
    fn normalize_lookup_token_permits_spaces_and_strips_braces() {
        // Spaces are allowed (alias lookup) — unlike `normalize_token`.
        assert_eq!(
            normalize_lookup_token("{{set active}}").as_deref(),
            Some("set active")
        );
        assert_eq!(
            normalize_lookup_token("  set   active  ").as_deref(),
            Some("set active")
        );
        // A plain identifier still resolves to itself.
        assert_eq!(
            normalize_lookup_token("set_active").as_deref(),
            Some("set_active")
        );
        // Empty / whitespace-only → None (the verb returns bad_request).
        assert_eq!(normalize_lookup_token(""), None);
        assert_eq!(normalize_lookup_token("{{   }}"), None);
        // `normalize_token` would have rejected the spaced form outright.
        assert_eq!(normalize_token("{{set active}}"), None);
    }

    #[test]
    fn unicode_text_byte_offsets_are_consistent() {
        // Multibyte leading text must not corrupt the span offsets.
        let text = "café {{beans}}";
        let spans = parse_anchors(text);
        assert_eq!(spans.len(), 1);
        assert_eq!(&text[spans[0].start..spans[0].end], "{{beans}}");
    }
}

//! Tokenizer suite (Build Order §5 file tree → `composer/tests/`).

use super::*;

fn texts(spans: &[TokenSpan]) -> Vec<&str> {
    spans.iter().map(|s| s.text.as_str()).collect()
}

#[test]
fn finds_code_shaped_words_and_skips_prose() {
    let spans = scan("please look at set_active and GraphView in turn::driver, the rest is prose");
    assert_eq!(
        texts(&spans),
        vec!["set_active", "GraphView", "turn::driver"]
    );
    for s in &spans {
        assert_eq!(s.kind, TokenKind::Identifier);
    }
}

#[test]
fn finds_anchor_refs_with_spaces() {
    let spans = scan("explain {{set active}} and {{registry}} please");
    assert_eq!(texts(&spans), vec!["set active", "registry"]);
    assert!(spans.iter().all(|s| s.kind == TokenKind::AnchorRef));
    // Byte offsets cover the braces.
    assert_eq!(
        &"explain {{set active}} and"[spans[0].start..spans[0].end],
        "{{set active}}"
    );
}

#[test]
fn unclosed_braces_fall_through_to_words() {
    let spans = scan("look at {{set_active please");
    assert_eq!(texts(&spans), vec!["set_active"]);
    assert_eq!(spans[0].kind, TokenKind::Identifier);
}

#[test]
fn dotted_paths_count_sentence_periods_do_not() {
    let spans = scan("open chat_panel.rs. Also self.layout works.");
    assert_eq!(texts(&spans), vec!["chat_panel.rs", "self.layout"]);
    // The trailing sentence period is trimmed from the span.
    assert!(!spans[0].text.ends_with('.'));
}

#[test]
fn duplicates_collapse_to_first_occurrence() {
    let spans = scan("set_active then set_active again {{set_active}}");
    // The bare word claims "set_active" first; the anchor ref of the same
    // text dedupes away (one lookup per distinct token).
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].start, 0);
}

#[test]
fn caps_at_max_candidates() {
    let text = (0..40)
        .map(|i| format!("token_{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    let spans = scan(&text);
    assert_eq!(spans.len(), MAX_CANDIDATES);
}

#[test]
fn short_and_all_caps_prose_skipped() {
    let spans = scan("a if We TODO NASA but setX");
    assert_eq!(
        texts(&spans),
        vec!["setX"],
        "mixed case qualifies, caps prose doesn't"
    );
}

#[test]
fn utf8_text_is_safe() {
    let spans = scan("émoji 🎉 then set_active — done");
    assert_eq!(texts(&spans), vec!["set_active"]);
}

#[test]
fn empty_and_prose_only_yield_nothing() {
    assert!(scan("").is_empty());
    assert!(scan("just a plain sentence with no code at all").is_empty());
}

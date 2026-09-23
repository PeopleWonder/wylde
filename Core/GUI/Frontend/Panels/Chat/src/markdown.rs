//! Minimal markdown → gpui renderer for chat bubbles.
//!
//! Alpha subset:
//!   * Paragraphs (blank-line separated).
//!   * ATX headings `#` / `##` / `###` (4+ collapses to `###`).
//!   * Bullet lists (`-` / `*`) and numbered lists (`1.` `2.` …).
//!   * Fenced code blocks (```).
//!   * Inline code (`x`), bold (`**x**`), italic (`*x*` / `_x_`).
//!   * Links (`[text](url)`) — open in the OS default browser on click.
//!
//! Deferred (with cause):
//!   * **Tables / images / blockquotes / footnotes / HTML / setext
//!     headings.**  Each requires a deeper parser state and richer
//!     visual elements (table layout, image cache, etc.).  The chat
//!     bubble subset above covers what a streamed LLM reply emits
//!     ~95% of the time; the rest waits for a slice that needs them.
//!
//! Why an in-tree parser rather than `pulldown-cmark`?
//!   * gpui rendering is fine-grained (one Div per inline span).  A
//!     full CommonMark parser produces an event stream that's ergonomic
//!     for HTML emitters but awkward to fold into gpui elements; we'd
//!     end up writing a converter half this length.
//!   * The lock file delta is non-trivial (5+ transitive crates).
//!   * Markdown subset risk is bounded: a malformed input renders as
//!     plain text, never panics.

use gpui::{
    div, prelude::*, px, rgb, ElementId, FontWeight, IntoElement, MouseDownEvent, SharedString,
    Stateful,
};
use wylde_theme::colors::{
    BORDER_SUBTLE, BRAND_LIGHT, SURFACE_700, SURFACE_750, TEXT_PRIMARY, TEXT_SECONDARY,
};
use wylde_theme::typography::{size as text_size, weight, FAMILY_INTER};

use crate::chat_panel::pack;
use wylde_gui_controls::control;

/// Block-level markdown element.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Paragraph(Vec<Inline>),
    Heading(u8, Vec<Inline>),
    BulletList(Vec<Vec<Inline>>),
    OrderedList(Vec<Vec<Inline>>),
    CodeBlock { lang: String, body: String },
}

/// Inline-level markdown element.
#[derive(Debug, Clone, PartialEq)]
pub enum Inline {
    Text(String),
    Bold(Vec<Inline>),
    Italic(Vec<Inline>),
    Code(String),
    Link { text: String, url: String },
}

// ── Parser ───────────────────────────────────────────────────────────

/// Parse `src` into a list of block elements.  Empty input yields an
/// empty Vec.
pub fn parse(src: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let lines: Vec<&str> = src.split('\n').collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];

        // Skip blank lines between blocks.
        if line.trim().is_empty() {
            i += 1;
            continue;
        }

        // Fenced code block.
        if let Some(lang) = fence_language(line) {
            let mut body = String::new();
            i += 1;
            while i < lines.len() && fence_language(lines[i]).is_none() {
                if !body.is_empty() {
                    body.push('\n');
                }
                body.push_str(lines[i]);
                i += 1;
            }
            if i < lines.len() {
                i += 1; // consume the closing fence
            }
            blocks.push(Block::CodeBlock { lang, body });
            continue;
        }

        // Heading.
        if let Some((level, content)) = parse_heading(line) {
            blocks.push(Block::Heading(level, parse_inlines(content)));
            i += 1;
            continue;
        }

        // Bullet list.
        if is_bullet(line) {
            let mut items = Vec::new();
            while i < lines.len() && is_bullet(lines[i]) {
                let content = trim_bullet(lines[i]);
                items.push(parse_inlines(content));
                i += 1;
            }
            blocks.push(Block::BulletList(items));
            continue;
        }

        // Ordered list.
        if is_ordered(line) {
            let mut items = Vec::new();
            while i < lines.len() && is_ordered(lines[i]) {
                let content = trim_ordered(lines[i]);
                items.push(parse_inlines(content));
                i += 1;
            }
            blocks.push(Block::OrderedList(items));
            continue;
        }

        // Paragraph — collect contiguous non-blank, non-special lines.
        let mut buf = String::from(line);
        i += 1;
        while i < lines.len()
            && !lines[i].trim().is_empty()
            && !is_bullet(lines[i])
            && !is_ordered(lines[i])
            && parse_heading(lines[i]).is_none()
            && fence_language(lines[i]).is_none()
        {
            buf.push(' ');
            buf.push_str(lines[i].trim_start());
            i += 1;
        }
        blocks.push(Block::Paragraph(parse_inlines(&buf)));
    }

    blocks
}

fn parse_heading(line: &str) -> Option<(u8, &str)> {
    let trimmed = line.trim_start();
    let mut count = 0u8;
    for ch in trimmed.chars() {
        if ch == '#' && count < 6 {
            count += 1;
        } else {
            break;
        }
    }
    if count == 0 {
        return None;
    }
    let rest = &trimmed[count as usize..];
    if !rest.starts_with(' ') && !rest.is_empty() {
        return None;
    }
    Some((count.min(3), rest.trim()))
}

fn fence_language(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix("```") {
        return Some(rest.trim().to_owned());
    }
    None
}

fn is_bullet(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("- ") || trimmed.starts_with("* ")
}

fn trim_bullet(line: &str) -> &str {
    let trimmed = line.trim_start();
    trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .unwrap_or(trimmed)
}

fn is_ordered(line: &str) -> bool {
    let trimmed = line.trim_start();
    let mut chars = trimmed.chars();
    let mut saw_digit = false;
    for c in chars.by_ref() {
        if c.is_ascii_digit() {
            saw_digit = true;
            continue;
        }
        if saw_digit && c == '.' {
            return chars.next() == Some(' ');
        }
        return false;
    }
    false
}

fn trim_ordered(line: &str) -> &str {
    let trimmed = line.trim_start();
    // Skip digits, the '.', and the space.
    let mut idx = 0;
    let bytes = trimmed.as_bytes();
    while idx < bytes.len() && bytes[idx].is_ascii_digit() {
        idx += 1;
    }
    if idx < bytes.len() && bytes[idx] == b'.' {
        idx += 1;
    }
    if idx < bytes.len() && bytes[idx] == b' ' {
        idx += 1;
    }
    &trimmed[idx..]
}

/// Inline parser.  Lightweight; emits literal text on any malformed
/// formatting marker so the output is never an error string.
pub fn parse_inlines(text: &str) -> Vec<Inline> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut i = 0;

    let flush = |buf: &mut String, out: &mut Vec<Inline>| {
        if !buf.is_empty() {
            out.push(Inline::Text(std::mem::take(buf)));
        }
    };

    while i < chars.len() {
        let c = chars[i];

        // Inline code: `…`
        if c == '`' {
            if let Some(end) = chars[i + 1..].iter().position(|&x| x == '`') {
                let body: String = chars[i + 1..i + 1 + end].iter().collect();
                flush(&mut buf, &mut out);
                out.push(Inline::Code(body));
                i += end + 2;
                continue;
            }
        }

        // Bold: **…**
        if c == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if let Some(end) = find_close(&chars, i + 2, "**") {
                let inner: String = chars[i + 2..end].iter().collect();
                flush(&mut buf, &mut out);
                out.push(Inline::Bold(parse_inlines(&inner)));
                i = end + 2;
                continue;
            }
        }

        // Italic: *…* (not part of **) or _…_
        if (c == '*' || c == '_')
            && (i == 0 || !is_word_char(chars[i - 1]) || c == '_')
            && i + 1 < chars.len()
            && chars[i + 1] != c
        {
            let marker = c.to_string();
            if let Some(end) = find_close(&chars, i + 1, &marker) {
                let inner: String = chars[i + 1..end].iter().collect();
                if !inner.is_empty() && !inner.contains('\n') {
                    flush(&mut buf, &mut out);
                    out.push(Inline::Italic(parse_inlines(&inner)));
                    i = end + 1;
                    continue;
                }
            }
        }

        // Link: [text](url)
        if c == '[' {
            if let Some(text_end) = chars[i + 1..].iter().position(|&x| x == ']') {
                let after = i + 1 + text_end + 1;
                if after < chars.len() && chars[after] == '(' {
                    if let Some(url_end) = chars[after + 1..].iter().position(|&x| x == ')') {
                        let link_text: String = chars[i + 1..i + 1 + text_end].iter().collect();
                        let url: String = chars[after + 1..after + 1 + url_end].iter().collect();
                        flush(&mut buf, &mut out);
                        out.push(Inline::Link {
                            text: link_text,
                            url,
                        });
                        i = after + url_end + 2;
                        continue;
                    }
                }
            }
        }

        buf.push(c);
        i += 1;
    }
    flush(&mut buf, &mut out);
    out
}

fn find_close(chars: &[char], start: usize, marker: &str) -> Option<usize> {
    let m: Vec<char> = marker.chars().collect();
    let mut i = start;
    while i + m.len() <= chars.len() {
        if chars[i..i + m.len()] == m[..] {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

// ── Renderer ─────────────────────────────────────────────────────────

/// Render parsed blocks into a flex-col container.  `bubble_id` makes
/// element ids unique across bubbles in the same log.
pub fn render(blocks: &[Block], bubble_id: &str) -> gpui::Div {
    let mut col = div().flex().flex_col().gap(px(8.0));
    for (i, block) in blocks.iter().enumerate() {
        let key = format!("{bubble_id}::b{i}");
        col = col.child(render_block(block, &key));
    }
    col
}

fn render_block(block: &Block, key: &str) -> gpui::Div {
    match block {
        Block::Paragraph(inlines) => paragraph_row(inlines, key),
        Block::Heading(level, inlines) => heading_row(*level, inlines, key),
        Block::BulletList(items) => list_block(items, key, false),
        Block::OrderedList(items) => list_block(items, key, true),
        Block::CodeBlock { body, .. } => code_block(body),
    }
}

fn paragraph_row(inlines: &[Inline], key: &str) -> gpui::Div {
    inline_row(inlines, key)
}

fn heading_row(level: u8, inlines: &[Inline], key: &str) -> gpui::Div {
    let (sz, w) = match level {
        1 => (text_size::XL, weight::BOLD),
        2 => (text_size::LG, weight::SEMIBOLD),
        _ => (text_size::BASE, weight::SEMIBOLD),
    };
    let row = inline_row(inlines, key);
    row.text_size(px(sz))
        .font_weight(FontWeight(w as f32))
        .text_color(rgb(pack(TEXT_PRIMARY)))
}

fn list_block(items: &[Vec<Inline>], key: &str, ordered: bool) -> gpui::Div {
    let mut col = div().flex().flex_col().gap(px(4.0)).pl_4();
    for (i, item) in items.iter().enumerate() {
        let bullet = if ordered {
            format!("{}.", i + 1)
        } else {
            "•".to_owned()
        };
        let item_key = format!("{key}::li{i}");
        col = col.child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .items_start()
                .child(
                    div()
                        .min_w(px(18.0))
                        .font_family(FAMILY_INTER)
                        .text_size(px(text_size::SM))
                        .text_color(rgb(pack(TEXT_SECONDARY)))
                        .child(SharedString::from(bullet)),
                )
                .child(inline_row(item, &item_key)),
        );
    }
    col
}

fn code_block(body: &str) -> gpui::Div {
    div()
        .bg(rgb(pack(SURFACE_750)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .px_3()
        .py_2()
        .font_family("Consolas")
        .text_size(px(text_size::XS))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .child(SharedString::from(body.to_owned()))
}

fn inline_row(inlines: &[Inline], key: &str) -> gpui::Div {
    let mut row = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_baseline()
        .gap(px(0.0))
        .font_family(FAMILY_INTER)
        .text_size(px(text_size::SM))
        .text_color(rgb(pack(TEXT_PRIMARY)));
    for (i, inline) in inlines.iter().enumerate() {
        let inline_key = format!("{key}::i{i}");
        row = row.child(render_inline(inline, &inline_key));
    }
    row
}

fn render_inline(inline: &Inline, key: &str) -> gpui::AnyElement {
    match inline {
        Inline::Text(s) => text_span(s).into_any_element(),
        Inline::Bold(children) => {
            let mut row = inline_row(children, key);
            row = row.font_weight(FontWeight(weight::SEMIBOLD as f32));
            row.into_any_element()
        }
        Inline::Italic(children) => {
            let row = inline_row(children, key);
            // gpui doesn't expose italic on Div; use a colour shift as a
            // visual cue.  TEXT_SECONDARY reads as a subtle emphasis
            // without needing a font-style API.
            row.text_color(rgb(pack(TEXT_SECONDARY))).into_any_element()
        }
        Inline::Code(body) => code_span(body).into_any_element(),
        Inline::Link { text, url } => link_span(text, url, key).into_any_element(),
    }
}

fn text_span(s: &str) -> gpui::Div {
    div().child(SharedString::from(s.to_owned()))
}

fn code_span(s: &str) -> gpui::Div {
    div()
        .bg(rgb(pack(SURFACE_700)))
        .rounded(px(3.0))
        .px(px(4.0))
        .font_family("Consolas")
        .text_size(px(text_size::XS))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .child(SharedString::from(s.to_owned()))
}

fn link_span(text: &str, url: &str, key: &str) -> Stateful<gpui::Div> {
    let target = url.to_owned();
    let id = ElementId::Name(format!("md-link::{key}").into());
    control(div(), id)
        .cursor_pointer()
        .text_color(rgb(pack(BRAND_LIGHT)))
        .on_mouse_down(
            gpui::MouseButton::Left,
            move |_ev: &MouseDownEvent, _window, _cx| {
                // Open in the OS default handler through the pipe seam. In a
                // control walk this records the target and opens nothing (a walk
                // must not spawn a real browser, #247); in the shipped build it is
                // a plain `opener::open`. Best-effort: any open error is swallowed
                // — the chat log is no place to surface it with a modal.
                wylde_gui_pipe::open_url(&target);
            },
        )
        .child(SharedString::from(text.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_parses_to_empty() {
        assert!(parse("").is_empty());
        assert!(parse("\n\n  \n").is_empty());
    }

    #[test]
    fn paragraph_collapses_wrapped_lines() {
        let blocks = parse("hello\nworld");
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Paragraph(ins) => match &ins[0] {
                Inline::Text(s) => assert_eq!(s, "hello world"),
                _ => panic!(),
            },
            _ => panic!(),
        }
    }

    #[test]
    fn blank_line_splits_paragraphs() {
        let blocks = parse("para one\n\npara two");
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn heading_levels_parse() {
        assert!(matches!(&parse("# hi")[0], Block::Heading(1, _)));
        assert!(matches!(&parse("## hi")[0], Block::Heading(2, _)));
        assert!(matches!(&parse("### hi")[0], Block::Heading(3, _)));
        assert!(matches!(&parse("#### hi")[0], Block::Heading(3, _)));
    }

    #[test]
    fn heading_without_space_is_paragraph() {
        // `#foo` is not a heading — needs the space.
        assert!(matches!(&parse("#foo")[0], Block::Paragraph(_)));
    }

    #[test]
    fn bullet_list_aggregates_items() {
        let blocks = parse("- one\n- two\n- three");
        match &blocks[0] {
            Block::BulletList(items) => assert_eq!(items.len(), 3),
            _ => panic!(),
        }
    }

    #[test]
    fn ordered_list_aggregates_items() {
        let blocks = parse("1. one\n2. two");
        match &blocks[0] {
            Block::OrderedList(items) => assert_eq!(items.len(), 2),
            _ => panic!(),
        }
    }

    #[test]
    fn fenced_code_block_captures_body() {
        let blocks = parse("```rust\nfn main() {}\n```");
        match &blocks[0] {
            Block::CodeBlock { lang, body } => {
                assert_eq!(lang, "rust");
                assert_eq!(body, "fn main() {}");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn inline_code_parses() {
        let ins = parse_inlines("use `foo` here");
        assert!(matches!(&ins[1], Inline::Code(s) if s == "foo"));
    }

    #[test]
    fn bold_parses() {
        let ins = parse_inlines("**hi**");
        assert!(matches!(&ins[0], Inline::Bold(_)));
    }

    #[test]
    fn italic_underscore_parses() {
        let ins = parse_inlines("_emph_");
        assert!(matches!(&ins[0], Inline::Italic(_)));
    }

    #[test]
    fn link_parses() {
        let ins = parse_inlines("see [docs](https://example.com)");
        match &ins[1] {
            Inline::Link { text, url } => {
                assert_eq!(text, "docs");
                assert_eq!(url, "https://example.com");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn unclosed_bold_falls_back_to_text() {
        let ins = parse_inlines("**oops");
        assert_eq!(ins.len(), 1);
        match &ins[0] {
            Inline::Text(s) => assert_eq!(s, "**oops"),
            _ => panic!(),
        }
    }

    #[test]
    fn unclosed_code_falls_back_to_text() {
        let ins = parse_inlines("foo `bar baz");
        assert_eq!(ins.len(), 1);
    }

    #[test]
    fn nested_bold_italic_parses() {
        let ins = parse_inlines("**a *b* c**");
        match &ins[0] {
            Inline::Bold(inner) => {
                // inner should contain Text, Italic, Text
                assert!(inner.iter().any(|i| matches!(i, Inline::Italic(_))));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn fenced_code_inside_paragraph_breaks_it() {
        let blocks = parse("text\n```\ncode\n```\nmore");
        assert!(matches!(&blocks[0], Block::Paragraph(_)));
        assert!(matches!(&blocks[1], Block::CodeBlock { .. }));
        assert!(matches!(&blocks[2], Block::Paragraph(_)));
    }

    #[test]
    fn list_followed_by_paragraph() {
        let blocks = parse("- a\n- b\n\nbody");
        assert!(matches!(&blocks[0], Block::BulletList(_)));
        assert!(matches!(&blocks[1], Block::Paragraph(_)));
    }
}

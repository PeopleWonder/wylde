//! System-prompt assembly — turns a gathered [`ChatContext`] into the named
//! slot block the turn driver appends to the base (tool-catalog) system prompt.
//!
//! **Conceptual path:** `Core/Harness/chat/turn/prompt_assembly`.
//!
//! Thought Bubble System Slice G (Phase 2). [`render`] is the single place that
//! knows the *layered context* order and the slot headers; both
//! [`super::context_gather`] (which feeds it the surviving context) and
//! [`super::token_budget`] (which measures the assembled size while evicting)
//! go through it, so there is exactly one definition of "what the model sees".
//!
//! ## Layering (Plan v2 §6)
//!
//! Slots are emitted in a stable order, each only when non-empty, under an
//! `### <Slot>` header so the model can tell them apart:
//!
//! 1. **User profile** — who it's talking to (never-dropped).
//! 2. **Conversation memory** — the short-term working memory (never-dropped).
//! 3. **Conversation summary** — the conversation doc's auto_summary (B2).
//! 4. **Long-term memory** — injected long-term records (B3, evictable).
//! 5. **Vocabulary** — anchors the prompt referenced (never-dropped).
//! 6. **Workspace context** — persona + notes + RAG (`gather_prompt`).
//! 7. **Code graph context** — structural retrieval for referenced symbols.
//!
//! An empty [`ChatContext`] renders to `""`, so a plain chat turn with nothing
//! to add is byte-identical to one with no gather at all.

use crate::turn::context_gather::ChatContext;

/// Render the surviving context into the appended system-prompt block. Returns
/// `""` when every slot is empty.
pub(crate) fn render(ctx: &ChatContext) -> String {
    let mut sections: Vec<String> = Vec::new();

    if !ctx.user_profile.trim().is_empty() {
        sections.push(section("User profile", ctx.user_profile.trim()));
    }

    if !ctx.conversation_short_term.is_empty() {
        sections.push(section(
            "Conversation memory",
            &ctx.conversation_short_term.join("\n"),
        ));
    }

    if let Some(summary) = ctx.conversation_summary.as_deref() {
        if !summary.trim().is_empty() {
            sections.push(section("Conversation summary", summary.trim()));
        }
    }

    if !ctx.long_term.is_empty() {
        sections.push(section("Long-term memory", &ctx.long_term.join("\n")));
    }

    if !ctx.vocabulary_anchors.is_empty() {
        let body = ctx
            .vocabulary_anchors
            .iter()
            .map(|a| format!("- {}", a.text))
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(section("Vocabulary", &body));
    }

    // The workspace parts render under ONE "### Workspace context" header
    // with the same subsection names the service's render used, but are
    // assembled here from the B6-split fields so each part evicts on its
    // own tier.
    let has_workspace = ctx.workspace_persona.is_some()
        || !ctx.workspace_notes.is_empty()
        || !ctx.workspace_rag.is_empty();
    if has_workspace {
        let mut body = String::new();
        if let Some(p) = ctx.workspace_persona.as_deref() {
            let p = p.trim();
            if !p.is_empty() {
                body.push_str("## Persona\n");
                body.push_str(p);
            }
        }
        let notes: Vec<&str> = ctx
            .workspace_notes
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if !notes.is_empty() {
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str("## Workspace memory");
            for n in notes {
                body.push_str("\n- ");
                body.push_str(n);
            }
        }
        let rag: Vec<&str> = ctx
            .workspace_rag
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if !rag.is_empty() {
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str("## Workspace files");
            for r in rag {
                body.push_str("\n\n");
                body.push_str(r);
            }
        }
        if !body.is_empty() {
            sections.push(section("Workspace context", &body));
        }
    }

    if !ctx.symbol_contexts.is_empty() {
        let body = ctx
            .symbol_contexts
            .iter()
            .map(render_symbol_block)
            .collect::<Vec<_>>()
            .join("\n\n");
        if !body.trim().is_empty() {
            sections.push(section("Code graph context", &body));
        }
    }

    if sections.is_empty() {
        String::new()
    } else {
        // A leading blank line separates the slots from whatever the base
        // system prompt ended with (the tool catalog).
        format!("\n\n{}", sections.join("\n\n"))
    }
}

/// One `### Header` block.
fn section(header: &str, body: &str) -> String {
    format!("### {header}\n{body}")
}

/// Render a symbol context block: the focal header + body, then its surviving
/// neighbour lines (the token budget may have shed deeper hops).
fn render_symbol_block(block: &super::context_gather::SymbolContextBlock) -> String {
    let mut out = block.focal.clone();
    for n in &block.neighbors {
        out.push('\n');
        out.push_str(&n.text);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::context_gather::{AnchorBlock, NeighborLine, SymbolContextBlock};

    #[test]
    fn empty_context_renders_empty() {
        assert_eq!(render(&ChatContext::default()), "");
    }

    #[test]
    fn profile_only_renders_one_section() {
        let ctx = ChatContext {
            user_profile: "Name: Aaron".into(),
            ..ChatContext::default()
        };
        let out = render(&ctx);
        assert!(out.contains("### User profile"));
        assert!(out.contains("Name: Aaron"));
        assert!(!out.contains("### Code graph context"));
        assert!(out.starts_with("\n\n"));
    }

    #[test]
    fn slots_appear_in_layered_order() {
        let ctx = ChatContext {
            user_profile: "Name: Aaron".into(),
            conversation_short_term: vec!["- recalled a thing".into()],
            conversation_summary: Some("They were debugging.".into()),
            long_term: vec!["- prefers tabs".into()],
            vocabulary_anchors: vec![AnchorBlock {
                identifier: "the_fn".into(),
                text: "{{the_fn}} — the entry point".into(),
            }],
            workspace_persona: Some("Be precise.".into()),
            workspace_notes: vec!["uses cargo nextest".into()],
            workspace_rag: vec!["fn main() {}".into()],
            symbol_contexts: vec![SymbolContextBlock {
                symbol_id: "foo".into(),
                focal: "Symbol `foo` — src/foo.rs:10\nfn foo() {}".into(),
                neighbors: vec![NeighborLine {
                    hop: 1,
                    text: "  calls `bar` (src/bar.rs)".into(),
                }],
            }],
            ..ChatContext::default()
        };
        let out = render(&ctx);
        let profile = out.find("### User profile").unwrap();
        let memory = out.find("### Conversation memory").unwrap();
        let summary = out.find("### Conversation summary").unwrap();
        let long_term = out.find("### Long-term memory").unwrap();
        let vocab = out.find("### Vocabulary").unwrap();
        let ws = out.find("### Workspace context").unwrap();
        let graph = out.find("### Code graph context").unwrap();
        assert!(
            profile < memory
                && memory < summary
                && summary < long_term
                && long_term < vocab
                && vocab < ws
                && ws < graph
        );
        // Symbol block renders focal + neighbour.
        assert!(out.contains("Symbol `foo`"));
        assert!(out.contains("calls `bar`"));
        // The B6-split workspace parts render under one section with the
        // established subsection names.
        assert!(out.contains("## Persona\nBe precise."));
        assert!(out.contains("## Workspace memory\n- uses cargo nextest"));
        assert!(out.contains("## Workspace files\n\nfn main() {}"));
    }

    #[test]
    fn workspace_section_renders_only_present_parts() {
        // Notes only — no Persona / files subsections.
        let ctx = ChatContext {
            workspace_notes: vec!["only memory".into()],
            ..ChatContext::default()
        };
        let out = render(&ctx);
        assert!(out.contains("### Workspace context"));
        assert!(out.contains("- only memory"));
        assert!(!out.contains("## Persona"));
        assert!(!out.contains("## Workspace files"));

        // Whitespace-only parts render nothing at all.
        let ctx = ChatContext {
            workspace_persona: Some("   ".into()),
            workspace_rag: vec!["  ".into()],
            ..ChatContext::default()
        };
        assert_eq!(render(&ctx), "");
    }
}

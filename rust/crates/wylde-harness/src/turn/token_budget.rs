// HOW TO MODIFY — token-budget eviction (OI-8 / Plan v2 §9.1)
// ============================================================
//
// When the gathered chat context would exceed the model's token budget, the
// assembled prompt is trimmed by dropping the LOWEST-priority material first.
// This module owns that priority ladder. (A user-facing surface — Settings →
// Token Budget with an "Open documentation" link — comes in a later slice;
// this doc-comment is the source of truth until then.)
//
// THE PRIORITY LADDER (tier 1 = lowest = dropped first … tier 7 = NEVER dropped):
//
//   1. (lowest) Generic auto-context / vector-RAG fallback chunks
//   2. Older standalone-conversation summaries
//   3. Older workspace_notes
//   4. Deeper-hop `symbol_context` results        ← drop the deepest hop first
//   5. Older bubbles (unpinned before pinned)
//   6. Older anchors (least-recently-used)
//   7. (highest, NEVER dropped) user_profile · current-conversation short-term ·
//      active pinned bubbles · vocabulary block for currently-referenced anchors
//
// HOW THIS MAPS ONTO THE Phase-2 `ChatContext`:
//   * tier 1  → `workspace_rag`                   (B6 split — lowest-ranked
//                                                   snippet sheds first)
//   * tier 2  → `conversation_summary`            (the conversation doc's
//                                                   auto_summary — B2)
//   * tier 2.5 → `long_term`                       (injected long-term memory
//                                                   — B3; least-relevant line
//                                                   sheds first)
//   * tier 2.7 → `workspace_memory`                (workspace memory records
//                                                   — memory plan M2 option B;
//                                                   importance+supersession
//                                                   tier; least-relevant line
//                                                   sheds first)
//   * tier 3  → `workspace_notes`                 (B6 split — lowest-ranked
//                                                   snippet sheds first)
//   * tier ~6 → `workspace_persona`               (B6 split — the workspace's
//                                                   voice; outlasts every
//                                                   retrieved slot)
//   * tier 6.5 → `history`                          (windowed prior turns —
//                                                   B1; rides the messages
//                                                   array, counted here, drops
//                                                   oldest-pair-first and only
//                                                   after every other
//                                                   evictable tier)
//   * tier 4  → `symbol_contexts`                 (shed the highest-hop_distance
//                                                   neighbour lines first; only
//                                                   once a block has no neighbours
//                                                   left is the whole focal block
//                                                   dropped)
//   * tier 7  → `user_profile`, `conversation_short_term`, `vocabulary_anchors`
//   Tiers 5 and 6 (pinned/unpinned bubbles, a broad older-anchors pool) still
//   have no source — every anchor gathered this turn is a *currently-
//   referenced* one, i.e. tier 7. When those land, give them a field on
//   `ChatContext` and a branch in `drop_one_lowest_priority` at the right
//   rung — the order above is the contract.
//
// HOW TO ADJUST THE BUDGET:
//   * The ceiling comes from `chat_options::slot_budget()` (improvement plan
//     B5): the model's effective num_ctx (user override → declared Modelfile
//     default → Ollama server default) minus the base prompt, the user
//     message, and a response reserve. `WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET`
//     (token count) bypasses the derivation and IS the slot ceiling when set;
//     `DEFAULT_TOKEN_BUDGET` is the last-resort fallback when the model's
//     window is unknowable (no override, service unreachable).
//   * Token counts are *estimated* (`estimate_tokens`, ~4 chars/token). This is
//     deliberately cheap — the gather runs on the chat hot path. If you need a
//     true tokenizer count, swap `estimate_tokens` only; the ladder is
//     independent of how tokens are counted.
//
// HOW TO CHANGE THE ORDER:
//   Edit `drop_one_lowest_priority` — it tries each tier bottom-up and returns
//   after removing exactly ONE unit, so `evict` re-measures between drops and
//   stops the moment it's under budget. Keep tier 7 unreachable (never drop it)
//   — the user profile et al. are load-bearing and the whole feature assumes
//   they're always present.
//
// NOTE — brief vs spec (the reconciliation the last six slices also made; the
// authoritative Plan v2 §7/§9.1 + Build Order Appendix A win): the slice brief
// ordered `workspace_notes` ABOVE `symbol_context` (notes dropped later). The
// spec orders them the other way — workspace_notes is tier 3, symbol_context is
// tier 4, so **notes evict before symbol context**. We follow the spec.

//! Token-budget eviction for the gathered chat context (Slice G).
//!
//! See the `HOW TO MODIFY` block at the top of this file for the full priority
//! ladder and how to tune it.

use crate::turn::context_gather::ChatContext;
use crate::turn::prompt_assembly;

/// Last-resort context-token ceiling, used only when the model's effective
/// `num_ctx` is unknowable (no user override AND the Ollama service was
/// unreachable — see [`super::chat_options::slot_budget`]). Sized for a
/// large model.
pub(crate) const DEFAULT_TOKEN_BUDGET: usize = 100_000;

/// Estimate the token count of `s` — per-content-class chars/token ratios
/// (improvement plan B7). BPE tokenizers spend roughly 3 chars/token on
/// code (dense punctuation, snake_case splits) vs ~4 on English prose; the
/// old flat ÷4 systematically underestimated code-heavy slots (symbol
/// bodies, RAG chunks) by 25-30% — real overshoot exactly when the prompt
/// was fullest. Still deliberately cheap (eviction re-renders per drop):
/// one sampled scan, no tokenizer dependency. Swapping in a true BPE count
/// later still only means replacing this one function.
pub(crate) fn estimate_tokens(s: &str) -> usize {
    s.chars().count().div_ceil(chars_per_token(s))
}

/// How many chars of `s` to sample when classifying its content class.
const CLASSIFY_SAMPLE_CHARS: usize = 2_000;

/// Punctuation density at/above this percentage classifies as code.
/// English prose runs 0-2% of [`CODE_SIGNAL_CHARS`]; real code runs
/// 8-15%+.
const CODE_SIGNAL_PERCENT: usize = 5;

/// Characters that signal code-like content.
const CODE_SIGNAL_CHARS: &[char] = &[
    '{', '}', '(', ')', ';', '_', '=', '<', '>', '[', ']', '#', '/', '\\',
];

/// 3 chars/token for code-like text, 4 for prose. Sampled, not exact —
/// an estimator, not a tokenizer.
fn chars_per_token(s: &str) -> usize {
    let mut total = 0usize;
    let mut signals = 0usize;
    for c in s.chars().take(CLASSIFY_SAMPLE_CHARS) {
        total += 1;
        if CODE_SIGNAL_CHARS.contains(&c) {
            signals += 1;
        }
    }
    if total == 0 {
        return 4;
    }
    if signals * 100 / total >= CODE_SIGNAL_PERCENT {
        3
    } else {
        4
    }
}

/// Per-message token overhead for a history message (role + chat-template
/// framing) on top of its content estimate (B1).
pub(crate) const HISTORY_MSG_OVERHEAD: usize = 4;

/// Estimated token cost of the windowed history — it rides the `messages`
/// array rather than the rendered block, but competes for the same context
/// window, so [`evict`] counts it.
pub(crate) fn history_tokens(ctx: &ChatContext) -> usize {
    ctx.history
        .iter()
        .map(|m| estimate_tokens(&m.content) + HISTORY_MSG_OVERHEAD)
        .sum()
}

/// Evict lowest-priority context (OI-8) until the assembled prompt fits
/// `max_tokens`, or until only never-drop (tier 7) material remains.
///
/// Measures against the *rendered* output ([`prompt_assembly::render`]) so the
/// budget reflects exactly what the model receives, not an internal sum.
pub(crate) fn evict(ctx: &mut ChatContext, max_tokens: usize) {
    loop {
        let rendered = prompt_assembly::render(ctx);
        if estimate_tokens(&rendered) + history_tokens(ctx) <= max_tokens {
            return;
        }
        if !drop_one_lowest_priority(ctx) {
            // Nothing droppable left — the never-drop tier alone is over budget.
            // Better to overshoot on the load-bearing slots than discard them.
            return;
        }
    }
}

/// Remove exactly one unit of the lowest-priority surviving context. Returns
/// `true` if something was dropped, `false` when only never-drop material is
/// left. Tiers are tried bottom-up (lowest priority first).
fn drop_one_lowest_priority(ctx: &mut ChatContext) -> bool {
    // tier 1 — workspace RAG snippets (B6 split): the generic retrieval
    // fallback, lowest-ranked snippet first.
    if ctx.workspace_rag.pop().is_some() {
        return true;
    }

    // tier 2 — conversation summary.
    if ctx.conversation_summary.take().is_some() {
        return true;
    }

    // tier ~2.5 — long-term memory (B3): shed the least-relevant line
    // first (the list arrives best-first). Deliberately evictable —
    // long-term recall is useful grounding, not load-bearing like the
    // tier-7 slots.
    if ctx.long_term.pop().is_some() {
        return true;
    }

    // tier ~2.7 — workspace memory records (M2 option B): least-relevant
    // line first (the list arrives best-first). Sits between the global
    // long-term tier and the user-curated notes: machine-distilled
    // insights outlast generic recall but yield to what the user wrote
    // deliberately.
    if ctx.workspace_memory.pop().is_some() {
        return true;
    }

    // tier 3 — workspace notes (B6 split): lowest-ranked snippet first.
    if ctx.workspace_notes.pop().is_some() {
        return true;
    }

    // tier 4 — symbol contexts: shed the deepest-hop neighbour line first;
    // only once a block has no neighbours do we drop the whole focal block.
    if drop_deepest_symbol_hop(ctx) {
        return true;
    }
    if !ctx.symbol_contexts.is_empty() {
        // All remaining blocks are bare focals — drop the last-gathered one
        // (lowest-ranked; the prompt's first/strongest references came first).
        ctx.symbol_contexts.pop();
        return true;
    }

    // tiers 5/6 — bubbles / older anchors: no Phase-2 field.

    // tier ~6 — the workspace persona (B6 split): the workspace's voice,
    // jarring to lose mid-conversation — outlasts every retrieved slot.
    if ctx.workspace_persona.take().is_some() {
        return true;
    }

    // tier ~6.5 — conversation history (B1): the most protected evictable
    // slot — retrieved enrichment above is re-derivable, dialogue isn't.
    // Drop the oldest PAIR (one exchange) so the window degrades a turn
    // at a time; the current exchange is never in `history` at all.
    if !ctx.history.is_empty() {
        let n = 2.min(ctx.history.len());
        ctx.history.drain(0..n);
        return true;
    }

    // tier 7 — user_profile, conversation_short_term, vocabulary_anchors: never.
    false
}

/// Drop the single neighbour line with the largest `hop` across all symbol
/// contexts (ties broken by the later block / later line). Returns `false` when
/// no block has any neighbour line left.
fn drop_deepest_symbol_hop(ctx: &mut ChatContext) -> bool {
    let mut best: Option<(usize, usize, u32)> = None; // (block_idx, line_idx, hop)
    for (bi, block) in ctx.symbol_contexts.iter().enumerate() {
        for (li, n) in block.neighbors.iter().enumerate() {
            match best {
                Some((_, _, h)) if n.hop <= h => {}
                _ => best = Some((bi, li, n.hop)),
            }
        }
    }
    if let Some((bi, li, _)) = best {
        ctx.symbol_contexts[bi].neighbors.remove(li);
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::context_gather::{AnchorBlock, NeighborLine, SymbolContextBlock};

    fn block(id: &str, body: &str, hops: &[u32]) -> SymbolContextBlock {
        SymbolContextBlock {
            symbol_id: id.into(),
            focal: format!("Symbol `{id}`\n{body}"),
            neighbors: hops
                .iter()
                .map(|h| NeighborLine {
                    hop: *h,
                    text: format!("  calls `n{h}` at hop {h}"),
                })
                .collect(),
        }
    }

    #[test]
    fn under_budget_keeps_everything() {
        let mut ctx = ChatContext {
            user_profile: "Name: Aaron".into(),
            symbol_contexts: vec![block("foo", "fn foo() {}", &[1, 2])],
            ..ChatContext::default()
        };
        let before = ctx.clone();
        evict(&mut ctx, 100_000);
        assert_eq!(ctx, before);
    }

    #[test]
    fn estimate_tokens_classifies_prose_vs_code() {
        // Prose: ~4 chars/token.
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        let prose = "The quick brown fox jumps over the lazy dog and keeps going.";
        assert_eq!(estimate_tokens(prose), prose.chars().count().div_ceil(4));

        // Code: dense punctuation → ~3 chars/token (B7).
        let code = "fn foo(x: usize) -> usize {\n    let y = x * 2;\n    bar(y)\n}";
        assert_eq!(estimate_tokens(code), code.chars().count().div_ceil(3));
        assert!(
            estimate_tokens(code) > code.chars().count().div_ceil(4),
            "code estimates higher than the old flat divisor"
        );
    }

    #[test]
    fn deeper_hops_drop_before_user_profile() {
        // A big profile we must never drop, plus a symbol context with a deep
        // (hop-2) neighbour and a shallow (hop-1) one.
        let mut ctx = ChatContext {
            user_profile: "Name: Aaron\nStyle: terse".into(),
            symbol_contexts: vec![block("foo", "fn foo() {}", &[1, 2])],
            ..ChatContext::default()
        };
        // Budget that forces shedding the deep neighbour but little else.
        let full = estimate_tokens(&prompt_assembly::render(&ctx));
        evict(&mut ctx, full - 1);

        // The deepest (hop-2) neighbour went first; the hop-1 one may survive.
        let neighbors = &ctx.symbol_contexts[0].neighbors;
        assert!(
            neighbors.iter().all(|n| n.hop != 2),
            "hop-2 neighbour must be evicted first"
        );
        // The user profile is always retained.
        assert!(ctx.user_profile.contains("Aaron"));
    }

    #[test]
    fn eviction_ladder_full_order_b6() {
        let mut ctx = ChatContext {
            user_profile: "P".into(),
            conversation_summary: Some("a summary".into()),
            long_term: vec!["- best fact".into()],
            workspace_memory: vec!["- distilled insight".into()],
            workspace_rag: vec!["fn a() {}".into(), "fn b() {}".into()],
            workspace_notes: vec!["uses pytest".into()],
            workspace_persona: Some("Be precise.".into()),
            symbol_contexts: vec![block("foo", "fn foo() {}", &[1])],
            ..ChatContext::default()
        };

        // tier 1: RAG snippets, lowest-ranked (last) first.
        drop_one_lowest_priority(&mut ctx);
        assert_eq!(ctx.workspace_rag, vec!["fn a() {}".to_owned()]);
        drop_one_lowest_priority(&mut ctx);
        assert!(ctx.workspace_rag.is_empty());
        assert!(ctx.conversation_summary.is_some());

        // tier 2: the summary.
        drop_one_lowest_priority(&mut ctx);
        assert!(ctx.conversation_summary.is_none());

        // tier 2.5: long-term.
        drop_one_lowest_priority(&mut ctx);
        assert!(ctx.long_term.is_empty());
        assert!(!ctx.workspace_memory.is_empty());

        // tier ~2.7: workspace memory records (M2 option B).
        drop_one_lowest_priority(&mut ctx);
        assert!(ctx.workspace_memory.is_empty());
        assert!(!ctx.workspace_notes.is_empty());

        // tier 3: notes.
        drop_one_lowest_priority(&mut ctx);
        assert!(ctx.workspace_notes.is_empty());
        assert!(!ctx.symbol_contexts.is_empty());

        // tier 4: the symbol's neighbour line, then the bare focal.
        drop_one_lowest_priority(&mut ctx);
        assert!(ctx.symbol_contexts[0].neighbors.is_empty());
        assert!(ctx.workspace_persona.is_some(), "persona outlasts symbols");
        drop_one_lowest_priority(&mut ctx);
        assert!(ctx.symbol_contexts.is_empty());

        // tier ~6: the persona — last evictable before history.
        drop_one_lowest_priority(&mut ctx);
        assert!(ctx.workspace_persona.is_none());

        // tier 7: nothing left to drop.
        assert!(!drop_one_lowest_priority(&mut ctx));
        assert_eq!(ctx.user_profile, "P");
    }

    #[test]
    fn history_drops_oldest_pair_only_after_every_other_evictable() {
        use crate::turn::context_gather::HistoryMessage;
        let msg = |role: &str, content: &str| HistoryMessage {
            role: role.into(),
            content: content.into(),
        };
        let mut ctx = ChatContext {
            user_profile: "P".into(),
            workspace_persona: Some("Be precise.".into()),
            history: vec![
                msg("user", "old question"),
                msg("assistant", "old answer"),
                msg("user", "recent question"),
                msg("assistant", "recent answer"),
            ],
            ..ChatContext::default()
        };

        // The persona (tier ~6) goes before any history.
        drop_one_lowest_priority(&mut ctx);
        assert!(ctx.workspace_persona.is_none());
        assert_eq!(ctx.history.len(), 4);

        // Then history sheds the OLDEST exchange as a pair.
        drop_one_lowest_priority(&mut ctx);
        assert_eq!(ctx.history.len(), 2);
        assert_eq!(ctx.history[0].content, "recent question");

        drop_one_lowest_priority(&mut ctx);
        assert!(ctx.history.is_empty());

        // Tier 7 still never drops.
        assert!(!drop_one_lowest_priority(&mut ctx));
    }

    #[test]
    fn evict_counts_history_toward_the_budget() {
        use crate::turn::context_gather::HistoryMessage;
        // No rendered slots at all — ONLY history. An unbudgeted history
        // would never trigger eviction; B1 requires it to.
        let mut ctx = ChatContext {
            history: (0..10)
                .map(|i| HistoryMessage {
                    role: "user".into(),
                    content: format!("message {i} {}", "x".repeat(400)),
                })
                .collect(),
            ..ChatContext::default()
        };
        evict(&mut ctx, 220);
        assert!(
            ctx.history.len() < 10,
            "history must shed when it alone exceeds the budget"
        );
        // The newest messages are the survivors.
        assert_eq!(ctx.history.last().unwrap().content[..9], *"message 9");
    }

    #[test]
    fn never_drops_profile_short_term_or_vocabulary_even_when_over_budget() {
        let mut ctx = ChatContext {
            user_profile: "a very long profile ".repeat(50),
            conversation_short_term: vec!["- working memory line".into()],
            vocabulary_anchors: vec![AnchorBlock {
                identifier: "x".into(),
                text: "{{x}} — def".into(),
            }],
            ..ChatContext::default()
        };
        // Absurdly small budget: nothing droppable, so the never-drop tier
        // survives intact (overshoot rather than discard).
        evict(&mut ctx, 1);
        assert!(!ctx.user_profile.is_empty());
        assert_eq!(ctx.conversation_short_term.len(), 1);
        assert_eq!(ctx.vocabulary_anchors.len(), 1);
    }
}

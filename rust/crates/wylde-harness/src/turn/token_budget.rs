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
//   * tier ~1.5 → `concept_context`                (concept-routing R2 Augment —
//                                                   boundary blurb + member
//                                                   snippets; above generic RAG,
//                                                   below all else; blurb-first
//                                                   layout means snippets shed
//                                                   before the boundary)
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
// THE TIER-7 DEGRADE PASS (memory plan M3):
//   When everything droppable is gone and the render STILL exceeds the
//   budget (a real 4096-window model under working-memory pressure), the
//   pass in `degrade_tier7` shrinks never-drop CONTENT instead of shipping
//   an over-window prompt the server would front-truncate: the
//   working-memory window sheds oldest-first to a 5-entry floor, the
//   profile's free-text-rules render steps 4k → 1k → dropped, vocabulary
//   anchors go last. Name/style/preference profile lines + the newest 5
//   WM entries are the hard floor. Each shrink leaves a visible marker.
//   Off via WYLDE_TIER7_DEGRADE=off, and always off when
//   WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET pins the budget explicitly.
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
/// `max_tokens`, or until only never-drop (tier 7) material remains —
/// then, when even that overshoots, run the **M3 tier-7 degrade pass**
/// rather than shipping an over-window prompt a small model's server
/// would front-truncate (killing the base instruction + tool catalog,
/// the most load-bearing bytes — evaluation §3.3).
///
/// Returns `true` when the degrade pass shrank tier-7 content; the
/// shrunk slots carry their own visible markers.
///
/// Measures against the *rendered* output ([`prompt_assembly::render`]) so the
/// budget reflects exactly what the model receives, not an internal sum.
pub(crate) fn evict(ctx: &mut ChatContext, max_tokens: usize) -> bool {
    loop {
        let rendered = prompt_assembly::render(ctx);
        if estimate_tokens(&rendered) + history_tokens(ctx) <= max_tokens {
            return false;
        }
        if !drop_one_lowest_priority(ctx) {
            // Nothing droppable left — the never-drop tier alone is over
            // budget. Pre-M3 this returned (deliberate overshoot); now the
            // degrade pass shrinks the tier-7 *content* instead.
            break;
        }
    }
    if !tier7_degrade_enabled() {
        return false;
    }
    degrade_tier7(ctx, max_tokens)
}

/// Whether the M3 degrade pass runs. Off when:
/// * `WYLDE_TIER7_DEGRADE=off|0|false` (the slice kill switch), or
/// * `WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET` is set — the explicit
///   deployment knob keeps meaning "I know what I'm doing": an operator
///   who pinned the budget gets the historical deliberate overshoot.
fn tier7_degrade_enabled() -> bool {
    if std::env::var("WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
    {
        return false;
    }
    match std::env::var("WYLDE_TIER7_DEGRADE") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false"
        ),
        Err(_) => true,
    }
}

/// Shrink never-drop content until the render fits `max_tokens` LESS a
/// ~10% pessimism margin (the B7 estimator's error band runs ±25-30% on
/// pathological content; 10% under-target keeps the fit invariant honest
/// for the common case without gutting the floor), or until the hard
/// floor is reached. Degrade order:
///
/// 1. working-memory window — oldest entries first, down to the newest
///    [`crate::turn::context_gather::WORKING_MEMORY_DEGRADE_FLOOR`];
/// 2. profile free-text rules render — 4k chars → 1k → dropped
///    (name/style/preference lines are the hard floor, never touched);
/// 3. vocabulary anchors — smallest and highest-signal, degrade last.
fn degrade_tier7(ctx: &mut ChatContext, max_tokens: usize) -> bool {
    let target = max_tokens.saturating_sub(max_tokens / 10);
    let mut changed = false;
    loop {
        let rendered = prompt_assembly::render(ctx);
        if estimate_tokens(&rendered) + history_tokens(ctx) <= target {
            break;
        }
        if !degrade_one_tier7(ctx) {
            // Hard floor reached — overshoot the remainder (the floor is
            // sized to fit any real window; see WORKING_MEMORY_DEGRADE_FLOOR).
            break;
        }
        changed = true;
    }
    if changed {
        tracing::warn!(
            "token_budget: tier-7 degrade pass shrank never-drop content to fit \
             a {max_tokens}-token slot budget (small context window)"
        );
    }
    changed
}

/// One unit of tier-7 degradation, in the documented order. Returns
/// `false` at the hard floor.
fn degrade_one_tier7(ctx: &mut ChatContext) -> bool {
    if crate::turn::context_gather::degrade_short_term_once(&mut ctx.conversation_short_term) {
        return true;
    }
    if crate::user_profile::profile::degrade_rules_once(&mut ctx.user_profile) {
        return true;
    }
    if ctx.vocabulary_anchors.pop().is_some() {
        return true;
    }
    false
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

    // tier ~1.5 — concept-routing R2 Augment context (plan §3, §6.3): sits
    // *above* generic RAG (coherent concept context outlives scattered chunks,
    // thesis §3.3) but below every other evictable tier. The slot is laid out
    // blurb-first then member snippets, so popping the tail sheds the lowest
    // snippet first and the (cheap, high-signal) boundary blurb survives
    // longest — it's the last concept_context element to go.
    if ctx.concept_context.pop().is_some() {
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
    fn concept_context_evicts_after_rag_but_before_summary() {
        // The R2 concept slot sits at tier ~1.5: generic RAG sheds first, then
        // concept context (snippets before the boundary blurb), then the summary.
        let mut ctx = ChatContext {
            user_profile: "P".into(),
            workspace_rag: vec!["fn a() {}".into()],
            concept_context: vec!["BLURB: boundary".into(), "snippet body".into()],
            conversation_summary: Some("a summary".into()),
            ..ChatContext::default()
        };

        // tier 1: the RAG snippet goes first.
        drop_one_lowest_priority(&mut ctx);
        assert!(ctx.workspace_rag.is_empty());
        assert_eq!(
            ctx.concept_context.len(),
            2,
            "concept context untouched yet"
        );

        // tier ~1.5: concept context sheds the tail (the member snippet) first.
        drop_one_lowest_priority(&mut ctx);
        assert_eq!(ctx.concept_context, vec!["BLURB: boundary".to_owned()]);
        assert!(
            ctx.conversation_summary.is_some(),
            "summary outlasts concepts"
        );

        // …then the boundary blurb (last concept_context element).
        drop_one_lowest_priority(&mut ctx);
        assert!(ctx.concept_context.is_empty());
        assert!(ctx.conversation_summary.is_some());

        // tier 2: only now the summary.
        drop_one_lowest_priority(&mut ctx);
        assert!(ctx.conversation_summary.is_none());
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
    fn hard_floor_survives_even_when_over_budget() {
        // The M3 floor: profile lines WITHOUT a rules section, ≤5 WM
        // entries, and no anchors left to shed — nothing degradable.
        let mut ctx = ChatContext {
            user_profile: "Name: Aaron\nStyle: terse".into(),
            conversation_short_term: vec!["- working memory line".into()],
            vocabulary_anchors: vec![AnchorBlock {
                identifier: "x".into(),
                text: "{{x}} — def".into(),
            }],
            ..ChatContext::default()
        };
        // Absurdly small budget: the degrade pass sheds the anchor (the
        // last degradable unit) and then overshoots rather than discard
        // the floor.
        let degraded = evict(&mut ctx, 1);
        assert!(degraded, "the anchor shed counts as degradation");
        assert_eq!(ctx.user_profile, "Name: Aaron\nStyle: terse");
        assert_eq!(ctx.conversation_short_term.len(), 1);
        assert!(ctx.vocabulary_anchors.is_empty(), "anchors degrade last");
    }

    // ── the M3 tier-7 degrade pass ───────────────────────────────────

    /// A guard that pins the degrade-pass env knobs for one test.
    struct DegradeEnvGuard {
        _g: std::sync::MutexGuard<'static, ()>,
        prior_budget: Option<std::ffi::OsString>,
        prior_switch: Option<std::ffi::OsString>,
    }

    impl DegradeEnvGuard {
        fn new() -> Self {
            let g = crate::memory::common::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let prior_budget = std::env::var_os("WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET");
            let prior_switch = std::env::var_os("WYLDE_TIER7_DEGRADE");
            std::env::remove_var("WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET");
            std::env::remove_var("WYLDE_TIER7_DEGRADE");
            Self {
                _g: g,
                prior_budget,
                prior_switch,
            }
        }
    }

    impl Drop for DegradeEnvGuard {
        fn drop(&mut self) {
            match self.prior_budget.take() {
                Some(v) => std::env::set_var("WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET", v),
                None => std::env::remove_var("WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET"),
            }
            match self.prior_switch.take() {
                Some(v) => std::env::set_var("WYLDE_TIER7_DEGRADE", v),
                None => std::env::remove_var("WYLDE_TIER7_DEGRADE"),
            }
        }
    }

    /// A never-drop-heavy context: 20 WM lines + a profile with a big
    /// rules section + one anchor. ~No droppable tiers at all.
    fn tier7_heavy_ctx() -> ChatContext {
        ChatContext {
            user_profile: format!(
                "Name: Aaron\nStyle: terse\nUser rules (follow verbatim):\n{}",
                "always run the linter before committing anything anywhere. ".repeat(100)
            ),
            conversation_short_term: (0..20)
                .map(|i| format!("- working memory entry {i} with some real content"))
                .collect(),
            vocabulary_anchors: vec![AnchorBlock {
                identifier: "x".into(),
                text: "{{x}} — a vocabulary definition".into(),
            }],
            ..ChatContext::default()
        }
    }

    #[test]
    fn degrade_pass_fits_the_render_to_a_small_window() {
        let _env = DegradeEnvGuard::new();
        let mut ctx = tier7_heavy_ctx();
        let full = estimate_tokens(&prompt_assembly::render(&ctx));
        assert!(full > 600, "fixture must overshoot the budget under test");

        let degraded = evict(&mut ctx, 600);
        assert!(degraded);
        let after = estimate_tokens(&prompt_assembly::render(&ctx)) + history_tokens(&ctx);
        // The M3 fit invariant: rendered slots fit the budget (the pass
        // targets budget − 10% for estimator slack).
        assert!(
            after <= 600,
            "degraded render must fit the budget: {after} > 600"
        );
        // The shrunk slots carry visible markers.
        let rendered = prompt_assembly::render(&ctx);
        assert!(rendered.contains("older working-memory entries omitted"));
        // The hard floor held: name/style + the newest 5 WM entries.
        assert!(rendered.contains("Name: Aaron"));
        assert!(rendered.contains("- working memory entry 19"));
        assert!(rendered.contains("- working memory entry 15"));
    }

    #[test]
    fn degrade_sheds_wm_before_rules_before_anchors() {
        let _env = DegradeEnvGuard::new();
        let mut ctx = tier7_heavy_ctx();
        // First degrade unit: the oldest WM entry, marker prepended.
        assert!(super::degrade_one_tier7(&mut ctx));
        assert!(ctx.conversation_short_term[0].contains("1 older working-memory"));
        assert!(!ctx
            .conversation_short_term
            .iter()
            .any(|l| l.contains("entry 0 ")));
        assert!(ctx.user_profile.contains("User rules"), "rules untouched");

        // Exhaust WM down to the floor; rules degrade next.
        while crate::turn::context_gather::degrade_short_term_once(&mut ctx.conversation_short_term)
        {
        }
        assert_eq!(ctx.conversation_short_term.len(), 5 + 1, "floor + marker");
        assert!(super::degrade_one_tier7(&mut ctx));
        assert!(
            ctx.user_profile.contains("truncated to fit"),
            "rules step down with a marker: {}",
            ctx.user_profile
        );
        // Two more steps: 1k, then dropped entirely.
        assert!(super::degrade_one_tier7(&mut ctx));
        assert!(super::degrade_one_tier7(&mut ctx));
        assert!(
            !ctx.user_profile.contains("User rules"),
            "rules section dropped at the last step: {}",
            ctx.user_profile
        );
        assert!(ctx.user_profile.contains("Name: Aaron"), "hard floor");

        // Anchors are the last degradable unit.
        assert!(super::degrade_one_tier7(&mut ctx));
        assert!(ctx.vocabulary_anchors.is_empty());
        assert!(!super::degrade_one_tier7(&mut ctx), "hard floor reached");
    }

    #[test]
    fn explicit_budget_knob_disables_the_degrade_pass() {
        let _env = DegradeEnvGuard::new();
        std::env::set_var("WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET", "16000");
        let mut ctx = tier7_heavy_ctx();
        let before = ctx.clone();
        let degraded = evict(&mut ctx, 100);
        assert!(!degraded);
        assert_eq!(ctx, before, "explicit knob keeps the historical overshoot");
        std::env::remove_var("WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET");
    }

    #[test]
    fn kill_switch_disables_the_degrade_pass() {
        let _env = DegradeEnvGuard::new();
        std::env::set_var("WYLDE_TIER7_DEGRADE", "off");
        let mut ctx = tier7_heavy_ctx();
        let before = ctx.clone();
        let degraded = evict(&mut ctx, 100);
        assert!(!degraded);
        assert_eq!(ctx, before);
        std::env::remove_var("WYLDE_TIER7_DEGRADE");
    }
}

//! Prompt eval/regression harness (improvement plan **B11**).
//!
//! Golden snapshot tests over the **rendered prompt** — the repo's
//! parity-test culture (turn scripts gated the 5.D flip) applied to the
//! prompt itself. Every P0/P1 prompt change (history, catalog
//! migration, ordering) lands against this net: if the bytes the model
//! sees change, a golden drifts and the diff is reviewable.
//!
//! ## What is pinned
//!
//! * The **base system prompt** (instruction + guidance + memory rule +
//!   tool catalog) for a fixed fixture catalog, in both verb and legacy
//!   mode.
//! * The **gathered slot block** (`prompt_assembly::render`) for a full
//!   fixture context: slot order, `### ` headers, exact bodies.
//! * **Eviction behavior at fixed budgets**: which tiers survive a
//!   mid-pressure budget and the never-drop floor.
//! * An **end-to-end gather** through `gather_with` with a mocked
//!   `WorkspaceSource` and seeded in-process stores.
//!
//! ## Blessing
//!
//! Goldens live at `src/turn/goldens/<name>.golden.txt`. On an intended
//! prompt change, re-bless with:
//!
//! ```text
//! $env:WYLDE_BLESS_GOLDENS = "1"; cargo test -p wylde-harness --lib golden
//! ```
//!
//! then REVIEW THE DIFF in git before committing — the diff is the
//! point. Line endings are normalised to `\n` on both sides.

use serde_json::{json, Value};

use crate::turn::context_gather::{
    AnchorBlock, ChatContext, NeighborLine, SymbolContextBlock, TokenOverrides,
};
use crate::turn::{prompt, prompt_assembly, token_budget};

/// Compare `actual` against the named golden file, or rewrite the file
/// when `WYLDE_BLESS_GOLDENS` is set.
fn assert_golden(name: &str, actual: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("turn")
        .join("goldens")
        .join(format!("{name}.golden.txt"));
    let normalise = |s: &str| s.replace("\r\n", "\n");
    let actual = normalise(actual);
    if std::env::var("WYLDE_BLESS_GOLDENS").is_ok() {
        std::fs::create_dir_all(path.parent().expect("goldens dir")).expect("mkdir goldens");
        std::fs::write(&path, &actual).expect("bless golden");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "missing golden {} — bless with WYLDE_BLESS_GOLDENS=1 and review the diff",
            path.display()
        )
    });
    assert_eq!(
        normalise(&expected),
        actual,
        "golden '{name}' drifted. If the prompt change is INTENDED, re-bless \
         with WYLDE_BLESS_GOLDENS=1 and review the git diff; otherwise this \
         is a prompt regression."
    );
}

/// A fixed catalog fixture — one verb tool, one imperative survivor, one
/// retired resource-backed tool, one deferred tool. Frozen so the base
/// prompt goldens only move when prompt *logic* moves.
fn fixture_catalog() -> Vec<Value> {
    let row = |id: &str, name: &str, group: &str, desc: &str, params: Value| {
        json!({
            "id": id, "name": name, "group": group, "description": desc,
            "parameters": params, "destructive": false,
            "status": "active", "deferred_phase": null,
        })
    };
    vec![
        row(
            "wylde_search",
            "wylde_search",
            "verbs",
            "Search a resource type.",
            json!([
                {"name": "resource_type", "type": "string", "required": true,
                 "description": "Which resource to search"},
                {"name": "query", "type": "string", "required": true,
                 "description": "Search query"},
                {"name": "limit", "type": "int", "required": false,
                 "description": "Max results"}
            ]),
        ),
        row(
            "voice_mic_start",
            "voice.mic.start",
            "voice",
            "Open the OS microphone device.",
            json!([]),
        ),
        row(
            "memory_search",
            "memory.search",
            "memory",
            "Search long-term memory.",
            json!([{"name": "query", "type": "string", "required": true,
                    "description": "Search query"}]),
        ),
        json!({
            "id": "screenshot", "name": "visual.screenshot", "group": "visual",
            "description": "Take a screenshot.", "parameters": [],
            "destructive": true, "status": "deferred", "deferred_phase": "11",
        }),
    ]
}

/// The full-context slot fixture: every `ChatContext` slot populated
/// with deterministic text.
fn fixture_context() -> ChatContext {
    ChatContext {
        user_profile: "Name: Golden User\nStyle: terse\nUser rules (follow verbatim):\nAlways show diffs.".into(),
        conversation_short_term: vec![
            "- the user is renaming the gather module".into(),
            "- tests must stay green".into(),
        ],
        conversation_summary: Some(
            "The user has been refactoring the context gather flow.".into(),
        ),
        long_term: vec![
            "- prefers Rust over Python for new modules".into(),
            "- works from a Windows box".into(),
        ],
        workspace_memory: vec![
            "- the gather refactor must keep render byte-stable".into(),
            "- goldens bless via WYLDE_BLESS_GOLDENS".into(),
        ],
        vocabulary_anchors: vec![AnchorBlock {
            identifier: "the_gather".into(),
            text: "{{the_gather}} — the pre-LLM context gather (code symbol `gather_with`)".into(),
        }],
        workspace_rag: vec!["fn gather_with() { /* fixture snippet */ }".into()],
        workspace_notes: vec!["uses cargo nextest".into()],
        workspace_persona: Some("Be precise.".into()),
        symbol_contexts: vec![SymbolContextBlock {
            symbol_id: "gather_with".into(),
            focal: "Symbol `gather_with` — src/turn/context_gather.rs:404\npub(crate) async fn gather_with(...) {}".into(),
            neighbors: vec![
                NeighborLine { hop: 1, text: "  called by `handle_run_turn` (src/turn/actions.rs)".into() },
                NeighborLine { hop: 2, text: "  calls `evict` (src/turn/token_budget.rs)".into() },
            ],
        }],
        // History rides the messages array, not the rendered block — the
        // goldens over `render` must stay byte-identical with it present
        // (and the eviction goldens exercise its budget participation).
        history: vec![
            crate::turn::context_gather::HistoryMessage {
                role: "user".into(),
                content: "what does the gather do?".into(),
            },
            crate::turn::context_gather::HistoryMessage {
                role: "assistant".into(),
                content: "It assembles the layered context for a turn.".into(),
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── base system prompt ───────────────────────────────────────────

    #[test]
    fn golden_base_prompt_verb_mode() {
        assert_golden(
            "base_prompt_verb_mode",
            &prompt::build_system_prompt(&fixture_catalog(), true, false),
        );
    }

    #[test]
    fn golden_base_prompt_legacy_mode() {
        assert_golden(
            "base_prompt_legacy_mode",
            &prompt::build_system_prompt(&fixture_catalog(), false, false),
        );
    }

    #[test]
    fn golden_base_prompt_native_tools() {
        // B10: native-tool-capable models get the lean base instruction
        // (no in-content JSON shape).
        assert_golden(
            "base_prompt_native_tools",
            &prompt::build_system_prompt(&fixture_catalog(), true, true),
        );
    }

    // ── rendered slot block ──────────────────────────────────────────

    #[test]
    fn golden_slots_full_context() {
        assert_golden(
            "slots_full_context",
            &prompt_assembly::render(&fixture_context()),
        );
    }

    #[test]
    fn empty_context_renders_empty_no_golden_needed() {
        assert_eq!(prompt_assembly::render(&ChatContext::default()), "");
    }

    // ── eviction behavior at fixed budgets ───────────────────────────

    #[test]
    fn golden_eviction_mid_pressure() {
        // A budget chosen (relative to the fixture) to force the first
        // few drops: the RAG snippet (tier 1, B6) and the summary
        // (tier 2) go; long-term, notes, persona, the symbol context,
        // and the history window (tier 6.5) survive.
        let mut ctx = fixture_context();
        let full = token_budget::estimate_tokens(&prompt_assembly::render(&ctx))
            + token_budget::history_tokens(&ctx);
        token_budget::evict(&mut ctx, full - 40);
        assert_eq!(ctx.history.len(), 2, "history outlasts tiers 2-4");
        assert_golden("eviction_mid_pressure", &prompt_assembly::render(&ctx));
    }

    #[test]
    fn golden_eviction_never_drop_floor() {
        // An absurdly small budget: everything droppable is gone — the
        // history window included — and the never-drop tier (profile,
        // short-term, vocabulary) survives verbatim. This is the floor
        // the model ALWAYS sees.
        let mut ctx = fixture_context();
        token_budget::evict(&mut ctx, 1);
        assert!(ctx.history.is_empty(), "history is evictable (B1)");
        assert_golden("eviction_never_drop_floor", &prompt_assembly::render(&ctx));
    }

    // ── end-to-end gather (mocked service + seeded stores) ───────────

    struct GoldenSource;

    impl crate::turn::context_gather::WorkspaceSource for GoldenSource {
        async fn gather_prompt(
            &self,
            _ws: &str,
            _m: &str,
            _route: bool,
        ) -> Result<
            Option<crate::turn::context_gather::WorkspaceBlock>,
            crate::turn::context_gather::SourceStatus,
        > {
            Ok(Some(crate::turn::context_gather::WorkspaceBlock {
                persona: Some("Be precise.".to_owned()),
                notes: Vec::new(),
                rag: Vec::new(),
                route_candidates: None,
            }))
        }
        async fn find_anchors(
            &self,
            _ws: &str,
            token: &str,
        ) -> Result<Vec<wylde_shared::anchor::Anchor>, crate::turn::context_gather::SourceStatus>
        {
            use wylde_shared::anchor::{Anchor, AnchorKind, AnchorScope, AnchorTarget};
            if token == "the_gather" {
                Ok(vec![Anchor::new(
                    "the_gather",
                    AnchorKind::Concept,
                    AnchorTarget::Concept {
                        text: "the gather".into(),
                    },
                    AnchorScope::Workspace {
                        workspace_id: "ws".into(),
                    },
                    "the pre-LLM context gather",
                )])
            } else {
                Ok(Vec::new())
            }
        }
        async fn find_symbols(
            &self,
            _ws: &str,
            token: &str,
        ) -> Result<Vec<String>, crate::turn::context_gather::SourceStatus> {
            if token == "gather_with" {
                Ok(vec!["gather_with".to_owned()])
            } else {
                Ok(Vec::new())
            }
        }
        async fn symbol_context(
            &self,
            _ws: &str,
            symbol_id: &str,
        ) -> Result<Value, crate::turn::context_gather::SourceStatus> {
            Ok(json!({
                "symbol": {"id": symbol_id, "name": symbol_id, "kind": "Function",
                           "file": "src/turn/context_gather.rs", "line": 404,
                           "body": "pub(crate) async fn gather_with(...) {}"},
                "callers": [{"id": "handle_run_turn", "name": "handle_run_turn",
                             "kind": "Function", "file": "src/turn/actions.rs",
                             "hop_distance": 1, "rel_type": "CALLS"}],
                "callees": [],
                "types_used": [],
                "siblings": [],
                "hops_traversed": 1,
                "took_ms": 1
            }))
        }
    }

    #[tokio::test]
    async fn golden_gather_end_to_end() {
        let _env = crate::user_profile::test_support::TestEnv::new();
        // Seed the in-process stores deterministically.
        crate::user_profile::store::with_store(|s| {
            s.profile.name = Some("Golden User".into());
            s.profile.style = Some("terse".into());
        })
        .unwrap();
        let mut doc = serde_json::Map::new();
        doc.insert("id".into(), json!("conv-golden"));
        doc.insert("messages".into(), json!([]));
        doc.insert(
            "auto_summary".into(),
            json!("The user has been refactoring the gather flow."),
        );
        crate::memory::conversations::store::save_conversation(&doc).unwrap();
        // (No long-term records: keeps the e2e embed-free and fast; the
        // long-term slot is pinned by the fixture-level goldens above.)
        // One workspace memory record (M2 option B) — the in-process
        // records tier renders deterministically (body text only).
        crate::memory::workspace::store::save_new(
            "ws",
            "the gather flow is being refactored slice by slice",
            "reflection:conversation:conv-golden",
            Some(8.0),
            Vec::new(),
        )
        .unwrap();

        let out = crate::turn::context_gather::gather_with(
            &GoldenSource,
            Some("ws"),
            "explain gather_with and {{the_gather}}",
            "conv-golden",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;
        assert!(!out.degraded);
        assert_golden("gather_end_to_end", &out.system_slots);
    }
}

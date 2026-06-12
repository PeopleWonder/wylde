//! Slot **liveness** net (memory plan M8).
//!
//! Three instances of one defect class — history pre-B1, long-term
//! pre-B3, auto-summary pre-M1 — proved that byte-pinning the render
//! (the goldens) cannot catch a *dead data source*: a slot renders
//! perfectly from fixture data while its real producer has zero
//! callers. Each test here drives a slot's REAL producer and asserts
//! the data comes out of `gather_with` + `render`, end to end.
//!
//! Where a live service or LLM would be needed, the test drives the
//! producer's seam instead (mock `WorkspaceSource`, injected
//! `ReflectionChat`, the extraction `apply` half) — the point is the
//! WIRING between producer and slot, which goldens structurally can't
//! see. The auto-summary LLM half is covered end-to-end (mock ollama
//! pipe, real post-turn hook) in `tests/auto_summary_liveness.rs`.

#![cfg(test)]

use serde_json::{json, Value};

use crate::turn::context_gather::{gather_with, TokenOverrides, WorkspaceBlock, WorkspaceSource};
use crate::turn::token_budget;
use crate::user_profile::test_support::TestEnv;

/// A minimal mock source: optional notes/persona, nothing else.
#[derive(Default)]
struct Source {
    block: Option<WorkspaceBlock>,
}

impl WorkspaceSource for Source {
    async fn gather_prompt(
        &self,
        _ws: &str,
        _m: &str,
    ) -> Result<Option<WorkspaceBlock>, crate::turn::context_gather::SourceStatus> {
        Ok(self.block.clone())
    }
    async fn find_anchors(
        &self,
        _ws: &str,
        _t: &str,
    ) -> Result<Vec<wylde_shared::anchor::Anchor>, crate::turn::context_gather::SourceStatus> {
        Ok(Vec::new())
    }
    async fn find_symbols(
        &self,
        _ws: &str,
        _t: &str,
    ) -> Result<Vec<String>, crate::turn::context_gather::SourceStatus> {
        Ok(Vec::new())
    }
    async fn symbol_context(
        &self,
        _ws: &str,
        _s: &str,
    ) -> Result<Value, crate::turn::context_gather::SourceStatus> {
        Err(crate::turn::context_gather::SourceStatus::Empty)
    }
}

async fn gather(ws: Option<&str>, msg: &str, conv: &str) -> String {
    gather_with(
        &Source::default(),
        ws,
        msg,
        conv,
        &TokenOverrides::default(),
        token_budget::DEFAULT_TOKEN_BUDGET,
    )
    .await
    .system_slots
}

fn seed_conversation(id: &str, messages: Value) {
    let mut doc = serde_json::Map::new();
    doc.insert("id".into(), json!(id));
    doc.insert("messages".into(), messages);
    crate::memory::conversations::store::save_conversation(&doc).unwrap();
}

/// Tier-2 summary: the producer's persist half (`summary_fields` +
/// `merge_fields` — exactly what `refresh_standalone` writes) feeds the
/// rendered slot. (The LLM half + the post-turn hook are pinned in
/// `tests/auto_summary_liveness.rs`.)
#[tokio::test]
async fn summary_producer_feeds_the_tier2_slot() {
    let _env = TestEnv::new();
    seed_conversation("sl-sum", json!([{"role": "user", "content": "x"}]));
    let fields = crate::chat::search::summary::summary_fields(
        "They were porting the memory tier.",
        &["memory".into()],
        &[0.6, 0.8],
        1,
    );
    crate::memory::conversations::store::merge_fields("sl-sum", fields).unwrap();

    let slots = gather(None, "hello", "sl-sum").await;
    assert!(slots.contains("### Conversation summary"));
    assert!(slots.contains("They were porting the memory tier."));
}

/// Long-term: `long_term::save` (the real verb/extractor sink) feeds
/// the rendered slot.
#[tokio::test]
async fn long_term_save_feeds_the_slot() {
    let _env = TestEnv::new();
    crate::memory::long_term::save("prefers tabs over spaces", "verb", Some(9.0), vec![], None)
        .unwrap();
    let slots = gather(None, "anything", "c").await;
    assert!(slots.contains("### Long-term memory"));
    assert!(slots.contains("- prefers tabs over spaces"));
}

/// Working memory: the post-turn extractor's `apply` half (the real
/// producer since B14) lands entries that the never-drop slot renders.
#[tokio::test]
async fn extractor_apply_feeds_the_working_memory_slot() {
    let _env = TestEnv::new();
    use crate::memory::post_turn_extractor::{apply, Extraction, MemoryEntry};
    apply(
        "sl-wm",
        None,
        Extraction {
            memory_entries: vec![MemoryEntry {
                kind: "decision".into(),
                text: "pin the gather budget at 4096".into(),
                importance: 6,
            }],
            profile_proposals: Vec::new(),
            anchor_proposals: Vec::new(),
        },
    )
    .await;

    let slots = gather(None, "anything", "sl-wm").await;
    assert!(slots.contains("### Conversation memory"));
    assert!(slots.contains("- pin the gather budget at 4096"));
}

/// Workspace insights (M2 option B): conversation reflection — the
/// real consolidation producer, driven through `reflect` with an
/// injected chat — writes the workspace record that the new evictable
/// slot renders. THE loop the evaluation found silently broken.
#[tokio::test]
async fn reflection_output_reaches_the_workspace_insights_slot() {
    let _env = TestEnv::new();
    use crate::memory::long_term::reflection::{ReflectOptions, ReflectionChat};

    struct InsightChat;
    #[async_trait::async_trait]
    impl ReflectionChat for InsightChat {
        async fn ask(&self, _m: Vec<Value>, _model: Option<String>) -> String {
            "The user is migrating the harness memory tier to Rust.".to_owned()
        }
    }

    // A workspace-bound conversation with enough working memory.
    seed_conversation("sl-refl", json!([]));
    crate::memory::conversations::store::set_workspace("sl-refl", Some("sl-ws")).unwrap();
    for i in 0..3 {
        crate::memory::short_term::store::append_working_memory(
            "sl-refl",
            json!({"kind": "fact", "data": format!("migration step {i}"), "importance": 5}),
        )
        .unwrap();
    }

    let r = crate::memory::reflection::reflect(
        "conversation:sl-refl",
        Some(&InsightChat),
        ReflectOptions::default(),
    )
    .await;
    assert!(!r.skipped, "reflection ran: {:?}", r.skip_reason);

    let slots = gather(Some("sl-ws"), "anything", "sl-refl").await;
    assert!(
        slots.contains("### Workspace insights"),
        "insights slot live: {slots}"
    );
    assert!(slots.contains("- The user is migrating the harness memory tier to Rust."));
}

/// Notes + persona: the service-side parts arrive through the
/// `WorkspaceSource` seam and render under `### Workspace context`.
#[tokio::test]
async fn workspace_block_feeds_notes_and_persona_subsections() {
    let _env = TestEnv::new();
    let src = Source {
        block: Some(WorkspaceBlock {
            persona: Some("Be precise.".into()),
            notes: vec!["uses cargo nextest".into()],
            rag: Vec::new(),
        }),
    };
    let out = gather_with(
        &src,
        Some("ws"),
        "anything",
        "c",
        &TokenOverrides::default(),
        token_budget::DEFAULT_TOKEN_BUDGET,
    )
    .await;
    assert!(out.system_slots.contains("## Persona\nBe precise."));
    assert!(out
        .system_slots
        .contains("## Workspace memory\n- uses cargo nextest"));
}

/// History: persisted messages ride the wire array (B1).
#[tokio::test]
async fn persisted_messages_feed_the_history_window() {
    let _env = TestEnv::new();
    seed_conversation(
        "sl-hist",
        json!([
            {"role": "user", "content": "first question"},
            {"role": "assistant", "content": "first answer"},
        ]),
    );
    let out = gather_with(
        &Source::default(),
        None,
        "second question",
        "sl-hist",
        &TokenOverrides::default(),
        token_budget::DEFAULT_TOKEN_BUDGET,
    )
    .await;
    let contents: Vec<&str> = out
        .history
        .iter()
        .filter_map(|m| m["content"].as_str())
        .collect();
    assert_eq!(contents, vec!["first question", "first answer"]);
}

/// Persona slot (the user profile): the profile store feeds tier 7.
#[tokio::test]
async fn profile_store_feeds_the_user_profile_slot() {
    let _env = TestEnv::new();
    crate::user_profile::store::with_store(|s| {
        s.profile.name = Some("Liveness User".into());
    })
    .unwrap();
    let slots = gather(None, "anything", "c").await;
    assert!(slots.contains("### User profile"));
    assert!(slots.contains("Name: Liveness User"));
}

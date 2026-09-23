//! Deferred tool stubs.
//!
//! A *deferred* entry is registered for catalog/alias purposes but has
//! no live handler — a dispatch returns a `phase_<n>_deferred` error
//! envelope the LLM can interpret rather than `unknown_tool` confusion.
//!
//! Why register them at all? Two reasons:
//!
//! 1. The alias map needs to know every name the model might emit so
//!    the salvage parser can route a known-but-not-ready call to the
//!    registry instead of mislabelling it `tool_call_text_unrecognised`.
//!    Deferred entries still contribute aliases.
//! 2. `tools.list` / `meta.tool_search` return deferred tools too, with
//!    `status: "deferred"` and the target phase tag, so the GUI can
//!    render them as "coming soon" and an inquisitive model gets a
//!    usable "not yet — Phase N" error.
//!
//! Tier classification still applies — a destructive-but-deferred tool
//! is still gated, so a `read_only` turn won't get back a misleading
//! `phase_7_deferred` instead of `tier_read_only`.
//!
//! ## 2026-06-05 catalog cleanup (`chore/llm-catalog-cleanup-…`)
//!
//! The two large "coming soon" blocks were removed — they advertised
//! features with no committed implementation path and only cluttered
//! `tool_search` discovery with dead rows:
//!
//! * **Phase-6 shell / git / code / test / dev (17)** —
//!   `execute_bash`, `execute_python`, `git_*` (×7), `run_tests`,
//!   `run_test_file`, `wylde_check`, `lint_*` (×4), `gui_errors_recent`.
//!   These shelled out from Python and were gated on a sandbox-spawn
//!   decision that never landed. (The *gateway* still exposes the real
//!   lint / gui-error HTTP routes; those are untouched.)
//! * **Phase-11 visual / computer-use (15)** — `screenshot`, `click`,
//!   `type_text`, `hotkey`, `scroll`, `mouse_move`,
//!   `get_mouse_position`, `get_screen_size`, `wait_for`, `navigate`,
//!   and the five `browser_*` tools. The visual / computer-use layer
//!   was never built and is out of scope.
//!
//! Three deferred entries are retained because they back genuinely
//! planned or live-but-non-model-callable surfaces — see [`register`].

use crate::tooling::registry::{entry_deferred, Registry};

pub fn register(reg: &mut Registry) {
    // ── Memory layer (Phase 7) ─────────────────────────────────────────
    //
    // The long-term tier (`memory_long_term_save` / `memory_update` /
    // `memory_delete` / `memory_search`) AND the workspace-scoped tier
    // (`memory_workspace_save` / `_update` / `_delete` / `_search` /
    // `_list`) are now both ACTIVE in [`crate::tooling::tools::memory`].
    // Nothing memory-related is deferred any longer.

    // ── Voice streaming subscriptions (Phase 11) ───────────────────────
    //
    // The unary voice device tools (voice.mic.start / voice.mic.stop /
    // voice.wakeword.start / voice.wakeword.stop) and the
    // transcribe/synthesize inference tools are all ACTIVE in
    // [`super::voice`]. The two *streaming subscriptions* stay deferred
    // on purpose: the model-callable surface is unary-only, and the
    // orchestrator / GUI consume the streaming primitives directly over
    // `send_action_stream` (never through the tool registry). They are
    // catalogued here only so the alias map + tools.list know the names.
    reg.insert(entry_deferred(
        "voice_mic_chunks",
        "voice.mic.chunks",
        "voice",
        "Streaming PCM chunks from the active mic capture. Subscribe \
         to receive base64-encoded `pcm_s16le` frames as they arrive.",
        vec![],
        false,
        "11",
        "streaming subscription — orchestrator/GUI call send_action_stream directly; \
         model-callable surface stays unary (use voice.mic.start / voice.mic.stop)",
    ));
    reg.insert(entry_deferred(
        "voice_wakeword_events",
        "voice.wakeword.events",
        "voice",
        "Streaming detection events from the active wake-word listener. \
         Subscribe to receive `event` payloads with score + elapsed_ms.",
        vec![],
        false,
        "11",
        "streaming subscription — orchestrator/GUI call send_action_stream directly; \
         model-callable surface stays unary (use voice.wakeword.start / voice.wakeword.stop)",
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_register_contributes_expected_keepers() {
        let mut reg = Registry::empty();
        register(&mut reg);
        // Only the two voice streaming subscriptions (Phase 11) remain
        // deferred; the workspace-memory tier is now an active tool.
        assert!(reg.lookup("memory_workspace_save").is_none());
        assert!(reg.lookup("voice_mic_chunks").is_some());
        assert!(reg.lookup("voice_wakeword_events").is_some());
    }

    #[test]
    fn deleted_phase6_and_visual_stubs_are_gone() {
        // The 2026-06-05 cleanup removed every phase-6 shell/git/dev
        // stub and every phase-11 visual stub. Spot-check that the
        // representative ids no longer resolve.
        let mut reg = Registry::empty();
        register(&mut reg);
        for id in [
            "execute_bash",
            "execute_python",
            "git_status",
            "git_commit",
            "run_tests",
            "lint_rust",
            "gui_errors_recent",
            "screenshot",
            "type_text",
            "browser_eval",
            "navigate",
        ] {
            assert!(
                reg.lookup(id).is_none(),
                "{id} should have been deleted by the catalog cleanup"
            );
        }
    }

    #[test]
    fn active_tools_are_not_deferred() {
        // Ollama (Phase 8) and the voice transcribe/synthesize tools
        // (Slice 11.E) are active handlers, never deferred here.
        let mut reg = Registry::empty();
        register(&mut reg);
        for id in [
            "list_loaded_models",
            "preload_model",
            "evict_model",
            "auto_evict_lru",
            "voice_transcribe",
            "voice_synthesize",
            "voice_transcribe_stream",
            "voice_synthesize_stream",
        ] {
            assert!(
                reg.lookup(id).is_none(),
                "{id} should not be registered by deferred::register"
            );
        }
    }

    #[test]
    fn workspace_memory_save_no_longer_deferred() {
        // It moved to an active handler in tools::memory; deferred no
        // longer catalogs it.
        let mut reg = Registry::empty();
        register(&mut reg);
        assert!(reg.lookup("memory_workspace_save").is_none());
    }

    #[test]
    fn deferred_dotted_aliases_resolve_to_canonical_ids() {
        let mut reg = Registry::empty();
        register(&mut reg);
        assert_eq!(
            reg.lookup("voice.mic.chunks").unwrap().id,
            "voice_mic_chunks"
        );
    }

    #[test]
    fn voice_streaming_variants_registered_with_phase_11_tag() {
        let mut reg = Registry::empty();
        register(&mut reg);
        // Slice 11.D streaming subscriptions — voice.mic.chunks and
        // voice.wakeword.events stay catalog-only; their unary
        // counterparts (voice.mic.start, voice.wakeword.start) are
        // active via super::voice::register.
        for (id, dotted) in [
            ("voice_mic_chunks", "voice.mic.chunks"),
            ("voice_wakeword_events", "voice.wakeword.events"),
        ] {
            assert_eq!(reg.lookup(id).unwrap().id, id);
            assert_eq!(reg.lookup(dotted).unwrap().id, id);
            match &reg.lookup(id).unwrap().kind {
                crate::tooling::registry::HandlerKind::Deferred { phase, .. } => {
                    assert_eq!(*phase, "11", "{id} should be tagged phase 11");
                }
                crate::tooling::registry::HandlerKind::Active(_) => {
                    panic!(
                        "{id} should be Deferred — streaming subscriptions are not model-callable"
                    );
                }
            }
        }
    }
}

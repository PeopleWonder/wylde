//! Deferred tool stubs.
//!
//! Phase 6's job is to land the registry, the dispatcher, the alias map,
//! and the self-contained tool implementations. Memory tools, RAG
//! tools, visual / computer-use tools, and the shell/git/test/dev
//! groups that depend on a sandbox decision all register `phase_*_deferred`
//! entries here.
//!
//! Why register them at all? Two reasons:
//!
//! 1. The alias map needs to know every name the model might emit so
//!    the salvage parser can route a `memory_search` call to the
//!    registry instead of mislabelling it `tool_call_text_unrecognised`.
//!    Deferred entries still contribute aliases.
//! 2. `tools.list` returns deferred tools too, with `status: "deferred"`
//!    and the target phase tag. The GUI can render them as "coming
//!    soon" rows, and an inquisitive model can ask `tool_search` for
//!    them and get a usable error message ("not yet — Phase 7") rather
//!    than silently routing to the wrong handler.
//!
//! Tier classification still applies — a destructive-but-deferred tool
//! is still gated, so a `read_only` turn won't get back a misleading
//! `phase_7_deferred` instead of `tier_read_only`.

use crate::tooling::registry::{entry_deferred, param, Registry};

pub fn register(reg: &mut Registry) {
    // ── Memory layer (Phase 7) ─────────────────────────────────────────
    //
    // Phase 7.B (long-term memory) moved
    // `memory_long_term_save` / `memory_update` / `memory_delete` /
    // `memory_search` into [`crate::tooling::tools::memory`] as active
    // handlers. They no longer appear here.
    //
    // The workspace-scoped tier is still deferred — port lands with the
    // `workspace_memory/` slice (Phase 7.B+).
    reg.insert(entry_deferred(
        "memory_workspace_save",
        "memory.workspace.save",
        "memory",
        "Save a memory scoped to the active workspace.",
        vec![param("body", "string", true, "Memory text")],
        true,
        "7",
        "lands with the workspace-memory port",
    ));

    // ── RAG (Phase 7.B-3) ──────────────────────────────────────────────
    //
    // The eight `rag_*` tools (rag_ask / rag_index / rag_reindex /
    // rag_prune / rag_feedback / rag_misses / rag_chunk_usage /
    // rag_graph_stats) moved to [`crate::tooling::tools::rag`] as active
    // handlers and no longer appear here.

    // ── Visual / computer-use (Phase 11) ───────────────────────────────
    for (id, name, desc) in [
        ("screenshot", "visual.screenshot", "Take a screenshot."),
        ("click", "visual.click", "Mouse click at coordinates."),
        ("type_text", "visual.type_text", "Type text to the keyboard."),
        ("hotkey", "visual.hotkey", "Send a keyboard chord."),
        ("scroll", "visual.scroll", "Scroll at coordinates."),
        ("mouse_move", "visual.mouse_move", "Move the mouse."),
        ("get_mouse_position", "visual.get_mouse_position", "Read current cursor position."),
        ("get_screen_size", "visual.get_screen_size", "Read display dimensions."),
        ("wait_for", "visual.wait_for", "Wait for an image or text match."),
        ("navigate", "visual.navigate", "Navigate to a URL."),
        ("browser_click", "visual.browser_click", "Click in the browser."),
        ("browser_eval", "visual.browser_eval", "Evaluate JS in the browser."),
        ("browser_fill", "visual.browser_fill", "Fill a form field in the browser."),
        ("browser_screenshot", "visual.browser_screenshot", "Browser screenshot."),
        ("browser_text", "visual.browser_text", "Read text from the browser DOM."),
    ] {
        reg.insert(entry_deferred(
            id, name, "visual", desc, vec![], true, "11",
            "lands with the Voice / Visual port (Phase 11)",
        ));
    }

    // ── Voice service primitives (Phase 11) ────────────────────────────
    //
    // Slice 11.E cutover (2026-05-26) moved the unary + streaming
    // transcribe/synthesize entries to [`super::voice`] as active tools
    // (thin bridges over the wylde-voice pipe). The streaming chunk +
    // event subscriptions stay deferred — see the block below for those.

    // Streaming variants from Slice 11.D. The unary voice.mic.start /
    // voice.mic.stop / voice.wakeword.start / voice.wakeword.stop tools
    // are active in `super::voice`; the streaming chunk/event
    // subscriptions stay deferred because the model-callable surface
    // is one-shot-only (the orchestrator + GUI consume the streaming
    // primitives directly over `send_action_stream`).
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

    // ── Shell / git / code / test / dev (Phase 6 follow-up) ────────────
    //
    // These shell-out from Python today. The Rust port needs a sandbox
    // decision (whether to spawn a subprocess from inside the harness
    // ring, vs. routing through wylde-lifecycle, vs. a dedicated
    // sandbox crate). Deferred until that's settled.
    for (id, name, group, desc, destructive) in [
        ("execute_bash", "code.execute_bash", "code", "Execute a shell command line.", true),
        ("execute_python", "code.execute_python", "code", "Execute Python code.", true),
        ("git_status", "git.git_status", "git", "Show git working tree status.", false),
        ("git_diff", "git.git_diff", "git", "Show git diff.", false),
        ("git_log", "git.git_log", "git", "Show git log.", false),
        ("git_branch", "git.git_branch", "git", "Manage git branches.", true),
        ("git_add", "git.git_add", "git", "Stage files.", true),
        ("git_commit", "git.git_commit", "git", "Create a git commit.", true),
        ("git_stash", "git.git_stash", "git", "Manage stash entries.", true),
        ("run_tests", "test.run_tests", "test", "Run the test suite.", false),
        ("run_test_file", "test.run_test_file", "test", "Run one test file.", false),
        ("wylde_check", "dev.wylde_check", "dev", "Run wylde_check guardrails.", false),
        ("lint_all", "dev.lint_all", "dev", "Lint every codebase.", false),
        ("lint_python", "dev.lint_python", "dev", "Lint Python.", false),
        ("lint_rust", "dev.lint_rust", "dev", "Lint Rust.", false),
        ("lint_svelte", "dev.lint_svelte", "dev", "Lint Svelte.", false),
        ("gui_errors_recent", "dev.gui_errors_recent", "dev", "Show recent GUI errors.", false),
    ] {
        reg.insert(entry_deferred(
            id, name, group, desc, vec![], destructive, "6",
            "needs a sandbox-spawn decision before the Rust port lands",
        ));
    }

    // ── Ollama tools (Phase 8) ────────────────────────────────────────
    //
    // Moved to [`crate::tooling::tools::ollama`] as active handlers —
    // thin wrappers over the wylde-ollama pipe. No deferred entries
    // remain here.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_register_contributes_all_expected_groups() {
        let mut reg = Registry::empty();
        register(&mut reg);
        // Spot-check one tool per group.
        // memory_workspace_save is the only memory tool still deferred
        // (long_term tools moved to active in Phase 7.B; rag.* moved
        // to active in Phase 7.B-3).
        assert!(reg.lookup("memory_workspace_save").is_some());
        assert!(reg.lookup("screenshot").is_some());
        assert!(reg.lookup("execute_bash").is_some());
        assert!(reg.lookup("git_status").is_some());
        // Ollama tools moved to active in Phase 8.
        assert!(reg.lookup("list_loaded_models").is_none());
    }

    #[test]
    fn ollama_tools_are_no_longer_deferred() {
        let mut reg = Registry::empty();
        register(&mut reg);
        for id in [
            "list_loaded_models",
            "preload_model",
            "evict_model",
            "auto_evict_lru",
        ] {
            assert!(
                reg.lookup(id).is_none(),
                "{id} should not be registered by deferred::register"
            );
        }
    }

    #[test]
    fn workspace_memory_save_still_deferred_and_destructive() {
        let mut reg = Registry::empty();
        register(&mut reg);
        assert!(reg.lookup("memory_workspace_save").unwrap().destructive);
    }

    #[test]
    fn deferred_dotted_aliases_resolve_to_canonical_ids() {
        let mut reg = Registry::empty();
        register(&mut reg);
        assert_eq!(
            reg.lookup("memory.workspace.save").unwrap().id,
            "memory_workspace_save"
        );
        assert_eq!(reg.lookup("git.git_status").unwrap().id, "git_status");
    }

    #[test]
    fn voice_unary_and_streaming_promoted_to_active_at_11e() {
        // Slice 11.E cutover (2026-05-26): the four voice transcribe/
        // synthesize entries moved out of `deferred::register` and into
        // `super::voice::register` as active tools. `deferred` MUST NOT
        // re-register them (a stale Deferred would shadow the active
        // bridge handler and fail the model dispatch).
        let mut reg = Registry::empty();
        register(&mut reg);
        for id in [
            "voice_transcribe",
            "voice_synthesize",
            "voice_transcribe_stream",
            "voice_synthesize_stream",
        ] {
            assert!(
                reg.lookup(id).is_none(),
                "{id} must not be registered by deferred::register after Slice 11.E"
            );
        }
    }

    #[test]
    fn voice_11d_streaming_variants_registered_with_phase_11_tag() {
        let mut reg = Registry::empty();
        register(&mut reg);
        // Slice 11.D streaming subscriptions — voice.mic.chunks and
        // voice.wakeword.events stay catalog-only; their unary
        // counterparts (voice.mic.start, voice.wakeword.start) flip to
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
                    panic!("{id} should be Deferred — streaming subscriptions are not model-callable");
                }
            }
        }
    }
}

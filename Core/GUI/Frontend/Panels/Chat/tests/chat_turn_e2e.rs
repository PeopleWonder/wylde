//! **The all-surfaces chat-turn end-to-end test** (issue #236, 0.2).
//!
//! One test per GUI chat entry point, each driving the WHOLE path a user
//! actually drives:
//!
//! ```text
//!   type into that surface's composer
//!     → Enter (InputEvent::Submit)
//!       → ChatPanel::submit_text → send_user_message
//!         → Chat/src/ipc.rs::start_turn_with_model
//!           → wylde_gui_pipe::call ─────────┐
//!           → wylde_gui_pipe::stream_call ──┤ the transport seam (see below)
//!                                           ↓
//!             wylde_shared::ipc::dispatch_action / take_streaming_action
//!                                     ── REAL action registry, populated by
//!                                        `wylde_harness::install()` — the same
//!                                        call the shipped harness binary makes
//!               → DefaultHarnessApi::chat_start_turn
//!                 → wylde_harness::turn   ── REAL turn driver
//!                   → ollama.chat_stream  ── over a REAL named pipe to the
//!                                            stub inference server
//!           → TurnChunk::from_value → apply_turn_chunk
//!             → the assistant bubble in THAT SAME surface
//! ```
//!
//! ## Why this file exists
//!
//! Coverage stopped short at both ends and nothing joined them:
//!
//! * [`type_and_send.rs`] drives the real composer but answers
//!   `chat.start_turn` from `ScriptedBackend` — canned JSON, **the turn
//!   driver never runs**.
//! * `wylde-harness/tests/reasoning_plan_e2e.rs` drives the real turn driver
//!   over a real pipe with a mock ollama, but enters at
//!   `chat::handle_start_turn` — **the GUI is never involved**.
//!
//! Everything between them — the transport, the `start_turn` + `stream_turn`
//! pair, the event decode, the render back onto the bubble — was covered by
//! nothing, on a path that IS the product.
//!
//! ## What is stubbed, and what deliberately is not
//!
//! **Stubbed: inference only.** A fixture `wylde-ollama` service returns
//! [`STUB_REPLY`] for every `ollama.chat_stream`, reached by the turn driver
//! over a **real named pipe**. That keeps the test hermetic and sub-second
//! while leaving every layer under test real — we are asserting *wiring*,
//! not model quality.
//!
//! **Not stubbed: the harness.** [`HarnessBackend`] is emphatically not a
//! `ScriptedBackend` — it answers nothing itself. It hands the GUI's own
//! request envelope to [`wylde_shared::ipc::dispatch_action`], the same
//! registry lookup the pipe server performs, against the registry
//! `wylde_harness::install()` populated. `chat.start_turn` therefore lands
//! on the production `DefaultHarnessApi` and runs the production turn
//! driver. If a verb is unregistered, misnamed, or the driver breaks, this
//! test goes red exactly as the product would.
//!
//! ### The one seam, and why it is here
//!
//! What this test does *not* cross is the GUI's own msgpack framing
//! (`call_inner` / `run_stream_inner`). That is not a preference — gpui's
//! test scheduler **panics** ("Your test is not deterministic") the moment a
//! gpui task is woken from a foreign thread, which is precisely what a real
//! pipe round-trip on the tokio bridge does. A windowed gpui test and the
//! GUI's own async transport are mutually exclusive by construction.
//!
//! Dispatching through the registry keeps every layer that owns *behaviour*
//! under test and keeps the test deterministic. The framing layer below the
//! seam is pure encode/decode with no chat logic in it, and is covered by
//! `wylde-gui-pipe`'s own tests. The seam is drawn at the narrowest place
//! that buys determinism.
//!
//! ## Hermeticity
//!
//! Nothing here touches the developer's live stack (the `#83` self-collision
//! class):
//!
//! * both fixture services bind **private, pid-keyed pipe names**, so a
//!   running Wylde on the same box is untouched and never consulted (the
//!   `#75` lesson: binding a production pipe name fails on any machine
//!   running the product, and CI cannot see it);
//! * `WYLDE_DATA_DIR` is redirected to a per-run temp dir, so conversation
//!   docs, memory stores and config are this binary's own;
//! * `WYLDE_HARNESS_OLLAMA_SERVICE` / `WYLDE_HARNESS_WORKSPACES_SERVICE` are
//!   set explicitly rather than inherited, so an ambient dev shell
//!   (`WYLDE_ROOT` / `WYLDE_SERVICES`) cannot steer the turn at the live
//!   install.
//!
//! ## Coverage enforcement (the point of [`COVERED`])
//!
//! A chat entry point added later must not silently go untested. Two layers:
//!
//! 1. **Compile time, here.** [`spec`] matches `ChatScope` **exhaustively**
//!    with no `_` arm. A new variant stops this test binary compiling, which
//!    reds `cargo panel-walk` — the required `gui panel-walk (L7)` gate.
//! 2. **Source scan, in `wylde_check`.** Rule 57
//!    (`chat_surfaces_are_e2e_covered`) reads the `ChatScope` variants out of
//!    `chat_panel.rs` and the [`COVERED`] entries out of this file and fails
//!    when they disagree — and independently flags any *new* send-capable
//!    chat composer anywhere in the GUI tree, which is the case a `ChatScope`
//!    match cannot see (a new panel growing its own bar adds no variant).
//!
//! [`type_and_send.rs`]: ../type_and_send.rs
//!
//! Windows-only — the fixture inference/workspaces services are named pipes.

#![cfg(windows)]

use gpui::TestAppContext;
use wylde_panel_chat::chat_panel::ChatScope;

mod chat_e2e_harness;

use chat_e2e_harness::{
    case_guard, conversation_id, message_contents, mount, run_turn, STUB_REPLY,
};

// ─────────────────────────────────────────────────────────────────────────
// The covered-surface registry
// ─────────────────────────────────────────────────────────────────────────

/// One GUI chat entry point, as this test drives it.
#[derive(Clone, Copy, Debug)]
struct SurfaceSpec {
    /// Which `ChatPanel` entity backs the surface. The two scopes are
    /// *separate process-wide singletons* (C1), not two views of one panel,
    /// which is exactly why each needs its own end-to-end case.
    scope: ChatScope,
    /// Human name, used in assertion messages so a red case names its surface.
    label: &'static str,
    /// The workspace this surface is scoped into before the send. `None` is
    /// the structurally-unbound Global slot (D1) — there is no escape hatch
    /// to bind it, so its turns must never carry a workspace.
    workspace: Option<&'static str>,
    /// Whether the turn is expected to reach the workspaces service. This is
    /// the chat/memory scoping model's observable: a *bound* turn gathers
    /// workspace context, an *unbound* one structurally cannot.
    expects_workspace_context: bool,
}

/// Every chat surface, keyed by the scope that defines it.
///
/// **This match is a coverage trip-wire.** It is deliberately exhaustive with
/// no `_` arm: adding a `ChatScope` variant makes this file stop compiling,
/// which reds `cargo panel-walk`. The fix is to give the new surface a spec
/// here and an entry in [`COVERED`] — i.e. to actually cover it.
const fn spec(scope: ChatScope) -> SurfaceSpec {
    match scope {
        // The Chat panel slot. Unbound by construction: `resolve_workspace_id`
        // forces `None` regardless of any field, so a global turn can never
        // ride a workspace context.
        ChatScope::Global => SurfaceSpec {
            scope: ChatScope::Global,
            label: "global Chat panel",
            workspace: None,
            expects_workspace_context: false,
        },
        // The Workspaces IDE's docked InferenceBar (`InferenceBarDock`), and
        // with it the per-workspace conversations the dock's list selects
        // between — they are threads *inside* this entity, so the composer,
        // the send path and the scope under test are these.
        ChatScope::Docked => SurfaceSpec {
            scope: ChatScope::Docked,
            label: "Workspaces InferenceBar dock",
            workspace: Some("ws-e2e-alpha"),
            expects_workspace_context: true,
        },
    }
}

/// The surfaces this test actually exercises. Rule 57 cross-checks this list
/// against the `ChatScope` variants declared in `chat_panel.rs`.
const COVERED: &[SurfaceSpec] = &[spec(ChatScope::Global), spec(ChatScope::Docked)];

/// Source files that own a **send-capable chat composer** — a text input whose
/// Enter reaches the chat turn path — and are therefore accounted for by the
/// surfaces in [`COVERED`].
///
/// Rule 57 scans the whole GUI tree for such composers and compares the result
/// against this list. A new panel growing its own chat bar is invisible to the
/// [`spec`] match (it adds no `ChatScope` variant), so this is the half of the
/// enforcement that catches it: the rule goes red until the new composer is
/// listed here *and* given a covered surface.
///
/// Paths are repo-relative with forward slashes. Read verbatim by the rule —
/// keep one entry per line.
const COVERED_COMPOSER_FILES: &[&str] = &["Core/GUI/Frontend/Panels/Chat/src/chat_panel.rs"];
// ─────────────────────────────────────────────────────────────────────────
// The per-surface end-to-end cases
// ─────────────────────────────────────────────────────────────────────────

/// **The core deliverable.** For EVERY covered surface: a user types into that
/// surface's composer, presses Enter, and the reply comes back into that same
/// surface — having crossed the real transport and the real turn driver.
///
/// Both surfaces run in one `#[gpui::test]` so the loop is over [`COVERED`]
/// and cannot skip an entry: adding a surface adds a case here automatically.
#[gpui::test]
fn every_chat_surface_completes_a_turn_end_to_end(cx: &mut TestAppContext) {
    let (_guard, fixture) = case_guard();

    for surface in COVERED {
        fixture.inference_requests.lock().unwrap().clear();
        fixture.workspace_calls.lock().unwrap().clear();

        let window = mount(cx, surface.scope, surface.workspace);
        let question = format!("e2e probe for the {} surface", surface.label);
        let reply = run_turn(cx, &window, &question, surface.label);

        // (1) THE REPLY SURFACED — in this surface's own message log.
        assert_eq!(
            reply, STUB_REPLY,
            "{}: the assistant bubble must render the inference reply; \
             getting something else means the turn broke between the driver \
             and this surface's bubble",
            surface.label
        );

        // (2) THE MESSAGE REACHED THE TURN DRIVER — not a shortcut. The only
        // way the typed text can appear in an `ollama.chat_stream` body is if
        // the driver built the request from it.
        let requests = fixture.inference_requests.lock().unwrap().clone();
        assert!(
            !requests.is_empty(),
            "{}: the turn driver never called inference — the turn did not \
             traverse the driver",
            surface.label
        );
        assert!(
            requests
                .iter()
                .any(|r| message_contents(r).iter().any(|c| c.contains(&question))),
            "{}: the typed text never reached the model-facing messages; \
             bodies were {requests:?}",
            surface.label
        );

        // (3) CORRECT SCOPING FOR THIS SURFACE. The chat/memory scoping model:
        // a *bound* (workspace) surface gathers workspace context; the Global
        // slot is structurally unbound (D1) and must not.
        let workspace_calls = fixture.workspace_calls.lock().unwrap().clone();
        if surface.expects_workspace_context {
            assert!(
                !workspace_calls.is_empty(),
                "{}: a workspace-bound turn must gather workspace context, but \
                 the workspaces service was never consulted — the workspace_id \
                 did not survive to the driver",
                surface.label
            );
            let ws = surface
                .workspace
                .expect("a bound surface names a workspace");
            assert!(
                workspace_calls
                    .iter()
                    .any(|(_, p)| p.to_string().contains(ws)),
                "{}: the workspace reads must carry {ws}; saw {workspace_calls:?}",
                surface.label
            );
        } else {
            assert!(
                workspace_calls.is_empty(),
                "{}: the Global slot is structurally unbound (D1) — a turn from \
                 it must never gather workspace context, but the workspaces \
                 service saw {workspace_calls:?}",
                surface.label
            );
        }
    }
}

/// Per-chat scoping, end-to-end: every surface holds its **own** conversation,
/// and consecutive turns on one surface stay on it.
///
/// This is the "per-chat" half of the scoping model as the GUI owns it. The
/// conversation id the panel ends up holding is the one the *driver* echoed
/// back on `chat.start_turn` — so a stable id across two turns, and distinct
/// ids across the two surfaces, can only happen if each surface's turns really
/// were threaded through the driver on its own thread.
///
/// Note on what this deliberately does NOT assert: that the follow-up turn
/// carries the first exchange in its model-facing messages. It does not, and
/// that is a property of the harness, not of this wiring — nothing on the Rust
/// turn path appends to a conversation's `messages`, so `load_history` has
/// nothing to read (see the issue filed alongside #236). Asserting it here
/// would be asserting a behaviour the product does not have.
#[gpui::test]
fn each_surface_threads_its_own_conversation(cx: &mut TestAppContext) {
    let (_guard, _fixture) = case_guard();

    let mut ids_by_surface = Vec::new();

    for surface in COVERED {
        let window = mount(cx, surface.scope, surface.workspace);

        let first = format!("my first question on {}", surface.label);
        assert_eq!(
            run_turn(cx, &window, &first, surface.label),
            STUB_REPLY,
            "{}: first turn",
            surface.label
        );
        let after_first = conversation_id(cx, &window);
        assert!(
            !after_first.is_empty(),
            "{}: the driver must hand back a conversation id for the turn",
            surface.label
        );

        let second = format!("my follow-up on {}", surface.label);
        assert_eq!(
            run_turn(cx, &window, &second, surface.label),
            STUB_REPLY,
            "{}: second turn",
            surface.label
        );
        assert_eq!(
            conversation_id(cx, &window),
            after_first,
            "{}: a follow-up must stay on the same thread, not silently open a \
             new one",
            surface.label
        );

        ids_by_surface.push((surface.label, after_first));
    }

    for (i, (label, id)) in ids_by_surface.iter().enumerate() {
        for (other_label, other_id) in &ids_by_surface[i + 1..] {
            assert_ne!(
                id, other_id,
                "{label} and {other_label} must hold independent conversations \
                 — sharing one would leak each surface's chat into the other"
            );
        }
    }
}

/// The two surfaces are independent entities (C1), so a turn on one must not
/// land in the other's log. Mounted together — the shipped arrangement — and
/// only one is sent from.
#[gpui::test]
fn a_turn_on_one_surface_does_not_leak_into_the_other(cx: &mut TestAppContext) {
    let (_guard, _fixture) = case_guard();

    let global = mount(cx, ChatScope::Global, None);
    let docked = mount(cx, ChatScope::Docked, spec(ChatScope::Docked).workspace);

    let sent = "this belongs to the dock alone";
    assert_eq!(run_turn(cx, &docked, sent, "dock"), STUB_REPLY);

    let global_log = global
        .update(cx, |panel, _w, _cx| {
            panel
                .messages
                .iter()
                .map(|m| m.content.clone())
                .collect::<Vec<_>>()
        })
        .unwrap();
    assert!(
        global_log.is_empty(),
        "a dock turn must not appear in the Global slot's log — the two are \
         separate entities with separate conversations (C1); saw {global_log:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// Coverage-registry integrity
// ─────────────────────────────────────────────────────────────────────────

/// Guards the registry itself: every covered entry must be internally
/// consistent, and no two entries may claim the same scope (which would let a
/// surface be silently dropped while the count still looked right).
#[test]
fn the_covered_registry_is_well_formed() {
    assert!(
        !COVERED_COMPOSER_FILES.is_empty(),
        "the composer-file declaration rule 57 reads must never be emptied — \
         an empty list would make the source scan vacuously pass"
    );
    for (i, s) in COVERED.iter().enumerate() {
        assert_eq!(
            s.workspace.is_some(),
            s.expects_workspace_context,
            "{}: a surface gathers workspace context iff it binds a workspace",
            s.label
        );
        assert_eq!(
            s.scope.allows_workspace_bind(),
            s.workspace.is_some(),
            "{}: the spec's binding must match what ChatScope actually permits",
            s.label
        );
        for other in &COVERED[i + 1..] {
            assert_ne!(
                s.scope, other.scope,
                "two covered entries claim the same scope ({} / {}) — one \
                 surface would go untested while the list still looked full",
                s.label, other.label
            );
        }
    }
}

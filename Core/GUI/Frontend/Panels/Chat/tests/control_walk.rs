//! L7 **control**-walk — Chat, the largest surface in the tree (issue #247).
//!
//! Chat renders one `ChatPanel` whose controls come from two source files, and
//! painting the panel paints both sets at once — so they are walked together
//! here rather than split across two fixtures that would each have to arm the
//! other's preconditions:
//!
//! * **chrome** (`chat_panel.rs`) — the InferenceBar send/stop, the model
//!   eject, the reasoning/model/workspace/conversation toggles, the
//!   processing indicator, the working-memory clear, and the conversation
//!   switcher (new / import / per-conversation pick / export / delete +
//!   inline delete-confirm).
//! * **composer** (`composer_ui.rs`) — the per-word chip strip + context
//!   chip, the floating thought-bubble strip (bubbles / expanded card /
//!   right-click menu), the disambiguation dropdown, the anchor offer, the
//!   3-tier ignore menu, the curate-before-send popover, and the Ctrl+P
//!   symbol palette.
//!
//! Every composer surface is state-gated (nothing paints on an empty
//! composer), so the fixture seeds `composer` (two recognized words, one
//! ambiguous) and `bubbles` (a symbol + an anchor bubble) in `reset`, and each
//! named state opens exactly one overlay. `reset` closes every dropdown and
//! overlay before every click so an absolute-positioned one can't sit over a
//! later target.

use gpui::TestAppContext;

use serde_json::json;

use wylde_gui_test_support::control_walk::{ControlWalk, WalkReport};
use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_chat::chat_panel::{ChatMessage, ChatScope, MessageRole};
use wylde_panel_chat::composer::bubbles::{Bubble, BubbleKind};
use wylde_panel_chat::composer::tokenizer::{TokenKind, TokenSpan};
use wylde_panel_chat::composer::{PaletteState, SymbolCandidate, WordRecognition};
use wylde_panel_chat::ipc::WorkingMemoryEntry;
use wylde_panel_chat::processing::{ProcessingPhase, ProcessingState};
use wylde_panel_chat::ChatPanel;

// ── Composer fixture builders ────────────────────────────────────────────

fn sym(id: &str) -> SymbolCandidate {
    SymbolCandidate {
        id: id.to_string(),
        name: format!("{id}_fn"),
        kind: "function".to_string(),
        file: "src/lib.rs".to_string(),
        line: 10,
        module_path: "crate".to_string(),
        score: 1.0,
    }
}

fn token(text: &str) -> TokenSpan {
    TokenSpan {
        text: text.to_string(),
        start: 0,
        end: text.len(),
        kind: TokenKind::Identifier,
    }
}

/// Two symbols, no pick → `is_ambiguous`: drives the `?2` chip + dropdown.
fn ambiguous_word(text: &str) -> WordRecognition {
    let mut w = WordRecognition::new(token(text));
    w.candidates = vec![sym("a1"), sym("a2")];
    w
}

/// One candidate → `effective_symbol` is `Some`: drives the anchor offer.
fn resolved_word(text: &str) -> WordRecognition {
    let mut w = WordRecognition::new(token(text));
    w.candidates = vec![sym("s1")];
    w
}

fn symbol_bubble() -> Bubble {
    Bubble {
        label: "s1_fn".to_string(),
        kind: BubbleKind::Symbol {
            id: "s1".to_string(),
            file: "src/lib.rs".to_string(),
            line: 10,
        },
    }
}

fn anchor_bubble() -> Bubble {
    Bubble {
        label: "an_anchor".to_string(),
        kind: BubbleKind::Anchor {
            description: "an anchor".to_string(),
        },
    }
}

// ── Chrome fixture ───────────────────────────────────────────────────────

fn conv(id: &str) -> serde_json::Value {
    json!({ "id": id, "workspace_id": "ws-a", "updated_at": 0, "title": "t" })
}

fn healthy() -> std::sync::Arc<ScriptedBackend> {
    ScriptedBackend::new()
        .conversations(vec![conv("c1")])
        .on(
            "chat.start_turn",
            serde_json::json!({ "turn_id": "t1", "conversation_id": "c1" }),
        )
        .on("chat.cancel", serde_json::json!({ "ok": true }))
        .on("models.eject", serde_json::json!({ "ok": true }))
        .on("conversations.new", serde_json::json!({ "id": "c2" }))
}

fn processing() -> ProcessingState {
    let mut p = ProcessingState::new();
    p.set_phase(ProcessingPhase::Working);
    // A logged step so `has_detail` is true: the indicator's expand affordance
    // (its only control — a chevron that toggles `expanded`) is attached only
    // when there is detail to show.
    p.on_step("retrieval", "Retrieved 8 snippets", None);
    p
}

fn mount(cx: &mut TestAppContext) -> gpui::WindowHandle<ChatPanel> {
    let window = cx.add_window(|_w, cx| {
        let panel = ChatPanel::new(ChatScope::Docked, cx);
        ChatPanel::spawn_load_workspaces(cx);
        ChatPanel::spawn_load_models(cx);
        panel
    });
    cx.run_until_parked();
    window
        .update(cx, |panel, _w, cx| {
            // A bound workspace so `create_anchor_for_word` clears the offer
            // (instead of stashing a "no workspace" error), and the workspace
            // dropdown is reachable on this Docked surface.
            panel.apply_workspace_scope(Some("ws-a".to_owned()), cx);
            // Arm the prompt input so send (which no-ops on an empty composer)
            // has something to send. Set once here, not per click.
            panel
                .prompt_input
                .update(cx, |i, cx| i.set_text("hello", cx));
        })
        .unwrap();
    cx.run_until_parked();
    window
}

fn fingerprint(p: &ChatPanel) -> String {
    // Per-word recognition state moved by the chip / disambig / ignore /
    // curation / bubble-exclude controls.
    let words: String = p
        .composer
        .words
        .iter()
        .map(|w| {
            format!(
                "[{}|res={:?}|ex={}|re={}|ign={}|anc={}|cand={}]",
                w.token.text,
                w.resolved,
                w.excluded,
                w.reactivated,
                w.ignored_tiers.len(),
                w.anchor_count,
                w.candidates.len(),
            )
        })
        .collect();
    format!(
        // chrome fields, then composer fields
        "msgs={} turn={:?} processing={:?} model={:?} ejecting={} wm={} convs={} \
         show_conv={} show_wm={} show_ws={} show_model={} reasoning={:?} confirm_del={:?} \
         words={words} disambig={:?} anchor={:?} ignore={:?} curating={} \
         palette={} psel={:?} bwidx={:?} bubbles={} bexp={:?} bmenu={:?} pins={} err={}",
        p.messages.len(),
        p.active_turn_id,
        // Some(expanded) — presence AND the flag the processing row's chevron
        // toggles, so that click registers an effect.
        p.processing.as_ref().map(|pr| pr.expanded),
        p.active_model,
        p.ejecting,
        p.working_memory.len(),
        p.conversations.len(),
        p.show_conversations,
        p.show_working_memory,
        p.show_ws_dropdown,
        p.show_model_dropdown,
        p.reasoning_depth,
        p.confirm_delete,
        p.composer.disambiguating,
        p.composer.anchor_offer,
        p.composer.ignore_menu,
        p.composer.curating,
        p.composer.palette.is_some(),
        p.composer.palette.as_ref().map(|pl| pl.selected),
        p.bubbles.word_idx,
        p.bubbles.bubbles.len(),
        p.bubbles.expanded,
        p.bubbles.menu,
        p.bubbles.pinned.len(),
        p.error.is_some(),
    )
}

fn walk(
    cx: &mut TestAppContext,
    window: gpui::WindowHandle<ChatPanel>,
    fake: &std::sync::Arc<ScriptedBackend>,
) -> WalkReport {
    ControlWalk::new(window, fake)
        .fingerprint(fingerprint)
        .reset(|p: &mut ChatPanel, _w, cx| {
            // ── chrome base ──
            p.active_turn_id = None;
            p.processing = None;
            p.ejecting = false;
            // Close every dropdown between clicks so an opened one can't sit
            // over a later target.
            p.show_conversations = false;
            p.show_working_memory = false;
            p.show_ws_dropdown = false;
            p.show_model_dropdown = false;
            p.confirm_delete = None;
            // A model is loaded so the eject control is enabled in the default
            // frame; working memory has an entry so the clear control paints.
            p.active_model = Some("llama3:8b".to_string());
            if p.working_memory.is_empty() {
                p.working_memory.push(WorkingMemoryEntry {
                    kind: "note".to_string(),
                    summary: "a working-memory item".to_string(),
                });
            }
            // ── composer base ──
            // Word 0 ambiguous (drives disambiguation), word 1 single-candidate
            // (drives the anchor offer + a count chip); both recognized, so the
            // curation popover lists both. Bubbles pre-seeded but the strip
            // stays hidden (`word_idx = None`) until a bubble state opens it.
            p.composer.words = vec![ambiguous_word("foo"), resolved_word("bar")];
            p.composer.disambiguating = None;
            p.composer.anchor_offer = None;
            p.composer.ignore_menu = None;
            p.composer.curating = false;
            p.composer.palette = None;
            p.bubbles.word_idx = None;
            p.bubbles.bubbles = vec![symbol_bubble(), anchor_bubble()];
            p.bubbles.expanded = None;
            p.bubbles.menu = None;
            p.bubbles.pinned.clear();
            p.error = None;
            cx.notify();
        })
        // ── chrome states ──
        // A turn in flight — the send button becomes Stop.
        .state("streaming", |p: &mut ChatPanel, _w, cx| {
            p.active_turn_id = Some("t1".to_string());
            cx.notify();
        })
        // The processing indicator is folded onto the *in-flight assistant tail
        // bubble* — it paints only for a message with `role == Assistant &&
        // streaming`, plus `processing` set. The row lives in a virtualized
        // `list` that follows the tail and builds only the visible slice, so the
        // streaming message must BE the tail. Install exactly one such message
        // (unconditionally — a conversation auto-loaded on mount would otherwise
        // leave a non-streaming tail and the indicator would never paint), reset
        // the reconciler to it, and pin follow-mode so item 0 is on screen.
        .state("processing", |p: &mut ChatPanel, _w, cx| {
            p.messages = vec![ChatMessage {
                id: "m1".to_string(),
                role: MessageRole::Assistant,
                content: "working".to_string(),
                thinking: None,
                streaming: true,
                activity: None,
                activity_expanded: false,
            }];
            p.message_list.reset(1);
            p.message_list.set_follow_mode(gpui::FollowMode::Tail);
            p.processing = Some(processing());
            cx.notify();
        })
        // The working-memory strip (its "clear" control) shows only when open.
        .state("wm-open", |p: &mut ChatPanel, _w, cx| {
            p.show_working_memory = true;
            cx.notify();
        })
        // The conversation switcher (new / import / pick / export / delete).
        .state("conversations-open", |p: &mut ChatPanel, _w, cx| {
            p.show_conversations = true;
            p.confirm_delete = None;
            cx.notify();
        })
        // The inline delete-confirmation (its yes/no) shows only once a
        // conversation delete has been requested.
        .state("delete-confirm-pending", |p: &mut ChatPanel, _w, cx| {
            p.show_conversations = true;
            p.confirm_delete = Some("c1".to_string());
            cx.notify();
        })
        // ── composer states ──
        // The `?N` disambiguation dropdown for the ambiguous word.
        .state("disambig", |p: &mut ChatPanel, _w, cx| {
            p.composer.disambiguating = Some(0);
            cx.notify();
        })
        // "Anchor this?" offer for the single-candidate word.
        .state("anchor-offer", |p: &mut ChatPanel, _w, cx| {
            p.composer.anchor_offer = Some(1);
            cx.notify();
        })
        // The 3-tier right-click ignore menu.
        .state("ignore-menu", |p: &mut ChatPanel, _w, cx| {
            p.composer.ignore_menu = Some(0);
            cx.notify();
        })
        // Curate-before-send popover (one row per recognized word).
        .state("curating", |p: &mut ChatPanel, _w, cx| {
            p.composer.curating = true;
            cx.notify();
        })
        // Ctrl+P symbol palette with one hit.
        .state("palette", |p: &mut ChatPanel, _w, cx| {
            p.composer.palette = Some(PaletteState {
                query: "foo".to_string(),
                hits: vec![sym("p1")],
                selected: 0,
                generation: 0,
            });
            cx.notify();
        })
        // The floating bubble strip (bubbles + collapse) for the open word.
        .state("bubbles", |p: &mut ChatPanel, _w, cx| {
            p.bubbles.word_idx = Some(0);
            cx.notify();
        })
        // One bubble expanded into its drill-in card (pin / exclude / view in
        // graph). Bubble 0 is a Symbol, so "view in graph" paints.
        .state("bubble-card", |p: &mut ChatPanel, _w, cx| {
            p.bubbles.word_idx = Some(0);
            p.bubbles.expanded = Some(0);
            cx.notify();
        })
        // The shared right-click bubble menu (one row per routed action).
        .state("bubble-menu", |p: &mut ChatPanel, _w, cx| {
            p.bubbles.word_idx = Some(0);
            p.bubbles.menu = Some(0);
            cx.notify();
        })
        // Native-dialog controls: the workspace folder picker and the
        // conversation import/export file dialogs (`rfd`). A headless test
        // cannot open or drive an OS dialog, so these clicks produce no
        // observable delta — live controls with an external effect, not dead
        // ones. Clicked for panic-safety, not asserted.
        .external_effect(&[
            "chat-ws-pick",
            "chat-conversation-import",
            "chat-conversation-export::c1",
        ])
        .sources(&[
            include_str!("../src/chat_panel.rs"),
            include_str!("../src/composer_ui.rs"),
        ])
        .run(cx)
}

#[gpui::test]
fn every_chat_control_does_something_when_clicked(cx: &mut TestAppContext) {
    let fake = healthy();
    let _guard = fake.clone().install();
    let window = mount(cx);

    walk(cx, window, &fake)
        .assert_every_control_lives()
        .assert_covers_every_literal_id();
}

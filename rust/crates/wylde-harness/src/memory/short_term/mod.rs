//! `short_term/` — conversation-scoped working memory (Layer 3 of the
//! memory architecture). Rust port of the working-memory surface of
//! `Core/harness/memory/conversation.py`.
//!
//! Working memory is the rolling buffer of tool calls / files opened /
//! decisions reached / summaries read that the chat-turn driver accrues
//! as it works, so a re-opened conversation doesn't re-do work it
//! already did. It is NOT a tier of its own on disk — it lives as the
//! `working_memory` array inside each conversation document
//! (`<conversations_dir>/<id>.json`). See [`store`] for the storage
//! contract and why this module ports only the working-memory verbs (the
//! broader conversation surface — `conversations.*`, `memory.reflect` —
//! stays on Python for now).
//!
//! Three pipe verbs land here, registered unconditionally in
//! [`crate::pipe::install_all_against`] (they're a cutover of three
//! Python verbs that previously fell through to the strangler as
//! `no_action`):
//!
//! * `memory.short_term.get`    — read the buffer.
//! * `memory.short_term.append` — append one entry.
//! * `memory.short_term.clear`  — drop the buffer.

pub mod actions;
pub mod store;

#[cfg(test)]
mod test_support;

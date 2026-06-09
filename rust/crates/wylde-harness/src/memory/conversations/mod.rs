//! `conversations/` — conversation-document lifecycle (mint / list /
//! read / delete) + active-conversation persistence. Rust port of the
//! conversation-listing half of `Core/harness/memory/conversation.py`.
//!
//! This is the sibling of [`super::short_term`], which ports the
//! *working-memory* half of the same `conversation.py` file. Together
//! they cover the whole pipe-verb surface of that module; the Python
//! `conversation.py` itself stays load-bearing only for `memory.reflect`.
//!
//! Six pipe verbs land here, registered unconditionally in
//! [`crate::pipe::install_all_against`] (a cutover of the four Python
//! `conversations.*` verbs that previously fell through to the strangler
//! as `no_action`, plus the net-new `get_active`/`set_active` persistence
//! pair Slice B's switcher uses to remember the user's selection):
//!
//! * `conversations.new`        — mint a fresh id.
//! * `conversations.list`       — metadata for every saved chat.
//! * `conversations.get`        — the full conversation document.
//! * `conversations.delete`     — remove a conversation file.
//! * `conversations.get_active` — read the persisted active selection.
//! * `conversations.set_active` — persist the active selection.

pub mod actions;
pub mod store;

#[cfg(test)]
pub(crate) mod test_support;

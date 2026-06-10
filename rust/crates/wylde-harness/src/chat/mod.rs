//! `chat/` — the chat-surface harness modules that sit above the turn
//! driver.
//!
//! **Conceptual path:** `Core/Harness/chat/`.
//!
//! The Thought Bubble System Build Order (§3) groups the chat-facing
//! harness code under `chat/`:
//!
//! * `chat/turn/`   — the turn driver + context-gather hook (still lives at
//!   the crate root `turn/` for now; its relocation under `chat/` is a
//!   later, mechanical move out of this slice's scope).
//! * [`search`]     — scoped chat-history search tools (**Slice E**).
//! * [`ignore`]     — the global tier of the symbol ignore list (**Slice M**).
//! * [`exchange`]   — conversation export/import dispatch (**Slice J**):
//!   standalone in-process, workspace forwarded to the service.

pub mod exchange;
pub mod ignore;
pub mod search;

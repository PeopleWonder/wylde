//! `workspaces/` — folder-based RAG workspace registry.
//!
//! Rust port of `Core/harness/memory/workspaces/`. Slice 7.A of the
//! Wylde Rust migration ports the *registry-only* half:
//!
//! * [`Workspace`] dataclass — id, path, persona, file_count,
//!   timestamps, indexing flag.
//! * [`store`] — JSON registry IO, list / recent / get / activate
//!   bookkeeping, delete (incl. on-disk index folder + durable
//!   workspace memory folder).
//! * [`mru`] — `mru_limit` get / set, clamping, persistence.
//! * [`slug`] — `slug_for(path)` deterministic id derivation.
//!
//! ## What 7.A does NOT include
//!
//! The Python `_index.py` (`_index_full` / `_index_delta`) and
//! `_search.py` (`search_files`) depend on:
//!
//! 1. LanceDB — no first-class Rust client today; either we add a
//!    LanceDB crate dependency or bridge to Python over IPC.
//! 2. The embedder (`embeddings.py`) which routes through
//!    `wylde-ollama`'s embed action.
//!
//! Both of those choices want their own slice. The Python `activate`
//! / `reindex_workspace` / `refresh_workspace` / `status` /
//! `search_files` stay canonical until 7.B lands.
//!
//! ## Wire surface
//!
//! Slice 7.A registers these actions on the Rust harness pipe (see
//! [`crate::service::install`]) — they're NEW wire surface, distinct
//! from Python's `rag.workspaces.*` actions which stay canonical:
//!
//! * `memory.workspaces.list` — every workspace in MRU order.
//! * `memory.workspaces.recent` — first N (default = MRU cap).
//! * `memory.workspaces.get` — one workspace by id.
//! * `memory.workspaces.get_mru_limit` — current cap + min/max/default.
//! * `memory.workspaces.set_mru_limit` — persist new cap; evict
//!   overflow workspaces (index dir removed, workspace memory
//!   preserved).
//! * `memory.workspaces.get_persona` — persona text for a workspace.
//! * `memory.workspaces.set_persona` — persist persona text.
//! * `memory.workspaces.delete` — explicit delete (registry +
//!   index dir + workspace memory dir).
//!
//! The Python pipe surface (`rag.workspaces.*`) is unchanged and
//! continues to drive the GUI. When 7.B lands the indexing half, the
//! Python handlers grow a strangler-fig forward layer gated on
//! [`crate::memory::impl_for`].

pub mod indexer;
pub mod mru;
pub mod slug;
pub mod store;

pub mod actions;

#[cfg(test)]
mod test_support;

pub use mru::{
    get_mru_limit, set_mru_limit, MRU_LIMIT_DEFAULT, MRU_LIMIT_MAX, MRU_LIMIT_MIN,
};
pub use slug::slug_for;
pub use store::{
    delete_workspace, get_persona, get_workspace, indexes_dir, list_workspaces, recent_workspaces,
    registry_path, set_persona, set_indexing_flag, touch_activated, update_file_count, Workspace,
};

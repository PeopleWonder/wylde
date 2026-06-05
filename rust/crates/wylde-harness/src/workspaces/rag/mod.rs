//! `rag/` — RAG integration for a workspace's folder.
//!
//! **Conceptual path:** `Core/Harness/Workspaces/Rag/`.
//!
//! A workspace *is* a folder (per memory `wylde_rag_workspaces`). This
//! module translates that folder into a RAG query scope so retrieval for
//! a turn anchored to the workspace is bounded to the workspace's files,
//! rather than searching the global index.
//!
//! This is the read/scope side only. The heavy indexing machinery
//! (LanceDB, the embedder) is out of scope for the redesign scaffold and
//! continues to live where it does today (see the design doc's
//! migration section).
//!
//! ## Split
//!
//! * [`scope`] — folder → [`WorkspaceRagScope`] translation + the
//!   retrieval entrypoint the prompt builder calls.

pub mod scope;

pub use scope::WorkspaceRagScope;

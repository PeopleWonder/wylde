//! Fetch + score workspace-memory entries for prompt injection.
//!
//! The prompt builder ([`super::super::prompt`]) asks this module for
//! the top-K entries to inject as the workspace-memory slot. Scoring is
//! a recency + (optionally) relevance blend — the exact formula is an
//! open question for the design review.
//!
//! Scaffold only — no logic yet.

use super::entry::WorkspaceMemoryEntry;

/// A request for the most relevant workspace-memory entries to inject.
#[derive(Clone, Debug)]
pub struct WorkspaceMemoryQuery {
    /// Which workspace's bucket to read.
    pub workspace_id: String,

    /// The current user message — used for relevance scoring if/when an
    /// embedding pass is added. May be empty for a pure-recency fetch.
    pub user_message: String,

    /// Max entries to inject.
    pub limit: usize,
}

impl WorkspaceMemoryQuery {
    /// Default injection budget for the workspace-memory slot.
    pub const DEFAULT_LIMIT: usize = 5;
}

/// Return the top entries to inject for `query`, highest-scoring first.
///
/// TODO: load via [`super::entry::load`], score (recency + relevance),
/// truncate to `query.limit`.
pub fn top_entries(_query: &WorkspaceMemoryQuery) -> Vec<WorkspaceMemoryEntry> {
    todo!("workspaces redesign: top_entries")
}

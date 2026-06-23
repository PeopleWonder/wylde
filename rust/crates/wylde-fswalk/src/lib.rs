//! `wylde-fswalk` — the shared filesystem-detection library.
//!
//! Pure detection *logic*, no storage. Extracted out of the hot
//! `wylde-workspaces` RAG indexer so a second consumer — the
//! `wylde-organize` Service — can reuse the exact same "what is junk / what is
//! a duplicate / what would the walk see" detectors **without** cross-importing
//! `wylde-workspaces` internals (which `wylde_check` rule 26 forbids: Wylde
//! crates depend on each other only via a shared crate, and this is that shared
//! crate for the detection surface).
//!
//! ## What lives here
//!
//! * [`exclude::ExclusionMatcher`] — the layered exclusion predicate:
//!   `.git` (hard) → `.wyldeignore` (user override) → built-in artifact
//!   deny-list → nested `.gitignore`. One predicate, four layers, one
//!   precedence. The RAG walk and the organizer's junk-detection both consult
//!   it, so they agree byte-for-byte on what's an artifact.
//! * [`stats`] — the metadata-only walk ([`stats::walk_file_stats`] →
//!   [`stats::FileStat`] `{path, mtime, size}`, **mtime only, no atime**), the
//!   per-file content fingerprint ([`stats::hash_file`] = sha256 truncated to
//!   16 hex chars), the canonical-path helper, the path pre-filter
//!   ([`stats::is_indexable_path`]), and the read-only preview
//!   ([`stats::walk_preview`]).
//! * [`dedup`] — a content-hash duplicate grouper ([`dedup::group_duplicates`]
//!   / [`dedup::find_duplicates_under`]) that buckets files by identical
//!   content. Net-new: the workspaces manifest does *change-detection*, never
//!   dedup, so no existing code grouped by hash.
//!
//! Nothing in this crate persists anything or talks to a service — every
//! function is pure over a path. Reusing it is importing a library, not sharing
//! a database.

pub mod dedup;
pub mod exclude;
pub mod stats;

// Flat re-exports so a consumer can `use wylde_fswalk::{ExclusionMatcher,
// FileStat, walk_file_stats, hash_file}` without threading the module path.
pub use dedup::{find_duplicates_under, group_duplicates, DuplicateGroup};
pub use exclude::ExclusionMatcher;
pub use stats::{
    canonical_path, hash_file, is_indexable_path, mtime_secs, walk_file_stats, walk_preview,
    FileStat, WalkPreview, SKIP_SUFFIXES,
};

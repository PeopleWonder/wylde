//! Walk-time exclusion — **re-export shim**.
//!
//! The `ExclusionMatcher` (the layered `.git` → `.wyldeignore` → deny-list →
//! nested-`.gitignore` predicate) was extracted into the shared
//! [`wylde_fswalk`] crate (file-organizer build, 2026-06-23) so the
//! `wylde-organize` Service can reuse the identical junk-detection logic
//! without cross-importing this crate's internals (`wylde_check` rule 26).
//!
//! This module re-exports it so every existing `super::exclude::ExclusionMatcher`
//! reference inside the indexer keeps resolving unchanged — the extraction is a
//! pure move + dep-swap, no behaviour change. See `wylde-fswalk/src/exclude.rs`
//! for the implementation and its tests.

pub use wylde_fswalk::exclude::ExclusionMatcher;

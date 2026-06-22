//! Curate-before-inject (concept-routing plan §4, requirement 2; relation-model
//! addendum §4) — the explainable menu the user curates before any injection,
//! and the apply step that turns their choices into the final inject set.
//!
//! Both halves are **pure** (no I/O, no embed): [`candidate`] reshapes a settled
//! [`CandidateSet`](crate::router::CandidateSet) into the menu payload
//! ([`CuratedMenu`]); [`apply`] diffs the user's checked set against the routed
//! candidates, enforces the token budget (evict lowest-activation first), and
//! yields the [`InjectionPlan`]. The impure injection (blurb + member snippets)
//! lives server-side in the workspaces bridge — this module only decides *what*
//! to inject.

pub mod apply;
pub mod candidate;

pub use apply::{apply_curation, InjectionPlan};
pub use candidate::{CuratedMenu, MenuAnnotation, MenuItem, MenuItemKind};

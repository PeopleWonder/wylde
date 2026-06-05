//! `persona/` — optional per-workspace persona override.
//!
//! **Conceptual path:** `Core/Harness/Workspaces/Persona/`.
//!
//! A persona is a system-prompt modifier scoped to a workspace (e.g.
//! "answer as a Rust reviewer; prefer terse diffs"). Stored as
//! `<data_dir>/workspaces/<workspace_id>/persona.md` — plain markdown so
//! a user can hand-edit it. The prompt builder ([`super::prompt`]) folds
//! it in only when [`super::registry::WorkspaceDefinition::persona_enabled`]
//! is set.
//!
//! This supersedes the `persona` string field that the old verb-driven
//! the retired `memory::workspaces::store::Workspace` carried inline; see
//! the migration section of the design doc.
//!
//! ## Split
//!
//! * [`template`] — load the persona file + render it into the form the
//!   prompt builder consumes ([`PersonaOverride`]).

pub mod template;

pub use template::{load, persona_path, save, PersonaOverride};

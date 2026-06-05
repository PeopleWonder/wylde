//! `workspaces/` — config-file-backed workspaces (redesign).
//!
//! **Conceptual path:** `Core/Harness/Workspaces/` in Aaron's
//! Python-style mental model. The actual Rust home is this module,
//! `crate::workspaces`, per the everything-Rust direction.
//!
//! ## What this is (and what it replaces)
//!
//! A workspace is **configuration that shapes prompt building**, not a
//! service with a CRUD verb surface. The prior verb-driven design
//! (`rag.workspaces.*` + the registry-only port in
//! the now-retired `crate::memory::workspaces`) treated workspaces like a mini
//! database. The redesign reframes them as config files the harness
//! reads: the turn driver picks up `workspace_id` off `chat.run_turn`
//! and the prompt builder injects that workspace's context — RAG scope,
//! persona, and the workspace-layer memory tier — into the system
//! prompt.
//!
//! See `docs/plans/workspaces-redesign-2026-06-04.md` for the full
//! design, migration plan, and open questions. This module is a
//! **scaffold only** — types + signatures + TODOs, no logic yet.
//!
//! ## Submodule map
//!
//! Each submodule maps to a `Core/Harness/Workspaces/<Subfolder>/` in
//! the conceptual layout:
//!
//! * [`registry`] — the **store of workspace configurations** (Aaron's
//!   "workspaces memory of some sort that stores each workspace
//!   configuration"). Owns `workspaces.json` + `state.json`. This is the
//!   *config* tier; do not confuse it with [`memory`] below.
//! * [`memory`] — the **per-workspace memory-entries layer**: the middle
//!   tier of the 3-layer memory architecture (long-term / **workspace**
//!   / short-term). One bucket of curated entries per workspace, fetched
//!   and scored for prompt injection.
//! * [`persona`] — optional per-workspace persona override (a system
//!   prompt modifier stored as `persona.md`).
//! * [`rag`] — translates a workspace's folder into a RAG query scope so
//!   retrieval is bounded to the workspace's files.
//! * [`prompt`] — assembles this workspace's contribution to a chat
//!   turn's system prompt from persona + memory + RAG.
//! * [`api`] — the minimal IPC verb surface (writes + active-selection;
//!   reads go straight to `workspaces.json` from the GUI).
//!
//! ## Why two "memory" concepts live side by side
//!
//! `registry` is storage-of-configs; `memory` is the memory *tier*.
//! Aaron uses "memory" loosely for both. The names here are chosen so
//! the code reads unambiguously: **registry = configs, memory =
//! entries.**

pub mod api;
pub mod memory;
pub mod persona;
pub mod prompt;
pub mod rag;
pub mod registry;

#[cfg(test)]
mod test_support;

// ── Public surface (re-exported so the scaffold is part of the crate's
// public API — keeps `cargo check` warning-free while the bodies are
// still TODO) ──────────────────────────────────────────────────────────

pub use memory::{WorkspaceMemoryEntry, WorkspaceMemoryQuery};
pub use persona::PersonaOverride;
pub use prompt::WorkspaceContext;
pub use rag::WorkspaceRagScope;
pub use registry::{WorkspaceDefinition, WorkspaceState};

/// End-to-end lifecycle integration test exercising the full
/// create → activate → switch → delete cycle across the verb handlers,
/// registry state machine, on-disk persistence, and prompt assembly.
#[cfg(test)]
mod lifecycle_tests {
    use super::test_support::TestEnv;
    use super::{api, persona, prompt, registry};
    use serde_json::{json, Value};
    use tempfile::TempDir;

    fn make_dir(parent: &TempDir, name: &str) -> String {
        let p = parent.path().join(name);
        std::fs::create_dir(&p).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn full_create_activate_switch_delete_cycle() {
        let _env = TestEnv::new();
        let td = TempDir::new().unwrap();

        // ── create A → registered + active ──────────────────────────
        let a = api::handle_create(json!({ "folder": make_dir(&td, "proj-a"), "name": "Proj A" }))
            .await;
        assert!(a.ok, "create A failed: {:?}", a.error);
        let a_id = a.data["id"].as_str().unwrap().to_owned();
        assert_eq!(registry::state::load().active_id.as_deref(), Some(a_id.as_str()));

        // ── create B → active flips to B, MRU = [B, A] ──────────────
        let b = api::handle_create(json!({ "folder": make_dir(&td, "proj-b") })).await;
        let b_id = b.data["id"].as_str().unwrap().to_owned();
        let listed = api::handle_list_mru(Value::Null).await;
        let mru: Vec<&str> = listed.data["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w["id"].as_str().unwrap())
            .collect();
        assert_eq!(mru, vec![b_id.as_str(), a_id.as_str()], "newest-first MRU");
        assert_eq!(listed.data["active_id"], b_id);

        // ── switch back to A (explicit set_active) → MRU = [A, B] ────
        let sw = api::handle_set_active(json!({ "workspace_id": a_id })).await;
        assert!(sw.ok);
        assert_eq!(sw.data["active_id"], a_id);
        let mru2 = api::handle_list_mru(Value::Null).await;
        assert_eq!(mru2.data["workspaces"][0]["id"], a_id);

        // ── give A a persona, confirm a turn's gather injects it ─────
        api::handle_set_persona(json!({ "workspace_id": a_id, "text": "Answer tersely." }))
            .await;
        assert!(registry::get(&a_id).unwrap().persona_enabled);
        assert_eq!(persona::load(&a_id).text, "Answer tersely.");
        let ctx = prompt::inject::gather(&a_id, "what is this project?").await;
        assert_eq!(ctx.persona, "Answer tersely.");
        let slots = prompt::inject::render_slots(&ctx);
        assert!(slots.contains("## Persona"));
        assert!(slots.contains("Answer tersely."));

        // ── delete A → bundle gone, active cleared, only B remains ──
        let del = api::handle_delete(json!({ "workspace_id": a_id })).await;
        assert!(del.ok);
        assert_eq!(del.data["ok"], true);
        assert!(registry::get(&a_id).is_none());
        assert!(!registry::persistence::workspace_dir(&a_id).exists());
        assert!(registry::state::load().active_id.is_none());
        let after = api::handle_list_mru(Value::Null).await;
        let remaining: Vec<&str> = after.data["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w["id"].as_str().unwrap())
            .collect();
        assert_eq!(remaining, vec![b_id.as_str()], "only B survives");

        // ── an unknown / empty workspace gathers nothing ────────────
        assert!(prompt::inject::gather(&a_id, "hi").await.is_empty());
        assert!(prompt::inject::gather("", "hi").await.is_empty());
    }
}

//! In-process tool catalog. Rust port of
//! `Core/harness/tooling/tool_registry/` — but with the manifest scan
//! replaced by a static `register_*` call from each tool module. The
//! filesystem-as-registry pattern Python uses is unnecessary here: Rust
//! tools are compiled in, so the only catalog is the one
//! [`Registry::default`] builds up at init time.
//!
//! ## Entry shape
//!
//! A [`ToolEntry`] carries everything the runner + alias map + tier
//! gate need:
//!
//! * `id` — canonical snake-case form (e.g. `read_file`).
//! * `name` — dotted form (e.g. `fs.read_file`). Used by the model when
//!   it emits a tool call; mirrored to `id` via the alias map.
//! * `group` — top-level grouping for the model's catalog ("fs", "git",
//!   "memory", "meta", …). Mirrors Python's `manifest.group`.
//! * `description` — passed to the LLM in the tool catalog. Pulled from
//!   the Python manifests during the port so wording is identical.
//! * `parameters` — JSON Schema-ish array of `{name, type, required,
//!   description, default}`. Wire-compatible with Python's
//!   `manifest.parameters`.
//! * `destructive` — tier classifier. `true` means the tool is denied
//!   on `tool_use` tier; only `destructive_tool_access` may invoke it.
//! * `kind` — either [`HandlerKind::Active`] (with a closure) or
//!   [`HandlerKind::Deferred`] (with a phase tag explaining why the
//!   handler isn't implemented yet — memory/RAG/visual etc).
//!
//! ## Global registry
//!
//! The harness owns one process-wide [`Registry`] built once at first
//! use via [`global`]. Tests use [`Registry::with_only`] /
//! [`Registry::empty`] to avoid touching the global.

use std::collections::HashMap;
use std::sync::Arc;

use once_cell::sync::OnceCell;
use serde_json::{json, Value};
use wylde_shared::ipc::IpcError;

use crate::config::Config;

/// One tool catalog entry.
#[derive(Clone)]
pub struct ToolEntry {
    pub id: String,
    pub name: String,
    pub group: String,
    pub description: String,
    pub parameters: Vec<Value>,
    pub destructive: bool,
    pub kind: HandlerKind,
}

/// What the registry will actually do when dispatch reaches this entry.
#[derive(Clone)]
pub enum HandlerKind {
    /// Live handler. Async closure taking JSON args, returning JSON
    /// output or an `IpcError`. Closures get a borrow of the harness
    /// `Config` for tools that need IPC peer service names.
    Active(Arc<dyn Handler>),
    /// Tool is registered for catalog/alias purposes but has no live
    /// handler yet. Calls return a `phase_<n>_deferred` error envelope
    /// the LLM can interpret without confusion.
    Deferred {
        /// `"7"` for memory/rag, `"6"` for tools still in-flight,
        /// `"11"` for voice/visual. Matches the master plan's phase
        /// numbering.
        phase: &'static str,
        /// Human-readable rationale — shown back to the model in the
        /// error message so it can choose a different tool.
        reason: &'static str,
    },
}

/// Handler trait — async fn (args, cfg) -> Result<Value, IpcError>.
/// Implemented for closures via the [`HandlerFn`] helper.
pub trait Handler: Send + Sync + 'static {
    fn call<'a>(
        &'a self,
        args: Value,
        cfg: &'static Config,
    ) -> futures::future::BoxFuture<'a, Result<Value, IpcError>>;
}

/// Box helper. Tools build a `HandlerFn` from an async closure; the
/// resulting `Arc<dyn Handler>` plugs into [`ToolEntry::kind`].
pub struct HandlerFn<F>(pub F);

impl<F, Fut> Handler for HandlerFn<F>
where
    F: Fn(Value, &'static Config) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<Value, IpcError>> + Send + 'static,
{
    fn call<'a>(
        &'a self,
        args: Value,
        cfg: &'static Config,
    ) -> futures::future::BoxFuture<'a, Result<Value, IpcError>> {
        Box::pin((self.0)(args, cfg))
    }
}

/// Build a [`ToolEntry`] with an active handler from an async closure.
#[allow(clippy::too_many_arguments)]
pub fn entry_active<F, Fut>(
    id: &str,
    name: &str,
    group: &str,
    description: &str,
    parameters: Vec<Value>,
    destructive: bool,
    handler: F,
) -> ToolEntry
where
    F: Fn(Value, &'static Config) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<Value, IpcError>> + Send + 'static,
{
    ToolEntry {
        id: id.to_owned(),
        name: name.to_owned(),
        group: group.to_owned(),
        description: description.to_owned(),
        parameters,
        destructive,
        kind: HandlerKind::Active(Arc::new(HandlerFn(handler))),
    }
}

/// Build a [`ToolEntry`] that has no live handler — catalogued only so
/// the alias map and `tools.list` see it.
#[allow(clippy::too_many_arguments)]
pub fn entry_deferred(
    id: &str,
    name: &str,
    group: &str,
    description: &str,
    parameters: Vec<Value>,
    destructive: bool,
    phase: &'static str,
    reason: &'static str,
) -> ToolEntry {
    ToolEntry {
        id: id.to_owned(),
        name: name.to_owned(),
        group: group.to_owned(),
        description: description.to_owned(),
        parameters,
        destructive,
        kind: HandlerKind::Deferred { phase, reason },
    }
}

/// In-process tool catalog. Built up by [`Registry::default`] which
/// calls each module's `register` fn; immutable after that point.
pub struct Registry {
    by_id: HashMap<String, Arc<ToolEntry>>,
    /// Every key shape a tool may be looked up under — canonical id,
    /// dotted name, snake/dot inverses. Stored separately so [`lookup`]
    /// doesn't have to rebuild it per call.
    aliases: HashMap<String, String>,
}

impl Registry {
    /// Empty registry. Tests use this when they don't want the full
    /// catalog.
    pub fn empty() -> Self {
        Self {
            by_id: HashMap::new(),
            aliases: HashMap::new(),
        }
    }

    /// Build a registry with exactly the given entries — for tests
    /// where the catalog should be small + deterministic.
    pub fn with_only(entries: Vec<ToolEntry>) -> Self {
        let mut reg = Self::empty();
        for e in entries {
            reg.insert(e);
        }
        reg
    }

    /// Insert one entry. Updates `by_id` and the alias map. Aliases
    /// never overwrite a canonical id — if a derived alias clashes
    /// with another tool's id, the alias is skipped (matches Python
    /// `_apply_aliases` semantics).
    pub fn insert(&mut self, entry: ToolEntry) {
        let id = entry.id.clone();
        let aliases = alias_keys_for(&entry);
        let entry = Arc::new(entry);
        self.by_id.insert(id.clone(), Arc::clone(&entry));
        for alias in aliases {
            if alias == id {
                continue;
            }
            // Canonical ids always win — never let an alias displace one.
            if self.by_id.contains_key(&alias) {
                continue;
            }
            // First alias wins if two entries derive the same alias.
            self.aliases.entry(alias).or_insert_with(|| id.clone());
        }
    }

    /// Resolve any of (canonical id, dotted name, snake/dot inverse,
    /// manifest name) to the canonical [`ToolEntry`].
    pub fn lookup(&self, key: &str) -> Option<Arc<ToolEntry>> {
        if let Some(entry) = self.by_id.get(key) {
            return Some(Arc::clone(entry));
        }
        let canonical = self.aliases.get(key)?;
        self.by_id.get(canonical).cloned()
    }

    /// Return every canonical id in the catalog. Used by `tools.list`.
    pub fn canonical_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.by_id.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Return canonical entries (id-keyed only, no aliases). Order is
    /// stable by id.
    pub fn canonical_entries(&self) -> Vec<Arc<ToolEntry>> {
        self.canonical_ids()
            .into_iter()
            .filter_map(|id| self.by_id.get(&id).cloned())
            .collect()
    }

    /// Materialise the alias map. Phase 5.C's `build_alias_map` was a
    /// stub returning empty; Phase 6 produces the real mapping the
    /// salvage parser uses to resolve model-emitted names.
    ///
    /// Includes canonical id → id identity entries so a single
    /// `aliases.get(name).cloned()` resolves everything.
    pub fn alias_map(&self) -> HashMap<String, String> {
        let mut out = self.aliases.clone();
        for id in self.by_id.keys() {
            out.insert(id.clone(), id.clone());
        }
        out
    }

    /// True when the registry has at least one entry. Useful in tests.
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Number of canonical entries (excluding aliases).
    pub fn len(&self) -> usize {
        self.by_id.len()
    }
}

impl Default for Registry {
    /// Default registry — every tool the harness ships: the built-in
    /// groups plus the installed Core plugins (TX S4 — see
    /// `crate::plugins` for the linkage table).
    fn default() -> Self {
        let mut reg = Self::empty();
        crate::tooling::tools::register_all(&mut reg);
        crate::plugins::register(&mut reg);
        reg
    }
}

static GLOBAL: OnceCell<Registry> = OnceCell::new();

/// Process-wide registry. Built once on first call.
pub fn global() -> &'static Registry {
    GLOBAL.get_or_init(Registry::default)
}

/// Replace the global registry — for tests only. Tests that need a
/// hand-built registry call this before the first `global()`. Tests
/// that just need the default catalog don't have to call it at all.
#[cfg(test)]
pub fn install_for_tests(reg: Registry) {
    let _ = GLOBAL.set(reg);
}

/// Compute every key shape a given entry should be findable under.
/// Mirrors Python's `_alias_keys_for` in `tool_registry/__init__.py`.
///
/// Returns the canonical id plus every derivation:
/// * id (`read_file`)
/// * name (`fs.read_file`)
/// * dot-form derived from id (`read.file` if id has underscores)
/// * snake-form derived from name (`fs_read_file`)
fn alias_keys_for(entry: &ToolEntry) -> Vec<String> {
    let mut keys: Vec<String> = Vec::with_capacity(4);
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for raw in [
        entry.id.clone(),
        entry.name.clone(),
        entry.id.replace('_', "."),
        entry.name.replace('.', "_"),
    ] {
        let trimmed = raw.trim().to_owned();
        if !trimmed.is_empty() && seen.insert(trimmed.clone()) {
            keys.push(trimmed);
        }
    }
    keys
}

/// Build a JSON parameter descriptor — small helper used by every
/// tool's `register` fn so the schema literals stay readable.
pub fn param(name: &str, typ: &str, required: bool, description: &str) -> Value {
    json!({
        "name": name,
        "type": typ,
        "required": required,
        "description": description,
    })
}

/// Build a parameter descriptor with a default value.
pub fn param_default(name: &str, typ: &str, description: &str, default: Value) -> Value {
    json!({
        "name": name,
        "type": typ,
        "required": false,
        "description": description,
        "default": default,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy() -> ToolEntry {
        entry_active(
            "read_file",
            "fs.read_file",
            "fs",
            "read a file",
            vec![],
            false,
            |_, _| async { Ok(json!({"ok": true})) },
        )
    }

    #[test]
    fn alias_keys_cover_dot_and_snake_inverses() {
        let entry = dummy();
        let keys = alias_keys_for(&entry);
        assert!(keys.contains(&"read_file".to_string()));
        assert!(keys.contains(&"fs.read_file".to_string()));
        // dot-form derived from id: "read.file"
        assert!(keys.contains(&"read.file".to_string()));
        // snake-form derived from name: "fs_read_file"
        assert!(keys.contains(&"fs_read_file".to_string()));
    }

    #[test]
    fn lookup_resolves_canonical_id() {
        let reg = Registry::with_only(vec![dummy()]);
        let hit = reg.lookup("read_file").expect("canonical id resolves");
        assert_eq!(hit.id, "read_file");
    }

    #[test]
    fn lookup_resolves_dotted_name() {
        let reg = Registry::with_only(vec![dummy()]);
        let hit = reg.lookup("fs.read_file").expect("dotted name resolves");
        assert_eq!(hit.id, "read_file");
    }

    #[test]
    fn lookup_resolves_snake_inverse_of_name() {
        let reg = Registry::with_only(vec![dummy()]);
        let hit = reg.lookup("fs_read_file").expect("snake inverse resolves");
        assert_eq!(hit.id, "read_file");
    }

    #[test]
    fn lookup_returns_none_for_unknown() {
        let reg = Registry::with_only(vec![dummy()]);
        assert!(reg.lookup("totally_unknown").is_none());
    }

    #[test]
    fn alias_map_includes_canonical_identity_entries() {
        let reg = Registry::with_only(vec![dummy()]);
        let map = reg.alias_map();
        assert_eq!(map.get("read_file"), Some(&"read_file".to_string()));
        assert_eq!(map.get("fs.read_file"), Some(&"read_file".to_string()));
        assert_eq!(map.get("fs_read_file"), Some(&"read_file".to_string()));
    }

    #[test]
    fn canonical_ids_returns_only_canonical_keys() {
        let reg = Registry::with_only(vec![dummy()]);
        let ids = reg.canonical_ids();
        assert_eq!(ids, vec!["read_file"]);
    }

    #[test]
    fn aliases_do_not_overwrite_canonical_ids() {
        // Two entries: the first has id="fs.read", the second has name
        // alias "fs.read" — the alias should be skipped so the
        // canonical lookup wins.
        let first = entry_active(
            "fs.read",
            "fs.read",
            "fs",
            "first wins",
            vec![],
            false,
            |_, _| async { Ok(json!({"who": "first"})) },
        );
        let second = entry_active(
            "totally_other",
            "fs.read", // collides with first's canonical id via alias
            "fs",
            "second loses",
            vec![],
            false,
            |_, _| async { Ok(json!({"who": "second"})) },
        );
        let reg = Registry::with_only(vec![first, second]);
        let hit = reg.lookup("fs.read").expect("resolves");
        assert_eq!(hit.id, "fs.read");
    }

    #[test]
    fn deferred_entry_records_phase_tag() {
        let entry = entry_deferred(
            "memory_search",
            "memory.search",
            "memory",
            "memory search",
            vec![],
            false,
            "7",
            "ports with the memory layer",
        );
        match entry.kind {
            HandlerKind::Deferred { phase, .. } => assert_eq!(phase, "7"),
            HandlerKind::Active(_) => panic!("expected deferred kind"),
        }
    }
}

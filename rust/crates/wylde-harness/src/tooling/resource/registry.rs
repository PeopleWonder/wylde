//! The [`ResourceRegistry`] — hybrid static built-ins + dynamic
//! extension overlay. Tool-registry consolidation Slice 1.
//!
//! ## Why hybrid (plan §3.2)
//!
//! * **Built-ins** register explicitly via
//!   [`super::register_resources`], exactly like the per-tool
//!   [`crate::tooling::tools::register_all`]. The codebase already uses
//!   explicit registration everywhere; link-time collection (`inventory`
//!   / `linkme`) would fight the `with_only` / `empty` test idiom and
//!   make registration order non-obvious. Built-ins are sealed after
//!   init.
//! * **Extensions** need a `RwLock` overlay because they enable /
//!   disable / restart at runtime (Slice 5). Lookup checks `builtins`
//!   first, then the overlay. Extension resource types are namespaced
//!   (`ext:<name>:<resource>`) so they can never shadow a built-in.
//!
//! In Slice 1 **no resources are registered** — `register_resources` is
//! a no-op stub. The verb tools therefore return empty (`describe`) or
//! `not_found` (`list`/`get`/…) until Slice 2 lights up the memory
//! cluster. This is intentional: Slice 1 ships the substrate only.

use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use once_cell::sync::OnceCell;

use super::definition::{ResourceDefinition, Scope};

/// Per-conversation / per-extension visibility filter. Consulted at
/// `describe` + dispatch time so a scope never sees resources it
/// shouldn't (plan §3.3). The Slice-1 default ([`ToolsetFilter::all`])
/// is fully permissive; scope/allow-list plumbing arrives with the
/// workspace + extension slices.
#[derive(Debug, Clone)]
pub struct ToolsetFilter {
    /// `None` = every built-in visible. `Some(set)` = only these
    /// resource types (plus any always-allowed namespaced extensions).
    pub allow: Option<HashSet<String>>,
    /// The active scope. `Workspace`-scoped resources are hidden unless
    /// this is `Workspace`; `Conversation`-scoped likewise.
    pub scope: Scope,
}

impl ToolsetFilter {
    /// Fully-permissive filter — every built-in resource visible at
    /// global scope. The Slice-1 default.
    pub fn all() -> Self {
        Self {
            allow: None,
            scope: Scope::Global,
        }
    }

    /// True when a resource of `scope` + `resource_type` is visible
    /// under this filter.
    pub fn permits(&self, resource_type: &str, scope: Scope) -> bool {
        if let Some(allow) = &self.allow {
            if !allow.contains(resource_type) {
                return false;
            }
        }
        // Narrower scopes are hidden when the active scope is broader.
        match scope {
            Scope::Global => true,
            Scope::Workspace => matches!(self.scope, Scope::Workspace | Scope::Conversation),
            Scope::Conversation => matches!(self.scope, Scope::Conversation),
        }
    }
}

impl Default for ToolsetFilter {
    fn default() -> Self {
        Self::all()
    }
}

/// In-process resource catalog. Built-ins sealed at init; extensions in
/// a `RwLock` overlay.
pub struct ResourceRegistry {
    builtins: HashMap<&'static str, ResourceDefinition>,
    extensions: RwLock<HashMap<String, ResourceDefinition>>,
}

impl ResourceRegistry {
    /// Empty registry — no built-ins, empty overlay. Tests use this when
    /// they want to register an exact set.
    pub fn empty() -> Self {
        Self {
            builtins: HashMap::new(),
            extensions: RwLock::new(HashMap::new()),
        }
    }

    /// Register a built-in resource. Called from
    /// [`super::register_resources`] at init. Later inserts for the same
    /// `resource_type` overwrite (last-writer-wins, matching how a
    /// `register_*` fn would be the single authority for its resource).
    pub fn register_builtin(&mut self, def: ResourceDefinition) {
        self.builtins.insert(def.resource_type, def);
    }

    /// Register / replace an extension-provided resource at runtime
    /// (Slice 5). Keyed by the namespaced `resource_type` string.
    /// Returns the previous definition if one existed.
    pub fn register_extension(&self, def: ResourceDefinition) -> Option<ResourceDefinition> {
        let key = def.resource_type.to_owned();
        self.extensions
            .write()
            .expect("resource overlay poisoned")
            .insert(key, def)
    }

    /// Drop an extension resource (Slice 5 — on disable/restart).
    pub fn unregister_extension(&self, resource_type: &str) -> Option<ResourceDefinition> {
        self.extensions
            .write()
            .expect("resource overlay poisoned")
            .remove(resource_type)
    }

    /// Resolve a `resource_type` to its definition. Built-ins win over
    /// extensions (they can never be shadowed). Returns a clone — cheap:
    /// the only non-`Copy` member is the `Arc<dyn OpHandler>` map, and
    /// `Arc::clone` is a refcount bump.
    pub fn lookup(&self, resource_type: &str) -> Option<ResourceDefinition> {
        if let Some(def) = self.builtins.get(resource_type) {
            return Some(def.clone());
        }
        self.extensions
            .read()
            .expect("resource overlay poisoned")
            .get(resource_type)
            .cloned()
    }

    /// Resolve `resource_type` only if the filter permits it. Returns
    /// `None` both when the resource is absent and when it is filtered
    /// out — the verb dispatcher maps either to the same `not_found`
    /// envelope so a hidden resource is indistinguishable from a missing
    /// one (no information leak across scopes).
    pub fn lookup_visible(
        &self,
        resource_type: &str,
        filter: &ToolsetFilter,
    ) -> Option<ResourceDefinition> {
        let def = self.lookup(resource_type)?;
        if filter.permits(def.resource_type, def.scope) {
            Some(def)
        } else {
            None
        }
    }

    /// Compact summary rows for every visible resource — the no-arg
    /// `wylde_describe` payload. Built-ins first (sorted), then
    /// extensions (sorted), so the listing is deterministic.
    pub fn summary_rows(&self, filter: &ToolsetFilter) -> Vec<serde_json::Value> {
        let mut rows = Vec::new();
        let mut builtin_keys: Vec<&&'static str> = self.builtins.keys().collect();
        builtin_keys.sort();
        for k in builtin_keys {
            let def = &self.builtins[*k];
            if filter.permits(def.resource_type, def.scope) {
                rows.push(def.summary_row());
            }
        }
        let overlay = self.extensions.read().expect("resource overlay poisoned");
        let mut ext_keys: Vec<&String> = overlay.keys().collect();
        ext_keys.sort();
        for k in ext_keys {
            let def = &overlay[k];
            if filter.permits(def.resource_type, def.scope) {
                rows.push(def.summary_row());
            }
        }
        rows
    }

    /// Every visible resource type that supports `Search` — backs the
    /// `wylde_search("*", …)` fan-out (plan §4). Built-ins + extensions,
    /// sorted, deduped by type.
    pub fn searchable_types(&self, filter: &ToolsetFilter) -> Vec<String> {
        use super::definition::ResourceOp;
        let mut out: Vec<String> = Vec::new();
        for def in self.builtins.values() {
            if def.supports(ResourceOp::Search) && filter.permits(def.resource_type, def.scope) {
                out.push(def.resource_type.to_owned());
            }
        }
        let overlay = self.extensions.read().expect("resource overlay poisoned");
        for def in overlay.values() {
            if def.supports(ResourceOp::Search) && filter.permits(def.resource_type, def.scope) {
                out.push(def.resource_type.to_owned());
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// Count of built-in resource types. Useful in tests.
    pub fn builtin_len(&self) -> usize {
        self.builtins.len()
    }

    /// True when no built-ins and no extensions are registered.
    pub fn is_empty(&self) -> bool {
        self.builtins.is_empty()
            && self
                .extensions
                .read()
                .expect("resource overlay poisoned")
                .is_empty()
    }
}

impl Default for ResourceRegistry {
    /// Default registry — every built-in resource. In Slice 1 that is
    /// none (the `register_resources` stub is a no-op).
    fn default() -> Self {
        let mut reg = Self::empty();
        super::register_resources(&mut reg);
        reg
    }
}

static GLOBAL: OnceCell<ResourceRegistry> = OnceCell::new();

/// Process-wide resource registry. Built once on first call — the verb
/// tools dispatch through this. Mirrors
/// [`crate::tooling::registry::global`].
pub fn resources() -> &'static ResourceRegistry {
    GLOBAL.get_or_init(ResourceRegistry::default)
}

/// Replace the global resource registry — tests only.
#[cfg(test)]
pub fn install_for_tests(reg: ResourceRegistry) {
    let _ = GLOBAL.set(reg);
}

#[cfg(test)]
mod tests {
    use super::super::definition::{op_handler, ResourceOp};
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    fn def(resource_type: &'static str, scope: Scope, search: bool) -> ResourceDefinition {
        let mut ops: HashMap<ResourceOp, Arc<dyn super::super::definition::OpHandler>> =
            HashMap::new();
        ops.insert(
            ResourceOp::List,
            op_handler(|_r, _c, _x| async { Ok(json!({})) }),
        );
        if search {
            ops.insert(
                ResourceOp::Search,
                op_handler(|_r, _c, _x| async { Ok(json!({})) }),
            );
        }
        ResourceDefinition {
            resource_type,
            display_name: resource_type,
            description: "test",
            scope,
            identifier_fields: &["id"],
            filter_fields: &[],
            operations: ops,
            destructive_ops: &[],
            describe: super::super::definition::describe_value(|| json!({"d": true})),
        }
    }

    #[test]
    fn empty_registry_finds_nothing() {
        let reg = ResourceRegistry::empty();
        assert!(reg.is_empty());
        assert!(reg.lookup("memory").is_none());
        assert!(reg.summary_rows(&ToolsetFilter::all()).is_empty());
    }

    #[test]
    fn builtin_lookup_resolves() {
        let mut reg = ResourceRegistry::empty();
        reg.register_builtin(def("memory", Scope::Global, false));
        let hit = reg.lookup("memory").expect("resolves");
        assert_eq!(hit.resource_type, "memory");
        assert_eq!(reg.builtin_len(), 1);
    }

    #[test]
    fn extension_overlay_resolves_and_drops() {
        let reg = ResourceRegistry::empty();
        reg.register_extension(def("ext:web:page", Scope::Global, false));
        assert!(reg.lookup("ext:web:page").is_some());
        let dropped = reg.unregister_extension("ext:web:page");
        assert!(dropped.is_some());
        assert!(reg.lookup("ext:web:page").is_none());
    }

    #[test]
    fn builtins_win_over_extensions() {
        let mut reg = ResourceRegistry::empty();
        reg.register_builtin(def("memory", Scope::Global, false));
        // An extension trying to shadow a built-in type: lookup still
        // returns the built-in.
        reg.register_extension(def("memory", Scope::Conversation, false));
        let hit = reg.lookup("memory").expect("resolves");
        assert_eq!(hit.scope, Scope::Global, "built-in must win");
    }

    #[test]
    fn filter_hides_workspace_resource_at_global_scope() {
        let mut reg = ResourceRegistry::empty();
        reg.register_builtin(def("ws_thing", Scope::Workspace, false));
        let global = ToolsetFilter::all();
        assert!(reg.lookup_visible("ws_thing", &global).is_none());
        let ws = ToolsetFilter {
            allow: None,
            scope: Scope::Workspace,
        };
        assert!(reg.lookup_visible("ws_thing", &ws).is_some());
    }

    #[test]
    fn filter_allow_list_hides_unlisted() {
        let mut reg = ResourceRegistry::empty();
        reg.register_builtin(def("memory", Scope::Global, false));
        let mut allow = HashSet::new();
        allow.insert("file".to_string());
        let filter = ToolsetFilter {
            allow: Some(allow),
            scope: Scope::Global,
        };
        assert!(reg.lookup_visible("memory", &filter).is_none());
    }

    #[test]
    fn searchable_types_lists_only_search_capable() {
        let mut reg = ResourceRegistry::empty();
        reg.register_builtin(def("memory", Scope::Global, true));
        reg.register_builtin(def("file", Scope::Global, false));
        let types = reg.searchable_types(&ToolsetFilter::all());
        assert_eq!(types, vec!["memory".to_string()]);
    }

    #[test]
    fn summary_rows_sorted_builtins_then_extensions() {
        let mut reg = ResourceRegistry::empty();
        reg.register_builtin(def("zeta", Scope::Global, false));
        reg.register_builtin(def("alpha", Scope::Global, false));
        reg.register_extension(def("ext:x:y", Scope::Global, false));
        let rows = reg.summary_rows(&ToolsetFilter::all());
        let types: Vec<&str> = rows
            .iter()
            .map(|r| r["resource_type"].as_str().unwrap())
            .collect();
        assert_eq!(types, vec!["alpha", "zeta", "ext:x:y"]);
    }
}

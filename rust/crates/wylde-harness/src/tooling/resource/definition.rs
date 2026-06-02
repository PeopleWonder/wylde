//! Resource-verb substrate — core declarative types.
//!
//! Tool-registry consolidation Slice 1 (see
//! `docs/plans/tool-registry-consolidation.md`). This is the **verb
//! layer** that sits *on top of* the per-tool [`crate::tooling::registry`].
//! A [`ResourceDefinition`] maps the generic verbs
//! (`list/get/create/update/delete/search/execute`) to in-process Rust
//! handlers for one resource type, exactly the way Harness's
//! `mcp-server` pattern maps verbs to endpoints — only here the handler
//! is an `async fn` rather than an HTTP path, so there is *less*
//! indirection than the status quo, not more.
//!
//! ## Mirrors the existing `Handler` idiom
//!
//! [`OpHandler`] / [`OpHandlerFn`] are a deliberate copy of
//! [`crate::tooling::registry::Handler`] / `HandlerFn`: an
//! `Arc<dyn OpHandler>` boxes an async closure into a `BoxFuture`, so the
//! verb dispatcher reuses the same plumbing the per-tool runner already
//! uses. The only added argument is the per-call [`ToolContext`] (§5 of
//! the plan, the Slice-2 port). It is passed **by value** — mirroring the
//! by-value `args: Value` of `Handler::call` — which sidesteps the
//! async-closure-borrows-its-argument lifetime trap and keeps the
//! `BoxFuture` return shape identical to `Handler`.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};
use wylde_shared::ipc::IpcError;

use crate::config::Config;

/// The seven generic verbs a resource may support. The eighth verb,
/// `describe`, is local metadata only (no `OpHandler`) so it is not a
/// member here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResourceOp {
    List,
    Get,
    Create,
    Update,
    Delete,
    Search,
    Execute,
}

impl ResourceOp {
    /// Canonical lower-case verb string (`"list"`, `"get"`, …). Used in
    /// `describe()` output and the `wylde_<verb>` tool ids.
    pub fn as_str(self) -> &'static str {
        match self {
            ResourceOp::List => "list",
            ResourceOp::Get => "get",
            ResourceOp::Create => "create",
            ResourceOp::Update => "update",
            ResourceOp::Delete => "delete",
            ResourceOp::Search => "search",
            ResourceOp::Execute => "execute",
        }
    }

    /// Every op, in a stable order — for `describe()` enumeration.
    pub fn all() -> [ResourceOp; 7] {
        [
            ResourceOp::List,
            ResourceOp::Get,
            ResourceOp::Create,
            ResourceOp::Update,
            ResourceOp::Delete,
            ResourceOp::Search,
            ResourceOp::Execute,
        ]
    }
}

/// Where a resource lives. Used by the [`super::ToolsetFilter`] so a
/// conversation with no workspace never sees `Workspace`-scoped
/// resources (R3 in the plan — describe-schema trimming).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Global,
    Workspace,
    Conversation,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Global => "global",
            Scope::Workspace => "workspace",
            Scope::Conversation => "conversation",
        }
    }
}

/// One per-call context, set by the verb dispatcher **once** before it
/// delegates to an [`OpHandler`]. This is the choke-point where the
/// resolved `(resource_type, op, resource_id)` and the turn's
/// `device_tier` become visible to handler code — without a hidden
/// global (R2 in the plan).
///
/// ## Slice-2 note
///
/// The plan (R2) calls for building on the in-flight harness
/// `ToolContext` port rather than defining a second type. That type is
/// **not yet in the working tree** (`grep -r ToolContext` finds nothing
/// under `rust/`), so Slice 1 ships this minimal, self-contained stub to
/// avoid blocking. When the harness `ToolContext` lands, a follow-up
/// slice replaces this struct with a re-export / extension of it; the
/// `OpHandler::call(req, cfg, ctx)` signature stays the same, so handler
/// code does not change.
#[derive(Debug, Clone, Default)]
pub struct ToolContext {
    /// Resolved resource type, e.g. `"memory"`. Empty for `describe`.
    pub resource_type: String,
    /// The verb being dispatched. `None` for `describe` (local only).
    pub op: Option<ResourceOp>,
    /// Identifier the call targets (get/update/delete), if supplied.
    pub resource_id: Option<String>,
    /// The turn's normalised device tier. Defaults to empty until the
    /// Slice-2 harness `ToolContext` threads the real value from the
    /// runner; handlers must treat empty as `tool_use`.
    pub device_tier: String,
}

impl ToolContext {
    /// Build a context for a resolved verb dispatch.
    pub fn for_op(resource_type: &str, op: ResourceOp, resource_id: Option<String>) -> Self {
        Self {
            resource_type: resource_type.to_owned(),
            op: Some(op),
            resource_id,
            device_tier: String::new(),
        }
    }
}

/// The decoded arguments of a verb call, normalised into one shape every
/// [`OpHandler`] reads. The verb dispatcher builds this from the model's
/// raw `arguments` object via [`ResourceRequest::from_args`].
#[derive(Debug, Clone, Default)]
pub struct ResourceRequest {
    pub op: Option<ResourceOp>,
    /// get / update / delete target.
    pub resource_id: Option<String>,
    /// create / update payload.
    pub body: Value,
    /// list / search / filter-delete predicate.
    pub filter: Value,
    /// execute sub-op selector.
    pub action: Option<String>,
    /// search query string.
    pub query: Option<String>,
    pub limit: Option<u64>,
    pub cursor: Option<String>,
}

impl ResourceRequest {
    /// Decode a model `arguments` object for the given verb. Unknown /
    /// absent fields stay `None` / `Value::Null`; per-op required-field
    /// validation is the handler's job (it owns the resource's schema).
    pub fn from_args(op: ResourceOp, args: &Value) -> Self {
        let get_str = |k: &str| {
            args.get(k)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .filter(|s| !s.is_empty())
        };
        Self {
            op: Some(op),
            resource_id: get_str("resource_id"),
            body: args.get("body").cloned().unwrap_or(Value::Null),
            filter: args.get("filter").cloned().unwrap_or(Value::Null),
            action: get_str("action"),
            query: get_str("query"),
            limit: args.get("limit").and_then(Value::as_u64),
            cursor: get_str("cursor"),
        }
    }
}

/// One operation handler. Mirrors [`crate::tooling::registry::Handler`]
/// so the `Arc<dyn _>` boxing and `BoxFuture` return shape are identical
/// — see the module docs. `ctx` is owned (by value) for the same reason
/// `Handler::call` takes `args: Value` by value.
pub trait OpHandler: Send + Sync + 'static {
    fn call<'a>(
        &'a self,
        req: ResourceRequest,
        cfg: &'static Config,
        ctx: ToolContext,
    ) -> futures::future::BoxFuture<'a, Result<Value, IpcError>>;
}

/// Box helper — wraps an async closure into an [`OpHandler`]. Exact
/// analogue of [`crate::tooling::registry::HandlerFn`].
pub struct OpHandlerFn<F>(pub F);

impl<F, Fut> OpHandler for OpHandlerFn<F>
where
    F: Fn(ResourceRequest, &'static Config, ToolContext) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<Value, IpcError>> + Send + 'static,
{
    fn call<'a>(
        &'a self,
        req: ResourceRequest,
        cfg: &'static Config,
        ctx: ToolContext,
    ) -> futures::future::BoxFuture<'a, Result<Value, IpcError>> {
        Box::pin((self.0)(req, cfg, ctx))
    }
}

/// Build an `Arc<dyn OpHandler>` from an async closure — the verb-layer
/// twin of `entry_active`'s closure boxing.
pub fn op_handler<F, Fut>(handler: F) -> Arc<dyn OpHandler>
where
    F: Fn(ResourceRequest, &'static Config, ToolContext) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<Value, IpcError>> + Send + 'static,
{
    Arc::new(OpHandlerFn(handler))
}

/// One declarative resource — the verb-layer analogue of a
/// [`crate::tooling::registry::ToolEntry`], but covering a *family* of
/// operations on a single named resource type rather than one tool.
#[derive(Clone)]
pub struct ResourceDefinition {
    /// Lookup key the model passes as `resource_type`, e.g. `"memory"`,
    /// `"file"`, `"rag_chunk"`. Snake-case, stable.
    pub resource_type: &'static str,
    /// Human-readable name for `describe()` rows.
    pub display_name: &'static str,
    /// One-line summary, surfaced by `describe()`.
    pub description: &'static str,
    /// Visibility scope (consulted by [`super::ToolsetFilter`]).
    pub scope: Scope,
    /// Field name(s) that identify one instance — `["id"]`, `["path"]`.
    pub identifier_fields: &'static [&'static str],
    /// Field name(s) accepted in a `list`/`search` filter object.
    pub filter_fields: &'static [&'static str],
    /// Per-op handlers. Absent op → `unsupported_op` envelope.
    pub operations: HashMap<ResourceOp, Arc<dyn OpHandler>>,
    /// Which ops mutate / delete data. Replaces `ToolEntry.destructive`
    /// (one flag can't say "delete is destructive but get is not"). The
    /// verb dispatcher derives the effective destructive bool per
    /// `(resource, op)` from this set.
    pub destructive_ops: &'static [ResourceOp],
    /// Compact self-description. Returned verbatim by `wylde_describe`
    /// when this resource is named.
    pub describe: fn() -> Value,
}

impl ResourceDefinition {
    /// True when `op` mutates or deletes data for this resource — drives
    /// the per-op tier/consent classification (R7 in the plan).
    pub fn is_destructive(&self, op: ResourceOp) -> bool {
        self.destructive_ops.contains(&op)
    }

    /// True when this resource has a handler for `op`.
    pub fn supports(&self, op: ResourceOp) -> bool {
        self.operations.contains_key(&op)
    }

    /// The ops this resource supports, in stable order.
    pub fn supported_ops(&self) -> Vec<ResourceOp> {
        ResourceOp::all()
            .into_iter()
            .filter(|op| self.operations.contains_key(op))
            .collect()
    }

    /// A compact one-row summary — `{resource_type, display_name, ops,
    /// scope, destructive_ops}` — used by the no-arg `wylde_describe`
    /// listing (R3: one line each, not full schemas).
    pub fn summary_row(&self) -> Value {
        json!({
            "resource_type": self.resource_type,
            "display_name": self.display_name,
            "description": self.description,
            "scope": self.scope.as_str(),
            "ops": self.supported_ops().iter().map(|o| o.as_str()).collect::<Vec<_>>(),
            "destructive_ops": self.destructive_ops.iter().map(|o| o.as_str()).collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_def() -> ResourceDefinition {
        let mut ops: HashMap<ResourceOp, Arc<dyn OpHandler>> = HashMap::new();
        ops.insert(
            ResourceOp::List,
            op_handler(|_req, _cfg, _ctx| async { Ok(json!({"ok": true})) }),
        );
        ops.insert(
            ResourceOp::Delete,
            op_handler(|_req, _cfg, _ctx| async { Ok(json!({"deleted": true})) }),
        );
        ResourceDefinition {
            resource_type: "widget",
            display_name: "Widget",
            description: "a test widget",
            scope: Scope::Global,
            identifier_fields: &["id"],
            filter_fields: &["kind"],
            operations: ops,
            destructive_ops: &[ResourceOp::Delete],
            describe: || json!({"resource_type": "widget"}),
        }
    }

    #[test]
    fn op_as_str_round_trips() {
        assert_eq!(ResourceOp::Create.as_str(), "create");
        assert_eq!(ResourceOp::all().len(), 7);
    }

    #[test]
    fn is_destructive_reads_the_set() {
        let def = dummy_def();
        assert!(def.is_destructive(ResourceOp::Delete));
        assert!(!def.is_destructive(ResourceOp::List));
    }

    #[test]
    fn supported_ops_filters_and_orders() {
        let def = dummy_def();
        // List comes before Delete in ResourceOp::all() order.
        assert_eq!(def.supported_ops(), vec![ResourceOp::List, ResourceOp::Delete]);
    }

    #[test]
    fn summary_row_is_compact() {
        let def = dummy_def();
        let row = def.summary_row();
        assert_eq!(row["resource_type"], "widget");
        assert_eq!(row["scope"], "global");
        assert_eq!(row["ops"], json!(["list", "delete"]));
        assert_eq!(row["destructive_ops"], json!(["delete"]));
    }

    #[test]
    fn request_decodes_known_fields() {
        let args = json!({
            "resource_id": "abc",
            "body": {"x": 1},
            "filter": {"kind": "z"},
            "action": "preload",
            "query": "find me",
            "limit": 7,
            "cursor": "next",
        });
        let req = ResourceRequest::from_args(ResourceOp::Update, &args);
        assert_eq!(req.op, Some(ResourceOp::Update));
        assert_eq!(req.resource_id.as_deref(), Some("abc"));
        assert_eq!(req.body["x"], 1);
        assert_eq!(req.filter["kind"], "z");
        assert_eq!(req.action.as_deref(), Some("preload"));
        assert_eq!(req.query.as_deref(), Some("find me"));
        assert_eq!(req.limit, Some(7));
        assert_eq!(req.cursor.as_deref(), Some("next"));
    }

    #[test]
    fn request_empty_strings_become_none() {
        let req = ResourceRequest::from_args(ResourceOp::Get, &json!({"resource_id": ""}));
        assert!(req.resource_id.is_none());
    }

    #[tokio::test]
    async fn op_handler_boxes_async_closure() {
        let h = op_handler(|_req, _cfg, _ctx| async { Ok(json!({"hit": 1})) });
        let cfg = Config::default_for_tests();
        let cfg: &'static Config = Box::leak(Box::new(cfg));
        let out = h
            .call(ResourceRequest::default(), cfg, ToolContext::default())
            .await
            .unwrap();
        assert_eq!(out["hit"], 1);
    }
}

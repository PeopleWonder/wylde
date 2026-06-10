//! Scope resolution + the strict workspace boundary (Plan v2 §3.2).
//!
//! This is the security-critical core of Slice E. Every search/list/get
//! verb resolves which storage backends it may read **here**, and only
//! here; the handlers never read a store the resolver didn't return.
//!
//! The boundary, restated as a truth table over `(active_workspace,
//! requested)`:
//!
//! | active_workspace | requested            | backends                         |
//! |---|---|---|
//! | `None`           | `CurrentContext`     | `[Standalone]`                   |
//! | `None`           | `StandaloneOnly`     | `[Standalone]`                   |
//! | `None`           | `WorkspaceOnly(_)`   | **`BadRequest`** (standalone can't reach a workspace) |
//! | `Some(X)`        | `CurrentContext`     | `[Workspace(X), Standalone]`     |
//! | `Some(X)`        | `StandaloneOnly`     | `[Standalone]`                   |
//! | `Some(X)`        | `WorkspaceOnly(X)`   | `[Workspace(X)]`                 |
//! | `Some(X)`        | `WorkspaceOnly(Y≠X)` | **`BadRequest`** (the escape attempt) |
//!
//! A workspace backend is *always* pinned to a single workspace id, so no
//! query can ever fan out across workspaces by construction.

use serde_json::Value;

use super::ChatSearchError;

/// Which conversations a caller is asking to search, before the boundary
/// is applied. The default (and overwhelmingly common) case is
/// [`WorkspaceScope::CurrentContext`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceScope {
    /// Explicit override: standalone conversations only, never a workspace.
    StandaloneOnly,
    /// Explicit override: search ONLY this workspace. Allowed solely when
    /// it *is* the active workspace — otherwise a `bad_request`.
    WorkspaceOnly(String),
    /// Default: standalone if no active workspace; otherwise this
    /// workspace **plus** standalone.
    CurrentContext,
}

impl WorkspaceScope {
    /// Parse the optional `workspace_scope` payload field. Accepted shapes:
    ///
    /// * absent / `null`                              → `CurrentContext`
    /// * `"current"` / `"current_context"`            → `CurrentContext`
    /// * `"standalone"` / `"standalone_only"`         → `StandaloneOnly`
    /// * `{"workspace_only": "<id>"}`                 → `WorkspaceOnly(id)`
    /// * `{"mode": "workspace_only", "workspace_id": "<id>"}` → same
    ///
    /// Anything else is a `bad_request` so a typo can't silently widen
    /// scope.
    pub fn from_payload(value: Option<&Value>) -> Result<Self, ChatSearchError> {
        match value {
            None | Some(Value::Null) => Ok(WorkspaceScope::CurrentContext),
            Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
                "current" | "current_context" | "" => Ok(WorkspaceScope::CurrentContext),
                "standalone" | "standalone_only" => Ok(WorkspaceScope::StandaloneOnly),
                other => Err(ChatSearchError::BadRequest(format!(
                    "unknown workspace_scope {other:?} (expected \"current\", \
                     \"standalone\", or {{\"workspace_only\": \"<id>\"}})"
                ))),
            },
            Some(Value::Object(obj)) => {
                // {"workspace_only": "<id>"} or
                // {"mode": "workspace_only", "workspace_id": "<id>"}.
                let id = obj
                    .get("workspace_only")
                    .and_then(Value::as_str)
                    .or_else(|| obj.get("workspace_id").and_then(Value::as_str))
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                match id {
                    Some(id) => Ok(WorkspaceScope::WorkspaceOnly(id.to_owned())),
                    None => Err(ChatSearchError::BadRequest(
                        "workspace_scope object needs a non-empty \
                         \"workspace_only\" / \"workspace_id\""
                            .to_owned(),
                    )),
                }
            }
            Some(other) => Err(ChatSearchError::BadRequest(format!(
                "workspace_scope must be a string or object, got {other}"
            ))),
        }
    }
}

/// One storage backend the resolver authorised a verb to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// The harness flat store (`workspace_id == None`).
    Standalone,
    /// One workspace's store in `wylde-workspaces`, pinned to this id.
    Workspace(String),
}

/// The authorised read set for a verb call. Constructed only by
/// [`resolve_scope`]; handlers iterate `backends` and read nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedQuery {
    pub backends: Vec<Backend>,
}

impl ScopedQuery {
    /// True when standalone conversations are in scope.
    pub fn includes_standalone(&self) -> bool {
        self.backends
            .iter()
            .any(|b| matches!(b, Backend::Standalone))
    }

    /// The single workspace id in scope, if any.
    pub fn workspace_id(&self) -> Option<&str> {
        self.backends.iter().find_map(|b| match b {
            Backend::Workspace(id) => Some(id.as_str()),
            Backend::Standalone => None,
        })
    }
}

/// Normalise an active-workspace hint: trim, and treat empty as "no active
/// workspace" (standalone context).
fn normalise_active(active_workspace: Option<&str>) -> Option<&str> {
    active_workspace.map(str::trim).filter(|s| !s.is_empty())
}

/// Resolve the reachable backends for `requested`, enforcing the strict
/// boundary. This is the only function permitted to decide what a search
/// may read.
///
/// `active_workspace` is the workspace the *current chat* is bound to
/// (`None` / empty = standalone context).
pub fn resolve_scope(
    active_workspace: Option<&str>,
    requested: WorkspaceScope,
) -> Result<ScopedQuery, ChatSearchError> {
    let active = normalise_active(active_workspace);

    let backends = match requested {
        WorkspaceScope::StandaloneOnly => vec![Backend::Standalone],

        WorkspaceScope::WorkspaceOnly(id) => {
            let id = id.trim();
            if id.is_empty() {
                return Err(ChatSearchError::BadRequest(
                    "workspace_only scope needs a non-empty workspace id".to_owned(),
                ));
            }
            match active {
                // Asking for the workspace you're actually in — allowed.
                Some(a) if a == id => vec![Backend::Workspace(id.to_owned())],
                // Asking for a *different* workspace — the escape attempt.
                Some(a) => {
                    return Err(ChatSearchError::BadRequest(format!(
                        "cannot search workspace {id:?} from workspace {a:?} — \
                         conversations are strictly scoped to their workspace"
                    )))
                }
                // Standalone context cannot reach any workspace.
                None => {
                    return Err(ChatSearchError::BadRequest(format!(
                        "cannot search workspace {id:?} from standalone chat — \
                         standalone conversations only"
                    )))
                }
            }
        }

        WorkspaceScope::CurrentContext => match active {
            None => vec![Backend::Standalone],
            // Workspace context sees its own conversations + standalone.
            Some(a) => vec![Backend::Workspace(a.to_owned()), Backend::Standalone],
        },
    };

    Ok(ScopedQuery { backends })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── the four core cases the brief calls out ──────────────────────────

    #[test]
    fn standalone_only_is_just_standalone_from_any_context() {
        // From standalone context.
        let q = resolve_scope(None, WorkspaceScope::StandaloneOnly).unwrap();
        assert_eq!(q.backends, vec![Backend::Standalone]);
        // From a workspace context — still just standalone.
        let q = resolve_scope(Some("ws-a"), WorkspaceScope::StandaloneOnly).unwrap();
        assert_eq!(q.backends, vec![Backend::Standalone]);
    }

    #[test]
    fn workspace_only_current_is_allowed() {
        let q = resolve_scope(Some("ws-a"), WorkspaceScope::WorkspaceOnly("ws-a".into())).unwrap();
        assert_eq!(q.backends, vec![Backend::Workspace("ws-a".into())]);
        assert_eq!(q.workspace_id(), Some("ws-a"));
        assert!(!q.includes_standalone());
    }

    #[test]
    fn workspace_only_different_is_bad_request_the_escape_attempt() {
        // THE boundary test: in workspace A, ask for B → refused.
        let err =
            resolve_scope(Some("ws-a"), WorkspaceScope::WorkspaceOnly("ws-b".into())).unwrap_err();
        assert!(matches!(err, ChatSearchError::BadRequest(_)));
        assert_eq!(err.code(), "bad_request");
    }

    #[test]
    fn workspace_only_from_standalone_is_bad_request() {
        // Standalone chat cannot reach ANY workspace by id.
        let err = resolve_scope(None, WorkspaceScope::WorkspaceOnly("ws-b".into())).unwrap_err();
        assert!(matches!(err, ChatSearchError::BadRequest(_)));
    }

    #[test]
    fn current_context_standalone_sees_only_standalone() {
        let q = resolve_scope(None, WorkspaceScope::CurrentContext).unwrap();
        assert_eq!(q.backends, vec![Backend::Standalone]);
        assert_eq!(q.workspace_id(), None);
    }

    #[test]
    fn current_context_workspace_sees_workspace_plus_standalone() {
        let q = resolve_scope(Some("ws-a"), WorkspaceScope::CurrentContext).unwrap();
        assert_eq!(
            q.backends,
            vec![Backend::Workspace("ws-a".into()), Backend::Standalone]
        );
        assert!(q.includes_standalone());
        assert_eq!(q.workspace_id(), Some("ws-a"));
    }

    #[test]
    fn empty_active_workspace_is_standalone_context() {
        // "" / whitespace active id is treated as no workspace.
        let q = resolve_scope(Some("   "), WorkspaceScope::CurrentContext).unwrap();
        assert_eq!(q.backends, vec![Backend::Standalone]);
        // And asking for a workspace from that context is still refused.
        assert!(resolve_scope(Some(""), WorkspaceScope::WorkspaceOnly("x".into())).is_err());
    }

    #[test]
    fn empty_workspace_only_id_is_bad_request() {
        let err =
            resolve_scope(Some("ws-a"), WorkspaceScope::WorkspaceOnly("  ".into())).unwrap_err();
        assert!(matches!(err, ChatSearchError::BadRequest(_)));
    }

    // ── payload parsing ──────────────────────────────────────────────────

    #[test]
    fn parse_default_and_string_forms() {
        assert_eq!(
            WorkspaceScope::from_payload(None).unwrap(),
            WorkspaceScope::CurrentContext
        );
        assert_eq!(
            WorkspaceScope::from_payload(Some(&Value::Null)).unwrap(),
            WorkspaceScope::CurrentContext
        );
        assert_eq!(
            WorkspaceScope::from_payload(Some(&json!("current"))).unwrap(),
            WorkspaceScope::CurrentContext
        );
        assert_eq!(
            WorkspaceScope::from_payload(Some(&json!("standalone"))).unwrap(),
            WorkspaceScope::StandaloneOnly
        );
        assert_eq!(
            WorkspaceScope::from_payload(Some(&json!("Standalone_Only"))).unwrap(),
            WorkspaceScope::StandaloneOnly
        );
    }

    #[test]
    fn parse_workspace_only_object_forms() {
        assert_eq!(
            WorkspaceScope::from_payload(Some(&json!({"workspace_only": "ws-a"}))).unwrap(),
            WorkspaceScope::WorkspaceOnly("ws-a".into())
        );
        assert_eq!(
            WorkspaceScope::from_payload(Some(
                &json!({"mode": "workspace_only", "workspace_id": "ws-b"})
            ))
            .unwrap(),
            WorkspaceScope::WorkspaceOnly("ws-b".into())
        );
    }

    #[test]
    fn parse_rejects_unknown_string_and_empty_object() {
        assert!(WorkspaceScope::from_payload(Some(&json!("everything"))).is_err());
        assert!(WorkspaceScope::from_payload(Some(&json!({}))).is_err());
        assert!(WorkspaceScope::from_payload(Some(&json!({"workspace_only": ""}))).is_err());
        assert!(WorkspaceScope::from_payload(Some(&json!(42))).is_err());
    }
}

//! Workspace prompt context for a chat turn — fetched from the
//! `wylde-workspaces` service over the pipe, with graceful degradation.
//!
//! **Conceptual path:** `Core/Harness/turn/workspace_context`.
//!
//! Through Slices 0b/0c the turn driver gathered the active workspace's
//! persona / notes / RAG **in-process** via the harness `workspaces`
//! module. Slice 0d retired that module: the harness now holds no
//! workspace code and fetches the rendered system-prompt slots from the
//! service via the [`wylde_workspaces_client`] crate.
//!
//! ## Graceful degradation (scope v2 §7.5)
//!
//! Wylde Core must work with workspaces disabled or unreachable. Chat
//! works without the service; it's *richer* with it. When the service is
//! Broken (pipe missing / transport failure) or the breaker is open, the
//! gather returns [`WorkspacePrompt::degraded`] and the turn proceeds with
//! base context only, prefixed with a one-line notice in the response.
//!
//! `workspaces.gather_prompt` is registered with a NoRetry policy in the
//! client verb table, so a slow/unreachable service fails fast into this
//! degraded path instead of stacking retry budgets onto every turn.

use serde_json::Value;
use wylde_workspaces_client::{ClientError, WorkspacesClient};

/// The inline notice prefixed to a turn's response when the workspaces
/// service was requested but unreachable. Kept short and user-facing.
pub(crate) const WORKSPACES_UNAVAILABLE_NOTICE: &str =
    "Workspaces unavailable; using base context.";

/// Render-time ceiling on the persona text the harness injects (~2k
/// estimated tokens at 4 chars/token; the B8 cap, applied here since B6
/// reads the RAW structured persona instead of the service-rendered
/// block). Truncation is marked; the stored persona.md is untouched.
const PERSONA_MAX_CHARS: usize = 8_000;

/// The service name the turn driver forwards workspace reads to. Defaults
/// to `wylde-workspaces`; overridable via `WYLDE_HARNESS_WORKSPACES_SERVICE`
/// so tests can point it at a guaranteed-dead pipe and exercise the
/// degraded path deterministically (never a real running service).
pub(crate) fn workspaces_service() -> String {
    std::env::var("WYLDE_HARNESS_WORKSPACES_SERVICE")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "wylde-workspaces".to_owned())
}

/// The outcome of gathering a turn's workspace prompt context. Since B6
/// the parts arrive STRUCTURED — persona / notes / RAG map onto separate
/// `ChatContext` fields (and separate eviction tiers) instead of one
/// opaque pre-rendered block that evicted wholesale.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct WorkspacePrompt {
    /// The workspace persona text (B8-capped), if any.
    pub persona: Option<String>,
    /// Workspace note snippets, highest-scoring first.
    pub notes: Vec<String>,
    /// RAG snippets scoped to the workspace folder, best-first.
    pub rag: Vec<String>,
    /// True when a workspace was requested but the service was unreachable
    /// (Broken / breaker open) — the caller surfaces the inline notice.
    pub degraded: bool,
    /// Concept-routing candidate set (concept-routing plan R1) — `Some` only
    /// when the master toggle was on and the service routed something. **R1:
    /// logged, never injected**; carrying it here lets the harness log it from
    /// its single gather site. Does not affect [`is_empty`](Self::is_empty).
    pub route_candidates: Option<wylde_concept_routing::CandidateSet>,
}

impl WorkspacePrompt {
    /// The base-context outcome: nothing gathered, not degraded. Used when
    /// no workspace is active or the id is blank.
    fn base() -> Self {
        Self::default()
    }

    /// True when the workspace contributed nothing.
    pub fn is_empty(&self) -> bool {
        self.persona.is_none() && self.notes.is_empty() && self.rag.is_empty()
    }
}

/// Parse the structured `workspaces.gather_prompt` reply fields.
fn parse_prompt_reply(v: &Value) -> WorkspacePrompt {
    let persona = v
        .get("persona")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(cap_persona);
    let list = |key: &str| -> Vec<String> {
        v.get(key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    // Concept-routing R1: deserialize the candidate set when present. A
    // null/absent/garbage field ⇒ None (routing off or nothing routed) — never
    // an error, so a malformed field can't break a turn.
    let route_candidates = v
        .get("route_candidates")
        .filter(|c| !c.is_null())
        .and_then(|c| serde_json::from_value(c.clone()).ok());
    WorkspacePrompt {
        persona,
        notes: list("memory_snippets"),
        rag: list("rag_snippets"),
        degraded: false,
        route_candidates,
    }
}

/// Apply the B8 persona cap with a visible marker.
fn cap_persona(persona: &str) -> String {
    if persona.chars().count() <= PERSONA_MAX_CHARS {
        return persona.to_owned();
    }
    let mut out: String = persona.chars().take(PERSONA_MAX_CHARS).collect();
    out.push_str(&format!(
        "\n[persona truncated at {PERSONA_MAX_CHARS} characters — shorten persona.md]"
    ));
    out
}

/// True when the client error means the service did not give an
/// authoritative answer — unreachable (transport) or the breaker is open.
/// Those degrade to base context; a genuine application error (e.g. an
/// unknown workspace) is NOT degradation — it just means no context.
fn is_unavailable(e: &ClientError) -> bool {
    e.transport || e.code == "breaker_open"
}

/// Resolve the active workspace's contribution to this turn's system
/// prompt by calling `workspaces.gather_prompt` over the pipe.
///
/// An absent / empty `workspace_id` yields base context (no pipe call), so
/// a plain chat turn is byte-identical to before. A reachable service
/// returns the structured parts (B6); an unreachable one degrades.
pub(crate) async fn gather(
    workspace_id: Option<&str>,
    user_message: &str,
    route: bool,
) -> WorkspacePrompt {
    let Some(ws_id) = workspace_id.map(str::trim).filter(|s| !s.is_empty()) else {
        return WorkspacePrompt::base();
    };

    let client = WorkspacesClient::for_service(workspaces_service());
    match client.gather_prompt_raw(ws_id, user_message, route).await {
        Ok(reply) => parse_prompt_reply(&reply),
        // Service unreachable / breaker open → base context + notice.
        Err(e) if is_unavailable(&e) => WorkspacePrompt {
            degraded: true,
            ..WorkspacePrompt::default()
        },
        // Application error (unknown workspace, bad request) → the service
        // is healthy, it just has nothing for us. Base context, no notice.
        Err(_) => WorkspacePrompt::base(),
    }
}

/// Prefix the workspaces-unavailable notice to a turn's final text when the
/// gather degraded. A no-op when not degraded. Keeps the notice as the
/// first line so it's visible regardless of what the model produced.
pub(crate) fn apply_degraded_notice(text: String, degraded: bool) -> String {
    if !degraded {
        return text;
    }
    if text.trim().is_empty() {
        WORKSPACES_UNAVAILABLE_NOTICE.to_owned()
    } else {
        format!("{WORKSPACES_UNAVAILABLE_NOTICE}\n\n{text}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// `WYLDE_HARNESS_WORKSPACES_SERVICE` is process-wide; serialize the
    /// tests that set it so parallel threads don't clobber each other.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_env() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[tokio::test]
    async fn no_workspace_is_base_context() {
        let out = gather(None, "hello", false).await;
        assert!(out.is_empty());
        assert!(!out.degraded);

        let out = gather(Some("   "), "hello", false).await;
        assert!(out.is_empty());
        assert!(!out.degraded);
    }

    #[tokio::test]
    async fn unreachable_service_degrades_to_base_context() {
        let _g = lock_env();
        let prior = std::env::var_os("WYLDE_HARNESS_WORKSPACES_SERVICE");
        // Point at a guaranteed-dead pipe so the gather fails at transport.
        std::env::set_var(
            "WYLDE_HARNESS_WORKSPACES_SERVICE",
            format!("wylde-workspaces-dead-{}", std::process::id()),
        );

        let out = gather(Some("any-workspace"), "hello", false).await;
        assert!(out.is_empty(), "degraded gather must add nothing");
        assert!(out.degraded, "an unreachable service must degrade");

        match prior {
            Some(v) => std::env::set_var("WYLDE_HARNESS_WORKSPACES_SERVICE", v),
            None => std::env::remove_var("WYLDE_HARNESS_WORKSPACES_SERVICE"),
        }
    }

    #[test]
    fn parse_prompt_reply_reads_structured_fields() {
        let v = serde_json::json!({
            "workspace_id": "ws",
            "slots": "(ignored — B6 consumes the parts)",
            "persona": "  Be precise.  ",
            "memory_snippets": ["uses pytest", "  ", "prefers Rust"],
            "rag_snippets": ["fn main() {}"],
        });
        let p = parse_prompt_reply(&v);
        assert_eq!(p.persona.as_deref(), Some("Be precise."));
        assert_eq!(p.notes, vec!["uses pytest", "prefers Rust"]);
        assert_eq!(p.rag, vec!["fn main() {}"]);
        assert!(!p.degraded);

        // Absent / blank fields parse to an empty contribution.
        let p = parse_prompt_reply(&serde_json::json!({"persona": ""}));
        assert!(p.is_empty());
    }

    #[test]
    fn persona_cap_marks_truncation_b8() {
        let long = "p".repeat(PERSONA_MAX_CHARS + 500);
        let capped = cap_persona(&long);
        assert!(capped.contains("[persona truncated at"));
        assert!(capped.chars().count() < PERSONA_MAX_CHARS + 100);
        assert_eq!(cap_persona("short"), "short");
    }

    #[test]
    fn degraded_notice_prefixes_text() {
        let out = apply_degraded_notice("Here is the answer.".to_owned(), true);
        assert!(out.starts_with(WORKSPACES_UNAVAILABLE_NOTICE));
        assert!(out.contains("Here is the answer."));
    }

    #[test]
    fn degraded_notice_alone_when_text_empty() {
        let out = apply_degraded_notice(String::new(), true);
        assert_eq!(out, WORKSPACES_UNAVAILABLE_NOTICE);
    }

    #[test]
    fn no_notice_when_not_degraded() {
        let out = apply_degraded_notice("untouched".to_owned(), false);
        assert_eq!(out, "untouched");
    }
}

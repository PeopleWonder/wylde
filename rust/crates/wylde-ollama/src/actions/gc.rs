//! Model-store garbage collection — the keep-only-referenced reclaim
//! engine (0.2 stability finding E, issue #100).
//!
//! ## The problem this closes
//!
//! Every model Wylde ever pulls persists forever. `ollama.pull` is the
//! only download path and the only removal verb (`ollama.delete`) is
//! user-clicked — nothing bounds the store or reclaims a model that the
//! configuration has switched away from. Each default-reasoner bump left
//! its multi-GiB predecessor on disk (~16 GB and climbing).
//!
//! ## The structural fix
//!
//! The set of models Wylde *references* is fully derivable from config —
//! the reasoning slots `{embedder, fast, reasoner}` plus any pins — and
//! the inventory (`/api/tags`, with sizes) and the removal primitive
//! (`/api/delete`) already exist. So a **keep-only-referenced** sweep is
//! structurally feasible: diff the inventory against the referenced set,
//! reclaim the unreferenced remainder. Wired to the slot-change seam
//! (`settings.reasoning.set`), a model that a config change dereferences
//! becomes reclaim-eligible **by construction** — no hand-maintained
//! "delete these two tags" list, and a new slot kind inherits the same GC
//! the moment its tag joins the referenced set.
//!
//! ## Safety is the load-bearing property (this deletes user disk data)
//!
//! [`plan_gc`] enforces one invariant above everything: **a model in the
//! `keep` (referenced) set or the `pins` set is NEVER placed in the
//! reclaim list**, in any mode, even if the caller also names it a
//! candidate. Protection is checked first and wins unconditionally. The
//! handler additionally defaults to **dry-run** (announce, never delete)
//! so the caller must opt in explicitly to touch the disk — consistent
//! with Wylde's consent-gate ethos: a self-hosted app never silently
//! deletes a model the user pulled.
//!
//! ## Two modes
//!
//! * **Superseded** (the seam default): `candidates` is `Some(set)` — only
//!   the tags a specific config change just dereferenced are eligible. A
//!   model the user pulled by hand is never in that set, so it is never
//!   touched. This is the conservative policy the slot-change wiring uses.
//! * **Sweep**: `candidates` is `None` — every unreferenced, unpinned
//!   model is eligible (the full keep-only-referenced sweep). Available as
//!   an explicit `ollama.gc` call with no `superseded` field; NOT wired to
//!   auto-run (it would reclaim user-pulled models, a policy call left to
//!   the operator — see the issue's "fuller policy" flag).

use std::collections::BTreeSet;
use std::sync::Arc;

use reqwest::{Method, StatusCode};
use serde::Serialize;
use serde_json::{json, Value};
use wylde_shared::ipc::Reply;

use crate::actions::error::{excerpt, invalid_request, ollama_unreachable_err};
use crate::config::Config;
use crate::upstream::Upstream;

const BODY_EXCERPT_CAP: usize = 300;

/// One installed model as reported by `/api/tags`: the tag and its
/// on-disk size in bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoreModel {
    pub name: String,
    pub size: u64,
}

/// The outcome of planning a GC pass: which models are protected, which
/// are eligible to reclaim, and the byte totals for visibility.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcPlan {
    /// Protected — referenced by a slot or pinned. Never deleted.
    pub keep: Vec<StoreModel>,
    /// Eligible for reclaim (unreferenced, unpinned, and — in superseded
    /// mode — a named candidate).
    pub reclaim: Vec<StoreModel>,
    /// Total on-disk bytes across the whole store.
    pub total_bytes: u64,
    /// Bytes that reclaiming the `reclaim` set would free.
    pub reclaimable_bytes: u64,
}

/// Normalise a model tag for reference matching. Ollama's `/api/tags`
/// reports an implicit `:latest` on an untagged pull (`nomic-embed-text`
/// → `nomic-embed-text:latest`) while a slot may store the bare name, so
/// a trailing `:latest` is stripped on both sides before comparison.
/// Everything else (registry path, explicit tag) is compared verbatim.
fn normalize_tag(tag: &str) -> &str {
    tag.trim()
        .strip_suffix(":latest")
        .unwrap_or_else(|| tag.trim())
}

fn normalized_set(tags: &BTreeSet<String>) -> BTreeSet<&str> {
    tags.iter().map(|t| normalize_tag(t)).collect()
}

/// Plan a GC pass — a pure function over the inventory and the reference
/// sets, so the safety property is unit-testable without a daemon.
///
/// * `inventory` — every installed model (`/api/tags`).
/// * `keep` — the referenced set (slot tags): protected.
/// * `pins` — user-pinned tags: protected.
/// * `candidates` — `Some(set)` restricts eligibility to those tags
///   (superseded mode); `None` makes every unprotected model eligible
///   (sweep mode).
///
/// **Invariant (the load-bearing safety property):** a model whose
/// normalised tag is in `keep ∪ pins` is always placed in `keep`, never
/// in `reclaim`, regardless of `candidates`. Protection is evaluated
/// first and is absolute.
pub fn plan_gc(
    inventory: &[StoreModel],
    keep: &BTreeSet<String>,
    pins: &BTreeSet<String>,
    candidates: Option<&BTreeSet<String>>,
) -> GcPlan {
    let protected: BTreeSet<&str> = normalized_set(keep)
        .union(&normalized_set(pins))
        .copied()
        .collect();
    let candidate_set = candidates.map(normalized_set);

    let mut plan = GcPlan {
        keep: Vec::new(),
        reclaim: Vec::new(),
        total_bytes: 0,
        reclaimable_bytes: 0,
    };
    for model in inventory {
        plan.total_bytes = plan.total_bytes.saturating_add(model.size);
        let norm = normalize_tag(&model.name);
        // Protection wins first and unconditionally.
        if protected.contains(norm) {
            plan.keep.push(model.clone());
            continue;
        }
        // In superseded mode, only named candidates are eligible; every
        // other unreferenced model is left alone (protected by omission).
        let eligible = candidate_set
            .as_ref()
            .map(|c| c.contains(norm))
            .unwrap_or(true);
        if eligible {
            plan.reclaimable_bytes = plan.reclaimable_bytes.saturating_add(model.size);
            plan.reclaim.push(model.clone());
        } else {
            plan.keep.push(model.clone());
        }
    }
    plan
}

/// Parse the `/api/tags` envelope into a `StoreModel` inventory. Entries
/// without a string `name` are skipped; a missing/garbage `size` reads as
/// 0 rather than failing the whole parse.
fn parse_inventory(envelope: &Value) -> Vec<StoreModel> {
    envelope
        .get("models")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let name = m.get("name").and_then(Value::as_str)?.to_owned();
                    let size = m.get("size").and_then(Value::as_u64).unwrap_or(0);
                    Some(StoreModel { name, size })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Pull a payload field that is an array of non-empty strings into a set.
/// A missing field is an empty set; a present-but-not-an-array field is a
/// hard error (the caller fat-fingered the shape and we must not treat it
/// as "protect nothing").
fn string_set(
    payload: &Value,
    field: &str,
) -> Result<BTreeSet<String>, wylde_shared::ipc::IpcError> {
    match payload.get(field) {
        None | Some(Value::Null) => Ok(BTreeSet::new()),
        Some(Value::Array(items)) => Ok(items
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()),
        Some(_) => Err(invalid_request(format!(
            "payload.{field} must be an array of strings"
        ))),
    }
}

fn model_to_value(m: &StoreModel) -> Value {
    json!({ "name": m.name, "size": m.size })
}

/// `ollama.gc` — plan (and optionally perform) a keep-only-referenced
/// reclaim.
///
/// Payload:
/// * `keep`: `[tag, …]` — **required**, the referenced set to protect.
///   Required (not defaulted to empty) so a malformed caller can never
///   accidentally sweep the whole store.
/// * `pins`: `[tag, …]` — optional additional protected tags.
/// * `superseded`: `[tag, …]` — optional. Present ⇒ superseded mode (only
///   these are eligible); absent ⇒ sweep mode (every unreferenced model).
/// * `dry_run`: bool — default **true**. `false` performs the deletes.
///
/// Reply: `{dry_run, mode, total_bytes, model_count, keep, reclaim,
/// reclaimable_bytes, deleted, freed_bytes, errors}`. Every reclaim is
/// logged; a delete that fails is collected in `errors` and the pass
/// continues (fail-soft — one stuck model never blocks the rest).
pub async fn handle_gc(payload: Value, up: Arc<Upstream>) -> Reply {
    if !payload.is_object() {
        return Reply::err(invalid_request("payload must be an object"));
    }
    // `keep` is mandatory: a GC with no referenced set is almost certainly
    // a bug, and defaulting it to empty would make a sweep delete every
    // model. Force the caller to state what is referenced.
    if payload.get("keep").is_none() {
        return Reply::err(invalid_request(
            "payload.keep (array of referenced tags) is required",
        ));
    }
    let keep = match string_set(&payload, "keep") {
        Ok(s) => s,
        Err(e) => return Reply::err(e),
    };
    let pins = match string_set(&payload, "pins") {
        Ok(s) => s,
        Err(e) => return Reply::err(e),
    };
    let superseded_present = payload.get("superseded").is_some()
        && !matches!(payload.get("superseded"), Some(Value::Null));
    let superseded = match string_set(&payload, "superseded") {
        Ok(s) => s,
        Err(e) => return Reply::err(e),
    };
    // Default dry-run: only an explicit `false` touches the disk.
    let dry_run = payload
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let cfg = Config::get();
    let resp = match up
        .request(Method::GET, "/api/tags", None, cfg.list_models_timeout_s)
        .await
    {
        Ok(r) => r,
        Err(e) => return Reply::err(ollama_unreachable_err(&e)),
    };
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Reply::err(crate::actions::error::ollama_http_err(
            status,
            excerpt(&body, BODY_EXCERPT_CAP),
        ));
    }
    let envelope: Value = match resp.bytes().await {
        Ok(b) => match serde_json::from_slice(&b) {
            Ok(v) => v,
            Err(e) => {
                return Reply::err(crate::actions::error::ollama_http_err(
                    200,
                    format!("decode /api/tags failed: {e}"),
                ))
            }
        },
        Err(e) => return Reply::err(ollama_unreachable_err(&e)),
    };
    let inventory = parse_inventory(&envelope);

    let candidates = superseded_present.then_some(&superseded);
    let plan = plan_gc(&inventory, &keep, &pins, candidates);
    let mode = if superseded_present {
        "superseded"
    } else {
        "sweep"
    };

    let keep_json: Vec<Value> = plan.keep.iter().map(model_to_value).collect();
    let reclaim_json: Vec<Value> = plan.reclaim.iter().map(model_to_value).collect();

    // Announce the plan whether or not we delete — visibility (criterion 3):
    // the total store size and what is reclaimable are always logged.
    tracing::info!(
        "ollama.gc: mode={mode} dry_run={dry_run} store={} bytes across {} models; \
         reclaimable={} bytes across {} models ({:?})",
        plan.total_bytes,
        inventory.len(),
        plan.reclaimable_bytes,
        plan.reclaim.len(),
        plan.reclaim
            .iter()
            .map(|m| m.name.as_str())
            .collect::<Vec<_>>(),
    );

    if dry_run {
        return Reply::ok(json!({
            "dry_run": true,
            "mode": mode,
            "total_bytes": plan.total_bytes,
            "model_count": inventory.len(),
            "keep": keep_json,
            "reclaim": reclaim_json,
            "reclaimable_bytes": plan.reclaimable_bytes,
            "deleted": [],
            "freed_bytes": 0,
            "errors": [],
        }));
    }

    // Perform the reclaim: one DELETE /api/delete per eligible model,
    // fail-soft. Each deletion is logged (never a silent removal).
    let mut deleted: Vec<Value> = Vec::new();
    let mut errors: Vec<Value> = Vec::new();
    let mut freed_bytes: u64 = 0;
    for model in &plan.reclaim {
        let body = json!({ "name": model.name });
        match up
            .request(
                Method::DELETE,
                "/api/delete",
                Some(&body),
                cfg.delete_timeout_s,
            )
            .await
        {
            Ok(r) if r.status().is_success() => {
                tracing::info!(
                    "ollama.gc: reclaimed superseded/unreferenced model {} ({} bytes)",
                    model.name,
                    model.size
                );
                freed_bytes = freed_bytes.saturating_add(model.size);
                deleted.push(model_to_value(model));
            }
            Ok(r) => {
                let status = r.status().as_u16();
                let excerpt = if r.status() == StatusCode::NOT_FOUND {
                    // Already gone — treat as reclaimed-by-someone-else,
                    // still an error entry so the caller sees it didn't
                    // free bytes this pass.
                    "model not found (already removed)".to_owned()
                } else {
                    let b = r.text().await.unwrap_or_default();
                    excerpt(&b, BODY_EXCERPT_CAP)
                };
                tracing::warn!(
                    "ollama.gc: delete failed for {} (HTTP {status}): {excerpt}",
                    model.name
                );
                errors.push(json!({ "name": model.name, "status": status, "error": excerpt }));
            }
            Err(e) => {
                tracing::warn!("ollama.gc: delete transport error for {}: {e}", model.name);
                errors.push(json!({ "name": model.name, "error": format!("{e}") }));
            }
        }
    }

    Reply::ok(json!({
        "dry_run": false,
        "mode": mode,
        "total_bytes": plan.total_bytes,
        "model_count": inventory.len(),
        "keep": keep_json,
        "reclaim": reclaim_json,
        "reclaimable_bytes": plan.reclaimable_bytes,
        "deleted": deleted,
        "freed_bytes": freed_bytes,
        "errors": errors,
    }))
}

/// `ollama.store_usage` — the store's total on-disk size and per-model
/// sizes (visibility, criterion 3). Read-only passthrough over
/// `/api/tags` with the sizes summed and sorted largest-first.
/// Reply: `{total_bytes, model_count, models:[{name,size}, …]}`.
pub async fn handle_store_usage(_payload: Value, up: Arc<Upstream>) -> Reply {
    let cfg = Config::get();
    let resp = match up
        .request(Method::GET, "/api/tags", None, cfg.list_models_timeout_s)
        .await
    {
        Ok(r) => r,
        Err(e) => return Reply::err(ollama_unreachable_err(&e)),
    };
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Reply::err(crate::actions::error::ollama_http_err(
            status,
            excerpt(&body, BODY_EXCERPT_CAP),
        ));
    }
    let envelope: Value = match resp.bytes().await {
        Ok(b) => match serde_json::from_slice(&b) {
            Ok(v) => v,
            Err(e) => {
                return Reply::err(crate::actions::error::ollama_http_err(
                    200,
                    format!("decode /api/tags failed: {e}"),
                ))
            }
        },
        Err(e) => return Reply::err(ollama_unreachable_err(&e)),
    };
    let mut inventory = parse_inventory(&envelope);
    inventory.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
    let total: u64 = inventory
        .iter()
        .fold(0u64, |acc, m| acc.saturating_add(m.size));
    let models: Vec<Value> = inventory.iter().map(model_to_value).collect();
    Reply::ok(json!({
        "total_bytes": total,
        "model_count": inventory.len(),
        "models": models,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn m(name: &str, size: u64) -> StoreModel {
        StoreModel {
            name: name.to_owned(),
            size,
        }
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_owned().to_owned()).collect()
    }

    // ── The load-bearing safety property ─────────────────────────────
    // Referenced (keep) AND pinned models are NEVER reclaimed, even when
    // the caller also lists them as candidates. Protection is absolute.

    #[test]
    fn referenced_and_pinned_are_never_reclaimed_even_if_candidates() {
        let inv = vec![m("X", 100), m("Y", 200), m("Z", 300)];
        // Candidates deliberately name ALL three — an adversarial caller.
        // Y is referenced (keep), Z is pinned. Only X may be reclaimed.
        let plan = plan_gc(
            &inv,
            &set(&["Y"]),
            &set(&["Z"]),
            Some(&set(&["X", "Y", "Z"])),
        );
        let reclaim: Vec<&str> = plan.reclaim.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(reclaim, vec!["X"], "only the unreferenced, unpinned model");
        let kept: BTreeSet<&str> = plan.keep.iter().map(|m| m.name.as_str()).collect();
        assert!(kept.contains("Y"), "referenced model protected");
        assert!(kept.contains("Z"), "pinned model protected");
        assert_eq!(plan.reclaimable_bytes, 100);
        assert_eq!(plan.total_bytes, 600);
    }

    #[test]
    fn switching_reasoner_supersedes_the_old_tag_only() {
        // Reasoner switched X → Y. Inventory still has both. keep = {Y}
        // (the new referenced set); superseded = {X}. X reclaimed, Y kept.
        let inv = vec![m("X", 13_000), m("Y", 9_000), m("nomic", 500)];
        let plan = plan_gc(
            &inv,
            &set(&["Y", "nomic"]),
            &BTreeSet::new(),
            Some(&set(&["X"])),
        );
        let reclaim: Vec<&str> = plan.reclaim.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(reclaim, vec!["X"]);
    }

    #[test]
    fn superseded_mode_leaves_user_pulled_models_alone() {
        // A model the user pulled by hand ("mistral") is unreferenced but
        // NOT a candidate — superseded mode must not touch it.
        let inv = vec![m("X", 100), m("mistral", 4_000), m("Y", 200)];
        let plan = plan_gc(&inv, &set(&["Y"]), &BTreeSet::new(), Some(&set(&["X"])));
        let reclaim: Vec<&str> = plan.reclaim.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(reclaim, vec!["X"], "mistral (user-pulled) untouched");
    }

    #[test]
    fn sweep_mode_reclaims_every_unreferenced_model() {
        // candidates=None ⇒ every unreferenced, unpinned model is eligible.
        let inv = vec![m("X", 100), m("mistral", 4_000), m("Y", 200)];
        let plan = plan_gc(&inv, &set(&["Y"]), &BTreeSet::new(), None);
        let mut reclaim: Vec<&str> = plan.reclaim.iter().map(|m| m.name.as_str()).collect();
        reclaim.sort();
        assert_eq!(reclaim, vec!["X", "mistral"]);
        assert_eq!(plan.reclaimable_bytes, 4_100);
    }

    #[test]
    fn latest_tag_is_normalised_on_both_sides() {
        // Inventory reports the implicit :latest; the slot stores the bare
        // name. They must match so the referenced embedder is protected.
        let inv = vec![m("nomic-embed-text:latest", 500), m("old", 100)];
        let plan = plan_gc(&inv, &set(&["nomic-embed-text"]), &BTreeSet::new(), None);
        let reclaim: Vec<&str> = plan.reclaim.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(reclaim, vec!["old"], "nomic protected across :latest");
    }

    #[test]
    fn empty_inventory_reclaims_nothing() {
        let plan = plan_gc(&[], &set(&["Y"]), &BTreeSet::new(), None);
        assert!(plan.reclaim.is_empty());
        assert_eq!(plan.total_bytes, 0);
    }

    #[test]
    fn parse_inventory_is_defensive() {
        let env = json!({"models": [
            {"name": "a", "size": 100},
            {"name": "b"},                 // missing size ⇒ 0
            {"size": 999},                 // missing name ⇒ skipped
            {"name": "c", "size": "big"},  // garbage size ⇒ 0
        ]});
        let inv = parse_inventory(&env);
        assert_eq!(inv, vec![m("a", 100), m("b", 0), m("c", 0)]);
    }

    // ── Handler tests (wiremock) ─────────────────────────────────────

    async fn fake_upstream() -> (MockServer, Arc<Upstream>) {
        let server = MockServer::start().await;
        let up = crate::upstream::for_test(&server.uri());
        (server, up)
    }

    fn tags_body() -> Value {
        json!({"models": [
            {"name": "X", "size": 13_000},
            {"name": "Y", "size": 9_000},
            {"name": "nomic-embed-text:latest", "size": 500},
        ]})
    }

    #[tokio::test]
    async fn gc_dry_run_deletes_nothing() {
        let (server, up) = fake_upstream().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(tags_body()))
            .mount(&server)
            .await;
        // No DELETE mock is mounted: if the handler tried to delete, the
        // request would 404 against wiremock and show in errors. dry_run
        // must not issue any delete at all.
        let reply = handle_gc(
            json!({"keep": ["Y", "nomic-embed-text"], "superseded": ["X"], "dry_run": true}),
            up,
        )
        .await;
        assert!(reply.ok);
        assert_eq!(reply.data["dry_run"], true);
        assert_eq!(reply.data["mode"], "superseded");
        assert_eq!(reply.data["reclaim"].as_array().unwrap().len(), 1);
        assert_eq!(reply.data["reclaim"][0]["name"], "X");
        assert_eq!(reply.data["reclaimable_bytes"], 13_000);
        assert_eq!(reply.data["total_bytes"], 22_500);
        assert!(reply.data["deleted"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn gc_perform_reclaims_superseded_and_never_referenced() {
        let (server, up) = fake_upstream().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(tags_body()))
            .mount(&server)
            .await;
        // Only X may be deleted. Y and nomic are referenced. Mounting the
        // DELETE mock ONLY for {name:"X"} means a delete of anything else
        // 404s and lands in errors — a hard proof the referenced models
        // are never targeted.
        Mock::given(method("DELETE"))
            .and(path("/api/delete"))
            .and(body_json(json!({"name": "X"})))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let reply = handle_gc(
            json!({"keep": ["Y", "nomic-embed-text"], "superseded": ["X"], "dry_run": false}),
            up,
        )
        .await;
        assert!(reply.ok);
        assert_eq!(reply.data["dry_run"], false);
        let deleted = reply.data["deleted"].as_array().unwrap();
        assert_eq!(deleted.len(), 1);
        assert_eq!(deleted[0]["name"], "X");
        assert_eq!(reply.data["freed_bytes"], 13_000);
        assert!(
            reply.data["errors"].as_array().unwrap().is_empty(),
            "no referenced model was ever targeted: {:?}",
            reply.data["errors"]
        );
    }

    #[tokio::test]
    async fn gc_requires_keep_field() {
        let up = crate::upstream::for_test("http://127.0.0.1:1");
        let reply = handle_gc(json!({"superseded": ["X"]}), up).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "invalid_request");
    }

    #[tokio::test]
    async fn gc_rejects_non_array_keep() {
        let up = crate::upstream::for_test("http://127.0.0.1:1");
        let reply = handle_gc(json!({"keep": "Y"}), up).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "invalid_request");
    }

    #[tokio::test]
    async fn store_usage_sums_and_sorts_desc() {
        let (server, up) = fake_upstream().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(tags_body()))
            .mount(&server)
            .await;
        let reply = handle_store_usage(Value::Null, up).await;
        assert!(reply.ok);
        assert_eq!(reply.data["total_bytes"], 22_500);
        assert_eq!(reply.data["model_count"], 3);
        // Largest first.
        assert_eq!(reply.data["models"][0]["name"], "X");
        assert_eq!(reply.data["models"][2]["name"], "nomic-embed-text:latest");
    }
}

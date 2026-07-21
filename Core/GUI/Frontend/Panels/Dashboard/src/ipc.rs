//! Per-panel IPC helpers for the Dashboard panel.
//!
//! Each card on the Dashboard reads from a different service:
//!
//!   * **Service health strip** — fires `service.health` on
//!     `wylde-lifecycle` for every service the strip covers (see
//!     [`strip_services`], derived from the stack roster rather than a
//!     hand-kept list). Failures land as a red dot; absent means "haven't
//!     probed yet" and renders as a neutral grey.
//!   * **Hardware card** — `system.inventory` on `wylde-vram-broker`,
//!     projected to a small struct (full envelope is bigger than the
//!     card needs).
//!   * **Active model card** — `ollama.list_loaded` reports what's
//!     resident in VRAM right now; the first row is "the model the
//!     chat is using" close enough for a glance.
//!   * **Recent activity card** — `memory.long_term.list` for the
//!     most-recently-touched curated memories.  The harness sorts by
//!     importance + recency so the first N is the right list to show.
//!
//! Every soft-fails: an individual service being down doesn't break
//! the dashboard, it just shows the relevant card in a degraded state.

use serde_json::{json, Value};
use wylde_stack::roster::{roster, StackBinary, Tier};

pub const SVC_LIFECYCLE: &str = "wylde-lifecycle";
pub const SVC_BROKER: &str = "wylde-vram-broker";
pub const SVC_HARNESS: &str = "wylde-harness";
pub const SVC_OLLAMA: &str = "wylde-ollama";

/// Services the strip monitors that own no roster row, kept by name.
///
/// A roster row means a shippable Wylde binary. These services have none —
/// they are a deliberate `image: None` in [`wylde_stack::CORE_STACK`] and so
/// are absent from [`roster`] — yet they are daemon-managed and carry a real
/// health signal, so the strip must still probe them. This is exactly the
/// idiom `wylde_stack::shutdown_targets::NON_ROSTER_GUI_IMAGES` uses for the
/// GUI's kill path: derive the bulk, keep the genuine exceptions *typed and
/// documented* rather than silent.
///
/// * `wylde-memgraph` — JVM-supervised (no `wylde-memgraph.exe`). Its liveness
///   is "is Neo4j accepting Bolt connections?"
///   (`wylde_lifecycle::control::memgraph_health`), a signal the operator
///   needs. It is a `Role::Standard` `DAEMON_MANAGED` row with a stop hook, so
///   it also offers Stop — hence [`StripService::manageable`] is `true` for it.
///
/// `wylde-memory-scheduler` is *not* here: it is a `Role::BootOnlyNoop`
/// in-process tokio task inside `wylde-harness` with no subprocess and nothing
/// to probe or stop, so it correctly never appears on the strip.
const NON_ROSTER_MONITORED: &[&str] = &["wylde-memgraph"];

/// One row of the service-health strip: a service name plus whether the
/// console may offer it a Stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripService {
    pub name: String,
    /// Whether a Stop is a live backend action — the service's *role*, not a
    /// name literal. `true` for every daemon-managed service (a roster
    /// [`Tier::Service`] row, plus the managed [`NON_ROSTER_MONITORED`]
    /// carve-outs); `false` for the daemon itself ([`Tier::Daemon`],
    /// `wylde-lifecycle`), which serves the stop and is not in its own
    /// manageable set, so a Stop there is a backend no-op.
    pub manageable: bool,
}

/// Derive the strip's service list from a stack roster, in render order.
///
/// The GUI shell ([`Tier::Gui`]) is dropped — it renders the strip; there is
/// nothing here to probe or stop. The daemon and every service — in-tree or a
/// discovered `Services/` sibling — get a row, and so does each
/// [`NON_ROSTER_MONITORED`] carve-out. Order is the roster's own stable order
/// (daemon, in-tree declaration order, then siblings alphabetical), then the
/// carve-outs; the exact order is a per-product concern, which is the only
/// thing the old hand-kept list legitimately encoded.
///
/// Membership is *derived*: a new binary — including a future `Services/`
/// sibling — gets a chip, and a Stop button where its role warrants one, with
/// no edit here. That is the defect this closes (#123): the strip no longer
/// drifts from the roster the updater and launcher already follow.
pub fn strip_services_from(roster: &[StackBinary]) -> Vec<StripService> {
    let mut out: Vec<StripService> = roster
        .iter()
        .filter(|b| b.tier != Tier::Gui)
        .map(|b| StripService {
            name: b.name.clone(),
            manageable: b.tier == Tier::Service,
        })
        .collect();
    for name in NON_ROSTER_MONITORED {
        // Guard against a future roster row shadowing a carve-out.
        if out.iter().any(|s| &s.name == name) {
            continue;
        }
        out.push(StripService {
            name: (*name).to_owned(),
            // The carve-outs listed here are all daemon-managed (see the
            // `NON_ROSTER_MONITORED` doc) — they offer Stop.
            manageable: true,
        });
    }
    out
}

/// The live strip roster, walked from the install tree. Thin wrapper over
/// [`strip_services_from`] against [`wylde_stack::roster`]; the pure function
/// is the tempdir-testable seam the falsification test drives.
pub fn strip_services() -> Vec<StripService> {
    strip_services_from(&roster())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    Unknown,
    Healthy,
    /// Pipe is up but the service is in a partially-degraded state — e.g.
    /// `wylde-ollama`'s wrapper answers but its upstream Ollama daemon is
    /// unreachable or slow. Renders yellow. The accompanying detail string
    /// (see [`ServiceHealth::detail`]) explains the specific degradation.
    Degraded,
    Unhealthy,
}

impl HealthStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Unhealthy => "unhealthy",
        }
    }
}

/// A service's projected health plus an optional hover/tooltip detail.
///
/// `detail` is `Some` only for states that warrant an explanation — today
/// the degraded ollama tile ("Ollama daemon unreachable…", "Slow response
/// (>2s)"). Green/red tiles carry `None`: the dot colour already says it.
/// Tri-state lives here (rather than on a shared widget) because the dot
/// is rendered inline by the Dashboard's `service_chip`; any future
/// service can opt into yellow simply by returning `Degraded` + a detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceHealth {
    pub status: HealthStatus,
    pub detail: Option<String>,
}

impl ServiceHealth {
    /// A bare status with no hover detail (the common green/red case).
    pub fn plain(status: HealthStatus) -> Self {
        Self {
            status,
            detail: None,
        }
    }
}

/// Latency (ms) above which a *reachable* upstream is still flagged
/// degraded/yellow. Mirrors the "Slow response (>2s)" hover copy.
pub const OLLAMA_SLOW_MS: u64 = 2000;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HardwareCard {
    pub cpu_brand: String,
    pub cpu_cores: u32,
    /// Bytes — humanise at render time.
    pub ram_total_bytes: u64,
    pub ram_available_bytes: u64,
    /// Largest single NVIDIA card's VRAM in bytes.
    pub nvidia_vram_bytes: u64,
    /// VRAM currently held by tracked leases.  When the broker is up
    /// this lets the card show "used / total" rather than just "total".
    pub nvidia_vram_used_bytes: u64,
    pub nvidia_count: u32,
    pub intel_count: u32,
    pub amd_count: u32,
    pub has_npu: bool,
    /// Largest disk's free bytes; the card shows "free disk" because
    /// that's the metric that matters for "can I pull this model?"
    pub free_disk_bytes: u64,
}

impl HardwareCard {
    pub fn from_value(v: &Value) -> Self {
        let cpu = v.get("cpu").cloned().unwrap_or_else(|| json!({}));
        let cpu_brand = cpu
            .get("brand")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_owned();
        let cpu_cores = cpu
            .get("logical_cores")
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as u32;
        let ram_total_bytes = v
            .get("memory_total_bytes")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let ram_available_bytes = v
            .get("memory_available_bytes")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let gpus = v
            .get("gpus")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        let nvidia_count = gpus.len() as u32;
        let (nvidia_vram_bytes, nvidia_vram_used_bytes) = gpus
            .iter()
            .map(|g| {
                let total = g
                    .get("memory_total_bytes")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0);
                let free = g
                    .get("memory_free_bytes")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(total);
                (total, total.saturating_sub(free))
            })
            .max_by_key(|(total, _)| *total)
            .unwrap_or((0, 0));
        let intel_count = v
            .get("intel_gpus")
            .and_then(|x| x.as_array())
            .map(|a| a.len() as u32)
            .unwrap_or(0);
        let amd_count = v
            .get("amd_gpus")
            .and_then(|x| x.as_array())
            .map(|a| a.len() as u32)
            .unwrap_or(0);
        let has_npu = v
            .get("npus")
            .and_then(|x| x.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        let free_disk_bytes = v
            .get("disks")
            .and_then(|x| x.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|d| d.get("available_bytes").and_then(|x| x.as_u64()))
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        Self {
            cpu_brand,
            cpu_cores,
            ram_total_bytes,
            ram_available_bytes,
            nvidia_vram_bytes,
            nvidia_vram_used_bytes,
            nvidia_count,
            intel_count,
            amd_count,
            has_npu,
            free_disk_bytes,
        }
    }

    pub fn is_unknown(&self) -> bool {
        self.cpu_brand.is_empty() && self.ram_total_bytes == 0 && self.nvidia_count == 0
    }
}

/// One row off `ollama.list_loaded` (`/api/ps`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LoadedModel {
    pub name: String,
    /// Approximate VRAM the model occupies.  Ollama reports
    /// `size_vram` on a recent build; 0 when the field is missing.
    pub size_vram_bytes: u64,
    /// Wall-clock string for when the model is scheduled to fall out
    /// of VRAM unless touched again.  Empty when omitted.
    pub expires_at: String,
}

impl LoadedModel {
    pub fn from_value(v: &Value) -> Self {
        Self {
            name: v
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            size_vram_bytes: v.get("size_vram").and_then(|x| x.as_u64()).unwrap_or(0),
            expires_at: v
                .get("expires_at")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
        }
    }
}

/// One recent long-term memory record.  Subset of the Memory panel's
/// `LongTermRecord` — the Dashboard's "recent" surface only needs
/// enough to render a single line per row.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecentMemory {
    pub id: String,
    pub body: String,
    pub source: String,
    /// Unix seconds, last touch.
    pub last_used_at: f64,
    pub created_at: f64,
    pub importance: i32,
}

impl RecentMemory {
    pub fn from_value(v: &Value) -> Self {
        Self {
            id: v
                .get("id")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            body: v
                .get("body")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            source: v
                .get("source")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            last_used_at: v
                .get("last_used_at")
                .and_then(|x| x.as_f64())
                .unwrap_or(0.0),
            created_at: v.get("created_at").and_then(|x| x.as_f64()).unwrap_or(0.0),
            importance: v
                .get("importance")
                .and_then(|x| x.as_i64())
                .map(|n| n as i32)
                .unwrap_or(0),
        }
    }
}

// ── Unary verbs ──────────────────────────────────────────────────────

/// Probe one service via `lifecycle.service.health`.  Returns a
/// [`ServiceHealth`] rather than `Result<_, _>` because the dashboard
/// wants "unhealthy" and "couldn't reach the daemon" to look the same
/// from the strip's point of view.
pub async fn probe_service(name: &str) -> ServiceHealth {
    let outcome = wylde_gui_pipe::call(
        SVC_LIFECYCLE,
        "POST",
        "/__action__",
        Some(json!({
            "action": "service.health",
            "payload": { "name": name },
        })),
    )
    .await;
    match outcome {
        // The lifecycle daemon answered `ok` — project the body into a
        // health state. Pure logic lives in `project_health` so it's unit
        // testable without a live pipe.
        Ok(v) => project_health(name, &v),
        // Couldn't reach the daemon (or it replied not-ok, which
        // `wylde_gui_pipe::call` surfaces as `Err`) — red.
        Err(_) => ServiceHealth::plain(HealthStatus::Unhealthy),
    }
}

/// Project a `service.health` ok-reply body into a [`ServiceHealth`].
///
/// For `wylde-ollama` the lifecycle daemon composes the wrapper pipe
/// liveness with the upstream Ollama probe (see
/// `wylde_lifecycle::control::ollama_health`), so the body carries a
/// nested `reply.upstream` / `reply.latency_ms` we fold into the
/// tri-state. Every other service keeps the original forgiving shape
/// (`{ok}` / `{healthy}` / `{status}`).
fn project_health(name: &str, v: &Value) -> ServiceHealth {
    if name == SVC_OLLAMA {
        if let Some(reply) = v.get("reply") {
            if reply.get("upstream").and_then(|x| x.as_str()).is_some() {
                return project_ollama_health(reply);
            }
        }
    }
    // Accept any of the common positive shapes (`{ok: true}`, `{healthy:
    // true}`, `{status: "running"}`) and treat anything else as unhealthy
    // — same forgiving approach the Shell's startup probe uses.
    if let Some(b) = v.get("ok").and_then(|x| x.as_bool()) {
        return ServiceHealth::plain(if b {
            HealthStatus::Healthy
        } else {
            HealthStatus::Unhealthy
        });
    }
    if let Some(b) = v.get("healthy").and_then(|x| x.as_bool()) {
        return ServiceHealth::plain(if b {
            HealthStatus::Healthy
        } else {
            HealthStatus::Unhealthy
        });
    }
    if let Some(s) = v.get("status").and_then(|x| x.as_str()) {
        return ServiceHealth::plain(match s {
            "running" | "ok" | "healthy" | "up" => HealthStatus::Healthy,
            "stopped" | "down" | "unhealthy" | "error" => HealthStatus::Unhealthy,
            _ => HealthStatus::Unknown,
        });
    }
    // Lifecycle returned ok with no recognisable field — call it healthy
    // on the principle that the daemon answered.
    ServiceHealth::plain(HealthStatus::Healthy)
}

/// Fold `wylde-ollama`'s composed `reply` blob (`{upstream, latency_ms,
/// ...}`) into a tri-state:
///   * upstream `ok` + fast → green (`Healthy`)
///   * upstream `ok` but ≥ [`OLLAMA_SLOW_MS`] → yellow ("Slow response")
///   * upstream `unreachable` / `timeout` → yellow (daemon down) — the
///     wrapper pipe is still up, so it's degraded, not red
///   * anything unrecognised → green (the wrapper answered)
fn project_ollama_health(reply: &Value) -> ServiceHealth {
    let upstream = reply.get("upstream").and_then(|x| x.as_str()).unwrap_or("");
    let latency_ms = reply.get("latency_ms").and_then(|x| x.as_u64());
    match upstream {
        "ok" => {
            if matches!(latency_ms, Some(ms) if ms >= OLLAMA_SLOW_MS) {
                ServiceHealth {
                    status: HealthStatus::Degraded,
                    detail: Some("Slow response (>2s)".to_owned()),
                }
            } else {
                ServiceHealth::plain(HealthStatus::Healthy)
            }
        }
        "unreachable" | "timeout" => ServiceHealth {
            status: HealthStatus::Degraded,
            detail: Some("Ollama daemon unreachable at 127.0.0.1:11434".to_owned()),
        },
        _ => ServiceHealth::plain(HealthStatus::Healthy),
    }
}

pub async fn read_hardware_card() -> Result<HardwareCard, String> {
    let v = wylde_gui_pipe::call(
        SVC_BROKER,
        "POST",
        "/__action__",
        Some(json!({ "action": "system.inventory", "payload": {} })),
    )
    .await?;
    Ok(HardwareCard::from_value(&v))
}

pub async fn read_loaded_models() -> Result<Vec<LoadedModel>, String> {
    let v = wylde_gui_pipe::call(
        SVC_OLLAMA,
        "POST",
        "/__action__",
        Some(json!({ "action": "ollama.list_loaded", "payload": {} })),
    )
    .await?;
    let Some(arr) = v.get("models").and_then(|x| x.as_array()) else {
        return Ok(Vec::new());
    };
    Ok(arr.iter().map(LoadedModel::from_value).collect())
}

/// Read the top N recently-touched curated memories.  We piggy-back on
/// `memory.long_term.list` (sorted by importance/recency on the harness
/// side) and clip to `limit` at the panel.
pub async fn read_recent_memories(limit: usize) -> Result<Vec<RecentMemory>, String> {
    let v = wylde_gui_pipe::call(
        SVC_HARNESS,
        "POST",
        "/__action__",
        Some(json!({ "action": "memory.long_term.list", "payload": {} })),
    )
    .await?;
    let Some(arr) = v.get("memories").and_then(|x| x.as_array()) else {
        return Ok(Vec::new());
    };
    Ok(arr
        .iter()
        .take(limit)
        .map(RecentMemory::from_value)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_card_parses_phase_12_4_shape() {
        let v = json!({
            "cpu": { "brand": "Intel Core i9", "logical_cores": 16 },
            "memory_total_bytes": 32_u64 * 1024 * 1024 * 1024,
            "memory_available_bytes": 24_u64 * 1024 * 1024 * 1024,
            "gpus": [{
                "memory_total_bytes": 24_u64 * 1024 * 1024 * 1024,
                "memory_free_bytes": 20_u64 * 1024 * 1024 * 1024,
            }],
            "intel_gpus": [{}],
            "amd_gpus": [],
            "npus": [{}],
            "disks": [
                { "available_bytes": 100_u64 * 1024 * 1024 * 1024 },
                { "available_bytes": 50_u64 * 1024 * 1024 * 1024 },
            ],
        });
        let hw = HardwareCard::from_value(&v);
        assert_eq!(hw.cpu_brand, "Intel Core i9");
        assert_eq!(hw.cpu_cores, 16);
        assert_eq!(hw.nvidia_count, 1);
        assert_eq!(hw.nvidia_vram_bytes, 24_u64 * 1024 * 1024 * 1024);
        assert_eq!(hw.nvidia_vram_used_bytes, 4_u64 * 1024 * 1024 * 1024);
        assert_eq!(hw.intel_count, 1);
        assert!(hw.has_npu);
        assert_eq!(hw.free_disk_bytes, 100_u64 * 1024 * 1024 * 1024);
        assert!(!hw.is_unknown());
    }

    #[test]
    fn hardware_card_is_unknown_for_empty_envelope() {
        let hw = HardwareCard::from_value(&json!({}));
        assert!(hw.is_unknown());
    }

    #[test]
    fn loaded_model_parses_passthrough_envelope() {
        let v = json!({
            "name": "qwen2.5:1.5b",
            "size_vram": 1_500_000_000_u64,
            "expires_at": "2026-05-29T11:30:00Z",
        });
        let m = LoadedModel::from_value(&v);
        assert_eq!(m.name, "qwen2.5:1.5b");
        assert_eq!(m.size_vram_bytes, 1_500_000_000);
    }

    #[test]
    fn recent_memory_parses_long_term_row() {
        let v = json!({
            "id": "abcd",
            "body": "the Wylde user prefers Bash on Windows",
            "source": "settings_ui",
            "importance": 7,
            "created_at": 1_700_000_000_f64,
            "last_used_at": 1_700_001_000_f64,
        });
        let r = RecentMemory::from_value(&v);
        assert_eq!(r.id, "abcd");
        assert_eq!(r.importance, 7);
        assert!(r.last_used_at > r.created_at);
    }

    #[test]
    fn health_status_strings_are_stable() {
        assert_eq!(HealthStatus::Unknown.as_str(), "unknown");
        assert_eq!(HealthStatus::Healthy.as_str(), "healthy");
        assert_eq!(HealthStatus::Degraded.as_str(), "degraded");
        assert_eq!(HealthStatus::Unhealthy.as_str(), "unhealthy");
    }

    #[test]
    fn project_health_generic_ok_is_healthy() {
        let h = project_health(
            "wylde-harness",
            &json!({"name": "wylde-harness", "reply": {"pong": true}}),
        );
        // Generic services have no `ok`/`healthy`/`status` at top level and
        // no upstream — the daemon answered, so healthy with no detail.
        assert_eq!(h, ServiceHealth::plain(HealthStatus::Healthy));
    }

    #[test]
    fn project_health_respects_explicit_status_field() {
        let down = project_health("wylde-x", &json!({"status": "down"}));
        assert_eq!(down, ServiceHealth::plain(HealthStatus::Unhealthy));
        let up = project_health("wylde-x", &json!({"ok": true}));
        assert_eq!(up, ServiceHealth::plain(HealthStatus::Healthy));
    }

    #[test]
    fn ollama_upstream_ok_fast_is_green() {
        let reply = json!({"name": "wylde-ollama", "reply": {"ok": true, "upstream": "ok", "latency_ms": 120}});
        let h = project_health("wylde-ollama", &reply);
        assert_eq!(h.status, HealthStatus::Healthy);
        assert!(h.detail.is_none());
    }

    #[test]
    fn ollama_upstream_ok_slow_is_yellow_with_detail() {
        let reply = json!({"name": "wylde-ollama", "reply": {"ok": true, "upstream": "ok", "latency_ms": 2500}});
        let h = project_health("wylde-ollama", &reply);
        assert_eq!(h.status, HealthStatus::Degraded);
        assert_eq!(h.detail.as_deref(), Some("Slow response (>2s)"));
    }

    #[test]
    fn ollama_upstream_unreachable_is_yellow_with_detail() {
        for state in ["unreachable", "timeout"] {
            let reply = json!({"name": "wylde-ollama", "reply": {"ok": true, "upstream": state}});
            let h = project_health("wylde-ollama", &reply);
            assert_eq!(h.status, HealthStatus::Degraded, "state={state}");
            assert_eq!(
                h.detail.as_deref(),
                Some("Ollama daemon unreachable at 127.0.0.1:11434"),
                "state={state}",
            );
        }
    }

    #[test]
    fn ollama_without_composed_reply_falls_back_to_generic() {
        // Older lifecycle (pre-compose) returns `{name, reply: {pong}}`
        // with no `upstream` — must not crash, just project healthy.
        let reply = json!({"name": "wylde-ollama", "reply": {"pong": true}});
        let h = project_health("wylde-ollama", &reply);
        assert_eq!(h.status, HealthStatus::Healthy);
    }

    #[test]
    fn ollama_slow_threshold_is_exactly_2s() {
        // Frozen so a future tweak surfaces in review and stays aligned
        // with the "(>2s)" hover copy.
        assert_eq!(OLLAMA_SLOW_MS, 2000);
        // Exactly at the threshold counts as slow (>= comparison).
        let at = project_ollama_health(&json!({"upstream": "ok", "latency_ms": 2000}));
        assert_eq!(at.status, HealthStatus::Degraded);
        let under = project_ollama_health(&json!({"upstream": "ok", "latency_ms": 1999}));
        assert_eq!(under.status, HealthStatus::Healthy);
    }

    #[test]
    fn strip_covers_every_binary_backed_service_in_the_roster() {
        // #123: the strip must derive from the roster, so *every* service the
        // roster carries a binary for appears — not a hand-kept subset. This
        // is the criterion that MONITORED_SERVICES could not meet: it was
        // short by four (workspaces, treesitter, n8n, vpn).
        let strip = strip_services();
        let names: Vec<&str> = strip.iter().map(|s| s.name.as_str()).collect();
        for b in roster() {
            if b.tier == Tier::Service {
                assert!(
                    names.contains(&b.name.as_str()),
                    "roster service {} has no strip row — the strip drifted from the roster",
                    b.name,
                );
            }
        }
        // The four the hand-kept list silently dropped, named explicitly so a
        // regression reads plainly.
        for svc in [
            "wylde-workspaces",
            "wylde-treesitter",
            "wylde-n8n",
            "wylde-vpn",
        ] {
            assert!(names.contains(&svc), "missing {svc} on the strip");
        }
        // The daemon shows health but is never offered a Stop; the managed
        // services are.
        let daemon = strip
            .iter()
            .find(|s| s.name == "wylde-lifecycle")
            .expect("daemon on the strip");
        assert!(!daemon.manageable, "the daemon must not offer Stop");
        let gateway = strip
            .iter()
            .find(|s| s.name == "wylde-gateway")
            .expect("gateway on the strip");
        assert!(gateway.manageable, "a managed service offers Stop");
    }

    #[test]
    fn strip_carries_non_roster_monitored_services() {
        // memgraph has no binary (JVM-supervised, `image: None`) so it never
        // appears in `roster()`, but it is daemon-managed with a real Bolt
        // health signal — the typed carve-out keeps it on the strip, with Stop.
        let strip = strip_services();
        let memgraph = strip
            .iter()
            .find(|s| s.name == "wylde-memgraph")
            .expect("memgraph stays on the strip via the carve-out");
        assert!(
            memgraph.manageable,
            "memgraph is daemon-managed → offers Stop"
        );
    }

    #[test]
    fn strip_never_lists_the_gui_shell() {
        // The GUI renders the strip; probing/stopping itself is nonsense.
        assert!(
            !strip_services().iter().any(|s| s.name == "wylde-gui"),
            "the GUI shell must not appear on its own strip",
        );
    }

    #[test]
    fn dropping_a_services_bucket_sibling_extends_the_strip_with_no_code_edit() {
        // The maintainer's bar: a synthetic service dropped into the
        // `Services/` bucket is covered by construction. Revert
        // `strip_services_from` to a hand-kept list and this goes red — the
        // synthetic (and every real sibling) would vanish. Mirrors the
        // discovery seam `wylde_updater/tests/whole_stack_coverage.rs` drives.
        let tmp = tempfile::tempdir().unwrap();
        let svc_dir = tmp.path().join("Services").join("wylde-synthetic-probe");
        std::fs::create_dir_all(&svc_dir).unwrap();
        std::fs::write(
            svc_dir.join("manifest.json"),
            r#"{"name":"synthetic-probe"}"#,
        )
        .unwrap();

        let roster = wylde_stack::roster::roster_in(tmp.path());
        let strip = strip_services_from(&roster);
        let names: Vec<&str> = strip.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"wylde-synthetic-probe"),
            "a dropped-in Services/ sibling must appear on the strip automatically; got {names:?}",
        );
        // And it is manageable (a discovered sibling is stoppable via the
        // daemon's generic teardown), never the GUI.
        let synth = strip
            .iter()
            .find(|s| s.name == "wylde-synthetic-probe")
            .unwrap();
        assert!(synth.manageable, "a discovered sibling offers Stop");
    }

    #[test]
    fn pipe_helpers_exist() {
        let _ = probe_service;
        let _ = read_hardware_card;
        let _ = read_loaded_models;
        let _ = read_recent_memories;
    }
}

//! Phase 12.2 — per-tool consent gates.
//!
//! ## Model
//!
//! A tool dispatch flows through three gates in order:
//!
//! 1. Registry lookup (`Registry::lookup` — was the tool name resolvable?)
//! 2. Tier gate (`runner::check_registry_tier` — does the turn's device
//!    tier permit this tool's destructive flag?)
//! 3. **Consent gate** (this module — has the user approved this tool?)
//!
//! The consent gate runs **after** the tier gate so a tool the tier
//! would refuse anyway never produces a consent prompt — that would be
//! noise the user can't act on.
//!
//! ## Decision shape
//!
//! Per-tool, the store records one of three states:
//!
//! * `Approved` — persistent allow. Tool runs without prompting.
//! * `Denied` — persistent deny. Tool dispatch returns `consent_denied`
//!   without ever invoking the handler.
//! * absent — no decision yet. Dispatch returns `consent_required` with
//!   a human-readable prompt; the GUI surfaces it, the user picks an
//!   answer, the GUI calls `consent.respond` to record the decision.
//!
//! Plus a global `no_auth` flag — when `true`, every tool is approved
//! without prompting. the Wylde user asked for this as a power-user escape hatch:
//! after the alpha settles, set `no_auth: true` and stop seeing prompts.
//! Default is `false` (always ask). Default on every new install: ON.
//!
//! ## Persistence
//!
//! Stored at `<wylde_root>/data/preferences/consent.json`. The file is
//! created on first write; absent means "empty store, ask for every
//! tool." JSON shape:
//!
//! ```json
//! {
//!   "no_auth": false,
//!   "tools": {
//!     "fs.write_file": "approved",
//!     "memory.long_term.delete": "denied"
//!   }
//! }
//! ```
//!
//! ## Concurrency
//!
//! One process-wide [`ConsentStore`] behind a mutex. Reads + writes are
//! cheap (the catalog is small — ~50 tools max). Disk writes are
//! atomic-rename via `wylde_shared::secure_file::atomic_write`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::config::Config;

/// Per-tool consent decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Approved,
    Denied,
}

impl Decision {
    pub fn as_wire(&self) -> &'static str {
        match self {
            Decision::Approved => "approved",
            Decision::Denied => "denied",
        }
    }
}

/// On-disk shape. Mirrors what the GUI reads/writes; the BTreeMap keeps
/// per-tool entries in alphabetical order so the file diffs cleanly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConsentFile {
    #[serde(default)]
    pub no_auth: bool,
    #[serde(default)]
    pub tools: BTreeMap<String, Decision>,
}

/// Outcome of a single gate check. Maps onto the runner's
/// `DispatchError` taxonomy plus a "let it through" variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    /// Tool is approved (per-tool decision or global no_auth). Dispatch
    /// proceeds.
    Allow,
    /// Tool is denied (persistent decision). Dispatch returns a
    /// `consent_denied` error without invoking the handler.
    Deny { reason: String },
    /// No decision yet. Dispatch returns `consent_required` with the
    /// prompt; the GUI prompts the user, who responds via
    /// `consent.respond`. The next dispatch retries the gate.
    Pending { prompt: String },
}

/// Process-wide consent store. One mutex over the in-memory file shape;
/// disk I/O happens on `set` / `set_no_auth` / `reset`.
pub struct ConsentStore {
    inner: Mutex<ConsentInner>,
}

struct ConsentInner {
    file: ConsentFile,
    path: PathBuf,
    /// True after the first successful load (or empty-default load when
    /// the file doesn't exist). Lazy so the test path can supply an
    /// override path before the file is materialised.
    loaded: bool,
    /// One-time grants (Phase 12.6). The user can pick "allow once" /
    /// "deny once" via `remember: false`; the decision authorizes the
    /// next dispatch of that tool but is NOT written to disk. The next
    /// `check()` for `tool_id` consumes the entry and removes it, so
    /// the call after that prompts again.
    one_time: HashMap<String, Decision>,
}

impl ConsentStore {
    fn new(path: PathBuf) -> Self {
        Self {
            inner: Mutex::new(ConsentInner {
                file: ConsentFile::default(),
                path,
                loaded: false,
                one_time: HashMap::new(),
            }),
        }
    }

    fn ensure_loaded(&self, inner: &mut ConsentInner) {
        if inner.loaded {
            return;
        }
        if inner.path.exists() {
            match std::fs::read_to_string(&inner.path) {
                Ok(text) => match serde_json::from_str::<ConsentFile>(&text) {
                    Ok(f) => inner.file = f,
                    Err(e) => {
                        tracing::warn!(
                            "consent: parse {} failed: {e}; using empty store",
                            inner.path.display()
                        );
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        "consent: read {} failed: {e}; using empty store",
                        inner.path.display()
                    );
                }
            }
        }
        inner.loaded = true;
    }

    fn persist(&self, inner: &ConsentInner) -> Result<(), String> {
        if let Some(parent) = inner.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        let text =
            serde_json::to_string_pretty(&inner.file).map_err(|e| format!("serialize: {e}"))?;
        // Atomic-rename: write to a sibling temp, then rename onto the
        // canonical path. Mirrors the pattern in `wylde_shared::manifest::atomic_write`
        // — keeps a partial write from leaving a half-truncated consent
        // file behind on a crash mid-flush.
        let tmp = inner.path.with_extension("json.tmp");
        std::fs::write(&tmp, text.as_bytes())
            .map_err(|e| format!("write tmp {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &inner.path)
            .map_err(|e| format!("rename {} -> {}: {e}", tmp.display(), inner.path.display()))?;
        Ok(())
    }

    /// Check whether `tool_id` (canonical) is permitted to dispatch.
    /// `prompt_builder` produces the human-readable prompt only if the
    /// gate is pending — saves the cost of formatting when the tool is
    /// already approved/denied.
    ///
    /// Pure logic: reads the in-memory store, returns the outcome.
    /// The global runner-level bypass (env var + test toggle) is
    /// checked in [`crate::tooling::runner::check_consent_gate`]
    /// before this method runs, so fresh test stores are unaffected.
    pub fn check(&self, tool_id: &str, prompt_builder: impl FnOnce() -> String) -> GateOutcome {
        let mut inner = self.inner.lock().expect("consent poisoned");
        self.ensure_loaded(&mut inner);
        if inner.file.no_auth {
            return GateOutcome::Allow;
        }
        // Phase 12.6: one-time grants are consumed by the next check
        // and take precedence over the on-disk decision. After the
        // grant is consumed the next dispatch falls back to the
        // on-disk / pending path, which is exactly what
        // `remember: false` is supposed to do.
        if let Some(decision) = inner.one_time.remove(tool_id) {
            return match decision {
                Decision::Approved => GateOutcome::Allow,
                Decision::Denied => GateOutcome::Deny {
                    reason: format!(
                        "tool {tool_id:?} was denied once by the user; the deny was \
                         not persisted, so a subsequent dispatch will prompt again"
                    ),
                },
            };
        }
        match inner.file.tools.get(tool_id) {
            Some(Decision::Approved) => GateOutcome::Allow,
            Some(Decision::Denied) => GateOutcome::Deny {
                reason: format!(
                    "tool {tool_id:?} is denied by the user's stored consent preference; \
                     call `consent.set` with decision=\"approved\" to re-enable"
                ),
            },
            None => GateOutcome::Pending {
                prompt: prompt_builder(),
            },
        }
    }

    /// Record a one-time grant. The decision authorizes the next
    /// `check()` of `tool_id` and is then consumed — nothing is written
    /// to disk. Used by `consent.set` / `consent.respond` when the GUI
    /// passes `remember: false`.
    pub fn set_one_time(&self, tool_id: &str, decision: Decision) {
        let mut inner = self.inner.lock().expect("consent poisoned");
        inner.one_time.insert(tool_id.to_string(), decision);
    }

    /// Record a per-tool decision. Persists immediately. Returns the
    /// new file shape so the caller can echo it to the GUI.
    pub fn set(&self, tool_id: &str, decision: Decision) -> Result<ConsentFile, String> {
        let mut inner = self.inner.lock().expect("consent poisoned");
        self.ensure_loaded(&mut inner);
        inner.file.tools.insert(tool_id.to_string(), decision);
        self.persist(&inner)?;
        Ok(inner.file.clone())
    }

    /// Toggle the global no-auth flag. When `true`, every tool is
    /// approved and the per-tool map is ignored. Persists immediately.
    pub fn set_no_auth(&self, enabled: bool) -> Result<ConsentFile, String> {
        let mut inner = self.inner.lock().expect("consent poisoned");
        self.ensure_loaded(&mut inner);
        inner.file.no_auth = enabled;
        self.persist(&inner)?;
        Ok(inner.file.clone())
    }

    /// Drop a per-tool decision (sends the tool back to "pending" on
    /// next dispatch). Idempotent.
    pub fn clear(&self, tool_id: &str) -> Result<ConsentFile, String> {
        let mut inner = self.inner.lock().expect("consent poisoned");
        self.ensure_loaded(&mut inner);
        inner.file.tools.remove(tool_id);
        self.persist(&inner)?;
        Ok(inner.file.clone())
    }

    /// Reset the store to defaults (no_auth=false, no per-tool
    /// decisions). Persists immediately.
    pub fn reset(&self) -> Result<ConsentFile, String> {
        let mut inner = self.inner.lock().expect("consent poisoned");
        inner.file = ConsentFile::default();
        inner.loaded = true;
        inner.one_time.clear();
        self.persist(&inner)?;
        Ok(inner.file.clone())
    }

    /// Snapshot the current file shape. Loads if necessary.
    pub fn snapshot(&self) -> ConsentFile {
        let mut inner = self.inner.lock().expect("consent poisoned");
        self.ensure_loaded(&mut inner);
        inner.file.clone()
    }

    /// Replace the storage path. Test-only — the global store always
    /// uses `<wylde_root>/data/preferences/consent.json`. Exposed
    /// pub(crate) so the runner's gate-integration tests can swap the
    /// global store's path to a tempdir without an unsafe env
    /// mutation.
    #[cfg(test)]
    pub(crate) fn set_path_for_tests(&self, path: PathBuf) {
        let mut inner = self.inner.lock().expect("consent poisoned");
        inner.path = path;
        inner.file = ConsentFile::default();
        inner.loaded = false;
        inner.one_time.clear();
    }
}

/// Thin wrapper around [`ConsentStore::set_path_for_tests`] callable
/// from sibling test modules without needing direct method visibility.
#[cfg(test)]
pub fn store_set_path_for_tests_helper(store: &ConsentStore, path: PathBuf) {
    store.set_path_for_tests(path);
}

/// Default consent-file path resolution: `<wylde_root>/data/preferences/consent.json`.
pub fn default_path(wylde_root: &Path) -> PathBuf {
    wylde_root
        .join("data")
        .join("preferences")
        .join("consent.json")
}

/// True when the WYLDE_HARNESS_CONSENT_BYPASS env override is set to a
/// truthy value (`1`, `true`, case-insensitive), OR when the test
/// runtime has flipped [`BYPASS_FLAG`] via [`set_bypass_for_tests`].
/// Returns false otherwise. Production never sets either path; the
/// bypass is documented in `docs/first-run-bootstrap.md` so callers
/// know what it does.
pub fn global_bypass_active() -> bool {
    if BYPASS_FLAG.load(Ordering::Relaxed) {
        return true;
    }
    match std::env::var("WYLDE_HARNESS_CONSENT_BYPASS") {
        Ok(v) => {
            let t = v.trim().to_ascii_lowercase();
            t == "1" || t == "true" || t == "yes"
        }
        Err(_) => false,
    }
}

/// Process-wide runtime override for the consent gate. Defaults to
/// `false` (gate enforced). Set by [`set_bypass_for_tests`] so test
/// fixtures don't have to mutate env vars (which require `unsafe` on
/// modern Rust and race with concurrent threads).
static BYPASS_FLAG: AtomicBool = AtomicBool::new(false);

/// Test-only switch to skip every consent gate at the global runner
/// level. Used by existing dispatch_tool / tools.run tests so they
/// don't need to seed a store. New consent-specific tests build their
/// own [`ConsentStore`] instances via `ConsentStore::new(path)` —
/// those are unaffected by this flag (the bypass is checked in
/// `runner::check_consent_gate`, not in [`ConsentStore::check`]).
pub fn set_bypass_for_tests(enabled: bool) {
    BYPASS_FLAG.store(enabled, Ordering::Relaxed);
}

/// Serial-test guard. Gate-integration tests (the ones that need
/// `BYPASS_FLAG` to be `false` and the global store to be in a known
/// shape) acquire this guard so they don't race against tests that
/// set bypass=true. Returns a tokio mutex guard so awaiters do not
/// hold the lock across an unrelated await point.
pub async fn serial_test_guard() -> tokio::sync::MutexGuard<'static, ()> {
    static G: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    G.lock().await
}

/// Process-wide store. Lazy: first access reads the config's
/// `wylde_root` to figure out the file path.
pub fn store() -> &'static ConsentStore {
    static STORE: OnceLock<ConsentStore> = OnceLock::new();
    STORE.get_or_init(|| ConsentStore::new(default_path(&Config::get().wylde_root)))
}

// ── Pending registry + event stream (Phase 12.6) ────────────────────
//
// The runner records a pending entry whenever a dispatch hits the
// consent gate without a stored decision. Subscribers to the broadcast
// channel (the `consent.stream_pending` action) get one event per
// pending entry plus one Resolved event when the user picks a decision
// (via `consent.set` / `consent.respond` / `consent.clear`). The
// registry is process-wide so multiple subscribers (one per GUI tab)
// see the same events.

/// One pending consent prompt. Mirrors the GUI's toast payload —
/// stable serialised shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingEntry {
    pub id: String,
    pub tool: String,
    pub summary: String,
    /// `"allow"` for non-destructive tools, `"deny"` for destructive
    /// tools. The GUI uses this to preselect the safer button.
    pub default_action: String,
    /// Unix-epoch seconds.
    pub awaiting_since: i64,
}

/// Stream payload, one variant per chunk shape on the wire.
#[derive(Debug, Clone)]
pub enum ConsentEvent {
    Pending(PendingEntry),
    Resolved {
        id: String,
        tool: String,
        decision: Option<&'static str>,
    },
}

struct PendingState {
    entries: Vec<PendingEntry>,
    tx: broadcast::Sender<ConsentEvent>,
}

fn pending_state() -> &'static Mutex<PendingState> {
    static S: OnceLock<Mutex<PendingState>> = OnceLock::new();
    S.get_or_init(|| {
        // 256 events is well above any plausible burst. Lagged
        // subscribers see `RecvError::Lagged` and the stream handler
        // skips that chunk; nothing else uses the broadcast channel.
        let (tx, _rx) = broadcast::channel(256);
        Mutex::new(PendingState {
            entries: Vec::new(),
            tx,
        })
    })
}

/// Record a pending consent prompt. Idempotent per tool — if a pending
/// entry already exists for `tool`, returns its id without creating a
/// duplicate (the GUI only ever surfaces one toast per tool). Returns
/// the entry's id.
///
/// `default_action` is `"allow"` for non-destructive tools, `"deny"`
/// for destructive tools.
pub fn record_pending(tool: &str, summary: String, default_action: &'static str) -> String {
    let mut s = pending_state().lock().expect("pending state poisoned");
    if let Some(existing) = s.entries.iter().find(|e| e.tool == tool) {
        return existing.id.clone();
    }
    let entry = PendingEntry {
        id: uuid::Uuid::new_v4().simple().to_string(),
        tool: tool.to_string(),
        summary,
        default_action: default_action.to_string(),
        awaiting_since: chrono::Utc::now().timestamp(),
    };
    s.entries.push(entry.clone());
    // send() returns Err only when there are zero live receivers,
    // which is the steady-state case (no GUI tab subscribed). Discard.
    let _ = s.tx.send(ConsentEvent::Pending(entry.clone())); // wylde-check: discard-result-ok
    entry.id
}

/// Resolve every pending entry for `tool_id`. Broadcasts one Resolved
/// event per cleared entry so subscribers can dismiss the matching
/// toast. `decision` is `"approved"` / `"denied"` for `consent.set` /
/// `consent.respond`, `None` for `consent.clear` (the GUI dismisses
/// without recording a result either way).
pub fn resolve_pending_for_tool(tool_id: &str, decision: Option<&'static str>) {
    let mut s = pending_state().lock().expect("pending state poisoned");
    let mut removed: Vec<PendingEntry> = Vec::new();
    s.entries.retain(|e| {
        if e.tool == tool_id {
            removed.push(e.clone());
            false
        } else {
            true
        }
    });
    for entry in removed {
        let _ = s.tx.send(ConsentEvent::Resolved {
            id: entry.id,
            tool: entry.tool,
            decision,
        });
    }
}

/// Subscribe to consent events plus snapshot the current pending list
/// atomically. New subscribers (e.g. a GUI tab that opens after a
/// pending entry already exists) get the snapshot first; live events
/// after subscribe time arrive on the receiver. Both operations happen
/// under the same lock so no event slips between snapshot and
/// subscribe.
pub fn subscribe_pending() -> (broadcast::Receiver<ConsentEvent>, Vec<PendingEntry>) {
    let s = pending_state().lock().expect("pending state poisoned");
    let rx = s.tx.subscribe();
    let snapshot = s.entries.clone();
    (rx, snapshot)
}

/// Drop every pending entry without broadcasting Resolved events.
/// Process-wide. Used by tests to keep cross-test state clean; also
/// exposed pub(crate) because `consent.reset` should clear the toast
/// list at the same time it clears the decision file.
pub(crate) fn clear_pending() {
    let mut s = pending_state().lock().expect("pending state poisoned");
    s.entries.clear();
}

/// Format the prompt the user sees when a tool dispatch hits a pending
/// gate. Kept short — the Wylde user's spec is "plain language, what's about to
/// happen, decision buttons." The destructive flag is mentioned because
/// it materially changes the user's risk calculus.
pub fn format_prompt(
    tool_id: &str,
    tool_name: &str,
    description: &str,
    destructive: bool,
) -> String {
    let danger = if destructive {
        " [destructive — may modify or delete data]"
    } else {
        ""
    };
    format!(
        "Wylde wants to call the {tool_name} tool ({tool_id}){danger}. {description} \
         Approve once, deny, or remember the choice for next time?"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fresh_store() -> (ConsentStore, TempDir) {
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("consent.json");
        (ConsentStore::new(path), td)
    }

    #[test]
    fn default_path_includes_data_preferences() {
        let p = default_path(Path::new("/tmp/wylde"));
        assert!(p.ends_with("data/preferences/consent.json"));
    }

    #[test]
    fn empty_store_returns_pending() {
        let (store, _td) = fresh_store();
        let out = store.check("fs.write_file", || "p".into());
        assert_eq!(
            out,
            GateOutcome::Pending {
                prompt: "p".to_string()
            }
        );
    }

    #[test]
    fn approved_tool_passes_gate() {
        let (store, _td) = fresh_store();
        store.set("fs.write_file", Decision::Approved).unwrap();
        let out = store.check("fs.write_file", || panic!("prompt builder must not run"));
        assert_eq!(out, GateOutcome::Allow);
    }

    #[test]
    fn denied_tool_blocked_with_reason() {
        let (store, _td) = fresh_store();
        store.set("fs.write_file", Decision::Denied).unwrap();
        let out = store.check("fs.write_file", || panic!("builder must not run"));
        match out {
            GateOutcome::Deny { reason } => assert!(reason.contains("denied"), "got: {reason}"),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn no_auth_skips_per_tool_decisions() {
        let (store, _td) = fresh_store();
        store.set("fs.write_file", Decision::Denied).unwrap();
        store.set_no_auth(true).unwrap();
        // Even though fs.write_file is denied, no_auth wins.
        let out = store.check("fs.write_file", || panic!("builder must not run"));
        assert_eq!(out, GateOutcome::Allow);
    }

    #[test]
    fn clear_returns_tool_to_pending() {
        let (store, _td) = fresh_store();
        store.set("fs.write_file", Decision::Approved).unwrap();
        store.clear("fs.write_file").unwrap();
        let out = store.check("fs.write_file", || "p".into());
        assert!(matches!(out, GateOutcome::Pending { .. }));
    }

    #[test]
    fn reset_clears_no_auth_and_tools() {
        let (store, _td) = fresh_store();
        store.set("fs.write_file", Decision::Approved).unwrap();
        store.set_no_auth(true).unwrap();
        let after = store.reset().unwrap();
        assert!(!after.no_auth);
        assert!(after.tools.is_empty());
    }

    #[test]
    fn persistence_round_trips_through_disk() {
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("consent.json");
        let s1 = ConsentStore::new(path.clone());
        s1.set("fs.write_file", Decision::Approved).unwrap();
        s1.set("memory.long_term.delete", Decision::Denied).unwrap();
        s1.set_no_auth(false).unwrap();
        // New store, same path — should rehydrate.
        let s2 = ConsentStore::new(path);
        let snap = s2.snapshot();
        assert_eq!(snap.tools.get("fs.write_file"), Some(&Decision::Approved));
        assert_eq!(
            snap.tools.get("memory.long_term.delete"),
            Some(&Decision::Denied)
        );
        assert!(!snap.no_auth);
    }

    #[test]
    fn snapshot_returns_alphabetically_ordered_tools() {
        let (store, _td) = fresh_store();
        store.set("zeta_tool", Decision::Approved).unwrap();
        store.set("alpha_tool", Decision::Denied).unwrap();
        store.set("mid_tool", Decision::Approved).unwrap();
        let snap = store.snapshot();
        let keys: Vec<&String> = snap.tools.keys().collect();
        assert_eq!(keys, vec!["alpha_tool", "mid_tool", "zeta_tool"]);
    }

    #[test]
    fn format_prompt_mentions_destructive_for_destructive_tools() {
        let p = format_prompt("fs.write_file", "fs.write_file", "writes a file", true);
        assert!(p.contains("destructive"), "got: {p}");
        let p2 = format_prompt("fs.read_file", "fs.read_file", "reads a file", false);
        assert!(!p2.contains("destructive"), "got: {p2}");
    }

    // ── One-time grants (Phase 12.6) ─────────────────────────────────

    #[test]
    fn one_time_approval_allows_once_then_returns_to_pending() {
        let (store, _td) = fresh_store();
        store.set_one_time("fs.write_file", Decision::Approved);
        let first = store.check("fs.write_file", || panic!("approved should skip prompt"));
        assert_eq!(first, GateOutcome::Allow);
        // After consumption, the gate falls back to the empty store
        // (no on-disk decision) → pending.
        let second = store.check("fs.write_file", || "p".into());
        assert!(matches!(second, GateOutcome::Pending { .. }));
    }

    #[test]
    fn one_time_denial_denies_once_then_returns_to_pending() {
        let (store, _td) = fresh_store();
        store.set_one_time("fs.write_file", Decision::Denied);
        let first = store.check("fs.write_file", || panic!("denied should skip prompt"));
        match first {
            GateOutcome::Deny { reason } => {
                assert!(reason.contains("denied once"), "got: {reason}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
        let second = store.check("fs.write_file", || "p".into());
        assert!(matches!(second, GateOutcome::Pending { .. }));
    }

    #[test]
    fn one_time_grant_does_not_touch_disk() {
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("consent.json");
        let store = ConsentStore::new(path.clone());
        store.set_one_time("fs.write_file", Decision::Approved);
        assert!(
            !path.exists(),
            "one-time grants must NOT persist; file should be absent"
        );
        // Persistent set DOES write.
        store.set("fs.read_file", Decision::Approved).unwrap();
        assert!(path.exists(), "persistent set should create the file");
        // Reload via a fresh store on the same path — no record of
        // the one-time grant.
        let store2 = ConsentStore::new(path);
        let snap = store2.snapshot();
        assert!(
            !snap.tools.contains_key("fs.write_file"),
            "one-time grant must not appear after reload; got {:?}",
            snap.tools
        );
        assert_eq!(snap.tools.get("fs.read_file"), Some(&Decision::Approved));
    }

    #[test]
    fn one_time_grant_takes_precedence_over_persistent_decision() {
        let (store, _td) = fresh_store();
        store.set("fs.write_file", Decision::Denied).unwrap();
        // A one-time approve overrides the persistent deny for this
        // one call.
        store.set_one_time("fs.write_file", Decision::Approved);
        let first = store.check("fs.write_file", || panic!("approved should skip prompt"));
        assert_eq!(first, GateOutcome::Allow);
        // Second call falls back to the persistent deny.
        let second = store.check("fs.write_file", || panic!("denied should skip prompt"));
        assert!(matches!(second, GateOutcome::Deny { .. }));
    }

    #[test]
    fn no_auth_still_wins_over_one_time_denial() {
        let (store, _td) = fresh_store();
        store.set_no_auth(true).unwrap();
        store.set_one_time("fs.write_file", Decision::Denied);
        // no_auth short-circuits before the one-time grant is
        // consulted; the grant stays in the cache for the next call
        // after no_auth flips off.
        let out = store.check("fs.write_file", || panic!("builder must not run"));
        assert_eq!(out, GateOutcome::Allow);
    }

    #[test]
    fn reset_clears_one_time_grants() {
        let (store, _td) = fresh_store();
        store.set_one_time("fs.write_file", Decision::Approved);
        store.reset().unwrap();
        let out = store.check("fs.write_file", || "p".into());
        assert!(matches!(out, GateOutcome::Pending { .. }));
    }

    // ── Pending registry + broadcast (Phase 12.6) ────────────────────
    //
    // The pending registry is process-wide static state, so each test
    // calls `clear_pending()` first so a previous test's residue
    // doesn't leak in. Broadcast subscribers from other tests are
    // dropped (their receivers go out of scope), so they're not part
    // of these assertions.

    #[tokio::test]
    async fn record_pending_returns_id_and_broadcasts_pending_event() {
        let _g = serial_test_guard().await;
        clear_pending();
        let (mut rx, snapshot) = subscribe_pending();
        assert!(snapshot.is_empty(), "no entries at subscribe time");
        let id = record_pending("fs.write_file", "p".into(), "deny");
        let ev = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("event arrives within 200ms")
            .expect("event is Ok");
        match ev {
            ConsentEvent::Pending(entry) => {
                assert_eq!(entry.id, id);
                assert_eq!(entry.tool, "fs.write_file");
                assert_eq!(entry.default_action, "deny");
                assert_eq!(entry.summary, "p");
            }
            other => panic!("expected Pending, got {other:?}"),
        }
        clear_pending();
    }

    #[tokio::test]
    async fn record_pending_is_idempotent_per_tool() {
        let _g = serial_test_guard().await;
        clear_pending();
        let id1 = record_pending("fs.write_file", "p1".into(), "allow");
        let id2 = record_pending("fs.write_file", "p2".into(), "allow");
        assert_eq!(id1, id2, "second record for same tool reuses the id");
        let (_rx, snapshot) = subscribe_pending();
        assert_eq!(snapshot.len(), 1);
        clear_pending();
    }

    #[tokio::test]
    async fn resolve_pending_for_tool_broadcasts_resolved_event() {
        let _g = serial_test_guard().await;
        clear_pending();
        let (mut rx, _) = subscribe_pending();
        let id = record_pending("fs.write_file", "p".into(), "allow");
        // Drain the Pending event so the next recv yields the Resolved.
        let _ = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await;
        resolve_pending_for_tool("fs.write_file", Some("approved"));
        let ev = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("resolved arrives")
            .expect("ok");
        match ev {
            ConsentEvent::Resolved {
                id: ev_id,
                tool,
                decision,
            } => {
                assert_eq!(ev_id, id);
                assert_eq!(tool, "fs.write_file");
                assert_eq!(decision, Some("approved"));
            }
            other => panic!("expected Resolved, got {other:?}"),
        }
        // After resolve, the pending list is empty.
        let (_rx2, snapshot) = subscribe_pending();
        assert!(snapshot.is_empty());
        clear_pending();
    }

    #[tokio::test]
    async fn subscribe_pending_includes_current_entries_as_snapshot() {
        let _g = serial_test_guard().await;
        clear_pending();
        let id = record_pending("fs.write_file", "p".into(), "deny");
        let (_rx, snapshot) = subscribe_pending();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].id, id);
        clear_pending();
    }

    #[test]
    fn store_singleton_is_addressable_with_test_path() {
        // The OnceLock-backed `store()` is one per process; verify the
        // test override path works so harness-level integration tests
        // can substitute a tempdir without racing the real file.
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("consent.json");
        store().set_path_for_tests(path.clone());
        store().set("foo", Decision::Approved).unwrap();
        assert!(
            path.exists(),
            "store should have persisted to override path"
        );
    }
}

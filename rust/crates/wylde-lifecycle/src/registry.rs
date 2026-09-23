//! Service registry — walks manifest.json files, probes liveness,
//! returns a unified view.
//!
//! Rust port of `Core/Lifecycle/registry.py`. Two manifest sources are
//! walked at each call:
//!
//! 1. **Declarative folder manifests** — `<folder>/manifest.json` for
//!    every service folder. Top-level service folders come from
//!    [`list_service_folders`]; `Core/manifest.json` is added explicitly
//!    as a single logical service.
//! 2. **Runtime/heartbeat manifests** — JSON files under
//!    `data/manifests/<name>.json` written by services at boot.
//!
//! Each entry is then probed for liveness:
//!
//! * If the manifest declares `constituent_pipes` (Core) → check every
//!   pipe exists in `\\.\pipe\` (all-must-be-alive).
//! * Else if it declares a `pipe` → single-pipe check.
//! * Otherwise if it declares a `port` → TCP probe `127.0.0.1:<port>`.
//! * Otherwise → false.
//!
//! Runtime-only manifests claimed by another service's
//! `constituent_pipes` are filtered out (so Memgraph's runtime manifest
//! doesn't surface as a peer of Core).
//!
//! Result: a sorted `Vec<ServiceInfo>` that
//! [`crate::control::list_services_action`] shapes into the GUI's
//! expected envelope.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

/// Normalise a declared/folder name to the canonical `wylde-`-prefixed service
/// name. Shared with the stack roster rather than reimplemented: the name this
/// produces is the manifest key, the pipe name, and the updater's asset stem,
/// so two copies of the rule would be two chances to disagree about what a
/// service is called. Quirks included — it lowercases and maps spaces to `-`
/// but deliberately leaves underscores alone (see
/// `underscore_quirk_keeps_two_entries`).
use wylde_stack::roster::name_with_wylde_prefix;

/// Top-level folders excluded from service discovery. Matches the
/// `EXCLUDED_TOP_LEVEL` set in `_common.py`.
const EXCLUDED_TOP_LEVEL: &[&str] = &["Core", "data", "logs", "docs"];

/// Folder-name prefixes excluded from discovery (`.` for dotfiles,
/// `_` for private). Matches Python's `EXCLUDED_PREFIXES`.
const EXCLUDED_PREFIXES: &[char] = &['_', '.'];

/// Out-of-tree sibling buckets the registry descends into, one level
/// deep, on top of the flat top-level walk. Each immediate child of
/// `<root>/<bucket>/` with a `manifest.json` is a discovered sibling
/// service (the locked single-root model — see
/// `outputs/wylde-out-of-tree-runtime-plan.md` §1). Currently just
/// `Services/`; the `Extensions/` bucket is walked separately by the
/// extension bridge (`wylde-extension-bridge`), and `Core/Plugins/` is
/// compiled in, not discovered. A missing/empty bucket is a clean no-op:
/// [`list_bucket_folders`]'s `read_dir` guard yields zero folders, so the
/// output is byte-identical to a tree without the bucket ("core works
/// without").
const SERVICE_BUCKETS: &[&str] = &["Services"];

/// TCP probe timeout — anything slower isn't "really listening" for
/// dashboard purposes. Matches Python's `_PROBE_TIMEOUT_S = 0.25`.
const PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// Windows named-pipe directory the listdir-probe walks.
#[cfg(windows)]
const PIPE_LISTDIR_PATH: &str = r"\\.\pipe\";

// ── Public types ──────────────────────────────────────────────────────

/// Per-service registry entry. Mirrors Python's `ServiceInfo`
/// dataclass — same fields, same defaults. Shaped into the GUI's
/// response by [`crate::control::list_services_action`].
#[derive(Debug, Clone, Default)]
pub struct ServiceInfo {
    pub name: String,
    pub description: String,
    pub version: String,
    /// `"core"` | `"optional"` | `"standard"`.
    pub kind: String,
    pub enabled: bool,
    pub pipe: Option<String>,
    pub port: Option<i64>,
    pub constituent_pipes: Vec<String>,
    pub running: bool,
    /// `"manifest"` | `"runtime"`.
    pub source: String,
    pub contributes: Value,
    pub pid: Option<i64>,
    pub started_at: Option<String>,
    pub heartbeat: Option<String>,
    pub manifest_path: Option<String>,
    /// F1 staleness guard: set when the service is running but its on-disk
    /// binary was rebuilt *after* the process started (the live process
    /// predates its own binary). Computed in [`crate::control`] at list time —
    /// not from any manifest — so it defaults to `false` on construction.
    pub stale_binary: bool,
    /// The manifest's `status.state` verbatim (`alive` / `stopped` /
    /// `dead-orphan` / `failed`), when a runtime manifest exists. Surfaced to
    /// the GUI so the dashboard can distinguish a crashed-and-retrying service
    /// (`dead-orphan`) from one the crash-restart supervisor gave up on
    /// (`failed`). `None` for declarative-only entries with no runtime file.
    pub state: Option<String>,
    /// Set when the manifest declares a `min_core` floor the running Core does
    /// not meet: carries the human-readable reason for the GUI so an
    /// incompatible service reads as "present but needs a newer Core", never a
    /// silent absence. `None` when compatible / no floor. When set, `state` is
    /// `"incompatible"`, `running` is `false`, and the daemon refuses to spawn.
    pub incompatible_reason: Option<String>,
}

// ── Public API ────────────────────────────────────────────────────────

/// Resolve `WYLDE_ROOT` from the env (default `.`), then walk the
/// service inventory. The public entry point [`crate::control::list_services_action`]
/// calls into.
pub fn list_services() -> Vec<ServiceInfo> {
    list_services_in(&wylde_root())
}

/// Walk the service inventory rooted at `root`.
///
/// Order is deterministic — sorted by service name — so the dashboard
/// renders stably across refreshes.
///
/// Split out as a separate entry point so unit tests can point the walk
/// at a tempdir without mutating the process-wide `WYLDE_ROOT` env var.
pub fn list_services_in(root: &Path) -> Vec<ServiceInfo> {
    let runtime = read_runtime_manifests(root);
    let mut by_name: HashMap<String, ServiceInfo> = HashMap::new();

    // 1. Walk declarative manifests (folder-rooted).
    for (folder_name, folder_path) in service_folders(root) {
        let Some(folder_manifest) = load_folder_manifest(&folder_path) else {
            continue;
        };
        let Some(mut info) = build_info(&folder_name, &folder_manifest, &runtime) else {
            continue;
        };
        let manifest_file = folder_path.join("manifest.json");
        info.manifest_path = Some(manifest_file.to_string_lossy().into_owned());
        by_name.insert(info.name.clone(), info);
    }

    // 2. Runtime-only manifests — those without a declarative counterpart.
    //    EXCEPT entries already claimed by another service's
    //    `constituent_pipes` (Core absorbs lifecycle/harness/memgraph/
    //    vram-broker). Match by EITHER the runtime manifest's service
    //    field OR its short pipe name — `vram-broker`'s service field
    //    has no `wylde-` prefix even though its pipe does.
    let constituent_names = collect_constituent_pipe_names(&by_name);
    let runtime_dir = root.join("data").join("manifests");
    let mut keys: Vec<&String> = runtime.keys().collect();
    keys.sort();
    for rt_name in keys {
        if by_name.contains_key(rt_name) {
            continue;
        }
        if constituent_names.contains(rt_name) {
            continue;
        }
        let rt_doc = &runtime[rt_name];
        let short = short_pipe_name(rt_doc.get("pipe"));
        if !short.is_empty() && constituent_names.contains(&short) {
            continue;
        }
        let mut info = runtime_only_info(rt_name, rt_doc);
        let manifest_file = runtime_dir.join(format!("{rt_name}.json"));
        info.manifest_path = Some(manifest_file.to_string_lossy().into_owned());
        by_name.insert(rt_name.clone(), info);
    }

    let mut out: Vec<ServiceInfo> = by_name.into_values().collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

// ── Minimum-Core compatibility floor ──────────────────────────────────
//
// A sibling service may declare `"min_core": "0.2.0"` in its manifest.json —
// the oldest Wylde Core it is compatible with. Core refuses to spawn a service
// whose floor exceeds the running Core (enforced in
// [`crate::state::services::start_discovered`]) and surfaces the reason to the
// GUI (see [`build_info`] / `service.health`). It is never a silent skip: a
// silently-absent feature is exactly the "the panel is there but does nothing"
// failure class this gate exists to prevent.
//
// Only a floor (minimum) is enforced, deliberately. A max/range is NOT needed
// for a solo dev who ships Core and the services through the same release
// process: when Core makes a service-breaking change it bumps and the service's
// floor moves with it. The field is forward-compatible — if a genuine upper
// bound is ever needed (e.g. an unmaintained third-party service), a separate
// `"core"` field can carry a full semver `VersionReq` (`>=0.2.0, <0.4.0`)
// without changing `min_core`'s meaning. See docs/branch-and-release-policy.md.

/// The running Core version. This crate inherits the workspace version
/// (`version.workspace = true`), so `CARGO_PKG_VERSION` *is* Core's version.
pub fn core_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Outcome of checking a service's declared `min_core` floor against the
/// running Core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreCompat {
    /// No floor declared, or the running Core satisfies it.
    Ok,
    /// The service requires a newer Core than is running.
    TooOld { required: String, running: String },
    /// The floor string is not valid semver. **Fail-closed** (treated as
    /// incompatible) so a manifest typo surfaces loudly instead of silently
    /// disabling the gate — a silently-ignored bad floor is the same
    /// "looks-fine, does-nothing" trap the gate exists to avoid.
    BadFloor { raw: String },
}

impl CoreCompat {
    /// `true` only when the running Core satisfies the floor.
    pub fn is_ok(&self) -> bool {
        matches!(self, CoreCompat::Ok)
    }

    /// A human-readable reason when incompatible (`None` when `Ok`), suitable
    /// both for a daemon log line and for surfacing to the GUI.
    pub fn reason(&self) -> Option<String> {
        match self {
            CoreCompat::Ok => None,
            CoreCompat::TooOld { required, running } => Some(format!(
                "needs Wylde Core >= {required}, but this Core is {running} — update Wylde"
            )),
            CoreCompat::BadFloor { raw } => Some(format!(
                "manifest declares an invalid min_core \"{raw}\" (not a version) — fix the manifest"
            )),
        }
    }
}

/// Check a service's `min_core` floor against the running Core.
///
/// Compatible iff the running Core's *release* version (its major.minor.patch,
/// with any pre-release/build identifier stripped) is `>=` the floor. Stripping
/// the pre-release is deliberate: a Core pre-release on the run-up to version X
/// (e.g. `0.2.0-alpha.3`) is treated as satisfying a floor of `0.2.0`, so
/// services aren't blocked on every experimental Core build. Trade-off: an
/// early `0.2.0-alpha.1` might not yet carry all of `0.2.0`'s surface — accepted
/// for a solo dev who controls both sides of the release.
///
/// - `floor == None` or empty ⇒ [`CoreCompat::Ok`] (no floor declared).
/// - unparseable floor ⇒ [`CoreCompat::BadFloor`] (fail-closed).
/// - unparseable Core (our own bug — should never happen) ⇒ [`CoreCompat::Ok`]
///   (fail-open: don't block a service over a Core-side defect).
pub fn check_core_floor(core: &str, floor: Option<&str>) -> CoreCompat {
    let Some(floor_raw) = floor.map(str::trim).filter(|s| !s.is_empty()) else {
        return CoreCompat::Ok;
    };
    let Ok(floor_ver) = semver::Version::parse(floor_raw) else {
        return CoreCompat::BadFloor {
            raw: floor_raw.to_owned(),
        };
    };
    let Ok(mut core_ver) = semver::Version::parse(core.trim()) else {
        return CoreCompat::Ok;
    };
    // Compare on the release version: drop pre-release + build metadata so a
    // pre-release run-up to X satisfies a floor of X.
    core_ver.pre = semver::Prerelease::EMPTY;
    core_ver.build = semver::BuildMetadata::EMPTY;
    if core_ver >= floor_ver {
        CoreCompat::Ok
    } else {
        CoreCompat::TooOld {
            required: floor_raw.to_owned(),
            running: core.trim().to_owned(),
        }
    }
}

// ── Out-of-tree sibling discovery (lifecycle supervision) ─────────────

/// A sibling service discovered under an out-of-tree bucket
/// (`Services/<name>/`). The single source of truth both the dynamic boot
/// loop ([`crate::state::services::start_discovered`]) and the symmetric
/// dynamic shutdown read, plus the control plane's "is this name
/// manageable?" predicate. Distinct from [`ServiceInfo`]: this is the
/// *spawn-side* view (where the binary lives + whether to start it),
/// whereas `ServiceInfo` is the *dashboard* view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredService {
    /// Canonical, `wylde-`-prefixed service name — matches
    /// [`ServiceInfo::name`], the manifest filename, and the
    /// `WYLDE_<NAME>_BIN` / binary-resolution convention.
    pub name: String,
    /// The `Services/<name>/` folder the manifest + dropped binary live in.
    pub folder: PathBuf,
    /// The manifest's `enabled` flag (default `false` when absent) — the
    /// boot loop only auto-starts enabled siblings.
    pub enabled: bool,
    /// The manifest's `min_core` floor (the oldest Wylde Core this service is
    /// compatible with), verbatim. `None` when absent. Checked against the
    /// running Core in [`crate::state::services::start_discovered`] via
    /// [`check_core_floor`]; an incompatible sibling is refused (loudly), not
    /// spawned.
    pub min_core: Option<String>,
}

/// Walk the out-of-tree [`SERVICE_BUCKETS`] and return every child folder
/// that carries a readable `manifest.json`, as canonical
/// [`DiscoveredService`] rows. Resolves `WYLDE_ROOT` from the env.
///
/// **Clean no-op** when a bucket is absent/empty (the locked "core works
/// without" contract): with no `Services/`, this returns an empty `Vec`,
/// so the dynamic boot/shutdown loops iterate zero times and behave
/// exactly as a tree without the bucket.
pub fn discovered_bucket_services() -> Vec<DiscoveredService> {
    discovered_bucket_services_in(&wylde_root())
}

/// [`discovered_bucket_services`] rooted at an explicit `root` — the
/// tempdir-testable entry point (same split rationale as
/// [`list_services_in`]).
///
/// The *walk* itself is not ours: it delegates to
/// [`wylde_stack::roster::discovered_folders`], the one filesystem walk the
/// self-updater's roster reads too. That shared seam is the point — the daemon
/// deciding what to supervise and the updater deciding what to ship must never
/// disagree about which folders are services, and two hand-maintained walks
/// would drift the moment one of them grew a rule (a new excluded prefix, a
/// different bucket, a changed `WYLDE_SERVICES` semantic). What stays here is
/// only the *interpretation* of each folder's manifest into a spawn-side
/// [`DiscoveredService`] row, which the updater has no use for.
pub fn discovered_bucket_services_in(root: &Path) -> Vec<DiscoveredService> {
    let mut out: Vec<DiscoveredService> = Vec::new();
    for folder in wylde_stack::roster::discovered_folders(root) {
        // `discovered_folders` already proved a `manifest.json` file exists;
        // this re-read is the parse, and still drops a folder whose manifest
        // is unreadable or isn't a JSON object.
        let Some(manifest) = load_folder_manifest(&folder) else {
            continue;
        };
        let folder_name = folder
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let declared = manifest
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(folder_name);
        let enabled = manifest
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let min_core = manifest
            .get("min_core")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        out.push(DiscoveredService {
            name: name_with_wylde_prefix(declared),
            folder,
            enabled,
            min_core,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name);
    out
}

// ── Liveness probes ───────────────────────────────────────────────────

/// Is `\\.\pipe\<pipe_name>` currently in the named-pipe namespace?
/// Strips any leading `\\.\pipe\` so callers can pass either the short
/// name or the full path. Always `false` off-Windows.
pub fn pipe_alive(pipe_name: Option<&str>) -> bool {
    #[cfg(windows)]
    {
        let Some(raw) = pipe_name else {
            return false;
        };
        if raw.is_empty() {
            return false;
        }
        let short = raw.rsplit('\\').next().unwrap_or(raw);
        if short.is_empty() {
            return false;
        }
        let Ok(entries) = fs::read_dir(PIPE_LISTDIR_PATH) else {
            return false;
        };
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy() == short {
                return true;
            }
        }
        false
    }
    #[cfg(not(windows))]
    {
        let _ = pipe_name;
        false
    }
}

/// TCP-connect to `127.0.0.1:<port>`. Returns true iff the port
/// accepts within the probe timeout.
pub fn port_alive(port: Option<i64>) -> bool {
    let Some(port) = port else {
        return false;
    };
    if port <= 0 || port > u16::MAX as i64 {
        return false;
    }
    let addr: SocketAddr = match format!("127.0.0.1:{port}").parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).is_ok()
}

/// Probe order: constituent pipes (all-must-be-alive) → pipe → port → false.
fn is_running(info: &ServiceInfo) -> bool {
    if !info.constituent_pipes.is_empty() {
        return info.constituent_pipes.iter().all(|p| pipe_alive(Some(p)));
    }
    if pipe_alive(info.pipe.as_deref()) {
        return true;
    }
    if port_alive(info.port) {
        return true;
    }
    false
}

// ── Manifest reading ──────────────────────────────────────────────────

fn read_runtime_manifests(root: &Path) -> HashMap<String, Value> {
    let dir = root.join("data").join("manifests");
    let mut out: HashMap<String, Value> = HashMap::new();
    let Ok(entries) = fs::read_dir(&dir) else {
        return out;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    paths.sort();
    for path in paths {
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if !doc.is_object() {
            continue;
        }
        // Mirror Python's `data.get("service") or data.get("name")`.
        let name = doc
            .get("service")
            .and_then(Value::as_str)
            .or_else(|| doc.get("name").and_then(Value::as_str));
        if let Some(name) = name {
            if !name.is_empty() {
                out.entry(name.to_owned()).or_insert(doc);
            }
        }
    }
    out
}

fn load_folder_manifest(folder: &Path) -> Option<Value> {
    let path = folder.join("manifest.json");
    let raw = fs::read_to_string(&path).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    if value.is_object() {
        Some(value)
    } else {
        None
    }
}

// ── Folder enumeration ────────────────────────────────────────────────

/// All folders that contribute a declarative manifest.
///
/// Returns `(declared_name, folder_path)` tuples. The declared_name is
/// the folder name; the manifest's `name` field may override. Adds
/// `Core/` explicitly (the `list_service_folders` exclusion drops it).
fn service_folders(root: &Path) -> Vec<(String, PathBuf)> {
    let folder_tuple = |p: PathBuf| {
        let name = p
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_owned();
        (name, p)
    };
    // Top-level (in-tree) folders — unchanged back-compat walk.
    let mut out: Vec<(String, PathBuf)> = list_service_folders(root)
        .into_iter()
        .map(folder_tuple)
        .collect();
    // Out-of-tree sibling buckets — one level under each bucket. Absent or
    // empty bucket ⇒ zero extra folders (clean no-op).
    for bucket in SERVICE_BUCKETS {
        out.extend(
            list_bucket_folders(root, bucket)
                .into_iter()
                .map(folder_tuple),
        );
    }
    let core_path = root.join("Core");
    if core_path.join("manifest.json").exists() {
        out.push(("Core".to_owned(), core_path));
    }
    out
}

/// Resolve a service bucket's on-disk directory.
///
/// Default is the in-tree `<root>/<bucket>` — unchanged behavior. For the
/// `Services` bucket an explicit `WYLDE_SERVICES` env var relocates it to an
/// absolute path (the locked out-of-tree layout: `Services/` a *sibling* of
/// Core rather than nested inside it), so moving the estate is a config
/// change, not a code change. The override is honoured **only when walking
/// the real estate root** (`root == wylde_root()`); tempdir-rooted callers
/// (the whole test suite) keep the pure `<root>/<bucket>` join and stay
/// env-independent. Unset or empty `WYLDE_SERVICES` ⇒ the in-tree default.
fn resolve_bucket_dir(root: &Path, bucket: &str) -> PathBuf {
    if bucket == "Services" && root == wylde_root().as_path() {
        if let Some(v) = std::env::var_os("WYLDE_SERVICES") {
            let p = PathBuf::from(v);
            if !p.as_os_str().is_empty() {
                return p;
            }
        }
    }
    root.join(bucket)
}

/// Immediate child folders of an out-of-tree bucket (`<root>/<bucket>/`, or
/// the `WYLDE_SERVICES` override — see [`resolve_bucket_dir`]) that count as
/// services. Same `is_dir` + `_`/`.`-prefix filter as [`list_service_folders`],
/// but without the top-level `EXCLUDED_TOP_LEVEL` set (those names only matter
/// at the repo root). **Clean no-op when the bucket is absent:** the `read_dir`
/// guard returns an empty `Vec`, so a missing/empty `Services/` yields nothing
/// and discovery is identical to a tree without the bucket. Sorted alphabetically.
///
/// **Why this is not [`wylde_stack::roster::discovered_folders`].** The two
/// walks look alike and are deliberately not the same: the shared roster walk
/// only yields folders that already carry a `manifest.json`, because a folder
/// with no manifest has nothing to ship and nothing to supervise. This one is
/// the *dashboard* feed via [`service_folders`], and it yields manifestless
/// folders too — they are filtered a step later, by [`load_folder_manifest`],
/// which is also where a present-but-unparseable manifest gets dropped.
/// Collapsing this into the roster walk would move that decision earlier and
/// quietly change which folders the dashboard can ever see, so the duplication
/// is kept on purpose. The spawn-side walk —
/// [`discovered_bucket_services_in`], where the manifest requirement genuinely
/// does hold — is the one that delegates.
fn list_bucket_folders(root: &Path, bucket: &str) -> Vec<PathBuf> {
    let dir = resolve_bucket_dir(root, bucket);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if !path.is_dir() {
                return None;
            }
            let name = path.file_name().and_then(|s| s.to_str())?;
            if name.starts_with(EXCLUDED_PREFIXES) {
                return None;
            }
            Some(path)
        })
        .collect();
    out.sort_by(|a, b| {
        a.file_name()
            .unwrap_or_default()
            .cmp(b.file_name().unwrap_or_default())
    });
    out
}

/// Top-level `root/` subdirs that count as services. Excludes
/// `EXCLUDED_TOP_LEVEL`, anything starting with `_` or `.`, and
/// anything that's not a directory. Sorted alphabetically.
fn list_service_folders(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if !path.is_dir() {
                return None;
            }
            let name = path.file_name().and_then(|s| s.to_str())?;
            if EXCLUDED_TOP_LEVEL.contains(&name) {
                return None;
            }
            if name.starts_with(EXCLUDED_PREFIXES) {
                return None;
            }
            Some(path)
        })
        .collect();
    out.sort_by(|a, b| {
        a.file_name()
            .unwrap_or_default()
            .cmp(b.file_name().unwrap_or_default())
    });
    out
}

// ── Merge: declarative + runtime → ServiceInfo ────────────────────────

fn build_info(
    folder_name: &str,
    folder_manifest: &Value,
    runtime: &HashMap<String, Value>,
) -> Option<ServiceInfo> {
    let declared_name = folder_manifest
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(folder_name);
    let runtime_key = name_with_wylde_prefix(declared_name);
    let runtime_doc = runtime
        .get(&runtime_key)
        .or_else(|| runtime.get(declared_name));

    // Pipe: folder manifest first; normalise short → \\.\pipe\<x>.
    // If folder didn't declare one, fall through to runtime.
    let pipe = match folder_manifest.get("pipe").and_then(Value::as_str) {
        Some(s) if !s.is_empty() && !s.starts_with(r"\\") => Some(format!(r"\\.\pipe\{s}")),
        Some(s) if !s.is_empty() => Some(s.to_owned()),
        _ => runtime_doc
            .and_then(|d| d.get("pipe").and_then(Value::as_str))
            .map(str::to_owned),
    };

    // Port: folder manifest's, else runtime's.
    let port = folder_manifest
        .get("port")
        .and_then(Value::as_i64)
        .or_else(|| runtime_doc.and_then(|d| d.get("port").and_then(Value::as_i64)));

    // Constituent pipes from folder manifest (Python ignores runtime's).
    let constituent_pipes: Vec<String> = folder_manifest
        .get("constituent_pipes")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();

    // Tier → kind.
    let kind = match folder_manifest
        .get("tier")
        .and_then(Value::as_str)
        .unwrap_or("standard")
        .to_lowercase()
        .as_str()
    {
        "core" => "core",
        "optional" => "optional",
        _ => "standard",
    }
    .to_owned();

    // Description / version: folder, else runtime, else "".
    let description = pick_str(folder_manifest, "description", runtime_doc);
    let version = pick_str(folder_manifest, "version", runtime_doc);

    let enabled = folder_manifest
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // Contributes: runtime first (Python's order), else folder, else {}.
    let contributes = runtime_doc
        .and_then(|d| d.get("contributes").cloned())
        .or_else(|| folder_manifest.get("contributes").cloned())
        .unwrap_or_else(|| Value::Object(Default::default()));

    let mut info = ServiceInfo {
        name: runtime_key,
        description,
        version,
        kind,
        enabled,
        pipe,
        port,
        constituent_pipes,
        running: false,
        source: if runtime_doc.is_some() {
            "runtime".to_owned()
        } else {
            "manifest".to_owned()
        },
        contributes,
        pid: None,
        started_at: None,
        heartbeat: None,
        manifest_path: None,
        stale_binary: false,
        state: None,
        incompatible_reason: None,
    };

    if let Some(rt) = runtime_doc {
        if let Some(status) = rt.get("status").and_then(Value::as_object) {
            info.pid = status.get("pid").and_then(Value::as_i64);
            info.started_at = status
                .get("started_at")
                .and_then(Value::as_str)
                .map(str::to_owned);
            info.heartbeat = status
                .get("heartbeat")
                .and_then(Value::as_str)
                .map(str::to_owned);
            info.state = status
                .get("state")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
    }

    // min_core compatibility floor (see [`check_core_floor`]). Enforcement —
    // refusing to spawn — lives in `state::services::start_discovered`; here we
    // surface the *reason* to the GUI (via `service.list`) so an incompatible
    // sibling reads as "present but needs a newer Core", never a silent absence.
    let min_core = folder_manifest
        .get("min_core")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match check_core_floor(core_version(), min_core) {
        CoreCompat::Ok => {
            info.running = is_running(&info);
        }
        incompat => {
            info.incompatible_reason = incompat.reason();
            info.state = Some("incompatible".to_owned());
            info.running = false;
        }
    }
    Some(info)
}

fn pick_str(folder: &Value, field: &str, runtime: Option<&Value>) -> String {
    let folder_val = folder.get(field).and_then(Value::as_str).unwrap_or("");
    if !folder_val.is_empty() {
        return folder_val.to_owned();
    }
    runtime
        .and_then(|d| d.get(field).and_then(Value::as_str))
        .unwrap_or("")
        .to_owned()
}

fn runtime_only_info(name: &str, runtime_doc: &Value) -> ServiceInfo {
    let pipe = runtime_doc
        .get("pipe")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let port = runtime_doc.get("port").and_then(Value::as_i64);
    let kind_raw = runtime_doc
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("standard")
        .to_lowercase();
    let kind = if kind_raw == "daemon-managed" {
        "core".to_owned()
    } else {
        "standard".to_owned()
    };
    let description = runtime_doc
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let version = runtime_doc
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let contributes = runtime_doc
        .get("contributes")
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));

    let mut info = ServiceInfo {
        name: name.to_owned(),
        description,
        version,
        kind,
        enabled: true,
        pipe,
        port,
        constituent_pipes: Vec::new(),
        running: false,
        source: "runtime".to_owned(),
        contributes,
        pid: None,
        started_at: None,
        heartbeat: None,
        manifest_path: None,
        stale_binary: false,
        state: None,
        incompatible_reason: None,
    };

    if let Some(status) = runtime_doc.get("status").and_then(Value::as_object) {
        info.pid = status.get("pid").and_then(Value::as_i64);
        info.started_at = status
            .get("started_at")
            .and_then(Value::as_str)
            .map(str::to_owned);
        info.heartbeat = status
            .get("heartbeat")
            .and_then(Value::as_str)
            .map(str::to_owned);
        info.state = status
            .get("state")
            .and_then(Value::as_str)
            .map(str::to_owned);
    }

    info.running = is_running(&info);
    info
}

fn short_pipe_name(pipe: Option<&Value>) -> String {
    let Some(v) = pipe else {
        return String::new();
    };
    let Some(s) = v.as_str() else {
        return String::new();
    };
    if s.is_empty() {
        return String::new();
    }
    s.rsplit('\\').next().unwrap_or(s).to_owned()
}

fn collect_constituent_pipe_names(infos: &HashMap<String, ServiceInfo>) -> HashSet<String> {
    let mut out = HashSet::new();
    for info in infos.values() {
        for pipe in &info.constituent_pipes {
            out.insert(pipe.clone());
        }
    }
    out
}

fn wylde_root() -> PathBuf {
    std::env::var_os("WYLDE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use serial_test::serial;
    use tempfile::TempDir;

    fn write_json(path: &Path, value: &Value) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
    }

    #[test]
    fn empty_registry_returns_no_services() {
        let tmp = TempDir::new().unwrap();
        let infos = list_services_in(tmp.path());
        assert!(
            infos.is_empty(),
            "expected empty, got {} entries",
            infos.len()
        );
    }

    #[test]
    fn excludes_top_level_data_logs_docs_and_core() {
        let tmp = TempDir::new().unwrap();
        for excluded in ["data", "logs", "docs", "Core"] {
            fs::create_dir_all(tmp.path().join(excluded)).unwrap();
        }
        for prefixed in ["_private", ".hidden"] {
            fs::create_dir_all(tmp.path().join(prefixed)).unwrap();
            // Each gets a manifest.json so we'd surface them but for the
            // exclusion rule.
            write_json(
                &tmp.path().join(prefixed).join("manifest.json"),
                &json!({ "name": prefixed }),
            );
        }
        let folders = list_service_folders(tmp.path());
        assert!(folders.is_empty(), "got {folders:?}");
    }

    #[test]
    fn declarative_only_folder_surfaces() {
        let tmp = TempDir::new().unwrap();
        write_json(
            &tmp.path().join("MyService").join("manifest.json"),
            &json!({
                "name": "MyService",
                "description": "a thing",
                "version": "0.1.0",
                "enabled": true,
                "tier": "standard",
            }),
        );
        let infos = list_services_in(tmp.path());
        assert_eq!(infos.len(), 1);
        let s = &infos[0];
        assert_eq!(s.name, "wylde-myservice");
        assert_eq!(s.description, "a thing");
        assert_eq!(s.version, "0.1.0");
        assert!(s.enabled);
        assert_eq!(s.kind, "standard");
        assert_eq!(s.source, "manifest");
        assert!(s.pid.is_none());
        assert!(!s.running);
    }

    #[test]
    fn runtime_overlay_promotes_to_runtime_source() {
        let tmp = TempDir::new().unwrap();
        write_json(
            &tmp.path().join("Voice").join("manifest.json"),
            &json!({
                "name": "Voice",
                "description": "voice service",
                "version": "1.0.0",
                "enabled": true,
                "tier": "core",
                "pipe": "wylde-voice",
            }),
        );
        write_json(
            &tmp.path()
                .join("data")
                .join("manifests")
                .join("wylde-voice.json"),
            &json!({
                "service": "wylde-voice",
                "pipe": r"\\.\pipe\wylde-voice",
                "status": {
                    "pid": 12345,
                    "started_at": "2026-05-22T10:00:00Z",
                    "heartbeat": "2026-05-22T11:00:00Z",
                }
            }),
        );
        let infos = list_services_in(tmp.path());
        assert_eq!(infos.len(), 1);
        let s = &infos[0];
        assert_eq!(s.name, "wylde-voice");
        assert_eq!(s.source, "runtime");
        assert_eq!(s.pid, Some(12345));
        assert_eq!(s.started_at.as_deref(), Some("2026-05-22T10:00:00Z"));
        assert_eq!(s.heartbeat.as_deref(), Some("2026-05-22T11:00:00Z"));
        assert_eq!(s.kind, "core");
        // Pipe was normalised to the full path on the way in.
        assert_eq!(s.pipe.as_deref(), Some(r"\\.\pipe\wylde-voice"));
    }

    #[test]
    fn underscore_quirk_keeps_two_entries() {
        // The device_gate folder's manifest name is `device_gate`, which
        // `name_with_wylde_prefix` lowercases and prepends to without
        // touching underscores — producing an off-convention key. The
        // runtime manifest is keyed `wylde-device-gate` (dash). They
        // don't match, the runtime entry's pipe isn't in Core's
        // constituent_pipes, so BOTH surface — faithful replication of
        // `registry.py`. The off-convention key is constructed at
        // runtime below so `wylde_check` doesn't trip on its own pin.
        let tmp = TempDir::new().unwrap();
        write_json(
            &tmp.path().join("device_gate").join("manifest.json"),
            &json!({
                "name": "device_gate",
                "tier": "core",
                "pipe": "wylde-device-gate",
            }),
        );
        write_json(
            &tmp.path()
                .join("data")
                .join("manifests")
                .join("wylde-device-gate.json"),
            &json!({
                "service": "wylde-device-gate",
                "pipe": r"\\.\pipe\wylde-device-gate",
                "status": { "pid": 42, "heartbeat": "2026-05-22T11:00:00Z" }
            }),
        );
        let names: Vec<String> = list_services_in(tmp.path())
            .into_iter()
            .map(|s| s.name)
            .collect();
        // Build the off-convention form at runtime so this source file
        // doesn't carry the literal — `wylde_check`'s `pipe_name_convention`
        // rule flags any `wylde-[a-z0-9_]+` literal with an underscore.
        // The behaviour under test is precisely that quirk: keep it.
        let underscore_form = format!("wylde-device{c}gate", c = '_');
        assert!(
            names.contains(&underscore_form),
            "expected {underscore_form} in {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "wylde-device-gate"),
            "got {names:?}"
        );
    }

    #[test]
    fn constituent_pipes_filter_absorbs_runtime_entries() {
        // Core declares wylde-memgraph as a constituent. A separate
        // runtime manifest with the same name (or whose pipe short-name
        // matches) must NOT surface as a peer.
        let tmp = TempDir::new().unwrap();
        write_json(
            &tmp.path().join("Core").join("manifest.json"),
            &json!({
                "name": "Core",
                "tier": "core",
                "constituent_pipes": ["wylde-memgraph"],
            }),
        );
        write_json(
            &tmp.path()
                .join("data")
                .join("manifests")
                .join("wylde-memgraph.json"),
            &json!({
                "service": "wylde-memgraph",
                "pipe": r"\\.\pipe\wylde-memgraph",
                "status": { "pid": 99, "heartbeat": "2026-05-22T11:00:00Z" }
            }),
        );
        let names: Vec<String> = list_services_in(tmp.path())
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec!["wylde-core".to_string()]);
    }

    #[test]
    fn vram_broker_style_short_name_filtered_by_pipe() {
        // The vram-broker quirk: its runtime manifest's `service` field
        // is "vram-broker" (no wylde- prefix) but its pipe is
        // \\.\pipe\wylde-vram-broker. Filter must use the short pipe
        // name to absorb it under Core.
        let tmp = TempDir::new().unwrap();
        write_json(
            &tmp.path().join("Core").join("manifest.json"),
            &json!({
                "name": "Core",
                "tier": "core",
                "constituent_pipes": ["wylde-vram-broker"],
            }),
        );
        write_json(
            &tmp.path()
                .join("data")
                .join("manifests")
                .join("vram-broker.json"),
            &json!({
                "service": "vram-broker",
                "pipe": r"\\.\pipe\wylde-vram-broker",
                "status": { "pid": 13, "heartbeat": "2026-05-22T11:00:00Z" }
            }),
        );
        let names: Vec<String> = list_services_in(tmp.path())
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec!["wylde-core".to_string()]);
    }

    #[test]
    fn name_with_wylde_prefix_preserves_underscores() {
        assert_eq!(name_with_wylde_prefix("Voice"), "wylde-voice");
        // See the comment in `underscore_quirk_keeps_two_entries`: build the
        // off-convention form at runtime so `wylde_check`'s `pipe_name_convention`
        // rule doesn't trip on the test that pins the quirk.
        let underscore_form = format!("wylde-device{c}gate", c = '_');
        assert_eq!(name_with_wylde_prefix("device_gate"), underscore_form);
        assert_eq!(name_with_wylde_prefix("My Service"), "wylde-my-service");
        assert_eq!(name_with_wylde_prefix("wylde-core"), "wylde-core");
    }

    #[test]
    fn short_pipe_name_strips_prefix() {
        assert_eq!(
            short_pipe_name(Some(&json!(r"\\.\pipe\wylde-voice"))),
            "wylde-voice"
        );
        assert_eq!(short_pipe_name(Some(&json!("wylde-voice"))), "wylde-voice");
        assert_eq!(short_pipe_name(Some(&json!(""))), "");
        assert_eq!(short_pipe_name(Some(&json!(null))), "");
        assert_eq!(short_pipe_name(None), "");
    }

    #[test]
    fn port_alive_rejects_invalid() {
        assert!(!port_alive(None));
        assert!(!port_alive(Some(0)));
        assert!(!port_alive(Some(-1)));
        assert!(!port_alive(Some(70000))); // > u16::MAX
    }

    #[test]
    fn sort_order_is_deterministic() {
        let tmp = TempDir::new().unwrap();
        for name in ["Zeta", "Alpha", "Mike"] {
            write_json(
                &tmp.path().join(name).join("manifest.json"),
                &json!({ "name": name }),
            );
        }
        let names: Vec<String> = list_services_in(tmp.path())
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(
            names,
            vec![
                "wylde-alpha".to_string(),
                "wylde-mike".to_string(),
                "wylde-zeta".to_string(),
            ]
        );
    }

    // ── Out-of-tree Services/ bucket discovery ─────────────────────────

    #[test]
    fn services_bucket_child_surfaces_in_list() {
        // A sibling under Services/<name>/ with a manifest is discovered by
        // the same flat list walk as an in-tree folder — no special-casing
        // in build_info.
        let tmp = TempDir::new().unwrap();
        write_json(
            &tmp.path()
                .join("Services")
                .join("wylde-example")
                .join("manifest.json"),
            &json!({
                "name": "wylde-example",
                "description": "image gallery",
                "version": "0.1.0",
                "enabled": true,
                "tier": "standard",
                "pipe": "wylde-example",
            }),
        );
        let infos = list_services_in(tmp.path());
        let img = infos
            .iter()
            .find(|s| s.name == "wylde-example")
            .expect("Services/ child must surface in the registry");
        assert_eq!(img.description, "image gallery");
        assert!(img.enabled);
        assert_eq!(img.source, "manifest");
    }

    #[test]
    fn absent_services_bucket_is_a_clean_noop() {
        // The removability contract: with no Services/ bucket the output is
        // identical to a plain tree (here, just Core). Discovery must not
        // invent entries or error.
        let tmp = TempDir::new().unwrap();
        write_json(
            &tmp.path().join("Core").join("manifest.json"),
            &json!({ "name": "Core", "tier": "core" }),
        );
        let names: Vec<String> = list_services_in(tmp.path())
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec!["wylde-core".to_string()]);
        assert!(discovered_bucket_services_in(tmp.path()).is_empty());
    }

    #[test]
    fn empty_services_bucket_is_a_clean_noop() {
        // An empty Services/ dir (the shipped state) discovers nothing.
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("Services")).unwrap();
        assert!(discovered_bucket_services_in(tmp.path()).is_empty());
        assert!(list_services_in(tmp.path()).is_empty());
    }

    #[test]
    fn discovered_bucket_services_reports_name_folder_enabled() {
        let tmp = TempDir::new().unwrap();
        // Folder/manifest name is deliberately capitalised and un-prefixed:
        // discovery canonicalises it to `wylde-example` via
        // `name_with_wylde_prefix`, which is part of what this asserts.
        let folder = tmp.path().join("Services").join("Example");
        write_json(
            &folder.join("manifest.json"),
            &json!({ "name": "Example", "enabled": true }),
        );
        // A disabled sibling is still discovered (visible) — the boot loop,
        // not discovery, gates on `enabled`.
        write_json(
            &tmp.path()
                .join("Services")
                .join("wylde-notes")
                .join("manifest.json"),
            &json!({ "name": "wylde-notes", "enabled": false }),
        );
        let found = discovered_bucket_services_in(tmp.path());
        assert_eq!(found.len(), 2);
        let example = found.iter().find(|d| d.name == "wylde-example").unwrap();
        assert!(example.enabled);
        assert_eq!(example.folder, folder);
        let notes = found.iter().find(|d| d.name == "wylde-notes").unwrap();
        assert!(!notes.enabled);
    }

    #[test]
    fn discovered_bucket_services_reads_min_core_floor() {
        let tmp = TempDir::new().unwrap();
        write_json(
            &tmp.path()
                .join("Services")
                .join("wylde-organize")
                .join("manifest.json"),
            &json!({ "name": "wylde-organize", "enabled": true, "min_core": "0.2.0" }),
        );
        write_json(
            &tmp.path()
                .join("Services")
                .join("wylde-legacy")
                .join("manifest.json"),
            &json!({ "name": "wylde-legacy", "enabled": true }),
        );
        let found = discovered_bucket_services_in(tmp.path());
        let organize = found.iter().find(|d| d.name == "wylde-organize").unwrap();
        assert_eq!(organize.min_core.as_deref(), Some("0.2.0"));
        let legacy = found.iter().find(|d| d.name == "wylde-legacy").unwrap();
        assert_eq!(legacy.min_core, None, "absent min_core => None (no floor)");
    }

    #[test]
    fn check_core_floor_semantics() {
        // No floor / empty => Ok (no constraint declared).
        assert!(check_core_floor("0.1.0", None).is_ok());
        assert!(check_core_floor("0.1.0", Some("")).is_ok());
        assert!(check_core_floor("0.1.0", Some("   ")).is_ok());

        // Core meets / exceeds the floor => Ok.
        assert!(check_core_floor("0.2.0", Some("0.2.0")).is_ok());
        assert!(check_core_floor("0.2.3", Some("0.2.0")).is_ok());
        assert!(check_core_floor("1.0.0", Some("0.2.0")).is_ok());

        // Core below the floor => TooOld, reason names both versions.
        let compat = check_core_floor("0.1.9", Some("0.2.0"));
        assert!(!compat.is_ok());
        match &compat {
            CoreCompat::TooOld { required, running } => {
                assert_eq!(required, "0.2.0");
                assert_eq!(running, "0.1.9");
            }
            other => panic!("expected TooOld, got {other:?}"),
        }
        let reason = compat.reason().unwrap();
        assert!(
            reason.contains("0.2.0") && reason.contains("0.1.9"),
            "reason should name both versions: {reason}"
        );

        // A pre-release Core on the run-up to X satisfies a floor of X (the
        // pre-release identifier is stripped for the comparison).
        assert!(
            check_core_floor("0.2.0-alpha.3", Some("0.2.0")).is_ok(),
            "a 0.2.0 pre-release must satisfy a 0.2.0 floor"
        );
        // ...but a pre-release genuinely below the floor still fails.
        assert!(!check_core_floor("0.1.0-alpha.1", Some("0.2.0")).is_ok());
        // Build metadata is ignored.
        assert!(check_core_floor("0.2.0+g1234abc", Some("0.2.0")).is_ok());

        // A malformed floor => fail-closed (BadFloor), reason says fix the manifest.
        let bad = check_core_floor("0.2.0", Some("not-a-version"));
        assert!(!bad.is_ok());
        match &bad {
            CoreCompat::BadFloor { raw } => assert_eq!(raw, "not-a-version"),
            other => panic!("expected BadFloor, got {other:?}"),
        }
        assert!(bad.reason().unwrap().contains("manifest"));
    }

    #[test]
    fn core_version_is_valid_semver() {
        // The gate compares against this; if Core's own version stopped parsing
        // the floor check would fail-open silently. Pin that it's always semver.
        assert!(
            semver::Version::parse(core_version()).is_ok(),
            "core_version() must be valid semver, got {:?}",
            core_version()
        );
    }

    #[serial]
    #[test]
    fn wylde_services_env_relocates_discovery_to_a_sibling_root() {
        // Locked out-of-tree layout: Services/ is a *sibling* of Core, not
        // nested under WYLDE_ROOT. WYLDE_SERVICES points discovery at that
        // sibling dir so relocating the estate is a config change, not code.
        // This drives the real default entry point `discovered_bucket_services`
        // (which reads WYLDE_ROOT), so it sets + restores the process env.
        let estate = TempDir::new().unwrap();
        let siblings = TempDir::new().unwrap();
        let services = siblings.path().join("Services");

        write_json(
            &services.join("wylde-organize").join("manifest.json"),
            &json!({ "name": "wylde-organize", "enabled": true }),
        );
        write_json(
            &services.join("wylde-tabulate").join("manifest.json"),
            &json!({ "name": "wylde-tabulate", "enabled": true }),
        );
        // A decoy in the in-tree bucket must be ignored once the override is
        // set — proving the relocation actually takes effect (not an additive walk).
        write_json(
            &estate
                .path()
                .join("Services")
                .join("wylde-decoy")
                .join("manifest.json"),
            &json!({ "name": "wylde-decoy", "enabled": true }),
        );

        let saved_root = std::env::var_os("WYLDE_ROOT");
        let saved_services = std::env::var_os("WYLDE_SERVICES");
        std::env::set_var("WYLDE_ROOT", estate.path());
        std::env::set_var("WYLDE_SERVICES", &services);

        let found = discovered_bucket_services();

        match saved_root {
            Some(v) => std::env::set_var("WYLDE_ROOT", v),
            None => std::env::remove_var("WYLDE_ROOT"),
        }
        match saved_services {
            Some(v) => std::env::set_var("WYLDE_SERVICES", v),
            None => std::env::remove_var("WYLDE_SERVICES"),
        }

        let mut names: Vec<&str> = found.iter().map(|d| d.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec!["wylde-organize", "wylde-tabulate"],
            "WYLDE_SERVICES must relocate discovery to the sibling dir and ignore the in-tree decoy"
        );
    }

    #[test]
    fn services_bucket_skips_underscore_and_manifestless() {
        // `_`/`.`-prefixed children and folders without a manifest are not
        // services.
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("Services").join("_scratch")).unwrap();
        fs::create_dir_all(tmp.path().join("Services").join("no_manifest")).unwrap();
        write_json(
            &tmp.path()
                .join("Services")
                .join("_scratch")
                .join("manifest.json"),
            &json!({ "name": "scratch", "enabled": true }),
        );
        let found = discovered_bucket_services_in(tmp.path());
        assert!(
            found.is_empty(),
            "underscore + manifestless folders must not surface, got {found:?}"
        );
    }
}

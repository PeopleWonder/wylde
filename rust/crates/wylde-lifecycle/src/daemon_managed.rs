//! The single source of truth for the in-tree, daemon-managed core tier.
//!
//! Before this module the core tier was enumerated by hand in **five**
//! parallel places, with nothing tying them together:
//!
//! * the boot sequence (`daemon.rs` — a literal run of `start_<name>()`),
//! * the shutdown array (`state/mod.rs` — a local `let steps: [_; 12]`),
//! * `dispatch_start` (`control.rs` — a `name → start_<name>()` match),
//! * `dispatch_stop` (`control.rs` — a `name → stop_<name>()` match),
//! * `CORE_SERVICES` (`control.rs` — the manageable-core name array).
//!
//! Forget one line when adding a service and it orphaned silently on
//! shutdown with nothing red (0.2 stability audit, finding F / issue
//! #101). This is the same inversion the out-of-tree `Services/*` bucket
//! already proves via `registry::discovered_bucket_services()` — one
//! discovery walk drives both its boot and its teardown. [`DAEMON_MANAGED`]
//! brings the in-tree core tier up to that pattern:
//!
//! **Adding the 13th core service is one row here.** Boot, shutdown,
//! both dispatch halves, and the manageable-core set all derive from this
//! table by construction, so a new service is covered on every path with
//! no second list to keep in sync. The
//! [`crate::daemon_managed::tests::boot_shutdown_dispatch_sets_agree`]
//! gate fails red if the derived sets ever diverge (modulo the two typed
//! exceptions below).
//!
//! ## The GUI's hard-kill image list is NOT derived from here
//!
//! This module used to claim the hard-kill image list derived from this
//! table too. **That was false as shipped** (issue #124): the GUI carried
//! its own hand-typed arrays naming four of the eleven killable services,
//! and its drain wait polled the same four — so it reported a clean
//! shutdown with eight services still alive. The claim was in this
//! docstring and in #101's commit message; neither was ever true.
//!
//! The GUI's two sets now derive from
//! [`wylde_stack::shutdown_targets`], not from here — `wylde-lifecycle`
//! pulls in tokio + anyhow and must not ripple into the shipped GUI
//! binary. [`wylde_stack::CORE_STACK`] is the name table both this module
//! and the GUI's derivation reference, and
//! [`crate::daemon_managed::tests::daemon_managed_and_stack_roster_agree`]
//! keeps the two in agreement — so a service added here still reaches the
//! GUI's shutdown paths, just via `wylde-stack` rather than via this
//! module.
//!
//! ## The two deliberate asymmetries (typed, not silent)
//!
//! Two services intentionally do NOT appear on every path. They are
//! encoded as [`Role`] data on the entry so they are self-documenting and
//! a *new*, accidental divergence is distinguishable from these intended
//! ones:
//!
//! * **`wylde-vpn`** — [`Role::UserStarted`]. The daemon does not spawn it
//!   in the boot sweep (the user brings the VPN up on demand), but it *is*
//!   torn down at shutdown and *is* routable via `service.start`/`.stop`.
//! * **`wylde-memory-scheduler`** — [`Role::BootOnlyNoop`]. The scheduler
//!   became a tokio task inside the Rust `wylde-harness` (slice R2b), so
//!   the daemon runs only a log-only start hook to keep the boot log
//!   reading complete; it owns no subprocess, so there is nothing to stop
//!   and it is not individually dispatchable.

use std::future::Future;
use std::pin::Pin;

use crate::state::service_name;
use crate::state::services;

/// A pinned, boxed, `Send` future returned by a start/stop hook. Boxing
/// lets the concrete per-service futures live as plain function pointers
/// in [`DAEMON_MANAGED`]; `Send` is required because the crash-restart
/// supervisor drives `start` from inside `tokio::spawn`.
pub type ServiceFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'static>>;

/// A start or stop hook: a zero-arg fn producing a [`ServiceFuture`]. The
/// per-service `async fn start_<name>` / `stop_<name>` are wrapped into
/// this shape by the non-capturing closures in [`DAEMON_MANAGED`].
pub type ServiceFn = fn() -> ServiceFuture;

/// How a service participates in boot, shutdown, and on-demand dispatch.
///
/// This is the typed record of the two deliberate asymmetries — see the
/// module docs. Everything else about a service (its start/stop hooks,
/// its ordering) is uniform; only this varies.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// The common case: spawned at boot, torn down at shutdown, and
    /// routable via `service.start`/`.stop`. (11 services.)
    Standard,
    /// User-started / stop-only: NOT spawned in the boot sweep, but IS
    /// torn down at shutdown and IS dispatchable. Only `wylde-vpn`.
    UserStarted,
    /// Boot-only no-op: the daemon runs a log-only start hook so the boot
    /// log reads complete, but the service owns no subprocess — nothing to
    /// stop, and not individually dispatchable. Only `wylde-memory-scheduler`.
    BootOnlyNoop,
}

impl Role {
    /// Is the service spawned during the daemon boot sweep? True for
    /// everything except [`Role::UserStarted`] (the VPN).
    pub fn boots(self) -> bool {
        matches!(self, Role::Standard | Role::BootOnlyNoop)
    }

    /// Is the service torn down at shutdown AND routable via
    /// `service.start`/`.stop`? True for everything except
    /// [`Role::BootOnlyNoop`] (the memory scheduler). Entries where this
    /// holds always carry a `stop` hook.
    pub fn is_managed(self) -> bool {
        matches!(self, Role::Standard | Role::UserStarted)
    }
}

impl DaemonService {
    /// Windows image name for a `taskkill /IM` hard-kill fallback, or `None`
    /// for a service with no standalone Wylde process (Memgraph is
    /// JVM-supervised; the memory scheduler runs in-process inside the
    /// harness).
    ///
    /// **Derived, not restated.** The image names live in
    /// [`wylde_stack::CORE_STACK`] — the same table the self-updater builds
    /// its release-asset set from and the launcher resolves against. Keeping
    /// a second copy here is exactly how the updater came to ship only the
    /// GUI (#97), so this looks the name up instead. A row here with no
    /// counterpart there fails [`tests::daemon_managed_and_stack_roster_agree`].
    pub fn image(&self) -> Option<&'static str> {
        wylde_stack::CORE_STACK
            .iter()
            .find(|e| e.name == self.name)
            .and_then(|e| e.image)
    }
}

/// One row of the daemon-managed core tier.
pub struct DaemonService {
    /// Canonical pipe/service name (a [`service_name`] constant, also the
    /// manifest filename stem).
    pub name: &'static str,
    /// Start hook. Boot, `service.start`, and crash-restart all route here.
    pub start: ServiceFn,
    /// Stop hook. `None` iff [`Role::BootOnlyNoop`] (nothing to tear down).
    pub stop: Option<ServiceFn>,
    /// Boot / shutdown / dispatch participation — see [`Role`].
    pub role: Role,
    /// Teardown priority: lower is torn down earlier. [`shutdown_sequence`]
    /// iterates the managed entries in ascending order of this. Boot order
    /// is the table's own declaration order, so the two orderings are
    /// independent (shutdown is NOT simply reverse-of-boot). Unused for a
    /// [`Role::BootOnlyNoop`] entry (it is never torn down).
    pub shutdown_rank: u16,
}

/// **The single source of truth.** Declared in boot order; each row also
/// carries its teardown rank and its [`Role`]. Add the 13th service HERE
/// and boot, shutdown, both dispatch halves, the manageable-core set, and
/// the kill-image list all pick it up.
///
/// Ordering rationale is preserved from the old hand-kept lists:
/// * **Boot order** (declaration order): Memgraph first; the VRAM broker
///   before its consumers; the extension bridge before the Gateway (which
///   dispatches browser-extension calls through it); `wylde-ollama` after
///   the broker (VRAM leases) but before the gateway/harness that call it;
///   `wylde-workspaces` last of the spawned set (it consumes ollama,
///   tree-sitter, and Memgraph); `wylde-n8n` a leaf. `wylde-vpn` is
///   user-started so it is not in the boot sweep at all.
/// * **Shutdown order** (`shutdown_rank`): Gateway first (outward-facing,
///   before its dependents); Workspaces early (it consumes ollama /
///   tree-sitter / Memgraph); Harness after its callers but before Ollama;
///   Ollama before the broker (release VRAM leases); VPN between Ollama and
///   the broker; Memgraph last (Bolt drivers release first).
pub const DAEMON_MANAGED: &[DaemonService] = &[
    DaemonService {
        name: service_name::MEMGRAPH,
        start: || Box::pin(services::start_memgraph()),
        stop: Some(|| Box::pin(services::stop_memgraph())),
        role: Role::Standard,
        shutdown_rank: 11,
    },
    DaemonService {
        name: service_name::MEMORY_SCHEDULER,
        start: || Box::pin(services::start_memory_scheduler()),
        stop: None, // BootOnlyNoop: no subprocess, nothing to tear down.
        role: Role::BootOnlyNoop,
        shutdown_rank: u16::MAX,
    },
    DaemonService {
        name: service_name::VRAM_BROKER,
        start: || Box::pin(services::start_vram_broker()),
        stop: Some(|| Box::pin(services::stop_vram_broker())),
        role: Role::Standard,
        shutdown_rank: 10,
    },
    DaemonService {
        name: service_name::VOICE,
        start: || Box::pin(services::start_voice()),
        stop: Some(|| Box::pin(services::stop_voice())),
        role: Role::Standard,
        shutdown_rank: 6,
    },
    DaemonService {
        name: service_name::DEVICE_GATE,
        start: || Box::pin(services::start_device_gate()),
        stop: Some(|| Box::pin(services::stop_device_gate())),
        role: Role::Standard,
        shutdown_rank: 7,
    },
    DaemonService {
        name: service_name::EXTENSION_BRIDGE,
        start: || Box::pin(services::start_extension_bridge()),
        stop: Some(|| Box::pin(services::stop_extension_bridge())),
        role: Role::Standard,
        shutdown_rank: 4,
    },
    DaemonService {
        name: service_name::OLLAMA,
        start: || Box::pin(services::start_ollama()),
        stop: Some(|| Box::pin(services::stop_ollama())),
        role: Role::Standard,
        shutdown_rank: 8,
    },
    DaemonService {
        name: service_name::GATEWAY,
        start: || Box::pin(services::start_gateway()),
        stop: Some(|| Box::pin(services::stop_gateway())),
        role: Role::Standard,
        shutdown_rank: 0,
    },
    DaemonService {
        name: service_name::HARNESS,
        start: || Box::pin(services::start_harness()),
        stop: Some(|| Box::pin(services::stop_harness())),
        role: Role::Standard,
        shutdown_rank: 5,
    },
    DaemonService {
        name: service_name::TREESITTER,
        start: || Box::pin(services::start_treesitter()),
        stop: Some(|| Box::pin(services::stop_treesitter())),
        role: Role::Standard,
        shutdown_rank: 3,
    },
    DaemonService {
        name: service_name::WORKSPACES,
        start: || Box::pin(services::start_workspaces()),
        stop: Some(|| Box::pin(services::stop_workspaces())),
        role: Role::Standard,
        shutdown_rank: 2,
    },
    DaemonService {
        name: service_name::N8N,
        start: || Box::pin(services::start_n8n()),
        stop: Some(|| Box::pin(services::stop_n8n())),
        role: Role::Standard,
        shutdown_rank: 1,
    },
    // wylde-vpn is user-started (Role::UserStarted): absent from the boot
    // sweep, present in shutdown + dispatch. Placed last because table
    // order is boot order and the VPN never boots — its position here is
    // cosmetic; shutdown position is `shutdown_rank`, dispatch is by name.
    DaemonService {
        name: service_name::VPN,
        start: || Box::pin(services::start_vpn()),
        stop: Some(|| Box::pin(services::stop_vpn())),
        role: Role::UserStarted,
        shutdown_rank: 9,
    },
];

/// The services spawned during the daemon boot sweep, in boot order.
/// Drives the boot loop in `daemon.rs`.
pub fn boot_sequence() -> impl Iterator<Item = &'static DaemonService> {
    DAEMON_MANAGED.iter().filter(|s| s.role.boots())
}

/// The managed services torn down at shutdown, in teardown order
/// (ascending [`DaemonService::shutdown_rank`]). Drives the shutdown loop
/// in `state/mod.rs`. Every entry returned carries a `stop` hook.
pub fn shutdown_sequence() -> Vec<&'static DaemonService> {
    let mut v: Vec<&'static DaemonService> = DAEMON_MANAGED
        .iter()
        .filter(|s| s.role.is_managed())
        .collect();
    v.sort_by_key(|s| s.shutdown_rank);
    v
}

/// Look up the dispatchable (managed) entry for `name`, or `None`. Excludes
/// the [`Role::BootOnlyNoop`] memory scheduler. Drives `dispatch_start` /
/// `dispatch_stop` and the `is_manageable` core check in `control.rs`.
pub fn dispatchable(name: &str) -> Option<&'static DaemonService> {
    DAEMON_MANAGED
        .iter()
        .find(|s| s.name == name && s.role.is_managed())
}

/// The core, in-tree service names accepted by `service.start`/`.stop`/
/// `.wake` and reported by the no-spawn parity surface (the old
/// `CORE_SERVICES`). Managed set = [`Role::Standard`] + [`Role::UserStarted`].
pub fn core_service_names() -> Vec<&'static str> {
    DAEMON_MANAGED
        .iter()
        .filter(|s| s.role.is_managed())
        .map(|s| s.name)
        .collect()
}

/// The Windows image names of the daemon-managed services that have a
/// standalone process (Memgraph and the memory scheduler have none).
///
/// **The GUI does not call this** — and could not, since this crate pulls
/// in tokio + anyhow, which must not ripple into the shipped GUI binary
/// (the objection that deferred PR #109). The GUI's hard-kill roster and
/// drain-wait poll set derive from [`wylde_stack::shutdown_targets`],
/// which reads the same names out of the dependency-lean
/// [`wylde_stack::CORE_STACK`].
///
/// This function is the in-tree equivalent for daemon-side callers. It is
/// kept because it is the honest expression of "the images this crate
/// manages"; if it acquires no caller it should be deleted rather than
/// left as a decoy — an unused derivation next to a hand-kept list is
/// what made issue #124 look fixed for two releases.
pub fn kill_target_images() -> Vec<&'static str> {
    DAEMON_MANAGED.iter().filter_map(|s| s.image()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The keystone gate (issue #101): the set spawned at boot, the set
    /// torn down at shutdown, and the set routable via dispatch must all
    /// agree — modulo the two *typed* exceptions. A service wired into one
    /// path but forgotten on another turns this RED.
    ///
    /// This is provable-by-construction here because all three sets derive
    /// from [`DAEMON_MANAGED`]; the test pins the intended asymmetries so a
    /// *new* accidental one (e.g. a Standard service that silently lost its
    /// stop hook) still fails.
    #[test]
    fn boot_shutdown_dispatch_sets_agree() {
        let boot: BTreeSet<&str> = boot_sequence().map(|s| s.name).collect();
        let shutdown: BTreeSet<&str> = shutdown_sequence().into_iter().map(|s| s.name).collect();
        let dispatch: BTreeSet<&str> = DAEMON_MANAGED
            .iter()
            .filter(|s| dispatchable(s.name).is_some())
            .map(|s| s.name)
            .collect();

        // Dispatch == shutdown exactly: everything routable on demand is
        // also drained at shutdown, and vice-versa.
        assert_eq!(
            dispatch, shutdown,
            "dispatch set and shutdown set diverged: {dispatch:?} vs {shutdown:?}"
        );

        // Boot vs shutdown differ ONLY by the two intended exceptions:
        //   - memory_scheduler is in boot, not in shutdown (BootOnlyNoop)
        //   - vpn is in shutdown, not in boot (UserStarted)
        let boot_only: BTreeSet<&str> = boot.difference(&shutdown).copied().collect();
        let shutdown_only: BTreeSet<&str> = shutdown.difference(&boot).copied().collect();
        assert_eq!(
            boot_only,
            BTreeSet::from([service_name::MEMORY_SCHEDULER]),
            "the ONLY service booted-but-not-stopped must be the memory scheduler \
             (BootOnlyNoop); a new entry here is an accidental orphan"
        );
        assert_eq!(
            shutdown_only,
            BTreeSet::from([service_name::VPN]),
            "the ONLY service stopped-but-not-booted must be the VPN (UserStarted); \
             a new entry here is an accidental divergence"
        );
    }

    /// Every entry's `Role` must be self-consistent with its `stop` hook:
    /// only a `BootOnlyNoop` entry may omit `stop`, and it must omit it.
    #[test]
    fn stop_hook_matches_role() {
        for s in DAEMON_MANAGED {
            match s.role {
                Role::BootOnlyNoop => assert!(
                    s.stop.is_none(),
                    "{}: BootOnlyNoop must have no stop hook",
                    s.name
                ),
                Role::Standard | Role::UserStarted => assert!(
                    s.stop.is_some(),
                    "{}: a managed service must carry a stop hook",
                    s.name
                ),
            }
        }
    }

    /// `shutdown_rank` must be a strict total order over the managed set —
    /// no two managed services share a rank (an ambiguous teardown order).
    #[test]
    fn shutdown_ranks_are_unique() {
        let ranks: Vec<u16> = shutdown_sequence()
            .iter()
            .map(|s| s.shutdown_rank)
            .collect();
        let unique: BTreeSet<u16> = ranks.iter().copied().collect();
        assert_eq!(
            ranks.len(),
            unique.len(),
            "two managed services share a shutdown_rank — teardown order is ambiguous"
        );
    }

    /// Names must be unique across the table (a duplicate would make
    /// `dispatchable` / lookups ambiguous).
    #[test]
    fn names_are_unique() {
        let names: Vec<&str> = DAEMON_MANAGED.iter().map(|s| s.name).collect();
        let unique: BTreeSet<&str> = names.iter().copied().collect();
        assert_eq!(
            names.len(),
            unique.len(),
            "duplicate service name in DAEMON_MANAGED"
        );
    }

    /// **The whole-stack coverage gate (issue #97).**
    ///
    /// Every daemon-managed service must have a row in
    /// [`wylde_stack::CORE_STACK`], because that table is what the
    /// self-updater turns into its release-asset set and what the launcher
    /// resolves against. A service wired up here but absent there would boot
    /// on a dev machine and be **silently uncarried by the updater** — the
    /// exact shape of the bug that made a backend fix undeliverable.
    ///
    /// This fires in both directions, so neither table can grow a service the
    /// other doesn't know about. It is the standing guard behind "adding the
    /// Nth service needs zero updater and zero launcher edits": the edit you
    /// *do* have to make is the one that turns this red if you skip it.
    #[test]
    fn daemon_managed_and_stack_roster_agree() {
        let managed: BTreeSet<&str> = DAEMON_MANAGED.iter().map(|s| s.name).collect();
        let shipped: BTreeSet<&str> = wylde_stack::CORE_STACK.iter().map(|e| e.name).collect();

        let uncarried: Vec<_> = managed.difference(&shipped).collect();
        assert!(
            uncarried.is_empty(),
            "these daemon-managed services have no wylde_stack::CORE_STACK row, so the updater cannot carry them and the launcher cannot resolve them: {uncarried:?}. Add a CoreEntry in wylde-stack/src/roster.rs."
        );

        let orphaned: Vec<_> = shipped.difference(&managed).collect();
        assert!(
            orphaned.is_empty(),
            "these wylde_stack::CORE_STACK rows have no daemon-managed counterpart, so nothing ever starts them: {orphaned:?}"
        );
    }

    /// Every managed service that ships a binary is actually *reachable*
    /// through the roster the updater and launcher consume.
    ///
    /// Deliberately NOT a second name-set comparison: that would be a strict
    /// subset of [`daemon_managed_and_stack_roster_agree`] above and could
    /// only ever fail when that one already had — a check that cannot fail
    /// independently is the "permanently green" trap #97 calls out. This
    /// instead exercises the derivation path (`image()` → `CORE_STACK` →
    /// `roster()`), so it fires if the lookup itself breaks while both tables
    /// still agree.
    #[test]
    fn managed_services_with_a_binary_appear_in_the_shipped_roster() {
        // Root at a path that cannot exist so no `Services/` bucket is walked
        // and the roster is exactly the in-tree tier under test.
        let shipped: BTreeSet<String> = wylde_stack::roster_in(std::path::Path::new(
            "this-root-does-not-exist-so-only-in-tree-entries-appear",
        ))
        .into_iter()
        .map(|b| b.name)
        .collect();

        for svc in DAEMON_MANAGED.iter().filter(|s| s.image().is_some()) {
            assert!(
                shipped.contains(svc.name),
                "{} declares an image but does not reach the shipped roster — \
                 the updater would not carry it and the launcher would not \
                 resolve it",
                svc.name
            );
        }
        // ...and the reverse for the typed exclusions: a service with no
        // standalone process must NOT appear, or the updater would demand a
        // release asset that can never exist.
        for svc in DAEMON_MANAGED.iter().filter(|s| s.image().is_none()) {
            assert!(
                !shipped.contains(svc.name),
                "{} has no standalone binary but appears in the shipped \
                 roster; the updater would require an asset for it",
                svc.name
            );
        }
    }

    /// Kill-target images (when present) are unique and `.exe`-suffixed —
    /// the derivation a hard-kill roster would consume.
    #[test]
    fn kill_target_images_are_wellformed() {
        let imgs = kill_target_images();
        let unique: BTreeSet<&str> = imgs.iter().copied().collect();
        assert_eq!(imgs.len(), unique.len(), "duplicate kill-target image");
        for img in imgs {
            assert!(
                img.ends_with(".exe"),
                "kill-target image {img:?} must end in .exe"
            );
        }
    }
}

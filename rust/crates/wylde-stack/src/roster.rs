//! The stack roster: every binary that ships, derived rather than listed.
//!
//! [`roster_in`] is the one function the updater and the launcher both call.
//! It answers "what makes up the stack" from two sources:
//!
//! 1. **The in-tree core tier** — [`CORE_STACK`]. This is an enumeration and
//!    unavoidably so: each of these services also needs a start/stop hook,
//!    which lives in `wylde_lifecycle::daemon_managed::DAEMON_MANAGED`. What
//!    makes it safe is that the *names* exist only here, and a
//!    `daemon_managed` gate fails red if the two tables' name sets diverge.
//! 2. **The out-of-tree `Services/` bucket** — a live filesystem walk, the
//!    same one `wylde_lifecycle::registry::discovered_bucket_services` reads
//!    (it delegates here). This half is pure discovery: drop a service in and
//!    both consumers pick it up with no code edit anywhere.
//!
//! Services with no standalone Wylde binary (Memgraph is JVM-supervised, the
//! memory scheduler runs in-process inside the harness) carry `image: None`
//! and are correctly absent from the roster — there is nothing to ship for
//! them. That is a typed exclusion, not an oversight, so the coverage gate
//! can tell the two apart.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{service_name as sn, wylde_root, EXE_SUFFIX, RELEASE_TARGET};

/// Out-of-tree buckets walked for sibling services. Mirrors
/// `wylde_lifecycle::registry::SERVICE_BUCKETS`; `Extensions/` is the
/// extension bridge's business and `Core/Plugins/` is compiled in.
const SERVICE_BUCKETS: &[&str] = &["Services"];

/// Folder-name prefixes excluded from discovery (`.` dotfiles, `_` private).
const EXCLUDED_PREFIXES: &[char] = &['_', '.'];

/// Which layer of the stack a binary belongs to. The updater treats all
/// three identically (verify, stage, swap); the launcher needs the
/// distinction because the GUI builds out of a separate workspace and the
/// daemon has to be up before the GUI starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// The gpui shell — `Core/GUI/target/<profile>/wylde-gui.exe`.
    Gui,
    /// The lifecycle daemon — spawns everything else.
    Daemon,
    /// A backend service, in-tree or bucket-discovered.
    Service,
}

/// One row of the in-tree core tier.
pub struct CoreEntry {
    /// Canonical service name — a [`crate::service_name`] constant.
    pub name: &'static str,
    /// Windows image name, or `None` for a service that owns no standalone
    /// Wylde binary. `None` entries are deliberately unshippable: there is no
    /// artifact for the updater to carry and nothing for the launcher to run.
    pub image: Option<&'static str>,
}

/// The in-tree core tier. Names live here and nowhere else;
/// `wylde_lifecycle::daemon_managed::DAEMON_MANAGED` references these
/// constants for its own rows and a gate there asserts the sets agree.
pub const CORE_STACK: &[CoreEntry] = &[
    CoreEntry {
        name: sn::MEMGRAPH,
        image: None, // JVM-supervised — no wylde-memgraph.exe exists.
    },
    CoreEntry {
        name: sn::MEMORY_SCHEDULER,
        image: None, // In-process inside wylde-harness (slice R2b).
    },
    CoreEntry {
        name: sn::VRAM_BROKER,
        image: Some("wylde-vram-broker.exe"),
    },
    CoreEntry {
        name: sn::VOICE,
        image: Some("wylde-voice.exe"),
    },
    CoreEntry {
        name: sn::DEVICE_GATE,
        image: Some("wylde-device-gate.exe"),
    },
    CoreEntry {
        name: sn::EXTENSION_BRIDGE,
        image: Some("wylde-extension-bridge.exe"),
    },
    CoreEntry {
        name: sn::OLLAMA,
        image: Some("wylde-ollama.exe"),
    },
    CoreEntry {
        name: sn::GATEWAY,
        image: Some("wylde-gateway.exe"),
    },
    CoreEntry {
        name: sn::HARNESS,
        image: Some("wylde-harness.exe"),
    },
    CoreEntry {
        name: sn::TREESITTER,
        image: Some("wylde-treesitter.exe"),
    },
    CoreEntry {
        name: sn::WORKSPACES,
        image: Some("wylde-workspaces.exe"),
    },
    CoreEntry {
        name: sn::N8N,
        image: Some("wylde-n8n.exe"),
    },
    CoreEntry {
        name: sn::VPN,
        image: Some("wylde-vpn.exe"),
    },
];

/// One shippable binary in the stack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StackBinary {
    /// Canonical service name, e.g. `wylde-gateway`.
    pub name: String,
    /// On-disk / in-release file name, e.g. `wylde-gateway.exe`.
    pub image: String,
    pub tier: Tier,
    /// For a bucket-discovered sibling, the `Services/<name>/` folder it was
    /// found in. `None` for in-tree binaries.
    pub folder: Option<PathBuf>,
}

impl StackBinary {
    /// The release-asset name the updater looks for:
    /// `<image-stem>-<target>.exe`, e.g.
    /// `wylde-gateway-x86_64-pc-windows-msvc.exe`. Matches the naming already
    /// documented in `docs/self-updater-design.md` for the GUI, generalised
    /// to every binary.
    pub fn asset_name(&self) -> String {
        let stem = self.image.strip_suffix(".exe").unwrap_or(&self.image);
        format!("{stem}-{RELEASE_TARGET}.exe")
    }

    /// The detached-signature asset that must accompany [`Self::asset_name`].
    /// Every binary is verified individually — there is no bundle signature
    /// and no unsigned member.
    pub fn signature_name(&self) -> String {
        format!("{}.minisig", self.asset_name())
    }

    /// Is this binary discovered out-of-tree (a `Services/*` sibling)?
    pub fn is_sibling(&self) -> bool {
        self.folder.is_some()
    }
}

/// The stack roster rooted at [`wylde_root`].
pub fn roster() -> Vec<StackBinary> {
    roster_in(&wylde_root())
}

/// [`roster`] rooted at an explicit `root` — the tempdir-testable entry
/// point, and the seam the coverage gate drives a synthetic service through.
///
/// Order is stable and meaningful: GUI, daemon, in-tree services
/// (declaration order), then bucket siblings (alphabetical).
pub fn roster_in(root: &Path) -> Vec<StackBinary> {
    let mut out = Vec::with_capacity(CORE_STACK.len() + 4);

    out.push(StackBinary {
        name: sn::GUI.to_owned(),
        image: format!("{}{EXE_SUFFIX}", sn::GUI),
        tier: Tier::Gui,
        folder: None,
    });
    out.push(StackBinary {
        name: sn::LIFECYCLE.to_owned(),
        image: format!("{}{EXE_SUFFIX}", sn::LIFECYCLE),
        tier: Tier::Daemon,
        folder: None,
    });

    for entry in CORE_STACK {
        // `image: None` ⇒ nothing ships for this service. Typed exclusion.
        let Some(image) = entry.image else {
            continue;
        };
        out.push(StackBinary {
            name: entry.name.to_owned(),
            // The table's image names are Windows-shaped; off-Windows the
            // suffix is dropped so the resolver is exercisable on any host.
            image: retarget_suffix(image),
            tier: Tier::Service,
            folder: None,
        });
    }

    for folder in discovered_folders(root) {
        let Some(name) = folder.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let name = name_with_wylde_prefix(name);
        // A sibling that shadows an in-tree name is not a second binary.
        if out.iter().any(|b| b.name == name) {
            continue;
        }
        out.push(StackBinary {
            image: format!("{name}{EXE_SUFFIX}"),
            name,
            tier: Tier::Service,
            folder: Some(folder),
        });
    }

    out
}

/// Rewrite a `.exe`-suffixed image name for the host. Identity on Windows.
fn retarget_suffix(image: &str) -> String {
    match image.strip_suffix(".exe") {
        Some(stem) => format!("{stem}{EXE_SUFFIX}"),
        None => image.to_owned(),
    }
}

/// Immediate child folders of each service bucket that carry a readable
/// `manifest.json`. The same walk (and the same `WYLDE_SERVICES` override
/// semantics) as the daemon's registry, which delegates here.
///
/// **Clean no-op when a bucket is absent** — an unreadable directory yields
/// zero folders, so a tree with no `Services/` behaves exactly as one that
/// never had the bucket.
pub fn discovered_folders(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for bucket in SERVICE_BUCKETS {
        let dir = resolve_bucket_dir(root, bucket);
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut found: Vec<PathBuf> = entries
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
                // A folder without a manifest is not a service.
                path.join("manifest.json").is_file().then_some(path)
            })
            .collect();
        found.sort();
        out.extend(found);
    }
    out.dedup();
    out
}

/// Resolve a bucket's on-disk directory. `WYLDE_SERVICES` relocates the
/// `Services` bucket out-of-tree, but **only** when walking the real estate
/// root — tempdir-rooted callers (the whole test suite) stay env-independent.
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

/// Normalise a discovered folder name to the canonical `wylde-`-prefixed
/// service name. Mirrors the daemon registry's helper of the same name.
pub fn name_with_wylde_prefix(name: &str) -> String {
    let candidate: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c == ' ' { '-' } else { c })
        .collect();
    if candidate.starts_with("wylde-") {
        candidate
    } else {
        format!("wylde-{candidate}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Drop a bucket service into `root` the way a real one arrives: a
    /// folder under `Services/` with a manifest.
    fn drop_service(root: &Path, name: &str) -> PathBuf {
        let folder = root.join("Services").join(name);
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("manifest.json"), r#"{"enabled": true}"#).unwrap();
        folder
    }

    #[test]
    fn roster_carries_the_daemon_and_backend_services_not_just_the_gui() {
        // The #97 regression in its simplest form: the roster the updater
        // consumes must contain more than `wylde-gui`.
        let dir = TempDir::new().unwrap();
        let names: Vec<String> = roster_in(dir.path()).into_iter().map(|b| b.name).collect();

        assert!(names.contains(&sn::GUI.to_string()));
        assert!(
            names.contains(&sn::LIFECYCLE.to_string()),
            "the lifecycle daemon — the process that spawns every service — \
             must be in the roster; it was the headline omission of the \
             GUI-only updater. got: {names:?}"
        );
        for backend in [sn::GATEWAY, sn::HARNESS, sn::WORKSPACES, sn::TREESITTER] {
            assert!(
                names.contains(&backend.to_string()),
                "{backend} missing from the roster: {names:?}"
            );
        }
    }

    #[test]
    fn services_with_no_standalone_binary_are_excluded_by_type_not_omission() {
        let dir = TempDir::new().unwrap();
        let names: Vec<String> = roster_in(dir.path()).into_iter().map(|b| b.name).collect();
        // Memgraph is JVM-supervised and the scheduler is in-process: there
        // is no artifact to ship, so they must NOT appear.
        assert!(!names.contains(&sn::MEMGRAPH.to_string()));
        assert!(!names.contains(&sn::MEMORY_SCHEDULER.to_string()));
        // ...and the exclusion is declared, so the gate can distinguish it
        // from a service someone simply forgot.
        for name in [sn::MEMGRAPH, sn::MEMORY_SCHEDULER] {
            let entry = CORE_STACK.iter().find(|e| e.name == name).unwrap();
            assert!(entry.image.is_none());
        }
    }

    /// The autonomy property, at the roster layer: a service that did not
    /// exist when this code was written is carried anyway.
    #[test]
    fn a_newly_dropped_service_joins_the_roster_with_no_code_edit() {
        let dir = TempDir::new().unwrap();
        let before = roster_in(dir.path()).len();

        drop_service(dir.path(), "acme-widget");

        let after = roster_in(dir.path());
        assert_eq!(after.len(), before + 1);
        let added = after
            .iter()
            .find(|b| b.name == "wylde-acme-widget")
            .unwrap();
        assert!(added.is_sibling());
        assert_eq!(added.tier, Tier::Service);
        assert_eq!(
            added.asset_name(),
            "wylde-acme-widget-x86_64-pc-windows-msvc.exe"
        );
        assert_eq!(
            added.signature_name(),
            "wylde-acme-widget-x86_64-pc-windows-msvc.exe.minisig"
        );
    }

    #[test]
    fn a_folder_without_a_manifest_is_not_a_service() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("Services").join("not-a-service")).unwrap();
        let names: Vec<String> = roster_in(dir.path()).into_iter().map(|b| b.name).collect();
        assert!(!names.iter().any(|n| n.contains("not-a-service")));
    }

    #[test]
    fn absent_bucket_is_a_clean_no_op() {
        let dir = TempDir::new().unwrap();
        let without = roster_in(dir.path());
        fs::create_dir_all(dir.path().join("Services")).unwrap();
        let with_empty = roster_in(dir.path());
        assert_eq!(without, with_empty);
    }

    #[test]
    fn discovery_ignores_private_and_dotted_folders() {
        let dir = TempDir::new().unwrap();
        drop_service(dir.path(), "_staging");
        drop_service(dir.path(), ".hidden");
        drop_service(dir.path(), "real");
        let names: Vec<String> = roster_in(dir.path()).into_iter().map(|b| b.name).collect();
        assert!(names.contains(&"wylde-real".to_string()));
        assert!(!names.iter().any(|n| n.contains("staging")));
        assert!(!names.iter().any(|n| n.contains("hidden")));
    }

    #[test]
    fn a_sibling_never_duplicates_an_in_tree_binary() {
        let dir = TempDir::new().unwrap();
        drop_service(dir.path(), "gateway");
        let names: Vec<String> = roster_in(dir.path()).into_iter().map(|b| b.name).collect();
        assert_eq!(
            names.iter().filter(|n| *n == sn::GATEWAY).count(),
            1,
            "a Services/gateway sibling must not produce a second \
             wylde-gateway row: {names:?}"
        );
    }

    #[test]
    fn asset_names_follow_the_published_convention() {
        let dir = TempDir::new().unwrap();
        let gui = roster_in(dir.path())
            .into_iter()
            .find(|b| b.name == sn::GUI)
            .unwrap();
        // This exact name is what ships today (CHANGELOG, release notes) —
        // the generalisation must not rename the GUI asset.
        assert_eq!(gui.asset_name(), "wylde-gui-x86_64-pc-windows-msvc.exe");
    }

    #[test]
    fn core_stack_names_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for entry in CORE_STACK {
            assert!(seen.insert(entry.name), "duplicate row: {}", entry.name);
        }
    }
}

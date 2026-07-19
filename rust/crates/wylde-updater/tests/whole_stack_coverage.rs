//! **The autonomy gate (#97 criterion 4, #92).**
//!
//! The thesis of the 0.2 stability milestone is that adding the Nth service
//! must be covered *by construction*, not by remembering to update a list.
//! Two consumers have to pick it up:
//!
//! * the **updater**, which must fetch, verify, and stage a binary for it, and
//! * the **launcher**, which must resolve and run it.
//!
//! Both used to keep their own hand-written notion of what the stack is —
//! the updater's was the literal `wylde-gui`, the launcher's was a per-binary
//! candidate walk — so a new service was silently covered by neither. This
//! test drives a service that did not exist when any of this code was written
//! through both paths at once.
//!
//! It is deliberately written as a *behavioural* test rather than a
//! list-comparison. Comparing two lists that are both derived from the same
//! table is tautological and would stay green through exactly the bug it
//! claims to prevent (the mistake made twice already in this repo — a
//! permanently-green check is worse than none). Here the service is created
//! on disk, discovered by a filesystem walk, and then required to appear in
//! the updater's asset set and the launcher's resolution. Break discovery and
//! this goes red.

use std::fs;
use std::path::Path;

use wylde_updater::{pick_assets, Release, ReleaseAsset, UpdateError};

/// Drop a service into the `Services/` bucket exactly the way a real one
/// arrives: a folder with a manifest, and a binary beside it.
fn drop_service(root: &Path, name: &str, with_binary: bool) {
    let folder = root.join("Services").join(name);
    fs::create_dir_all(&folder).unwrap();
    fs::write(
        folder.join("manifest.json"),
        r#"{"name": "PLACEHOLDER", "enabled": true}"#.replace("PLACEHOLDER", name),
    )
    .unwrap();
    if with_binary {
        let image = format!("wylde-{name}{}", wylde_stack::EXE_SUFFIX);
        fs::write(folder.join(image), b"service binary").unwrap();
    }
}

/// A release carrying an asset pair for every entry in `roster`.
fn release_for(roster: &[wylde_stack::StackBinary]) -> Release {
    let mut assets = Vec::new();
    for entry in roster {
        for name in [entry.asset_name(), entry.signature_name()] {
            assets.push(ReleaseAsset {
                url: format!("https://example.test/{name}"),
                name,
                size: 16,
            });
        }
    }
    Release {
        tag_name: "v9.9.9".into(),
        draft: false,
        prerelease: false,
        body: String::new(),
        html_url: String::new(),
        assets,
    }
}

/// The gate. A service nobody wrote code for is carried by the updater AND
/// resolved by the launcher.
#[test]
fn the_nth_service_is_covered_by_the_updater_and_the_launcher_with_no_code_edit() {
    let root = tempfile::tempdir().unwrap();
    // Give the tree a daemon so build-tree resolution has its anchor.
    let bin = root.path().join("rust").join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        bin.join(format!("wylde-lifecycle{}", wylde_stack::EXE_SUFFIX)),
        b"daemon",
    )
    .unwrap();

    // Nobody has ever heard of this service.
    drop_service(root.path(), "quasar", true);

    // 1. Discovery picks it up.
    let roster = wylde_stack::roster_in(root.path());
    let entry = roster
        .iter()
        .find(|b| b.name == "wylde-quasar")
        .expect("a service dropped into Services/ must appear in the roster");

    // 2. The UPDATER carries it — it has an asset slot, and that slot demands
    //    its own signature like every other member.
    let release = release_for(&roster);
    let picked = pick_assets(&release, &roster).unwrap();
    let carried = picked.iter().find(|a| a.name == "wylde-quasar").expect(
        "the updater must carry a discovered service; if this fails, \
                 a backend fix for it can never reach a user (#97)",
    );
    assert_eq!(carried.binary.name, entry.asset_name());
    assert_eq!(carried.signature.name, entry.signature_name());
    assert_eq!(
        carried.signature.name,
        format!("{}.minisig", carried.binary.name),
        "fail-closed verification is per binary — a discovered service must \
         not be exempt from it"
    );

    // 3. The LAUNCHER resolves it to a real file.
    let stack = wylde_stack::current::resolve_in(root.path());
    let resolved = stack
        .path_of("wylde-quasar")
        .expect("the launcher must resolve a discovered service (#92)");
    assert!(resolved.is_file());
    assert!(resolved.starts_with(root.path().join("Services").join("quasar")));
}

/// The same gate from the failure side: if a discovered service has **no**
/// asset in the release, the updater must not quietly proceed as though the
/// stack were complete. In-tree binaries are hard-required; a bucket sibling
/// is optional by design, so the assertion here is that the *distinction is
/// real* — remove an in-tree asset and the whole release is refused.
#[test]
fn a_release_missing_an_in_tree_binary_is_refused_outright() {
    let root = tempfile::tempdir().unwrap();
    let roster = wylde_stack::roster_in(root.path());

    let mut release = release_for(&roster);
    let victim = format!("wylde-harness-{}.exe", wylde_stack::RELEASE_TARGET);
    release.assets.retain(|a| a.name != victim);

    assert!(
        matches!(pick_assets(&release, &roster), Err(UpdateError::NoAsset)),
        "a release missing a required backend binary must be refused, not \
         installed as a partial stack"
    );
}

/// Every roster entry maps to a distinct asset name. A collision would mean
/// two services silently sharing one binary in the staged stack.
#[test]
fn every_roster_entry_has_a_distinct_asset_slot() {
    let root = tempfile::tempdir().unwrap();
    drop_service(root.path(), "alpha", false);
    drop_service(root.path(), "beta", false);

    let roster = wylde_stack::roster_in(root.path());
    let mut names: Vec<String> = roster.iter().map(|b| b.asset_name()).collect();
    let total = names.len();
    names.sort();
    names.dedup();
    assert_eq!(total, names.len(), "two stack binaries share an asset name");
}

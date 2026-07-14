//! Live probe for the estate path indirection.
//!
//! Prints the resolved `WYLDE_ROOT`, the `WYLDE_SERVICES` override (if any),
//! the process cwd, and the sibling services that `registry::
//! discovered_bucket_services()` — the *exact* function the Lifecycle daemon
//! calls at boot — discovers. Run it with different `WYLDE_ROOT` /
//! `WYLDE_SERVICES` values and from different working directories to prove
//! discovery follows the env vars, not the cwd or a hardcoded default.
//!
//! ```text
//! cargo build -p wylde-lifecycle --example discover_probe
//! ./target/debug/examples/discover_probe
//! ```
fn main() {
    let root = std::env::var("WYLDE_ROOT").unwrap_or_else(|_| "<unset -> \".\" cwd fallback>".into());
    let services =
        std::env::var("WYLDE_SERVICES").unwrap_or_else(|_| "<unset -> WYLDE_ROOT/Services>".into());
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<none>".into());

    println!("WYLDE_ROOT     = {root}");
    println!("WYLDE_SERVICES = {services}");
    println!("cwd            = {cwd}");

    let found = wylde_lifecycle::registry::discovered_bucket_services();
    println!("discovered {} service(s):", found.len());
    for d in &found {
        println!(
            "  - {} (enabled={}, folder={})",
            d.name,
            d.enabled,
            d.folder.display()
        );
    }
}

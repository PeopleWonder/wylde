//! Scoped-lens region derivation (concept-routing plan §6.2, **R3a**).
//!
//! The concept *lens* itself — `members ∩ region` — already lives in
//! `wylde-workspaces` (`concepts/lens.rs`); this module owns the *one pure
//! decision the lens needs from the routing layer*: **given the turn's active
//! file, what region string should the lens narrow to?**
//!
//! When a concept is curated AND an active file is present, the R2 injection
//! narrows that concept's member chunks to the active file's *subsystem* — the
//! "authentication within the VPN tunnel vs. within an extension" idea (plan
//! §6.2): `Authentication` curated while a VPN file is open injects from
//! `authentication ∩ services/vpn`, not the whole concept.
//!
//! ## Behaviour-safe (the R3 contract)
//!
//! * The region is only ever *consumed* on the routing-on, curated path — when
//!   the master toggle is OFF the lens is never reached, so retrieval is
//!   byte-identical to today.
//! * No active file ⇒ [`region_for_active_file`] returns `None` ⇒ the caller
//!   passes no scope ⇒ the whole concept (unchanged from R2).
//! * An **empty intersection** is *not* this module's concern — the caller
//!   ([`crate::curation`]'s server-side injector) falls back to the whole
//!   concept so a too-narrow region never silently drops everything.
//!
//! Pure + gpui-free + I/O-free: this is only the region *string* computation.
//! The intersection is `wylde-workspaces`'s existing `lens()`.

/// Derive the scoping region from the turn's active file path.
///
/// The region is the active file's **containing directory** — the subsystem the
/// user is working in. `services/vpn/tunnel.rs` ⇒ `services/vpn`, so a concept
/// lensed against it keeps only members under that subtree (the existing
/// `lens()` does a path-component-boundary prefix match, so a directory region
/// is exactly what it wants).
///
/// Returns `None` when there is no usable region — a blank path, or a bare
/// filename with no directory component (lensing to "the whole repo" is the
/// same as not lensing, so we signal "no scope" rather than an empty string).
/// Both `/` and `\` are accepted (Windows paths arrive on the wire too); the
/// returned region is normalised to `/`.
pub fn region_for_active_file(active_file: &str) -> Option<String> {
    let norm = active_file.trim().replace('\\', "/");
    let trimmed = norm.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.rsplit_once('/') {
        // Strip the filename → the containing directory is the region.
        Some((parent, _)) if !parent.is_empty() => Some(parent.to_owned()),
        // A bare filename (`main.rs`) or a leading-slash root (`/x`): no
        // narrower subsystem than the repo — treat as no scope.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_is_the_active_files_directory() {
        assert_eq!(
            region_for_active_file("services/vpn/tunnel.rs").as_deref(),
            Some("services/vpn")
        );
        assert_eq!(
            region_for_active_file("a/b/c/d.rs").as_deref(),
            Some("a/b/c")
        );
    }

    #[test]
    fn windows_paths_normalise_to_forward_slash() {
        assert_eq!(
            region_for_active_file("services\\vpn\\tunnel.rs").as_deref(),
            Some("services/vpn")
        );
    }

    #[test]
    fn bare_filename_or_blank_is_no_scope() {
        // No directory component ⇒ no narrower subsystem ⇒ no scope.
        assert_eq!(region_for_active_file("main.rs"), None);
        assert_eq!(region_for_active_file(""), None);
        assert_eq!(region_for_active_file("   "), None);
        // A leading-slash root has an empty parent — also no scope.
        assert_eq!(region_for_active_file("/top.rs"), None);
    }

    #[test]
    fn trailing_slashes_are_tolerated() {
        // A directory path passed with a trailing slash still yields its parent.
        assert_eq!(
            region_for_active_file("services/vpn/").as_deref(),
            Some("services")
        );
    }
}

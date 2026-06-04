//! The shared minisign public key embedded in every shipped binary.
//!
//! A minisign public key is **public by design** — it is meant to be
//! embedded and committed. The matching **private** signing key is never
//! committed (see `keys/.gitignore` and `docs/self-updater-design.md`).
//!
//! The fail-closed sentinel ([`PLACEHOLDER`]) stays in the source so
//! [`crate::verify_signature`] still refuses to install on any future build
//! whose `PUBLIC_KEY` is reset to it — an un-keyed build can never be tricked
//! into swapping in a downloaded binary. To rotate the key, run
//! `rsign generate` and copy the base64 key line (the second line of the
//! generated `.pub` file) into `PUBLIC_KEY` below, then cut a release built
//! from it; from then on the binary trusts only releases signed by the
//! matching private key.
//!
//! See the "Key management" section of `docs/self-updater-design.md`.

/// The sentinel placeholder value. While `PUBLIC_KEY` equals this, the
/// updater has no production key and verification fails closed.
pub(crate) const PLACEHOLDER: &str = "PLACEHOLDER_REPLACE_BEFORE_RELEASE";

/// The embedded minisign public key (base64, one line).
///
/// Aaron's production signing key (Ed25519/minisign), key ID
/// `DA7E13F4E9F2ACB6`, generated 2026-06-04. The matching **private** key
/// lives only on the dev host at
/// `rust/crates/wylde-updater/keys/wylde-signing.key` (gitignored) and is
/// never committed. Rotating it is a one-line change here — see the module
/// docs and `docs/self-updater-design.md`.
pub const PUBLIC_KEY: &str = "RWS2rPLp9BN+2obJk6h80IJAlurEyac8bz7REt0ea7v6uLG2AoppP0kb";

/// `true` once a real production key has been embedded (i.e. `PUBLIC_KEY`
/// is no longer the placeholder *and* parses as a minisign key). The GUI
/// uses this to keep the install path inert on un-keyed dev builds.
pub fn has_signing_key() -> bool {
    PUBLIC_KEY != PLACEHOLDER && minisign_verify::PublicKey::from_base64(PUBLIC_KEY).is_ok()
}

//! The shared minisign public key embedded in every shipped binary.
//!
//! A minisign public key is **public by design** — it is meant to be
//! embedded and committed. The matching **private** signing key is never
//! committed (see `keys/.gitignore` and `docs/self-updater-design.md`).
//!
//! Until the real release key exists, this is a clearly-labelled
//! placeholder. [`crate::verify_signature`] treats the placeholder as
//! [`crate::UpdateError::NoSigningKey`] and **refuses to install** — an
//! un-keyed build can never be tricked into swapping in a downloaded
//! binary. Replace the constant below with the base64 key line from
//! `rsign generate` (the second line of the generated `.pub` file) and cut
//! a release built from it; from then on the binary trusts only releases
//! signed by the matching private key.
//!
//! See the "Key management" section of `docs/self-updater-design.md`.

/// The sentinel placeholder value. While `PUBLIC_KEY` equals this, the
/// updater has no production key and verification fails closed.
pub(crate) const PLACEHOLDER: &str = "PLACEHOLDER_REPLACE_BEFORE_RELEASE";

/// The embedded minisign public key (base64, one line).
///
/// DEV PLACEHOLDER — replace before the first signed release. See the
/// module docs.
pub const PUBLIC_KEY: &str = PLACEHOLDER;

/// `true` once a real production key has been embedded (i.e. `PUBLIC_KEY`
/// is no longer the placeholder *and* parses as a minisign key). The GUI
/// uses this to keep the install path inert on un-keyed dev builds.
pub fn has_signing_key() -> bool {
    PUBLIC_KEY != PLACEHOLDER && minisign_verify::PublicKey::from_base64(PUBLIC_KEY).is_ok()
}

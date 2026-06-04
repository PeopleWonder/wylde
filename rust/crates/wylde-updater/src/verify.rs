//! minisign signature verification — the fail-closed gate before any
//! downloaded binary is allowed near the running executable.

use minisign_verify::{PublicKey, Signature};

use crate::pubkey::{self, PUBLIC_KEY};
use crate::UpdateError;

/// Verify `data` against the detached minisign signature `minisig` (the
/// full text of a `.minisig` file) using the **embedded** public key.
///
/// Returns [`UpdateError::NoSigningKey`] when the build still carries the
/// dev placeholder key, so an un-keyed build refuses to install rather
/// than failing in some softer way. Any tampering — flipped bytes, wrong
/// key, malformed signature — is [`UpdateError::Verify`].
pub fn verify_signature(data: &[u8], minisig: &str) -> Result<(), UpdateError> {
    if PUBLIC_KEY == pubkey::PLACEHOLDER {
        return Err(UpdateError::NoSigningKey);
    }
    verify_with_key(PUBLIC_KEY, data, minisig)
}

/// Verify against an explicit base64 public key. Split out from
/// [`verify_signature`] so the test suite can exercise the real
/// cryptographic path with an ephemeral keypair, without depending on the
/// (placeholder) embedded key.
pub(crate) fn verify_with_key(
    public_key_b64: &str,
    data: &[u8],
    minisig: &str,
) -> Result<(), UpdateError> {
    let public_key = PublicKey::from_base64(public_key_b64)
        .map_err(|_| UpdateError::NoSigningKey)?;
    let signature =
        Signature::decode(minisig).map_err(|e| UpdateError::Verify(format!("bad signature: {e}")))?;
    // `allow_legacy = false`: require the modern prehashed signature
    // algorithm that both rsign2 and the `minisign` crate emit by default.
    public_key
        .verify(data, &signature, false)
        .map_err(|e| UpdateError::Verify(format!("signature verification failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pubkey::has_signing_key;

    /// Mint an ephemeral keypair, sign `data`, and return
    /// `(public_key_base64, minisig_text)`.
    fn sign(data: &[u8]) -> (String, String) {
        let keypair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let signature_box = minisign::sign(
            None,
            &keypair.sk,
            std::io::Cursor::new(data),
            Some("test trusted comment"),
            Some("test untrusted comment"),
        )
        .unwrap();
        (keypair.pk.to_base64(), signature_box.to_string())
    }

    #[test]
    fn verifies_a_genuine_signature() {
        let data = b"the real binary bytes";
        let (pk, sig) = sign(data);
        assert!(verify_with_key(&pk, data, &sig).is_ok());
    }

    #[test]
    fn rejects_tampered_payload() {
        let data = b"the real binary bytes";
        let (pk, sig) = sign(data);
        let tampered = b"the EVIL binary bytes";
        assert!(matches!(
            verify_with_key(&pk, tampered, &sig),
            Err(UpdateError::Verify(_))
        ));
    }

    #[test]
    fn rejects_signature_from_a_different_key() {
        let data = b"payload";
        let (_pk_a, sig_a) = sign(data);
        let (pk_b, _sig_b) = sign(data);
        // pk_b never signed sig_a.
        assert!(matches!(
            verify_with_key(&pk_b, data, &sig_a),
            Err(UpdateError::Verify(_))
        ));
    }

    #[test]
    fn rejects_malformed_signature_text() {
        let (pk, _sig) = sign(b"x");
        assert!(matches!(
            verify_with_key(&pk, b"x", "not a minisig file"),
            Err(UpdateError::Verify(_))
        ));
    }

    #[test]
    fn placeholder_key_fails_closed() {
        // The shipped placeholder build must refuse to verify anything.
        let data = b"x";
        let (_pk, sig) = sign(data);
        assert!(matches!(
            verify_signature(data, &sig),
            Err(UpdateError::NoSigningKey)
        ));
        // ...and the convenience flag agrees there's no key yet.
        assert!(!has_signing_key());
    }
}

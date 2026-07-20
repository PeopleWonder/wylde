//! End-to-end proof that the **embedded** public key in `pubkey.rs` matches
//! the production private key on the maintainer's dev host.
//!
//! The fixture under `tests/fixtures/` was signed once, out of band, with
//! `rsign sign` using `keys/wylde-signing.key` (key ID `DA7E13F4E9F2ACB6`,
//! 2026-06-04). The signature is *public* by design, so it is committed; the
//! private key never is. If the embedded `PUBLIC_KEY` ever drifts from the
//! private key that produced this signature (a botched rotation, a typo in
//! the base64), this test fails — catching the exact mistake that would
//! otherwise ship an updater that trusts nobody.

const PAYLOAD: &[u8] = include_bytes!("fixtures/roundtrip_payload.bin");
const MINISIG: &str = include_str!("fixtures/roundtrip_payload.bin.minisig");

#[test]
fn embedded_public_key_verifies_a_real_production_signature() {
    // A real key must be baked in, otherwise the verify path would short out
    // with `NoSigningKey` regardless of the signature.
    assert!(
        wylde_updater::has_signing_key(),
        "no production key embedded — pubkey.rs still on the placeholder?"
    );
    wylde_updater::verify_signature(PAYLOAD, MINISIG)
        .expect("production signature must verify against the embedded public key");
}

#[test]
fn tampered_payload_is_rejected_under_the_embedded_key() {
    // Flip one byte of the signed payload: the real signature must no longer
    // verify, proving the round-trip above isn't vacuously passing.
    let mut tampered = PAYLOAD.to_vec();
    tampered[0] ^= 0xff;
    assert!(
        wylde_updater::verify_signature(&tampered, MINISIG).is_err(),
        "a tampered payload must fail verification under the embedded key"
    );
}

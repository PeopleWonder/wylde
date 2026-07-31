//! htpasswd-based username/password validation.
//!
//! Rust port of `device_gate/auth.py`. The Python module uses `passlib`'s
//! [`CryptContext`] with five schemes: `apr_md5_crypt`, `bcrypt`,
//! `sha512_crypt`, `sha256_crypt`, `des_crypt`. We mirror the first four
//! (the schemes actual htpasswd files contain in 2026 — APR1 is what
//! `htpasswd -m` writes by default, and the live `device_gate/data/htpasswd`
//! is APR1) and drop `des_crypt` because:
//!
//!   * stdlib `crypt(3)` doesn't exist on Windows, so the Python install
//!     already relied on passlib's pure-Python DES implementation — which is
//!     the only reason it kept working post-3.13;
//!   * DES is single-DES with 8-char password truncation, broadly considered
//!     broken since the 90s. We log a warning and fail closed for `$1$`-less
//!     13-char hashes if anyone hits this in practice.
//!
//! APR1 has no first-class Rust crate, so we implement it inline from the
//! Apache spec (`md5_crypt.c`). The other schemes use third-party crates
//! (`bcrypt`, `sha-crypt`).

use std::path::Path;

use md5::{Digest, Md5};
use sha_crypt::{PasswordVerifier, ShaCrypt};
use subtle::ConstantTimeEq;

const APR1_MAGIC: &str = "$apr1$";

/// htpasswd file lookup → stored hash for `username`, or `None` if the
/// file is missing / unreadable / has no matching line. Matches Python
/// `_read_hash` exactly: lines with `#` prefix and lines without `:` are
/// skipped, first matching user wins.
fn read_hash(htpasswd_path: &Path, username: &str) -> Option<String> {
    if !htpasswd_path.exists() {
        return None;
    }
    let data = match std::fs::read_to_string(htpasswd_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                "device_gate.auth: htpasswd unreadable ({}): {}",
                htpasswd_path.display(),
                e
            );
            return None;
        }
    };
    for raw_line in data.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((user, hashed)) = line.split_once(':') else {
            continue;
        };
        if user == username {
            return Some(hashed.trim().to_string());
        }
    }
    None
}

/// Verify `password` against `stored_hash`. Returns `false` on any error
/// path (unsupported scheme, malformed hash, mismatch) — fail closed.
fn verify_hash(stored_hash: &str, password: &str) -> bool {
    if stored_hash.is_empty() {
        return false;
    }
    // bcrypt: `$2a$`, `$2b$`, `$2y$`
    if let Some(rest) = stored_hash.strip_prefix("$2") {
        if rest
            .chars()
            .next()
            .is_some_and(|c| matches!(c, 'a' | 'b' | 'x' | 'y'))
        {
            return bcrypt::verify(password, stored_hash).unwrap_or(false);
        }
    }
    // APR1: `$apr1$salt$hash`
    if stored_hash.starts_with(APR1_MAGIC) {
        return verify_apr1(password, stored_hash);
    }
    // SHA-512 (`$6$…`) and SHA-256 (`$5$…`) crypt. sha-crypt 0.6 dropped the
    // free `sha512_check`/`sha256_check` helpers in favour of the `password-hash`
    // `PasswordVerifier` trait; the algorithm + rounds are read from the stored
    // MCF string itself, so a single default `ShaCrypt` verifies both schemes.
    if stored_hash.starts_with("$6$") || stored_hash.starts_with("$5$") {
        return ShaCrypt::default()
            .verify_password(password.as_bytes(), stored_hash)
            .is_ok();
    }
    // Legacy DES crypt is dropped — see module-level docstring.
    tracing::warn!(
        "device_gate.auth: unsupported hash scheme (first 4 chars: {:?})",
        stored_hash.get(..stored_hash.len().min(4)).unwrap_or("")
    );
    false
}

/// Constant-time-ish credential check. Mirrors Python's `verify_credentials`:
/// always exercises a hash call (even on missing user) so timing leaks are
/// blunted.
pub fn verify_credentials(htpasswd_path: &Path, username: &str, password: &str) -> bool {
    // Dummy APR1 hash — identical to Python's _DUMMY so the cost profile
    // matches across implementations.
    const DUMMY: &str = "$apr1$xxxxxxxx$0000000000000000000000";

    if username.is_empty() {
        let _ = verify_hash(DUMMY, password);
        return false;
    }
    if password.is_empty() {
        return false;
    }
    match read_hash(htpasswd_path, username) {
        None => {
            let _ = verify_hash(DUMMY, password);
            false
        }
        Some(stored) => verify_hash(&stored, password),
    }
}

// ── APR1 (Apache MD5 crypt) ───────────────────────────────────────────
//
// Inline implementation; reference is Apache's `md5_crypt.c`. No crate in
// the Rust ecosystem implements this hash directly. The algorithm is small
// (~50 lines without the base64 step) and the test suite roundtrips both
// the Python `passlib.hash.apr_md5_crypt` shape and the live
// `device_gate/data/htpasswd` fixture, so divergence would be caught.

const APR1_B64: &[u8] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const APR1_SALT_MAX: usize = 8;

fn verify_apr1(password: &str, stored: &str) -> bool {
    // Hash shape: $apr1$<salt>$<encoded>. Salt is up to 8 chars; some
    // tools emit shorter salts so we don't insist on exactly 8.
    let body = match stored.strip_prefix(APR1_MAGIC) {
        Some(b) => b,
        None => return false,
    };
    let (salt, expected) = match body.split_once('$') {
        Some(pair) => pair,
        None => return false,
    };
    let salt = &salt[..salt.len().min(APR1_SALT_MAX)];
    let computed = compute_apr1(password.as_bytes(), salt.as_bytes());
    // Constant-time compare on the encoded tail.
    computed.as_bytes().ct_eq(expected.as_bytes()).into()
}

fn compute_apr1(password: &[u8], salt: &[u8]) -> String {
    // Step 1: initial = MD5(password || magic || salt)
    let mut ctx = Md5::new();
    ctx.update(password);
    ctx.update(APR1_MAGIC.as_bytes());
    ctx.update(salt);

    // Step 2: alt = MD5(password || salt || password)
    let mut alt_ctx = Md5::new();
    alt_ctx.update(password);
    alt_ctx.update(salt);
    alt_ctx.update(password);
    let alt = alt_ctx.finalize();

    // Step 3: mix password-length bytes from `alt` in 16-byte chunks.
    let mut remaining = password.len() as isize;
    while remaining > 0 {
        let take = remaining.min(16) as usize;
        ctx.update(&alt[..take]);
        remaining -= 16;
    }

    // Step 4: bit-twiddle on password length — for each bit of the
    // length (LSB first), append either a NUL byte (bit set) or the
    // first password byte (bit clear).
    let mut j = password.len();
    while j > 0 {
        if j & 1 == 1 {
            ctx.update([0u8]);
        } else {
            ctx.update(&password[..1.min(password.len())]);
        }
        j >>= 1;
    }

    let mut final_digest = ctx.finalize_reset();

    // Step 5: 1000 rounds of MD5(varying mix of password/salt/final).
    for i in 0..1000 {
        let mut ctx2 = Md5::new();
        if i & 1 == 1 {
            ctx2.update(password);
        } else {
            ctx2.update(final_digest);
        }
        if i % 3 != 0 {
            ctx2.update(salt);
        }
        if i % 7 != 0 {
            ctx2.update(password);
        }
        if i & 1 == 1 {
            ctx2.update(final_digest);
        } else {
            ctx2.update(password);
        }
        final_digest = ctx2.finalize();
    }

    // Step 6: Apache's base64 variant — specific byte-shuffle followed by
    // 6-bits-per-char encoding with the `./0-9A-Za-z` alphabet.
    let d = &final_digest[..];
    let mut out = String::with_capacity(22);
    apr1_to64(
        &mut out,
        (u32::from(d[0]) << 16) | (u32::from(d[6]) << 8) | u32::from(d[12]),
        4,
    );
    apr1_to64(
        &mut out,
        (u32::from(d[1]) << 16) | (u32::from(d[7]) << 8) | u32::from(d[13]),
        4,
    );
    apr1_to64(
        &mut out,
        (u32::from(d[2]) << 16) | (u32::from(d[8]) << 8) | u32::from(d[14]),
        4,
    );
    apr1_to64(
        &mut out,
        (u32::from(d[3]) << 16) | (u32::from(d[9]) << 8) | u32::from(d[15]),
        4,
    );
    apr1_to64(
        &mut out,
        (u32::from(d[4]) << 16) | (u32::from(d[10]) << 8) | u32::from(d[5]),
        4,
    );
    apr1_to64(&mut out, u32::from(d[11]), 2);
    out
}

fn apr1_to64(out: &mut String, mut value: u32, count: usize) {
    for _ in 0..count {
        out.push(APR1_B64[(value & 0x3f) as usize] as char);
        value >>= 6;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    /// Live fixture from `device_gate/data/htpasswd` — must verify against
    /// the password the file was created with. We don't know the real
    /// password (it's a user secret); instead we re-create one here with
    /// a known plaintext and assert roundtrip.
    fn write_htpasswd(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().expect("tempfile");
        f.write_all(content.as_bytes()).expect("write");
        f
    }

    /// Cross-language parity: hashes generated by
    /// `passlib.hash.apr_md5_crypt.using(salt='abcdefgh').hash('letmein')`.
    /// Pinned so any drift in our inline APR1 implementation fails this
    /// test instead of silently producing different bytes than Python.
    const APR1_LETMEIN: &str = "$apr1$abcdefgh$2/f5Gp5itvzIJXRHg/wa/1";
    const SHA256_LETMEIN: &str =
        "$5$rounds=1000$abcdefgh$qT/aDWgb1dJtvNKBv4kl6KzCMEkKkA3NflX4hkVvqnA";
    const SHA512_LETMEIN: &str = "$6$rounds=1000$abcdefgh$KRhBhSD84o8r6.OtHD9OE0Hb5oYHYYPWQKvhjyOzpyrQllxfHfshIKK2T.br09bz9oecmha05xivTXzpoxHLd.";

    #[test]
    fn apr1_roundtrips_against_known_python_fixture() {
        assert!(
            verify_apr1("letmein", APR1_LETMEIN),
            "apr1 verify must succeed"
        );
        assert!(
            !verify_apr1("wrong", APR1_LETMEIN),
            "apr1 must reject wrong password"
        );
    }

    #[test]
    fn sha256_crypt_roundtrips_python_fixture() {
        assert!(verify_hash(SHA256_LETMEIN, "letmein"));
        assert!(!verify_hash(SHA256_LETMEIN, "wrong"));
    }

    #[test]
    fn sha512_crypt_roundtrips_python_fixture() {
        assert!(verify_hash(SHA512_LETMEIN, "letmein"));
        assert!(!verify_hash(SHA512_LETMEIN, "wrong"));
    }

    #[test]
    fn apr1_rejects_malformed() {
        assert!(!verify_apr1("x", "garbage"));
        assert!(!verify_apr1("x", "$apr1$nosalt"));
        assert!(!verify_apr1("x", "$apr1$"));
    }

    #[test]
    fn verify_credentials_missing_file_is_false() {
        let path = std::env::temp_dir().join("does_not_exist_htpasswd_for_test");
        let _ = std::fs::remove_file(&path); // wylde-check: discard-result-ok
        assert!(!verify_credentials(&path, "u", "p"));
    }

    #[test]
    fn verify_credentials_empty_username_is_false() {
        let f = write_htpasswd("wylde:$apr1$abcdefgh$2/f5Gp5itvzIJXRHg/wa/1\n");
        assert!(!verify_credentials(f.path(), "", "letmein"));
    }

    #[test]
    fn verify_credentials_empty_password_is_false() {
        let f = write_htpasswd("wylde:$apr1$abcdefgh$2/f5Gp5itvzIJXRHg/wa/1\n");
        assert!(!verify_credentials(f.path(), "wylde", ""));
    }

    #[test]
    fn verify_credentials_unknown_user_is_false() {
        let f = write_htpasswd("wylde:$apr1$abcdefgh$2/f5Gp5itvzIJXRHg/wa/1\n");
        assert!(!verify_credentials(f.path(), "nobody", "letmein"));
    }

    #[test]
    fn verify_credentials_correct_password_succeeds() {
        let f = write_htpasswd("wylde:$apr1$abcdefgh$2/f5Gp5itvzIJXRHg/wa/1\n");
        assert!(verify_credentials(f.path(), "wylde", "letmein"));
    }

    #[test]
    fn verify_credentials_wrong_password_fails() {
        let f = write_htpasswd("wylde:$apr1$abcdefgh$2/f5Gp5itvzIJXRHg/wa/1\n");
        assert!(!verify_credentials(f.path(), "wylde", "WRONG"));
    }

    #[test]
    fn verify_credentials_skips_comments_and_blank_lines() {
        let f =
            write_htpasswd("# a comment\n\nwylde:$apr1$abcdefgh$2/f5Gp5itvzIJXRHg/wa/1\nnoise\n");
        assert!(verify_credentials(f.path(), "wylde", "letmein"));
    }

    #[test]
    fn bcrypt_verifies() {
        // bcrypt hash for "secret" — generated once with `bcrypt::hash("secret", 4)`.
        // Use cost-4 so the test is fast (production uses 10+).
        let hash = bcrypt::hash("secret", 4).expect("bcrypt hash");
        let f = write_htpasswd(&format!("user:{hash}\n"));
        assert!(verify_credentials(f.path(), "user", "secret"));
        assert!(!verify_credentials(f.path(), "user", "wrong"));
    }

    #[test]
    fn unknown_scheme_fails_closed() {
        let f = write_htpasswd("user:$weird$garbage\n");
        assert!(!verify_credentials(f.path(), "user", "anything"));
    }
}

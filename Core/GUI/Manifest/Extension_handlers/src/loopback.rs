//! Loopback URL predicate, port of
//! `rust/crates/wylde-extension-bridge/src/manifest.rs::is_loopback_url`
//! into the GUI workspace.
//!
//! The extension-bridge crate is in the backend workspace (`rust/`)
//! which `Core/GUI/`'s standalone workspace deliberately does not pull
//! from — see the `Cargo.toml` comment.  Copying the 25-line predicate
//! beats inheriting the entire bridge crate's dep graph.  The
//! test-suite below is also lifted verbatim, so a regression in the
//! port is caught the same way it would have been caught on the
//! backend side.
//!
//! If the canonical predicate ever changes (e.g. add `[::1]` literal
//! port-zero handling), update both places in the same commit.

/// True iff `url` is an `http://` or `https://` URL whose host is the
/// loopback interface.  Conservative on purpose: a spoof like
/// `http://127.0.0.1.evil.com/` is rejected because the suffix
/// `.evil.com` makes the host *not* one of the three accepted literals.
pub fn is_loopback_url(url: &str) -> bool {
    let rest = match url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
    {
        Some(r) => r,
        None => return false,
    };
    let after_userinfo = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
    let host_with_port = after_userinfo.split(['/', '?', '#']).next().unwrap_or("");
    let host = if let Some(rest) = host_with_port.strip_prefix('[') {
        match rest.split_once(']') {
            Some((h, _)) => h,
            None => return false,
        }
    } else {
        host_with_port
            .rsplit_once(':')
            .map_or(host_with_port, |(h, _)| h)
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ported from `wylde-extension-bridge` `loopback_predicate_accepts_all_local_variants`.
    /// The accept list and the reject list must stay byte-identical to
    /// the bridge crate's so a future audit can confirm both sides agree
    /// on what counts as "local".
    #[test]
    fn loopback_predicate_accepts_all_local_variants() {
        for url in [
            "http://127.0.0.1",
            "http://127.0.0.1/",
            "http://127.0.0.1:5678",
            "http://127.0.0.1:5678/path",
            "https://localhost:9000/x?q=1#frag",
            "http://[::1]:8080/",
            "http://user:pw@localhost:1/",
        ] {
            assert!(is_loopback_url(url), "expected loopback for {url}");
        }
        for url in [
            "http://example.com",
            "https://10.0.0.1",
            "ftp://localhost/",
            "file:///etc/hosts",
            "http://127.0.0.1.evil.com/",
            "//localhost/",
            "",
        ] {
            assert!(!is_loopback_url(url), "expected NOT loopback for {url}");
        }
    }

    /// Defense-in-depth: `host:port` with the host being something that
    /// merely *starts* with `127.0.0.1` must NOT be accepted.  The
    /// bridge crate has the same expectation; the spoof attack here is
    /// `localhost.attacker.com:443` — the trailing domain is what an
    /// attacker uses to phish a loopback rule.
    #[test]
    fn spoofed_localhost_suffix_is_rejected() {
        for spoof in [
            "http://localhost.attacker.com/",
            "http://localhost.attacker.com:8080/",
            "http://127.0.0.1.example.com/",
            "https://127.0.0.1.example.com:443/",
        ] {
            assert!(
                !is_loopback_url(spoof),
                "spoofed loopback {spoof} must be rejected"
            );
        }
    }

}

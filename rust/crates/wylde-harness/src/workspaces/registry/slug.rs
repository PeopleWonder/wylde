//! Stable, filename-safe id derivation for a workspace folder.
//!
//! Moved into the redesign module from the retired
//! `crate::memory::workspaces::slug` (the strangler-fig's old half).
//! Output shape: `<sanitized-basename>-<sha256[..6]>`. The 6-hex suffix
//! protects against `/foo/bar` vs `/baz/bar` collisions; the basename
//! keeps the id human-readable in `<data_dir>/workspaces/<slug>/`.
//!
//! Behaviour is byte-for-byte the proven legacy implementation so a
//! folder keeps the same id across the cutover.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Derive a stable, filename-safe id from a folder path.
///
/// Output is `<sanitized-basename>-<sha256[..6]>` (resolved absolute
/// path → sha256 → first 6 hex chars; basename sanitized to
/// `[A-Za-z0-9_-]+`, truncated to 40 chars, fallback `"workspace"` if
/// empty).
pub fn slug_for(path: &str) -> String {
    let abs = resolve(path);
    let digest = sha256_first6_hex(abs.to_string_lossy().as_ref());

    let base_raw = abs
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let base = sanitize(&base_raw);
    let truncated: String = base.chars().take(40).collect();
    let final_base = if truncated.is_empty() {
        "workspace".to_owned()
    } else {
        truncated
    };
    format!("{final_base}-{digest}")
}

/// Best-effort absolute-path resolution (mirrors Python's
/// `Path(path).expanduser().resolve()` with `strict=False`).
fn resolve(path: &str) -> PathBuf {
    let expanded = expand_tilde(path);
    let p = PathBuf::from(&expanded);

    if let Ok(can) = std::fs::canonicalize(&p) {
        return strip_verbatim_prefix(can);
    }

    let mut buf = if p.is_absolute() {
        p
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(&p)
    };
    buf = normalise(&buf);
    buf
}

fn expand_tilde(path: &str) -> String {
    if !path.starts_with('~') {
        return path.to_owned();
    }
    if path == "~" {
        if let Some(home) = home_dir() {
            return home.to_string_lossy().into_owned();
        }
        return path.to_owned();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    path.to_owned()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

fn strip_verbatim_prefix(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        return PathBuf::from(stripped);
    }
    p
}

/// Drop `.` and `..` segments lexically so the slug doesn't flap based
/// on the relative-path form the caller used.
fn normalise(p: &Path) -> PathBuf {
    let mut stack: Vec<std::ffi::OsString> = Vec::new();
    for comp in p.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if stack
                    .last()
                    .map(|s| s != std::ffi::OsStr::new(""))
                    .unwrap_or(false)
                {
                    stack.pop();
                }
            }
            other => stack.push(other.as_os_str().to_owned()),
        }
    }
    let mut out = PathBuf::new();
    for s in stack {
        out.push(s);
    }
    out
}

fn sha256_first6_hex(data: &str) -> String {
    let mut h = Sha256::new();
    h.update(data.as_bytes());
    let digest = h.finalize();
    let mut s = String::with_capacity(6);
    for b in &digest[..3] {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Mirrors Python's `re.sub(r"[^A-Za-z0-9_-]+", "_", base)`. Every run
/// of non-allowed characters collapses to a single underscore.
fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_run = false;
    for c in s.chars() {
        let allowed = c.is_ascii_alphanumeric() || c == '_' || c == '-';
        if allowed {
            out.push(c);
            in_run = false;
        } else if !in_run {
            out.push('_');
            in_run = true;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_for_returns_basename_dash_six_hex() {
        let s = slug_for("/tmp/some-project");
        assert!(s.starts_with("some-project-"), "got {s}");
        let suffix = s.rsplit_once('-').unwrap().1;
        assert_eq!(suffix.len(), 6);
        assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn slug_for_is_stable_and_distinct() {
        assert_eq!(slug_for("/tmp/proj"), slug_for("/tmp/proj"));
        assert_ne!(slug_for("/tmp/aaa/proj"), slug_for("/tmp/bbb/proj"));
    }

    #[test]
    fn sanitize_collapses_runs() {
        assert_eq!(sanitize("foo!!!bar"), "foo_bar");
        assert_eq!(sanitize("a b c"), "a_b_c");
        assert_eq!(sanitize("ok-name_1"), "ok-name_1");
    }
}

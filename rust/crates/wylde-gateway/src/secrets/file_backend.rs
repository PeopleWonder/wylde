//! File-based secrets backend — `.env` reader with OS-environ pass-through.
//!
//! Rust port of `Gateway/secrets/file_backend.py`. Reads `KEY=value`
//! lines from a `.env` file (and optional `.env.local` override). Returns
//! OS environment values for any key not in the file, so existing
//! env-var-driven config keeps working without changes.
//!
//! Format (intentionally tiny):
//! * `KEY=value` per line.
//! * Optional `export ` prefix.
//! * Optional surrounding single/double quotes.
//! * `#` for comments.
//! * No multi-line values, no variable substitution.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;

use super::SecretsProvider;

const DEFAULT_REL_PATH: &str = ".wylde/.env";

pub struct FileBackend {
    env_path: PathBuf,
    use_os_environ: bool,
    cache: RwLock<Option<HashMap<String, String>>>,
}

impl FileBackend {
    pub fn new(env_path: PathBuf, use_os_environ: bool) -> Self {
        Self {
            env_path,
            use_os_environ,
            cache: RwLock::new(None),
        }
    }

    /// Construct from `WYLDE_GATEWAY_ENV_FILE` (defaults to
    /// `~/.wylde/.env`). Mirrors Python's `FileBackend.default()`.
    pub fn default_paths() -> Self {
        let env_path = std::env::var_os("WYLDE_GATEWAY_ENV_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(default_env_path);
        Self::new(env_path, true)
    }

    fn ensure_loaded(&self) -> HashMap<String, String> {
        if let Some(cached) = self.cache.read().expect("file cache poisoned").as_ref() {
            return cached.clone();
        }
        let mut loaded: HashMap<String, String> = HashMap::new();
        for path in self.paths_to_load() {
            if !path.is_file() {
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(text) => loaded.extend(parse_env(&text)),
                Err(e) => {
                    tracing::warn!("file secrets: cannot read {}: {}", path.display(), e);
                }
            }
        }
        tracing::debug!(
            "file secrets: loaded {} keys from {}",
            loaded.len(),
            self.env_path.display()
        );
        let mut guard = self.cache.write().expect("file cache poisoned");
        *guard = Some(loaded.clone());
        loaded
    }

    fn paths_to_load(&self) -> Vec<PathBuf> {
        let mut out = vec![self.env_path.clone()];
        let mut local = self.env_path.as_os_str().to_owned();
        local.push(".local");
        out.push(PathBuf::from(local));
        out
    }
}

impl SecretsProvider for FileBackend {
    fn get(&self, key: &str, default: Option<&str>) -> Option<String> {
        if self.use_os_environ {
            if let Ok(v) = std::env::var(key) {
                return Some(v);
            }
        }
        let cache = self.ensure_loaded();
        cache
            .get(key)
            .cloned()
            .or_else(|| default.map(str::to_owned))
    }

    fn health_check(&self) -> bool {
        let _ = self.ensure_loaded();
        true
    }
}

fn default_env_path() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(DEFAULT_REL_PATH)
}

fn parse_env(text: &str) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let stripped = line.strip_prefix("export ").map(str::trim).unwrap_or(line);
        let (key, value) = match stripped.split_once('=') {
            Some(p) => p,
            None => continue,
        };
        let key = key.trim().to_owned();
        let value = value.trim();
        let value = unquote(value).to_owned();
        if !key.is_empty() {
            out.insert(key, value);
        }
    }
    out
}

fn unquote(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;

    static ENV_LOCK: StdMutex<()> = StdMutex::new(());

    #[test]
    fn parses_basic_key_value() {
        let m = parse_env("FOO=bar\nBAZ=qux\n");
        assert_eq!(m.get("FOO"), Some(&"bar".to_owned()));
        assert_eq!(m.get("BAZ"), Some(&"qux".to_owned()));
    }

    #[test]
    fn strips_export_prefix() {
        let m = parse_env("export FOO=bar\n");
        assert_eq!(m.get("FOO"), Some(&"bar".to_owned()));
    }

    #[test]
    fn strips_quotes() {
        let m = parse_env("A=\"hello world\"\nB='single'\n");
        assert_eq!(m.get("A"), Some(&"hello world".to_owned()));
        assert_eq!(m.get("B"), Some(&"single".to_owned()));
    }

    #[test]
    fn skips_comments_and_blank_lines() {
        let m = parse_env("# comment\n\nFOO=bar\n");
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn os_environ_takes_precedence_over_file() {
        let _g = ENV_LOCK.lock().expect("env lock");
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join(".env");
        std::fs::write(&p, "FROM_FILE_PRECEDENCE_TEST=from_file\n").unwrap();
        std::env::set_var("FROM_FILE_PRECEDENCE_TEST", "from_env");

        let b = FileBackend::new(p, true);
        assert_eq!(
            b.get("FROM_FILE_PRECEDENCE_TEST", None),
            Some("from_env".into())
        );
        std::env::remove_var("FROM_FILE_PRECEDENCE_TEST");
    }

    #[test]
    fn falls_back_to_file_when_env_missing() {
        let _g = ENV_LOCK.lock().expect("env lock");
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join(".env");
        std::fs::write(&p, "WYLDE_TEST_FILE_BACKEND_ONLY=value\n").unwrap();

        let b = FileBackend::new(p, true);
        assert_eq!(
            b.get("WYLDE_TEST_FILE_BACKEND_ONLY", None),
            Some("value".into())
        );
    }

    #[test]
    fn health_check_succeeds_with_missing_file() {
        let b = FileBackend::new(PathBuf::from("/nonexistent.env"), true);
        assert!(b.health_check());
    }
}

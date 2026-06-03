//! `search.*` tools — Rust port of `Core/harness/tooling/tools/search/`.
//!
//! Phase 6 ships pure-Rust implementations (no ripgrep dependency) for
//! `code_search` (regex over files) and `code_search_files` (glob over
//! filenames). Python's tools shell out to `rg` when available; the
//! Rust port uses `regex` + a hand-rolled walk that skips the standard
//! noise dirs (`.git`, `node_modules`, `__pycache__`, `dist`, `build`,
//! `venv`). Adding an `rg` fast path is on Phase 6's deferral list —
//! the slow path is fine for the LLM's typical query volume.

use std::path::{Path, PathBuf};

use regex::RegexBuilder;
use serde_json::{json, Value};
use tokio::fs as tokio_fs;
use wylde_shared::ipc::IpcError;

use crate::tooling::registry::{entry_active, param, param_default, Registry};

const NOISE_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "venv",
    ".venv",
    "__pycache__",
    "dist",
    "build",
    "target",
];

pub fn register(reg: &mut Registry) {
    reg.insert(entry_active(
        "code_search",
        "search.code_search",
        "search",
        "Search file contents for a regex pattern. Returns a list of \
         matches with path/line/text.",
        vec![
            param("pattern", "string", true, "Regex pattern to search for"),
            param_default("path", "string", "Directory to search", json!(".")),
            param_default("glob", "string", "Glob to filter files (e.g. '*.rs')", json!("")),
            param_default("case_insensitive", "boolean", "Case insensitive", json!(false)),
            param_default("max_count", "number", "Max matches to return", json!(500)),
        ],
        false,
        |args, _| async move { run_code_search(args).await },
    ));

    reg.insert(entry_active(
        "code_search_files",
        "search.code_search_files",
        "search",
        "Find files matching a glob (e.g. `*.py`, `src/**/*.tsx`). \
         Skips the usual noise dirs.",
        vec![
            param("glob", "string", true, "Glob pattern (e.g. '*.py')"),
            param_default("path", "string", "Directory to search", json!(".")),
            param_default("max_count", "number", "Max files to return", json!(500)),
        ],
        false,
        |args, _| async move { run_code_search_files(args).await },
    ));
}

pub(crate) async fn run_code_search(args: Value) -> Result<Value, IpcError> {
    let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
        return Ok(json!({"status": "error", "error": "'pattern' is required"}));
    };
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or(".")
        .to_owned();
    let glob = args
        .get("glob")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let case_insensitive = args
        .get("case_insensitive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let max_count = args
        .get("max_count")
        .and_then(Value::as_i64)
        .map(|n| n.max(1) as usize)
        .unwrap_or(500);

    let rx = match RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .build()
    {
        Ok(rx) => rx,
        Err(e) => {
            return Ok(json!({
                "status": "error",
                "error": format!("invalid regex: {e}"),
            }));
        }
    };

    let mut matches: Vec<Value> = Vec::new();
    let files = walk_files(Path::new(&path), &glob, usize::MAX).await;
    'outer: for file in files {
        let bytes = match tokio_fs::read(&file).await {
            Ok(b) => b,
            Err(_) => continue,
        };
        let text = String::from_utf8_lossy(&bytes);
        for (i, line) in text.lines().enumerate() {
            if rx.is_match(line) {
                matches.push(json!({
                    "path": file.display().to_string(),
                    "line": (i + 1) as i64,
                    "text": line,
                }));
                if matches.len() >= max_count {
                    break 'outer;
                }
            }
        }
    }

    Ok(json!({
        "status": "success",
        "pattern": pattern,
        "tool": "pure-rust",
        "matches": matches,
        "count": matches.len(),
    }))
}

pub(crate) async fn run_code_search_files(args: Value) -> Result<Value, IpcError> {
    let Some(glob) = args.get("glob").and_then(Value::as_str) else {
        return Ok(json!({"status": "error", "error": "'glob' is required"}));
    };
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or(".")
        .to_owned();
    let max_count = args
        .get("max_count")
        .and_then(Value::as_i64)
        .map(|n| n.max(1) as usize)
        .unwrap_or(500);

    let files = walk_files(Path::new(&path), glob, max_count).await;
    let payload: Vec<Value> = files
        .iter()
        .map(|p| json!({"path": p.display().to_string()}))
        .collect();
    Ok(json!({
        "status": "success",
        "glob": glob,
        "tool": "pure-rust",
        "files": payload,
        "count": payload.len(),
    }))
}

async fn walk_files(root: &Path, glob: &str, max_count: usize) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut rd = match tokio_fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            if NOISE_DIRS.iter().any(|n| *n == name) {
                continue;
            }
            let ft = match entry.file_type().await {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            let p = entry.path();
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() && (glob.is_empty() || glob_match(glob, &name)) {
                out.push(p);
                if out.len() >= max_count {
                    return out;
                }
            }
        }
    }
    out
}

/// Minimal glob matcher — `*` matches any run of non-slash chars, `?`
/// matches one non-slash char, `**` matches any depth. Matches against
/// the filename only (which is consistent with the Python fallback's
/// `fnmatch(fname, glob)` behaviour).
fn glob_match(pattern: &str, name: &str) -> bool {
    // Strip leading prefixes that point at directories so a glob like
    // `src/**/*.rs` still matches against `foo.rs`.
    let pat = pattern.rsplit('/').next().unwrap_or(pattern);
    let pat_chars: Vec<char> = pat.chars().collect();
    let name_chars: Vec<char> = name.chars().collect();
    glob_recurse(&pat_chars, 0, &name_chars, 0)
}

fn glob_recurse(pat: &[char], pi: usize, name: &[char], ni: usize) -> bool {
    if pi == pat.len() {
        return ni == name.len();
    }
    match pat[pi] {
        '*' => {
            // Match zero or more chars.
            for k in ni..=name.len() {
                if glob_recurse(pat, pi + 1, name, k) {
                    return true;
                }
            }
            false
        }
        '?' => {
            if ni < name.len() && glob_recurse(pat, pi + 1, name, ni + 1) {
                return true;
            }
            false
        }
        c => {
            if ni < name.len() && name[ni] == c {
                glob_recurse(pat, pi + 1, name, ni + 1)
            } else {
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn code_search_finds_pattern_with_line_numbers() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.txt");
        tokio_fs::write(&path, "alpha\nbeta\nalpha\n").await.unwrap();
        let v = run_code_search(json!({
            "pattern": "alpha",
            "path": dir.path().display().to_string(),
        }))
        .await
        .unwrap();
        assert_eq!(v["status"], "success");
        assert_eq!(v["count"], 2);
        let matches = v["matches"].as_array().unwrap();
        assert_eq!(matches[0]["line"], 1);
        assert_eq!(matches[1]["line"], 3);
    }

    #[tokio::test]
    async fn code_search_case_insensitive_flag_works() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.txt");
        tokio_fs::write(&path, "ALPHA\n").await.unwrap();
        let v = run_code_search(json!({
            "pattern": "alpha",
            "path": dir.path().display().to_string(),
            "case_insensitive": true,
        }))
        .await
        .unwrap();
        assert_eq!(v["count"], 1);
    }

    #[tokio::test]
    async fn code_search_invalid_regex_returns_error() {
        let v = run_code_search(json!({"pattern": "[invalid"})).await.unwrap();
        assert_eq!(v["status"], "error");
        assert!(v["error"].as_str().unwrap().contains("invalid regex"));
    }

    #[tokio::test]
    async fn code_search_skips_noise_dirs() {
        let dir = tempdir().unwrap();
        let noise = dir.path().join("node_modules");
        tokio_fs::create_dir(&noise).await.unwrap();
        tokio_fs::write(noise.join("a.txt"), "needle\n")
            .await
            .unwrap();
        tokio_fs::write(dir.path().join("b.txt"), "needle\n")
            .await
            .unwrap();
        let v = run_code_search(json!({
            "pattern": "needle",
            "path": dir.path().display().to_string(),
        }))
        .await
        .unwrap();
        assert_eq!(v["count"], 1);
    }

    #[tokio::test]
    async fn code_search_files_filters_by_glob() {
        let dir = tempdir().unwrap();
        tokio_fs::write(dir.path().join("a.rs"), "").await.unwrap();
        tokio_fs::write(dir.path().join("a.py"), "").await.unwrap();
        let v = run_code_search_files(json!({
            "glob": "*.rs",
            "path": dir.path().display().to_string(),
        }))
        .await
        .unwrap();
        assert_eq!(v["count"], 1);
        assert!(v["files"][0]["path"]
            .as_str()
            .unwrap()
            .ends_with("a.rs"));
    }

    #[test]
    fn glob_match_handles_star() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "main.py"));
    }

    #[test]
    fn glob_match_strips_directory_prefix() {
        assert!(glob_match("src/**/*.rs", "main.rs"));
    }

    #[test]
    fn glob_match_question_mark_matches_single_char() {
        assert!(glob_match("a?.txt", "ab.txt"));
        assert!(!glob_match("a?.txt", "abc.txt"));
    }
}

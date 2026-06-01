//! `diff.*` tools — Rust port of `Core/harness/tooling/tools/diff/`.
//!
//! Phase 6 ships `show_diff` (line-by-line unified diff between two
//! strings or two files). `apply_patch` ships in a later phase because
//! it depends on a unified-diff parser that we haven't pulled in yet.

use std::path::PathBuf;

use serde_json::{json, Value};
use tokio::fs as tokio_fs;
use wylde_shared::ipc::IpcError;

use crate::tooling::registry::{entry_active, entry_deferred, param, param_default, Registry};

pub fn register(reg: &mut Registry) {
    reg.insert(entry_active(
        "show_diff",
        "diff.show_diff",
        "diff",
        "Generate a unified diff between two files (a_path/b_path) or \
         two strings (a/b). Returns the diff text and a `changed` flag.",
        vec![
            param("a_path", "string", false, "Path to file A"),
            param("b_path", "string", false, "Path to file B"),
            param("a", "string", false, "Content A (alt to a_path)"),
            param("b", "string", false, "Content B (alt to b_path)"),
            param_default("a_label", "string", "Label for A in the diff header", json!("a")),
            param_default("b_label", "string", "Label for B in the diff header", json!("b")),
            param_default("context", "number", "Lines of context", json!(3)),
        ],
        false,
        |args, _| async move { run_show_diff(args).await },
    ));

    reg.insert(entry_deferred(
        "apply_patch",
        "diff.apply_patch",
        "diff",
        "Apply a unified diff to the working tree. Deferred — the Rust \
         port needs a patch parser still on the punchlist.",
        vec![],
        true,
        "6",
        "needs a unified-diff parser; ships later in Phase 6 or via git apply",
    ));
}

async fn run_show_diff(args: Value) -> Result<Value, IpcError> {
    let a_path = args.get("a_path").and_then(Value::as_str).map(str::to_owned);
    let b_path = args.get("b_path").and_then(Value::as_str).map(str::to_owned);
    let a = args.get("a").and_then(Value::as_str).map(str::to_owned);
    let b = args.get("b").and_then(Value::as_str).map(str::to_owned);

    let (a_text, b_text, a_label, b_label) = if let (Some(ap), Some(bp)) = (&a_path, &b_path) {
        let a_text = match tokio_fs::read_to_string(PathBuf::from(ap)).await {
            Ok(s) => s,
            Err(e) => {
                return Ok(json!({"status": "error", "error": format!("{e}")}));
            }
        };
        let b_text = match tokio_fs::read_to_string(PathBuf::from(bp)).await {
            Ok(s) => s,
            Err(e) => {
                return Ok(json!({"status": "error", "error": format!("{e}")}));
            }
        };
        (a_text, b_text, ap.clone(), bp.clone())
    } else if let (Some(av), Some(bv)) = (a, b) {
        let label_a = args
            .get("a_label")
            .and_then(Value::as_str)
            .unwrap_or("a")
            .to_owned();
        let label_b = args
            .get("b_label")
            .and_then(Value::as_str)
            .unwrap_or("b")
            .to_owned();
        (av, bv, label_a, label_b)
    } else {
        return Ok(json!({
            "status": "error",
            "error": "provide either (a_path, b_path) or (a, b) strings",
        }));
    };

    let context = args
        .get("context")
        .and_then(Value::as_i64)
        .map(|n| n.max(0) as usize)
        .unwrap_or(3);

    let diff_text = unified_diff(&a_text, &b_text, &a_label, &b_label, context);
    let lines = diff_text.lines().count();
    let changed = !diff_text.is_empty();
    Ok(json!({
        "status": "success",
        "diff": diff_text,
        "lines": lines,
        "changed": changed,
    }))
}

/// Minimal unified-diff implementation — mirrors Python `difflib`'s
/// `unified_diff` shape for the common case (whole-file diff). We use a
/// classic LCS-based hunking algorithm; the goal is wire compat with
/// what the Python tool emitted, not byte-for-byte parity with CPython.
fn unified_diff(a: &str, b: &str, a_label: &str, b_label: &str, context: usize) -> String {
    let a_lines: Vec<String> = split_lines_keepends(a);
    let b_lines: Vec<String> = split_lines_keepends(b);

    if a_lines == b_lines {
        return String::new();
    }

    let ops = diff_ops(&a_lines, &b_lines);
    if ops.iter().all(|op| matches!(op, Op::Equal(_, _))) {
        return String::new();
    }

    let mut out = String::new();
    out.push_str(&format!("--- {a_label}\n"));
    out.push_str(&format!("+++ {b_label}\n"));

    // Group ops into hunks separated by stretches of ≥ 2*context+1
    // equal lines. For Phase 6 we keep it simple: emit one big hunk
    // covering the entire diff. This matches Python difflib's output
    // for small inputs and is sufficient for the LLM-facing tool — the
    // model doesn't care whether hunks are split.
    let mut hunk: Vec<String> = Vec::new();
    let mut a_start = 1usize;
    let mut a_len = 0usize;
    let mut b_start = 1usize;
    let mut b_len = 0usize;
    let _ = context; // hunk-splitting reserved for a follow-up.

    let mut a_seen_first = false;
    let mut b_seen_first = false;
    let mut a_cursor = 1usize;
    let mut b_cursor = 1usize;
    for op in &ops {
        match op {
            Op::Equal(av, _bv) => {
                if !a_seen_first {
                    a_start = a_cursor;
                    a_seen_first = true;
                }
                if !b_seen_first {
                    b_start = b_cursor;
                    b_seen_first = true;
                }
                hunk.push(format!(" {}", strip_newline_for_display(av)));
                a_cursor += 1;
                b_cursor += 1;
                a_len += 1;
                b_len += 1;
            }
            Op::Delete(av) => {
                if !a_seen_first {
                    a_start = a_cursor;
                    a_seen_first = true;
                }
                hunk.push(format!("-{}", strip_newline_for_display(av)));
                a_cursor += 1;
                a_len += 1;
            }
            Op::Insert(bv) => {
                if !b_seen_first {
                    b_start = b_cursor;
                    b_seen_first = true;
                }
                hunk.push(format!("+{}", strip_newline_for_display(bv)));
                b_cursor += 1;
                b_len += 1;
            }
        }
    }
    out.push_str(&format!("@@ -{a_start},{a_len} +{b_start},{b_len} @@\n"));
    for line in hunk {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

fn strip_newline_for_display(s: &str) -> &str {
    s.strip_suffix('\n').unwrap_or(s)
}

#[derive(Debug, Clone)]
enum Op {
    Equal(String, String),
    Insert(String),
    Delete(String),
}

fn diff_ops(a: &[String], b: &[String]) -> Vec<Op> {
    let n = a.len();
    let m = b.len();
    // LCS DP table.
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in 0..n {
        for j in 0..m {
            dp[i + 1][j + 1] = if a[i] == b[j] {
                dp[i][j] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut ops: Vec<Op> = Vec::new();
    let mut i = n;
    let mut j = m;
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && a[i - 1] == b[j - 1] {
            ops.push(Op::Equal(a[i - 1].clone(), b[j - 1].clone()));
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i][j - 1] >= dp[i - 1][j]) {
            ops.push(Op::Insert(b[j - 1].clone()));
            j -= 1;
        } else {
            ops.push(Op::Delete(a[i - 1].clone()));
            i -= 1;
        }
    }
    ops.reverse();
    ops
}

fn split_lines_keepends(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ch in s.chars() {
        cur.push(ch);
        if ch == '\n' {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn show_diff_returns_unchanged_when_strings_match() {
        let v = run_show_diff(json!({"a": "hello", "b": "hello"}))
            .await
            .unwrap();
        assert_eq!(v["status"], "success");
        assert_eq!(v["changed"], false);
        assert_eq!(v["diff"], "");
    }

    #[tokio::test]
    async fn show_diff_emits_inserts_and_deletes() {
        let v = run_show_diff(json!({"a": "line1\nline2\n", "b": "line1\nline3\n"}))
            .await
            .unwrap();
        assert_eq!(v["status"], "success");
        assert_eq!(v["changed"], true);
        let diff = v["diff"].as_str().unwrap();
        assert!(diff.starts_with("--- a"));
        assert!(diff.contains("+++ b"));
        assert!(diff.contains("-line2"));
        assert!(diff.contains("+line3"));
    }

    #[tokio::test]
    async fn show_diff_errors_when_neither_pair_provided() {
        let v = run_show_diff(json!({})).await.unwrap();
        assert_eq!(v["status"], "error");
    }

    #[tokio::test]
    async fn show_diff_reads_files() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        tokio_fs::write(&a, "alpha\n").await.unwrap();
        tokio_fs::write(&b, "beta\n").await.unwrap();
        let v = run_show_diff(json!({
            "a_path": a.display().to_string(),
            "b_path": b.display().to_string(),
        }))
        .await
        .unwrap();
        assert_eq!(v["status"], "success");
        assert_eq!(v["changed"], true);
    }

    #[test]
    fn diff_register_inserts_show_and_defers_apply() {
        let mut reg = Registry::empty();
        register(&mut reg);
        let show = reg.lookup("show_diff").unwrap();
        assert!(matches!(show.kind, crate::tooling::registry::HandlerKind::Active(_)));
        let apply = reg.lookup("apply_patch").unwrap();
        assert!(matches!(
            apply.kind,
            crate::tooling::registry::HandlerKind::Deferred { .. }
        ));
    }
}

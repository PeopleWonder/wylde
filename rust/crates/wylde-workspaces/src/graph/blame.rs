//! Git-blame for the focal symbol — TBS Slice L ("extends
//! `workspaces.symbol_context` to include recent blame").
//!
//! [`blame_lines`] shells `git blame --line-porcelain -L <start>,<end>`
//! against the focal's file and aggregates the per-line output into
//! per-commit [`BlameEntry`] rows, newest-first — "who touched this
//! symbol's body, when, and why" as LLM-ready context.
//!
//! **Fail-soft by design** (OI-1 spirit): no git on PATH, not a repository,
//! an untracked file, or a bad line range all yield `None` — blame is
//! enrichment, never a reason for `symbol_context` to fail. The acceptance
//! ("git-blame appears in symbol_context for *tracked* files") is exactly
//! this asymmetry, pinned by a hermetic real-`git init` test below.
//!
//! The parse half ([`parse_line_porcelain`]) is pure and unit-tested
//! without git; `--line-porcelain` (not `--porcelain`) is used precisely
//! because it repeats every header for every line — no cross-line state to
//! carry while parsing.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

/// At most this many distinct commits in the reply ("recent blame", not the
/// file's whole history).
const MAX_ENTRIES: usize = 8;

/// Blame at most this many lines past `start` — a pathological body can't
/// turn one verb call into a whole-file blame.
const MAX_SPAN_LINES: u32 = 400;

/// One commit that touched the focal's body lines.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameEntry {
    /// Abbreviated commit hash (12 hex chars).
    pub commit: String,
    pub author: String,
    /// Author time, epoch seconds (the porcelain `author-time`).
    pub author_time: i64,
    /// The commit's summary line.
    pub summary: String,
    /// How many of the blamed lines this commit owns.
    pub lines: u32,
}

/// Blame `file[start..=end]` (1-based, inclusive). `None` when git/repo/
/// tracking is absent or the invocation fails — see the module docs.
pub fn blame_lines(file: &str, start: u32, end: u32) -> Option<Vec<BlameEntry>> {
    if file.is_empty() || start == 0 {
        return None;
    }
    let path = Path::new(file);
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty())?;
    let name = path.file_name()?;
    let end = end.max(start).min(start.saturating_add(MAX_SPAN_LINES));

    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("blame")
        .arg("--line-porcelain")
        .arg("-L")
        .arg(format!("{start},{end}"))
        .arg("--")
        .arg(name)
        .output()
        .ok()?;
    if !output.status.success() {
        // Not a repo / untracked / range past EOF — enrichment absent.
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let entries = parse_line_porcelain(&text);
    if entries.is_empty() {
        return None;
    }
    Some(entries)
}

/// Aggregate `git blame --line-porcelain` output into per-commit entries,
/// newest-first, capped at [`MAX_ENTRIES`]. Pure — testable without git.
pub fn parse_line_porcelain(text: &str) -> Vec<BlameEntry> {
    struct Acc {
        author: String,
        author_time: i64,
        summary: String,
        lines: u32,
    }
    let mut by_sha: BTreeMap<String, Acc> = BTreeMap::new();
    let mut current: Option<String> = None;

    for line in text.lines() {
        // A new blamed line opens with `<40-hex sha> <orig> <final> [count]`.
        let is_sha_line = line.len() >= 40
            && line.as_bytes()[..40].iter().all(u8::is_ascii_hexdigit)
            && line.as_bytes().get(40) == Some(&b' ');
        if is_sha_line {
            let sha = line[..40].to_owned();
            by_sha
                .entry(sha.clone())
                .or_insert(Acc {
                    author: String::new(),
                    author_time: 0,
                    summary: String::new(),
                    lines: 0,
                })
                .lines += 1;
            current = Some(sha);
            continue;
        }
        let Some(sha) = &current else { continue };
        let Some(acc) = by_sha.get_mut(sha) else {
            continue;
        };
        if let Some(rest) = line.strip_prefix("author ") {
            acc.author = rest.trim().to_owned();
        } else if let Some(rest) = line.strip_prefix("author-time ") {
            acc.author_time = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("summary ") {
            acc.summary = rest.trim().to_owned();
        }
        // `\t<content>` and the other headers are irrelevant here.
    }

    let mut out: Vec<BlameEntry> = by_sha
        .into_iter()
        .map(|(sha, a)| BlameEntry {
            commit: sha.chars().take(12).collect(),
            author: a.author,
            author_time: a.author_time,
            summary: a.summary,
            lines: a.lines,
        })
        .collect();
    // Newest-first; line count breaks ties so the dominant commit leads.
    out.sort_by(|a, b| {
        b.author_time
            .cmp(&a.author_time)
            .then(b.lines.cmp(&a.lines))
            .then(a.commit.cmp(&b.commit))
    });
    out.truncate(MAX_ENTRIES);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn porcelain_fixture() -> String {
        // Two lines from commit aaaa…, one from bbbb… (newer).
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        format!(
            "{a} 1 1 2\nauthor Alice\nauthor-time 100\nsummary first pass\n\tline one\n\
             {a} 2 2\nauthor Alice\nauthor-time 100\nsummary first pass\n\tline two\n\
             {b} 3 3 1\nauthor Bob\nauthor-time 200\nsummary fix bug\n\tline three\n"
        )
    }

    #[test]
    fn parse_aggregates_per_commit_newest_first() {
        let entries = parse_line_porcelain(&porcelain_fixture());
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].author, "Bob", "newest first");
        assert_eq!(entries[0].summary, "fix bug");
        assert_eq!(entries[0].lines, 1);
        assert_eq!(entries[0].commit, "b".repeat(12));
        assert_eq!(entries[1].author, "Alice");
        assert_eq!(entries[1].lines, 2);
        assert_eq!(entries[1].author_time, 100);
    }

    #[test]
    fn parse_tolerates_junk_and_empty_input() {
        assert!(parse_line_porcelain("").is_empty());
        assert!(parse_line_porcelain("not porcelain at all\n").is_empty());
    }

    /// The Slice L acceptance, hermetically: blame APPEARS for a tracked
    /// file in a real repository, and is ABSENT for an untracked one.
    /// Builds a throwaway repo with `git init` — git is a host requirement
    /// the workspaces service already assumes (it indexes git checkouts).
    #[test]
    fn git_blame_appears_for_tracked_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        run(&["init", "-q"]);
        run(&["config", "user.name", "Blame Tester"]);
        run(&["config", "user.email", "blame@test.local"]);

        let tracked = dir.path().join("tracked.rs");
        let mut f = std::fs::File::create(&tracked).unwrap();
        writeln!(f, "fn alpha() {{\n    1\n}}").unwrap();
        drop(f);
        run(&["add", "tracked.rs"]);
        run(&["commit", "-q", "-m", "add alpha"]);

        let entries =
            blame_lines(tracked.to_str().unwrap(), 1, 3).expect("blame appears for a tracked file");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].author, "Blame Tester");
        assert_eq!(entries[0].summary, "add alpha");
        assert_eq!(entries[0].lines, 3);
        assert!(entries[0].author_time > 0);

        // Untracked sibling → fail-soft None, not an error.
        let untracked = dir.path().join("untracked.rs");
        std::fs::write(&untracked, "fn beta() {}\n").unwrap();
        assert!(blame_lines(untracked.to_str().unwrap(), 1, 1).is_none());
        // Nonsense inputs → None.
        assert!(blame_lines("", 1, 1).is_none());
        assert!(blame_lines(tracked.to_str().unwrap(), 0, 1).is_none());
    }
}

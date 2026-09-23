//! Walk-time exclusion — the one shared predicate the full walk
//! ([`super::walk::walk_and_chunk`]) and the watcher's per-file pre-filter
//! ([`super::walk::is_indexable_path`]) both consult, so a `target-dev/doc`
//! rustdoc tree (the ~58 % build-artifact pollution R4 measured) never reaches
//! the index on either path.
//!
//! ## Why this exists (the root cause)
//!
//! The old prune was an exact-name dir set (`SKIP_DIR_NAMES` had `target` but
//! not `target-dev`) and had no `.gitignore` awareness, so the **dev** build
//! trees — gitignored only by *nested* `.gitignore`s (`rust/.gitignore`,
//! `Core/GUI/.gitignore`) the hand walker couldn't see — sailed straight in.
//!
//! ## Three layers, one precedence
//!
//! For a candidate path the matcher resolves, in order:
//!   1. **`.git/` is always excluded** — hard, nothing re-includes it.
//!   2. **`.wyldeignore`** (gitignore syntax, nested) — the per-workspace user
//!      override. A `!`-re-include here **wins over the deny-list** (the locked
//!      SOFT decision: user control beats the backstop); a plain ignore here
//!      excludes.
//!   3. **Built-in artifact deny-list** (layer b) — the backstop that holds
//!      even when a workspace has no `.gitignore` at all. Component-wise +
//!      prefix-agnostic (works on a `\\?\`-canonical path too), so it reliably
//!      catches `target` / `target-*` / `node_modules` / lockfiles / minified
//!      assets / dotfiles regardless of root-prefix quirks. This is the layer
//!      that makes the one-time purge robust.
//!   4. **`.gitignore`** (gitignore syntax, **nested** — the root-cause fix) —
//!      built per directory, deepest-first, so `rust/.gitignore`'s `target-dev/`
//!      excludes `rust/target-dev/...`.
//!
//! ## Cost
//!
//! Construction is O(1): the per-directory `.gitignore` / `.wyldeignore`
//! matchers are built **lazily** on first reach and cached on the instance, so
//! a single full walk reads each ignore file at most once. The full walk builds
//! one matcher and threads it; the watcher / file-tree pre-filter build a fresh
//! one per call (cheap, and always reads the *current* ignore files — no
//! staleness, no invalidation plumbing).

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use ignore_crate::gitignore::Gitignore;

/// Directory names the built-in deny-list always prunes (layer b). `target` /
/// `target-*` is handled by a glob check, not this set; `.git` is the layer-1
/// hard rule. These are conservative, well-known build/tooling artifact dirs.
const DENY_DIRS: &[&str] = &[
    "node_modules",
    "dist",
    "build",
    "out",
    ".next",
    ".svelte-kit",
    "__pycache__",
    "venv",
    ".venv",
    "env",
    ".env",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".idea",
    ".vscode",
    ".wylde",
    ".hg",
    ".svn",
];

/// Lockfiles the deny-list prunes by exact filename — large, generated, and
/// pure noise for retrieval.
const DENY_LOCKFILES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "poetry.lock",
    "uv.lock",
    "composer.lock",
    "Gemfile.lock",
];

/// Generated/minified file suffixes the deny-list prunes (layer b file globs).
/// Deliberately **not** `.html` — the rustdoc problem is a *directory* problem
/// (`target-*/doc`), covered by the `target-*` rule + gitignore; banning
/// `.html` would wrongly exclude a legitimate HTML-content workspace.
const DENY_FILE_SUFFIXES: &[&str] = &[".min.js", ".min.css", ".map"];

/// The shared exclusion predicate (see module docs). Built once per workspace
/// root via [`ExclusionMatcher::for_root`]; consult with [`is_excluded`].
///
/// [`is_excluded`]: ExclusionMatcher::is_excluded
pub struct ExclusionMatcher {
    /// Workspace root, normalised (no `\\?\` verbatim prefix). Paths are made
    /// relative to this for the deny-list scan and the gitignore ancestor walk.
    root: PathBuf,
    /// Per-directory `.gitignore` matchers, built lazily + cached. `None` marks
    /// a directory we've checked that has no `.gitignore` (so we don't re-stat).
    gi_cache: Mutex<HashMap<PathBuf, Option<Arc<Gitignore>>>>,
    /// Per-directory `.wyldeignore` matchers, same lazy-cache discipline.
    wi_cache: Mutex<HashMap<PathBuf, Option<Arc<Gitignore>>>>,
}

impl ExclusionMatcher {
    /// Build a matcher rooted at `root`. O(1) — the ignore-file matchers are
    /// built lazily on first reach.
    pub fn for_root(root: &Path) -> Self {
        Self {
            root: strip_verbatim(root),
            gi_cache: Mutex::new(HashMap::new()),
            wi_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Should `path` be excluded from the index? `is_dir` is whether the leaf is
    /// a directory (affects gitignore dir-only patterns on the leaf). See the
    /// module docs for the layered precedence.
    pub fn is_excluded(&self, path: &Path, is_dir: bool) -> bool {
        let np = strip_verbatim(path);
        let rel = rel_under(&np, &self.root);

        // Layer 1 — `.git/` is always out, before any override.
        if rel_components(&np, rel.as_deref()).any(|c| c == ".git") {
            return true;
        }

        // Layer 2 — `.wyldeignore` (user override). A re-include here wins over
        // the deny-list + gitignore; a plain ignore here excludes.
        match self.eval_ignore_stack(&self.wi_cache, ".wyldeignore", &np, is_dir) {
            Decision::Include => return false,
            Decision::Exclude => return true,
            Decision::Pass => {}
        }

        // Layer 3 — built-in artifact deny-list (the backstop).
        if deny_excluded(rel_components(&np, rel.as_deref())) {
            return true;
        }

        // Layer 4 — nested `.gitignore` (the root-cause fix).
        matches!(
            self.eval_ignore_stack(&self.gi_cache, ".gitignore", &np, is_dir),
            Decision::Exclude
        )
    }

    /// Resolve `path` against the per-directory ignore-file stack named
    /// `filename`, walking from the path's own directory up to (and including)
    /// the root, deepest-first — so a nested file overrides a shallower one.
    /// Returns the first non-`Pass` decision.
    fn eval_ignore_stack(
        &self,
        cache: &Mutex<HashMap<PathBuf, Option<Arc<Gitignore>>>>,
        filename: &str,
        np: &Path,
        is_dir: bool,
    ) -> Decision {
        // Dirs from the path's parent up to the root, deepest-first.
        let mut dir = np.parent();
        while let Some(d) = dir {
            // Only consult ignore files at or under the root.
            if !is_under_or_eq(d, &self.root) {
                break;
            }
            if let Some(gi) = self.ignore_for_dir(cache, filename, d) {
                match gi.matched_path_or_any_parents(np, is_dir) {
                    m if m.is_whitelist() => return Decision::Include,
                    m if m.is_ignore() => return Decision::Exclude,
                    _ => {}
                }
            }
            if path_eq(d, &self.root) {
                break;
            }
            dir = d.parent();
        }
        Decision::Pass
    }

    /// Get-or-build the `filename` matcher rooted at directory `dir`. Caches the
    /// absence (`None`) so a dir with no ignore file is stat'd once.
    fn ignore_for_dir(
        &self,
        cache: &Mutex<HashMap<PathBuf, Option<Arc<Gitignore>>>>,
        filename: &str,
        dir: &Path,
    ) -> Option<Arc<Gitignore>> {
        let mut guard = cache.lock().expect("exclude cache mutex");
        if let Some(hit) = guard.get(dir) {
            return hit.clone();
        }
        let file = dir.join(filename);
        let built = if file.is_file() {
            // `Gitignore::new` roots the matcher at the file's parent dir and
            // returns a partial-error for malformed lines (which we ignore —
            // a bad line just doesn't match).
            let (gi, _err) = Gitignore::new(&file);
            Some(Arc::new(gi))
        } else {
            None
        };
        guard.insert(dir.to_path_buf(), built.clone());
        built
    }
}

/// The outcome of one ignore-file-stack evaluation.
enum Decision {
    /// A `!`-re-include matched — explicitly keep.
    Include,
    /// An ignore pattern matched — exclude.
    Exclude,
    /// No pattern matched — defer to the next layer.
    Pass,
}

/// The built-in artifact deny-list (layer b). Pure + component-wise so it works
/// on any path form (plain or `\\?\`-canonical) without a successful root-strip.
fn deny_excluded<'a>(components: impl Iterator<Item = &'a str>) -> bool {
    let mut last = "";
    for c in components {
        last = c;
        // Any dotfile/dot-dir (the prior walk's blanket hidden-skip; `.git`
        // already handled as the layer-1 hard rule, `.gitignore`/`.wyldeignore`
        // are config we never index anyway). A `.wyldeignore` re-include can
        // still override this — it's checked before the deny-list.
        if c.starts_with('.') {
            return true;
        }
        // `target` / `target-*` — the dev-build-tree blind spot that bit us.
        if c == "target" || c.starts_with("target-") {
            return true;
        }
        if DENY_DIRS.contains(&c) {
            return true;
        }
    }
    if DENY_LOCKFILES.contains(&last) {
        return true;
    }
    let lower = last.to_ascii_lowercase();
    DENY_FILE_SUFFIXES.iter().any(|s| lower.ends_with(s))
}

/// The `Normal`-component names of `path`, relative to root when `rel` resolved,
/// else the full path's components (the deny-list still finds an artifact name
/// deep in an absolute path; only paths *above* root could yield a false name,
/// which in practice they don't).
fn rel_components<'a>(path: &'a Path, rel: Option<&'a Path>) -> impl Iterator<Item = &'a str> {
    rel.unwrap_or(path).components().filter_map(|c| match c {
        Component::Normal(os) => os.to_str(),
        _ => None,
    })
}

/// `np` relative to `root` (case-insensitive on Windows), or `None` when `np`
/// is not under `root`.
fn rel_under(np: &Path, root: &Path) -> Option<PathBuf> {
    if let Ok(rel) = np.strip_prefix(root) {
        return Some(rel.to_path_buf());
    }
    #[cfg(windows)]
    {
        // Windows paths are case-insensitive; canonicalised chunk paths and a
        // user-typed `folder` can differ in case. Strip case-insensitively.
        let nc: Vec<_> = np.components().collect();
        let rc: Vec<_> = root.components().collect();
        if nc.len() >= rc.len()
            && rc.iter().zip(&nc).all(|(a, b)| {
                a.as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
            })
        {
            return Some(nc[rc.len()..].iter().collect());
        }
    }
    None
}

/// Whether `dir` is at or under `root` (case-insensitive on Windows).
fn is_under_or_eq(dir: &Path, root: &Path) -> bool {
    path_eq(dir, root) || rel_under(dir, root).is_some()
}

/// Path equality, case-insensitive on Windows.
fn path_eq(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    {
        a.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        a == b
    }
}

/// Strip the Windows extended-length (`\\?\`) / verbatim-UNC prefix so a
/// canonicalised chunk path lines up with a plain workspace root. Identity off
/// Windows and on a path without the prefix.
fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    p.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn excl(root: &Path, rel: &str, is_dir: bool) -> bool {
        ExclusionMatcher::for_root(root).is_excluded(&root.join(rel), is_dir)
    }

    #[test]
    fn target_dev_doc_is_pruned_by_the_deny_glob() {
        // The headline: `target-dev` (NOT `target`) — the exact blind spot — is
        // excluded by the `target-*` deny rule, even with no .gitignore present.
        let td = tempdir().unwrap();
        let root = td.path();
        assert!(excl(root, "Core/GUI/target-dev/doc/settings.html", false));
        assert!(excl(root, "rust/target-dev/doc/src/x.rs.html", false));
        assert!(excl(root, "target", true));
        // Plain source survives.
        assert!(!excl(root, "rust/crates/foo/src/main.rs", false));
        assert!(!excl(root, "Core/GUI/Frontend/Panels/x.rs", false));
    }

    #[test]
    fn nested_gitignore_is_honored() {
        // A nested `.gitignore` the old exact-name walker couldn't see.
        let td = tempdir().unwrap();
        let root = td.path();
        let sub = root.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join(".gitignore"), "generated/\n").unwrap();
        assert!(excl(root, "sub/generated/g.rs", false));
        assert!(excl(root, "sub/generated", true));
        // A sibling not covered by the nested rule still indexes.
        assert!(!excl(root, "sub/keep.rs", false));
        // The rule is scoped to `sub/` — a same-named dir elsewhere is NOT hit
        // by this nested file (only by its own rules, of which there are none).
        assert!(!excl(root, "other/generated/g.rs", false));
    }

    #[test]
    fn deny_list_backstops_without_any_gitignore() {
        let td = tempdir().unwrap();
        let root = td.path();
        assert!(excl(root, "node_modules/dep/index.js", false));
        assert!(excl(root, "dist/bundle.js", false));
        assert!(excl(root, "app.min.js", false));
        assert!(excl(root, "styles.min.css", false));
        assert!(excl(root, "Cargo.lock", false));
        assert!(excl(root, "uv.lock", false));
        // A regular file with a lock-ish but non-lockfile name is fine.
        assert!(!excl(root, "src/locker.rs", false));
    }

    #[test]
    fn wyldeignore_reinclude_wins_over_deny_list() {
        // The SOFT decision: a `.wyldeignore` `!`-re-include overrides the
        // built-in deny-list (user control wins).
        let td = tempdir().unwrap();
        let root = td.path();
        // Without the override, `build/` is deny-listed.
        assert!(excl(root, "build/keep.rs", false));
        // Add a re-include and it now indexes — over the deny-list.
        fs::write(root.join(".wyldeignore"), "!build/\n").unwrap();
        assert!(!excl(root, "build/keep.rs", false));
        assert!(!excl(root, "build", true));
    }

    #[test]
    fn wyldeignore_can_also_exclude() {
        let td = tempdir().unwrap();
        let root = td.path();
        fs::write(root.join(".wyldeignore"), "secret/\n").unwrap();
        assert!(excl(root, "secret/s.rs", false));
        assert!(!excl(root, "public/p.rs", false));
    }

    #[test]
    fn git_dir_is_always_excluded_even_with_reinclude() {
        // `.git/` is the one always-skip; a `.wyldeignore !` cannot re-include
        // it.
        let td = tempdir().unwrap();
        let root = td.path();
        fs::write(root.join(".wyldeignore"), "!.git/\n").unwrap();
        assert!(excl(root, ".git/config", false));
        assert!(excl(root, ".git", true));
    }

    #[test]
    fn legit_html_outside_a_build_tree_indexes() {
        // We deliberately do NOT blanket-ban `.html` — a real HTML workspace
        // must still index.
        let td = tempdir().unwrap();
        let root = td.path();
        assert!(!excl(root, "site/index.html", false));
        assert!(!excl(root, "docs/guide.html", false));
    }

    #[test]
    fn dotfiles_and_dot_dirs_are_excluded_by_default() {
        // Preserves the prior walk's blanket hidden-skip.
        let td = tempdir().unwrap();
        let root = td.path();
        assert!(excl(root, ".env", false));
        assert!(excl(root, ".vscode/settings.json", false));
        assert!(excl(root, "src/.secret.rs", false));
        assert!(!excl(root, "src/visible.rs", false));
    }

    #[cfg(windows)]
    #[test]
    fn matches_a_canonical_verbatim_path() {
        // The purge feeds stored canonical (`\\?\`) paths — the deny-list must
        // still fire on them.
        let m = ExclusionMatcher::for_root(Path::new(r"C:\ws"));
        assert!(m.is_excluded(Path::new(r"\\?\C:\ws\rust\target-dev\doc\x.html"), false));
        assert!(!m.is_excluded(Path::new(r"\\?\C:\ws\rust\src\main.rs"), false));
    }
}

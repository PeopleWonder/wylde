"""No-personal-identifiers rule (rule 55).

This repo is public. Twice now a one-time hand scrub drove personal
identifiers to zero and then *drifted back*, because a hand-audited
number is a snapshot, not a guarantee:

* 2026-05-31 — ``docs/security/pre-alpha-release-2026-05-31.md`` recorded
  "``0`` remaining" for the maintainer's name and home-directory paths.
* 2026-07-19 — the tree held ~175 name occurrences across ~70 files and
  11 home-directory paths across 8 files, all reintroduced by ordinary
  commits in the intervening seven weeks.

Nothing failed in between, so nothing stopped it. This rule is the gate
that makes the guarantee enforceable.

Two independent checks:

**A. Home-directory paths.** Any ``C:\\Users\\<x>``, ``/home/<x>``, or
``/Users/<x>`` whose ``<x>`` is not a recognised placeholder. This needs
no knowledge of who the maintainer is — it catches *any* contributor
leaking *any* real home path, which is strictly broader than the drift
that actually happened.

**B. Maintainer name tokens.** Matched against **salted SHA-256 digests**,
never plaintext. Writing the literal name into the linter that exists to
remove that name would reintroduce it on every checkout and make the
repo's own tooling the top ``grep`` hit. The digests below identify a
token without disclosing it; adding one is
``python -c "import hashlib;print(hashlib.sha256((SALT+word).encode()).hexdigest())"``.

Not disclosure-proof and not meant to be — the names are already in this
repo's commit-author metadata, so a digest reveals nothing new. Its job
is to keep the *tree* clean without the linter itself becoming the leak.

Escape hatch: ``wylde-check: personal-identifier-ok`` on the flagged line
or the line above, for a genuine false positive (a vendored third-party
path, a quoted upstream error).
"""

from __future__ import annotations

import hashlib
import re
from typing import List

from .. import Finding
from .._walkers import _read_text, _to_rel, _walk

# ── Scope ────────────────────────────────────────────────────────────
# Repo-wide: the drift landed in .gitignore, CHANGELOG.md and docs/ as
# well as source, so this scans from the repo root rather than
# ACTIVE_ROOTS. Binary/vendored trees are already cut by EXCLUDED_DIRS.
_EXTENSIONS = (
    ".rs", ".py", ".md", ".toml", ".json", ".yaml", ".yml", ".ps1",
    ".sh", ".nsi", ".txt", ".cfg", ".ini", ".gitignore", ".env",
)

# This rule's own source and its test legitimately discuss the patterns.
_SELF_PATHS = frozenset({
    "Core/harness/dev/wylde_check/rules/_personal_identifiers.py",
    "Core/harness/dev/tests/wylde_check/test_personal_identifiers.py",
})

_OPT_OUT = "wylde-check: personal-identifier-ok"

# ── A. Home-directory paths ──────────────────────────────────────────

_HOME_PATH_RE = re.compile(
    r"(?:[A-Za-z]:[\\/]+Users[\\/]+|/home/|/Users/)([A-Za-z0-9._%<>$-]+)"
)

# Path segments that are placeholders, CI accounts, or system accounts —
# none of them identify a person.
_PLACEHOLDER_SEGMENTS = frozenset({
    "user", "users", "you", "your-name", "yourname", "username", "name",
    "wylde", "someone", "example", "test", "runner", "root", "admin",
    "administrator", "public", "default", "all", "me", "dev", "developer",
    "ci", "build", "actions", "github", "vagrant", "docker", "home",
    # GitHub-hosted runner accounts — these appear in real CI log paths.
    "runneradmin", "runner-admin", "githubactions",
    # Leading word of a multi-word placeholder like `C:\Users\the Wylde user`
    # (the capture stops at the space, so only "the" is ever seen here).
    "the",
})

# `/Users/` also appears mid-URL (e.g. a wiki path segment), which is not a
# filesystem home directory.
_URL_RE = re.compile(r"https?://\S+")


def _is_placeholder(seg: str) -> bool:
    """True when a path segment is generic rather than a real account."""
    low = seg.lower()
    # Angle/percent/dollar forms are explicitly templated: <user>, %USERNAME%, $HOME.
    if low.startswith(("<", "%", "$")) or low.endswith((">", "%")):
        return True
    # One- and two-character segments are stand-ins (`/home/x`, `C:\Users\X`),
    # never a real account worth protecting.
    if len(low) <= 2:
        return True
    return low in _PLACEHOLDER_SEGMENTS


# ── B. Maintainer name tokens (salted digests, never plaintext) ──────

_SALT = "wylde-check/personal-identifiers/v1"

_DENY_DIGESTS = frozenset({
    "13f4dcf842cc4e8bfd7e4d3ea5a74c8db09d78625a3a743e5f7757aaf6f12310",
    "8f6c9c71fe1671af9bd8f0f85e880d9403d75845977d7a6664b044121fe80c7e",
    "13d4c68df1266e8fe71574f77ada1d9126d6769843ecfdf285133c8ca3dcf1fe",
    "acecfd8553edc79fc4b5815a7bbd3dfb8c3d0c875fe5a6f1e8d89d1979bfc525",
})

# Alphabetic runs only; a personal name never needs digits or punctuation
# to be recognised, and this keeps the hashing cost proportional.
_TOKEN_RE = re.compile(r"[A-Za-z]{3,20}")


def _is_denied(token: str) -> bool:
    digest = hashlib.sha256((_SALT + token.lower()).encode("utf-8")).hexdigest()
    return digest in _DENY_DIGESTS


def check_no_personal_identifiers() -> List[Finding]:
    """Flag real home-directory paths and maintainer name tokens.

    Walks the repo read-only and reports (A) any ``C:\\Users\\<x>`` /
    ``/home/<x>`` / ``/Users/<x>`` whose segment is not a placeholder,
    and (B) any word whose salted digest is on the maintainer-name
    denylist. Both are errors: this is a public repo and both classes
    have silently regrown once already.
    """
    out: List[Finding] = []

    for path in _walk(_EXTENSIONS, roots=("",)):
        rel = _to_rel(path)
        if rel in _SELF_PATHS:
            continue
        text = _read_text(path)
        if not text:
            continue

        lines = text.splitlines()
        for lineno, raw in enumerate(lines, start=1):
            if _OPT_OUT in raw or (lineno >= 2 and _OPT_OUT in lines[lineno - 2]):
                continue

            # A — home-directory paths
            url_spans = [(u.start(), u.end()) for u in _URL_RE.finditer(raw)]
            for m in _HOME_PATH_RE.finditer(raw):
                seg = m.group(1)
                if _is_placeholder(seg):
                    continue
                # A `/Users/` inside a URL is a web path, not a home dir.
                if any(a <= m.start() < b for a, b in url_spans):
                    continue
                out.append(
                    Finding(
                        rule="no_personal_identifiers",
                        severity="error",
                        file=rel,
                        line=lineno,
                        message=(
                            "Real home-directory path in a public repo — the "
                            "segment after Users/home identifies an account. "
                            "Use a placeholder: `%USERPROFILE%`, `$HOME`, "
                            "`C:\\Users\\<you>\\`, or `<WYLDE_ROOT>`. If this is "
                            f"genuinely not personal, annotate with `{_OPT_OUT}`."
                        ),
                        # Neither the offending segment nor the source line is
                        # echoed: this rule's findings are printed by a CI job
                        # whose logs are public, so a message quoting the
                        # account name would re-disclose exactly what the rule
                        # exists to remove. file:line is enough to fix it.
                        context="",
                    )
                )

            # B — maintainer name tokens
            for m in _TOKEN_RE.finditer(raw):
                if not _is_denied(m.group(0)):
                    continue
                out.append(
                    Finding(
                        rule="no_personal_identifiers",
                        severity="error",
                        file=rel,
                        line=lineno,
                        message=(
                            "Maintainer's personal name in a public repo. "
                            "Use a role word ('the maintainer') in prose, or a "
                            "neutral sample name in test fixtures. If this is a "
                            f"false positive, annotate with `{_OPT_OUT}`."
                        ),
                        # Context deliberately omitted: echoing the line would
                        # put the name straight back into CI logs.
                        context="",
                    )
                )

    return out

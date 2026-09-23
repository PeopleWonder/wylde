"""Presence-gated pointers from a rule's message to a self-expiring tracker doc.

A tracker under ``docs/trackers/`` (see ``docs/trackers/README.md``) is DESIGNED to
vanish: untouched past its front-matter ``expires`` date, a scheduled job deletes it.
That makes any reference to one a liability — the useful kind of reference is the one
that disappears with its target instead of rotting into a broken path.

So a rule never hard-codes the path into its message. It appends ``tracker_pointer(slug)``,
which reads the filesystem and returns:

  * a pointer sentence, when the doc is there;
  * **the empty string**, when it is not.

The rule's output therefore degrades to silence rather than to a dangling link, and the
day a tracker expires NOTHING in the linter needs editing.

## What NOT to do

Do not add a tracker path to ``_selfcheck.RULE_TARGET_SPECS``. That registry means "this
rule silently passes if the path is missing", and rule 51 (``rule_targets_exist``) reds
the build when a listed path is gone. A tracker listed there would fail CI on the exact
day it was designed to disappear — turning a self-cleaning mechanism into a scheduled
outage. The omission is deliberate; this docstring is the record of why.

Cost: one ``Path.is_file()`` per finding, on a path already in the OS cache. Findings are
rare by construction (a green tree emits none), so it is not on any hot path.
"""

from __future__ import annotations

import sys as _sys
from pathlib import Path

# Top package object, so ``monkeypatch.setattr(wc, "WYLDE_ROOT", tmp_path)`` in the unit
# suite reaches the lookup below (the ``_selfcheck._pkg`` idiom).
_pkg = _sys.modules[__name__.rsplit(".", 2)[0]]

TRACKER_DIR = "docs/trackers"


def tracker_path(slug: str) -> str:
    """Repo-relative path of a tracker doc, whether or not it exists."""
    return f"{TRACKER_DIR}/{slug}.md"


def tracker_exists(slug: str) -> bool:
    root = getattr(_pkg, "WYLDE_ROOT", None)
    if root is None:
        return False
    return (Path(root) / tracker_path(slug)).is_file()


def tracker_pointer(slug: str, prefix: str = " Background: ") -> str:
    """A pointer sentence if the tracker is present, else ``""``.

    Appended to a ``Finding.message``. The empty-string branch is the whole point and is
    pinned by ``tests/wylde_check/test_tracker_ref.py`` — see the module docstring.
    """
    if not tracker_exists(slug):
        return ""
    return f"{prefix}{tracker_path(slug)} (a self-expiring tracker doc)."

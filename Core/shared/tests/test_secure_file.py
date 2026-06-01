"""Tests for ``Core.shared.secure_file`` — owner-only file hardening.

Platform-split: the POSIX mode-bits check is skipped on Windows and the
Windows ACL check is skipped on POSIX. The content-intact and
missing-path no-op checks run everywhere.
"""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

import pytest

from Core.shared.secure_file import harden_perms


def test_missing_path_is_noop(tmp_path: Path) -> None:
    """No file at the path — harden_perms must return, not raise."""
    harden_perms(tmp_path / "does-not-exist.json")


def test_content_intact_after_harden(tmp_path: Path) -> None:
    """Hardening must not corrupt or truncate the file body."""
    target = tmp_path / "state.json"
    payload = '{"secret": "value", "n": 42}'
    target.write_text(payload, encoding="utf-8")
    harden_perms(target)
    assert target.read_text(encoding="utf-8") == payload


def test_accepts_str_path(tmp_path: Path) -> None:
    """The helper accepts a plain str as well as a Path."""
    target = tmp_path / "state.json"
    target.write_text("x", encoding="utf-8")
    harden_perms(str(target))
    assert target.read_text(encoding="utf-8") == "x"


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX permission bits")
def test_posix_mode_is_0600(tmp_path: Path) -> None:
    """On POSIX the file ends up owner read/write only (0o600)."""
    target = tmp_path / "state.json"
    target.write_text("x", encoding="utf-8")
    harden_perms(target)
    assert (os.stat(target).st_mode & 0o777) == 0o600


@pytest.mark.skipif(sys.platform != "win32", reason="Windows ACL check")
def test_windows_acl_owner_only(tmp_path: Path) -> None:
    """On Windows the file has no inherited ACEs and no broad principals.

    After ``icacls /inheritance:r`` + ``/grant:r <user>:(F)`` the only
    surviving ACE belongs to the current user.
    """
    target = tmp_path / "state.json"
    target.write_text("x", encoding="utf-8")
    harden_perms(target)

    out = subprocess.run(
        ["icacls", str(target)],
        check=False,
        capture_output=True,
        text=True,
    ).stdout
    user = os.environ.get("USERNAME", "")
    assert user and user in out, f"owner ACE present:\n{out}"
    # /inheritance:r removes inherited ACEs (marked with the (I) flag).
    assert "(I)" not in out, f"no inherited ACEs:\n{out}"
    for broad in ("Everyone", "Authenticated Users", "BUILTIN\\Users"):
        assert broad not in out, f"no broad ACE {broad}:\n{out}"

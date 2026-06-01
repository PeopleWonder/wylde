"""Owner-only filesystem permission hardening.

Sensitive state files — device bearer tokens, password hashes,
conversation history — must not be left world-readable. :func:`harden_perms`
restricts a file to the current user: ``chmod 0o600`` on POSIX, and on
Windows a *protected* DACL carrying a single Full-Control ACE for the
current user (installing a protected DACL also strips inherited ACEs).

The Windows path calls the Win32 security API directly via ``pywin32``
— it does *not* shell out to ``icacls``. Subprocess spawning is reserved
for the Lifecycle daemon (wylde_check rule 14).

Fail-soft by design. A hardening failure logs a warning and returns
normally — it never raises — so it cannot break the atomic-rename write
path it is meant to run *after*.
"""

from __future__ import annotations

import logging
import os
import sys
from pathlib import Path

logger = logging.getLogger("wylde.shared.secure_file")


def harden_perms(path: str | Path) -> None:
    """Restrict a file to owner-only access.

    On POSIX: ``chmod 0o600``.
    On Windows: replace the DACL with a single Full-Control ACE for the
    current user and mark it protected, which also strips inherited ACEs.

    No-op gracefully if the file doesn't exist or the platform call
    fails — a warning is logged rather than raised.
    """
    p = Path(path)
    if not p.exists():
        logger.warning("secure_file: cannot harden missing path %s", p)
        return
    try:
        if sys.platform == "win32":
            _harden_windows(p)
        else:
            os.chmod(p, 0o600)
    except Exception as exc:  # noqa: BLE001 — hardening must never break a write.
        logger.warning("secure_file: failed to harden %s: %s", p, exc)


def _harden_windows(p: Path) -> None:
    """Windows ACL hardening via the Win32 security API (pywin32)."""
    import ntsecuritycon
    import win32api
    import win32security

    # The current user's SID, read from this process's access token.
    token = win32security.OpenProcessToken(
        win32api.GetCurrentProcess(), win32security.TOKEN_QUERY
    )
    user_sid = win32security.GetTokenInformation(token, win32security.TokenUser)[0]

    # A fresh DACL carrying exactly one ACE: the current user, full control.
    dacl = win32security.ACL()
    dacl.AddAccessAllowedAce(
        win32security.ACL_REVISION, ntsecuritycon.FILE_ALL_ACCESS, user_sid
    )

    # PROTECTED_DACL_SECURITY_INFORMATION drops inherited ACEs; the new
    # DACL then becomes the file's complete, non-inheriting permission set.
    win32security.SetNamedSecurityInfo(
        str(p),
        win32security.SE_FILE_OBJECT,
        win32security.DACL_SECURITY_INFORMATION
        | win32security.PROTECTED_DACL_SECURITY_INFORMATION,
        None,
        None,
        dacl,
        None,
    )


__all__ = ["harden_perms"]

"""htpasswd-based username/password validation.

The pairing flow validates the desktop user's credentials against the
existing ``htpasswd`` file (kept from the legacy nginx auth_request
flow). Mobile sends ``{username, password}`` at pairing time; we
verify it here, then issue the device a token.

Hash schemes we accept: APR1 (the format ``htpasswd -m`` writes),
bcrypt (``$2*$``), SHA512-crypt (``$6$``), SHA256-crypt (``$5$``), and
legacy DES (``crypt(3)``-style 13-char hashes). ``passlib`` is a hard
dependency — stdlib ``crypt`` was removed in Python 3.13, so passlib
is the only portable verifier across the schemes htpasswd files can
contain.
"""

from __future__ import annotations

import logging
from pathlib import Path
from typing import Optional

from passlib.context import CryptContext

logger = logging.getLogger("wylde.device_gate.auth")


# One context, reused across calls. Schemes match the historical
# capability surface — htpasswd -m writes apr_md5_crypt by default;
# bcrypt / sha512_crypt / sha256_crypt / des_crypt cover entries
# produced by other htpasswd-compatible tools.
_CTX = CryptContext(
    schemes=[
        "apr_md5_crypt",
        "bcrypt",
        "sha512_crypt",
        "sha256_crypt",
        "des_crypt",
    ]
)


def _read_hash(htpasswd_path: Path, username: str) -> Optional[str]:
    """Return the stored hash for ``username``, or None if the file
    is missing / unreadable / has no matching line."""
    if not htpasswd_path.exists():
        return None
    try:
        data = htpasswd_path.read_text(encoding="utf-8")
    except Exception as exc:  # noqa: BLE001
        logger.warning(
            "device_gate.auth: htpasswd unreadable (%s): %s", htpasswd_path, exc
        )
        return None
    for raw_line in data.splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        if ":" not in line:
            continue
        user, _, hashed = line.partition(":")
        if user == username:
            return hashed.strip()
    return None


def _verify_hash(stored_hash: str, password: str) -> bool:
    """Constant-time check of ``password`` against ``stored_hash``."""
    if not isinstance(stored_hash, str) or not stored_hash:
        return False
    try:
        return bool(_CTX.verify(password, stored_hash))
    except Exception:  # noqa: BLE001
        # Unsupported scheme prefix, malformed hash, etc. — fail closed.
        return False


def verify_credentials(htpasswd_path: Path, username: str, password: str) -> bool:
    """Constant-time-ish credential check against the htpasswd file.

    Returns False on any failure path: missing file, unknown user,
    wrong password, or unsupported hash format. Always exercises a
    hash check (even on missing user) so timing leaks are blunted.
    """
    # A throwaway apr1 hash used to keep the timing of the missing-user
    # and unknown-user paths roughly equivalent to the happy path.
    _DUMMY = "$apr1$xxxxxxxx$0000000000000000000000"

    if not isinstance(username, str) or not username:
        _verify_hash(_DUMMY, password or "")
        return False
    if not isinstance(password, str) or not password:
        return False
    stored = _read_hash(htpasswd_path, username)
    if stored is None:
        _verify_hash(_DUMMY, password)
        return False
    return _verify_hash(stored, password)


__all__ = ["verify_credentials"]

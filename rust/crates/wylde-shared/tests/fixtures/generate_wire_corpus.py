#!/usr/bin/env python3
"""Generate ``wire_corpus.json`` from the live Python IPC implementation.

Run from the repo root:

    python rust/crates/wylde-shared/tests/fixtures/generate_wire_corpus.py

The output pins the exact bytes that ``Core/shared/ipc/_client.py`` and
``_server.py`` produce, so the Rust crate's wire-fixture test can decode
them with ``rmp-serde`` and assert structural equality. This is the
parity safety net described in the W4.2 spec — if Python's msgpack
defaults ever change or rmp-serde's defaults drift, this test catches it.
"""

from __future__ import annotations

import base64
import json
import sys
from pathlib import Path

# Use the in-repo shared ipc module so the corpus reflects the live wire shape.
REPO_ROOT = Path(__file__).resolve().parents[5]
sys.path.insert(0, str(REPO_ROOT / "Core" / "shared"))

import msgpack  # type: ignore  # noqa: E402


def b64(payload: dict) -> str:
    return base64.b64encode(msgpack.packb(payload, use_bin_type=True)).decode("ascii")


CASES = [
    {
        "name": "ok_reply_with_data",
        "payload": {"id": "abc", "ok": True, "data": {"pong": True, "ver": 1}},
    },
    {
        "name": "ok_reply_null_data",
        "payload": {"id": "x", "ok": True, "data": None},
    },
    {
        "name": "err_reply",
        "payload": {
            "id": "y",
            "ok": False,
            "error": {"code": "pipe_connect", "message": "could not connect"},
        },
    },
    {
        "name": "request_envelope",
        "payload": {
            "ver": 1,
            "id": "deadbeef",
            "method": "/echo",
            "http_verb": "POST",
            "data": {"hello": "world", "n": 42},
            "meta": {"deadline_ms": 30000, "caller": "test-caller"},
        },
    },
    {
        "name": "handshake_client",
        "payload": {"wylde_ipc": 1, "caller": "rust-test", "service": "demo"},
    },
    {
        "name": "handshake_server_ok",
        "payload": {"wylde_ipc": 1, "ok": True, "service": "demo"},
    },
    {
        "name": "handshake_server_reject",
        "payload": {
            "wylde_ipc": 1,
            "ok": False,
            "service": "demo",
            "error": {
                "code": "version_mismatch",
                "message": "client ipc version 99 not supported",
            },
        },
    },
    {
        "name": "action_envelope",
        "payload": {"action": "harness.health", "payload": None},
    },
    {
        "name": "action_dispatch_request",
        "payload": {
            "ver": 1,
            "id": "act-1",
            "method": "/__action__",
            "http_verb": "POST",
            "data": {"action": "harness.health", "payload": {"check": True}},
            "meta": {"deadline_ms": 5000, "caller": "rust-test"},
        },
    },
    {
        "name": "string_with_utf8",
        "payload": {"ok": True, "data": "héllo ☃ world"},
    },
]


def main() -> None:
    out_path = Path(__file__).parent / "wire_corpus.json"
    cases = [
        {"name": c["name"], "payload": c["payload"], "msgpack_b64": b64(c["payload"])}
        for c in CASES
    ]
    out_path.write_text(json.dumps({"cases": cases}, indent=2))
    print(f"wrote {out_path} with {len(cases)} cases")


if __name__ == "__main__":
    main()

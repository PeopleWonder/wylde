"""Tests for core/shared/ipc.py.

Focus is the wire protocol: frame encoding/decoding, handshake, and timeout
behavior. The pipe server itself needs Windows + pywin32 and is exercised
end-to-end by scripts/smoke_test_ipc.py; here we test the parts that are
platform-independent and the bits we can stub.
"""

from __future__ import annotations

import time
from pathlib import Path

import msgpack
import pytest

from Core.shared import ipc


# ── _size / envelope helpers ──────────────────────────────────────────
class TestSize:
    def test_none(self) -> None:
        assert ipc._size(None) == 0

    def test_bytes(self) -> None:
        assert ipc._size(b"abc") == 3

    def test_str_utf8(self) -> None:
        assert ipc._size("héllo") == len("héllo".encode())

    def test_dict_counts_json(self) -> None:
        # Just be sane, not exact — we only care that non-empty structures
        # produce non-zero size.
        assert ipc._size({"a": 1, "b": [1, 2, 3]}) > 5


# ── Frame encoding (length-prefix + msgpack) ──────────────────────────
class TestFraming:
    """The wire format is a 4-byte big-endian length + msgpack body.
    Verify we can round-trip both halves with the same primitives the
    pipe handler uses."""

    def test_length_prefix_roundtrip(self) -> None:
        payload = msgpack.packb({"method": "/health", "data": None}, use_bin_type=True)
        header = len(payload).to_bytes(4, "big")
        frame = header + payload
        # Decode back
        n = int.from_bytes(frame[:4], "big")
        assert n == len(payload)
        decoded = msgpack.unpackb(frame[4 : 4 + n], raw=False)
        assert decoded["method"] == "/health"

    def test_rejects_zero_length(self) -> None:
        # `read_frame` checks 0 < n <= 64MiB. A zero-length frame is bogus.
        assert int.from_bytes(b"\x00\x00\x00\x00", "big") == 0

    def test_rejects_oversize(self) -> None:
        # The cap is 64 MiB. Anything larger is treated as a desync and
        # the connection is torn down rather than allocating arbitrary RAM.
        bogus = (128 * 1024 * 1024).to_bytes(4, "big")
        assert int.from_bytes(bogus, "big") > 64 * 1024 * 1024


# ── Handshake envelope ────────────────────────────────────────────────
class TestHandshake:
    def test_client_handshake_carries_version(self) -> None:
        frame = msgpack.packb(
            {
                "wylde_ipc": ipc.IPC_VERSION,
                "caller": "test",
                "service": "smoke",
            },
            use_bin_type=True,
        )
        decoded = msgpack.unpackb(frame, raw=False)
        assert decoded["wylde_ipc"] == ipc.IPC_VERSION
        assert decoded["caller"] == "test"

    def test_server_handshake_reply_shape(self) -> None:
        # The server replies with {wylde_ipc, ok, service} on accept.
        reply = msgpack.packb(
            {
                "wylde_ipc": ipc.IPC_VERSION,
                "ok": True,
                "service": "smoke",
            },
            use_bin_type=True,
        )
        decoded = msgpack.unpackb(reply, raw=False)
        assert decoded["ok"] is True
        assert decoded["wylde_ipc"] == ipc.IPC_VERSION

    def test_version_mismatch_reply_shape(self) -> None:
        # v0 or v>current gets a structured error the client can surface.
        reply = msgpack.packb(
            {
                "id": "",
                "ok": False,
                "error": {
                    "code": "version_mismatch",
                    "message": "client ipc version 999 not supported",
                },
            },
            use_bin_type=True,
        )
        decoded = msgpack.unpackb(reply, raw=False)
        assert decoded["ok"] is False
        assert decoded["error"]["code"] == "version_mismatch"

    def test_pre_v1_request_has_no_wylde_ipc_key(self) -> None:
        # Backward-compat path: a v0 client sends a plain request as the
        # first frame. The server must accept it instead of dropping.
        req = msgpack.packb(
            {
                "id": "abc",
                "method": "/echo",
                "data": {"x": 1},
            },
            use_bin_type=True,
        )
        decoded = msgpack.unpackb(req, raw=False)
        assert "wylde_ipc" not in decoded
        assert decoded["method"] == "/echo"


# ── Reply + IpcError ──────────────────────────────────────────────────
class TestReply:
    def test_ok_reply_passes_through(self) -> None:
        r = ipc.Reply(ok=True, data={"x": 1}, transport="pipe")
        assert r.raise_for_error().data == {"x": 1}

    def test_error_reply_raises_ipcerror(self) -> None:
        r = ipc.Reply(
            ok=False,
            error={"code": "not_found", "message": "no such service"},
            transport="pipe",
        )
        with pytest.raises(ipc.IpcError) as exc_info:
            r.raise_for_error()
        assert exc_info.value.code == "not_found"
        assert "no such service" in str(exc_info.value)

    def test_ipcerror_without_details(self) -> None:
        err = ipc.IpcError("timeout", "took too long")
        assert err.details == {}
        assert err.message == "took too long"

    def test_reply_defaults(self) -> None:
        r = ipc.Reply(ok=True)
        assert r.data is None
        assert r.error is None
        assert r.transport == ""


# ── _resolve / backend selection (no pipe actually opened) ────────────
class TestResolve:
    def test_resolve_returns_pipe_only_in_pipe_mode(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # In the default pipe+pywin32 path, _resolve short-circuits to a
        # synthesized pipe-only instance without touching discovery.
        monkeypatch.setattr(ipc._wire, "_TRANSPORT", "pipe")
        monkeypatch.setattr(ipc._wire, "_HAS_WIN32", True)
        monkeypatch.setattr(ipc._wire, "_HAS_MSGPACK", True)
        monkeypatch.setattr(ipc._wire, "IPC_DISABLE", False)

        inst = ipc._resolve("whatever")
        assert inst is not None
        assert inst.pipe_only is True
        assert inst.supports_pipe is True

    def test_pick_backend_http_when_disabled(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setattr(ipc._wire, "IPC_DISABLE", True)
        inst = ipc._Instance(
            address="127.0.0.1", port=1234, tags=["ipc=pipe"], pipe_only=True
        )
        assert ipc._pick_backend(inst) == "http"

    def test_pick_backend_pipe_when_supported(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setattr(ipc._wire, "IPC_DISABLE", False)
        monkeypatch.setattr(ipc._wire, "_TRANSPORT", "pipe")
        monkeypatch.setattr(ipc._wire, "_HAS_WIN32", True)
        monkeypatch.setattr(ipc._wire, "_HAS_MSGPACK", True)
        # Clear the negative cache so the picker can return "pipe"
        ipc._pipe_negcache.clear()
        inst = ipc._Instance(
            address="127.0.0.1", port=0, tags=["ipc=pipe"], pipe_only=True
        )
        assert ipc._pick_backend(inst) == "pipe"

    def test_pick_backend_http_when_instance_lacks_pipe(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setattr(ipc._wire, "_TRANSPORT", "pipe")
        monkeypatch.setattr(ipc._wire, "_HAS_WIN32", True)
        monkeypatch.setattr(ipc._wire, "_HAS_MSGPACK", True)
        inst = ipc._Instance(address="127.0.0.1", port=8080, tags=[])
        assert ipc._pick_backend(inst) == "http"


# ── Pipe negative cache ───────────────────────────────────────────────
class TestNegCache:
    def setup_method(self) -> None:
        ipc._pipe_negcache.clear()

    def test_mark_and_check_dead(self) -> None:
        ipc._mark_pipe_dead("svc")
        assert ipc._is_pipe_dead("svc") is True

    def test_expires_after_ttl(self, monkeypatch: pytest.MonkeyPatch) -> None:
        ipc._mark_pipe_dead("svc")
        # Fast-forward past PIPE_NEGCACHE_SECONDS
        future = time.time() + ipc.PIPE_NEGCACHE_SECONDS + 1
        monkeypatch.setattr(time, "time", lambda: future)
        assert ipc._is_pipe_dead("svc") is False

    def test_unknown_service_not_dead(self) -> None:
        assert ipc._is_pipe_dead("never-seen") is False


# ── send() with service not resolvable ────────────────────────────────
class TestSendNotFound:
    def test_returns_not_found_when_resolve_is_none(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Force _resolve to return None (e.g. http mode with no discovery).
        # Patch on the _client submodule because that's where send() looks
        # up _resolve at call time after the ipc package split.
        monkeypatch.setattr(ipc._client, "_resolve", lambda _svc: None)
        reply = ipc.send("ghost-service", "/health")
        assert not reply.ok
        assert reply.error is not None
        assert reply.error["code"] == "not_found"
        assert reply.transport == "none"


# ── _log_call + _size don't explode on weird inputs ──────────────────
class TestLogCall:
    def test_logs_reply_without_crashing(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # LOG_PATH lives on _wire; _log_call reads it via _w.LOG_PATH so the
        # patch must land on the same module to be visible. _log_file is the
        # module-global handle in _observability — same reason.
        monkeypatch.setattr(ipc._wire, "LOG_PATH", tmp_path / "ipc.jsonl")
        # Force a fresh open on the next call
        ipc._observability._log_file = None
        r = ipc.Reply(ok=True, data={"x": 1}, transport="pipe", duration_ms=1.23)
        ipc._log_call("svc", "/m", r, bytes_in=2, bytes_out=3)
        # Close so the test doesn't leak a handle
        if ipc._observability._log_file is not None:
            ipc._observability._log_file.close()
            ipc._observability._log_file = None
        assert (tmp_path / "ipc.jsonl").exists()
        contents = (tmp_path / "ipc.jsonl").read_text(encoding="utf-8")
        assert '"ok": true' in contents
        assert '"callee": "svc"' in contents


# ── PipeTimeout inheritance ──────────────────────────────────────────
class TestPipeTimeout:
    def test_is_ioerror(self) -> None:
        # Existing `except (pywintypes.error, IOError, OSError)` handlers
        # must continue to catch timeouts.
        err = ipc.PipeTimeout("slow peer")
        assert isinstance(err, IOError)


# ── Instance property sanity ─────────────────────────────────────────
class TestInstance:
    def test_supports_pipe_via_tag(self) -> None:
        inst = ipc._Instance(address="127.0.0.1", port=0, tags=["ipc=pipe"])
        assert inst.supports_pipe is True

    def test_supports_pipe_via_meta(self) -> None:
        inst = ipc._Instance(address="127.0.0.1", port=0, meta={"ipc": "pipe"})
        assert inst.supports_pipe is True

    def test_supports_pipe_via_pipe_only_flag(self) -> None:
        inst = ipc._Instance(address="x", port=0, pipe_only=True)
        assert inst.supports_pipe is True

    def test_url_composition(self) -> None:
        inst = ipc._Instance(address="127.0.0.1", port=8080)
        assert inst.url == "http://127.0.0.1:8080"

    def test_http_only_instance_has_no_pipe(self) -> None:
        inst = ipc._Instance(address="127.0.0.1", port=8080, tags=[])
        assert inst.supports_pipe is False

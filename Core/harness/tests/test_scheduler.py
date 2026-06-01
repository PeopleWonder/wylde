"""Smoke for the reflection + curation scheduler.

Time is fully injectable: tests advance the clock with a closure and
call ``tick()`` directly instead of starting a thread + sleeping.
That makes cadence assertions deterministic and the suite fast.

The chat_fn is synthetic — emits canned reflections / verdicts so we
don't depend on Ollama. Coverage:

* No chat_fn ⇒ ``start()`` returns False, scheduler runs in
  "skipped-only" mode (caller can still drive ``reflect`` / ``curate``
  directly via Python).
* Long-term reflection fires on first tick (no prior state) and not
  again before its cadence elapses.
* Workspace reflection + curation each fire per-workspace at their
  own cadences.
* Conversation reflection fires only after the conversation has been
  idle for the configured window.
* State persists across scheduler restart — same instance reads its
  prior fires from disk and skips re-firing.
"""

from __future__ import annotations

from typing import Any

import importlib
import sys
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve()
_VAULT_ROOT = _HERE.parents[4]
if str(_VAULT_ROOT) not in sys.path:
    sys.path.insert(0, str(_VAULT_ROOT))


@pytest.fixture
def scheduler_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Any:
    """Tmp DATA_DIR + reload the memory modules so they pick up the
    fresh paths. Returns the scheduler module + a clock-controlled
    helper for tests to advance time."""
    data_dir = tmp_path / "data"
    monkeypatch.setenv("WYLDE_DATA_DIR", str(data_dir))
    monkeypatch.setenv("CONVERSATIONS_DIR", str(data_dir / "conversations"))

    import importlib as _importlib

    try:
        _common = _importlib.import_module("Core.harness.memory._common")
        embeddings = _importlib.import_module("Core.harness.memory.embeddings")
        conversation = _importlib.import_module("Core.harness.memory.conversation")
        workspaces = _importlib.import_module("Core.harness.memory.workspaces")
        workspace_memory = _importlib.import_module(
            "Core.harness.memory.workspace_memory"
        )
        long_term = _importlib.import_module("Core.harness.memory.long_term")
        reflection = _importlib.import_module("Core.harness.memory.reflection")
        scheduler = _importlib.import_module("Core.harness.memory.scheduler")
    except ImportError:
        _common = _importlib.import_module("Wylde.Core.harness.memory._common")
        embeddings = _importlib.import_module("Wylde.Core.harness.memory.embeddings")
        conversation = _importlib.import_module(
            "Wylde.Core.harness.memory.conversation"
        )
        workspaces = _importlib.import_module("Wylde.Core.harness.memory.workspaces")
        workspace_memory = _importlib.import_module(
            "Wylde.Core.harness.memory.workspace_memory"
        )
        long_term = _importlib.import_module("Wylde.Core.harness.memory.long_term")
        reflection = _importlib.import_module("Wylde.Core.harness.memory.reflection")
        scheduler = _importlib.import_module("Wylde.Core.harness.memory.scheduler")
    # Reload _common and embeddings first so DATA_DIR reflects the env
    # vars set above. Then reload subpackage submodules so their
    # module-level path constants (REGISTRY_PATH, SETTINGS_PATH,
    # INDEXES_DIR, WORKSPACE_MEMORIES_DIR) re-read DATA_DIR — just
    # reloading the package shims leaves the submodules pointing at
    # the previous test's tmp dir, which caused the scheduler to see
    # stale workspaces across tests.
    importlib.reload(_common)
    importlib.reload(embeddings)
    for _name in (
        f"{workspaces.__name__}._mru",
        f"{workspaces.__name__}._store",
        f"{workspaces.__name__}._index",
        f"{workspaces.__name__}._search",
        f"{workspace_memory.__name__}._store",
        f"{workspace_memory.__name__}._search",
        f"{workspace_memory.__name__}._curate",
    ):
        _sub = sys.modules.get(_name)
        if _sub is not None:
            importlib.reload(_sub)
    for mod in (
        conversation,
        workspaces,
        workspace_memory,
        long_term,
        reflection,
        scheduler,
    ):
        importlib.reload(mod)

    dim = _common.EMBED_DIM

    def fake_embed(texts: Any) -> Any:
        return [[(len(t) % 31) / 31.0] + [0.05] * (dim - 1) for t in texts]

    monkeypatch.setattr(embeddings, "embed", fake_embed)
    monkeypatch.setattr(embeddings, "embed_one", lambda t: fake_embed([t])[0])

    return {
        "scheduler": scheduler,
        "conversation": conversation,
        "workspaces": workspaces,
        "workspace_memory": workspace_memory,
        "long_term": long_term,
        "reflection": reflection,
    }


# ── Helpers ────────────────────────────────────────────────────────────


class _Clock:
    """Mutable clock used by tests. The scheduler accepts ``clock`` as
    a callable, so we hand it ``clock.now`` and bump ``clock.t`` to
    advance time."""

    def __init__(self, t: Any = 1_000_000.0) -> None:
        self.t = float(t)

    def now(self) -> Any:
        return self.t

    def advance(self, seconds: Any) -> None:
        self.t += float(seconds)


def _step(text: Any) -> Any:
    class S:
        text: Any

    s = S()
    s.text = text
    return s


def _make_chat_fn(
    canned_text: Any = "An insight emerged.", verdict: Any = "keep"
) -> Any:
    """Build a synthetic chat_fn the scheduler can pass to reflect /
    curate. Returns reflections by default; for curation prompts (the
    system message asks for JSON verdicts) it returns one verdict per
    listed input."""

    def fake_chat(
        *, messages: Any, tools: Any = None, model: Any = None, **_kw: Any
    ) -> Any:
        sysmsg = messages[0]["content"] if messages else ""
        usermsg = messages[-1]["content"] if messages else ""
        if "memory curator" in sysmsg.lower():
            # Curation — one JSON line per indexed input.
            out = []
            for raw in usermsg.splitlines():
                raw = raw.strip()
                if not raw or "." not in raw:
                    continue
                try:
                    idx = int(raw.split(".", 1)[0])
                except ValueError:
                    continue
                out.append(f'{{"index": {idx}, "verdict": "{verdict}"}}')
            return _step("\n".join(out))
        # Reflection — emit a canned synthesis.
        return _step(canned_text)

    return fake_chat


# ── Tests ──────────────────────────────────────────────────────────────


def test_no_chat_fn_skipped(scheduler_env: Any) -> None:
    sched_mod = scheduler_env["scheduler"]
    sched = sched_mod.MemoryScheduler(chat_fn=None)
    assert sched.start() is False
    # tick() is callable but the count dict says nothing fired.
    counts = sched.tick()
    assert all(v == 0 for v in counts.values())


def test_long_term_fires_on_first_tick_then_respects_cadence(
    scheduler_env: Any,
) -> None:
    sched_mod = scheduler_env["scheduler"]
    lt = scheduler_env["long_term"]
    clock = _Clock()

    # Seed enough memories for reflection to consider them.
    for i in range(4):
        lt.save(f"a memory body {i}", source="test", importance=5)

    # Tight cadences so the test is fast — 1s / 100s / 10000s.
    cadence = sched_mod.CadenceConfig(
        poll_interval_s=1,
        long_term_reflect_s=100,
        workspace_reflect_s=10_000,
        workspace_curate_s=10_000,
        conversation_idle_s=10_000,
    )
    sched = sched_mod.MemoryScheduler(
        chat_fn=_make_chat_fn(),
        cadence=cadence,
        clock=clock.now,
    )

    counts = sched.tick()
    assert counts["long_term"] == 1, "first tick should fire long-term"

    # Advance 50s — still under the 100s cadence; no fire.
    clock.advance(50)
    assert sched.tick()["long_term"] == 0

    # Advance another 60s (total 110s past first fire) — fires again.
    clock.advance(60)
    assert sched.tick()["long_term"] == 1


def test_workspace_reflection_and_curation_per_workspace(
    scheduler_env: Any, tmp_path: Path
) -> None:
    sched_mod = scheduler_env["scheduler"]
    ws = scheduler_env["workspaces"]
    wm = scheduler_env["workspace_memory"]

    folder = tmp_path / "ws_sched"
    folder.mkdir()
    (folder / "x.txt").write_text("anything", encoding="utf-8")
    record = ws.activate(str(folder))

    for i in range(4):
        wm.save(record.id, f"workspace memory {i}", importance=5, source="test")

    clock = _Clock()
    cadence = sched_mod.CadenceConfig(
        poll_interval_s=1,
        long_term_reflect_s=10_000,  # don't fire long-term in this test
        workspace_reflect_s=100,
        workspace_curate_s=200,
        conversation_idle_s=10_000,
    )
    sched = sched_mod.MemoryScheduler(
        chat_fn=_make_chat_fn(verdict="keep"),
        cadence=cadence,
        clock=clock.now,
    )

    counts = sched.tick()
    assert counts["workspace_reflect"] == 1
    assert counts["workspace_curate"] == 1

    # 50s later — neither reflection nor curation fires (under cadence).
    clock.advance(50)
    counts = sched.tick()
    assert counts["workspace_reflect"] == 0
    assert counts["workspace_curate"] == 0

    # 60s later (110s total) — workspace reflection fires; curation
    # still under its 200s cadence.
    clock.advance(60)
    counts = sched.tick()
    assert counts["workspace_reflect"] == 1
    assert counts["workspace_curate"] == 0

    # 100s later (210s past curate's last fire) — curation fires.
    clock.advance(100)
    counts = sched.tick()
    assert counts["workspace_curate"] == 1


def test_conversation_reflection_idle_window(scheduler_env: Any) -> None:
    """Conversation reflection fires once the conversation has been
    idle for ``conversation_idle_s``. The conversation's ``updated_at``
    is stamped by ``save_conversation`` using real ``time.time()``, so
    we anchor the test clock to that real value plus an idle gap.
    """
    import time as _real_time

    sched_mod = scheduler_env["scheduler"]
    conv = scheduler_env["conversation"]

    cid = "sched_idle_test_1"
    conv.save_conversation(
        conv_id=cid,
        messages=[
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": "hi"},
        ],
    )
    real_now = _real_time.time()

    cadence = sched_mod.CadenceConfig(
        poll_interval_s=1,
        long_term_reflect_s=10_000,
        workspace_reflect_s=10_000,
        workspace_curate_s=10_000,
        conversation_idle_s=300,  # 5 min idle window
    )

    # Anchor the clock just under the idle window — should NOT fire.
    clock_under = _Clock(real_now + 100)
    sched_under = sched_mod.MemoryScheduler(
        chat_fn=_make_chat_fn(),
        cadence=cadence,
        clock=clock_under.now,
    )
    counts_under = sched_under.tick()
    assert counts_under["conversation"] == 0, "should not fire under idle window"

    # Anchor a fresh scheduler past the idle window — should fire once.
    clock_over = _Clock(real_now + 600)
    sched_over = sched_mod.MemoryScheduler(
        chat_fn=_make_chat_fn(),
        cadence=cadence,
        clock=clock_over.now,
    )
    counts_over = sched_over.tick()
    assert counts_over["conversation"] >= 1, "should fire after idle window"


def test_state_persists_across_restart(scheduler_env: Any, tmp_path: Path) -> None:
    """A fresh scheduler instance reads its prior state from disk and
    skips re-firing scopes that fired recently. State path is the
    scheduler's STATE_PATH inside the tmp DATA_DIR."""
    sched_mod = scheduler_env["scheduler"]
    lt = scheduler_env["long_term"]

    for i in range(3):
        lt.save(f"persist body {i}", source="test", importance=5)

    clock = _Clock()
    cadence = sched_mod.CadenceConfig(
        poll_interval_s=1,
        long_term_reflect_s=1000,
        workspace_reflect_s=10_000,
        workspace_curate_s=10_000,
        conversation_idle_s=10_000,
    )
    sched1 = sched_mod.MemoryScheduler(
        chat_fn=_make_chat_fn(),
        cadence=cadence,
        clock=clock.now,
    )
    counts = sched1.tick()
    assert counts["long_term"] == 1

    # New scheduler, same disk state, same clock → must NOT fire again.
    sched2 = sched_mod.MemoryScheduler(
        chat_fn=_make_chat_fn(),
        cadence=cadence,
        clock=clock.now,
    )
    counts2 = sched2.tick()
    assert counts2["long_term"] == 0

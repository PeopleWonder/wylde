"""Smoke test for the unified model_registry.

Exercises the four user-visible surfaces:

1. ``list_models()`` returns every kind it knows about.
2. ``list_models(kind="llm")`` is the inference-bar slice.
3. The HF cache scanner walks a fixture cache laid out the way
   huggingface_hub does it on disk and returns ``ModelEntry`` records.
4. The heuristic categoriser ``_heuristics.infer_kind`` lands the right
   kind for the canonical names listed in the brief.
5. Service-manifest declarations override the heuristic.

Run with: ``pytest Core/harness/model_registry/tests/test_model_registry.py``
"""

from __future__ import annotations

from typing import Any

import importlib
import json
from pathlib import Path

import pytest


_PKG = None


def _registry() -> Any:
    """Locate the model_registry package on whichever sys.path layout we get."""
    global _PKG
    if _PKG is not None:
        return _PKG
    for candidate in (
        "Wylde.Core.harness.model_registry",
        "Core.harness.model_registry",
        "harness.model_registry",
        "model_registry",
    ):
        try:
            _PKG = importlib.import_module(candidate)
            return _PKG
        except ImportError:
            continue
    pytest.skip("model_registry not importable from this sys.path")


# Heuristic ---------------------------------------------------------------


@pytest.mark.parametrize(
    "repo, expected",
    [
        ("openai/whisper-small", "stt"),
        ("openai/whisper-large-v3", "stt"),
        ("rhasspy/piper-voices", "tts"),
        ("hexgrad/Kokoro-82M", "tts"),
        ("microsoft/Florence-2-large", "vision"),
        ("liuhaotian/llava-1.5-7b", "vision"),
        ("nomic-ai/nomic-embed-text-v1", "embed"),
        ("BAAI/bge-large-en-v1.5", "embed"),
        ("Qwen/Qwen2.5-14B-Instruct", "llm"),
        ("meta-llama/Llama-3.1-8B-Instruct", "llm"),
    ],
)
def test_heuristic_infer_kind(repo: Any, expected: Any) -> None:
    reg = _registry()
    assert reg.infer_kind(repo) == expected, (
        f"infer_kind({repo!r}) returned {reg.infer_kind(repo)!r}, expected {expected!r}"
    )


def test_heuristic_default_is_llm_for_unknown_names() -> None:
    reg = _registry()
    assert reg.infer_kind("some-org/some-completely-unknown-model") == "llm"
    assert reg.infer_kind("") == "llm"


# HF cache scanner --------------------------------------------------------


def _make_fake_hub(tmp_path: Path) -> Path:
    """Lay out a fake HF hub cache with a few representative models."""
    hub = tmp_path / "huggingface" / "hub"
    hub.mkdir(parents=True)
    layout = {
        "models--openai--whisper-small": b"fake whisper blob",
        "models--microsoft--Florence-2-large": b"fake florence blob",
        "models--rhasspy--piper-voices": b"fake piper blob",
        "models--nomic-ai--nomic-embed-text-v1": b"fake embed blob",
        "models--Qwen--Qwen2.5-14B-Instruct": b"fake qwen blob",
        # Non-models entry should be ignored.
        "datasets--squad": b"ignored",
    }
    for name, blob in layout.items():
        d = hub / name
        d.mkdir()
        (d / "blobs").mkdir()
        (d / "blobs" / "model.bin").write_bytes(blob)
    return hub


def test_hf_scanner_walks_cache(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    reg = _registry()
    hub = _make_fake_hub(tmp_path)
    monkeypatch.setenv("HF_HUB_CACHE", str(hub))
    scanner = importlib.import_module(reg.__name__ + "._hf_scanner")
    scanner.invalidate_cache()

    entries = scanner.scan_hf_cache()
    ids = {e.id for e in entries}
    assert ids == {
        "openai/whisper-small",
        "microsoft/Florence-2-large",
        "rhasspy/piper-voices",
        "nomic-ai/nomic-embed-text-v1",
        "Qwen/Qwen2.5-14B-Instruct",
    }, f"unexpected scan result: {sorted(ids)}"

    by_id = {e.id: e for e in entries}
    assert by_id["openai/whisper-small"].kind == "stt"
    assert by_id["microsoft/Florence-2-large"].kind == "vision"
    assert by_id["rhasspy/piper-voices"].kind == "tts"
    assert by_id["nomic-ai/nomic-embed-text-v1"].kind == "embed"
    assert by_id["Qwen/Qwen2.5-14B-Instruct"].kind == "llm"

    for entry in entries:
        assert entry.size_bytes > 0, f"{entry.id} should report a non-zero size"
        assert entry.provider == "huggingface"
        assert entry.path is not None and entry.path.startswith(str(hub))


def test_hf_scanner_handles_missing_cache(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    reg = _registry()
    monkeypatch.setenv("HF_HUB_CACHE", str(tmp_path / "does-not-exist"))
    scanner = importlib.import_module(reg.__name__ + "._hf_scanner")
    scanner.invalidate_cache()
    entries = scanner.scan_hf_cache()
    assert entries == []


def test_hf_scanner_caches_on_signature(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Repeat calls don't rebuild when (path, mtime, size) is unchanged."""
    reg = _registry()
    hub = _make_fake_hub(tmp_path)
    monkeypatch.setenv("HF_HUB_CACHE", str(hub))
    scanner = importlib.import_module(reg.__name__ + "._hf_scanner")
    scanner.invalidate_cache()

    first = scanner.scan_hf_cache()
    second = scanner.scan_hf_cache()
    assert {e.id for e in first} == {e.id for e in second}


# Manifest override -------------------------------------------------------


def test_service_manifest_overrides_heuristic(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A service manifest declaring kind=embed for a 'whisper' repo overrides
    the heuristic. Reverse case (declaring kind=llm for a clean LLM name)
    must also stick."""
    reg = _registry()

    # VoiceAssistant is one of the recognised _SERVICE_ROOTS.
    fake_root = tmp_path / "Wylde"
    (fake_root / "VoiceAssistant").mkdir(parents=True)
    (fake_root / "VoiceAssistant" / "manifest.json").write_text(
        json.dumps(
            {
                "name": "VoiceAssistant",
                "models": [
                    {"id": "openai/whisper-small", "kind": "embed", "required": True},
                    {"id": "fakeorg/CustomChat-7B", "kind": "llm", "required": False},
                ],
            }
        ),
        encoding="utf-8",
    )

    sm = importlib.import_module(reg.__name__ + "._service_manifests")
    monkeypatch.setattr(sm, "_WYLDE_ROOT", fake_root, raising=True)
    sm.invalidate_cache()

    overrides, required_by = sm.load_declarations(force=True)
    assert overrides["openai/whisper-small"] == "embed"
    assert overrides["fakeorg/CustomChat-7B"] == "llm"
    assert "VoiceAssistant" in required_by["openai/whisper-small"]


# Public list_models surface ----------------------------------------------


def test_list_models_returns_all_kinds(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    reg = _registry()
    hub = _make_fake_hub(tmp_path)
    monkeypatch.setenv("HF_HUB_CACHE", str(hub))
    monkeypatch.setattr(reg._routing, "list_ollama_models", lambda: [])
    monkeypatch.setattr(reg._routing, "list_profiles", lambda: [])
    reg.refresh_cache()

    all_entries = reg.list_models()
    kinds_seen = {e.kind for e in all_entries}
    assert {"llm", "stt", "tts", "vision", "embed"} <= kinds_seen, (
        f"missing kinds in unified view: {kinds_seen}"
    )


def test_list_models_kind_filter_isolates_llms(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The inference-bar contract: list_models(kind='llm') sees ONLY LLMs."""
    reg = _registry()
    hub = _make_fake_hub(tmp_path)
    monkeypatch.setenv("HF_HUB_CACHE", str(hub))
    monkeypatch.setattr(reg._routing, "list_ollama_models", lambda: [])
    monkeypatch.setattr(reg._routing, "list_profiles", lambda: [])
    reg.refresh_cache()

    llms = reg.list_models(kind="llm")
    assert llms, "expected at least one LLM in the fake hub"
    for entry in llms:
        assert entry.kind == "llm", (
            f"{entry.id} leaked into kind=llm filter as {entry.kind}"
        )

    forbidden = {
        "openai/whisper-small",
        "microsoft/Florence-2-large",
        "rhasspy/piper-voices",
        "nomic-ai/nomic-embed-text-v1",
    }
    leaked = forbidden & {e.id for e in llms}
    assert not leaked, f"non-LLM kinds leaked into llm filter: {leaked}"


def test_list_models_rejects_unknown_kind() -> None:
    reg = _registry()
    with pytest.raises(ValueError):
        reg.list_models(kind="not-a-kind")


def test_get_model_round_trip(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    reg = _registry()
    hub = _make_fake_hub(tmp_path)
    monkeypatch.setenv("HF_HUB_CACHE", str(hub))
    monkeypatch.setattr(reg._routing, "list_ollama_models", lambda: [])
    monkeypatch.setattr(reg._routing, "list_profiles", lambda: [])
    reg.refresh_cache()

    entry = reg.get_model("microsoft/Florence-2-large")
    assert entry is not None
    assert entry.kind == "vision"
    assert reg.get_model("definitely/not/a/model") is None


def test_is_loaded_reflects_ollama(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    reg = _registry()
    hub = _make_fake_hub(tmp_path)
    monkeypatch.setenv("HF_HUB_CACHE", str(hub))
    monkeypatch.setattr(
        reg._routing, "list_ollama_models", lambda: ["Qwen/Qwen2.5-14B-Instruct"]
    )
    monkeypatch.setattr(reg._routing, "list_profiles", lambda: [])
    reg.refresh_cache()

    assert reg.is_loaded("Qwen/Qwen2.5-14B-Instruct") is True
    assert reg.is_loaded("openai/whisper-small") is False
    assert reg.is_loaded("never/heard-of-it") is False

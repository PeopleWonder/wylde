"""Live smoke test of the gateway's OpenAI-compatible /v1 API (#345).

Run by `wylde-release preflight --launch` (check `l3.openai_v1`) against a
running Wylde stack; also runnable by hand. Prints ONE JSON line
({"ok", "mode", "steps": [...], "error"?}) and exits 0 when every step passed.

Two modes:

* auth-only (no token): proves /v1 is mounted on the running gateway and
  rejects an unauthenticated call with OpenAI's error shape (401
  invalid_api_key). Needs only the Python standard library.
* full (WYLDE_OPENAI_SMOKE_TOKEN set to a Wylde device token): drives the
  official `openai` client (pip install openai) through
  models -> chat -> tool call -> stream -> embeddings -> FIM.

Environment:
  WYLDE_OPENAI_SMOKE_BASE_URL    default http://127.0.0.1:<WYLDE_GATEWAY_PORT or 8005>/v1
  WYLDE_OPENAI_SMOKE_TOKEN       device token; enables full mode
  WYLDE_OPENAI_SMOKE_MODEL       chat/agent model (default: alias "coder" if listed)
  WYLDE_OPENAI_SMOKE_EMBED_MODEL embedding model (default: alias "embed" if listed)
  WYLDE_OPENAI_SMOKE_FIM_MODEL   FIM model (default: the chat model)
"""

import json
import os
import sys
import urllib.error
import urllib.request


def base_url():
    explicit = os.environ.get("WYLDE_OPENAI_SMOKE_BASE_URL", "").strip()
    if explicit:
        return explicit.rstrip("/")
    port = os.environ.get("WYLDE_GATEWAY_PORT", "").strip() or "8005"
    return f"http://127.0.0.1:{port}/v1"


def auth_only(base):
    """GET /v1/models with no key must be an OpenAI-shaped 401."""
    try:
        urllib.request.urlopen(base + "/models", timeout=10)
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", "replace")
        try:
            code = json.loads(body)["error"]["code"]
        except (ValueError, KeyError, TypeError):
            return False, f"HTTP {e.code} but not an OpenAI error body: {body[:200]}"
        if e.code == 401 and code == "invalid_api_key":
            return True, "401 invalid_api_key (route mounted, gate live)"
        return False, f"expected 401 invalid_api_key, got HTTP {e.code} code={code}"
    except (urllib.error.URLError, OSError) as e:
        return False, f"gateway unreachable at {base}: {e}"
    return False, "unauthenticated /v1/models was served (the gate is not enforced)"


def full(base, token):
    try:
        from openai import OpenAI
    except ImportError:
        return [("import-openai", False, "the `openai` package is missing: pip install openai")]
    client = OpenAI(base_url=base, api_key=token, timeout=180, max_retries=0)
    steps = []

    def step(name, fn):
        try:
            ok, detail = fn()
        except Exception as e:  # noqa: BLE001 - any client/API failure is a failed step
            ok, detail = False, f"{type(e).__name__}: {e}"
        steps.append((name, ok, detail))
        return ok

    ids = []

    def models():
        ids.extend(m.id for m in client.models.list())
        return bool(ids), f"{len(ids)} models: {', '.join(ids[:6])}"

    if not step("models", models):
        return steps
    chat_model = os.environ.get("WYLDE_OPENAI_SMOKE_MODEL") or (
        "coder" if "coder" in ids else next((i for i in ids if "embed" not in i), ids[0]))
    embed_model = os.environ.get("WYLDE_OPENAI_SMOKE_EMBED_MODEL") or (
        "embed" if "embed" in ids else next((i for i in ids if "embed" in i), None))
    fim_model = os.environ.get("WYLDE_OPENAI_SMOKE_FIM_MODEL") or chat_model

    def chat():
        r = client.chat.completions.create(
            model=chat_model, max_tokens=16,
            messages=[{"role": "user", "content": "Reply with the single word: ready"}])
        text = (r.choices[0].message.content or "").strip()
        return bool(text), f"{chat_model}: {text[:40]!r}"

    def tool_call():
        tools = [{"type": "function", "function": {
            "name": "read_file", "description": "Read a file from the workspace",
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}},
                           "required": ["path"]}}}]
        r = client.chat.completions.create(
            model=chat_model, tools=tools,
            messages=[{"role": "user", "content": "Use the read_file tool to read src/main.rs."}])
        calls = r.choices[0].message.tool_calls or []
        names = [c.function.name for c in calls]
        return "read_file" in names, f"finish={r.choices[0].finish_reason} calls={names}"

    def stream():
        parts = []
        for chunk in client.chat.completions.create(
                model=chat_model, max_tokens=24, stream=True,
                messages=[{"role": "user", "content": "Count from 1 to 5."}]):
            if chunk.choices and chunk.choices[0].delta.content:
                parts.append(chunk.choices[0].delta.content)
        text = "".join(parts).strip()
        return bool(text), f"{len(parts)} deltas: {text[:40]!r}"

    def embeddings():
        if not embed_model:
            return False, "no embedding model listed (set WYLDE_OPENAI_SMOKE_EMBED_MODEL)"
        r = client.embeddings.create(model=embed_model, input="hello")
        dim = len(r.data[0].embedding)
        return dim > 0, f"{embed_model}: dim {dim}"

    def fim():
        r = client.completions.create(
            model=fim_model, prompt="def add(a, b):\n    ", suffix="\n", max_tokens=16)
        text = r.choices[0].text
        return bool(text.strip()), f"{fim_model}: {text[:40]!r}"

    for name, fn in [("chat", chat), ("tool-call", tool_call), ("stream", stream),
                     ("embeddings", embeddings), ("fim", fim)]:
        step(name, fn)
    return steps


def main():
    base = base_url()
    token = os.environ.get("WYLDE_OPENAI_SMOKE_TOKEN", "").strip()
    if token:
        mode, steps = "full", full(base, token)
    else:
        ok, detail = auth_only(base)
        mode, steps = "auth-only", [("auth-gate", ok, detail)]
    ok = bool(steps) and all(s[1] for s in steps)
    print(json.dumps({
        "ok": ok,
        "mode": mode,
        "base_url": base,
        "steps": [{"step": n, "ok": o, "detail": d} for n, o, d in steps],
    }))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())

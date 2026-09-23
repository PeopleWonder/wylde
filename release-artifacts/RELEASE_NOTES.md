# Wylde 0.2 — release notes (staging)

> **STATUS: staging / not yet tagged.** This is the living, human-readable
> release notes for the **0.2** line, kept current as work lands so the notes
> don't have to be reconstructed at tag time (release-checklist §B.5). The
> workspace version is `0.2.0-beta.1` (a GitHub **pre-release** on the Beta
> channel); the stable **`0.2.0`** cut remains gated on the maintainer's
> separate say-so (#38) and its hands-on feel-test sign-off (#274). The release
> date is stamped here when the tag is cut. For the machine-checked, per-change
> log see [`CHANGELOG.md`](../CHANGELOG.md); this file is the readable summary.

**0.2 is the first release of the modern, all-Rust Wylde stack, and its theme is
*verified, not just built*.** The only earlier tag — `v0.1.0-alpha.1`
(2026-06-04, archived in `RELEASE_NOTES-v0.1.0-alpha.1.md`) — shipped the gpui
desktop rebuild while the runtime beneath it was still Python, and it was
redeployed broken more than once because nothing checked that the *assembled*
product actually ran. 0.2 closes that gap: it ships the fixed all-Rust trunk and
puts a launch-and-verify gate in front of every release so the class of defect
that shipped before is blocked, not merely documented.

## Headline changes

- **Full-Rust cutover.** Every Python runtime component was ported to Rust and
  its source deleted — RAG/retrieval, voice (ONNX Whisper + Kokoro), and the
  turn engine now run in-process. No Python venv to rot in the field.
- **Local-first memory.** Short-term, long-term, and reflection memory across
  the conversation, workspace, and long-term scopes — with a workspace
  **knowledge graph** (native gpui graph panel + in-app IDE), **BM25 lexical
  retrieval + RRF fusion**, and a **Thought Bubble System** that does
  structural retrieval before each turn. Nothing leaves the machine unless you
  turn it on.
- **Opt-in, off-by-default experiments.** A definitional **concept hierarchy**
  and a **concept-routing** decision layer (both isolated and byte-identical
  when disabled), and an **agentic reasoning tier** shipped `enabled: false` —
  present for testing, silent until you opt in.

## Verified, not just built — what the gate now guarantees

The enforcement layer whose absence let the alpha ship broken is now wired in:

- **The app is proven to *run*, not just compile.** A one-command
  launch-and-verify preflight cold-starts the stack from a neutral directory and
  asserts, each check failing closed: the daemon comes up, services are
  discovered and healthy, **Memgraph holds real data** (not an empty graph), RAG
  answers a query, and **a chat turn completes** — with a commit-bound receipt
  that `wylde-release publish` refuses to ship without.
- **Every GUI page loads and every control acts (#247).** A headless panel-walk
  (L7, a required CI job) mounts every panel and drives its controls, asserting
  each one *does something* rather than merely renders — every interactive
  control is now routed through one constructor, so a dead, unwired, or new
  defective button reds the build.
- **The model finally sees the conversation (#248).** A gap left by the
  Python→Rust cutover meant each turn's exchange was never persisted, so the
  assistant re-read an empty history every turn and came across as forgetful.
  Each completed exchange is now persisted (raw user message + reply) on the one
  seam both chat paths share; the 5-message auto-summary that the same missing
  write had starved now runs too.
- **No silent dead panels (#239/#241).** The GUI reflects live service presence
  and health instead of a snapshot taken at start-up, so an extracted or
  unreachable service shows its real state rather than a panel pointed at a dead
  port.
- **Durable, predictable state.** Path handling was unified onto one convention
  across the four data roots (#250); the default model is persisted and
  **survives an update** (#235/#238/#243); and version consistency (G7), the
  license/advisory gates, and the benchmark regression gate all gate every
  change.

## Known limits

- The **hands-on feel test (L6) is a required, non-waivable human gate** — 0.2
  is not tagged until it is done and signed off (#274). Automation, the
  control-walk, and the visual-smoke screenshots reduce and prepare it; none
  replaces it.
- The reasoning tier ships **off** and is a post-0.2 experiment.

## Assets

Stamped at tag time. Every release carries the **whole stack** (GUI + lifecycle
daemon + every backend service), each binary individually signed — the updater
refuses a partial stack (#97). See the release checklist for the exact roster.

---
title: First-run bootstrap (LLM playbook)
audience: the on-device LLM running the very first conversation after install
authored: 2026-05-27
status: SUPERSEDED 2026-07-16 — retained for its rationale; see the banner below
---

# First-run bootstrap

> ## ⚠️ SUPERSEDED — 2026-07-16. Do not implement this as written. See #66.
>
> **The direction reversed.** This document specifies an *invisible* first run: the LLM reads
> its own playbook and configures the stack, and the user "never sees a wizard". The maintainer
> has since decided first-run should be a **deliberately guided user experience**, with an
> **install wizard** (#67) — and that it is **post-0.2**, not the day-1 Phase 12 requirement it
> was written as:
>
> > "so for the bootstrap I want it to be a deliberately guided user experience. that will be
> > sometime further down the road. we don't need bootstrapping right now per se."
>
> **This doc is kept, not deleted, because the rationale is worth reading before redesigning.**
> The argument was that a wizard is a chore the user shouldn't have to complete, and that a model
> capable of running the product is capable of configuring it. That still has force; it lost on the
> grounds that "invisible" also means *unguided*, not on the grounds that it was wrong. Step 6's
> refusal to emit a completion event ("Do not say 'bootstrap complete'… The conversation flows")
> is the sharpest expression of it — and the thing a guided UX most directly overturns.
>
> ### What is actually true today
>
> **None of this is implemented. It is documentation.** Every code reference to this file is a
> *comment* (`recommend.rs:6`, `consent.rs:321`, `inventory.rs:3`) — nothing does `include_str!`,
> nothing injects it into a prompt, **nothing feeds this playbook to the model**. And there is
> **no first-run detection anywhere** in `rust/crates` or `Core/GUI` (no `is_first_run`,
> `first_launch`, `onboard`, …): Wylde cannot currently tell a first launch from the thousandth.
>
> The one real thing here is **`rust/crates/wylde-harness/tests/bootstrap_doc_validity.rs`**,
> which pins every tool/pipe/broker id in the appendix to the live registry so the doc can't rot
> into naming dead verbs. That test is design-independent and **should survive the redesign**.
>
> This is also why preflight **L4** was never scriptable and is now deferred (**#55**): it asserted
> the completion of something designed not to signal completion *and* not implemented at all.
> ⚠️ The **clean-install** requirement is separate, still 0.2, and lives in **#37** — it did not
> defer with L4.
>
> **Read on for the rationale and the verb reference; do not read it as current behaviour.**

This document is for **you**, the language model running the very
first conversation on a fresh Wylde install. There is no human-driven
wizard. The first conversation IS the setup. Greet the user, ask what
they want to use Wylde for in plain language, and tune the install via
the pipe-action calls listed below. Do not invent verbs or tool ids —
every one referenced here resolves in the live registry (the
`bootstrap_doc_validity` test gates that).

You are an LLM, not the user. The user's first message will be
something like "hi" — your reply is the start of setup, not a
restatement of these instructions.

## Operating rules

1. Be brief. The user is not reading a manual; they typed "hi."
2. One question at a time. Never bullet-list a configuration
   interrogation — ask, get an answer, ask the next thing.
3. Default to action, not narration. When you have enough to call
   `system.inventory` or `models.list`, do it; do not describe what you
   are about to do.
4. Consent gates fire on every tool dispatch (Phase 12.2). When the
   user approves once for a tool, decide whether to call
   `consent.respond` with `decision: "approved"` so it stops asking on
   that tool. Use this judgement sparingly — the user owns the
   decision.
5. Persist what you learn. Detected hardware goes into
   `data/settings.json` (write via `fs.write_file`); preferences go via
   `memory.long_term.save` so they survive the next boot.

## Step 1 — sample the hardware

Call `system.inventory` on the **vram-broker** service. No payload.
The reply is the host snapshot:

```json
{
  "cpu": {"brand": "...", "physical_cores": N, "logical_cores": N,
          "frequency_mhz": N, "arch": "x86_64", "vendor_id": "..."},
  "memory_total_bytes": N,
  "memory_available_bytes": N,
  "disks": [{"mount": "C:\\", "kind": "ssd|hdd|unknown",
             "total_bytes": N, "available_bytes": N,
             "file_system": "NTFS", "is_removable": false}],
  "gpus": [{"vendor": "nvidia", "name": "...",
            "vram_bytes": N, "vram_used_bytes": N}],
  "intel_gpus": [{"vendor": "intel", "vendor_id": 32902,
                  "device_id": N, "name": "Intel(R) ...",
                  "dedicated_vram_bytes": N,
                  "shared_system_memory_bytes": N}],
  "amd_gpus":   [{"vendor": "amd", "vendor_id": 4098, ...}],
  "npus": [{"vendor": "intel|amd", "kind": "ai_boost|xdna",
            "source": "heuristic|probe", "note": "..."}],
  "npu": {"present": false, "vendor": null, "kind": null},
  "os": {"family": "windows", "name": "Windows",
         "version": "11 (...)", "kernel_version": "...",
         "hostname": "..."}
}
```

`gpus` is NVIDIA-only (NVML-sourced, includes live `vram_used_bytes`).
`intel_gpus` / `amd_gpus` come from DXGI; they do **not** carry a used-
VRAM counter — DXGI doesn't expose one. For iGPUs / APUs the
`dedicated_vram_bytes` is typically 0 and `shared_system_memory_bytes`
is the working set borrowed from DRAM. Phase 12.4 added these three
arrays.

**NPU.** `npus` is a list of detected NPUs. Phase 12.4 only ships
`source: "heuristic"` entries — matched against the CPU brand string
("Core Ultra" → Intel AI Boost, "Ryzen AI" → AMD XDNA). The heuristic
**misses** Phoenix-class Ryzen 7040 chips that have an NPU but aren't
branded "Ryzen AI"; the `note` field flags this. Treat heuristic hits
as a strong hint, not a guarantee, and confirm with the user before
loading an NPU-tuned model. Legacy `npu` (singular) mirrors `npus[0]`
for older readers — prefer `npus` for new code.

## Step 2 — pick a model that fits

Compute usable VRAM as
`max(0, gpu.vram_bytes - gpu.vram_used_bytes - 1_000_000_000)` (the
1 GB margin matches the broker's `safety_margin` default). Then route:

| Branch | Pick a model |
|---|---|
| usable VRAM ≥ 24 GB | `qwen2.5-7b-instruct-q4_K_M` |
| usable VRAM 12–24 GB | `qwen2.5-3b-instruct-q4_K_M` |
| usable VRAM 6–12 GB | `qwen2.5-1.5b-instruct-q4_K_M` |
| usable VRAM 0–6 GB and RAM ≥ 16 GB | `qwen2.5-1.5b-instruct-q4_K_M` (CPU spill via broker) |
| usable VRAM 0 GB and RAM < 16 GB | `qwen2.5-0.5b-instruct-q4_K_M` |

The model ids above are pulled from the live ollama library; if the
exact tag is unavailable, ask the user to confirm an alternative
before pulling. Do not silently substitute.

**No discrete NVIDIA, but iGPU or NPU present.** If `gpus` is empty
*and* one of `intel_gpus` / `amd_gpus` / `npus` is non-empty, prefer
an INT8 ONNX variant over the fp16 GGUF tags above when one exists in
the live library — iGPU and NPU runtimes (DirectML, OpenVINO, Ryzen
AI) are tuned for INT8 paths and the throughput delta over CPU-only
GGUF is large. If no ONNX variant exists, fall back to the matching
GGUF row and route to CPU; flag this to the user so they know
they're leaving the iGPU/NPU idle.

Call `tools.run` with a payload naming `list_loaded_models` to
enumerate what ollama already has loaded. If the chosen model is
present, skip the pull. Pulling a model is not yet a first-class tool
in Phase 12.2 — ask the user to run the appropriate
`ollama pull <model>` command themselves, or call `fs.list_files`
against the ollama model directory listed in `data/settings.json` to
see what's on disk without a pull.

## Step 3 — find out why the user is here

Ask **one** question. Examples (pick by vibe of their first message —
not a script):

- "What are you hoping Wylde will do for you?"
- "Coding help, research, writing, voice control of the box — where
  do you want to start?"

Convert the answer into the matching service group. Each group
corresponds to entries in `data/settings.json`'s
`enabled_service_groups`:

- `"ai"` — always on; chat brain, memory, RAG.
- `"voice"` — on if the user mentions speaking, dictation, hands-free,
  or wake-word ("hey Wylde"). Requires `cpu_cores ≥ 4` and a working
  mic; if neither, say so and leave voice off.
- `"network"` — on if they mention "remote control of my laptop,"
  "phone access," or any cross-device usage. Requires VPN setup,
  which is post-12.2 — for now, flag it and proceed without.
- `"extensions"` — on if they mention web research, study, automation
  workflows. The shipped webcrawler + wylde-study extensions enable
  here.

Persist the chosen set by writing `data/settings.json` (read first
with `read_file` so you don't clobber the wizard's prior fields), and
save a one-line memory:

```
memory.long_term.save {
  "body": "user wants Wylde for <one phrase>; enabled service groups: <list>",
  "source": "first_run_bootstrap",
  "importance": 8,
  "tags": ["bootstrap", "user_intent"]
}
```

## Step 4 — create the first workspace

A workspace is Wylde's name for a project context. Call
`workspaces.list_mru` first; if the result is empty, create one that
matches their stated use with `workspaces.create { folder, name? }` —
this registers the folder as a workspace and activates it. (For
Phase 12.2: skip this step if no folder is in scope yet; the user
will create one naturally when they start their first project.)

Set the persona for the new workspace:
`workspaces.set_persona { workspace_id, text }` where `text`
is a 1-2 sentence orientation derived from Step 3 ("This workspace
is for working on a Rust port of a Python service for the Wylde user, a solo
maintainer.") The persona is what future LLM turns see at the top of
their context inside that workspace.

## Step 5 — explain the consent flow once

Consent gates are ON by default on every fresh install (Phase 12.2).
After the first tool dispatch (probably `system.inventory` in Step 1)
the user will have seen one gate prompt already. Tell them, in one
sentence, what's happening:

> "Every tool call needs your approval the first time — you can
> 'remember' an answer or turn approvals off entirely in settings."

If they push back ("just let it run"), call
`consent.set_no_auth { "enabled": true }`. This is a deliberate
power-user escape hatch; the user must say so explicitly.

If they engage with the prompts, that's fine — say nothing more
about consent until they ask.

## Step 6 — close the bootstrap

Save a final memory:

```
memory.long_term.save {
  "body": "first-run bootstrap complete on <YYYY-MM-DD>; hardware tier=<vram_gb>GB VRAM, model=<chosen>, intent=<one phrase>",
  "source": "first_run_bootstrap",
  "importance": 7,
  "tags": ["bootstrap"]
}
```

Then say one normal-conversation thing that flows from Step 3 —
e.g. "Ready when you want to start the Rust port. What's the first
file you want to look at?" Do not say "bootstrap complete," "setup
finished," or anything that sounds like a wizard. The conversation
flows.

## Tool / action reference (every id this doc cites)

These are checked by `tests/bootstrap_doc_validity.rs`; adding an id
to the doc body without adding it to a resolvable surface will fail
that test.

**Broker actions** (call against `wylde-vram-broker`):
- `system.inventory`

**Harness pipe actions** (call against `wylde-harness`):
- `tools.list`, `tools.run`
- `memory.long_term.list`, `memory.long_term.save`,
  `memory.long_term.update`, `memory.long_term.delete`,
  `memory.long_term.history`
- `conversations.set_workspace`

**Workspaces service actions** (call against `wylde-workspaces` — moved off
the harness pipe in Thought Bubble System Slice 0d; reach them via the
`wylde-workspaces-client` crate):
- `workspaces.list_mru`, `workspaces.set_active`, `workspaces.create`,
  `workspaces.update`, `workspaces.set_persona`, `workspaces.delete`
- `consent.list`, `consent.set`, `consent.respond`,
  `consent.clear`, `consent.set_no_auth`, `consent.reset`
- `chat.run_turn`, `chat.start_turn`, `chat.cancel`,
  `chat.stream_turn`, `chat.stream_tools`

**LLM-callable tool ids** (used inside the `tools.run` payload's
`name` field or emitted by the model in a tool-call envelope):
- `read_file`, `list_files`, `write_file`, `edit_file`
- `show_diff`, `apply_patch`
- `code_search`, `code_search_files`
- `tool_search`, `graph_query`
- `time_now`, `time_format`
- `list_loaded_models`, `preload_model`, `evict_model`, `auto_evict_lru`
- `memory_long_term_save`, `memory_update`, `memory_delete`, `memory_search`
- `memory_workspace_save`, `memory_workspace_update`,
  `memory_workspace_delete`, `memory_workspace_search`,
  `memory_workspace_list`

## Test bypass note

The harness honours `WYLDE_HARNESS_CONSENT_BYPASS=1` and a runtime
`set_bypass_for_tests(true)` to skip every consent gate. This exists
for the harness's own test corpus; production never sets either.
You, the LLM, never need to know about this — it does not change the
shape of any reply you receive. Documented here so anyone reading the
doc to debug a "gate didn't fire" mystery can find the answer.

## Runtime topology (how this conversation got here)

You are running inside the gpui desktop app, **`wylde-gui.exe`**. As of
the slice-11 cutover (2026-05-29) that binary replaced the old Tauri +
Svelte GUI; `Core/GUI/src/` and `Core/GUI/src-tauri/` no longer exist.

The launch chain is:

1. `launch_wylde.ps1` (the desktop shortcut target) boots the Lifecycle
   daemon (`Core/Lifecycle/daemon.py`, or the Rust `wylde-lifecycle.exe`
   when `WYLDE_LIFECYCLE_IMPL=rust`).
2. The daemon runs filesystem-as-registry discovery, spawns the enabled
   services from their `manifest.json` files (topologically ordered by
   `depends_on`), and brings up the tier=core services + the
   `\\.\pipe\wylde-lifecycle` and `\\.\pipe\wylde-harness` pipes.
3. Once the lifecycle pipe is up, the script launches
   `Core/GUI/target/release/wylde-gui.exe`, which is where you live.

Shutdown is the reverse: closing the window (X) or the tray **Quit Wylde**
item both run the graceful drain (`lifecycle.shutdown_all`, then a 10 s
grace, then a hard-kill fallback) before the GUI process exits — so the
backend services are never orphaned. You do not drive shutdown; it is a
user action.

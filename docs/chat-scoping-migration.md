# Chat-scoping migration note

**Date:** 2026-06-15 · Slice **C10** of the chat-scoping + RAG-relevance build
([`outputs/wylde-chat-scoping-EXECUTION-PLAN.md`](../outputs/wylde-chat-scoping-EXECUTION-PLAN.md)).
Documentation only — there is no migration code. An optional one-time tag pass is explicitly out of
scope.

## What changed

Conversations now carry an optional `workspace_id`. A conversation is **bound** iff its document
carries a `workspace_id`; otherwise it is **unbound**.

- **Bound** → reads workspace context (layer 2: persona + notes + RAG + code-graph), writes reflection
  to workspace memory, **never** long-term. Appears in that workspace's per-workspace switcher.
- **Unbound** → reads long-term (layer 1), writes reflection to long-term. Appears in the **global**
  Chat list only.

The **Global Chat** surface is strictly workspace-free (decision D1): it shows unbound conversations
and can never attach a `workspace_id`. **Workspace chats** (the Workspaces dock) show only the entered
workspace's bound conversations and never inject long-term (decision D2).

## Impact on existing data

**Every conversation that existed before this build is unbound** — none carries a `workspace_id`.
Consequently:

- Pre-existing conversations appear **only in the global Chat list**. They will **not** show up in any
  workspace's switcher.
- There is **no automatic backfill**. A legacy conversation becomes workspace-scoped only when a user
  manually binds it (mints/continues it inside a workspace). Until then it stays global.
- This is intentional and safe: an unbound conversation reads long-term and writes reflection to
  long-term, exactly as it did before the build.

## Canonical store (Route 1) — and the legacy service store

The **harness flat store** is canonical for live turns:

- Live store: `wylde-harness/src/memory/conversations/store.rs` →
  `<data_dir>/conversations/<id>.json`. "A workspace's conversation list" is simply a `workspace_id`
  filter over this flat store — there is no second live write path.

The **per-workspace conversation _service_ store** introduced in the earlier Slice-0c
(`wylde-workspaces/src/conversations/store.rs`) is **legacy-for-live / export-only**. Do not add a
second live write path against it. It still cascades on workspace delete, but the canonical flat-store
conversations are swept separately on delete (see slice C9).

## Deletion

Deleting a workspace removes its bound flat-store conversations via the C9 sweep (matching
`workspace_id`), in addition to the existing service-store cascade. Unbound (global) conversations are
never touched by a workspace delete.

## TL;DR

- Old conversations = unbound = global-only, until manually bound. No backfill.
- Flat store is canonical for live turns; the Slice-0c service store is legacy/export-only.
- Long-term lives on the global surface (+ manual copy-in into a workspace's notes); workspace chats
  never inject it.

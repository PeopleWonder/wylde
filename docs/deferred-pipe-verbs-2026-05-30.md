# Deferred pipe verbs — 2026-05-30

Tracks pipe verbs the gpui panels would consume but that aren't shipped
yet, why each is deferred, and the smallest concrete plan to ship it
later. Companion to the 2026-05-30 slice that shipped
`device_gate.recent_actions` and `models.{get,set}_default`.

Recommended ship order (cheapest / highest-leverage first):

1. **C — memory.short-term in the Chat panel** (verbs already exist;
   only needs a host with a `conversation_id`).
2. **D — `training.datasets` read-only** (if a clean datasets dir exists)
   then the rest of Training via the N8N thin-client.
3. **E — `vpn.ddns.status` / `vpn.ddns.set`** (provider lib already
   present; needs action wiring + a last-update field).
4. **F — AdGuard rewrites** (needs a whole new API client + infra).
5. ~~**G — `images.generate` streaming progress**~~ — **abandoned
   2026-07-22 (#234).** ComfyUI was removed from Wylde and the image
   Service was parked; there is no `images.generate` verb left to add
   progress to. Kept below as a record of the upstream audit only.

---

## C — memory.short_term layer in the Memory panel

**Verbs:** `memory.short_term.get`, `memory.short_term.clear` (both
already served by the Python harness `_ACTIONS`; `memory.short_term.append`
too).

**Why deferred:** the verbs exist and work, but they are
**conversation-scoped** — each requires a `conversation_id`. The Memory
panel (`Core/GUI/Frontend/Panels/Memory`) is a *global* three-layer
browser (long-term / workspace / short-term) with no active-conversation
context in scope. The working-memory layer is inherently per-conversation,
so it belongs to whichever surface owns the active conversation id — that
is the Chat panel, not the global Memory browser. Inventing a "most-recent
conversation" derivation here (via `conversations.list` → newest id →
`memory.short_term.get`) is possible but semantically wrong: the Memory
panel would show a buffer for a conversation the user may not be looking
at, and there is no cross-panel signal telling it which conversation is
active.

**Smallest plan to ship:**
1. Add an active-conversation channel: extend `wylde-gui-pipe`'s nav/state
   bus (already used for cross-panel nav) with a `current_conversation_id`
   the Chat panel publishes when a conversation is opened.
2. Render the short-term layer **in the Chat panel** (or a Chat-embedded
   side strip), reading `memory.short_term.get { conversation_id }` and
   wiring a "Clear working memory" button to `memory.short_term.clear`.
   Reply shape is `{ working_memory, conversation_id }`; confirm the
   `working_memory` entry shape against
   `Core/harness/memory/conversation.py::get_working_memory` before
   building the parser.
3. In the Memory panel, replace the placeholder with a one-line pointer
   ("Open a chat to see its working memory") rather than a degraded card.

**Files:** `Core/GUI/Frontend/Panels/Memory/src/memory_panel.rs`
(placeholder), Chat panel crate (new home), `wylde-gui-pipe` nav bus.

---

## D — Trainer: jobs / start_training / stop / status / datasets

> **Superseded 2026-06-04:** the entire trainer track (the `wylde-trainer`
> crate, `Trainer/` Python incl. Caption, and the gpui Training panel) was
> extracted from the alpha — see `docs/retired-trainer-scope.md`. The plan
> below is preserved as the intended approach for when the track resumes as
> a separate `wylde-trainer` project.

**Verbs the panel wants:** `training.jobs`, `training.start`,
`training.stop`, `training.status`, `training.datasets` (panel currently
reaches for `/api/*` routes that never existed).

**Why deferred:** the `wylde-trainer` service ships only the `caption.*`
surface (Florence-2 captioning). LLaMA-Factory was retired, so there is
no fine-tuning job runner behind any of these verbs, and the panel's
`/api/*` REST routes were never implemented. A read-only `training.datasets`
verb was scoped as a possible partial ship, but **no dataset source
directory exists in the tree today** (only `Trainer/Caption/`), so even
the read-only verb has nothing to enumerate. Shipping any subset now would
be a placeholder over a placeholder.

**Smallest plan to ship (recommended path — N8N thin-client, per
`wylde_n8n_principle`):**
1. Model training jobs as **N8N workflows** rather than a bespoke
   in-process runner. The Training panel becomes a thin client that
   lists / starts / stops workflow executions via the existing
   `n8n_*` tool surface (`N8N/tools/`).
2. Define the job contract as a workflow input schema (dataset path,
   base model, hyperparameters) and surface status via workflow
   execution state.
3. Only once a real dataset directory convention exists (e.g.
   `Trainer/datasets/<name>/`) add `training.datasets` to
   `wylde-trainer`'s `ALL_ACTIONS` (namespaced to match its existing
   scheme), enumerate that dir, add a Rust unit test, and wire the
   panel's dataset picker.

**Files:** `wylde-trainer` crate (`ALL_ACTIONS` + handlers), Training
panel crate, `N8N/tools/`.

---

## E — vpn.ddns.status / vpn.ddns.set

**Why deferred:** `wylde-vpn` carries the DDNS provider *library* but has
no action wiring, no last-update-timestamp persistence, and the
RemoteAccess panel reaches VPN over **REST** (`/api/link/config`), not
pipe actions. (Rule 38 skips `wylde-vpn` — it has no discoverable action
registry — so this is purely a feature gap, not a contract violation.)
There was no genuinely clean, low-risk REST seam in `VPN/api.py` to bolt
status/set onto without broader surgery.

**Smallest plan to ship:**
1. `vpn.ddns.status`: read the current DDNS config + the resolved
   endpoint history, plus a **new `last_update` field** persisted by the
   provider whenever it pushes an update (add the field to the provider's
   state file).
2. `vpn.ddns.set`: write the DDNS config and trigger `update()` on the
   provider library, returning the fresh `last_update`.
3. Expose both as REST routes under `VPN/api.py` (matching how
   RemoteAccess already reaches VPN), wire the RemoteAccess panel's DDNS
   card, add tests against the provider's update path.

**Files:** `VPN/api.py`, VPN DDNS provider module, RemoteAccess panel
crate.

---

## F — vpn.adguard.list_rewrites / set_rewrite / delete_rewrite

**Why deferred:** no AdGuard surface exists anywhere in the tree — no
client, no config, no service. This needs a full AdGuard Home API client
plus the infra story for where AdGuard runs and how Wylde authenticates
to it.

**Smallest plan to ship:**
1. Decide the infra: where AdGuard Home runs (alongside the Gateway? on
   the VPN host?) and how credentials are stored (Gateway secrets).
2. Add an AdGuard Home API client (REST: `/control/rewrite/list`,
   `/control/rewrite/add`, `/control/rewrite/delete`).
3. Expose `vpn.adguard.*` verbs over the chosen transport, add tests
   against a mocked AdGuard API, then wire the panel.

**Files:** new AdGuard client module, Gateway secrets, owning service's
action registry, panel crate.

---

## G — images.generate streaming progress — ABANDONED

> **Abandoned 2026-07-22 (#234).** ComfyUI has been removed from Wylde and
> the image Service is parked at
> [PeopleWonder/wylde-images](https://github.com/PeopleWonder/wylde-images).
> There is no image route, no `images.generate` verb, and no Images panel,
> so none of the plan below is actionable. It stays on the page because the
> upstream finding — ComfyUI's one-shot REST path exposes no progress —
> is a real audit result worth not re-deriving.

**Why deferred:** the Gateway forwards image generation one-shot to
ComfyUI (`127.0.0.1:8014`). The audit found ComfyUI exposes **no
per-step progress stream** on the path Wylde uses, so there is nothing
to bridge — this is upstream-dependent. The Gateway already has an SSE
bridge (`Gateway/.../streaming.rs`) that could carry progress frames
*if* ComfyUI gained a progress endpoint.

**Smallest plan to ship:**
1. Confirm/enable a ComfyUI progress source (its `/ws` websocket emits
   `progress` / `executing` messages — the one-shot REST path does not).
2. In the Gateway, subscribe to that websocket for the in-flight prompt
   id and re-emit per-step progress through the existing SSE bridge in
   `streaming.rs`.
3. Wire the Images panel's generate flow to consume the SSE progress
   stream (mirroring how the Models panel consumes the `ollama.pull`
   NDJSON stream).

**Files:** `Gateway/.../streaming.rs`, Gateway image route, Images panel
crate.

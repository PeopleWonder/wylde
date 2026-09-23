# Wylde Passwords — Self-Healing Browser Extension Plan

**Owner:** Wylde User. **Status:** proposal (no code yet). **Drafted:** 2026-05-25.
**Scope:** a fork of the Nextcloud Passwords browser extension
(<https://git.mdns.eu/nextcloud/passwords-webextension>) that ships two
parallel improvements over upstream: (A) a click-to-inject autofill UX
matching native browser password managers, and (B) an AI-driven self-healing
loop that auto-generates per-site fix rules from instrumented failure
reports.
**Companion docs:** [`wylde-rust-migration-master-plan.md`](plans/wylde-rust-migration-master-plan.md),
[`wylde-android-app-plan.md`](plans/wylde-android-app-plan.md),
[`privacy-plan.md`](plans/privacy-plan.md),
[`mcp_surface.md`](mcp_surface.md).

This document is the durable proposal. It does **not** propose code; it
proposes the shape of the project so the Wylde user can decide go / no-go and answer
the open questions that gate implementation. Phase 6 of the Rust migration
(`wylde-harness` tooling) shipped 2026-05-25 — the same day this doc was
drafted — so the backend dependency this project leans on now exists.
"Stable enough to build on" is a judgement call; a few weeks of letting
the tooling layer settle before B3 lands is sensible, but the hard
prerequisite is met. Full harness rust-nativeness arrives later at
**Phase 9** (the pipe surface) and is nice-to-have, not strictly required.
See [§14](#14-companions-and-dependencies) for the explicit cross-references.

---

## Table of contents

1. [Executive summary](#1-executive-summary)
2. [Novelty audit](#2-novelty-audit)
3. [The autofill long-tail problem](#3-the-autofill-long-tail-problem)
4. [Architecture — high level](#4-architecture--high-level)
5. [Idea A — click-to-inject UX patch](#5-idea-a--click-to-inject-ux-patch)
6. [Idea B — self-healing AI agent, extension side](#6-idea-b--self-healing-ai-agent-extension-side)
7. [Idea B — self-healing AI agent, backend side](#7-idea-b--self-healing-ai-agent-backend-side)
8. [Privacy considerations](#8-privacy-considerations)
9. [Validator design](#9-validator-design)
10. [Rules database design](#10-rules-database-design)
11. [Phased implementation roadmap](#11-phased-implementation-roadmap)
12. [Risks (top 5 ranked)](#12-risks-top-5-ranked)
13. [Open questions the Wylde user must decide before B0 starts](#13-open-questions-the-wylde-user-must-decide-before-b0-starts)
14. [Companions and dependencies](#14-companions-and-dependencies)

---

## 1. Executive summary

**What this is.** A fork of the Nextcloud Passwords browser extension —
call the fork `wylde-passwords` for working purposes — that fixes two
specific gaps with one architecture. Upstream Nextcloud Passwords does the
hard part already (talks to the user's Nextcloud server, manages the local
vault, encrypts in-browser, syncs across devices). What it lags on, relative
to Bitwarden or 1Password, is autofill polish: the icon-in-field overlay,
the dropdown of matching credentials, the dispatch of events that modern
single-page apps need to notice a programmatic fill. **Idea A** closes that
UX gap with a few hundred lines of content-script JavaScript. **Idea B**
closes a deeper gap — the long tail of weird sites that no autofill engine
gets right out of the box — by wiring the extension to a Wylde-hosted AI
agent that takes instrumented failure reports, generates candidate fix
rules, validates them in a headless browser, and ships the survivors back
to the extension as a hot-reloaded rule pack.

**The self-amplifying loop, in one sentence.** Every time autofill fails
on a site the Wylde user actually uses, the extension files a structured bug report
to the local Wylde harness; an agent on the local LLM stack proposes a fix,
a Playwright validator runs it against the failing page, and if it works
the rule lands in a per-site rules database that the extension pulls every
hour. The next time the Wylde user hits that site, autofill just works. No human
sees the report unless validation fails.

**Why this is the right post-Rust-migration project.**
Bitwarden's advantage over self-hosted password managers is not its
cryptography — Nextcloud Passwords' crypto is fine — but its operational
muscle: paid developers writing site-specific fixes, plus a community filing
bug reports through a triage pipeline staffed by humans. **An AI agent
running on Wylde's local LLM stack collapses both of those into one
self-hosted loop, for free**, and the whole thing runs inside the Wylde user's trust
boundary so the failure reports never leave the WyldeLink network. Phase 6
of the Rust migration shipped on 2026-05-25 — the tooling registry
(`wylde-harness/src/tooling/`) is now the canonical place to register
internal tools like `passwords.debug_report`. Landing the handler in that
registry gives it a stable home for years; landing it in Python earlier
would have meant writing throwaway code that Phase 6 then deleted. The
timing now is right.

**What this isn't.** It is not a Nextcloud Passwords replacement —
upstream still owns the vault, the sync, the crypto, the server side. It
is not a new MCP tool exposed to external clients — the
`passwords.debug_report` tool is internal-only, never registered on the
MCP surface. It is not a generalised "AI fixes your browser" framework —
it solves exactly one problem (selector and event-dispatch rules for
autofill) where the failure mode is well-bounded and the validator can
give a crisp yes/no on each proposed fix. And it is not a project the Wylde user
should try to start before Phase 6 ships; see the roadmap.

---

## 2. Novelty audit

Before committing to a fork, the upstream extension
(`marius-wieschollek/passwords-webextension`, the maintainer's GitHub mirror
of the canonical `git.mdns.eu` repo) was surveyed to confirm that **neither
Idea A nor Idea B duplicates work already shipping**. They do not. This
section captures the evidence so a future reader does not relitigate the
question.

> **Summary.** Both modifications proposed in this plan are net-new
> functionality relative to the upstream extension as of May 2026. The
> upstream codebase implements (a) a toolbar-popup-driven picker and (b)
> an opt-in auto-fill-first-match setting — neither of which is the
> click-to-inject inline field icon pattern used by Bitwarden, 1Password,
> and Apple Keychain. Site-specific breakage is handled entirely through
> manual user bug reports; no automated detection, instrumentation, or
> repair mechanism exists. The fork therefore adds capability rather than
> rebuilding existing features.

**Idea A (click-to-inject inline picker) — not present upstream.**
The README's complete feature list reads: "Password suggestion / Search /
Browse / Create and update / Multiple Accounts / End-to-End Encryption /
Two-factor authentication / Themes." No inline form UI is listed and none
exists. The interaction model is *toolbar-popup-driven* — click the
extension's icon in the browser toolbar, see matching accounts in a popup,
click an account, the popup script pastes the credential into the page.
There is also an opt-in "automatically paste the first suggestion into
any login form" setting, but that is unconditional auto-paste on page
load, not user-initiated click-on-the-field. The inline field-icon →
dropdown picker pattern that Bitwarden, 1Password, KeePassXC, and Apple
Keychain all use does not exist in this codebase. Issue
[#30](https://github.com/marius-wieschollek/passwords-webextension/issues/30)
("Autofill login forms") is the closest historical thread and is
exclusively about auto-on-load behavior, not click-to-inject UX. The
official documentation for the existing behavior is on the project wiki:
<https://git.mdns.eu/nextcloud/passwords/-/wikis/Users/Browser-Extension/Password-Autofill>.

**Idea B (autonomous self-healing) — not present upstream.**
The upstream extension has no telemetry, no failure aggregation, no rules
database, no agent loop, and no automated repair mechanism. Site-specific
breakage is handled entirely through manual user bug reports filed by hand
(see Issue [#121](https://github.com/marius-wieschollek/passwords-webextension/issues/121),
"Login form not detected"). No instrumentation, no automated fixes, no
per-eTLD+1 overrides, no validator — every breakage requires a human to
write the report, another human (the maintainer) to read and reproduce
it, and the maintainer to write and ship a fix. That serial-human-only
loop is exactly what Idea B replaces with the agent-validator-rules-
database flow described in §6–§10 below.

**Upstream contribution path — meaningful contribution is possible.**
The canonical repo lives at **<https://git.mdns.eu/nextcloud/passwords-webextension>**,
which is a Gitea instance. Gitea has full pull-request support with the
same mechanics as GitHub: register an account, fork on git.mdns.eu, push
a branch, open a PR through the Gitea UI. The GitHub repo is a read-only
mirror — it's the wrong URL to fork from for upstream contribution, but
it has nothing to do with whether PRs are accepted. What actually slows
upstream contribution is **maintainer bandwidth**, not infrastructure:
this is a one-person project with no paid developers and no triage team,
so PRs land on the maintainer's schedule and reviews can take weeks or
months. That bandwidth ceiling is why a fork is still the right default
for Idea B — the self-healing loop is too invasive to gate on a
stranger's review queue — but it does **not** mean Idea A has to be
fork-only. Idea A is small, scoped, and the kind of feature a single
maintainer can land in a sitting; proposing it upstream in parallel to
the fork is a high-EV side-quest. If it lands upstream, the fork drops
that code; if it doesn't, nothing was lost. Recommended timing: open
the PR at the end of B1 once the implementation has lived in the fork
long enough to shake out the edge cases.

**What the fork is therefore doing.** Adding capability, not rebuilding
existing features. This rules out the "you're reinventing what's already
in settings" failure mode and clarifies the scope of B1 and B3–B6 — they
are first-of-kind for this extension family, not parity work.

**Sources used in this audit:**

- Upstream repo & README (feature list): <https://github.com/marius-wieschollek/passwords-webextension>
- Historical autofill thread (Issue #30): <https://github.com/marius-wieschollek/passwords-webextension/issues/30>
- Current breakage-handling thread (Issue #121): <https://github.com/marius-wieschollek/passwords-webextension/issues/121>
- Official autofill wiki: <https://git.mdns.eu/nextcloud/passwords/-/wikis/Users/Browser-Extension/Password-Autofill>

---

## 3. The autofill long-tail problem

Autofill looks trivial — find the password input, type into it, submit
the form — and is in fact one of the more brittle pieces of a browser
extension. The failure modes cluster into five buckets, all of which
account for real-world breakage on real sites the Wylde user uses:

1. **Form not detected.** The extension's content script looks for
   `<input type="password">` and walks up to the enclosing `<form>`.
   Sites that render forms inside a Shadow DOM, lazily mount the form
   after the user clicks a "log in" button, or wrap inputs in a custom
   web component break this. Modern React/Vue/Svelte apps frequently fall
   into the third category — a `Login` component mounts only when the
   route activates, and an extension that scanned once at `DOMContentLoaded`
   has already missed it.

2. **Wrong field filled.** When a form has multiple `<input>` elements —
   a `current-password` field, a `new-password` field, a `confirm-password`
   field — naive selectors put the credential into the wrong one. Banks
   and brokerages with multi-step login (account number then password)
   are the worst offenders.

3. **Dynamic forms.** A form appears, the extension fills it, then the
   site replaces the DOM with a fresh one and the user types into the new
   form by hand. Common on SPAs that swap routes mid-login.

4. **Anti-autofill defenses.** Some sites actively try to defeat password
   managers: rotating field names, fields that demand input one character
   at a time (a hidden setTimeout loop), virtual keyboards, "type your
   password backwards" gimmicks, or input events that get rejected unless
   they pass a `isTrusted === true` check the extension can't satisfy.
   Most of these have known workarounds — they just have to be discovered
   and encoded.

5. **Multi-step flows.** Office 365 takes you through `userPrincipalName
   → "we redirected you" → password → MFA challenge`. Each step is a
   separate page navigation, and an extension that fires on the first
   page sees only the username input. Without a rule that says "this
   domain uses a two-page login flow," the extension fills the username,
   the page navigates, and the user is left typing the password manually.

What Bitwarden does about this, in practice, is two things: a substantial
handcrafted rule database baked into the extension (`equivalentDomains.json`
and friends), plus a triage pipeline where users file bug reports
("autofill doesn't work on bank-of-x.com"), human triagers verify and
write a fix, and the fix ships in the next release. Both arms cost real
money — payroll plus volunteer time — and even Bitwarden has a backlog.

Nextcloud Passwords today does the field-detection well enough but ships
no per-site rules and no triage pipeline — there is no upstream loop that
turns "this site doesn't work for me" into "this site works for everyone
next week." That's the gap. The thesis of this document is that the
gap is now closeable by a single person, because LLMs are now competent
at the narrow task of "look at this HTML snippet and write a better CSS
selector" and headless-browser tooling has matured to the point that
a generated rule can be validated automatically.

---

## 4. Architecture — high level

```
                ┌─────────────────────────────────────────────┐
                │             User's browser (Brave)          │
                │   ┌─────────────────────────────────────┐   │
                │   │   wylde-passwords extension (fork)   │  │
                │   │                                      │  │
                │   │  ┌─────────────┐   ┌──────────────┐  │  │
                │   │  │ content-    │   │ background   │  │  │
                │   │  │ script:     │   │ worker:      │  │  │
                │   │  │ field-      │◄──┤ rules cache, │  │  │
                │   │  │ detection,  │   │ vault auth,  │  │  │
                │   │  │ icon-in-    │   │ Gateway      │  │  │
                │   │  │ field UI    │   │ client       │  │  │
                │   │  └─────────────┘   └──────┬───────┘  │  │
                │   │           ▲               │          │  │
                │   │           │ rule pack     │ failure  │  │
                │   │           │ pull (1h)     │ report   │  │
                │   └───────────┼───────────────┼──────────┘  │
                └───────────────┼───────────────┼─────────────┘
                                │               │
                                │   HTTP over   │
                                │   loopback    │
                                ▼               ▼
                ┌─────────────────────────────────────────────┐
                │   Wylde Gateway (FastAPI, require_local)    │
                │                                             │
                │   GET  /api/passwords/rules?since=<hash>    │
                │   POST /api/passwords/report                │
                └────────────────────┬────────────────────────┘
                                     │ pipe call
                                     ▼
                ┌─────────────────────────────────────────────┐
                │  wylde-harness pipe — tools.run("passwords  │
                │                       .debug_report", …)    │
                └────────────────────┬────────────────────────┘
                                     │
                ┌────────────────────┼────────────────────────┐
                │                    ▼                        │
                │   passwords.debug_report tool handler        │
                │   (Rust, lives in wylde-harness/src/tooling/│
                │     tools/passwords/)                       │
                │                                             │
                │   1. parse + re-validate report             │
                │   2. dedupe against rules DB by domain      │
                │   3. invoke wylde-ollama with prompt        │
                │      template + few-shot examples           │
                │   4. parse candidate rule (JSON)            │
                │   5. enqueue validation job                 │
                └────────────────────┬────────────────────────┘
                                     │ spawn
                                     ▼
                ┌─────────────────────────────────────────────┐
                │   Playwright validator (headless Chromium)  │
                │                                             │
                │   - navigate to failure URL                 │
                │   - inject candidate rule                   │
                │   - attempt autofill with mock credentials  │
                │   - observe success / failure               │
                │   - report back to tool handler             │
                └────────────────────┬────────────────────────┘
                                     │
                              success │ failure
                                     ▼
                ┌─────────────────────────────────────────────┐
                │  Rules database (SQLite in Wylde data dir)  │
                │                                             │
                │  successful → auto-merged, served via       │
                │               /api/passwords/rules          │
                │  failed     → routed to GUI review queue    │
                └─────────────────────────────────────────────┘
```

The loop closes when the extension's hourly rules pull receives the new
rule, refreshes its in-memory cache, and the next autofill attempt on
that domain uses the corrected selector. There is no human in the loop on
the success path — the Wylde user only ever sees the review queue when the
validator could not confirm the agent's proposed fix.

Two design choices worth flagging up front:

- **Transport is Gateway HTTP, not a direct pipe call.** Browser
  extensions can't speak Windows named pipes; they call `fetch()`.
  Gateway already handles `require_local` auth (loopback CIDR allowlist
  per principle #16, see [`WYLDE_ENDPOINTS.md`](../WYLDE_ENDPOINTS.md)),
  so the new routes slot in cleanly behind the existing trust boundary.

- **The tool is internal-only, never on the MCP surface.** External
  callers (Claude Code, future external MCP clients) have no reason to
  trigger `passwords.debug_report`, and exposing it would give them a
  vector to feed adversarial HTML into the local LLM. Register the tool
  in the internal registry, leave it off the MCP whitelist.

---

## 5. Idea A — click-to-inject UX patch

> This pattern is confirmed absent from upstream — see
> [§2 Novelty audit](#2-novelty-audit).

**Goal:** match the autofill UX every user already expects from Chrome's
built-in manager, Bitwarden, 1Password, and Apple Keychain. The user sees
a small icon in the corner of a recognised field; clicking the icon opens
a dropdown of matching credentials; clicking a credential fills the form
and, optionally, submits it.

**Why this matters independent of Idea B.** Even with zero AI work,
matching the click-to-inject paradigm is a substantial quality-of-life
upgrade over Nextcloud Passwords' current UX and removes a class of
phishing-adjacent risk: an extension that fills silently on `load` will
fill into a hidden form a malicious page has injected; an extension that
only fills after an explicit user click cannot. Shipping Idea A alone is
already worth the fork.

**Implementation outline.** Five pieces, all in the content-script
layer:

1. **Field-detection** — keep upstream's logic largely as-is. Walk the
   DOM for `<input type="password">` and a sibling/preceding text or
   email input as the username field. Re-scan on `MutationObserver`
   events to pick up forms mounted after `DOMContentLoaded`. Upstream
   already does some of this; the fork tightens the MutationObserver
   reattach logic.

2. **Icon overlay.** For each detected field, append a positioned
   `<div>` containing an SVG icon inside the field's bounding rect (top
   or right edge, configurable). The overlay is a sibling of the input,
   not a child — children of `<input>` are forbidden — positioned via a
   wrapper or via `position: absolute` relative to a relatively-positioned
   ancestor. Approximately 50–100 lines of JS plus a small CSS bundle.
   Care about z-index and re-positioning on window resize / scroll.

3. **Dropdown of matching credentials.** Clicking the icon opens a
   `<div>` floating beneath the field, populated from the background
   worker's already-decrypted vault. The fork can reuse upstream's
   credential-matching code (domain hierarchy walk:
   `accounts.bank.example.com` → `bank.example.com` → `example.com`),
   adding a "show all credentials" affordance for the long-tail case
   where the user has a credential filed under a sibling subdomain.

4. **Fill + dispatch events.** This is the bit upstream gets *almost*
   right. Setting `input.value = "foo"` does not notify React, Vue, or
   Svelte that the value changed — they track state in their own
   reconciler. The fix is well-known: after setting the value, dispatch
   a synthetic `InputEvent('input', { bubbles: true })` and a
   `Event('change', { bubbles: true })`, both with the native
   `Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype,
   'value').set` trick to bypass React's setter override. This is the
   canonical workaround; copying it from any well-maintained autofill
   extension is the move.

5. **Optional auto-submit.** A per-site setting: "after fill, click the
   form's submit button." Off by default — too easy to misfire on
   multi-step flows — but worth offering because Bitwarden has it and
   power users like it.

**Settings UI.** Three knobs in the extension popup:
- Master per-site toggle (on / off).
- Idle timeout for the unlocked vault (default 15 min, matching
  Bitwarden).
- "Show icon for all sites I have a credential for" vs "show icon for
  any password field."

**Effort estimate.** ~200–400 lines of new content-script JS plus a
small CSS file. About a week solo, including testing on the ten or
fifteen sites the Wylde user uses most. The risk here is low: nothing in this
phase touches the vault, the server protocol, or the build pipeline
beyond rebundling.

---

## 6. Idea B — self-healing AI agent, extension side

> No equivalent loop exists in upstream — see
> [§2 Novelty audit](#2-novelty-audit).

The extension's job in the self-healing loop is twofold: **detect that
autofill failed**, and **assemble a structured report safe enough to
send**. The hardest part of this phase is the privacy filter, which gets
its own section ([§8](#8-privacy-considerations)); the rest is plumbing.

**Failure detection.** Three triggers, in order of confidence:

1. **No form detected.** The user clicked the toolbar icon (or invoked
   the extension via keyboard shortcut) on a page where the content
   script never reported a password field. High confidence: the user
   wanted autofill, the extension had nothing to offer.

2. **Fill rejected.** The extension filled a field; within N seconds
   (default 5), the field's value reverted to empty or to a placeholder
   that doesn't match the credential. Medium confidence — could be the
   user typing over the autofill, but worth reporting if the user then
   clicks the toolbar icon again on the same page within a short window
   (which signals "that didn't work").

3. **User explicit feedback.** A "Report autofill failure" menu item in
   the popup. Highest signal of all because the user is explicitly
   asking; lowest volume because users mostly won't click it. Worth
   shipping for the times they do.

**Report contents.** The structured payload, after redaction:

- `domain` (string) — the page's eTLD+1, derived from a Public Suffix
  List lookup. Not the full URL.
- `url_path` (string) — only the path component, query string stripped.
  Many login pages are at `/login` or `/auth/sign-in`; the path is
  diagnostic. Query strings can carry session tokens and are dropped
  whole.
- `extension_version`, `browser`, `browser_version` — for filtering
  reports across releases.
- `selectors_tried` (array of strings) — which CSS selectors the
  extension attempted and which failed. Cheap, no PII.
- `failure_type` — one of `no_form_detected`, `fill_rejected`,
  `submit_failed`, `user_reported`.
- `form_html` (string) — the outerHTML of the smallest enclosing
  `<form>` (or the smallest `<div>` containing the password field if no
  form), with **every `<input>`'s `value` attribute stripped**, every
  `type="password"` field blanked, and every field whose `name` or `id`
  matches a sensitive-pattern regex blanked. If the redactor's
  confidence is low (more than 20% of input values would have survived),
  drop the entire `form_html` and ship `null` instead — the agent can
  still propose a fix from selectors alone.
- `page_title` (string) — useful for the agent to spot multi-step flows
  ("Step 2 of 2" gives a strong hint).

Notably absent: cookies, localStorage, sessionStorage, any other form on
the page, the full HTML, any element outside the form's bounding box.

**Submitting reports.** A single POST to `/api/passwords/report` on the
Gateway, bearer auth via the extension's pre-registered device token
(same machinery the future mobile app uses, per
[`wylde-android-app-plan.md`](plans/wylde-android-app-plan.md) §5). The
extension queues reports if the Gateway is unreachable and drains the
queue on reconnect, with a per-domain dedupe so a runaway form doesn't
flood the queue with hundreds of identical reports.

**Pulling rules.** A separate GET to `/api/passwords/rules?since=<hash>`,
fired on extension startup and every hour after. The response is a delta
of rules created since the supplied content hash; the extension applies
them to an IndexedDB-backed rules cache that the content script consults
during field-detection.

**User-facing controls.** Three additional settings, on top of those
from Idea A:

- "Send failure reports to my Wylde instance" — default **on for the
  self-hosted fork**, hard-default off for any future public-distribution
  build (see [§13](#13-open-questions-the-wylde-user-must-decide-before-b0-starts)).
- "Show me each report before it sends" (debug mode) — default off; on
  flips reports into a confirmation flow that displays the redacted
  payload in a popover for the user to approve or cancel.
- "Pull rules updates" — default on; off freezes the extension's rule
  pack at its current state.

**Effort estimate.** Roughly 300–500 lines of new JS for the
instrumentation, report assembly, queue, and rule-pull client. Most of
the budget goes into the redaction logic and the dedupe heuristics, not
the network code. Plan for two weeks solo because adversarial test cases
(sites that try to leak PII through field names, sites with thousands of
inputs, sites with deeply nested forms) take time to enumerate.

---

## 7. Idea B — self-healing AI agent, backend side

The backend is where the loop actually closes. Three pieces: the tool
handler, the agent prompt, and the persistence layer. The validator
([§9](#9-validator-design)) and rules DB ([§10](#10-rules-database-design))
each get their own section because each is substantial on its own.

**The tool handler.** A new built-in tool registered in
`rust/crates/wylde-harness/src/tooling/tools/passwords/debug_report.rs`,
named `passwords.debug_report`. The handler signature accepts a
deserialised report (the same JSON the Gateway received), and its job is:

1. **Re-validate inputs.** Cheap defence: re-run the redactor on the
   server side, refuse the report if it still contains anything
   matching the sensitive-pattern regex. Belt-and-braces against an
   extension bug.
2. **Dedupe.** Hash `(domain, failure_type, selectors_tried)` and
   discard reports identical to one received in the last hour.
3. **Rate-limit.** Per-domain ceiling of N reports per hour (default
   10) to prevent a runaway-failing site from monopolising the LLM.
4. **Look up existing rules.** If the domain has rules, include them
   in the prompt — the agent's job becomes "refine these," not "invent
   from scratch."
5. **Build the prompt and call `wylde-ollama`.** Template described
   below.
6. **Parse the response.** Expect strict JSON; on parse failure, log
   and route to the human-review queue without further processing.
7. **Enqueue validation.** Hand the candidate rule to the Playwright
   validator (§8). The tool returns immediately; validation is async.

This lives in the Phase 6 tool registry, which is exactly the surface
the Rust migration plan creates for in-tree tools
([`wylde-rust-migration-master-plan.md`](plans/wylde-rust-migration-master-plan.md)
§Phase 6). No new pipe, no new top-level crate — one new tool family.

**The prompt template.** A system prompt that establishes role
("You are an expert at writing CSS selectors for HTML login forms")
followed by:

- The current rules for the domain (if any).
- Three few-shot examples covering the most common failure patterns —
  e.g. one for "selector picks the wrong field," one for "form mounts
  in a Shadow DOM," one for "submit handler requires the dispatch of a
  trusted-style event."
- The failure report itself.
- Strict output schema: a single JSON object with fields
  `selector_username`, `selector_password`, `submit_strategy`,
  `notes`. Empty strings are allowed; the JSON shape must hold.

Choice of model is deferred to runtime — `wylde-ollama` picks based on
its catalog. Realistically a 7B-class model is the floor; a 13–34B model
gives noticeably better selector reasoning. The exact pick is a tunable.

**Persistence.** Successful (validated) rules land in SQLite at
`data/passwords-rules.db` (Wylde-standard data directory), schema in
[§10](#10-rules-database-design). The Gateway's `/api/passwords/rules`
route reads from this DB. Failed rules land in a separate table with the
LLM's full response and the validator's failure log attached — the GUI
dashboard reads from here to surface the review queue.

**No N8N involvement.** The temptation is to model the whole loop as an
n8n workflow because n8n is the existing automation runtime. Resist:
n8n's job is user-visible automation the Wylde user edits in a flow editor; the
self-healing loop is an internal background process with no human
intervention on the happy path. It belongs in the harness, not in n8n,
and the harness's tool-runner gives it the right primitives (queueing,
retry, observability) for free.

---

## 8. Privacy considerations

This is the highest-stakes section. A password-manager extension that
ships HTML snippets off the user's machine is exactly the wrong thing if
done sloppily — even a single leak of one filled username via a
malformed redaction would be unrecoverable. The mitigations below
compose; defence in depth is the explicit posture.

**What gets sent.** The exhaustive list (and only this list):

- Page eTLD+1 domain.
- URL path (no query, no fragment).
- The `outerHTML` of the smallest form-bounding element, with all
  input values stripped.
- Selectors the extension tried and the failure type.
- Browser, extension version, page title.

**What does NOT get sent.** Equally exhaustive:

- Passwords. `type="password"` value attributes are stripped, full stop.
- Usernames. Even though the username isn't strictly secret on most
  sites, the rule is to treat all input values as PII because some
  sites use phone numbers, account numbers, or email addresses as the
  login identifier. The redactor strips the `value=` attribute from
  every `<input>`, of every type, before serialisation.
- Cookies, localStorage, sessionStorage — never touched, never read.
- Any DOM outside the form's bounding box. The redactor extracts the
  form first, then runs on a detached copy; sibling DOM cannot leak
  in.
- The query string and fragment of the URL.

**Redaction strategy.** Layered, fail-closed:

1. **Always blank `<input>` value attributes** for every input regardless
   of type. This is the baseline.
2. **Sensitive-name regex.** Drop the field entirely (replace with a
   placeholder `<input data-redacted="sensitive-name" />`) if the
   field's `name`, `id`, or `aria-label` matches
   `/ssn|account|card|cvv|cvc|otp|pin|tax|nino|sin|secret|api[_-]?key|token/i`.
   The regex is conservative; false positives are fine.
3. **Confidence threshold.** Count: how many `<input>`s in the
   pre-redaction form, how many made it through with their structure
   intact. If the ratio of dropped-fields-to-total exceeds 20%, drop
   the entire `form_html` from the report. Reports with no form_html
   still convey useful information (selectors tried, failure type,
   domain).
4. **Server-side re-run.** The tool handler re-runs the same redactor
   on the received payload and refuses the report if anything still
   matches the sensitive-name regex. Catches bugs in the extension-side
   redactor.

**Trust model.** Reports go to one place: the Wylde user's own Wylde instance,
reachable only over loopback or the WyldeLink VPN tunnel. They never
hit a public service; they never leave the WyldeLink network; they
never reach Anthropic or any other third-party LLM (the agent runs on
`wylde-ollama`, locally, on the Wylde user's hardware). If the Wylde user ever decides
to share the extension publicly ([§13](#13-open-questions-the-wylde-user-must-decide-before-b0-starts)),
the default for non-owner users must flip to "reports off" and the
target endpoint must be user-configurable per-instance, never default to
a public collector.

**User audit.** Two surfaces:

- Debug-mode confirmation popover (described in §5) — for users who
  want to inspect each report before it sends.
- A "reports sent" log in the extension popup, retained for 30 days
  client-side, showing domain, timestamp, redacted payload. Lets the
  user spot any leak retrospectively.

**Crypto note.** Reports in transit use TLS (Gateway's existing posture);
reports at rest in the rules DB don't contain PII by construction so
they're stored plain. The vault itself remains untouched — upstream NC
Passwords' E2EE is intact; nothing in this project decrypts a credential
for any purpose other than autofill.

---

## 9. Validator design

The validator answers one question: "If I apply this candidate rule to
the failing page, does autofill work now?" A yes promotes the rule to the
production DB; a no routes it to the review queue.

**Runtime.** Headless Playwright (Chromium by default, Firefox as a
secondary target). Playwright's per-browser-context isolation gives the
right primitives for parallel validation without cross-contamination.

**Inputs.** The candidate rule (JSON) plus the failure report (for the
URL and page-title hint). The validator does **not** receive the
extension's redacted `form_html` — it re-fetches the live page through
the headless browser so the validation runs against current DOM, not
against the snapshot from when the user hit the failure.

**Flow.**

1. Launch headless browser, fresh context.
2. Navigate to the failure URL.
3. Wait for `domcontentloaded` plus a short settle (1500ms) for SPAs to
   mount.
4. Inject the candidate rule into the page as if the extension had loaded
   with it active.
5. Fire the same autofill code path the extension uses, against mock
   credentials: `autofill-test@aliasdomain.test` / `Validator-Pass-1234!`
   (the password complies with most sites' rules and is recognisable in
   logs if it ever leaks).
6. Detect outcomes:
   - **Success:** form accepts input, no validation error appears,
     submit attempt proceeds far enough to navigate or to issue an XHR
     against an auth endpoint (the validator returns "success" without
     waiting for the auth response, since mock credentials will always
     401).
   - **Failure-input-rejected:** the value attribute is empty 500ms
     after fill, or a validation error appears.
   - **Failure-no-effect:** input filled, but submit attempt produces no
     network activity within 5s.
7. Report outcome back to the tool handler.

**Concurrency.** Cap parallel validations at 2 by default. Headless
Chromium is RAM-hungry (~200MB per context) and the LLM is the rate
limit anyway; serialising further is fine.

**Timeout.** 30s per validation. Above that the validator gives up and
reports "indeterminate," which routes to the review queue just like
explicit failure — no rule auto-promotes on indeterminate.

**Auth-state limitation.** Headless validation tests *can the form
accept input?*, not *can the user actually log in?*. A site that requires
a pre-existing session to even display the password field can't be
validated this way; the agent can still propose a rule, but it ships as
`source: "ai-agent-unvalidated"` with a low-confidence flag. The
extension consumes confidence flags as a tiebreaker between competing
rules. Most sites do not gate the login form on a session, so the limit
bites less often than it sounds.

**HAR-replay path (future enhancement).** For sites that genuinely can't
be reached from the validator (intranet, geo-restricted), the extension
could ship an HTTP Archive of the failing page along with the report,
and the validator could replay it locally. Privacy-fraught (HAR
captures cookies and request bodies wholesale), so this is deferred to a
post-B7 enhancement, never default-on.

**Host.** Runs in the harness as a sibling subprocess to other Phase 6
tools — invoked by the `passwords.debug_report` tool, lives in the same
crate tree. Not a separate service, not a new pipe; resource limits are
inherited from the harness process.

---

## 10. Rules database design

**Schema** (SQLite, single file at `data/passwords-rules.db`):

```
CREATE TABLE rules (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  domain          TEXT    NOT NULL,
  version         INTEGER NOT NULL,
  rule_type       TEXT    NOT NULL,    -- 'selector_username' | 'selector_password' | 'submit_strategy' | 'event_dispatch'
  selector        TEXT,                 -- CSS selector or strategy literal
  field_role      TEXT,                 -- 'username' | 'password' | 'mfa' | 'submit'
  confidence      TEXT    NOT NULL,    -- 'high' | 'low' (low = unvalidated)
  source          TEXT    NOT NULL,    -- 'ai-agent' | 'human-review' | 'manual'
  created_at      INTEGER NOT NULL,    -- unix epoch
  validated_at    INTEGER,             -- nullable
  retracted_at    INTEGER,             -- nullable; non-null = soft-deleted
  rule_hash       TEXT    NOT NULL UNIQUE
);

CREATE TABLE review_queue (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  domain          TEXT    NOT NULL,
  candidate_rule  TEXT    NOT NULL,    -- the JSON the LLM produced
  failure_reason  TEXT    NOT NULL,    -- validator output or parse-fail message
  report_id       TEXT    NOT NULL,    -- ties back to the originating failure report
  created_at      INTEGER NOT NULL
);

CREATE TABLE reports (
  id              TEXT    PRIMARY KEY, -- ULID
  domain          TEXT    NOT NULL,
  payload         TEXT    NOT NULL,    -- the redacted JSON
  created_at      INTEGER NOT NULL
);

CREATE INDEX idx_rules_domain_active ON rules(domain) WHERE retracted_at IS NULL;
```

**Versioning.** Each rule's `rule_hash` is a content hash over
`(domain, rule_type, selector)`. The `/api/passwords/rules?since=<hash>`
endpoint returns all rules created since the supplied hash plus the
identity of any rules retracted since then; the extension applies the
delta to its IndexedDB cache. New extension installs pull `?since=` with
no argument and get the whole rule pack.

**Rollback.** If a rule has been live for less than 7 days and two or
more failure reports for the same domain reference the selector that
rule introduced, the tool handler automatically retracts the rule
(`retracted_at = now()`) and routes the original report (plus the new
failure) back through the agent for a second attempt. the Wylde user sees the
retraction in the GUI dashboard's audit log. Older rules don't
auto-retract — a rule that worked for a month and then breaks more
likely signals a site redesign than a bad rule, and the new failure
report will trigger a fresh proposal of its own.

**Rule TTL.** Open question (see [§13](#13-open-questions-the-wylde-user-must-decide-before-b0-starts)).
Default for B5 is "no TTL, rules live forever until retracted." A
plausible later refinement: rules age into a `low_confidence` state
after N months without a reinforcing autofill success, and the agent
re-validates them on a schedule. Defer until the rules DB has enough
data to know what N should be.

**Serving.** Gateway route `GET /api/passwords/rules` does a single
indexed query on `rules` filtering by `retracted_at IS NULL` and
ordering by `created_at` for the delta semantics. Cheap; cache-friendly;
no LLM involvement on the read path. The route should support an
`If-None-Match` ETag derived from the max `created_at` to keep the
hourly polls from the extension nearly free.

---

## 11. Phased implementation roadmap

All durations are solo-developer estimates with normal evening hours;
double if the Wylde user is also juggling Wylde core work. **B0 can start
whenever**; **B3 depends on Rust Phase 6 (tooling) being shipped**,
which it now is (2026-05-25). In practice the calendar gating left on
this project is "the Wylde user has a free week," not Wylde-migration progress.

| Phase  | Scope                                    | Duration | Depends on              |
| ------ | ---------------------------------------- | -------- | ----------------------- |
| **B0** | Fork groundwork                          | 1–2w     | nothing                 |
| **B1** | Click-to-inject UX (Idea A in full)      | 1w       | B0                      |
| **B2** | Instrumentation (no backend yet)         | 1w       | B0                      |
| **B3** | Backend tool + agent (no validation yet) | 1–2w     | Rust Phase 6 (✅ shipped) |
| **B4** | Playwright validator                     | 1–2w     | B3                      |
| **B5** | Rules distribution                       | 1w       | B4                      |
| **B6** | Rollback + GUI dashboard                 | 1w       | B5                      |
| **B7** | Polish (open-ended)                      | n/a      | B6                      |

**Phase B0 — Fork groundwork (1–2w).** Clone the upstream extension,
get the build pipeline reproducing locally, sign with the Wylde user's key for
sideload distribution, write down a reproducible release process (one
script that produces `.crx` and `.xpi` from a clean checkout). No
functional changes. Confirm the existing test suite (if any) runs.
Establish a `wylde/` directory in the fork for new code so upstream
merges remain mechanical.

**Phase B1 — Click-to-inject UX (1w).** All of [§5](#5-idea-a--click-to-inject-ux-patch).
Ships as a usable improvement on its own; if the Wylde user stops here he still
has a meaningfully better extension than upstream.

**Phase B2 — Instrumentation (1w).** Failure detection, structured
report assembly, the redactor (and the redactor's tests, including
adversarial sites), the report queue. Backend is stubbed to a
`console.log` so the whole instrumentation pipeline can be exercised
without a Wylde tool yet. Gates: zero PII leaks in the redactor's
tests, including the adversarial set.

**Phase B3 — Backend tool (1–2w).** Rust Phase 6 shipped 2026-05-25
([[wylde-phase6-shipped]]) — the tooling registry, runner, and tier
gate are live with 10 active tools. The new addition: the
`passwords.debug_report` tool in `rust/crates/wylde-harness/src/tooling/
tools/passwords/`, the prompt template, the few-shot examples, the
Gateway routes (`POST /api/passwords/report` and a stub
`GET /api/passwords/rules`). Mark `destructive: false` in the registry
entry — the tool only reads page snippets and writes to its own
isolated DB, not anything the tier gate cares about. The agent's
output goes straight to the review queue — no validation, no
auto-promotion. Useful internal checkpoint: are the rules the agent
proposes even shaped correctly? Eyeball 50 from real failures.

**Phase B4 — Validator (1–2w).** Playwright integration, mock
credential flow, success/failure detection, concurrency limits,
timeouts. Wire validator outcomes into the rules DB schema. After B4
the loop is closed for the first time end-to-end.

**Phase B5 — Rules distribution (1w).** The extension-side rules cache,
the hourly pull, the delta semantics, the ETag caching on the server.
First time the extension actually consumes its own outputs.

**Phase B6 — Rollback + monitoring (1w).** Auto-retraction logic, GUI
dashboard surfacing the review queue and audit log (lives in the same
gpui-native `wylde-gui` desktop app as the rest of Wylde admin — one more
panel, not a new app).

**Phase B7 — Polish (open-ended).** Per-site overrides, manual rule
editing in the dashboard, Firefox build, Safari build (if the Wylde user decides
to target it). HAR-replay validation. Multi-user share-back if the
extension ever goes public.

**Total: ~7–10 weeks solo.** With Phase 6 already shipped, all phases
are unblocked from a dependency standpoint; the only remaining gate is
the Wylde user's calendar. B0–B2 (~3–4 weeks) is straightforward extension
hacking; B3–B6 (~4–6 weeks) is where the AI/validator/distribution
loop comes together, and is the bulk of the engineering interest.

---

## 12. Risks (top 5 ranked)

1. **False-positive fixes break other sites.** A rule that works for
   `bank-of-x.com` accidentally captures `bank-of-y.com` because the
   selector was generalised too aggressively. *Mitigations:* every rule
   is keyed strictly to one eTLD+1 with no wildcards; the validator
   re-runs the rule before promotion; rollback ([§10](#10-rules-database-design))
   auto-retracts rules whose introduction precedes a fresh failure burst.
   *Residual risk:* moderate — a sufficiently-confused agent can produce
   a brittle rule that validates once and breaks on the next site update.
   The seven-day rollback window plus user-visible audit log are how
   the Wylde user stays in the loop.

2. **Privacy leak in reports.** The redactor misses a sensitive input
   value, the agent prompt embeds it, the LLM emits it back, and now a
   credential lives in `data/passwords-rules.db`. *Mitigations:* the
   four-layer redactor in [§8](#8-privacy-considerations), the
   server-side re-run, the user audit dashboard, debug-mode preview
   before send. *Residual risk:* high stakes, low likelihood — the
   probability of any individual leak is low, but a single leak is
   catastrophic. This risk justifies the entire posture of "default-on
   only for the Wylde user's own instance; any public version forces
   user-configurable endpoint and reports-off-by-default."

3. **Validator coverage gaps.** Headless browser state ≠ real user
   state. Sites that require pre-existing auth (intranet apps,
   geo-locked services, sites behind CAPTCHA) can't be validated
   automatically. *Mitigation:* the `low_confidence` rule path — agent
   output ships unvalidated with a confidence flag, the extension uses
   it but flags the autofill outcome for explicit user feedback
   ("did that work?"). *Residual risk:* low — most sites the Wylde user actually
   uses are validate-able; the unvalidated path is a long-tail safety
   valve.

4. **Maintenance drift from upstream.** Nextcloud Passwords keeps
   shipping; the fork has to track. The wylde-only code in `wylde/`
   isolates the merge surface, but conflicts in shared files (manifest,
   build config, MV3 migration) are unavoidable. *Mitigation:* a
   monthly upstream-merge cadence so conflicts stay small; a regression
   test pack against the ten or fifteen sites the Wylde user uses most so a bad
   merge is caught immediately. *Residual risk:* the standard fork
   burden — high if the Wylde user drops upkeep for six months, low if monthly
   merges happen.

5. **LLM rule-quality variance.** Local Ollama models are decent at
   selector reasoning but not perfect — particularly weaker at understanding
   custom web components and at distinguishing structurally-similar
   fields. *Mitigation:* the few-shot prompt covers the common failure
   modes, the validator catches most bad outputs, and the review queue
   absorbs the rest. *Residual risk:* low; the rate-limit and
   review-queue path keep the cost of any individual bad output bounded.

---

## 13. Open questions the Wylde user must decide before B0 starts

These need answers before B0 to avoid retrofits later. They are ordered
by how expensive a wrong call is.

1. **License of the fork.** Upstream Nextcloud Passwords is AGPL-3.0.
   AGPL-3.0 means any network-deployed derivative must offer source to
   network users. For the Wylde user's personal-use fork on his own machine the
   obligation is essentially moot (he is the network user); for any
   public release it bites. *Decision needed:* keep AGPL (safest,
   compatible with upstream), or evaluate dual-licensing if the Wylde user ever
   wants to publish a closed extension store binary. *Recommendation:*
   keep AGPL, it costs nothing for personal use and keeps upstream-merge
   paths open.

2. **Public sharing of the extension and/or rules database.**
   Three states are possible:
   - (a) Personal-use only, never published. Simplest; no review,
     no distribution overhead.
   - (b) Extension published (Chrome Web Store / Mozilla Add-ons),
     rules DB stays private per-instance.
   - (c) Extension published AND rules database shareable (opt-in
     "share back" of validated rules to a federated commons).
   Each adds privacy and ops surface. *Recommendation:* start at (a)
   for B0–B6; reconsider after dogfooding for six months.

3. **Browser targets.** Brave only (Chromium), or Firefox too, or
   Safari? Each browser is its own build pipeline; MV3 migration
   pressure differs across them (Safari is strictest, Firefox slowest).
   *Recommendation:* Brave/Chromium for B0–B7; Firefox in a post-B7
   wave; Safari only if the Wylde user commits to an iOS roadmap.

4. **Validator host.** Inside the harness (as designed above), or as a
   separate service the Wylde user starts on demand? Inside-harness keeps the
   architecture simple and inherits the harness's lifecycle; separate
   service makes resource limits explicit. *Recommendation:* in-harness
   for B4; reconsider only if Playwright memory pressure becomes a
   problem in practice.

5. **Rule TTL.** Do rules expire? *Recommendation:* no expiry for
   B5–B6; revisit after the rules DB has six months of data to inform
   a sensible TTL.

6. **Failure-report transport at first-pair.** The extension needs a
   bearer token to talk to Gateway. Where does it come from? Two
   options: (a) the extension reads a token from a file the Wylde user drops in
   on install (manual, simple); (b) the extension performs a
   browser-side pairing flow with the desktop GUI analogous to the
   mobile pairing flow in
   [`wylde-android-app-plan.md`](plans/wylde-android-app-plan.md) §5.
   *Recommendation:* (a) for B3 — it's a five-minute setup — and
   reconsider (b) if the extension ever ships beyond the Wylde user's own
   machines.

---

## 14. Companions and dependencies

**[`wylde-rust-migration-master-plan.md`](plans/wylde-rust-migration-master-plan.md)
— Phase 6 (tooling) is the explicit foundation, and it shipped
2026-05-25.** The `passwords.debug_report` tool lives in
`rust/crates/wylde-harness/src/tooling/tools/passwords/`, registered
through the same `tooling::registry::global()` machinery Phase 6
introduced. Phase 6 bumped `wylde-harness` to 148 tests with the
registry, runner, and tier gate live — the surface this project
depends on exists in its permanent form. **Phase 9** (the Rust pipe
surface) is when the whole harness is rust-native end-to-end —
preferable, not strictly required, for B3 onwards.

**[`wylde-android-app-plan.md`](plans/wylde-android-app-plan.md)
— mobile autofill is the next problem after this one.** Android system
autofill is a different beast (the OS arbitrates, not the browser; apps
opt in to the autofill framework) and lives outside this document's
scope. The Android plan should grow a §7.x subsection acknowledging
that Wylde-side autofill on phones will eventually need to bridge
through the same Gateway routes this project introduces — at minimum
`POST /api/passwords/report` (the failure-reporting plumbing is
substrate-agnostic). Whether the Android app implements its own
self-healing loop or just consumes the rules DB the desktop extension
populates is a question for the Android plan, not this one.

**[`privacy-plan.md`](plans/privacy-plan.md) §3.3 — this extension is the
"in progress" half of that line item.** §3.3 currently reads "Strong
passwords + password manager. ✅ In progress — Nextcloud Passwords being
set up." This document is the proposal for the *enhanced* half: the
extension fork plus the self-healing loop. When B5 ships, §3.3 can be
updated to reference this document and flip the in-progress marker to
shipped.

**[`mcp_surface.md`](mcp_surface.md) — explicitly negative
relationship.** The `passwords.debug_report` tool is **not** exposed on
the MCP surface. External MCP clients (Claude Code, future Claude.ai
integrations) have no reason to file fake autofill failures, and
allowing them to do so would create a feed for adversarial HTML into
the local LLM. When this project lands, `mcp_surface.md` needs a
one-line note in its "internal-only tools" section confirming the
exclusion.

---

**Doc end.** Next action when the Wylde user decides to start: answer the six
open questions in [§13](#13-open-questions-the-wylde-user-must-decide-before-b0-starts),
then queue B0 for whenever a free week appears. With Phase 6 shipped
on the same day this doc landed, every phase B0–B7 is unblocked from
a dependency standpoint; calendar is the only gate.

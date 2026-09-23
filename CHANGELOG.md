# Changelog

All notable changes to Wylde are recorded here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org/), and pre-1.0 alphas may break between builds
(see [`docs/branch-and-release-policy.md`](docs/branch-and-release-policy.md) §3).

<!--
Maintenance: this changelog is hand-curated (deliberately richer than an
auto-generated bullet list). For any user-facing change, add an entry in the
matching section of the current unreleased version. `tools/changelog-draft.sh`
seeds a draft from Conventional Commits since the last tag — edit it into
narrative form.
Release lines: experimental builds ship 0.1.x (Beta channel); the stable gate is
0.2.0 (Stable channel), cut only on the maintainer's say-so.
State: the workspace version is now 0.2.0-beta.1, but it is NOT yet tagged/released
(that is #38, the maintainer's separate say-so). The section below is therefore
headed "[0.2.0-beta.1] — unreleased"; on release, replace "unreleased" with the tag
date and start a fresh [Unreleased] section above it for later work.
-->

## [0.2.0-beta.1] — unreleased

**Wylde 0.2.0-beta.1 is the first pre-release of the modern, all-Rust stack** (the stable
`0.2.0` cut remains gated on the maintainer's separate say-so, #38). The only
earlier tag, `v0.1.0-alpha.1` (2026-06-04, a GitHub *pre-release* on the Beta channel),
predates the full-Rust cutover entirely — it shipped the gpui desktop rebuild while the
runtime beneath it was still Python. Everything between that tag and this one was built in
the open on the `develop` line and is only now judged ready to carry a 0.2 version, so
this pre-release absorbs an unusually large body of work.

The headline changes: the **full-Rust cutover** (every Python runtime component ported to
Rust and its source deleted); a local-first **memory system** (short-term, long-term, and
reflection across the conversation, workspace, and long-term scopes); the **Thought Bubble
System** with pre-turn structural retrieval; a workspace **knowledge graph** with a native
gpui graph panel and an in-app IDE; **BM25 lexical retrieval + RRF fusion**; a definitional
**concept hierarchy** and a **concept-routing** decision layer (both isolated, default-off,
and byte-identical when disabled); and an **agentic reasoning tier** shipped `enabled:
false` as an opt-in experiment. Wrapping all of it is the **enforcement layer** whose
absence let the alpha ship broken — the GUI panel-walk (L7), the launch-and-verify preflight
and its commit-bound receipt, the benchmark regression gate, version-consistency (G7), and
the license/advisory gates — now wired so the class of defect that shipped before is blocked
rather than merely documented.

The entries below are long because the release is, and they are written to be read: each
says what changed and why it mattered. The release date is stamped when this version is
tagged on the maintainer's say-so (`docs/branch-and-release-policy.md` §5).

### Added

- **Every clicked live-handler GUI control is now *asserted*, not just clicked (refs #247, closes #276).** The #247 gate proved each control *does something*; a handful of controls whose whole effect was a fire-and-forget handoff were only clicked (declared `external_effect`) because the walk's oracle had no channel for that kind of effect. Two seams close the gap. A new **`emit_probe`** oracle channel (mirroring `nav_probe` / `focus_probe`) records a `cx.emit(..)` a control hands to a parent the walk never mounted; and a walk-suppressible **`open_url`** seam (mirroring `native_file_dialog`) records-and-suppresses a browser open instead of spawning one. With them: the **dependency-tree canvas** now asserts its `TreeEvent::Selected` emit (the fixture parks a real node under the click so there is a genuine selection to observe — its walk moved in-crate to reach the private camera); the **Chat markdown link** routes through `open_url` and asserts the exact target URL; and the **three Chat native file dialogs** dropped their `external_effect` and now assert on the recorded dialog-request channel (they were assertable all along, only conservatively suppressed). The only controls left non-asserted are declared true-no-ops — a `stop_propagation` swallow, the already-selected segment of a radio group (its siblings assert the real switch), and an inert breadcrumb crumb — each kept with a reason. Result: **zero clicked-but-unasserted live-handler controls** in the GUI.

- **The Devices panel is now control-walked (refs #247, part 2 — deferred-walk follow-up 1).** The first of the deferred stateful-panel walks. Devices carries four occluding cards (pairing, revoke-confirm, tier-escalation-confirm, rotated-token) plus a mutually-exclusive empty state, driven with one `.state()` each; the pairing card is opened through the real `start_pairing` flow.
  It surfaced a genuine edge for the effect-oracle: the tier row is a **segmented control**, and clicking the pill for the tier a device is *already on* is a deliberate no-op (`click_tier` early-returns) — a radio button clicking its own selection. That is correct behaviour, not a dead control, but a click-walk cannot tell the two apart. The fixture gives the device a tier outside the known set so no pill is "current" and every pill click exercises a real change (a `set_tier` call or the destructive-tier confirm); the click-the-active-pill no-op stays covered by the panel's own unit test. Documented in the walk.
- **The Models panel — the most stateful in the tree — is now control-walked (refs #247, deferred-walk follow-up 2).** Its thirteen controls live across nine render states (default pull bar, active search, pull dialog, pull-in-flight, delete-confirm, two separately-gated HuggingFace strips, the privacy-gated catalog row, a recommendation card, and the unreachable/retry branch), each driven by a `.state()`.
  Two mechanisms were needed. The pull input is armed once at mount (its submit no-ops on empty) and is never re-set per click, because its on-change subscription clears the staged `hf_selected` — so the state that changes the input is ordered **last**, and the HF-detail state matches `pull_selected` to the query rather than touching the input. And the "Search HuggingFace" catalog row is gated by a process-global privacy pref, seeded in-memory via a new dev-only `wylde_gui_pipe::privacy_prefs::set_cache_for_test` (behind `test-support`, requested only from `[dev-dependencies]`, so the shipped Shell has no cache-seed path) — cleaner than `persist`, which would write a stray `privacy.json` under CI.
- **The Chat panel — the largest surface in the tree — is now control-walked (refs #247, deferred-walk follow-up 3).** Chat renders one `ChatPanel` whose controls come from two files (`chat_panel.rs` chrome + `composer_ui.rs` composer), and painting the panel paints both at once — so they are walked together in a single mount rather than split across two fixtures that would each have to arm the other's preconditions and fingerprint the other's fields. Fourteen `.state()`s cover the chrome (send/stop streaming, the processing indicator, working-memory clear, the conversation switcher + inline delete-confirm) and the state-gated composer (per-word + context chips, the floating thought-bubble strip / expanded card / right-click menu, the disambiguation dropdown, the anchor offer, the 3-tier ignore menu, the curate-before-send popover, the Ctrl+P palette).
  The one hard case was the **processing indicator**, the only control that lives inside the virtualized message `list()`: it paints only on the in-flight assistant *tail* bubble, and its expand chevron — its sole handler — is attached only when the turn has logged detail (`has_detail`). So the state installs exactly one streaming-assistant message as the tail (unconditionally: a conversation auto-loaded on mount would otherwise leave a non-streaming tail and the indicator would never paint), resets the reconciler to it, seeds a logged step, and the fingerprint tracks the `expanded` flag the chevron toggles.
  The walk flagged two **mis-applied** controls (mis-wired in the enforcement sense, not dead handlers): `chat-bubble-strip` (the floating bubble *layer* — a tether canvas the id exists to anchor) and `chat-bubble-card` (the positioned drill-in panel) were routed through `control()` in the migration but have no click handler of their own — their bubbles / 📌 pin / ✕ exclude / "view in graph" children are the real controls. Both now carry the `// wylde-check: control-ok` opt-out, the same treatment the `TextInput`/code-editor roots got in batch 7. No genuinely *dead* control was found.
  The panel's three native-dialog controls (the workspace folder picker and the conversation import/export file dialogs, all `rfd`) are declared via a new harness affordance, **`ControlWalk::external_effect(&[…])`**: the walk still *clicks* them (a panicking click is still caught) but does not require an observable backend/nav/state delta, because a headless test cannot open or drive an OS dialog. It is deliberately narrow — for genuinely-external effects, justified at the call site — and is not a substitute for widening a fingerprint or adding a state; a stale id declared there but never painted is itself an error.
- **The whole Workspaces panel — the largest surface in the tree — is now control-walked (refs #247, deferred-walk follow-up 4).** All eleven of its surfaces: the registry/container chrome (`WorkspacesPanel`), the Files, Editor, Concepts, Relations, Vocabulary, Hierarchy, GraphSettings and Graph tabs, plus the dependency-tree and curate-before-inject views. Most sub-views expose their gated-state seams only as private/`pub(crate)`, so their walks live **in-crate** (they run in CI: `cargo panel-walk` is `cargo test -p wylde-panel-workspaces`, which executes the lib unit tests as well as `tests/`); the two fully-`pub` views stay out-of-crate. Container ids the migration routed through `control()` but that carry no click handler (the tab roots, scroll viewports, dropdown/menu shells, the static outline row) are opted out with `control-ok` — their rows/buttons inside are the controls.
  Two oracle channels were added to reach it. **`focus_bus::focus_probe`** — the exact analogue of `nav_bus::nav_probe` — makes a "view in graph" control's only effect (a `request_workspace_focus` cross-panel deep-link) observable. And **`ControlWalk::fingerprint_ctx`** hands the fingerprint closure the view's `Context`, so it can `read` a **child entity** the view composes: the graph Settings tab's Layout / Dark buttons call straight through to the child `GraphView` and change nothing on the settings view itself, so their effect is only visible in the child's `current_layout_kind()` / `dark_mode()`.
  The two hard cases were handled honestly. **GraphSettings' Layout** is a three-segment radio built in one loop, so its *active* segment is a deliberate no-op that can't be opted out in isolation — it's declared `external_effect` (the same treatment a radio active-segment gets), while the other two switch the layout for real. **Graph's canvas** (`workspaces-graph-canvas`) is a raw gpui surface whose click hit-tests a node at the click's centre; the fixture parks one node at `-pan` so it maps to the canvas centre (selecting it) while the camera keeps a non-zero pan for the Fit affordance to recentre — two live controls from one baseline. The scoped-only exit chips and the inert unscoped breadcrumb crumb are not painted here (covered by the panel's own windowed nav tests), the latter declared `external_effect`.

- **The Shell's nav chrome is now control-walked — the last deferred panel walk (refs #247, deferred-walk follow-up 5).** The sidebar, panel slot, and update-pill/changelog modal could not be walked in place: they lived in the `wylde-gui` (Shell) crate, which links `wry` (webview) + `tray-icon`, so the headless L7 job never builds it. Their only coupling to the Shell was the concrete type, so the renderers were extracted into a new `wry`-free crate, **`wylde-gui-shell-chrome`**, rendering generic over a small **`NavChromeHost`** trait the Shell implements (one delegating `impl` block — nothing else about the Shell changes). The walk supplies a fake host, `ChromeHarness`, that *is* the host: each trait method records an observable delta, which is exactly what proves the control is wired to a host method (the method's real behaviour — IPC, whole-stack install — stays the Shell's concern). Five `.state()`s cover the two nav rows, the slot's service-recovery button (gated on an unavailable required service with no suppressing reason), the update pill, and the changelog modal (scrim / card / close).
  It surfaced one genuinely under-routed pair: the pill's **Update** and **Ignore** buttons were built by a shared `pill_button()` helper as bare `div().id(…)`, so they were neither registered nor walkable — despite being real affordances (Update kicks the install, Ignore dismisses the version). `pill_button` now routes through `control()`, so both register and the walk clicks them and asserts their host-method deltas moved. The changelog card — whose only handler is a deliberate `stop_propagation` no-op that keeps a click on the card from closing the modal — is declared `external_effect`, the same narrow treatment the Chat bubble layers got. No genuinely dead control was found. This was the last outstanding walk; with it landed, every panel in the tree is control-walked.

- **The GUI control-functionality gate is complete: the grandfather ratchet is deleted and a walk is now mandatory per panel (closes #247).** With every panel walked (the five deferred follow-ups above), the two loose ends close together. **Rule 59** loses its `GRANDFATHERED_UNROUTED` budget entirely — the routing migration reached zero, so the mechanism is gone rather than kept empty: any interactive `.id(` that bypasses `control()` is now a finding on the PR that adds it, with the per-site `control-ok` marker the only escape hatch. **New rule 61 (`every_control_building_crate_is_walked`)** is its companion and closes the residue rule 59 could not: a GUI crate whose shipped `src` builds a control must have a `control_walk` (in-crate or `tests/`) that declares **every** one of its control-building sources in `.sources(&[include_str!(…)])`. Together the two mean a control can neither bypass the per-frame registry the walk enumerates nor sit in a file no walk's coverage assertion ever inspects — a new panel added with no walk, or a walk that silently omits one of its files, now reds the build. The constructor crate, the walk harness, and the focus-surface widget crates (text input / code editor, whose roots are `.id()` + `control-ok` rather than `control()`) build no shipped control and so require no walk. Adding the rule surfaced and closed one honest coverage gap — Chat's `markdown.rs` (a rendered-link control) was not in any walk's sources; it is now declared.

- **Every interactive GUI control is now routed through the constructor — the grandfather ratchet is drained to empty (refs #247, part 2 batch 8).** The Shell's last 7 sites (sidebar, slot, update_pill) go through `control()`, taking `GRANDFATHERED_UNROUTED` from 7 to **0**. From 140 sites across 28 files at the pilot to zero: rule 59 now enforces routing everywhere with **no exempt debt**, so a new control that bypasses the constructor is an error on the PR that adds it, anywhere in the GUI.
  The Shell takes a plain (non-`test-support`) dependency on `wylde-gui-controls`; `control()` there is `.id()` and nothing else, verified via `cargo tree` (only `feature "default"`). The Shell links `wry` + tray-icon, so the headless L7 job never builds it — *walking* the Shell's chrome still needs it extracted into a `wry`-free crate, which is part of the deferred walk work; *routing* it needs no extraction and is done here.
  The ratchet dict + branch are kept (empty) for one more step: the endgame deletes them together with the addition of a "require a control_walk per panel" rule, once the deferred stateful-panel walks (Models, Devices, Chat, the Workspaces sub-views + graph, the Shell chrome) have landed.

- **Chat controls are routed through the constructor; only the Shell's 7 sites remain (refs #247, part 2 batch 7).** Chat's 34 sites (chat_panel, composer_ui, markdown) go through `control()`. Ratchet **43 → 7 sites / 3 files** — every remaining site is in the Shell, which needs its nav chrome extracted from the `wry`-linking crate before the headless job can build it.
  Two shared *widgets* — the `TextInput` and code-editor roots — were flagged by rule 59 (they carry keyboard handlers) but are **not** click-buttons: they are focus/text-entry surfaces. Routing them through `control()` enrolled them in the per-frame registry, so every panel embedding a text input started "walking" its input field and demanding a click effect that focusing a field has no reason to produce — the Memory walk went red on exactly this. They now carry the `// wylde-check: control-ok` opt-out with a reason, which is precisely what that marker is for: a genuinely-interactive id that is not a clickable control.
  Chat's own control walk joins the deferred stateful-panel follow-up (it is already the most behaviourally-tested panel — `chat_turn_e2e`, `dock_scoping`, `conversations`, `virtualization`, …). Routing is enforced now.

- **Models + Devices controls are routed through the constructor, and the id scanner now sees the wrapped form (refs #247, part 2 batch 6).** 28 sites across the two panels; ratchet drops accordingly.
  This batch also fixes a real **coverage-guard hole**. The migration left many sites as
  `control(div(), ElementId::Name("models-hf-close".into()))` — the id literal nested one level deeper than the
  scanner looked. `literal_control_ids` only saw bare `control(el, "id")` strings, so **every `ElementId::Name`
  control silently escaped `assert_covers_every_literal_id`** — a modal control could go unwalked while the walk
  reported success, which is exactly the false coverage #247 exists to prevent. The scanner now takes a
  `control()` call's *last argument* and recognises both the bare literal and the `ElementId::Name("…")` wrapper,
  while still returning nothing for genuinely runtime ids (`format!`, the `("row", i)` tuple whose rendered id is
  `"row-{i}"`, not `"row"`). Five new scanner tests pin all of this.
  The **walks for Models and Devices are deferred** to a focused follow-up, not rushed in: each panel has four to
  seven modal sub-states (Models alone has the pull dialog, delete-confirm, an HF *detail* strip and a separate
  HF *results* strip, plus a privacy-pref-gated catalog row), and the sub-state fixtures deserve their own PR.
  Routing is enforced now, so a new unrouted control in either panel reds the build; the follow-up is about
  walking them.

- **Settings controls are now walked, and the walk distinguishes unreachable from dead (closes the Settings blocker on #247; refs #247, part 2 batch 4).** Ratchet **127 → 120 sites / 23 files**.
  Settings was held back two batches because its voice-section rows didn't respond to a synthetic click. The
  cause was the occluding-modal problem `ControlWalk::reset` already existed for — an earlier click in the pass
  opened a modal whose `.absolute().inset_0().occlude()` backdrop then swallowed every later click. The walk was
  reverted before it ever ran *with* a reset closure, so the combination was simply untested. With
  `.reset(|p, _w, cx| { /* close all modals */ })` the seven controls walk green.
  Two of them then surfaced as fixture gaps, not dead controls: the modal "don't show again" checkbox toggles
  `hf_dont_show_again` (which the fingerprint didn't cover), and the privacy-reset button clears a flag that the
  fixture left unset (so the reset was a visible no-op). Both fixed in the test — the reset closure now also arms
  the precondition, so the button always has something to reset.
  **One genuine find:** `per_tool_row` built its consent-decision control with a bare `.id(...)` while its click
  handler was attached by the *caller*. That is invisible to `wylde_check` rule 59 (function-scoped — the helper
  has no handler in its own body) *and* to the walk (unregistered, so never enumerated). It is the exact residue
  case rule 59's docs acknowledge, and the control walk is what caught it. Now routed through `control()`.
  **New harness capability — unreachable vs dead.** A control can paint valid bounds and still be unclickable
  (it laid out below the viewport, so the click lands outside the window). That is a walk problem, not a dead
  handler, and calling it "dead" sends you hunting a bug that isn't in the panel. The walk now checks the click
  point against the viewport and reports an out-of-bounds control as **unreachable**, with a message pointing at
  `.viewport()` — a separate assertion from the dead-handler one. Proven by shrinking the viewport under a live
  panel and confirming the control is named unreachable, not dead.
- **All Workspaces controls are routed through the constructor, and `control()` now accepts any gpui id (refs #247, part 2 batch 5).** Ratchet **120 → 71 sites / 10 files** (49 sites across Workspaces' 13 files).
  The migration surfaced a real ergonomic gap: Workspaces builds many per-item rows with the tuple id form
  `.id(("file-row", i))`, which the old `control(el, impl Into<SharedString>)` could not accept. Rather than
  churn every such site into a `format!`, `control()` now takes `impl Into<ElementId>` — exactly what gpui's
  own `.id()` takes — so it is a true drop-in at every site, tuple ids included. The registry key and the
  paint-time `debug_selector` both derive from the id's `Display` (`ElementId::Name("x")` → `"x"`,
  `("file-row", 3)` → `"file-row-3"`), so the two halves of the walk still agree on one string, and every
  existing string id is byte-identical (nothing that names an id breaks). Release builds are unchanged: the id
  is set with `.id()` and the dev-only recording block compiles out.
  The Hierarchy sub-view is walked. The other Workspaces surfaces are staged deliberately, not skipped: Concepts,
  Relations and Vocabulary render a sub-tab switcher whose click switches the *parent* `VocabularyTab`'s tab, so
  mounted standalone those pills have no parent to act on and can't be exercised in isolation — they are walked
  through the container in a follow-up. The graph canvas + main panel chrome are walked there too. Every one of
  the 13 files is routed now, so rule 59 enforces them; the follow-up is about *walking* them, not routing.

- **The control walk gained a nav channel and a viewport fix, and Dashboard + RemoteAccess are now walked
  (refs #247, part 2 batch 3).** Ratchet **136 → 127 sites / 24 files**.
  Dashboard exposed a real hole in the oracle. Its fifteen service chips and its empty-state rows do exactly
  one thing when clicked: call `wylde_gui_pipe::request_nav(...)`. That is neither a backend call nor a change
  to the panel's own state, so under the previous two-channel oracle **every one of them read as a dead
  control**. The harness doc had claimed nav "folds into state" — it does not; `request_nav` hands the key to
  the Shell and the originating panel never moves. Nav is now a third channel, recorded by a dev-only
  `nav_probe` in `wylde-gui-pipe`. It is a **thread-local**, not a reader on the existing process-wide
  `OnceLock` sender: a test that installed a real channel would collect nav requests from every other test in
  the binary, and that contamination could only ever turn a dead control into a live-looking one — the wrong
  direction for a gate to be wrong in. Same shape as the scripted backend's thread-local.
  A second false-positive class turned up with it: a long panel lays its lower controls out *below* the test
  display (1920×1080), where they still get painted bounds and so look walkable, but a click at y > 1080 lands
  outside the window and hits nothing. Every control past the fold read as dead. The walk now grows the
  viewport before drawing, which costs only layout on a headless platform. Same shape as the `open_window`
  trap, and fixed once in the harness rather than left for each panel to rediscover.
  The harness also re-establishes a baseline before **every** click (`ControlWalk::reset`), because Wylde's
  modals are `.absolute().inset_0().occlude()` backdrops: one click opening one would otherwise swallow every
  later click in the pass and report a whole tail of live controls as dead.

- **Self-expiring tracker docs — a standing tracking issue becomes a doc that garbage-collects itself (closes #253; closes #83).**
  A *tracker* is an issue that holds no open work and exists only to be the home for the next instance of a
  recurring problem. #83 — the self-collision class, tests that assert against production or shared resources —
  had been exactly that for months: five sightings (#80, #224, #225, #226, #232), all closed, both halves of
  the class guarded, and nothing to do. Its own closing criterion was *"close it when the class has gone quiet
  long enough to call it dead"* — a judgement call that requires someone to notice the **absence** of events,
  which nobody ever does, so it stayed open. Kept as a plain doc instead it would have rotted the other way:
  outliving its subject and becoming a confident description of a problem that no longer exists.
  This makes that criterion a timer. `docs/trackers/self-collision-class.md` carries the full diagnosis —
  the class, the tell, all five sightings, the two-halves split that decides which kind of guard a new one
  needs, and a "record a new sighting here" section — behind front matter with an `expires` date. **Recording
  a sighting resets the clock** (a commit touching the file re-derives `expires` to that commit's date + one
  month); untouched past expiry, a scheduled workflow **deletes it** through an ordinary squash-merged PR with
  an explanatory body — no force-push, no bypass, recoverable via `git log --diff-filter=D`. A heads-up issue
  opens seven days ahead so nothing ever vanishes unannounced.
  The mechanism is **general**, not special-cased: any `docs/trackers/*.md` with an `expires` key gets the
  same behaviour, with no registry to update (`docs/trackers/README.md` is the contract).
  Two details carry the design. Every commit the automation authors is marked, and the touch-detector skips
  its own marks — without that the bump commit would itself count as a touch and the doc could never expire,
  which is the rot failure mode with extra steps. And rule 56's pointer at the tracker is **presence-gated**
  (`rules._tracker_ref.tracker_pointer`): it returns a sentence when the doc exists and an empty string when
  it does not, so the day the tracker auto-deletes, findings simply lose a sentence rather than the linter
  gaining a dangling path. The tracker is deliberately *not* registered in `RULE_TARGET_SPECS`, which would
  have turned rule 51 red on the exact day the doc was designed to disappear.
- **Memory and Changelog controls are now walked (refs #247, part 2 batch 2).** Both crates route their
  interactive sites through `controls::control(el, "id")` and gain a `tests/control_walk.rs`, taking the
  grandfather ratchet from 140 sites / 28 files to **136 / 26**.
  Memory is the first panel to need a walk **state**: its copy-in button paints only on an *expanded* row, so
  the default frame never shows it. `.state("row-expanded", …)` opens the row and the walk covers it — with a
  test asserting the button is actually reached, so deleting the state fails rather than silently shrinking
  coverage (the button's id is built at runtime, so the literal-id guard cannot see it; that assertion is what
  keeps it honest). Changelog is the opposite case and a useful one: it takes **no backend at all**, so its
  walk runs on the state channel alone — proof the oracle does not quietly depend on IPC traffic to notice that
  a control did something.
  The ratchet's self-test changed shape with this batch. It used to pin the exact total (140 across 28 files),
  which would mean a churn edit every batch for no signal — and the total needs no guarding, because emptying
  the table without migrating does not go quiet, it puts every file over a budget of zero and reds the rule.
  What a fixed number would *not* catch is a budget entry for a renamed or deleted file, which lingers granting
  a budget to nothing and re-arms silently if the path returns (#101/#116). That is what it now asserts.

- **The control walk is now a shared harness, so covering a new GUI control costs nothing (refs #247, part 2 of N).**
  The #247 pilot proved the mechanism on one panel with the walk logic inlined in that panel's test. This lifts
  it into `wylde_gui_test_support::control_walk`, where a panel's whole cost is a fixture, a fingerprint and a
  call — and **adding a control after that needs no test edit at all.** Build it with
  `controls::control(div(), "id")` and it is registered, painted, walked, clicked and required to produce an
  observable effect automatically. Coverage becomes a property of *construction* rather than of somebody
  remembering to add a case, which is the whole reason every control routes through one constructor.
  Two capabilities land with the extraction. **Named states** (`.state("label", |panel, window, cx| …)`) drive
  the panel into a condition — a modal open, a section expanded — and walk whatever *that* frame paints, with
  coverage asserted over the union. That closes the modal-gated-control gap the pilot flagged. And
  **`.assert_covers_every_literal_id()`** scans the panel's own source (declared via `include_str!`) for
  `control(…, "literal")` ids and fails on any that no walked state ever painted. That is the part that matters:
  without it a modal control the walk never reaches is not reported as uncovered, it is simply never mentioned —
  the walk succeeds over a smaller set than the panel has, and the number looks complete. Now it goes red and
  names the id. Proven by adding a modal-gated control to Tools (walk red, naming `tools-advanced-reset` and
  telling you to add a state), adding the state (green), then reverting both.
  The id scanner lives in `wylde-gui-controls`, not in the test-support crate, for a reason worth recording:
  test-support is EXCLUDED from the GUI workspace and so has no lock file and cannot be `cargo test`-ed in CI at
  all. A scanner whose own tests never run would be the #56 shape exactly — enforcement enforced by nothing.
  Beside the constructor it rides `cargo panel-walk` (now 53 test binaries green, `wylde-gui-controls` at 10
  tests).


- **`wylde_check` rule 60 — a unit test that touches a process-global broadcast bus must own its channel or serialize on a guard (closes #246).**
  #246 was not a one-off flake, it was the #83 self-collision class again: several tests in one binary contending on
  one shared resource with nothing serializing them. Rule 56 (`graph_test_serialized_on_db_lock`) was written for
  exactly that shape and still missed this instance, twice over, and both misses are structural rather than bad luck:

  1. **Scope.** Rule 56 walks `rust/crates/**/tests/*.rs` — integration binaries only. #246 lived in a
     `#[cfg(test)] mod tests` inside `src/`, which no self-collision rule looked at.
  2. **The single-toucher carve-out.** Rule 56 deliberately skips a binary with fewer than two live-graph tests, on
     the reasoning that one test can't self-collide. For a *bus* that reasoning does not hold, and #246 is the
     counter-example: exactly **one** test called `subscribe()`. Its colliders were tests that never mentioned the
     bus at all — they merely ran watcher loops, and the loop published.

  So rule 60 covers the `src/` half with **no** minimum-count carve-out, and propagates "touches the bus" through
  helpers and through same-file product functions, so the publishing colliders are named too, not just the one test
  that reads. It is satisfied by **isolation** (the test, or a helper it calls, constructs its own
  `broadcast::channel`) or by **serialization** (a test-module `Mutex` guard — rule 56's `DB_LOCK` pattern, and the
  `TEST_GUARD`/`guard()` shape `Pipe/src/conversation_bus.rs` and `model_bus.rs` already use). Injection is preferred:
  serialization costs parallelism and is still a convention every new test must remember.

  Verified both directions on the real tree rather than only on fixtures: **5 findings** against the pre-fix watcher
  (the reader plus all four publishing colliders), **0** after. The rule also follows a *file-backed*
  `#[cfg(test)] mod tests;` into its sibling file — load-bearing, because #246's own fix pushed `watcher/mod.rs` past
  rule 20's 700-line cap and moved the tests to `watcher/tests.rs`. A rule that only understood the inline form would
  have gone quiet at precisely the moment the file it guards was split, which is the #101/#116 decay shape (a gate
  going quiet rather than red) that this suite exists to prevent.
- **GUI controls are now proved to DO something, not just to render (refs #247; pilot — Tools panel).**
  The L7 panel-walk (#35) proves every panel *loads*. Nothing proved a control in it *works*: no test in the
  tree had ever clicked a GUI control through its real listener, so a button could ship with an empty handler,
  a handler wired to a method that no longer runs, or no listener at all, and every gate stayed green. This
  lands the mechanism and pilots it on one panel; the ~140-site migration across the other eight panels is
  part 2, gated on the pilot result.
  Three pieces. **`wylde_gui_controls::control(el, "id")`** — the one constructor every interactive control
  routes through. In a shipped build it is `.id()` and nothing else: the registry module is behind a
  `test-support` feature requested only from `[dev-dependencies]` (which `resolver = "2"` never unifies into a
  normal lib), and the paint-time hook is gpui's own `debug_selector`, which **gpui itself** compiles as an
  `#[inline]` no-op that drops its closure unless gpui carries `test-support`. Verified the same way the pipe
  seam is: `cargo tree -p wylde-gui -e normal,features -i wylde-gui-controls` reports only `feature "default"`.
  **`tests/control_walk.rs`** — draws the panel, enumerates the controls that *actually painted* (the
  constructed-this-frame registry intersected with gpui's per-frame `debug_bounds`), clicks each at its painted
  centre through `simulate_click` — real platform event, real hit-testing, real listener — and asserts an
  observable effect via a two-channel oracle: the scripted backend's call count, and a per-panel state
  fingerprint. Deliberately weak per control and strong in aggregate: it cannot tell you the button did the
  *right* thing, but it cannot be satisfied by a button that does *nothing*. One fingerprint closure per panel
  is what makes it affordable at ~140 sites. It also repaints the loaded/error branches panel-walk never
  touches, so a **panic on click** in one of those surfaces as a red test rather than in front of the user.
  **`wylde_check` rule 59** — the static half: a dead handler body (empty, or only `cx.notify()` / `todo!()`),
  and an interactive site that bypasses the constructor. The second is the important one — an unregistered
  control is never enumerated and never clicked, so coverage drops silently while the suite stays green, the
  same decay shape as #56/#101/#116. It ships at **error** with a per-file grandfather ratchet recording the
  140 pre-existing sites, rather than the WARNING a staged rollout would suggest: the `wylde_check (full rule
  set)` CI job fails on any finding, warning included (by design since #114), so a WARN-only rule would red
  `develop` exactly as hard as an error one. The ratchet reports zero today and fails the build on a **new**
  unrouted control — the goal delivered now rather than after the migration. It tightens in both directions:
  a count below budget is also a finding, because an allowlist nobody must lower rusts open.
  The oracle was proved rather than assumed: the Refresh button was deliberately broken three ways — emptied
  handler, listener removed entirely, and a panic reachable only once the catalog loads — and the walk went red
  each time, naming the dead control; restoring it went green. The panic case reds exactly the two branch tests
  and correctly leaves the healthy-path tests green, which is the point of repainting those branches. 30
  consecutive runs at default parallelism: 30 green, 0 flakes.
  Two findings worth recording. gpui's `TestAppContext::open_window(size, …)` sets the reported `viewport_size`
  but the root still lays out against the test *display*, so every control paints outside the window, every
  click misses, and **every control reads as dead** — a total false positive shaped exactly like the bug. The
  walk mounts with `add_window`; `Core/GUI/docs/gui-testing.md` documents the trap. And rule 59's path matcher
  needed `(.*/)?` rather than the `.*/src/` form used elsewhere in the suite, because the Shell's sources sit
  at `Core/GUI/Shell/src/…` with nothing between the crate root and `src` — the `.*/src/` form matches no Shell
  file at all, and the Shell owns the nav chrome (7 of the 140 sites).

- **The persistent default model is now guaranteed to survive an UPDATE, not just a restart (closes #243; refs
  #235, #132).**
  #235 made the default survive a shutdown — it is read from disk on start. Whether it survives an *update* had
  never been asserted anywhere, and the answer was not obvious: the store resolves `DATA_DIR` → the **relative**
  literal `"data"`, so where it lands depends on the working directory lifecycle spawns services with
  (`cmd.current_dir(wylde_root())`, itself exported by `launch_wylde.ps1` as `$PSScriptRoot`). The investigation
  found it **is** safe today: `wylde-updater::install_stack` stages into `<home>/versions/<version>/`, flips the
  `%LOCALAPPDATA%\Wylde\current` pointer, then prunes older version directories — that `versions/` tree plus the
  pointer is its entire write surface, and it never touches the estate root the store lives under. **No
  relocation was required.** But it was safe by circumstance rather than by construction: the store sits in the
  stack/estate tree rather than a designated user-data directory, and stays safe only while the updater's blast
  radius stays narrow. Three tests turn that from an accident into a checked property — a round-trip that stages
  a new stack, prunes the old one, drops every in-memory cache and asserts the default is still readable; a
  **structural** assertion that neither `default_model.json` nor `active_model.json` resolves inside the
  `versions/` tree an update replaces; and a guard that a stale copy inside a superseded stack directory cannot
  shadow the live default. Rooting model-selection state in the stack directory — the change that would silently
  reset every user's default on every release — now turns the build red instead of shipping. Deliberately not
  addressed: that this store uses `<ROOT>/data` rather than convention A (`<WYLDE_ROOT>/.wylde/data`, #138), a
  documented deliberate deviation whose unification carries data-migration risk across the model registry,
  device gate and ollama overrides.

- **A persistent default model that survives restart, with sensible fallbacks — and a recommendation instead of
  silence when nothing is installed (closes #235; builds on #131/#132).**
  Wylde already persisted a starred default (`models.set_default` → `default_model.json`), but nothing ever
  checked it against reality. Three holes shared one symptom — *the model picker points at nothing usable*:
  the star was never validated against the store, so deleting that model (which #131 made a one-click
  operation) left a phantom tag that failed at inference time as an Ollama 404 rather than at selection time
  as a fallback; a user who never touched the star got `null` even with five models on disk; and an empty
  store also resolved to `null`, offering no way forward. The new `models.resolve_default` verb resolves
  against the **live on-disk inventory** in a fixed order: **(1)** the persisted default *if it is still
  installed* (matched across the implicit `:latest`, same rule #131 established for slot labelling);
  **(2)** otherwise the first available model in the inventory — a star whose model was deleted falls
  *through* to this silently, reporting the dangling name for the UI to explain but never erroring;
  **(3)** otherwise, with a genuinely empty store, a **recommendation** of `qwen3.5:9b` (6.6 GB, the real
  ~9B on-device Qwen) carrying its warnings: download size, VRAM fit, and the slower first message while
  weights load. It is a recommendation with a Pull button, **never an auto-download** — the same discipline
  as the locked never-auto-delete decision, pointed the other way: Wylde does not move 6.6 GB across
  someone's network because a picker was empty. Crucially, an **unreachable** model store is an error, not
  an empty one — #132's distinction applied to resolution, so a daemon still restarting after an update is
  never answered with "nothing installed, here is a 6.6 GB download". The Models panel now hydrates from
  this verb rather than the raw star, so a deleted default lights up no row, a fallen-through default
  explains itself in a note, and the empty state renders the recommendation and its warnings verbatim from
  the harness (one owner for that copy, so a second surface can't drift). Persistence itself is unchanged:
  `default_model.json` remains the single store — the resolver is a pure function over it, not a parallel
  one. The recommended chat model is deliberately distinct from `DEFAULT_REASONER_MODEL` (the 35B-A3B
  UD-IQ3_XXS quant locked by the 2026-07-13 planning eval): different slot, different job. Covered by 22
  backend tests and 4 L7 panel-walk cases (star survives restart; deleted default falls through;
  empty inventory recommends with warnings; unreachable ≠ empty).

- **`wylde_check` rule 57 (`service_backed_surface_declares_availability`) makes "no silent dead panel" a structural gate for every service/extension surface (refs #239).**
  The GUI already gated a panel's *dependence on services* two ways — `required_services` → the Shell's
  `SlotState::ServiceUnavailable` (rule 40 enforces the declaration), and the URL probe behind a first-party iframe.
  Neither could cover the defect in #239, and the reason is worth stating precisely: the Tools panel declared
  `wylde-extension-bridge` correctly, so **rule 40 was satisfied**. The bridge was up and the panel mounted — and then
  drew one card per extension panel, each pointing at a *different* service's URL that nothing checked. A panel-level
  gate is structurally incapable of covering a per-item surface, because the unit that can be dead is the item.
  The new rule closes that in three clauses, all **derived from the tree rather than a list of panels**: a wire row
  carrying a `url` must also carry an `availability` field (the endpoint is the tell — a row modelling something
  remote can be dead, so it has to say whether it is); the panel owning such a row must actually *read* that field
  outside its wire module (a field nothing renders is the same silent dead panel with extra steps); and a panel that
  opts out of rule 40 — thereby taking responsibility for showing unavailability itself — must demonstrably render a
  status, closing what was otherwise a free pass out of every gate. Corpus is both sides of the wire
  (`Core/GUI/Frontend/Panels/*/src/ipc.rs` plus the bridge's `host.rs`, which mints the rows), both registered in
  `RULE_TARGET_SPECS` so emptying either goes red instead of quietly disarming the rule. **Verified against the
  pre-fix tree: it reports both `Tools::ExtensionPanel` and `host::PanelEntry`** — it would have red-walled the change
  that shipped the dead Images card. A panel added later is walked because it exists, not because anyone remembered
  to register it, so coverage cannot regress by omission. It is a source rule and not a Rust test deliberately: the
  property has to hold for a panel nobody has written yet, and `Core/GUI` CI runs `build` + `panel-walk` only, so a
  test in the registry crate would never execute.


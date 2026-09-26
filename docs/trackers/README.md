# Self-expiring tracker docs

A **tracker** is a doc that exists to be the home for the *next* instance of something —
a recurring bug class, a watch-list, a "if this happens again, read this first" note. It
has no open work in it. Its value is entirely that a future diagnosis lands somewhere that
already knows the history.

Trackers have two failure modes, and they pull in opposite directions:

1. **Kept as an open issue**, a tracker clutters the issue list forever. Its closing
   criterion is always some version of *"close it when the thing has gone quiet long enough
   to call it dead"* — a judgement call that requires someone to notice the absence of
   events. Nobody does. It stays open.
2. **Kept as a doc**, it rots. Nothing ever deletes it, so it outlives its subject and
   becomes a confidently-worded description of a problem that no longer exists — worse than
   nothing, because a reader trusts it.

A self-expiring tracker resolves both by making "has it gone quiet?" a **timer** instead of
a judgement call.

## The contract

A doc under `docs/trackers/*.md` with an `expires:` key in its YAML front-matter opts in:

```yaml
---
tracker: <slug>            # optional; defaults to the filename stem
expires: 2026-08-23        # YYYY-MM-DD — the recorded expiry
warn_days: 7               # optional; heads-up window, defaults to 7
origin: 83                 # optional; the issue this replaced, used in PR/issue bodies
---
```

Three behaviours follow, all implemented by `Core/harness/dev/tracker_expiry.py` and driven
by `.github/workflows/tracker-expiry.yml`:

| behaviour | when | what happens |
|---|---|---|
| **reset on touch** | a commit modifies the doc | `expires` is re-derived as *that commit's date + 1 month* and a PR bumps the front-matter |
| **warn** | `warn_days` before expiry | a heads-up issue is opened (or its existing one updated) naming the doc and the date |
| **expire** | past `expires` | a PR **deletes** the doc and strips any marked references to it |

**Recording something in the doc is what keeps it alive.** You never edit `expires` by hand
to extend it — you use the doc, and using it resets the clock. That is the whole idea: the
doc survives exactly as long as it is being used, and no longer.

## How "touch" is computed

`expires` is derived from `git log` on the file, taking the newest commit that is **not**
one of the automation's own bumps:

```
git log --format='%H%x09%cI%x09%s' -- <path>
```

Bump commits carry the marker `[tracker-expiry]` in the subject and are skipped. Without
that filter the bump commit would itself count as a touch and the doc would renew itself
forever — a doc that can never expire, which is failure mode 2 with extra steps.

The recorded `expires` also acts as a **floor**: if a human sets a date further out than
the derived one, the later date wins. That is the escape hatch for "this tracker matters
even though nothing has happened yet" — but it is a deliberate, visible edit, not the
default.

## Deletion is a normal change

The expiry PR is an ordinary squash-merge behind every required check. Specifically:

- **No force-push, no direct push to `develop`.** Branch protection applies to the bot
  exactly as to a person; the automation has no bypass and does not want one.
- **Recoverable.** `git log --diff-filter=D -- docs/trackers/<slug>.md` finds the deletion
  commit; `git show <sha>^:docs/trackers/<slug>.md` prints the last content.
- **Announced first.** The warn step opens an issue `warn_days` ahead, so an expiry is
  never the first time anyone hears about it.

## Referencing a tracker from code

A tracker can vanish. A reference to one must therefore never be load-bearing.

**Do not** add a tracker path to `RULE_TARGET_SPECS` in
`Core/harness/dev/wylde_check/rules/_selfcheck.py`. That registry means *"this rule silently
passes if the path is missing"*, and it is checked by rule 51 — a tracker listed there would
red the build the day it expires, which is precisely the dangling reference this pattern
exists to avoid.

**Do** gate the reference on the file existing at runtime. The established form is
`wylde_check`'s `tracker_pointer()` helper:

```python
from ._tracker_ref import tracker_pointer

message = "…the #83 self-collision class." + tracker_pointer("self-collision-class")
```

It returns a pointer sentence when the doc is present and an **empty string** when it is
not, so the rule's output degrades to silence rather than to a broken path. Nothing needs
to be edited on the day the tracker expires.

For prose references in other markdown, mark the line so the expiry step can strip it:

```markdown
Background: [the widget-drift tracker](trackers/widget-drift.md). <!-- tracker-ref: <slug> -->
```

Substitute the real slug for `<slug>`. Any line carrying `tracker-ref: <the slug>` is
removed by the expiry PR — including, note, a line inside a fenced code block, since the
stripper is line-based and does not parse markdown. That is why the example above leaves
the placeholder in: a real slug here would make this README strip its own documentation
the day that tracker expired. (`<slug>` is safe because the marker pattern matches only
`[A-Za-z0-9._-]`, and `<` is not in it.)

An unmarked prose mention is left alone — write those so they still read correctly in the
past tense.

## Adding a new tracker

1. Write `docs/trackers/<slug>.md` with the front-matter above, `expires` about a month out.
2. Give it a **"record a new instance here"** section — that section is the reason the doc
   exists, and appending to it is what resets the clock.
3. Say so at the top: a reader who finds the doc should understand it may vanish, and why.
4. If code should point at it, use `tracker_pointer()`.

Nothing else is needed. The workflow discovers trackers by scanning `docs/trackers/*.md`
for the `expires` key — there is no registry to update.

## Current trackers

| doc | origin | subject |
|---|---|---|

*(Rows are marked, so an expiring tracker removes its own row from this table.)*

# outputs/

Local agent scratchpad; gitignored. Slice agents drop reports + intermediate
artifacts here. Only this README is tracked.

## Layout (since the 2026-06-09 pre-C-navigation sweep)

| Path | What it is |
|---|---|
| `scope_v2.md` | **LIVE working copy of Plan v2.2** (canonical: Nextcloud "The Thought Bubble System (scope, v2)") |
| `build_order.md` | **LIVE working copy of Build Order incl. the slice status board** (canonical: Nextcloud) |
| `quick_ref.md` | LIVE working copy of the Quick Reference (board mirror — status-only) |
| `open_issues_v2.md` | Working copy of Open Issues v2 (all resolved; stable) |
| `tbs-slice-<name>-*.md` | Slice orientation/status reports — Aaron reads these |
| `nextcloud-mirrors/` | Point-in-time fetches of other Nextcloud pages (strangler, memgraph, voice, collectives, …). **Fetch date unknown — re-fetch via nctool before trusting.** |
| `archive/` | Superseded planning snapshots + scratch, dated per sweep. Don't extend; make a new dated folder. |

Rules: Nextcloud is authoritative; the LIVE copies above exist because slice
agents can't always reach nctool. Any board edit made here is **owed to the
Nextcloud Build Order page** — sync with nctool when available and say so in
the slice report. One live copy per doc; supersede into `archive/`, never
fork a second variant at top level.

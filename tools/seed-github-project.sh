#!/usr/bin/env bash
# seed-github-project.sh — create + seed the private "Wylde Roadmap" GitHub Project (v2).
#
# DEPENDENCIES: only the GitHub CLI (`gh`). There is deliberately NO standalone
# `jq` — every JSON read uses gh's built-in `--jq` (gh embeds a jq engine). So
# this runs out of the box on a machine that has gh authenticated and nothing
# else installed. (A prior version required standalone `jq` and failed with
# "jq required" on a machine that didn't have it.)
#
# IDEMPOTENT: safe to re-run. It reuses the existing "Wylde Roadmap" project if
# one already exists (it never creates a second), creates the custom fields only
# if they're missing, and adds each issue / draft only if it isn't already on the
# board. A re-run against a fully-seeded project makes no changes.
#
# SCOPE: `gh project` needs the `project` token scope. If it's missing, run:
#     gh auth refresh -s project
#
# What it ensures on the board:
#   1. A private Project (v2) titled "Wylde Roadmap" under PeopleWonder.
#   2. Custom fields: Tier (single-select Tier 0–3), Target version (text).
#      (Status — Todo/In Progress/Done — exists by default.)
#   3. Every tracked backlog issue, Tier set.
#   4. A draft item per lighter tier item (cleanup / deferred-by-design), Tier set.
#
# ADDING AN ISSUE: add one line to the ISSUE_TIER map below — that map is the
# single source of truth and the seeding loop iterates it directly.
#
# The Roadmap (timeline) *view* is a UI toggle the CLI can't create — after this
# runs, open the project → new view → "Roadmap" if you want the calendar layout.
set -euo pipefail

OWNER="PeopleWonder"
REPO="PeopleWonder/wylde"
export TITLE="Wylde Roadmap"   # exported so gh's --jq can read it as env.TITLE

# --- Preflight: gh present + authenticated with the project scope ------------
command -v gh >/dev/null || { echo "ERROR: the GitHub CLI (gh) is required."; exit 1; }
gh auth status 2>&1 | grep -qi "project" || {
  echo "ERROR: the gh token lacks the 'project' scope. Run:  gh auth refresh -s project"; exit 1; }

# --- 1. Project: reuse if it exists, else create ----------------------------
num=$(gh project list --owner "$OWNER" --limit 100 --format json \
        --jq '.projects[] | select(.title == env.TITLE) | .number' | head -n1)
if [ -z "$num" ]; then
  num=$(gh project create --owner "$OWNER" --title "$TITLE" --format json --jq '.number')
  echo "Created project '$TITLE' (#$num)."
  # Best-effort private (flag availability varies by gh version; non-fatal).
  gh project edit "$num" --owner "$OWNER" --visibility PRIVATE >/dev/null 2>&1 \
    && echo "  set visibility Private" \
    || echo "  (set visibility to Private in the UI if the flag was unavailable)"
else
  echo "Reusing existing project '$TITLE' (#$num) — no duplicate created."
fi
pid=$(gh project view "$num" --owner "$OWNER" --format json --jq '.id')

# --- 2. Fields: create only if absent ---------------------------------------
ensure_field() {  # $1 = name; $2 = data-type; $3 = (single-select only) options csv
  local existing
  existing=$(gh project field-list "$num" --owner "$OWNER" --format json \
               --jq ".fields[] | select(.name==\"$1\") | .id")
  if [ -n "$existing" ]; then
    echo "  field '$1' exists"
    return
  fi
  if [ "$2" = "SINGLE_SELECT" ]; then
    gh project field-create "$num" --owner "$OWNER" --name "$1" \
      --data-type SINGLE_SELECT --single-select-options "$3" >/dev/null
  else
    gh project field-create "$num" --owner "$OWNER" --name "$1" --data-type "$2" >/dev/null
  fi
  echo "  created field '$1'"
}
echo "Ensuring fields…"
ensure_field "Tier" SINGLE_SELECT "Tier 0,Tier 1,Tier 2,Tier 3"
ensure_field "Target version" TEXT

# Resolve the Tier field id + its option ids (cached; used to set the field).
tier_fid=$(gh project field-list "$num" --owner "$OWNER" --format json \
             --jq '.fields[] | select(.name=="Tier") | .id')
opt_id() {  # $1 = option name -> option id
  gh project field-list "$num" --owner "$OWNER" --format json \
    --jq ".fields[] | select(.name==\"Tier\") | .options[] | select(.name==\"$1\") | .id"
}
oid0=$(opt_id "Tier 0"); oid1=$(opt_id "Tier 1"); oid2=$(opt_id "Tier 2"); oid3=$(opt_id "Tier 3")
tier_oid() { case "$1" in
  "Tier 0") echo "$oid0";; "Tier 1") echo "$oid1";;
  "Tier 2") echo "$oid2";; "Tier 3") echo "$oid3";; esac; }

set_tier() {  # $1 = item id, $2 = "Tier N"
  local oid; oid=$(tier_oid "$2")
  [ -n "$oid" ] && gh project item-edit --project-id "$pid" --id "$1" \
    --field-id "$tier_fid" --single-select-option-id "$oid" >/dev/null 2>&1 || true
}

# --- Snapshot what's already on the board (one call each; drives idempotency)-
present_issue_nums=$(gh project item-list "$num" --owner "$OWNER" --limit 200 --format json \
                       --jq '.items[] | select(.content.type=="Issue") | .content.number')
present_draft_titles=$(gh project item-list "$num" --owner "$OWNER" --limit 200 --format json \
                         --jq '.items[] | select(.content.type=="DraftIssue") | .title')

issue_present() {  # $1 = issue number
  local n
  for n in $present_issue_nums; do [ "$n" = "$1" ] && return 0; done
  return 1
}
draft_present() {  # $1 = exact draft title
  local t
  # Heredoc (not a pipe) so the loop runs in this shell and `return` works.
  while IFS= read -r t; do [ "$t" = "$1" ] && return 0; done <<EOF
$present_draft_titles
EOF
  return 1
}

# --- 3. Tracked issues ------------------------------------------------------
# The Project auto-populates its built-in Milestone field from each issue, so the
# milestone structure shows up without extra work here — we only set Tier.
declare -A ISSUE_TIER=(
  [25]="Tier 3"
  [26]="Tier 0" [27]="Tier 0" [28]="Tier 0" [29]="Tier 0"
  [30]="Tier 1" [31]="Tier 1" [32]="Tier 1"
  [33]="Tier 0" [34]="Tier 0" [35]="Tier 0" [36]="Tier 0" [37]="Tier 0" [38]="Tier 0"
  [39]="Tier 2" [40]="Tier 2"
  [41]="Tier 0"   # Ship 0.2 — release readiness gate (tracking)
  [43]="Tier 0"   # memory.long_term.save/update never embeds (0.2 verified build)
  [44]="Tier 3"   # rag(dx): wire in the functional RAG graph — deferred post-stabilization, no milestone
  [47]="Tier 0"   # preflight --launch self-collision
  [49]="Tier 1"   # ci: cargo-deny (advisories) as a required check
  [55]="Tier 0"   # L4 first-run bootstrap check (split out of #33)
  [56]="Tier 0"   # GUI behavioural tests aren't run by any required check
  [57]="Tier 1"   # ci: cargo-deny (licenses) as a required check
)
# Iterate the map itself (numerically sorted) rather than a hand-kept second
# list — that duplication is what let #41–#49 get added by hand and never make
# it into the script.
echo "Ensuring tracked issues…"
for n in $(printf '%s\n' "${!ISSUE_TIER[@]}" | sort -n); do
  if issue_present "$n"; then
    echo "  = #$n already on board (skip)"
    continue
  fi
  iid=$(gh project item-add "$num" --owner "$OWNER" \
          --url "https://github.com/$REPO/issues/$n" --format json --jq '.id')
  set_tier "$iid" "${ISSUE_TIER[$n]}"
  echo "  + #$n (${ISSUE_TIER[$n]})"
done

# --- 4. Draft items for tier work that is NOT (yet) a tracked issue ----------
# Milestone-gating work is already a real Issue above. Drafts here are the
# lighter tier items that don't gate a milestone (cleanup) or are deferred-by-
# design (no milestone on purpose) — so the board is complete without inventing
# issues for trivia. DONE items are kept as visible markers, not work.
draft() {  # $1 = "Tier N", $2 = title, $3 = body
  if draft_present "$2"; then
    echo "  = draft '$2' already on board (skip)"
    return
  fi
  local iid
  iid=$(gh project item-create "$num" --owner "$OWNER" --title "$2" --body "$3" \
          --format json --jq '.id')
  set_tier "$iid" "$1"
  echo "  + $2"
}
echo "Ensuring draft items…"
draft "Tier 1" "T1.1 Delete merged branches/worktrees" "Reviewed cleanup of the ~180 stale locals. Not a 0.2 gate."
draft "Tier 1" "T1.2 Delete dead Python references" "Scrub torch/lancedb/Python-service doc mentions. Not a 0.2 gate."
draft "Tier 1" "T1.4 Adopt the gpui-pin policy" "Write-it-down; the advisory acceptance is issue #30."
draft "Tier 1" "T1.5 Salvage wylde-fswalk crate" "Prerequisite for Organize (#39)."
draft "Tier 1" "T1.6 min_core compatibility floor — DONE" "Shipped in wylde-lifecycle + GUI."
draft "Tier 1" "T1.7 Artifact homes + lifecycle tracking — DONE" "Private wylde-planning repo + junctions + frontmatter/index/CI; Nextcloud retired."
draft "Tier 2" "T2.3 feat/n8n-embedded-service" "Priority is a product call; not milestoned yet."
draft "Tier 3" "T3.2 Concept query-routing + security P4+ + webcrawler egress" "Post-0.2, deferred."
draft "Tier 3" "T3.3 Mobile app, Wylde IDE, gateway wave-2 / Vault secrets" "Long-horizon, deferred."

echo ""
echo "Done. Open it:  gh project view $num --owner $OWNER --web"
echo "Add a Roadmap (timeline) view in the UI if you want the calendar layout."

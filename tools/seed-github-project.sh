#!/usr/bin/env bash
# seed-github-project.sh — create + seed the private "Wylde Roadmap" GitHub Project.
#
# BLOCKED ON SCOPE: `gh project` needs the `project` scope, which the token did
# not have when this was authored. Run this ONCE, after:
#     gh auth refresh -s project
#
# What it does:
#   1. Creates a Project (v2) "Wylde Roadmap" under PeopleWonder, sets it private.
#   2. Adds custom fields: Tier (single-select Tier 0–3), Target version (text).
#      (Status — Todo/In Progress/Done — exists by default.)
#   3. Adds the 8 filed backlog issues (#25–#32) to the project.
#   4. Creates a draft item per roadmap tier item (T0.0 … T3.3), Tier set.
#   5. Best-effort sets the Tier field on the issues too.
#
# The Roadmap (timeline) *view* is a UI toggle the CLI can't create — after this
# runs, open the project → new view → "Roadmap", or use the Table/Board it ships
# with. Re-running creates a NEW project (not idempotent) — run once.
set -euo pipefail

OWNER="PeopleWonder"
REPO="PeopleWonder/wylde"
TITLE="Wylde Roadmap"

command -v jq >/dev/null || { echo "jq required"; exit 1; }
gh auth status 2>&1 | grep -qi "project" || {
  echo "ERROR: the gh token lacks the 'project' scope. Run:  gh auth refresh -s project"; exit 1; }

echo "Creating project '$TITLE' under $OWNER…"
num=$(gh project create --owner "$OWNER" --title "$TITLE" --format json | jq -r '.number')
pid=$(gh project view "$num" --owner "$OWNER" --format json | jq -r '.id')
echo "  project #$num (id $pid)"

# Try to mark it private (flag availability varies by gh version; non-fatal).
gh project edit "$num" --owner "$OWNER" --visibility PRIVATE 2>/dev/null \
  && echo "  set private" || echo "  (set visibility to Private in the UI if the flag was unavailable)"

echo "Adding fields…"
gh project field-create "$num" --owner "$OWNER" --name "Tier" \
  --data-type SINGLE_SELECT --single-select-options "Tier 0,Tier 1,Tier 2,Tier 3" >/dev/null
gh project field-create "$num" --owner "$OWNER" --name "Target version" \
  --data-type TEXT >/dev/null

# Resolve the Tier field id + option ids for value-setting.
fields_json=$(gh project field-list "$num" --owner "$OWNER" --format json)
tier_fid=$(echo "$fields_json" | jq -r '.fields[] | select(.name=="Tier") | .id')
opt_id() { echo "$fields_json" | jq -r --arg n "$1" '.fields[] | select(.name=="Tier") | .options[] | select(.name==$n) | .id'; }

set_tier() {  # $1 = item id, $2 = "Tier N"
  local iid="$1" oid; oid=$(opt_id "$2")
  [ -n "$oid" ] && gh project item-edit --project-id "$pid" --id "$iid" \
    --field-id "$tier_fid" --single-select-option-id "$oid" >/dev/null 2>&1 || true
}

echo "Adding the tracked issues…"
# The Project auto-populates a built-in Milestone field from each issue, so the
# milestone structure (0.2 gate&hygiene -> verified build -> ship; 0.3) shows up
# without extra work here. We just set the Tier field.
# issue number -> tier
declare -A ISSUE_TIER=(
  [25]="Tier 3"
  [26]="Tier 0" [27]="Tier 0" [28]="Tier 0" [29]="Tier 0"
  [30]="Tier 1" [31]="Tier 1" [32]="Tier 1"
  [33]="Tier 0" [34]="Tier 0" [35]="Tier 0" [36]="Tier 0" [37]="Tier 0" [38]="Tier 0"
  [39]="Tier 2" [40]="Tier 2"
)
for n in 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39 40; do
  iid=$(gh project item-add "$num" --owner "$OWNER" --url "https://github.com/$REPO/issues/$n" --format json | jq -r '.id')
  set_tier "$iid" "${ISSUE_TIER[$n]}"
  echo "  + #$n (${ISSUE_TIER[$n]})"
done

echo "Creating draft items for tier work that is NOT (yet) a tracked issue…"
# Milestone-gating work is already a real Issue above. Drafts here are the
# lighter tier items that don't gate a milestone (cleanup) or are deferred-by-
# design (no milestone on purpose) — so the board is complete without inventing
# issues for trivia.
draft() {  # $1 = "Tier N", $2 = title, $3 = body
  local iid; iid=$(gh project item-create "$num" --owner "$OWNER" --title "$2" --body "$3" --format json | jq -r '.id')
  set_tier "$iid" "$1"
  echo "  + $2"
}

# Tier 1 — cheap hygiene that doesn't gate a milestone (not filed as issues)
draft "Tier 1" "T1.1 Delete merged branches/worktrees" "Reviewed cleanup of the ~180 stale locals. Not a 0.2 gate."
draft "Tier 1" "T1.2 Delete dead Python references" "Scrub torch/lancedb/Python-service doc mentions. Not a 0.2 gate."
draft "Tier 1" "T1.4 Adopt the gpui-pin policy" "Write-it-down; the advisory acceptance is issue #30."
draft "Tier 1" "T1.5 Salvage wylde-fswalk crate" "Prerequisite for Organize (#39)."
# DONE (kept as visible markers, not work)
draft "Tier 1" "T1.6 min_core compatibility floor — DONE" "Shipped in wylde-lifecycle + GUI."
draft "Tier 1" "T1.7 Artifact homes + lifecycle tracking — DONE" "Private wylde-planning repo + junctions + frontmatter/index/CI; Nextcloud retired."

# Tier 2 — post-0.2 (the gating features are issues #39/#40; n8n is a product call)
draft "Tier 2" "T2.3 feat/n8n-embedded-service" "Priority is a product call; not milestoned yet."

# Tier 3 — deferred-by-design (NO milestone on purpose; reasoning-v2 is issue #25)
draft "Tier 3" "T3.2 Concept query-routing + security P4+ + webcrawler egress" "Post-0.2, deferred."
draft "Tier 3" "T3.3 Mobile app, Wylde IDE, gateway wave-2 / Vault secrets" "Long-horizon, deferred."

echo ""
echo "Done. Open it:  gh project view $num --owner $OWNER --web"
echo "Add a Roadmap (timeline) view in the UI if you want the calendar layout."

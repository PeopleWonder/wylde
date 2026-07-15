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

echo "Adding the 8 backlog issues…"
# issue number -> tier
declare -A ISSUE_TIER=( [25]="Tier 3" [26]="Tier 0" [27]="Tier 0" [28]="Tier 0" [29]="Tier 0" [30]="Tier 1" [31]="Tier 1" [32]="Tier 1" )
for n in 25 26 27 28 29 30 31 32; do
  iid=$(gh project item-add "$num" --owner "$OWNER" --url "https://github.com/$REPO/issues/$n" --format json | jq -r '.id')
  set_tier "$iid" "${ISSUE_TIER[$n]}"
  echo "  + #$n (${ISSUE_TIER[$n]})"
done

echo "Creating roadmap tier draft items…"
draft() {  # $1 = "Tier N", $2 = title, $3 = body
  local iid; iid=$(gh project item-create "$num" --owner "$OWNER" --title "$2" --body "$3" --format json | jq -r '.id')
  set_tier "$iid" "$1"
  echo "  + $2"
}

# Tier 0 — release gating + the 0.2 release
draft "Tier 0" "T0.0 Adopt branch model + migrate (prerequisite)" "Rename trunk->develop, default->develop, protect main+develop, freeze main until 0.2. See docs/enforcement-matrix.md Aaron-actions."
draft "Tier 0" "T0.1 Build the release gate (G4-G7 + local preflight L1-L3 + release-checklist)" "The launch-and-verify one-command preflight + receipt that publish refuses to run without."
draft "Tier 0" "T0.1b GUI verification suite (L7 panel-walk + selective control checks)" "Extend the gpui harness across all 9 panels + Workspaces subtabs."
draft "Tier 0" "T0.2 Bump + reconcile versions to the 0.2 scheme; refresh CHANGELOG/RELEASE_NOTES" "0.1.x experimental -> 0.2.0 stable; G7 enforces."
draft "Tier 0" "T0.3 Rebuild + redeploy from trunk, then run the preflight" "Proves RAG/Neo4j/GUI fixes live."
draft "Tier 0" "T0.4 Fix whatever the preflight surfaces (scope-on-discovery)" "The honest stabilisation bucket."
draft "Tier 0" "T0.5 Re-index/purge the move-stale workspace" "Linked issue: workspace index pins a stale path (#28)."
draft "Tier 0" "T0.6 Tag + wylde-release publish once the 0.2 gate is green" "Verify the updater distributes it."

# Tier 1 — consolidation & hygiene
draft "Tier 1" "T1.1 Delete merged branches/worktrees" "Reviewed cleanup of the ~180 stale locals."
draft "Tier 1" "T1.2 Delete dead Python references" "Scrub torch/lancedb/Python-service doc mentions."
draft "Tier 1" "T1.3 Scrub move-stale docs" "Linked issue: stale old-vault path refs (#31)."
draft "Tier 1" "T1.4 Adopt the gpui-pin policy" "Write-it-down; linked issue: pinned advisories (#30)."
draft "Tier 1" "T1.5 Salvage wylde-fswalk crate" "Prerequisite for T2.1 Organize."
draft "Tier 1" "T1.6 min_core compatibility floor — DONE" "Shipped in wylde-lifecycle + GUI. Add floor to the external service manifests when touched."
draft "Tier 1" "T1.7 Resolve artifact homes — DONE (planning repo + junctions), enable G4/G6 (#32)" "Private wylde-planning repo created + junctioned; Nextcloud retired."

# Tier 2 — feature forward-ports (post-0.2)
draft "Tier 2" "T2.1 Organize + Tabulate panels" "Build the external services, drop binaries, forward-port branches."
draft "Tier 2" "T2.2 feat/temporal-memory-graph (bi-temporal edges, gated)" "Forward-port, ship gated, prove identity."
draft "Tier 2" "T2.3 feat/n8n-embedded-service" "Priority is a product call."

# Tier 3 — deferred-by-design
draft "Tier 3" "T3.1 Reasoning tier v2 (kill-criterion-gated)" "Linked issue: planner/executor vocab mismatch (#25). Off by default; blocks nothing."
draft "Tier 3" "T3.2 Concept query-routing + security P4+ + webcrawler egress" "Post-0.2."
draft "Tier 3" "T3.3 Mobile app, Wylde IDE, gateway wave-2 / Vault secrets" "Long-horizon."

echo ""
echo "Done. Open it:  gh project view $num --owner $OWNER --web"
echo "Add a Roadmap (timeline) view in the UI if you want the calendar layout."

#!/usr/bin/env bash
# check-manifest-coverage.sh — the gate that makes a forgotten workspace edit
# turn CI RED instead of shipping silently (#122).
#
# tools/list-workspaces.sh is the single source of truth for the GATED workspace
# set. Four things must stay in lockstep with it, and before #122 none of them
# failed if they drifted:
#
#   1. every gated workspace resolves to a deny.toml (advisory + license policy);
#   2. the cargo-deny advisory AND license matrices scan every gated manifest;
#   3. check-versions.sh (G7) derives its set from list-workspaces.sh;
#   4. .github/dependabot.yml tracks every gated workspace directory;
#   5. both branch rulesets require the cargo-deny contexts for every gated
#      manifest (so a new leg cannot be silently advisory).
#
# This script asserts all five. Add a workspace and forget any one edit and this
# gate goes red with an actionable message — that is the whole point of the
# issue. Pure bash + grep, no jq/yq (this repo dropped its jq dependency in #51).
#
#   tools/check-manifest-coverage.sh              # check the repo; exit 1 on any gap
#   tools/check-manifest-coverage.sh --selftest   # prove discovery catches a NEW workspace
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
lister="$repo_root/tools/list-workspaces.sh"

fail=0
note() { printf 'FAIL: %s\n' "$1" >&2; fail=1; }

sec_audit="$repo_root/.github/workflows/security-audit.yml"
lic_check="$repo_root/.github/workflows/license-check.yml"
versions="$repo_root/tools/check-versions.sh"
dependabot="$repo_root/.github/dependabot.yml"
rulesets=(
  "$repo_root/.github/rulesets/protect-develop.json"
  "$repo_root/.github/rulesets/protect-main.json"
)

# grep for a matrix entry `- <manifest>` (leading dash, optional whitespace).
matrix_has() {
  local file="$1" manifest="$2"
  grep -qE "^[[:space:]]*-[[:space:]]+${manifest//\//\\/}[[:space:]]*$" "$file"
}

# grep for the exact required-status-check context string.
ruleset_has_context() {
  local file="$1" context="$2"
  grep -qF "\"context\": \"$context\"" "$file"
}

check_repo() {
  local manifests
  mapfile -t manifests < <("$lister")
  if [[ ${#manifests[@]} -eq 0 ]]; then
    note "list-workspaces.sh returned no gated workspaces — discovery is broken"
    return
  fi
  printf 'Gated workspaces (%d): %s\n' "${#manifests[@]}" "${manifests[*]}"

  # 3. check-versions.sh must derive its set from the source of truth, not a
  #    hardcoded two-entry list.
  if ! grep -q 'list-workspaces.sh' "$versions"; then
    note "check-versions.sh (G7) does not read tools/list-workspaces.sh — its workspace set is hardcoded and will drift"
  fi

  local m dir ctx_adv ctx_lic
  for m in "${manifests[@]}"; do
    dir="$(dirname "$m")"

    # 1. deny.toml resolvable.
    if ! "$lister" --deny-config "$m" >/dev/null 2>&1; then
      note "$m has no deny.toml (adjacent or inherited) — cargo-deny cannot scan it"
    fi

    # 2. present in BOTH cargo-deny matrices.
    matrix_has "$sec_audit" "$m"  || note "$m missing from the cargo-deny ADVISORY matrix (.github/workflows/security-audit.yml) — no vulnerability scan"
    matrix_has "$lic_check" "$m"  || note "$m missing from the cargo-deny LICENSE matrix (.github/workflows/license-check.yml) — no GPLv3 scan"

    # 4. tracked by dependabot.
    grep -qE "\"/${dir//\//\\/}\"" "$dependabot" || note "workspace dir /$dir missing from .github/dependabot.yml — no dependency updates"

    # 5. required contexts in BOTH rulesets.
    ctx_adv="cargo-deny (advisories) ($m)"
    ctx_lic="cargo-deny (licenses) ($m)"
    for rs in "${rulesets[@]}"; do
      ruleset_has_context "$rs" "$ctx_adv" || note "ruleset $(basename "$rs") does not require \"$ctx_adv\" — the leg is advisory, not blocking"
      ruleset_has_context "$rs" "$ctx_lic" || note "ruleset $(basename "$rs") does not require \"$ctx_lic\" — the leg is advisory, not blocking"
    done
  done
}

# Prove the mechanism: a brand-new workspace root in a fixture tree must be
# discovered (and, being absent from every list here, would be flagged). This is
# the "add a scratch seventh workspace" falsification from the issue, run against
# a throwaway tree so nothing is left in the repo.
selftest() {
  local fixture rc=0 got
  fixture="$(mktemp -d)"
  mkdir -p "$fixture/rust" "$fixture/scratch-seventh" "$fixture/rust/spikes/voice-npu-spike"
  printf '[workspace]\nmembers=[]\n'        > "$fixture/rust/Cargo.toml"
  printf '[workspace]\nmembers=[]\n'        > "$fixture/scratch-seventh/Cargo.toml"
  printf '[package]\nname="spike"\n[workspace]\n' > "$fixture/rust/spikes/voice-npu-spike/Cargo.toml"

  got="$(WYLDE_SCAN_ROOT="$fixture" "$lister")"
  if ! grep -qx 'scratch-seventh/Cargo.toml' <<<"$got"; then
    echo "SELFTEST FAIL: discovery did not find the new workspace root" >&2
    echo "got: $got" >&2
    rc=1
  elif grep -qx 'rust/spikes/voice-npu-spike/Cargo.toml' <<<"$got"; then
    echo "SELFTEST FAIL: the documented exclusion leaked into the gated set" >&2
    rc=1
  else
    echo "selftest OK: a new [workspace] root is discovered; the documented exclusion is honored"
  fi
  rm -rf "$fixture"
  return $rc
}

if [[ "${1:-}" == "--selftest" ]]; then
  selftest
  exit $?
fi

check_repo
if [[ $fail -ne 0 ]]; then
  echo "" >&2
  echo "manifest coverage: FAIL — a gated workspace is not covered by every gate above." >&2
  echo "Fix the named list(s), or add the workspace to the documented exclusions in tools/list-workspaces.sh." >&2
  exit 1
fi
echo "manifest coverage: PASS — every gated workspace is scanned, versioned, tracked, and required."

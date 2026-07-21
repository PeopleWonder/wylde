#!/usr/bin/env bash
# check-actions-pinned.sh — the gate that makes a tag-pinned GitHub Action turn
# CI RED instead of shipping a mutable supply-chain dependency silently (#127).
#
# Every third-party `uses:` in .github/workflows/ must be pinned to a full
# 40-hex commit SHA, not a mutable tag (`@v4`) or branch (`@main`). A tag can be
# repointed by its upstream owner at any commit; where a job holds write scopes
# — dependabot-automerge.yml carries `contents: write` + `pull-requests: write`
# and a live GITHUB_TOKEN — "the tag moved under us" converts directly into repo
# write access with no code review in the path. SHA-pinning freezes the exact
# reviewed commit; Dependabot's github-actions ecosystem keeps the pins current
# and rewrites the trailing `# vN` comment on each bump.
#
# Allowed forms for a `uses:` ref:
#   * owner/repo@<40-hex-sha>            (optionally owner/repo/subdir@<sha>)
#   * docker://image@sha256:<64-hex>     (digest-pinned container action)
#   * ./path or reusable local workflow  (first-party, no external mutability)
# Anything else — a version tag, a branch, a short SHA — is a violation.
#
#   tools/check-actions-pinned.sh              # check the repo; exit 1 on any tag pin
#   tools/check-actions-pinned.sh --selftest   # prove the validator catches a tag pin
#
# Pure bash + grep, no jq/yq (this repo dropped its jq dependency in #51).
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workflows_dir="$repo_root/.github/workflows"

# is_pinned <ref> -> 0 if the ref is acceptably pinned, 1 otherwise.
# <ref> is the token after `uses:`, with any inline `# comment` already stripped.
is_pinned() {
  local ref="$1"
  case "$ref" in
    # First-party local action or reusable workflow — no external tag to move.
    ./*|.\\*) return 0 ;;
    # Digest-pinned container action.
    docker://*@sha256:*)
      [[ "$ref" =~ @sha256:[0-9a-f]{64}$ ]] && return 0 || return 1 ;;
  esac
  # owner/repo@<sha> or owner/repo/subdir@<sha>, exactly 40 lowercase hex.
  [[ "$ref" =~ ^[^@[:space:]]+@[0-9a-f]{40}$ ]] && return 0 || return 1
}

# scan_dir <dir> -> prints "file:line: ref" for each unpinned uses:, exit status
# reflects nothing (caller counts lines). Comments and blank uses are skipped.
scan_unpinned() {
  local dir="$1"
  # Match `uses:` anywhere it can legally appear (steps and job-level reusable
  # workflows). Strip through `uses:`, drop any trailing ` # comment`, trim.
  grep -rEn '^[[:space:]]*(-[[:space:]]+)?uses:[[:space:]]*[^[:space:]#]' "$dir" 2>/dev/null \
    | while IFS= read -r hit; do
        local loc ref
        loc="$(printf '%s' "$hit" | sed -E 's/:[[:space:]]*(-[[:space:]]+)?uses:.*$//')"
        ref="$(printf '%s' "$hit" | sed -E 's/^.*uses:[[:space:]]*//; s/[[:space:]]*#.*$//; s/[[:space:]]*$//')"
        [ -z "$ref" ] && continue
        if ! is_pinned "$ref"; then
          printf '%s: %s\n' "$loc" "$ref"
        fi
      done
}

selftest() {
  local fail=0
  # A mutable tag, a branch, and a short SHA must all be rejected.
  for bad in \
    "actions/checkout@v4" \
    "actions/checkout@v7.0.0" \
    "some/action@main" \
    "some/action@11d5960"; do
    if is_pinned "$bad"; then
      echo "selftest FAIL: '$bad' was accepted but is not SHA-pinned" >&2
      fail=1
    fi
  done
  # A full-SHA pin, a subdir SHA pin, a local action, and a digest must pass.
  for ok in \
    "actions/checkout@11d5960a326750d5838078e36cf38b85af677262" \
    "owner/repo/sub@11d5960a326750d5838078e36cf38b85af677262" \
    "./.github/actions/local" \
    "docker://alpine@sha256:$(printf 'a%.0s' {1..64})"; do
    if ! is_pinned "$ok"; then
      echo "selftest FAIL: '$ok' was rejected but is correctly pinned" >&2
      fail=1
    fi
  done
  if [ "$fail" -ne 0 ]; then
    echo "selftest FAILED — the validator does not enforce SHA pinning" >&2
    exit 1
  fi
  echo "selftest OK: tag/branch/short-SHA refs are rejected; full-SHA and local refs pass"
}

if [[ "${1:-}" == "--selftest" ]]; then
  selftest
  exit 0
fi

unpinned="$(scan_unpinned "$workflows_dir" || true)"
if [ -n "$unpinned" ]; then
  echo "::error::Unpinned GitHub Action(s) — pin each to a full 40-hex commit SHA (see tools/check-actions-pinned.sh header, #127):" >&2
  printf '%s\n' "$unpinned" >&2
  echo "" >&2
  echo "Resolve a tag to its SHA with:  gh api repos/<owner>/<repo>/commits/<tag> --jq .sha" >&2
  echo "then write:  uses: <owner>/<repo>@<sha> # <tag>" >&2
  exit 1
fi

echo "OK: every third-party action under .github/workflows/ is pinned to a commit SHA."

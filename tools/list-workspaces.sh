#!/usr/bin/env bash
# list-workspaces.sh — the ONE source of truth for "which Cargo workspaces does
# this repo have, and which of them are gated?".
#
# Why this exists (#122): the repo has several `[workspace]` roots, and at least
# three independent hand-kept lists (the cargo-deny advisory matrix, the
# cargo-deny license matrix, and check-versions.sh) each enumerated only TWO of
# them. Adding a workspace meant remembering to edit every list, and NONE of the
# lists failed if the edit was skipped — so a shipped release tool could carry a
# vulnerable or copyleft-incompatible dependency with every required check green.
#
# This script discovers the workspace roots from the tree. `check-versions.sh`
# and `check-manifest-coverage.sh` both read it, so the enumeration lives in one
# place and drift is a red gate (see tools/check-manifest-coverage.sh) rather
# than a silent omission.
#
# A "workspace root" is a Cargo.toml containing a `[workspace]` table. The GATED
# set is every root MINUS the documented exclusions below.
#
# Usage:
#   tools/list-workspaces.sh                 # gated workspace manifest paths, one per line
#   tools/list-workspaces.sh --all           # every root; excluded ones prefixed "# EXCLUDED "
#   tools/list-workspaces.sh --dirs          # gated workspace directories (manifest dir), one per line
#   tools/list-workspaces.sh --json          # gated manifest paths as a compact JSON array
#   tools/list-workspaces.sh --deny-config M # nearest deny.toml for manifest M (walking up); exit 1 if none
#
# Honors WYLDE_SCAN_ROOT (defaults to the repo root inferred from this script's
# location) so the coverage self-test can point it at a fixture tree.
set -euo pipefail

scan_root="${WYLDE_SCAN_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"

# --- Documented exclusions ---------------------------------------------------
# Workspace roots that are deliberately OUTSIDE every dependency/version gate.
# A path listed here MUST carry a reason. Anything not listed is gated.
#
#   rust/spikes/voice-npu-spike — an experimental NPU spike. Not CI-built (no
#     build/test job), version-pinned at 0.0.0 on purpose, and never shipped.
#     Documented as excluded from G4/G6 in ci.yml too.
is_excluded() {
  case "$1" in
    rust/spikes/voice-npu-spike/Cargo.toml) return 0 ;;
    *) return 1 ;;
  esac
}

# Print every Cargo.toml (repo-relative) that declares a [workspace] table.
all_roots() {
  # -F for a literal match on the table header at column 0.
  while IFS= read -r -d '' f; do
    if grep -qE '^\[workspace\]' "$f"; then
      printf '%s\n' "${f#"$scan_root/"}"
    fi
  done < <(find "$scan_root" -name Cargo.toml -type f -print0) | sort
}

gated() {
  local root
  while IFS= read -r root; do
    is_excluded "$root" || printf '%s\n' "$root"
  done < <(all_roots)
}

# Nearest deny.toml walking up from a manifest's directory to the repo root.
# Lets the tools/ workspaces share one tools/deny.toml instead of each carrying
# a near-duplicate copy of the ~90-line policy.
deny_config() {
  local manifest="$1"
  local dir
  dir="$(dirname "$manifest")"
  while :; do
    if [[ -f "$scan_root/$dir/deny.toml" ]]; then
      printf '%s\n' "$dir/deny.toml"
      return 0
    fi
    [[ "$dir" == "." || "$dir" == "/" || -z "$dir" ]] && break
    dir="$(dirname "$dir")"
  done
  if [[ -f "$scan_root/deny.toml" ]]; then
    printf 'deny.toml\n'
    return 0
  fi
  echo "no deny.toml found for $manifest (searched its dir up to the repo root)" >&2
  return 1
}

cmd="${1:-}"
case "$cmd" in
  ""|--paths)
    gated
    ;;
  --all)
    while IFS= read -r root; do
      if is_excluded "$root"; then
        printf '# EXCLUDED %s\n' "$root"
      else
        printf '%s\n' "$root"
      fi
    done < <(all_roots)
    ;;
  --dirs)
    gated | while IFS= read -r m; do dirname "$m"; done
    ;;
  --json)
    # Compact JSON array, no external jq dependency.
    printf '['
    first=1
    while IFS= read -r m; do
      [[ $first -eq 1 ]] && first=0 || printf ','
      printf '"%s"' "$m"
    done < <(gated)
    printf ']\n'
    ;;
  --deny-config)
    [[ $# -ge 2 ]] || { echo "usage: $0 --deny-config <manifest>" >&2; exit 2; }
    deny_config "$2"
    ;;
  *)
    echo "unknown argument: $cmd" >&2
    exit 2
    ;;
esac

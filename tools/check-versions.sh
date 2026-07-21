#!/usr/bin/env bash
# check-versions.sh — G7, the version-consistency gate.
#
# Wylde is several separate Cargo workspaces (rust/, Core/GUI/, and the two
# tools/ crates) that cannot share a single [workspace.package] version, so the
# version literal lives in every root and MUST move together. A split version is
# exactly the silent inconsistency release gating exists to catch (a GUI stamped
# 0.1.x shipped alongside a backend stamped 0.2.0 is a support nightmare).
#
# The set of workspaces checked is NOT hardcoded here — it is discovered by
# tools/list-workspaces.sh, the one source of truth (#122). Before #122 this
# script read exactly two roots, so tools/xtask and tools/wylde-release carried
# their own versions unchecked and a split there passed G7 green.
#
# This script enforces:
#   1. ALWAYS   — every gated workspace root carries the SAME version.
#   2. ON A TAG — when GITHUB_REF is refs/tags/vX.Y.Z, the tag (minus leading v)
#                 == that shared version.
#
# Usage:
#   tools/check-versions.sh            # consistency check (+ tag check if GITHUB_REF is a tag)
#   GITHUB_REF=refs/tags/v0.2.0 tools/check-versions.sh
#
# Exit 0 = green, exit 1 = mismatch. Portable POSIX-ish bash; runs on the
# git-bash shell available on the windows-latest CI runner.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Extract the version literal a workspace root carries. rust/ and Core/GUI/ set
# it under [workspace.package]; the single-package tools/ workspaces set it under
# [package]. Prefer [workspace.package]; fall back to the first [package] version.
extract_version() {
  local file="$1"
  awk '
    /^\[workspace\.package\]/ { in_wp = 1; in_pkg = 0; next }
    /^\[package\]/            { in_pkg = 1; in_wp = 0; next }
    /^\[/                     { in_wp = 0; in_pkg = 0 }
    in_wp && /^version[[:space:]]*=/ {
      match($0, /"[^"]*"/); print substr($0, RSTART + 1, RLENGTH - 2); exit
    }
    in_pkg && /^version[[:space:]]*=/ && wp_ver == "" {
      match($0, /"[^"]*"/); pkg_ver = substr($0, RSTART + 1, RLENGTH - 2)
    }
    END { if (pkg_ver != "") print pkg_ver }
  ' "$file"
}

mapfile -t manifests < <(bash "$repo_root/tools/list-workspaces.sh")
if [[ ${#manifests[@]} -eq 0 ]]; then
  echo "FAIL: tools/list-workspaces.sh returned no gated workspaces" >&2
  exit 1
fi

ref_ver=""
ref_file=""
status=0
for m in "${manifests[@]}"; do
  ver="$(extract_version "$repo_root/$m")"
  if [[ -z "$ver" ]]; then
    echo "FAIL: could not read a version from $m" >&2
    status=1
    continue
  fi
  printf '%-34s version = %s\n' "$m" "$ver"
  if [[ -z "$ref_ver" ]]; then
    ref_ver="$ver"; ref_file="$m"
  elif [[ "$ver" != "$ref_ver" ]]; then
    echo "FAIL: $m version ($ver) differs from $ref_file ($ref_ver). Bump every workspace root together." >&2
    status=1
  fi
done
[[ $status -ne 0 ]] && exit 1
echo "OK: all ${#manifests[@]} workspace versions agree ($ref_ver)."

# Tag check — only when building a tag.
ref="${GITHUB_REF:-}"
if [[ "$ref" == refs/tags/* ]]; then
  tag="${ref#refs/tags/}"
  tag_ver="${tag#v}"   # strip a leading v: v0.2.0 -> 0.2.0
  echo "Tag build detected: $tag (version $tag_ver)"
  if [[ "$tag_ver" != "$ref_ver" ]]; then
    echo "FAIL: tag $tag does not match workspace version $ref_ver." >&2
    echo "      A release tag must equal the version stamped in the workspaces." >&2
    exit 1
  fi
  echo "OK: tag matches workspace version."
fi

echo "G7 version-consistency: PASS"

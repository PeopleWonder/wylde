#!/usr/bin/env bash
# check-versions.sh — G7, the version-consistency gate.
#
# Wylde is two separate Cargo workspaces (rust/ and Core/GUI/) that cannot share
# a single [workspace.package] version, so the version literal lives in two
# places and MUST move together. A split version is exactly the silent
# inconsistency that release gating exists to catch (a GUI stamped 0.1.x shipped
# alongside a backend stamped 0.2.0 is a support nightmare).
#
# This script enforces:
#   1. ALWAYS   — rust/Cargo.toml [workspace.package] version
#                 == Core/GUI/Cargo.toml [workspace.package] version
#   2. ON A TAG — when GITHUB_REF is refs/tags/vX.Y.Z, the tag (minus leading v)
#                 == that workspace version.
#
# Usage:
#   tools/check-versions.sh            # consistency check (+ tag check if GITHUB_REF is a tag)
#   GITHUB_REF=refs/tags/v0.2.0 tools/check-versions.sh
#
# Exit 0 = green, exit 1 = mismatch. Portable POSIX-ish bash; runs on the
# git-bash shell available on the windows-latest CI runner.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Extract the `version = "..."` that immediately follows [workspace.package].
extract_version() {
  local file="$1"
  awk '
    /^\[workspace\.package\]/ { in_wp = 1; next }
    /^\[/                     { in_wp = 0 }
    in_wp && /^version[[:space:]]*=/ {
      # version = "0.1.0-alpha.1"  ->  0.1.0-alpha.1
      match($0, /"[^"]*"/)
      print substr($0, RSTART + 1, RLENGTH - 2)
      exit
    }
  ' "$file"
}

rust_toml="$repo_root/rust/Cargo.toml"
gui_toml="$repo_root/Core/GUI/Cargo.toml"

rust_ver="$(extract_version "$rust_toml")"
gui_ver="$(extract_version "$gui_toml")"

if [[ -z "$rust_ver" ]]; then
  echo "FAIL: could not read [workspace.package] version from $rust_toml" >&2
  exit 1
fi
if [[ -z "$gui_ver" ]]; then
  echo "FAIL: could not read [workspace.package] version from $gui_toml" >&2
  exit 1
fi

echo "rust/Cargo.toml       version = $rust_ver"
echo "Core/GUI/Cargo.toml   version = $gui_ver"

if [[ "$rust_ver" != "$gui_ver" ]]; then
  echo "FAIL: workspace versions differ ($rust_ver vs $gui_ver). Bump both roots together." >&2
  exit 1
fi
echo "OK: workspace versions agree."

# Tag check — only when building a tag.
ref="${GITHUB_REF:-}"
if [[ "$ref" == refs/tags/* ]]; then
  tag="${ref#refs/tags/}"
  tag_ver="${tag#v}"   # strip a leading v: v0.2.0 -> 0.2.0
  echo "Tag build detected: $tag (version $tag_ver)"
  if [[ "$tag_ver" != "$rust_ver" ]]; then
    echo "FAIL: tag $tag does not match workspace version $rust_ver." >&2
    echo "      A release tag must equal the version stamped in the workspaces." >&2
    exit 1
  fi
  echo "OK: tag matches workspace version."
fi

echo "G7 version-consistency: PASS"

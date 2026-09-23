#!/usr/bin/env bash
# changelog-draft.sh — generate a Keep-a-Changelog DRAFT from Conventional Commits.
#
# Wylde's CHANGELOG is hand-curated and narrative — deliberately richer than any
# auto-generated bullet list. This tool does NOT replace it. It produces a
# *starting draft* from the conventional commits since the last tag, grouped into
# Keep-a-Changelog sections, which the maintainer edits into shape before a
# release. Curation stays human; the mechanical "what landed since the last tag"
# gathering is automated.
#
# Usage:
#   tools/changelog-draft.sh                # commits since the most recent tag
#   tools/changelog-draft.sh v0.1.0-alpha.1 # commits since a specific tag/ref
#
# Conventional Commit mapping (Keep-a-Changelog section):
#   feat:            -> Added
#   fix:             -> Fixed
#   perf:            -> Changed (performance)
#   refactor:        -> Changed
#   feat!: / BREAKING-> Changed (breaking; flagged)
#   docs/chore/test/ci/build -> omitted from the user-facing changelog by default
#                               (shown under a collapsed "Internal" note for review)
set -euo pipefail

since="${1:-$(git describe --tags --abbrev=0 2>/dev/null || true)}"
if [[ -n "$since" ]]; then
  range="${since}..HEAD"
  echo "## [Unreleased] — draft (commits since ${since})"
else
  range="HEAD"
  echo "## [Unreleased] — draft (no prior tag; full history)"
fi
echo

# Pull "type(scope): subject" subjects in the range.
mapfile -t lines < <(git log --no-merges --pretty=format:'%s' ${range} 2>/dev/null || true)

added=(); fixed=(); changed=(); breaking=(); internal=()
for s in "${lines[@]}"; do
  # Detect a breaking change: `type!:` or a `BREAKING CHANGE` footer isn't in the
  # subject, so we only catch the `!` form here.
  case "$s" in
    feat!*|feat\(*\)!:*) breaking+=("$s") ;;
    fix!*|fix\(*\)!:*)   breaking+=("$s") ;;
    feat:*|feat\(*\):*)  added+=("$s") ;;
    fix:*|fix\(*\):*)    fixed+=("$s") ;;
    perf:*|perf\(*\):*)  changed+=("$s") ;;
    refactor:*|refactor\(*\):*) changed+=("$s") ;;
    docs:*|docs\(*\):*|chore:*|chore\(*\):*|test:*|test\(*\):*|ci:*|ci\(*\):*|build:*|build\(*\):*) internal+=("$s") ;;
    *) internal+=("$s") ;;  # non-conventional subjects land in Internal for triage
  esac
done

print_section() {
  local title="$1"; shift
  local -a items=("$@")
  [[ ${#items[@]} -eq 0 ]] && return
  echo "### ${title}"
  echo
  for i in "${items[@]}"; do echo "- ${i}"; done
  echo
}

print_section "⚠ Breaking" "${breaking[@]:-}"
print_section "Added"      "${added[@]:-}"
print_section "Changed"    "${changed[@]:-}"
print_section "Fixed"      "${fixed[@]:-}"

if [[ ${#internal[@]} -gt 0 ]]; then
  echo "<!-- Internal (docs/chore/test/ci/build; usually omitted from the user-facing changelog): -->"
  for i in "${internal[@]}"; do echo "<!-- - ${i} -->"; done
  echo
fi

echo "<!-- DRAFT — edit into narrative Keep-a-Changelog form before releasing. -->"

# build-all.ps1 -- thin wrapper around the all-Rust `cargo xtask build-all`
# multi-workspace build orchestrator (out-of-tree runtime foundation).
#
# The LOGIC is all-Rust and lives in tools/xtask (per locked decision 3:
# "A thin .ps1 may invoke it but the logic is Rust."). This script only
# forwards its arguments from the repo root so the cargo alias / relative
# --manifest-path resolves. Examples:
#
#   tools\build-all.ps1                # release build of Core + all buckets
#   tools\build-all.ps1 -- --debug     # debug profile
#   tools\build-all.ps1 -- --skip-gui  # backend + buckets only
#
$ErrorActionPreference = 'Stop'
$RepoRoot = Split-Path -Parent $PSScriptRoot
Push-Location $RepoRoot
try {
    & cargo run --manifest-path tools/xtask/Cargo.toml -- build-all @args
    $code = $LASTEXITCODE
} finally {
    Pop-Location
}
exit $code

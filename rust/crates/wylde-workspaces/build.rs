// Fail loud at build-script time if `wylde-workspaces.exe` is still alive
// in the process table — otherwise the linker explodes with `os error 32`
// (sharing violation) deep into the build. See `wylde-prebuild-guard` for
// the full policy.
//
// Slice 0a placeholder: there is nothing to compile here yet (grammars live
// in `wylde-treesitter`; later slices may add codegen). The guard is the
// only build-time concern for now.
fn main() {
    wylde_prebuild_guard::run_prebuild_guard("wylde-workspaces");
}

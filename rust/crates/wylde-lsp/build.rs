// Fail loud at build-script time if `wylde-lsp.exe` is still alive in the
// process table — otherwise the linker explodes with `os error 32` (sharing
// violation) deep into a build. See `wylde-prebuild-guard` for the policy.
fn main() {
    wylde_prebuild_guard::run_prebuild_guard("wylde-lsp");
}

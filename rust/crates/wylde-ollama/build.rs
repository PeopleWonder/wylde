// Fail loud at build-script time if `wylde-ollama.exe` is still alive
// in the process table — otherwise the linker explodes with `os error
// 32` (sharing violation) deep into the build. See `wylde-prebuild-guard`
// for the full policy.
fn main() {
    wylde_prebuild_guard::run_prebuild_guard("wylde-ollama");
}

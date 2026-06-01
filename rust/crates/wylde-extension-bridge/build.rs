// Fail loud at build-script time if `wylde-extension-bridge.exe` is still
// alive in the process table — otherwise the linker explodes with
// `os error 32` (sharing violation) deep into the build.
fn main() {
    wylde_prebuild_guard::run_prebuild_guard("wylde-extension-bridge");
}

// Fail loud at build-script time if `wylde-ext-webcrawler.exe` is still alive
// in the process table — otherwise the linker explodes with `os error 32`
// (sharing violation) deep into a build. See `wylde-prebuild-guard` for the
// full policy. Same shape as every other service crate.
fn main() {
    wylde_prebuild_guard::run_prebuild_guard("wylde-ext-webcrawler");
}

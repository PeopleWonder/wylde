// Fail loud at build-script time if a wylde-* daemon is still alive in
// the process table — otherwise the linker would explode with an
// `os error 32` sharing-violation halfway through the build. See the
// `wylde-prebuild-guard` crate doc for the full policy.
fn main() {
    wylde_prebuild_guard::run_prebuild_guard("wylde-vram-broker");
}

//! Diagnostic: print `system.inventory` payload as pretty JSON.
//!
//! Useful when validating the broker's hardware probe matches what the
//! GUI wizard sees. Run with:
//!     cargo run -p wylde-vram-broker --example print_inventory
//!
//! Not built by default; exists purely for the Phase-12.2 sample
//! attached to the slice's final report.

use wylde_vram_broker::{inventory, registry};

fn main() {
    // Initialise the NVML bridge so the GPU probe sees the device on
    // hosts that have one. The broker's `install` path normally does
    // this; the example has to call it explicitly.
    registry::init_nvml();
    let v = inventory::inventory_payload();
    let pretty = serde_json::to_string_pretty(&v).expect("inventory always serialises");
    println!("{pretty}");
}

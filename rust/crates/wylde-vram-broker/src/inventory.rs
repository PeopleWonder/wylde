//! Phase-12.2 host hardware inventory (Phase-12.4: non-NVIDIA fields).
//!
//! `system.inventory` is the action a first-run-bootstrap LLM (or any
//! caller that needs to make hardware-aware decisions) calls to learn
//! the shape of the box Wylde is running on: CPU, RAM, disk volume(s),
//! GPU(s), NPU presence, OS. The broker is the right home because it
//! already owns the NVML bridge for GPU info and the sysinfo bridge for
//! RAM — the inventory is just a wider sampling of the same hardware
//! probes plus a few additions (CPU brand, disk kind/space, OS name).
//!
//! ## Probe sourcing
//!
//! | field                  | source                                      |
//! |------------------------|---------------------------------------------|
//! | cpu                    | `sysinfo::System::cpus()`                   |
//! | memory                 | `sysinfo::System::total_memory()`           |
//! | disks                  | `sysinfo::Disks::new_with_refreshed_list`   |
//! | gpus (NVIDIA)          | NVML via [`crate::registry`]                |
//! | intel_gpus / amd_gpus  | DXGI `IDXGIFactory1::EnumAdapters1` (Win)   |
//! | npus                   | heuristic on CPU brand string               |
//! | npu (legacy)           | populated from `npus[0]` when present       |
//! | os                     | `sysinfo::System::name/os_version`          |
//! | arch                   | `std::env::consts::ARCH`                    |
//!
//! ## Non-NVIDIA GPUs (Phase 12.4)
//!
//! The average target box has integrated Intel graphics, occasionally a
//! Ryzen APU, rarely a discrete NVIDIA card. NVML covers only NVIDIA, so
//! before Phase 12.4 the broker reported `gpus: []` on the majority of
//! installs and the bootstrap LLM defaulted to CPU-only routing. The
//! DXGI walk fills the gap — it enumerates every adapter the Windows
//! display subsystem knows about and we partition the results by vendor
//! id (`0x8086` Intel, `0x1002` AMD). NVIDIA adapters also surface
//! through DXGI but we ignore them here because NVML is the higher-
//! fidelity source. Software / WARP adapters (`DXGI_ADAPTER_FLAG_SOFTWARE`)
//! are skipped.
//!
//! Linux is not the primary target; the inventory module must not
//! panic there, so the DXGI path is `#[cfg(windows)]` only and the
//! Linux fallback walks `/sys/class/drm/card*/device/vendor`.
//!
//! ## NPU caveat
//!
//! Pure-Rust NPU detection on Windows needs SetupAPI (`SetupDiGetClassDevs`
//! with `GUID_DEVCLASS_COMPUTEACCELERATOR`) which pulls in a large
//! windows-rs feature surface for one bit of information. Phase 12.4
//! instead ships a heuristic on the CPU brand string:
//!
//! * Brand contains "Core Ultra" → Intel AI Boost (Meteor / Lunar / Arrow
//!   Lake). Every Core Ultra SKU released to date includes the NPU, so
//!   the heuristic has no known false positives.
//! * Brand contains "Ryzen AI" → AMD XDNA (Strix Point Ryzen AI 300+).
//!   This *misses* older Phoenix chips (7040-series) that have an NPU
//!   but aren't branded "Ryzen AI"; the bootstrap LLM is told this via
//!   the `note` field so it can downgrade confidence accordingly.
//!
//! The heuristic surfaces via the new `npus` array, with `source:
//! "heuristic"` flagged on every entry. The legacy `npu` (singular)
//! field continues to mirror `npus[0]` so existing consumers of the
//! `present` boolean keep working.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::registry::{refresh_nvml, refresh_sysinfo, registry};

/// Intel vendor id on the PCI bus — `0x8086`, the canonical "Intel"
/// PCI vendor code (yes, named after the 8086).
pub const VENDOR_ID_INTEL: u32 = 0x8086;
/// AMD vendor id on the PCI bus — `0x1002`, originally ATI's code,
/// inherited by AMD after the 2006 acquisition.
pub const VENDOR_ID_AMD: u32 = 0x1002;
/// NVIDIA vendor id on the PCI bus — used to filter NVIDIA adapters
/// out of the DXGI walk (NVML is the higher-fidelity source for them).
pub const VENDOR_ID_NVIDIA: u32 = 0x10DE;
/// Microsoft vendor id — used by the WARP software adapter, which we
/// also skip via the `DXGI_ADAPTER_FLAG_SOFTWARE` flag but checking the
/// vendor id is a cheap second line of defence.
pub const VENDOR_ID_MICROSOFT: u32 = 0x1414;

/// One CPU's identifying info — brand + frequency. Mirrors what
/// `sysinfo::Cpu` exposes per-core; we collapse identical brand strings
/// and report the count + base frequency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuInfo {
    pub brand: String,
    pub vendor_id: String,
    pub physical_cores: u32,
    pub logical_cores: u32,
    /// Reported in MHz by `sysinfo`. 0 means the platform did not
    /// expose a frequency — Windows VMs sometimes elide this.
    pub frequency_mhz: u64,
    pub arch: String,
}

/// One mounted volume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskInfo {
    pub mount: String,
    pub file_system: String,
    /// "ssd", "hdd", or "unknown" — matches sysinfo's `DiskKind`.
    pub kind: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub is_removable: bool,
}

/// Per-GPU snapshot from NVML. NVIDIA only — Intel / AMD adapters
/// land in `intel_gpus` / `amd_gpus` because DXGI does not expose a
/// per-adapter "used VRAM" counter the way NVML does.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuInfo {
    pub vendor: String,
    pub name: String,
    pub vram_bytes: u64,
    pub vram_used_bytes: u64,
}

/// Non-NVIDIA adapter snapshot, sourced from the Windows DXGI factory.
/// `dedicated_vram_bytes` is non-zero for discrete cards (Intel Arc,
/// AMD Radeon dGPU); integrated graphics typically report 0 here and
/// a non-zero `shared_system_memory_bytes` instead (the iGPU borrows
/// DRAM). On Linux the same struct is populated from sysfs with the
/// memory fields zeroed (kernel doesn't expose them at that path).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DxgiGpuInfo {
    /// "intel" or "amd". Lowercase, no spaces — easy to match in the
    /// bootstrap LLM's routing table.
    pub vendor: String,
    /// PCI vendor id as an integer (`0x8086` for Intel, `0x1002` for
    /// AMD). Pinned in the tests so the doc keeps the literal values.
    pub vendor_id: u32,
    /// PCI device id. Useful for distinguishing specific SKUs (Arc A770
    /// vs UHD 770) when the name string is generic.
    pub device_id: u32,
    /// Human-readable adapter name from the DXGI desc — e.g.
    /// "Intel(R) Arc(TM) A770 Graphics", "AMD Radeon(TM) Graphics".
    pub name: String,
    /// Dedicated VRAM in bytes. Zero for iGPU / APU.
    pub dedicated_vram_bytes: u64,
    /// Shared system memory in bytes (DRAM the GPU can borrow). The
    /// iGPU's effective working set.
    pub shared_system_memory_bytes: u64,
}

/// Best-effort NPU descriptor. Phase 12.4 populates a new `npus`
/// array via the CPU-brand heuristic and mirrors the first entry here
/// for backward compatibility — Phase 12.2 consumers only ever read
/// `npu.present`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NpuInfo {
    pub present: bool,
    pub vendor: Option<String>,
    pub kind: Option<String>,
}

/// One detected NPU. Phase 12.4 only ever ships `source: "heuristic"`
/// entries; a real probe (`SetupDiGetClassDevs` /
/// `GUID_DEVCLASS_COMPUTEACCELERATOR`) is a future slice and will use
/// `source: "probe"` so the bootstrap LLM can tell the two apart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NpuEntry {
    /// "intel" (AI Boost / Meteor+) or "amd" (XDNA / Ryzen AI).
    pub vendor: String,
    /// Brand name for the NPU silicon — "ai_boost" or "xdna". Kept as
    /// a separate field from `vendor` because future Intel chips may
    /// rebrand without changing vendor.
    pub kind: String,
    /// "heuristic" (matched against CPU brand string) or "probe"
    /// (enumerated via PnP). Phase 12.4 only ships heuristic.
    pub source: String,
    /// One-sentence explanation of how the entry was derived. The
    /// bootstrap LLM is told to downgrade confidence when this field
    /// signals heuristic ambiguity (e.g. Phoenix Ryzen has an NPU but
    /// isn't branded "Ryzen AI").
    pub note: String,
}

/// OS identifying info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OsInfo {
    pub family: String,
    pub name: String,
    pub version: String,
    pub kernel_version: String,
    pub hostname: String,
}

/// Aggregate hardware inventory. The on-wire JSON shape mirrors this
/// struct field-for-field (snake_case via serde's default).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Inventory {
    pub cpu: CpuInfo,
    pub memory_total_bytes: u64,
    pub memory_available_bytes: u64,
    pub disks: Vec<DiskInfo>,
    /// NVIDIA adapters only (NVML-sourced). See [`intel_gpus`] /
    /// [`amd_gpus`] for the rest.
    pub gpus: Vec<GpuInfo>,
    /// Intel iGPUs + Arc dGPUs, sourced from DXGI. Empty on non-Windows.
    pub intel_gpus: Vec<DxgiGpuInfo>,
    /// AMD GPUs + APUs, sourced from DXGI. Empty on non-Windows.
    pub amd_gpus: Vec<DxgiGpuInfo>,
    /// Detected NPUs (heuristic only in Phase 12.4).
    pub npus: Vec<NpuEntry>,
    /// Legacy singular NPU field — kept for backward compatibility
    /// with Phase 12.2 readers. Mirrors `npus[0]` when populated;
    /// `present: false` otherwise.
    pub npu: NpuInfo,
    pub os: OsInfo,
}

/// Sample every hardware probe and return the snapshot. Refreshes
/// NVML and sysinfo so the result reflects current state, not stale
/// registry values.
pub fn sample() -> Inventory {
    refresh_nvml();
    refresh_sysinfo();
    let cpu = sample_cpu();
    let (intel_gpus, amd_gpus) = sample_dxgi_gpus();
    let npus = sample_npus(&cpu.brand);
    let npu = legacy_npu(&npus);
    Inventory {
        memory_total_bytes: registry().total_dram(),
        memory_available_bytes: registry()
            .total_dram()
            .saturating_sub(registry().actual_used_dram()),
        disks: sample_disks(),
        gpus: sample_gpus(),
        intel_gpus,
        amd_gpus,
        npus,
        npu,
        os: sample_os(),
        cpu,
    }
}

/// JSON action payload. Wraps [`sample`] so the action handler in
/// [`crate::service`] stays a one-liner.
pub fn inventory_payload() -> Value {
    let inv = sample();
    serde_json::to_value(inv).unwrap_or(Value::Null)
}

fn sample_cpu() -> CpuInfo {
    let mut sys = sysinfo::System::new();
    sys.refresh_cpu_all();
    let cpus = sys.cpus();
    let (brand, vendor_id, frequency_mhz) = cpus
        .first()
        .map(|c| {
            (
                c.brand().to_string(),
                c.vendor_id().to_string(),
                c.frequency(),
            )
        })
        .unwrap_or_default();
    let logical_cores = cpus.len() as u32;
    let physical_cores = sys
        .physical_core_count()
        .map(|n| n as u32)
        .unwrap_or(logical_cores);
    CpuInfo {
        brand,
        vendor_id,
        physical_cores,
        logical_cores,
        frequency_mhz,
        arch: std::env::consts::ARCH.to_string(),
    }
}

fn sample_disks() -> Vec<DiskInfo> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .iter()
        .map(|d| DiskInfo {
            mount: d.mount_point().to_string_lossy().into_owned(),
            file_system: d.file_system().to_string_lossy().into_owned(),
            kind: disk_kind_str(d.kind()),
            total_bytes: d.total_space(),
            available_bytes: d.available_space(),
            is_removable: d.is_removable(),
        })
        .collect()
}

fn disk_kind_str(k: sysinfo::DiskKind) -> String {
    match k {
        sysinfo::DiskKind::HDD => "hdd",
        sysinfo::DiskKind::SSD => "ssd",
        sysinfo::DiskKind::Unknown(_) => "unknown",
    }
    .to_string()
}

fn sample_gpus() -> Vec<GpuInfo> {
    let total = registry().total();
    let name = registry().gpu_name();
    // NVML only — Intel/AMD adapters surface through DXGI in
    // sample_dxgi_gpus().
    if total == 0 && name.is_empty() {
        return vec![];
    }
    vec![GpuInfo {
        vendor: "nvidia".to_string(),
        name,
        vram_bytes: total,
        vram_used_bytes: registry().actual_used(),
    }]
}

/// Returns `(intel_gpus, amd_gpus)`. Empty arrays on non-Windows
/// platforms unless the Linux sysfs fallback finds something.
fn sample_dxgi_gpus() -> (Vec<DxgiGpuInfo>, Vec<DxgiGpuInfo>) {
    let mut intel = Vec::new();
    let mut amd = Vec::new();
    for entry in enumerate_non_nvidia_adapters() {
        match entry.vendor_id {
            VENDOR_ID_INTEL => intel.push(entry),
            VENDOR_ID_AMD => amd.push(entry),
            _ => {}
        }
    }
    (intel, amd)
}

#[cfg(windows)]
fn enumerate_non_nvidia_adapters() -> Vec<DxgiGpuInfo> {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, DXGI_ADAPTER_DESC1,
        DXGI_ADAPTER_FLAG_SOFTWARE,
    };

    let mut out = Vec::new();
    // SAFETY: CreateDXGIFactory1 is a documented-safe call that
    // returns an HRESULT we convert into Result. No mutable state
    // crosses the FFI boundary.
    let factory: IDXGIFactory1 = match unsafe { CreateDXGIFactory1() } {
        Ok(f) => f,
        Err(_) => return out,
    };

    for i in 0u32..64 {
        // SAFETY: factory is a valid COM pointer; EnumAdapters1 returns
        // DXGI_ERROR_NOT_FOUND when we walk off the end, which we
        // translate to a loop break.
        let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(i) } {
            Ok(a) => a,
            Err(_) => break,
        };
        // SAFETY: adapter is a valid COM pointer; GetDesc1 fills a
        // stack-allocated DXGI_ADAPTER_DESC1 and returns the value.
        let desc: DXGI_ADAPTER_DESC1 = match unsafe { adapter.GetDesc1() } {
            Ok(d) => d,
            Err(_) => continue,
        };
        // Skip software adapters (WARP, Microsoft Basic Render Driver).
        let software_flag = DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32;
        if (desc.Flags & software_flag) != 0 {
            continue;
        }
        // NVIDIA goes through NVML — don't double-report it.
        if desc.VendorId == VENDOR_ID_NVIDIA || desc.VendorId == VENDOR_ID_MICROSOFT {
            continue;
        }
        let vendor = match desc.VendorId {
            VENDOR_ID_INTEL => "intel",
            VENDOR_ID_AMD => "amd",
            _ => continue,
        };
        let name_len = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len());
        let name = String::from_utf16_lossy(&desc.Description[..name_len])
            .trim()
            .to_string();
        out.push(DxgiGpuInfo {
            vendor: vendor.to_string(),
            vendor_id: desc.VendorId,
            device_id: desc.DeviceId,
            name,
            dedicated_vram_bytes: desc.DedicatedVideoMemory as u64,
            shared_system_memory_bytes: desc.SharedSystemMemory as u64,
        });
    }
    out
}

#[cfg(all(unix, target_os = "linux"))]
fn enumerate_non_nvidia_adapters() -> Vec<DxgiGpuInfo> {
    // sysfs fallback. The DRM subsystem exposes one entry per GPU at
    // /sys/class/drm/card<N>/device/{vendor,device}. We can't easily
    // read the human-readable name without lspci, and we deliberately
    // don't shell out (wylde_check rule 29). Memory accounting also
    // isn't exposed at this path, so the byte fields stay zero.
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/sys/class/drm") else {
        return out;
    };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        // Only `cardN` directories (skip `cardN-...` connector entries).
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        let device_path = ent.path().join("device");
        let vendor_id = read_sysfs_hex(&device_path.join("vendor"));
        let device_id = read_sysfs_hex(&device_path.join("device"));
        let Some(vendor_id) = vendor_id else { continue };
        let vendor = match vendor_id {
            VENDOR_ID_INTEL => "intel",
            VENDOR_ID_AMD => "amd",
            _ => continue,
        };
        out.push(DxgiGpuInfo {
            vendor: vendor.to_string(),
            vendor_id,
            device_id: device_id.unwrap_or(0),
            name: format!("{} (sysfs {})", vendor, name),
            dedicated_vram_bytes: 0,
            shared_system_memory_bytes: 0,
        });
    }
    out
}

#[cfg(all(unix, target_os = "linux"))]
fn read_sysfs_hex(path: &std::path::Path) -> Option<u32> {
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim().trim_start_matches("0x");
    u32::from_str_radix(trimmed, 16).ok()
}

#[cfg(not(any(windows, all(unix, target_os = "linux"))))]
fn enumerate_non_nvidia_adapters() -> Vec<DxgiGpuInfo> {
    Vec::new()
}

/// Lower-case the brand string and strip parenthesised tokens — the
/// Intel canonical form is "Intel(R) Core(TM) Ultra 7 155H", so a
/// naive `contains("core ultra")` misses the "(TM)" between "Core" and
/// "Ultra". Stripping `(...)` collapses that to "intel core ultra ...".
fn cpu_brand_normalised(s: &str) -> String {
    let mut out = String::new();
    let mut depth: u32 = 0;
    for c in s.to_ascii_lowercase().chars() {
        match c {
            '(' => depth = depth.saturating_add(1),
            ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Heuristic NPU detection from the CPU brand string. The branding is
/// stable enough that a substring match is a reasonable signal — see
/// the module-level doc for the full reasoning and the false-negative
/// caveat (Phoenix Ryzen 7040 has an NPU but no "AI" in the name).
fn sample_npus(cpu_brand: &str) -> Vec<NpuEntry> {
    let norm = cpu_brand_normalised(cpu_brand);
    let mut out = Vec::new();
    if norm.contains("core ultra") {
        out.push(NpuEntry {
            vendor: "intel".to_string(),
            kind: "ai_boost".to_string(),
            source: "heuristic".to_string(),
            note: "matched CPU brand string 'Core Ultra'; every Core Ultra \
                   SKU ships with the AI Boost NPU"
                .to_string(),
        });
    }
    if norm.contains("ryzen ai") {
        out.push(NpuEntry {
            vendor: "amd".to_string(),
            kind: "xdna".to_string(),
            source: "heuristic".to_string(),
            note: "matched CPU brand string 'Ryzen AI'; older Phoenix-class \
                   chips (Ryzen 7040-series) also have an XDNA NPU but are \
                   not flagged here — downgrade confidence accordingly"
                .to_string(),
        });
    }
    out
}

fn legacy_npu(npus: &[NpuEntry]) -> NpuInfo {
    match npus.first() {
        Some(n) => NpuInfo {
            present: true,
            vendor: Some(n.vendor.clone()),
            kind: Some(n.kind.clone()),
        },
        None => NpuInfo {
            present: false,
            vendor: None,
            kind: None,
        },
    }
}

fn sample_os() -> OsInfo {
    OsInfo {
        family: std::env::consts::FAMILY.to_string(),
        name: sysinfo::System::name().unwrap_or_else(|| std::env::consts::OS.to_string()),
        version: sysinfo::System::os_version().unwrap_or_default(),
        kernel_version: sysinfo::System::kernel_version().unwrap_or_default(),
        hostname: sysinfo::System::host_name().unwrap_or_default(),
    }
}

/// Action handler wrapper. The wire shape on success is the bare
/// inventory object (mirrors `vram.state` which returns the bare
/// snapshot, not a `{inventory: ...}` envelope).
pub fn handle_inventory() -> Value {
    inventory_payload()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_lock::guard;

    #[tokio::test(flavor = "current_thread")]
    async fn sample_returns_populated_cpu_and_os() {
        let _g = guard().await;
        let inv = sample();
        // Every host running tests has at least one logical CPU.
        assert!(
            inv.cpu.logical_cores >= 1,
            "expected at least 1 logical core; got {}",
            inv.cpu.logical_cores
        );
        // OS family is "unix" / "windows" / "wasm" — never empty on
        // platforms we build for.
        assert!(!inv.os.family.is_empty(), "os.family should be populated");
        // Arch is the build-target architecture — std::env::consts always
        // resolves to a non-empty string on supported targets.
        assert!(!inv.cpu.arch.is_empty(), "cpu.arch should be populated");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sample_serialises_to_flat_json_object() {
        let _g = guard().await;
        let v = inventory_payload();
        assert!(
            v.is_object(),
            "inventory payload must serialise to an object"
        );
        // Phase-12.2 fields stay present; Phase-12.4 adds the three
        // arrays + legacy `npu` continues to mirror `npus[0]`.
        for key in [
            "cpu",
            "memory_total_bytes",
            "memory_available_bytes",
            "disks",
            "gpus",
            "intel_gpus",
            "amd_gpus",
            "npus",
            "npu",
            "os",
        ] {
            assert!(
                v.get(key).is_some(),
                "inventory payload missing key {key}; got: {v}"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn new_phase_12_4_arrays_serialise_as_arrays() {
        let _g = guard().await;
        let v = inventory_payload();
        for key in ["intel_gpus", "amd_gpus", "npus", "gpus", "disks"] {
            assert!(
                v[key].is_array(),
                "inventory[{key}] must serialise to a JSON array; got: {}",
                v[key]
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn legacy_npu_mirrors_first_npus_entry() {
        // Pinned because the bootstrap doc tells the LLM it may read
        // `npu.present` as a shorthand for `!npus.is_empty()`. If the
        // mirroring ever drifts, both this test and the doc need updating.
        let _g = guard().await;
        let inv = sample();
        assert_eq!(
            inv.npu.present,
            !inv.npus.is_empty(),
            "npu.present must equal !npus.is_empty()"
        );
        if let Some(first) = inv.npus.first() {
            assert_eq!(inv.npu.vendor.as_deref(), Some(first.vendor.as_str()));
            assert_eq!(inv.npu.kind.as_deref(), Some(first.kind.as_str()));
        } else {
            assert!(inv.npu.vendor.is_none());
            assert!(inv.npu.kind.is_none());
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dxgi_entries_have_known_vendor_ids() {
        // If a non-Intel / non-AMD entry ever leaks through, the
        // bootstrap LLM would key off an unexpected `vendor` string.
        // sample_dxgi_gpus() partitions strictly by vendor id, so this
        // is a regression pin on the partition logic.
        let _g = guard().await;
        let inv = sample();
        for g in &inv.intel_gpus {
            assert_eq!(g.vendor_id, VENDOR_ID_INTEL, "intel_gpus entry: {:?}", g);
            assert_eq!(g.vendor, "intel");
        }
        for g in &inv.amd_gpus {
            assert_eq!(g.vendor_id, VENDOR_ID_AMD, "amd_gpus entry: {:?}", g);
            assert_eq!(g.vendor, "amd");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn npu_entries_carry_source_and_note() {
        // The bootstrap doc keys decisions off `source: "heuristic"` vs
        // `source: "probe"`. Pin the contract.
        let _g = guard().await;
        let inv = sample();
        for n in &inv.npus {
            assert!(
                matches!(n.source.as_str(), "heuristic" | "probe"),
                "npu source must be 'heuristic' or 'probe'; got: {}",
                n.source
            );
            assert!(!n.note.is_empty(), "npu note should explain the entry");
            assert!(
                matches!(n.vendor.as_str(), "intel" | "amd"),
                "unexpected npu vendor: {}",
                n.vendor
            );
        }
    }

    #[test]
    fn npu_heuristic_matches_core_ultra() {
        let npus = sample_npus("Intel(R) Core(TM) Ultra 7 155H");
        assert_eq!(npus.len(), 1);
        assert_eq!(npus[0].vendor, "intel");
        assert_eq!(npus[0].kind, "ai_boost");
        assert_eq!(npus[0].source, "heuristic");
    }

    #[test]
    fn npu_heuristic_matches_ryzen_ai() {
        let npus = sample_npus("AMD Ryzen AI 9 HX 370");
        assert_eq!(npus.len(), 1);
        assert_eq!(npus[0].vendor, "amd");
        assert_eq!(npus[0].kind, "xdna");
    }

    #[test]
    fn npu_heuristic_misses_phoenix_ryzen() {
        // Documented false negative: Phoenix 7040 has XDNA but isn't
        // branded "Ryzen AI". The heuristic returns empty; the
        // bootstrap LLM is told to downgrade confidence via the `note`
        // field on positive matches. Pinned so a future change to the
        // heuristic surface (e.g. adding "Ryzen 7 7840") is a
        // deliberate test edit, not a silent behaviour shift.
        let npus = sample_npus("AMD Ryzen 7 7840U");
        assert!(npus.is_empty(), "phoenix should not be heuristic-flagged");
    }

    #[test]
    fn npu_heuristic_misses_classic_core_i5() {
        let npus = sample_npus("Intel(R) Core(TM) i5-10500 CPU @ 3.10GHz");
        assert!(npus.is_empty());
    }

    #[test]
    fn npu_heuristic_is_case_insensitive() {
        let npus = sample_npus("intel(r) core(tm) ultra 5 125u");
        assert_eq!(npus.len(), 1);
        assert_eq!(npus[0].vendor, "intel");
    }

    #[test]
    fn legacy_npu_with_empty_list_is_absent() {
        let n = legacy_npu(&[]);
        assert!(!n.present);
        assert!(n.vendor.is_none());
        assert!(n.kind.is_none());
    }

    #[test]
    fn legacy_npu_with_entry_mirrors_first() {
        let entry = NpuEntry {
            vendor: "intel".to_string(),
            kind: "ai_boost".to_string(),
            source: "heuristic".to_string(),
            note: "x".to_string(),
        };
        let n = legacy_npu(&[entry]);
        assert!(n.present);
        assert_eq!(n.vendor.as_deref(), Some("intel"));
        assert_eq!(n.kind.as_deref(), Some("ai_boost"));
    }

    #[test]
    fn vendor_ids_are_stable_integers() {
        // The bootstrap doc cites these literals. Pin them so a typo
        // in the constants breaks tests rather than silently mis-
        // routing the LLM.
        assert_eq!(VENDOR_ID_INTEL, 0x8086);
        assert_eq!(VENDOR_ID_AMD, 0x1002);
        assert_eq!(VENDOR_ID_NVIDIA, 0x10DE);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn disk_kind_str_covers_all_variants() {
        assert_eq!(disk_kind_str(sysinfo::DiskKind::HDD), "hdd");
        assert_eq!(disk_kind_str(sysinfo::DiskKind::SSD), "ssd");
        assert_eq!(disk_kind_str(sysinfo::DiskKind::Unknown(0)), "unknown");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_inventory_returns_same_payload() {
        let _g = guard().await;
        let a = handle_inventory();
        let b = inventory_payload();
        // Both call `sample()` — only the host-variable fields (memory
        // available, disk available) may drift between calls. Compare
        // the stable cpu + os subset to confirm the wrapper is a
        // pass-through.
        assert_eq!(a["cpu"]["brand"], b["cpu"]["brand"]);
        assert_eq!(a["os"]["family"], b["os"]["family"]);
    }

    /// Behavioural test: when run on a box that actually has an Intel
    /// or AMD GPU, the DXGI walk should pick at least one up. Gated
    /// behind `#[ignore]` so CI on a machine without those vendors
    /// (or on non-Windows) doesn't fail. Run locally with
    /// `cargo test -- --ignored dxgi_finds_real_gpu --nocapture` to
    /// see what was actually enumerated.
    #[tokio::test(flavor = "current_thread")]
    #[ignore = "requires real Intel or AMD GPU"]
    async fn dxgi_finds_real_gpu() {
        let _g = guard().await;
        let inv = sample();
        eprintln!("intel_gpus = {:#?}", inv.intel_gpus);
        eprintln!("amd_gpus   = {:#?}", inv.amd_gpus);
        eprintln!("npus       = {:#?}", inv.npus);
        eprintln!("cpu.brand  = {}", inv.cpu.brand);
        assert!(
            !inv.intel_gpus.is_empty() || !inv.amd_gpus.is_empty(),
            "expected at least one Intel or AMD adapter via DXGI; got {:?} / {:?}",
            inv.intel_gpus,
            inv.amd_gpus
        );
    }

    /// Behavioural test: on a Core Ultra or Ryzen AI box the
    /// heuristic should flag an NPU. Ignored by default.
    #[tokio::test(flavor = "current_thread")]
    #[ignore = "requires real NPU-bearing CPU"]
    async fn npu_heuristic_finds_real_npu() {
        let _g = guard().await;
        let inv = sample();
        assert!(
            !inv.npus.is_empty(),
            "expected the NPU heuristic to flag a Core Ultra / Ryzen AI box; got cpu.brand={}",
            inv.cpu.brand
        );
    }
}

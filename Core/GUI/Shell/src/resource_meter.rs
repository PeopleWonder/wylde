//! Sidebar resource meter — a compact GPU-VRAM / system-RAM readout
//! pinned to the bottom of the left nav.
//!
//! The numbers come from the same source the Dashboard's hardware card
//! reads: the VRAM broker's `system.inventory` action.  We do *not* add
//! a new IPC verb — the Shell's [`crate::shell_root::Shell::spawn_resource_meter`]
//! poll reuses the existing `wylde-vram-broker` envelope and hands the
//! parsed [`ResourceSnapshot`] to [`render_resource_meter`] each frame.
//!
//! Visual goals: subtle.  MICRO text in `TEXT_MUTED`, two lines, a
//! `BORDER_SUBTLE` top divider so it reads as chrome rather than a nav
//! item.  When the broker hasn't answered yet (cold start) or a metric
//! is absent the line degrades to an em-dash rather than a zero — the
//! same "—" convention the Dashboard uses for absent values.

use gpui::{div, prelude::*, px, rgb, FontWeight, SharedString};
use serde_json::Value;
use wylde_theme::colors::{BORDER_SUBTLE, SURFACE_950, TEXT_MUTED};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::pack::pack;

/// Bare/`wylde-`-prefixed pipe name of the broker that serves
/// `system.inventory`.  Mirrors the Dashboard panel's `SVC_BROKER`.
pub const SVC_BROKER: &str = "wylde-vram-broker";

const GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// The slice of the broker's inventory envelope the sidebar meter needs.
///
/// A deliberately smaller projection than the Dashboard's `HardwareCard`
/// — the footer only renders VRAM + RAM, so CPU/disk/NPU fields are
/// dropped.  Same field semantics (raw bytes; humanise at render time).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResourceSnapshot {
    pub ram_total_bytes: u64,
    pub ram_available_bytes: u64,
    /// Largest single GPU's total VRAM in bytes.
    pub vram_total_bytes: u64,
    /// VRAM currently in use on that GPU (`total - free`).
    pub vram_used_bytes: u64,
}

impl ResourceSnapshot {
    /// Project the broker's `system.inventory` reply into the footer's
    /// shape.  Mirrors `dashboard::ipc::HardwareCard::from_value` for the
    /// two metrics we keep, so the sidebar and Dashboard never disagree
    /// on what "used / total" means.
    pub fn from_inventory_value(v: &Value) -> Self {
        let ram_total_bytes = v
            .get("memory_total_bytes")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let ram_available_bytes = v
            .get("memory_available_bytes")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        // Pick the largest card by total VRAM — same rule the Dashboard
        // uses so the two surfaces report the same GPU on a multi-card box.
        let (vram_total_bytes, vram_used_bytes) = v
            .get("gpus")
            .and_then(|x| x.as_array())
            .map(|gpus| {
                gpus.iter()
                    .map(|g| {
                        let total = g
                            .get("memory_total_bytes")
                            .and_then(|x| x.as_u64())
                            .unwrap_or(0);
                        let free = g
                            .get("memory_free_bytes")
                            .and_then(|x| x.as_u64())
                            .unwrap_or(total);
                        (total, total.saturating_sub(free))
                    })
                    .max_by_key(|(total, _)| *total)
                    .unwrap_or((0, 0))
            })
            .unwrap_or((0, 0));
        Self {
            ram_total_bytes,
            ram_available_bytes,
            vram_total_bytes,
            vram_used_bytes,
        }
    }

    /// "Used" RAM is total minus what the OS reports available.
    pub fn ram_used_bytes(&self) -> u64 {
        self.ram_total_bytes
            .saturating_sub(self.ram_available_bytes)
    }

    /// True when neither metric has landed yet (cold start / broker
    /// down) — the meter renders em-dashes in that case.
    pub fn is_empty(&self) -> bool {
        self.ram_total_bytes == 0 && self.vram_total_bytes == 0
    }

    /// `VRAM 4.0/24 GB`, or `VRAM —` when no GPU/data is present.
    fn vram_line(&self) -> String {
        if self.vram_total_bytes == 0 {
            "VRAM —".to_string()
        } else {
            format!(
                "VRAM {}",
                used_over_total(self.vram_used_bytes, self.vram_total_bytes)
            )
        }
    }

    /// `RAM 24/32 GB`, or `RAM —` when no data is present.
    fn ram_line(&self) -> String {
        if self.ram_total_bytes == 0 {
            "RAM —".to_string()
        } else {
            format!(
                "RAM {}",
                used_over_total(self.ram_used_bytes(), self.ram_total_bytes)
            )
        }
    }
}

/// Format a used/total byte pair as `12.3/16 GB` — used keeps one
/// decimal (it moves), total rounds to a whole number (it's fixed and
/// reads cleaner without `.0`).
fn used_over_total(used: u64, total: u64) -> String {
    let u = used as f64 / GB;
    let t = total as f64 / GB;
    format!("{u:.1}/{t:.0} GB")
}

/// The two footer lines.  `None` (broker never answered) and an empty
/// snapshot both degrade to em-dashes so the layout height is stable.
fn lines(snapshot: Option<&ResourceSnapshot>) -> (String, String) {
    match snapshot {
        Some(s) => (s.vram_line(), s.ram_line()),
        None => ("VRAM —".to_string(), "RAM —".to_string()),
    }
}

/// Build the resource-meter footer `Div`.  Pinned to the bottom of the
/// sidebar by the nav column above it taking the flexible space.
pub fn render_resource_meter(snapshot: Option<&ResourceSnapshot>) -> gpui::Div {
    let (vram, ram) = lines(snapshot);

    let line = |text: String| {
        div()
            .font_family(FAMILY_INTER)
            .text_size(px(size::MICRO))
            .text_color(rgb(pack(TEXT_MUTED)))
            .font_weight(FontWeight(weight::REGULAR as f32))
            .child(SharedString::from(text))
    };

    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .px_4()
        .py_2()
        .bg(rgb(pack(SURFACE_950)))
        .border_t_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .child(line(vram))
        .child(line(ram))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_vram_and_ram_from_inventory() {
        let v = json!({
            "memory_total_bytes": 32_u64 * 1024 * 1024 * 1024,
            "memory_available_bytes": 8_u64 * 1024 * 1024 * 1024,
            "gpus": [{
                "memory_total_bytes": 16_u64 * 1024 * 1024 * 1024,
                "memory_free_bytes": 4_u64 * 1024 * 1024 * 1024,
            }],
        });
        let s = ResourceSnapshot::from_inventory_value(&v);
        assert_eq!(s.ram_total_bytes, 32_u64 * 1024 * 1024 * 1024);
        assert_eq!(s.ram_used_bytes(), 24_u64 * 1024 * 1024 * 1024);
        assert_eq!(s.vram_total_bytes, 16_u64 * 1024 * 1024 * 1024);
        assert_eq!(s.vram_used_bytes, 12_u64 * 1024 * 1024 * 1024);
        assert!(!s.is_empty());
    }

    #[test]
    fn picks_largest_gpu_on_multi_card_box() {
        let v = json!({
            "gpus": [
                { "memory_total_bytes": 8_u64 * 1024 * 1024 * 1024,
                  "memory_free_bytes": 8_u64 * 1024 * 1024 * 1024 },
                { "memory_total_bytes": 24_u64 * 1024 * 1024 * 1024,
                  "memory_free_bytes": 20_u64 * 1024 * 1024 * 1024 },
            ],
        });
        let s = ResourceSnapshot::from_inventory_value(&v);
        assert_eq!(s.vram_total_bytes, 24_u64 * 1024 * 1024 * 1024);
        assert_eq!(s.vram_used_bytes, 4_u64 * 1024 * 1024 * 1024);
    }

    #[test]
    fn empty_envelope_is_unknown_and_renders_dashes() {
        let s = ResourceSnapshot::from_inventory_value(&json!({}));
        assert!(s.is_empty());
        assert_eq!(s.vram_line(), "VRAM —");
        assert_eq!(s.ram_line(), "RAM —");
    }

    #[test]
    fn none_snapshot_degrades_to_dashes() {
        let (vram, ram) = lines(None);
        assert_eq!(vram, "VRAM —");
        assert_eq!(ram, "RAM —");
    }

    #[test]
    fn formats_used_over_total_compactly() {
        assert_eq!(
            used_over_total(12_u64 * 1024 * 1024 * 1024, 16_u64 * 1024 * 1024 * 1024),
            "12.0/16 GB",
        );
    }

    #[test]
    fn ram_present_but_no_gpu_shows_vram_dash() {
        let v = json!({
            "memory_total_bytes": 32_u64 * 1024 * 1024 * 1024,
            "memory_available_bytes": 16_u64 * 1024 * 1024 * 1024,
            "gpus": [],
        });
        let s = ResourceSnapshot::from_inventory_value(&v);
        assert_eq!(s.vram_line(), "VRAM —");
        assert_eq!(s.ram_line(), "RAM 16.0/32 GB");
    }
}

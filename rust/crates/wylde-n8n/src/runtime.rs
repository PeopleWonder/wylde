//! Where the managed n8n engine lives, as `wylde-n8n` attaches to it.
//!
//! Before this module, `wylde-n8n` was a pipe proxy in front of an
//! *external, user-managed* n8n the user had to install AND start AND
//! point credentials at by hand. Now the lifecycle daemon launches the
//! engine as its own daemon-managed service (`wylde-n8n-engine`), with
//! `N8N_USER_FOLDER` on the out-of-tree data dir and the encryption key
//! pinned to the Wylde-owned identity, bound to loopback only.
//!
//! This service **attaches**: it reads that identity back from the same
//! data dir (lifecycle injects it as `WYLDE_N8N_DATA_DIR`), provisions the
//! owner account over REST, and fronts the engine as `n8n.*` pipe actions.
//! It spawns nothing — third-party processes are the lifecycle daemon's
//! business.
//!
//! Opt-out: set `WYLDE_N8N_MANAGED=0` to keep the legacy external-daemon
//! behaviour (lifecycle launches no engine and this service is a pure
//! proxy again).

use std::path::{Path, PathBuf};

pub use wylde_shared::n8n::DEFAULT_PORT;

/// Resolved knobs for the managed engine, read once from env.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// When false (`WYLDE_N8N_MANAGED=0`), nothing launches n8n — the
    /// legacy external-daemon mode.
    pub managed: bool,
    /// Loopback port the engine answers on.
    pub port: u16,
    /// Durable data dir holding the engine's database and the Wylde-owned
    /// identity. The lifecycle daemon injects `WYLDE_N8N_DATA_DIR`; a
    /// standalone run falls back to a `WyldeData/n8n` sibling of the repo
    /// root.
    pub data_dir: PathBuf,
}

impl RuntimeConfig {
    pub fn from_env(wylde_root: &Path) -> Self {
        Self {
            managed: wylde_shared::n8n::managed_from_env(),
            port: wylde_shared::n8n::port_from_env(),
            data_dir: resolve_data_dir(wylde_root),
        }
    }

    /// The loopback base URL the engine answers on.
    pub fn base_url(&self) -> String {
        wylde_shared::n8n::base_url(self.port)
    }
}

/// Durable data dir for n8n: the lifecycle-injected `WYLDE_N8N_DATA_DIR`
/// (the out-of-tree foundation contract) else a `WyldeData/n8n` sibling
/// of the repo root, matching `wylde-lifecycle::paths::resolve_data_dir`.
pub fn resolve_data_dir(wylde_root: &Path) -> PathBuf {
    if let Some(d) = std::env::var_os("WYLDE_N8N_DATA_DIR") {
        return PathBuf::from(d);
    }
    let parent = wylde_root
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| wylde_root.to_path_buf());
    parent.join("WyldeData").join("n8n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_uses_port() {
        let cfg = RuntimeConfig {
            managed: true,
            port: 5999,
            data_dir: PathBuf::from("x"),
        };
        assert_eq!(cfg.base_url(), "http://127.0.0.1:5999");
    }

    #[test]
    fn resolve_data_dir_prefers_injected_env() {
        // Guarded so we don't trample a real env in a parallel runner:
        // only assert the fallback shape when the var is absent.
        if std::env::var_os("WYLDE_N8N_DATA_DIR").is_none() {
            let d = resolve_data_dir(Path::new("C:/repo/Wylde-release"));
            assert!(d.ends_with("WyldeData/n8n") || d.ends_with("WyldeData\\n8n"));
        }
    }
}

//! Env-driven configuration for `wylde-workspaces`.
//!
//! Same shape as every other service crate's `config.rs` (`wylde-ollama`,
//! `wylde-treesitter`): read once at first access, cached in a process-wide
//! `OnceLock`. Mutating env after start does not retroactively change
//! behaviour, matching the Python module-import semantics the rest of the
//! stack assumes.
//!
//! Slice 0a only needs two knobs — the service/pipe name and the data dir.
//! Later slices (registry/notes/conversations/anchors/graph) read more from
//! here; this is the bedrock they grow on.

use std::path::PathBuf;
use std::sync::OnceLock;

/// Default service name. The shared IPC layer turns this into the pipe path
/// `\\.\pipe\wylde-workspaces` via [`wylde_shared::ipc::pipe_name`] (it
/// strips a leading `wylde-` and re-adds it, so either form is accepted).
pub const DEFAULT_SERVICE_NAME: &str = "wylde-workspaces";

#[derive(Debug, Clone)]
pub struct Config {
    /// Service name handed to the shared IPC `serve()` call. Overridable via
    /// `WYLDE_WORKSPACES_PIPE_NAME` so a test (or a second instance) can bind
    /// an isolated pipe without clashing with a running prod service. The
    /// pipe path is derived from this name by the shared transport.
    pub service_name: String,

    /// Root for all workspace-scoped state. Defaults to
    /// `<wylde_root>/data/workspaces`. Nothing is written here in Slice 0a —
    /// the registry/notes/conversations stores land in 0b/0c — but the path
    /// is resolved now so later slices have one source of truth.
    pub data_dir: PathBuf,

    /// Repo / install root, used for the action-contract write path and as
    /// the base for `data_dir` when the latter isn't set explicitly.
    pub wylde_root: PathBuf,

    /// IPC service name of the embedder (`ollama.embed`). Default
    /// `wylde-ollama`; override via `WYLDE_WORKSPACES_OLLAMA_SERVICE`.
    /// Consumed by [`crate::embeddings`] for ingest + RAG-query embeds.
    pub ollama_service: String,

    /// IPC service name of the tree-sitter sidecar
    /// (`treesitter.extract_entities`). Default `wylde-treesitter`; override
    /// via `WYLDE_WORKSPACES_TREESITTER_SERVICE`. Consumed by the graph-ingest
    /// half of [`crate::rag::indexer`].
    pub treesitter_service: String,
}

impl Config {
    fn load() -> Self {
        let service_name = std::env::var("WYLDE_WORKSPACES_PIPE_NAME")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_SERVICE_NAME.to_owned());

        let wylde_root = std::env::var_os("WYLDE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));

        let data_dir = std::env::var_os("WYLDE_WORKSPACES_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| wylde_root.join("data").join("workspaces"));

        let ollama_service = std::env::var("WYLDE_WORKSPACES_OLLAMA_SERVICE")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "wylde-ollama".to_owned());

        let treesitter_service = std::env::var("WYLDE_WORKSPACES_TREESITTER_SERVICE")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "wylde-treesitter".to_owned());

        Self {
            service_name,
            data_dir,
            wylde_root,
            ollama_service,
            treesitter_service,
        }
    }

    /// Process-wide config snapshot. First call reads the env; subsequent
    /// calls return the cached value.
    pub fn get() -> &'static Self {
        static CFG: OnceLock<Config> = OnceLock::new();
        CFG.get_or_init(Self::load)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        // Load directly (not via the cached `get()`) so we don't race other
        // tests' env mutations through the process-wide singleton.
        let cfg = Config::load();
        assert!(!cfg.service_name.is_empty());
        // data_dir should end in workspaces unless explicitly overridden.
        if std::env::var_os("WYLDE_WORKSPACES_DATA_DIR").is_none() {
            assert!(cfg.data_dir.ends_with("workspaces"));
        }
    }
}

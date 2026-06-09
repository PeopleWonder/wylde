//! Graph **model** — WHAT the graph is (pure data, no rendering).
//!
//! These types mirror the `workspaces.graph` verb's wire shape (Slice B) so a
//! reply deserialises straight into a [`WorkspaceGraph`]. They are the
//! canonical GUI-side homes per Build Order Appendix B; the renderer
//! (`super::render`) and IPC layer (`super::ipc`) build on them. No `gpui`,
//! no IPC, no theme — just data + deterministic scaffold layout.

pub mod cluster;
pub mod edge;
pub mod node;
pub mod view_mode;
pub mod workspace_graph;

pub use cluster::Cluster;
pub use edge::{Edge, RelType};
pub use node::{language_for_path, Node, NodeKind, NodeStyle, Position};
pub use view_mode::ViewMode;
pub use workspace_graph::{Layout, WorkspaceGraph};

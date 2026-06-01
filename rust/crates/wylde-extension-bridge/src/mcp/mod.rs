//! Minimal MCP client over JSON-RPC 2.0 / stdio.
//!
//! Why not `rmcp` directly: the SDK is Tier 2 and its API moves with
//! the spec. The protocol it implements is plain JSON-RPC over
//! line-delimited (or framed) stdio. The host needs only four
//! request methods (`initialize`, `tools/list`, `tools/call`, `ping`)
//! plus one notification (`notifications/initialized`). A direct
//! implementation gives us full control of capability negotiation,
//! the version policy (N / N-1 / N+1, [`crate::version`]), and
//! stream framing without a hard dep on rmcp's evolving surface.

pub mod client;
pub mod stdio;
pub mod wire;

pub use client::{McpClient, McpError, SpawnSpec, ToolDescription};
pub use wire::{InitializeResult, ServerInfo};

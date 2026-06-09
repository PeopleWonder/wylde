//! Graph **IPC** — the only part of the graph module that talks to
//! `wylde-workspaces`. The renderer/model are pure; tests drop fake graphs in
//! without a pipe (Build Order §8 "IPC is at the edge").
//!
//! [`client::fetch_active_graph`] is the entry point: it resolves the active
//! workspace and fetches its graph via the Slice B verb, classifying failures
//! for the graceful-degrade fallback (OI-1).

pub mod client;
pub mod graph_query;

pub use client::{fetch_active_graph, GraphFetchError, GraphLoad};

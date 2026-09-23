//! Workflows panel — the embedded n8n editor.
//!
//! The lifecycle daemon runs the local n8n engine (`wylde-n8n-engine`) and
//! `wylde-n8n` fronts it over the pipe. This panel is the user's window onto
//! it: a gpui header that reports whether the engine is answering, with Reload
//! and Open-in-browser controls, above a content region the Shell hosts the n8n
//! editor in.
//!
//! The editor is a web page, but this crate links no WebView: panel crates stay
//! `wry`-free so the headless L7 walk can build them. The panel asks the Shell
//! to host the page over its content region through
//! [`wylde_gui_pipe::embed_bus`], and the Shell reuses the WebView host and
//! shared-auth bootstrap it already runs for `iframe` panels — so the editor
//! still opens signed in as the Wylde-owned n8n owner.

pub mod n8n_panel;

pub use n8n_panel::{EngineState, N8nPanel};

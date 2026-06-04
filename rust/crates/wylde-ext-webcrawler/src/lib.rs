//! Webcrawler extension — standalone Rust MCP server.
//!
//! A drop-in Rust replacement for the Python `Extensions/Webcrawler/`
//! extension (today served through `Extensions/_shim/server.py`). It speaks
//! the minimum MCP-over-stdio subset the `wylde-extension-bridge` host drives
//! (`initialize`, `notifications/initialized`, `tools/list`, `tools/call`,
//! `ping`) and exposes the same three tools the shim does:
//!
//!   * `fetch`   — GET a URL, return the body as text or parsed JSON.
//!   * `scrape`  — GET HTML, optionally apply a list of CSS selectors.
//!   * `extract` — apply a `{field:{selector,attribute,multiple}}` rule set to HTML (fetched from `url` or passed inline as `html`).
//!
//! Greenfield Rust (NOT a line-by-line transpile) following the
//! `wylde-ollama` / `wylde-treesitter` sidecar precedent: `reqwest` for HTTP,
//! `scraper` for HTML/CSS, no headless browser (single-page static fetch only,
//! exactly as the Python does). External HTTP egress routes through the Rust
//! Gateway's `egress.forward` action over the pipe — with a *loud* direct
//! `reqwest` fallback when the Gateway isn't reachable (dev), mirroring the
//! Python handler's Gateway-first / requests-fallback shape. The
//! `_validate_external_url` SSRF guard is ported branch-for-branch.
//!
//! See `docs/plans/legacy-extensions-rust-rewrite.md` (Slice 1, "W1 + W2").
//!
//! Public entry point:
//!   * [`mcp::serve`] — run the MCP stdio server loop until stdin closes.

pub mod config;
pub mod egress;
pub mod extract;
pub mod mcp;
pub mod scrape;
pub mod ssrf;
pub mod tools;

pub use mcp::serve;

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
//! `wylde-ollama` / `wylde-treesitter` sidecar precedent: `scraper` for
//! HTML/CSS, no headless browser (single-page static fetch only, exactly as
//! the Python does). External HTTP egress routes **only** through the Rust
//! Gateway's `egress.forward` action over the pipe — there is no direct-HTTP
//! bypass (the old `reqwest` fallback was removed under the security boundary,
//! audit item B2), so the Gateway's allowlist is the sole egress chokepoint.
//! The `_validate_external_url` SSRF guard is ported branch-for-branch as a
//! defence-in-depth pre-check before the request reaches the Gateway.
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

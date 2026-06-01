//! Wylde Images panel — gpui-era surface over the gateway's image
//! library + ComfyUI proxy.
//!
//! The Tauri/Svelte page (`Core/GUI/src/pages/Images.svelte`) was
//! emptied earlier when image generation was moved out of Wylde proper
//! and into ComfyUI as a separate service.  The gpui edition restores
//! the in-app surface — with a single owner (`wylde-gateway`) and an
//! HTTP-shaped pipe rather than the direct executeTool() calls the old
//! Svelte page used.
//!
//! Surfaces:
//!
//!   * **Gallery grid** — thumbnails of every PNG under the gateway's
//!     `data/images/` directory, sorted newest-first.  Thumbnails are
//!     decoded once via gpui's built-in image asset loader (`Image::
//!     from_bytes` → `ImageSource::Image`); the cache is process-wide
//!     by file id so re-rendering a filtered subset is free.  PNG /
//!     JPEG / WebP are handled natively at the pinned `b3d93d44` rev —
//!     no extra `image` crate dep needed.
//!   * **Filters** — three chip rows (date range, workspace, model).
//!     Each chip flips a filter atom; the gallery re-projects locally
//!     instead of going back to the server, since the on-disk listing
//!     is small and the metadata is already in the row.
//!   * **Metadata pane** — slides in next to the gallery when a row is
//!     selected (sidebar pattern, not modal).  Surfaces prompt, seed,
//!     model, dimensions, generation timestamp, file size, source
//!     (generated / imported / received-from-tool), workspace, and any
//!     free-form tags the sidecar JSON carries.  The workspace chip is
//!     a `request_nav("core/workspaces")` jump.
//!   * **Delete** — per-image, inline Yes/Cancel confirmation strip
//!     attached to the metadata pane (no modal, matching the slice 7
//!     Devices panel pattern).  Calls `DELETE /api/images/library/:id`.
//!   * **Generate new** — a single-row entry bar at the top of the
//!     panel: TextInput (multi-line, 1-3 visible rows, Ctrl/Cmd+Enter
//!     submits) + a model picker pill + submit.  The submit kicks off
//!     a one-shot POST to `/api/images/generate`; while it runs the
//!     row shows a Stop button that aborts the underlying task (drop
//!     the future via a stored `JoinHandle`).
//!
//! Externalities (carry-over from the Svelte page's removal):
//!
//!   * The gateway proxies `/api/images/generate` to ComfyUI directly;
//!     there is no streaming progress verb in the slice spec's
//!     vocabulary.  The submit returns once ComfyUI replies (up to
//!     600 s).  When ComfyUI ships a streaming surface, swap the
//!     one-shot POST for `stream_call` here.
//!   * The metadata pane projects whatever the sidecar JSON files
//!     carry — schema varies by image-gen service version.  Known
//!     keys (`prompt`, `seed`, `model`, `width`, `height`,
//!     `workspace_id`, `source`) are pulled out; everything else
//!     surfaces as a JSON blob at the bottom of the pane.

pub mod images_panel;
pub mod ipc;

pub use images_panel::ImagesPanel;

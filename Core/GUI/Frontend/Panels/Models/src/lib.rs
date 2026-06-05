//! Models panel — gpui-era surface over the local Ollama model store.
//!
//! Scope:
//!   * **Installed models list** — `ollama.list_models` rows with
//!     family / parameter_size / quantization meta and an "in use"
//!     pill sourced from `ollama.list_loaded`.  A star per row sets
//!     the session-default model.
//!   * **Pull a model** — single-line TextInput backed by a
//!     search-as-you-type autocomplete over a curated catalog
//!     (`catalog`).  Typing fuzzy-matches catalog entries into a
//!     dropdown (family icon, name, size badge, license); selecting a
//!     row fills the exact tag.  An uncatalogued tag still pulls via the
//!     "Pull anyway" fallback.  Submit kicks off `ollama.pull`
//!     streaming; the chunk loop renders a live progress bar.  Cancel
//!     drops the stream (cancel-by-disconnect).
//!   * **Inline confirm-delete** — first click stages a "Confirm
//!     delete?" strip on the row; Yes commits via `ollama.delete`,
//!     Cancel reverts.  No modal — matches the rest of the gpui shell.
//!
//! Why no persistent `models.set_default` write?  The harness reads
//! its `default_model` from `WYLDE_DEFAULT_MODEL` at startup; there's
//! no mutator pipe verb yet.  The panel keeps a per-session preference
//! on its View state (`session_default`).  When the harness gains
//! `models.set_default` the call lands in `set_default(...)`; nothing
//! else moves.

pub mod catalog;
pub mod ipc;
pub mod models_panel;
pub mod recommend;

pub use models_panel::ModelsPanel;

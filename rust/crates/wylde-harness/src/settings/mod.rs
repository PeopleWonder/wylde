//! Settings stores owned by the harness.
//!
//! Today this hosts the **per-model Ollama inference override store**
//! ([`ollama_overrides`]) that backs the Settings → "Ollama inference"
//! panel. It supersedes the old flat `data/settings/ollama.json` the
//! Gateway wrote (a single global block applied to every model): the new
//! store keys overrides *per model* and leaves room in the on-disk layout
//! for future "model profiles" without a second migration.
//!
//! The Settings panel reaches these over the `settings.ollama.*` pipe
//! verbs (see [`crate::api::HarnessApi`]). Reading them via the harness
//! pipe — rather than the Gateway's HTTP-only `/api/settings/ollama`
//! route — is deliberate: the gpui GUI talks to services over named
//! pipes, and the Gateway's settings route is registered only on its
//! axum/TCP surface, so a pipe read of it always failed (the "all dashes"
//! bug). New work lands as harness verbs per the everything-Rust rule.

pub mod actions;
pub mod ollama_overrides;

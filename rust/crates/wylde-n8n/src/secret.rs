//! Wylde-owned n8n identity, as `wylde-n8n` sees it.
//!
//! The identity (encryption key + owner account) is minted and owned by
//! the lifecycle daemon, which launches the n8n engine with that key — see
//! [`wylde_shared::n8n`]. This service only reads it back: it provisions
//! and logs in as the owner, and hands the editor its auto-login script.
//! It never mints a key, because a key this side invented would not be the
//! one the engine encrypts credentials with.

pub use wylde_shared::n8n::N8nIdentity;

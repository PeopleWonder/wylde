//! Action handlers for the 9 first-class `ext.*` actions plus the
//! `extensions.dispatch` back-compat alias.

pub mod legacy_dispatch;
pub mod surface;

pub use surface::*;

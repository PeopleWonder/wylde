//! Workspace file-I/O — the jailed `workspaces.fs.*` verb surface (S1 /
//! plan P0.2). The foundation the IDE editor (read/save) and file-tree
//! (list) tabs are built on.
//!
//! `wylde-workspaces` is the correct owner of file access: it already holds
//! each workspace's `folder`, runs the file watcher, and reads/writes files
//! internally. These verbs expose that surface to the GUI **through a hard
//! per-workspace root jail** ([`jail`]) so the GUI process never reads an
//! arbitrary disk path itself. See [`api`] for the handlers and OQ-2/OQ-6/OQ-7
//! decisions they implement.

pub mod api;
pub mod jail;

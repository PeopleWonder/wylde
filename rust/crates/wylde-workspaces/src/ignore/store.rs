//! Ignore-list persistence (Slice M).
//!
//! One `ignore.json` per workspace at
//! `<data_dir>/workspaces/<workspace_id>/ignore.json`, beside `anchors.json`
//! — same discipline: encrypt-at-rest via the shared engine (OI-14),
//! atomic replace, fail-soft reads (missing/torn file → empty). The file
//! holds both service-side tiers:
//!
//! ```json
//! { "workspace": [entries], "conversations": { "<conv-id>": [entries] } }
//! ```
//!
//! Tokens are stored trimmed and matched case-sensitively — they're the
//! composer's recognized tokens (symbol identifiers / anchor tokens), which
//! are case-meaningful.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::registry::persistence::workspace_dir;

/// `<data_dir>/workspaces/<workspace_id>/ignore.json`.
pub fn ignore_path(workspace_id: &str) -> PathBuf {
    workspace_dir(workspace_id).join("ignore.json")
}

/// Which service-side tier an operation targets. (The global tier lives in
/// the harness.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IgnoreTier {
    Workspace,
    Conversation,
}

impl IgnoreTier {
    /// Parse the wire string (`"workspace"` / `"conversation"`).
    pub fn parse(s: &str) -> Option<IgnoreTier> {
        match s {
            "workspace" => Some(IgnoreTier::Workspace),
            "conversation" => Some(IgnoreTier::Conversation),
            _ => None,
        }
    }
}

/// One ignored token.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IgnoreEntry {
    pub token: String,
    /// Unix seconds when the ignore was added (display/cleanup ordering).
    #[serde(default)]
    pub added_at: u64,
}

/// The whole `ignore.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IgnoreFile {
    pub workspace: Vec<IgnoreEntry>,
    pub conversations: HashMap<String, Vec<IgnoreEntry>>,
}

impl IgnoreFile {
    fn tier_mut(&mut self, tier: IgnoreTier, conversation_id: &str) -> &mut Vec<IgnoreEntry> {
        match tier {
            IgnoreTier::Workspace => &mut self.workspace,
            IgnoreTier::Conversation => self
                .conversations
                .entry(conversation_id.to_owned())
                .or_default(),
        }
    }

    /// The conversation tier for `conversation_id` (empty slice when none).
    pub fn conversation(&self, conversation_id: &str) -> &[IgnoreEntry] {
        self.conversations
            .get(conversation_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

/// Load a workspace's ignore file. Fail-soft: empty on missing/torn.
/// Decrypts at rest (OI-14).
pub fn load(workspace_id: &str) -> IgnoreFile {
    let Ok(raw) = wylde_shared::encryption::read_to_string_at_rest(&ignore_path(workspace_id))
    else {
        return IgnoreFile::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Encrypt-at-rest + atomically replace a workspace's `ignore.json`.
pub fn save(workspace_id: &str, file: &IgnoreFile) -> std::io::Result<()> {
    let body = serde_json::to_string_pretty(file).unwrap();
    wylde_shared::encryption::write_at_rest(&ignore_path(workspace_id), body.as_bytes())
}

/// Add `token` to a tier. Idempotent: an already-ignored token is a
/// no-write success (`Ok(false)`).
pub fn add(
    workspace_id: &str,
    tier: IgnoreTier,
    conversation_id: &str,
    token: &str,
) -> std::io::Result<bool> {
    let token = token.trim();
    let mut file = load(workspace_id);
    let entries = file.tier_mut(tier, conversation_id);
    if entries.iter().any(|e| e.token == token) {
        return Ok(false);
    }
    entries.push(IgnoreEntry {
        token: token.to_owned(),
        added_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    });
    save(workspace_id, &file)?;
    Ok(true)
}

/// Remove `token` from a tier. `Ok(false)` when it wasn't there.
pub fn remove(
    workspace_id: &str,
    tier: IgnoreTier,
    conversation_id: &str,
    token: &str,
) -> std::io::Result<bool> {
    let token = token.trim();
    let mut file = load(workspace_id);
    let entries = file.tier_mut(tier, conversation_id);
    let before = entries.len();
    entries.retain(|e| e.token != token);
    let removed = entries.len() != before;
    if removed {
        // Drop an emptied conversation key so the file doesn't accrete.
        file.conversations.retain(|_, v| !v.is_empty());
        save(workspace_id, &file)?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;

    #[test]
    fn add_list_remove_round_trip_both_tiers() {
        let _env = TestEnv::new();
        let ws = "ws-ignore-rt";

        assert!(add(ws, IgnoreTier::Workspace, "", "telemetry_flush").unwrap());
        assert!(add(ws, IgnoreTier::Conversation, "conv-1", "debug_dump").unwrap());

        let file = load(ws);
        assert_eq!(file.workspace.len(), 1);
        assert_eq!(file.workspace[0].token, "telemetry_flush");
        assert!(file.workspace[0].added_at > 0);
        assert_eq!(file.conversation("conv-1").len(), 1);
        assert!(file.conversation("conv-other").is_empty());

        assert!(remove(ws, IgnoreTier::Workspace, "", "telemetry_flush").unwrap());
        assert!(remove(ws, IgnoreTier::Conversation, "conv-1", "debug_dump").unwrap());
        let file = load(ws);
        assert!(file.workspace.is_empty());
        assert!(
            file.conversations.is_empty(),
            "emptied conversation keys pruned"
        );
    }

    #[test]
    fn add_is_idempotent_and_remove_reports_absence() {
        let _env = TestEnv::new();
        let ws = "ws-ignore-idem";
        assert!(add(ws, IgnoreTier::Workspace, "", "set_active").unwrap());
        assert!(
            !add(ws, IgnoreTier::Workspace, "", "  set_active  ").unwrap(),
            "trimmed duplicate is a no-write success"
        );
        assert_eq!(load(ws).workspace.len(), 1);
        assert!(!remove(ws, IgnoreTier::Workspace, "", "never_there").unwrap());
    }

    #[test]
    fn conversation_tiers_are_isolated() {
        let _env = TestEnv::new();
        let ws = "ws-ignore-iso";
        add(ws, IgnoreTier::Conversation, "conv-a", "tok").unwrap();
        add(ws, IgnoreTier::Conversation, "conv-b", "tok").unwrap();
        remove(ws, IgnoreTier::Conversation, "conv-a", "tok").unwrap();
        let file = load(ws);
        assert!(file.conversation("conv-a").is_empty());
        assert_eq!(file.conversation("conv-b").len(), 1);
    }

    #[test]
    fn missing_file_loads_empty_and_tier_parse_validates() {
        let _env = TestEnv::new();
        assert_eq!(load("ws-never-seen"), IgnoreFile::default());
        assert_eq!(IgnoreTier::parse("workspace"), Some(IgnoreTier::Workspace));
        assert_eq!(
            IgnoreTier::parse("conversation"),
            Some(IgnoreTier::Conversation)
        );
        assert_eq!(IgnoreTier::parse("global"), None, "global is harness-side");
    }
}

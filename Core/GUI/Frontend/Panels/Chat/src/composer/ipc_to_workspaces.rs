//! Composer → wylde-workspaces pipe calls (Slice F, Build Order §5):
//! `workspaces.symbols.find` (F-data) and `workspaces.anchors.find_by_token`
//! (N-data), with OI-1 graceful degrade — a down service means recognition
//! quietly returns nothing (the composer shows a "recognition offline" hint,
//! never an error wall; typing and sending are unaffected).

use serde_json::{json, Value};

use super::tokenizer::{TokenKind, TokenSpan};
use super::{SymbolCandidate, WordRecognition};

const SVC_WORKSPACES: &str = "wylde-workspaces";

/// How many symbol candidates to ask for per word. >1 so single-match vs
/// ambiguous is distinguishable (the Slice G trick), capped small enough for
/// a readable dropdown.
const FIND_LIMIT: usize = 6;

async fn workspaces_call(action: &str, payload: Value) -> Result<Value, String> {
    wylde_gui_pipe::call(
        SVC_WORKSPACES,
        "POST",
        "/__action__",
        Some(json!({ "action": action, "payload": payload })),
    )
    .await
}

/// `workspaces.symbols.find` → ranked candidates for `query`.
pub async fn find_symbols(
    workspace_id: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<SymbolCandidate>, String> {
    let v = workspaces_call(
        "workspaces.symbols.find",
        json!({ "workspace_id": workspace_id, "query": query, "limit": limit }),
    )
    .await?;
    let matches = v.get("matches").and_then(Value::as_array);
    Ok(matches
        .map(|arr| arr.iter().filter_map(candidate_from).collect())
        .unwrap_or_default())
}

/// `workspaces.anchors.find_by_token` → how many anchors match `token`.
pub async fn count_anchors(workspace_id: &str, token: &str) -> Result<usize, String> {
    let v = workspaces_call(
        "workspaces.anchors.find_by_token",
        json!({ "workspace_id": workspace_id, "token": token }),
    )
    .await?;
    Ok(v.get("count")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .or_else(|| v.get("anchors").and_then(Value::as_array).map(Vec::len))
        .unwrap_or(0))
}

/// Resolve one scanned token to its recognition state. Identifier tokens hit
/// the symbol index AND the anchor store (a word can be both); `{{anchor}}`
/// tokens hit the anchor store only (spaces make them non-symbols).
pub async fn recognize(workspace_id: &str, token: TokenSpan) -> Result<WordRecognition, String> {
    let mut w = WordRecognition::new(token);
    match w.token.kind {
        TokenKind::Identifier => {
            let symbols = find_symbols(workspace_id, &w.token.text, FIND_LIMIT).await?;
            // The anchor lookup riding the same scan is best-effort: a
            // failure here shouldn't throw away a good symbol answer.
            let anchors = count_anchors(workspace_id, &w.token.text)
                .await
                .unwrap_or(0);
            w.candidates = symbols;
            w.anchor_count = anchors;
        }
        TokenKind::AnchorRef => {
            w.anchor_count = count_anchors(workspace_id, &w.token.text).await?;
        }
    }
    Ok(w)
}

/// One `matches[i]` entry (`{ entry: {...}, score }`) → [`SymbolCandidate`].
fn candidate_from(m: &Value) -> Option<SymbolCandidate> {
    let entry = m.get("entry")?;
    let s = |k: &str| {
        entry
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let id = s("id");
    if id.is_empty() {
        return None;
    }
    Some(SymbolCandidate {
        id,
        name: s("name"),
        kind: s("kind"),
        file: s("file"),
        line: entry.get("line").and_then(Value::as_u64).unwrap_or(0) as u32,
        module_path: s("module_path"),
        score: m.get("score").and_then(Value::as_f64).unwrap_or(0.0) as f32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn candidate_parses_the_wire_shape() {
        let m = json!({
            "entry": {
                "id": "set_active",
                "name": "set_active",
                "kind": "Function",
                "file": "src/registry.rs",
                "line": 42,
                "module_path": "wylde-workspaces::registry"
            },
            "score": 0.83
        });
        let c = candidate_from(&m).unwrap();
        assert_eq!(c.id, "set_active");
        assert_eq!(c.kind, "Function");
        assert_eq!(c.line, 42);
        assert!((c.score - 0.83).abs() < 1e-6);
    }

    #[test]
    fn malformed_entries_drop_quietly() {
        assert!(candidate_from(&json!({ "score": 1.0 })).is_none());
        assert!(candidate_from(&json!({ "entry": { "name": "x" } })).is_none());
    }
}

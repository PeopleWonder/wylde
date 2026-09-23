//! Post-turn extraction pass (improvement plan **B4 + B14**).
//!
//! The system prompt's memory rule has always told the model "the system
//! automatically tracks important context from your conversation through
//! a post-turn extraction pass" — and until now that pass did not exist
//! (the Rust port carried only the name-detection scaffold plus the
//! idle-time consolidation). Three fully-built consumer pipelines sat
//! idle for lack of one producer:
//!
//! * the **working-memory store** (short-term entries, injected every
//!   turn as the never-drop `### Conversation memory` slot),
//! * the **profile proposal queue** (`user_profile::reflection` — OI-7
//!   spam gates + OI-11 rejection suppression + GUI accept/reject),
//! * the **anchor proposal queue** (`workspaces.anchors.propose` — same
//!   gate family, Vocabulary-tab review).
//!
//! This module is that producer: ONE LLM call per completed turn, reading
//! the finished user/assistant exchange and emitting strict JSON
//! (`{memory_entries, profile_proposals, anchor_proposals}`) that feeds
//! all three gates. Nothing here writes user-visible state directly —
//! memory entries land in working memory (compacted later by idle
//! consolidation), and both proposal kinds queue for explicit user
//! accept (OI-18).
//!
//! ## Bounds and fail-softness
//!
//! * Spawned in the background by the turn driver — never delays a reply.
//! * `WYLDE_POST_TURN_EXTRACTION=off|0|false` disables the pass; no
//!   default chat model (`WYLDE_DEFAULT_MODEL` unset) skips it.
//! * Per-turn caps ([`MAX_MEMORY_ENTRIES`] / [`MAX_PROFILE_PROPOSALS`] /
//!   [`MAX_ANCHOR_PROPOSALS`]) bound the LLM's enthusiasm before the
//!   downstream gates apply their own quotas/cooldowns.
//! * Every failure (model down, garbage output, gate refusal, service
//!   unreachable) is swallowed with a trace log — extraction can never
//!   affect the turn.
//!
//! The extraction prompt is catalog-managed ([`EXTRACTION_PROMPT_ID`],
//! per B9) so it's tunable from Settings without a rebuild.
//!
//! ## Grammar-constrained decoding (2026-07-13)
//!
//! The reply is machine-consumed fixed-schema JSON — exactly the class the
//! constrained-decoding policy says to constrain (see the table in
//! `turn/reasoning/constrained.rs`). By default the call carries
//! [`extraction_format`] as Ollama's `format` (schema-forced decoding, the
//! same treatment that took PLAN 93.3% → 100% valid); the
//! `WYLDE_CONSTRAINED_EXTRACTION=off|0|false` kill switch drops back to
//! the legacy JSON *mode* (`"format": "json"`) + lenient parser. Fail-soft
//! twice over: a backend that rejects the schema is retried once freehand
//! (`ollama_chat_maybe_constrained`), and [`parse_extraction`] stays
//! lenient so an unconstrained reply parses exactly as before. The
//! schema-vs-parser lockstep tests below are load-bearing: this Ollama
//! build silently ignores malformed schemas (HTTP 200, unconstrained), so
//! a schema bug fails open with no runtime signal.

use serde_json::{json, Value};

use crate::user_profile::reflection::{self, ProposalCandidate};

/// Catalog id of the extraction system prompt (B9-managed).
pub const EXTRACTION_PROMPT_ID: &str = "chat.post_turn_extraction";

/// Per-turn cap on extracted working-memory entries.
pub const MAX_MEMORY_ENTRIES: usize = 3;
/// Ceiling on extractor-assigned importance (M6): 9–10 are reserved
/// for the user's own hand-flagged memories; a small-model extractor
/// emitting junk tens must not outrank those.
pub const MAX_EXTRACTOR_IMPORTANCE: i32 = 8;
/// Per-turn cap on profile proposals handed to the OI-7 gate.
pub const MAX_PROFILE_PROPOSALS: usize = 2;
/// Per-turn cap on anchor proposals forwarded to the workspace gate.
pub const MAX_ANCHOR_PROPOSALS: usize = 2;

/// Cap on the exchange text fed to the extractor (chars) — a huge paste
/// doesn't need to be re-read in full to decide what to remember.
const MAX_EXCHANGE_CHARS: usize = 12_000;

/// One extracted working-memory entry.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryEntry {
    /// `fact` / `decision` / `preference` (free-form; `fact` default).
    pub kind: String,
    pub text: String,
    /// LLM-judged importance (memory plan M6), normalised 1..=10 at
    /// parse time and capped at [`MAX_EXTRACTOR_IMPORTANCE`] — 9–10
    /// stay reserved for hand-flagged memories. Rides the working-
    /// memory entry so the consolidation cycle inherits real signal
    /// instead of pinning a constant.
    pub importance: i32,
}

/// One extracted anchor candidate (always a concept — the extractor has
/// no symbol index; code-symbol anchors come from the composer flow).
#[derive(Debug, Clone, PartialEq)]
pub struct AnchorCandidate {
    pub identifier: String,
    pub description: String,
    pub rationale: String,
    pub confidence: f64,
}

/// The parsed output of one extraction call, capped and validated.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Extraction {
    pub memory_entries: Vec<MemoryEntry>,
    pub profile_proposals: Vec<ProposalCandidate>,
    pub anchor_proposals: Vec<AnchorCandidate>,
}

impl Extraction {
    pub fn is_empty(&self) -> bool {
        self.memory_entries.is_empty()
            && self.profile_proposals.is_empty()
            && self.anchor_proposals.is_empty()
    }
}

/// What one pass did — for logs and tests.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExtractionStats {
    pub memory_entries_saved: usize,
    pub profile_proposals_admitted: usize,
    pub anchor_proposals_sent: usize,
    /// Why the pass did nothing, when it didn't run at all.
    pub skipped: Option<String>,
}

// ── pure half: prompt input + output parsing ──────────────────────────

/// The user-message block fed to the extractor: the finished exchange,
/// length-capped.
pub fn exchange_block(user_message: &str, assistant_message: &str) -> String {
    let mut block = format!(
        "User: {}\nAssistant: {}",
        user_message.trim(),
        assistant_message.trim()
    );
    if block.chars().count() > MAX_EXCHANGE_CHARS {
        block = block.chars().take(MAX_EXCHANGE_CHARS).collect();
        block.push_str("\n[exchange truncated]");
    }
    block
}

/// Parse the model's reply into a capped, validated [`Extraction`].
/// Lenient about wrapping (code fences, prose before/after the JSON
/// object) and strict about content (blank texts dropped, identifiers
/// snake_case-normalised, confidence clamped to [0, 1]). Garbage in ⇒
/// empty extraction out — never an error.
pub fn parse_extraction(raw: &str, conversation_id: &str) -> Extraction {
    let Some(v) = lenient_json_object(raw) else {
        return Extraction::default();
    };

    let mut out = Extraction::default();

    if let Some(arr) = v.get("memory_entries").and_then(Value::as_array) {
        for e in arr {
            if out.memory_entries.len() >= MAX_MEMORY_ENTRIES {
                break;
            }
            let text = str_field(e, "text");
            if text.is_empty() {
                continue;
            }
            let kind = match str_field(e, "kind").as_str() {
                k @ ("fact" | "decision" | "preference") => k.to_owned(),
                _ => "fact".to_owned(),
            };
            // M6: the extractor's importance judgment, normalised by the
            // shared rule (numeric → clamp 1..=10; missing/garbage → the
            // length+entity heuristic, its documented fallback role) and
            // capped below the hand-flagged band.
            let importance = crate::memory::long_term::normalize_importance(
                e.get("importance").and_then(Value::as_f64),
                &text,
                0,
            )
            .min(MAX_EXTRACTOR_IMPORTANCE);
            out.memory_entries.push(MemoryEntry {
                kind,
                text,
                importance,
            });
        }
    }

    if let Some(arr) = v.get("profile_proposals").and_then(Value::as_array) {
        for p in arr {
            if out.profile_proposals.len() >= MAX_PROFILE_PROPOSALS {
                break;
            }
            let field = str_field(p, "field");
            let proposed = str_field(p, "proposed");
            if field.is_empty() || proposed.is_empty() || !valid_profile_field(&field) {
                continue;
            }
            out.profile_proposals.push(ProposalCandidate {
                field,
                proposed,
                current: None, // the gate/UI re-reads the live value for the diff
                rationale: str_field(p, "rationale"),
                confidence: confidence_of(p),
                conversation_id: Some(conversation_id.to_owned()),
            });
        }
    }

    if let Some(arr) = v.get("anchor_proposals").and_then(Value::as_array) {
        for a in arr {
            if out.anchor_proposals.len() >= MAX_ANCHOR_PROPOSALS {
                break;
            }
            let identifier = normalise_identifier(&str_field(a, "identifier"));
            let description = str_field(a, "description");
            if identifier.is_empty() || description.is_empty() {
                continue;
            }
            out.anchor_proposals.push(AnchorCandidate {
                identifier,
                description,
                rationale: str_field(a, "rationale"),
                confidence: confidence_of(a),
            });
        }
    }

    out
}

/// The canonical JSON Schema for the extractor's reply — the value handed
/// to Ollama's `format` when [`constrained_extraction_enabled`]. MUST stay
/// key-for-key in lockstep with what [`parse_extraction`] reads: a key the
/// parser reads but the schema omits can never be emitted under the
/// grammar (`additionalProperties: false`), and that drift is invisible at
/// runtime because this Ollama build fails open on schema bugs. The
/// lockstep tests below pin the coupling in both directions.
///
/// Deliberately conservative constructs only (object/array/string/
/// integer/number/enum/required/additionalProperties/minItems-style
/// bounds — the set `plan_dag_format` live-verified): `field` is a plain
/// string rather than a `pattern`/`anyOf` union because `preference:<key>`
/// needs regex support we can't verify the grammar compiler honours, and
/// an unsupported construct silently un-constrains the whole call. The
/// parser's [`valid_profile_field`] gate stays the enforcer.
pub fn extraction_format() -> Value {
    json!({
        "type": "object",
        "properties": {
            "memory_entries": {
                "type": "array",
                "maxItems": MAX_MEMORY_ENTRIES,
                "items": {
                    "type": "object",
                    "properties": {
                        "kind": {"enum": ["fact", "decision", "preference"]},
                        "text": {"type": "string"},
                        "importance": {"type": "integer", "minimum": 1, "maximum": MAX_EXTRACTOR_IMPORTANCE}
                    },
                    "required": ["kind", "text", "importance"],
                    "additionalProperties": false
                }
            },
            "profile_proposals": {
                "type": "array",
                "maxItems": MAX_PROFILE_PROPOSALS,
                "items": {
                    "type": "object",
                    "properties": {
                        "field": {"type": "string"},
                        "proposed": {"type": "string"},
                        "rationale": {"type": "string"},
                        "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0}
                    },
                    "required": ["field", "proposed", "rationale", "confidence"],
                    "additionalProperties": false
                }
            },
            "anchor_proposals": {
                "type": "array",
                "maxItems": MAX_ANCHOR_PROPOSALS,
                "items": {
                    "type": "object",
                    "properties": {
                        "identifier": {"type": "string"},
                        "description": {"type": "string"},
                        "rationale": {"type": "string"},
                        "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0}
                    },
                    "required": ["identifier", "description", "rationale", "confidence"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["memory_entries", "profile_proposals", "anchor_proposals"],
        "additionalProperties": false
    })
}

/// Pull the first balanced `{...}` object out of a possibly fence-wrapped
/// / prose-wrapped reply and parse it.
fn lenient_json_object(raw: &str) -> Option<Value> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<Value>(&raw[start..=end])
        .ok()
        .filter(Value::is_object)
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_owned()
}

fn confidence_of(v: &Value) -> f64 {
    v.get("confidence")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        .clamp(0.0, 1.0)
}

/// The profile fields the proposal gate understands (see
/// [`crate::user_profile::profile::ProfileProposal::field`]).
fn valid_profile_field(field: &str) -> bool {
    matches!(
        field,
        "name" | "style" | "free_text_rules" | "recurring_topic"
    ) || field
        .strip_prefix("preference:")
        .is_some_and(|k| !k.trim().is_empty())
}

/// Lowercase + map spaces/hyphens to underscores + strip everything else;
/// the workspace store requires alphanumeric+underscore identifiers.
fn normalise_identifier(raw: &str) -> String {
    raw.trim()
        .to_lowercase()
        .chars()
        .map(|c| if c == ' ' || c == '-' { '_' } else { c })
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

// ── impure half: the LLM call + the three sinks ───────────────────────

/// Whether the pass is enabled (`WYLDE_POST_TURN_EXTRACTION` kill switch;
/// on by default).
fn enabled() -> bool {
    match std::env::var("WYLDE_POST_TURN_EXTRACTION") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false"
        ),
        Err(_) => true,
    }
}

/// Whether the extraction call carries the [`extraction_format`] schema
/// (`WYLDE_CONSTRAINED_EXTRACTION` kill switch; on by default — same
/// idiom as the pass's own switch). Off ⇒ the legacy JSON *mode*
/// (`"format": "json"`) + lenient parser, byte-identical to the
/// pre-constrained behaviour.
pub fn constrained_extraction_enabled() -> bool {
    match std::env::var("WYLDE_CONSTRAINED_EXTRACTION") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false"
        ),
        Err(_) => true,
    }
}

/// Run one post-turn extraction pass for a completed exchange. Best-effort
/// end to end; see the module docs for the bound/fail-soft contract.
pub async fn run(
    conversation_id: &str,
    workspace_id: Option<&str>,
    user_message: &str,
    assistant_message: &str,
) -> ExtractionStats {
    if !enabled() {
        return skipped("disabled via WYLDE_POST_TURN_EXTRACTION");
    }
    let Some(model) = default_model() else {
        return skipped("no default model (WYLDE_DEFAULT_MODEL unset)");
    };
    if assistant_message.trim().is_empty() && user_message.trim().is_empty() {
        return skipped("empty exchange");
    }

    let cfg = crate::config::Config::get();
    let mut body = json!({
        "model": model,
        "messages": [
            {"role": "system",
             "content": crate::prompts::store::effective_prompt(EXTRACTION_PROMPT_ID)},
            {"role": "user", "content": exchange_block(user_message, assistant_message)},
        ],
        "priority": cfg.default_chat_priority,
        "stream": false,
        "keep_alive": "24h",
    });
    let options = crate::turn::chat_options::chat_options(&model);
    if !options.is_empty() {
        body["options"] = Value::Object(options);
    }

    // Grammar-constrained by default; the kill switch degrades to the
    // legacy JSON mode. A backend that rejects the schema is retried once
    // freehand inside the wrapper — the lenient parser owns that path.
    let format = constrained_extraction_enabled().then(extraction_format);
    if format.is_none() {
        body["format"] = json!("json");
    }
    let reply = crate::turn::reasoning::constrained::ollama_chat_maybe_constrained(
        &cfg.ollama_service,
        body,
        format.as_ref(),
    )
    .await;
    let content = match reply {
        Ok(v) => v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        Err(e) => {
            tracing::trace!("post_turn_extractor: chat call failed: {}", e.message);
            return skipped("chat call failed");
        }
    };

    let extraction = parse_extraction(&content, conversation_id);
    apply(conversation_id, workspace_id, extraction).await
}

/// Feed a parsed extraction into the three sinks. Split from [`run`] so
/// the sink wiring is testable without an LLM.
pub async fn apply(
    conversation_id: &str,
    workspace_id: Option<&str>,
    extraction: Extraction,
) -> ExtractionStats {
    let mut stats = ExtractionStats::default();

    // 1. Working-memory entries (the store stamps `at`). The M6
    // importance rides each entry so consolidation inherits it.
    for e in &extraction.memory_entries {
        let entry = json!({ "kind": e.kind, "data": e.text, "importance": e.importance });
        match crate::memory::short_term::store::append_working_memory(conversation_id, entry) {
            Ok(_) => stats.memory_entries_saved += 1,
            Err(err) => {
                tracing::trace!("post_turn_extractor: working-memory append failed: {err:?}")
            }
        }
    }

    // 2. Profile proposals → the OI-7/OI-11 gate (queue, never the profile).
    for cand in extraction.profile_proposals {
        match reflection::propose(cand) {
            Ok(p) => {
                stats.profile_proposals_admitted += 1;
                tracing::debug!(proposal_id = %p.id, field = %p.field,
                    "post_turn_extractor: profile proposal admitted");
            }
            Err(e) => {
                tracing::trace!(
                    reason = e.code(),
                    "post_turn_extractor: profile proposal refused"
                )
            }
        }
    }

    // 3. Anchor proposals → the workspace gate over IPC (active ws only).
    if let Some(ws) = workspace_id.filter(|w| !w.trim().is_empty()) {
        let client = wylde_workspaces_client::WorkspacesClient::for_service(
            crate::turn::workspace_context::workspaces_service(),
        );
        for cand in &extraction.anchor_proposals {
            let payload = anchor_propose_payload(ws, cand);
            match client.anchors_propose(payload).await {
                Ok(_) => stats.anchor_proposals_sent += 1,
                Err(e) => {
                    tracing::trace!("post_turn_extractor: anchor propose failed: {}", e.message)
                }
            }
        }
    }

    stats
}

/// The `workspaces.anchors.propose` wire payload for one candidate.
/// Always a concept target — the extractor has no symbol index.
pub fn anchor_propose_payload(workspace_id: &str, cand: &AnchorCandidate) -> Value {
    json!({
        "workspace_id": workspace_id,
        "identifier": cand.identifier,
        "kind": "concept",
        "target": { "type": "concept", "text": cand.description },
        "description": cand.description,
        "confidence": cand.confidence,
        "rationale": cand.rationale,
    })
}

/// The default chat model (`WYLDE_DEFAULT_MODEL`) — the same knob
/// `chat.complete` and the summariser fall back to.
fn default_model() -> Option<String> {
    std::env::var("WYLDE_DEFAULT_MODEL")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn skipped(reason: &str) -> ExtractionStats {
    ExtractionStats {
        skipped: Some(reason.to_owned()),
        ..ExtractionStats::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_shape_with_fences_and_prose() {
        let raw = r#"Sure! Here's the extraction:
```json
{
  "memory_entries": [
    {"kind": "decision", "text": "The project pins gpui at rev b3d93d44."},
    {"kind": "weird", "text": "Sam prefers terse replies."}
  ],
  "profile_proposals": [
    {"field": "style", "proposed": "terse", "rationale": "asked twice", "confidence": 0.9}
  ],
  "anchor_proposals": [
    {"identifier": "The Gather", "description": "the pre-LLM context gather", "rationale": "used as vocabulary", "confidence": 0.8}
  ]
}
```"#;
        let x = parse_extraction(raw, "c1");
        assert_eq!(x.memory_entries.len(), 2);
        assert_eq!(x.memory_entries[0].kind, "decision");
        assert_eq!(x.memory_entries[1].kind, "fact", "unknown kind → fact");
        assert_eq!(x.profile_proposals.len(), 1);
        assert_eq!(x.profile_proposals[0].field, "style");
        assert_eq!(
            x.profile_proposals[0].conversation_id.as_deref(),
            Some("c1")
        );
        assert_eq!(x.anchor_proposals.len(), 1);
        assert_eq!(
            x.anchor_proposals[0].identifier, "the_gather",
            "identifier snake_case-normalised"
        );
    }

    #[test]
    fn parse_drops_invalid_rows_and_caps_counts() {
        let raw = json!({
            "memory_entries": (0..10).map(|i| json!({"text": format!("m{i}")})).collect::<Vec<_>>(),
            "profile_proposals": [
                {"field": "hairstyle", "proposed": "x"},          // unknown field
                {"field": "preference:", "proposed": "x"},        // empty key
                {"field": "preference:editor", "proposed": "vim"},
                {"field": "name", "proposed": ""},                // empty value
                {"field": "name", "proposed": "Sam"},
                {"field": "style", "proposed": "terse"},          // over cap
            ],
            "anchor_proposals": [
                {"identifier": "ok_term", "description": "a term"},
                {"identifier": "???", "description": "no usable chars"},
                {"identifier": "", "description": "empty"},
            ],
        })
        .to_string();
        let x = parse_extraction(&raw, "c1");
        assert_eq!(x.memory_entries.len(), MAX_MEMORY_ENTRIES);
        let fields: Vec<&str> = x
            .profile_proposals
            .iter()
            .map(|p| p.field.as_str())
            .collect();
        assert_eq!(
            fields,
            vec!["preference:editor", "name"],
            "cap = 2, invalid dropped"
        );
        assert_eq!(x.anchor_proposals.len(), 1);
        assert_eq!(x.anchor_proposals[0].identifier, "ok_term");
    }

    #[test]
    fn parse_importance_clamps_and_falls_back_to_heuristic() {
        let raw = json!({
            "memory_entries": [
                {"text": "judged entry", "importance": 6},
                {"text": "over-enthusiastic entry", "importance": 10},
                {"text": "short", "importance": "junk"},
            ],
        })
        .to_string();
        let x = parse_extraction(&raw, "c1");
        assert_eq!(x.memory_entries[0].importance, 6, "LLM judgment kept");
        assert_eq!(
            x.memory_entries[1].importance, MAX_EXTRACTOR_IMPORTANCE,
            "9-10 reserved for hand-flagged"
        );
        // Non-numeric → the length+entity heuristic (3 for a short body).
        assert_eq!(x.memory_entries[2].importance, 3, "heuristic fallback");
    }

    /// Lockstep direction 1 (schema → parser): the minimal object the
    /// grammar admits (`required` = the three arrays, all empty) must
    /// parse to a clean empty extraction.
    #[test]
    fn minimal_schema_conformant_reply_parses() {
        let minimal = json!({
            "memory_entries": [],
            "profile_proposals": [],
            "anchor_proposals": [],
        })
        .to_string();
        assert!(parse_extraction(&minimal, "c1").is_empty());
    }

    /// Lockstep direction 1, maximal: a reply exercising every field the
    /// grammar can force (full rows at every cap, valid kinds/fields) must
    /// parse with NOTHING dropped — grammar-forced output the parser
    /// rejects would mean the schema admits shapes the parser doesn't.
    #[test]
    fn maximal_schema_conformant_reply_parses_lossless() {
        let kinds = ["fact", "decision", "preference"];
        let maximal = json!({
            "memory_entries": (0..MAX_MEMORY_ENTRIES).map(|i| json!({
                "kind": kinds[i % 3],
                "text": format!("entry {i}"),
                "importance": MAX_EXTRACTOR_IMPORTANCE,
            })).collect::<Vec<_>>(),
            "profile_proposals": [
                {"field": "style", "proposed": "terse", "rationale": "r", "confidence": 0.9},
                {"field": "preference:editor", "proposed": "vim", "rationale": "r", "confidence": 1.0},
            ],
            "anchor_proposals": [
                {"identifier": "the_gather", "description": "d", "rationale": "r", "confidence": 0.8},
                {"identifier": "exit_edges", "description": "d", "rationale": "r", "confidence": 0.0},
            ],
        })
        .to_string();
        let x = parse_extraction(&maximal, "c1");
        assert_eq!(x.memory_entries.len(), MAX_MEMORY_ENTRIES);
        assert!(x
            .memory_entries
            .iter()
            .all(|e| e.importance == MAX_EXTRACTOR_IMPORTANCE));
        assert_eq!(x.profile_proposals.len(), MAX_PROFILE_PROPOSALS);
        assert_eq!(x.profile_proposals[1].field, "preference:editor");
        assert_eq!(x.anchor_proposals.len(), MAX_ANCHOR_PROPOSALS);
        assert_eq!(x.anchor_proposals[0].rationale, "r");
        assert_eq!(x.anchor_proposals[1].confidence, 0.0);
    }

    /// Lockstep direction 2 (parser → schema): every key the parser reads
    /// must be admitted by the schema (under `additionalProperties: false`
    /// an omitted key can never be emitted), the `kind` enum must equal
    /// the parser's accepted set, and the array caps / importance ceiling
    /// must mirror the parser's constants.
    #[test]
    fn schema_admits_exactly_what_the_parser_reads() {
        let s = extraction_format();
        assert_eq!(
            s["required"],
            json!(["memory_entries", "profile_proposals", "anchor_proposals"])
        );

        let mem = &s["properties"]["memory_entries"];
        assert_eq!(mem["maxItems"], MAX_MEMORY_ENTRIES);
        assert_eq!(
            mem["items"]["required"],
            json!(["kind", "text", "importance"])
        );
        assert_eq!(
            mem["items"]["properties"]["kind"]["enum"],
            json!(["fact", "decision", "preference"]),
            "schema kinds must be the parser's pass-through set"
        );
        assert_eq!(
            mem["items"]["properties"]["importance"]["maximum"], MAX_EXTRACTOR_IMPORTANCE,
            "grammar must not admit the hand-flagged 9-10 band"
        );

        let prof = &s["properties"]["profile_proposals"];
        assert_eq!(prof["maxItems"], MAX_PROFILE_PROPOSALS);
        assert_eq!(
            prof["items"]["required"],
            json!(["field", "proposed", "rationale", "confidence"])
        );

        let anch = &s["properties"]["anchor_proposals"];
        assert_eq!(anch["maxItems"], MAX_ANCHOR_PROPOSALS);
        assert_eq!(
            anch["items"]["required"],
            json!(["identifier", "description", "rationale", "confidence"])
        );

        // Closed objects at every level — the property that makes
        // direction 2 load-bearing.
        assert_eq!(s["additionalProperties"], false);
        for arr in ["memory_entries", "profile_proposals", "anchor_proposals"] {
            assert_eq!(
                s["properties"][arr]["items"]["additionalProperties"], false,
                "{arr} items must be closed"
            );
        }
    }

    #[test]
    fn constrained_extraction_toggle_parses_kill_switch() {
        let _g = crate::memory::common::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var("WYLDE_CONSTRAINED_EXTRACTION").ok(); // wylde-check: discard-result-ok
        std::env::remove_var("WYLDE_CONSTRAINED_EXTRACTION");
        assert!(constrained_extraction_enabled(), "default is on");
        for v in ["off", "0", "false", " OFF "] {
            std::env::set_var("WYLDE_CONSTRAINED_EXTRACTION", v);
            assert!(!constrained_extraction_enabled(), "{v:?} must disable");
        }
        match prev {
            Some(v) => std::env::set_var("WYLDE_CONSTRAINED_EXTRACTION", v),
            None => std::env::remove_var("WYLDE_CONSTRAINED_EXTRACTION"),
        }
    }

    #[test]
    fn parse_garbage_yields_empty_extraction() {
        assert!(parse_extraction("", "c").is_empty());
        assert!(parse_extraction("no json here", "c").is_empty());
        assert!(parse_extraction("[1, 2, 3]", "c").is_empty());
        assert!(parse_extraction("{broken", "c").is_empty());
        // An empty object is a valid "nothing worth extracting" reply.
        assert!(parse_extraction("{}", "c").is_empty());
    }

    #[test]
    fn exchange_block_formats_and_truncates() {
        let b = exchange_block("  hi  ", "hello there");
        assert_eq!(b, "User: hi\nAssistant: hello there");
        let long = "x".repeat(20_000);
        let b = exchange_block(&long, "y");
        assert!(b.chars().count() <= MAX_EXCHANGE_CHARS + 25);
        assert!(b.ends_with("[exchange truncated]"));
    }

    #[test]
    fn anchor_payload_carries_concept_target_and_gate_inputs() {
        let cand = AnchorCandidate {
            identifier: "the_gather".into(),
            description: "the pre-LLM context gather".into(),
            rationale: "used as vocabulary".into(),
            confidence: 0.8,
        };
        let p = anchor_propose_payload("ws-1", &cand);
        assert_eq!(p["workspace_id"], "ws-1");
        assert_eq!(p["identifier"], "the_gather");
        assert_eq!(p["target"]["type"], "concept");
        assert_eq!(p["target"]["text"], "the pre-LLM context gather");
        assert_eq!(p["confidence"], 0.8);
    }

    #[tokio::test]
    async fn apply_feeds_working_memory_and_the_profile_gate() {
        let _env = crate::user_profile::test_support::TestEnv::new();
        let x = Extraction {
            memory_entries: vec![MemoryEntry {
                kind: "decision".into(),
                text: "pin gpui at b3d93d44".into(),
                importance: 6,
            }],
            profile_proposals: vec![
                ProposalCandidate {
                    field: "style".into(),
                    proposed: "terse".into(),
                    current: None,
                    rationale: "asked twice".into(),
                    confidence: 0.9,
                    conversation_id: Some("c1".into()),
                },
                // Below the OI-7 confidence floor → the gate refuses it.
                ProposalCandidate {
                    field: "name".into(),
                    proposed: "Sam".into(),
                    current: None,
                    rationale: "weak".into(),
                    confidence: 0.3,
                    conversation_id: Some("c1".into()),
                },
            ],
            anchor_proposals: Vec::new(),
        };
        let stats = apply("c1", None, x).await;
        assert_eq!(stats.memory_entries_saved, 1);
        assert_eq!(stats.profile_proposals_admitted, 1, "the 0.3 one refused");
        assert_eq!(stats.anchor_proposals_sent, 0);

        // The working-memory entry landed with the {kind, at, data} shape.
        let wm = crate::memory::short_term::store::get_working_memory("c1").unwrap();
        assert_eq!(wm.len(), 1);
        assert_eq!(wm[0]["kind"], "decision");
        assert_eq!(wm[0]["data"], "pin gpui at b3d93d44");
        assert!(wm[0].get("at").is_some(), "store stamps the timestamp");
        assert_eq!(wm[0]["importance"], 6, "M6 importance rides the entry");

        // The admitted proposal is in the pending queue, not the profile.
        let store = crate::user_profile::store::read();
        assert_eq!(store.pending.len(), 1);
        assert_eq!(store.pending[0].field, "style");
        assert!(store.profile.style.is_none(), "OI-18: queue, never write");
    }

    #[tokio::test]
    async fn run_skips_cleanly_without_a_model() {
        let _env = crate::user_profile::test_support::TestEnv::new();
        let prior = std::env::var_os("WYLDE_DEFAULT_MODEL");
        std::env::remove_var("WYLDE_DEFAULT_MODEL");
        let stats = run("c1", None, "hi", "hello").await;
        assert!(stats.skipped.is_some());
        assert_eq!(stats.memory_entries_saved, 0);
        if let Some(v) = prior {
            std::env::set_var("WYLDE_DEFAULT_MODEL", v);
        }
    }
}

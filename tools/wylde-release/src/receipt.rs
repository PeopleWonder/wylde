//! The preflight receipt — the record that makes every local gate
//! un-skippable (roadmap T0.1, enforcement-matrix row 14).
//!
//! ## Why a receipt at all
//!
//! CI structurally cannot run the launch-and-verify checks (no GPU, Ollama,
//! Memgraph, or desktop session on a GitHub runner). Those are exactly the
//! checks that caught "shipped broken." So the gate has to live on the release
//! machine — and a gate you have to *remember* to run is the same failure mode
//! as no gate. The receipt closes that: `preflight` writes it, `publish`
//! refuses without a valid one, so "the live system was verified" becomes a
//! precondition of shipping rather than a habit.
//!
//! ## Trust model — deliberately not cryptographic
//!
//! A JSON file is trivially forgeable. **That does not matter here**, and
//! pretending otherwise would be security theatre. This is a solo developer
//! releasing his own software: the threat is *forgetting to run the checks*,
//! not an attacker fabricating a receipt to sneak a build past himself. So we
//! spend the complexity budget on the one property that actually prevents the
//! real failure — **binding the receipt to the exact commit** — and not on
//! signatures:
//!
//! * `commit` must equal the commit being published. A receipt from an earlier
//!   commit cannot validate a new build — the moment you change code, the last
//!   receipt goes stale and `publish` demands a fresh `preflight`.
//! * `git_dirty` must be false. A receipt taken over uncommitted changes does
//!   not describe a reproducible commit, so it is rejected.
//! * `version` must equal the release tag (minus a leading `v`), and
//!   `all_green` must be true.
//!
//! If a second maintainer ever appears and forgery becomes a real threat, the
//! receipt can be attached to (and its hash signed alongside) the GitHub
//! Release; the shape here is forward-compatible with that. Until then, YAGNI.

use serde::{Deserialize, Serialize};

use crate::bench::HostEnv;

/// Current receipt schema version.
pub const RECEIPT_SCHEMA: u32 = 1;

/// Default receipt filename, written to the repo root by `preflight` and read
/// by `publish`. Gitignored (it is machine- and moment-specific).
pub const RECEIPT_FILENAME: &str = "preflight-receipt.json";

/// A single gate's outcome in the receipt.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateOutcome {
    Pass,
    Fail,
    /// The gate did not run (e.g. `--skip-build`); recorded honestly rather
    /// than silently omitted.
    Skipped,
}

/// One benchmark delta, denormalised into the receipt so the release record is
/// self-contained (you can read what the numbers were without re-running).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BenchDelta {
    pub baseline: f64,
    /// `None` if the benchmark was skipped.
    pub current: Option<f64>,
    pub status: String,
    pub gate: String,
    pub detail: String,
}

/// The preflight receipt.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub schema: u32,
    /// Full commit SHA the preflight ran on.
    pub commit: String,
    /// `true` if the working tree had uncommitted changes at preflight time.
    pub git_dirty: bool,
    /// Workspace version at preflight time (both roots agree — G7 checked it).
    pub version: String,
    /// ISO-8601 UTC timestamp the caller supplies (the tool keeps no clock of
    /// its own to stay deterministic/testable).
    pub timestamp: String,
    pub host: HostEnv,
    /// Named gate → outcome. `benchmarks` and `version_consistency_g7` always
    /// present; `build_artifacts` present when L1 ran.
    pub gates: std::collections::BTreeMap<String, GateOutcome>,
    /// Per-metric benchmark deltas, keyed by metric.
    pub benchmarks: std::collections::BTreeMap<String, BenchDelta>,
    /// Non-blocking warnings surfaced to the operator (soft regressions,
    /// improvements to re-baseline, skips that were allowed).
    #[serde(default)]
    pub warnings: Vec<String>,
    /// The bottom line: every gate passed and nothing required was skipped.
    pub all_green: bool,
    /// Whether the **L2 cold-start + L3 service-health** launch gate ran and
    /// every one of its checks passed. `false` on any receipt written without
    /// `preflight --launch` (and, via serde default, on any pre-launch-gate
    /// receipt). `publish` refuses a receipt that is not launch-verified — that
    /// is what makes the launch-and-verify checks un-skippable at release, the
    /// exact "shipped a build whose running system was never verified" failure.
    #[serde(default)]
    pub launch_verified: bool,
}

impl Receipt {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Receipt> {
        use anyhow::Context;
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading receipt {}", path.display()))?;
        let r: Receipt = serde_json::from_str(&raw)
            .with_context(|| format!("parsing receipt {}", path.display()))?;
        Ok(r)
    }

    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        use anyhow::Context;
        let mut json = serde_json::to_string_pretty(self).context("serialising receipt")?;
        json.push('\n');
        std::fs::write(path, json)
            .with_context(|| format!("writing receipt {}", path.display()))?;
        Ok(())
    }
}

/// Why a receipt is not valid for publishing a given (commit, version). Each
/// variant is a distinct, human-actionable reason.
#[derive(Debug, PartialEq, Eq)]
pub enum ReceiptError {
    SchemaMismatch {
        found: u32,
        want: u32,
    },
    NotGreen,
    Dirty,
    /// The receipt is green but its L2/L3 launch-and-verify gate never ran (or
    /// didn't fully pass) — the running system was not verified.
    NotLaunchVerified,
    CommitMismatch {
        receipt: String,
        head: String,
    },
    VersionMismatch {
        receipt: String,
        tag: String,
    },
}

impl std::fmt::Display for ReceiptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReceiptError::SchemaMismatch { found, want } => write!(
                f,
                "receipt schema {found} != supported {want} — re-run `wylde-release preflight`"
            ),
            ReceiptError::NotGreen => write!(
                f,
                "receipt is not all-green — a gate failed or a required benchmark was skipped; \
                 fix it and re-run `wylde-release preflight`"
            ),
            ReceiptError::Dirty => write!(
                f,
                "receipt was taken over a dirty working tree — commit your changes and re-run \
                 `wylde-release preflight` so the receipt describes a real commit"
            ),
            ReceiptError::NotLaunchVerified => write!(
                f,
                "receipt is not launch-verified — the L2 cold-start + L3 service-health gate did \
                 not run or did not fully pass. Run `wylde-release preflight --launch` on the \
                 release machine (with the stack up) so the running system is actually verified"
            ),
            ReceiptError::CommitMismatch { receipt, head } => write!(
                f,
                "receipt is for commit {} but you are publishing {} — the code changed since \
                 preflight; re-run `wylde-release preflight`",
                short(receipt),
                short(head)
            ),
            ReceiptError::VersionMismatch { receipt, tag } => write!(
                f,
                "receipt version {receipt} != release tag {tag} — bump/preflight the version you \
                 intend to ship"
            ),
        }
    }
}

fn short(sha: &str) -> &str {
    if sha.len() >= 8 {
        &sha[..8]
    } else {
        sha
    }
}

/// Validate a receipt for publishing `version` at `head_commit`. This is the
/// pure gate `publish` calls; it is exhaustively unit-tested. `tag_version` is
/// the release tag with any leading `v` already stripped.
pub fn validate_for_publish(
    r: &Receipt,
    head_commit: &str,
    tag_version: &str,
) -> Result<(), ReceiptError> {
    if r.schema != RECEIPT_SCHEMA {
        return Err(ReceiptError::SchemaMismatch {
            found: r.schema,
            want: RECEIPT_SCHEMA,
        });
    }
    if !r.all_green {
        return Err(ReceiptError::NotGreen);
    }
    if r.git_dirty {
        return Err(ReceiptError::Dirty);
    }
    if !r.launch_verified {
        return Err(ReceiptError::NotLaunchVerified);
    }
    if r.commit != head_commit {
        return Err(ReceiptError::CommitMismatch {
            receipt: r.commit.clone(),
            head: head_commit.to_string(),
        });
    }
    if r.version != tag_version {
        return Err(ReceiptError::VersionMismatch {
            receipt: r.version.clone(),
            tag: tag_version.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn host() -> HostEnv {
        HostEnv {
            label: "rig".into(),
            cpu: "cpu".into(),
            gpu: "gpu".into(),
            ram: "ram".into(),
            os: "windows".into(),
            model: "model".into(),
            ollama: "0".into(),
        }
    }

    fn green_receipt() -> Receipt {
        let mut gates = BTreeMap::new();
        gates.insert("benchmarks".into(), GateOutcome::Pass);
        gates.insert("version_consistency_g7".into(), GateOutcome::Pass);
        Receipt {
            schema: RECEIPT_SCHEMA,
            commit: "deadbeefcafebabe0123".into(),
            git_dirty: false,
            version: "0.1.5".into(),
            timestamp: "2026-07-15T00:00:00Z".into(),
            host: host(),
            gates,
            benchmarks: BTreeMap::new(),
            warnings: vec![],
            all_green: true,
            launch_verified: true,
        }
    }

    #[test]
    fn a_matching_green_receipt_validates() {
        let r = green_receipt();
        assert!(validate_for_publish(&r, "deadbeefcafebabe0123", "0.1.5").is_ok());
    }

    #[test]
    fn stale_commit_is_rejected() {
        // The heart of the design: a receipt from an earlier commit cannot
        // validate a new build.
        let r = green_receipt();
        let err = validate_for_publish(&r, "aNewCommitSha000", "0.1.5").unwrap_err();
        assert!(matches!(err, ReceiptError::CommitMismatch { .. }));
    }

    #[test]
    fn dirty_tree_is_rejected() {
        let mut r = green_receipt();
        r.git_dirty = true;
        assert_eq!(
            validate_for_publish(&r, "deadbeefcafebabe0123", "0.1.5"),
            Err(ReceiptError::Dirty)
        );
    }

    #[test]
    fn not_green_is_rejected() {
        let mut r = green_receipt();
        r.all_green = false;
        assert_eq!(
            validate_for_publish(&r, "deadbeefcafebabe0123", "0.1.5"),
            Err(ReceiptError::NotGreen)
        );
    }

    #[test]
    fn not_launch_verified_is_rejected() {
        // A green receipt that never ran the L2/L3 launch gate (or ran it with a
        // skipped/failed check) must not publish — the un-skippable wiring.
        let mut r = green_receipt();
        r.launch_verified = false;
        assert_eq!(
            validate_for_publish(&r, "deadbeefcafebabe0123", "0.1.5"),
            Err(ReceiptError::NotLaunchVerified)
        );
    }

    #[test]
    fn launch_verified_defaults_false_on_older_receipts() {
        // A receipt serialized before this field existed must deserialize with
        // launch_verified=false (serde default) and therefore be rejected —
        // fail-closed, never grandfathered in as verified.
        let json = r#"{
            "schema": 1, "commit": "deadbeefcafebabe0123", "git_dirty": false,
            "version": "0.1.5", "timestamp": "2026-07-15T00:00:00Z",
            "host": {"label":"r","cpu":"c","gpu":"g","ram":"m","os":"w","model":"x","ollama":"0"},
            "gates": {}, "benchmarks": {}, "all_green": true
        }"#;
        let r: Receipt = serde_json::from_str(json).unwrap();
        assert!(!r.launch_verified);
        assert_eq!(
            validate_for_publish(&r, "deadbeefcafebabe0123", "0.1.5"),
            Err(ReceiptError::NotLaunchVerified)
        );
    }

    #[test]
    fn version_mismatch_is_rejected() {
        let r = green_receipt();
        let err = validate_for_publish(&r, "deadbeefcafebabe0123", "0.2.0").unwrap_err();
        assert!(matches!(err, ReceiptError::VersionMismatch { .. }));
    }

    #[test]
    fn schema_mismatch_is_rejected() {
        let mut r = green_receipt();
        r.schema = 999;
        assert!(matches!(
            validate_for_publish(&r, "deadbeefcafebabe0123", "0.1.5"),
            Err(ReceiptError::SchemaMismatch { .. })
        ));
    }

    #[test]
    fn receipt_json_round_trips() {
        let r = green_receipt();
        let json = serde_json::to_string_pretty(&r).unwrap();
        let back: Receipt = serde_json::from_str(&json).unwrap();
        assert_eq!(back.commit, r.commit);
        assert_eq!(back.all_green, r.all_green);
    }
}

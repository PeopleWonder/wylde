//! The gold set — `(query, expected-relevant-files)` cases the eval grades the
//! arms against (concept-routing plan §6.4; relation-model addendum §6.1).
//!
//! **The fixture is a DRAFT** (`fixtures/gold_set.json`, embedded via
//! `include_str!`): authored by Dispatch because Aaron delegated it, grounded in
//! real Wylde source files, to be vetted + extended. Three case kinds:
//! `Easy` (one clear subsystem), `Conflation` (a cosine would wrongly pull an
//! adjacent same-named concept — `avoid_files` — that a `Negative` edge should
//! suppress), and `Dependency` (a depended-on concept should be pulled in —
//! `dependency_files`).
//!
//! Pure serde; the harness grades a case's `relevant_files` against a ranked
//! file list by path-suffix match ([`crate::eval::corpus`]).

use serde::{Deserialize, Serialize};

/// What the case is exercising — drives which relation-specific metric applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseKind {
    /// One clear subsystem; just recall/precision/nDCG of `relevant_files`.
    Easy,
    /// A cosine would conflate an adjacent concept (`avoid_files`); a `Negative`
    /// edge should suppress it. Measures exclusion precision.
    Conflation,
    /// A depended-on concept (`dependency_files`) should be pulled in by spread.
    /// Measures dependency recall.
    Dependency,
}

/// One gold case.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GoldCase {
    pub id: String,
    pub kind: CaseKind,
    pub query: String,
    /// Files that SHOULD ground the answer (suffix-matched, case-insensitive).
    #[serde(default)]
    pub relevant_files: Vec<String>,
    /// Conflation: files a naive cosine wrongly pulls; a `Negative` edge should
    /// keep them out.
    #[serde(default)]
    pub avoid_files: Vec<String>,
    /// Dependency: files that should be pulled in via a depends-on relation.
    #[serde(default)]
    pub dependency_files: Vec<String>,
    /// Optional expected concept ids (left empty on the live draft — the live
    /// concept store has only generic auto-cluster labels; see the fixture
    /// README).
    #[serde(default)]
    pub concepts: Vec<String>,
}

/// The whole gold set.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GoldSet {
    #[serde(default)]
    pub version: String,
    pub cases: Vec<GoldCase>,
}

impl GoldSet {
    /// Parse a gold set from JSON. Tolerant of unknown top-level keys (the
    /// fixture carries a `_README`).
    pub fn from_json(s: &str) -> Result<Self, String> {
        serde_json::from_str(s).map_err(|e| format!("gold set parse: {e}"))
    }

    /// The embedded draft fixture (`fixtures/gold_set.json`).
    pub fn embedded() -> Self {
        // include_str! is relative to THIS source file.
        const RAW: &str = include_str!("fixtures/gold_set.json");
        Self::from_json(RAW).expect("embedded gold_set.json is valid")
    }

    /// Count by kind, for the report header.
    pub fn counts(&self) -> (usize, usize, usize) {
        let mut easy = 0;
        let mut conf = 0;
        let mut dep = 0;
        for c in &self.cases {
            match c.kind {
                CaseKind::Easy => easy += 1,
                CaseKind::Conflation => conf += 1,
                CaseKind::Dependency => dep += 1,
            }
        }
        (easy, conf, dep)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_fixture_parses_and_is_nontrivial() {
        let g = GoldSet::embedded();
        assert!(
            g.cases.len() >= 30,
            "draft gold set should have ~30-50 cases, got {}",
            g.cases.len()
        );
        // Every case carries at least one relevant file to grade against.
        for c in &g.cases {
            assert!(
                !c.relevant_files.is_empty(),
                "case {} has no relevant_files",
                c.id
            );
            assert!(!c.query.trim().is_empty(), "case {} has empty query", c.id);
        }
        // The three kinds are all represented (conflation + dependency are the
        // relation-model's whole point).
        let (easy, conf, dep) = g.counts();
        assert!(easy > 0 && conf > 0 && dep > 0, "kinds: {easy}/{conf}/{dep}");
    }

    #[test]
    fn conflation_cases_carry_avoid_files() {
        let g = GoldSet::embedded();
        for c in g.cases.iter().filter(|c| c.kind == CaseKind::Conflation) {
            assert!(
                !c.avoid_files.is_empty(),
                "conflation case {} needs avoid_files",
                c.id
            );
        }
    }

    #[test]
    fn dependency_cases_carry_dependency_files() {
        let g = GoldSet::embedded();
        for c in g.cases.iter().filter(|c| c.kind == CaseKind::Dependency) {
            assert!(
                !c.dependency_files.is_empty(),
                "dependency case {} needs dependency_files",
                c.id
            );
        }
    }

    #[test]
    fn ids_are_unique() {
        let g = GoldSet::embedded();
        let mut ids: Vec<&str> = g.cases.iter().map(|c| c.id.as_str()).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate case id in gold set");
    }
}

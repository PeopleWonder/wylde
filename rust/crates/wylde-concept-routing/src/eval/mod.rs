//! Eval harness — does concept-routed retrieval beat raw-vector RAG?
//! (concept-routing plan §6.4, the thesis claim).
//!
//! **Stub — filled in by R4.** Will hold `metrics` (recall@k / precision@k /
//! nDCG / concept-activation P·R / token cost) and `harness` (the three-arm
//! runner: baseline | augment | replace over the gold set). The R1
//! `CandidateSet` log is the first calibration input it consumes; Dispatch
//! drafts the gold set (locked decision #6) before this lands.

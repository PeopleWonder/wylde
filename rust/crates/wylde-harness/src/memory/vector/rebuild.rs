//! Rebuilding a vector mirror from its authoritative JSON records (#136).
//!
//! ## Why this module exists
//!
//! Three separate doc comments in the memory layer asserted that the vector
//! mirrors were "rebuilt by `reindex` from the JSON if the two ever drift",
//! and used that claim to justify discarding a mirror whenever it looked
//! incompatible. **There was no `reindex`.** `git grep 'fn reindex'` over the
//! harness returned nothing; the verb list had no rebuild entry. The safety
//! property the destructive path relied on was fictional, and the mirror
//! drifted permanently partial in normal operation too — `embed_for_write`
//! returns `None` whenever the embedder is down or over its 1.2 s budget, the
//! record saves JSON-only, and nothing ever revisited it.
//!
//! This is that missing function. The JSON record list is canonical; this
//! re-embeds it and writes a fresh mirror stamped with the current embedder.
//!
//! ## The empty-rebuild guard
//!
//! A rebuild that embeds *nothing* — because the embedder is down — must not
//! persist. Writing an empty store over a populated one is exactly the
//! data-destroying move this issue is about, and a rebuild is the one code
//! path where it would look intentional. [`rebuild`] refuses to persist when
//! it has records to embed and none of them succeeded, leaving the existing
//! mirror untouched so a later pass can do the job properly.

use std::path::Path;

use super::{VectorStore, VectorStoreError};
use crate::memory::common::embed_model;

/// What one rebuild pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RebuildReport {
    /// Records offered to the rebuild (the authoritative JSON count).
    pub total: usize,
    /// Records successfully embedded and written to the mirror.
    pub embedded: usize,
    /// Records whose embed failed — absent from the rebuilt mirror.
    pub failed: usize,
    /// Whether the rebuilt mirror was actually persisted. `false` means the
    /// pass was abandoned and the previous mirror is untouched.
    pub persisted: bool,
}

/// Errors a rebuild can fail with. A failed rebuild never modifies the
/// existing mirror.
#[derive(Debug, thiserror::Error)]
pub enum RebuildError {
    /// Every record failed to embed — almost always a down or unreachable
    /// embedder. Refused rather than persisting an empty mirror.
    #[error(
        "rebuild embedded 0 of {total} records (is the embedder running?); \
             the existing mirror was left untouched"
    )]
    NothingEmbedded { total: usize },

    #[error("rebuild could not write the mirror: {0}")]
    Store(#[from] VectorStoreError),
}

/// Rebuild the mirror at `path` from `items` (`(record_id, text)` pairs taken
/// from the tier's authoritative JSON), embedding each with `embed`.
///
/// `embed` is injected rather than called directly so this stays testable
/// without an Ollama round-trip; production passes
/// [`crate::memory::embed_write::embed_for_write`].
///
/// Records whose embed returns `None` are counted in `failed` and simply
/// absent from the new mirror — the same partial state a normal write path
/// produces, and honest about it in the report rather than silent.
pub async fn rebuild<F, Fut>(
    path: &Path,
    dim: usize,
    items: Vec<(String, String)>,
    embed: F,
) -> Result<RebuildReport, RebuildError>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Option<Vec<f32>>>,
{
    let total = items.len();
    let mut store = VectorStore::new_with_model(dim, embed_model());
    let mut embedded = 0usize;

    for (id, text) in items {
        match embed(text).await {
            Some(vector) if vector.len() == dim => match store.insert(&id, vector) {
                Ok(()) => embedded += 1,
                Err(e) => {
                    tracing::warn!("vector rebuild: insert failed for {id}: {e}");
                }
            },
            Some(vector) => {
                tracing::warn!(
                    "vector rebuild: {id} embedded at width {} but the mirror is {dim}; skipping",
                    vector.len()
                );
            }
            None => {}
        }
    }
    let failed = total - embedded;

    // Never persist a mirror that embedded nothing when there WAS something to
    // embed — that would destroy the store the rebuild was meant to restore.
    if embedded == 0 && total > 0 {
        tracing::error!(
            "vector rebuild: embedded 0 of {total} records for {}; refusing to persist \
             an empty mirror over the existing one",
            path.display()
        );
        return Err(RebuildError::NothingEmbedded { total });
    }

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    store.persist(path)?;
    if failed > 0 {
        tracing::warn!(
            "vector rebuild: {} of {total} records could not be embedded for {}; \
             the mirror is partial",
            failed,
            path.display()
        );
    } else {
        tracing::info!(
            "vector rebuild: rebuilt {} records for {}",
            embedded,
            path.display()
        );
    }
    Ok(RebuildReport {
        total,
        embedded,
        failed,
        persisted: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn unit_vec(dim: usize, seed: f32) -> Vec<f32> {
        let mut v = vec![0.0; dim];
        v[0] = seed;
        v
    }

    #[tokio::test]
    async fn rebuild_populates_a_mirror_from_json_records() {
        let td = tempdir().unwrap();
        let path = td.path().join("m.vec.bin");
        let items = vec![
            ("a".to_owned(), "alpha".to_owned()),
            ("b".to_owned(), "beta".to_owned()),
            ("c".to_owned(), "gamma".to_owned()),
        ];

        let report = rebuild(&path, 4, items, |_t| async move { Some(unit_vec(4, 1.0)) })
            .await
            .unwrap();

        assert_eq!(report.total, 3);
        assert_eq!(report.embedded, 3);
        assert_eq!(report.failed, 0);
        assert!(report.persisted);

        // The mirror is on disk and complete — the property the missing
        // `reindex` was supposed to provide.
        let loaded = VectorStore::load(&path, 4).unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded.embed_model(), embed_model());
    }

    /// The whole point of a rebuild is recovering a drifted mirror: records
    /// that were saved JSON-only because the embedder was down must be
    /// picked up.
    #[tokio::test]
    async fn rebuild_recovers_records_a_failed_write_left_unembedded() {
        let td = tempdir().unwrap();
        let path = td.path().join("m.vec.bin");

        // Simulate the drifted state: one record made it into the mirror.
        let mut partial = VectorStore::new_with_model(4, embed_model());
        partial.insert("a", unit_vec(4, 1.0)).unwrap();
        partial.persist(&path).unwrap();
        assert_eq!(VectorStore::load(&path, 4).unwrap().len(), 1);

        // The JSON list has three. A rebuild must close the gap.
        let items = vec![
            ("a".to_owned(), "alpha".to_owned()),
            ("b".to_owned(), "beta".to_owned()),
            ("c".to_owned(), "gamma".to_owned()),
        ];
        let report = rebuild(&path, 4, items, |_t| async move { Some(unit_vec(4, 1.0)) })
            .await
            .unwrap();

        assert_eq!(report.embedded, 3);
        assert_eq!(VectorStore::load(&path, 4).unwrap().len(), 3);
    }

    /// A rebuild with a dead embedder must NOT persist an empty mirror over a
    /// populated one — that would make the recovery path the destroyer.
    #[tokio::test]
    async fn rebuild_refuses_to_persist_when_nothing_embedded() {
        let td = tempdir().unwrap();
        let path = td.path().join("m.vec.bin");

        let mut existing = VectorStore::new_with_model(4, embed_model());
        existing.insert("a", unit_vec(4, 1.0)).unwrap();
        existing.insert("b", unit_vec(4, 1.0)).unwrap();
        existing.persist(&path).unwrap();
        let before = std::fs::read(&path).unwrap();

        let items = vec![
            ("a".to_owned(), "alpha".to_owned()),
            ("b".to_owned(), "beta".to_owned()),
        ];
        // Embedder down → every embed returns None.
        let err = rebuild(&path, 4, items, |_t| async move { None })
            .await
            .expect_err("must refuse to persist an empty mirror");
        assert!(matches!(err, RebuildError::NothingEmbedded { total: 2 }));

        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "the existing mirror must be untouched by a failed rebuild"
        );
    }

    /// An empty JSON list legitimately produces an empty mirror — the guard
    /// must not block the "user deleted everything" case.
    #[tokio::test]
    async fn rebuild_of_an_empty_record_set_persists_an_empty_mirror() {
        let td = tempdir().unwrap();
        let path = td.path().join("m.vec.bin");
        let report = rebuild(&path, 4, vec![], |_t| async move { None })
            .await
            .unwrap();
        assert_eq!(report.total, 0);
        assert!(report.persisted);
        assert_eq!(VectorStore::load(&path, 4).unwrap().len(), 0);
    }

    /// A partial rebuild persists what it got and reports the shortfall
    /// rather than claiming success.
    #[tokio::test]
    async fn rebuild_reports_partial_coverage_honestly() {
        let td = tempdir().unwrap();
        let path = td.path().join("m.vec.bin");
        let items = vec![
            ("a".to_owned(), "alpha".to_owned()),
            ("skip".to_owned(), "".to_owned()),
            ("c".to_owned(), "gamma".to_owned()),
        ];
        let report = rebuild(&path, 4, items, |t: String| async move {
            if t.is_empty() {
                None
            } else {
                Some(unit_vec(4, 1.0))
            }
        })
        .await
        .unwrap();

        assert_eq!(report.total, 3);
        assert_eq!(report.embedded, 2);
        assert_eq!(
            report.failed, 1,
            "the shortfall must be reported, not hidden"
        );
        assert_eq!(VectorStore::load(&path, 4).unwrap().len(), 2);
    }
}

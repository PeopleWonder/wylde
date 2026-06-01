//! First-run download for openWakeWord ONNX bundles (Slice 11.E+).
//!
//! Drops the three ONNX files for a given model id into the layout
//! `crate::wakeword::WakeWordConfig::from_layout` expects:
//!
//! ```text
//! <wakeword_models_dir>/<vendor>/<name>/
//!   melspectrogram.onnx
//!   embedding_model.onnx
//!   <name>.onnx          (per-model classifier)
//! ```
//!
//! ## URL resolution
//!
//! The dscripka/openWakeWord repository hosts the canonical bundles in
//! `openwakeword/resources/models/`. The base files (`melspectrogram`,
//! `embedding_model`) are shared across every wake word; the
//! classifier file's name comes from the model id and varies by
//! release. We map the model id to filenames via an env-driven helper
//! so operators can mirror the bundles internally (corporate firewalls)
//! or pin a specific release.
//!
//! Env vars:
//!   * `WYLDE_VOICE_WAKEWORD_URL_BASE` — base URL; defaults to the
//!     raw-content view of dscripka/openWakeWord on GitHub.
//!   * `WYLDE_VOICE_WAKEWORD_CLASSIFIER_FILE` — explicit classifier
//!     filename override (rarely needed; used by tests).
//!
//! ## Why it's a background task
//!
//! The action handler can't block the pipe worker for tens of seconds
//! while a download completes. [`spawn_pull_job`] returns a job id
//! immediately and runs the actual download on a tokio task. The GUI
//! polls `voice.check_wake_word_model` to learn when it's done.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::config::Config;

/// File downloaded for every bundle, regardless of wake word.
const BASE_FILES: &[&str] = &["melspectrogram.onnx", "embedding_model.onnx"];

/// Default raw-content base for the openWakeWord upstream repo.
const DEFAULT_URL_BASE: &str =
    "https://raw.githubusercontent.com/dscripka/openWakeWord/main/openwakeword/resources/models";

/// Outcome a pull job records. Polled via [`PullJobs::status`].
#[derive(Debug, Clone)]
pub enum PullStatus {
    InProgress,
    Done { bundle_dir: PathBuf },
    Failed { error: String },
}

#[derive(Default)]
struct PullJobsInner {
    by_id: std::collections::HashMap<String, PullStatus>,
}

/// Process-wide store of in-flight + completed pull jobs.
#[derive(Default)]
pub struct PullJobs {
    inner: Mutex<PullJobsInner>,
}

impl PullJobs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn global() -> Arc<PullJobs> {
        static SINGLETON: std::sync::OnceLock<Arc<PullJobs>> = std::sync::OnceLock::new();
        SINGLETON.get_or_init(|| Arc::new(PullJobs::new())).clone()
    }

    async fn record(&self, job_id: &str, status: PullStatus) {
        self.inner
            .lock()
            .await
            .by_id
            .insert(job_id.to_owned(), status);
    }

    pub async fn status(&self, job_id: &str) -> Option<PullStatus> {
        self.inner.lock().await.by_id.get(job_id).cloned()
    }
}

/// Compose a 12-hex-char job id — same shape Python's
/// `uuid.uuid4().hex[:12]` produces.
fn new_job_id() -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    id.chars().take(12).collect()
}

/// Decide the URL layout for `model_id`. Returns base files + classifier
/// filename. The classifier filename is the second-segment of the model
/// id with hyphens turned into underscores plus `.onnx` — matches
/// openWakeWord's naming convention (`hey-jarvis` → `hey_jarvis.onnx`).
pub(crate) fn resolve_layout(model_id: &str) -> Option<(String, String, Vec<String>)> {
    let (_, name) = split_model_id(model_id)?;
    let mut files: Vec<String> =
        BASE_FILES.iter().map(|s| (*s).to_owned()).collect();
    let classifier = std::env::var("WYLDE_VOICE_WAKEWORD_CLASSIFIER_FILE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("{}.onnx", name.replace('-', "_")));
    files.push(classifier.clone());
    let base_url = std::env::var("WYLDE_VOICE_WAKEWORD_URL_BASE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_URL_BASE.to_owned());
    Some((base_url, classifier, files))
}

pub(crate) fn bundle_dir_for(model_id: &str, models_root: &Path) -> Option<PathBuf> {
    let (vendor, name) = split_model_id(model_id)?;
    Some(models_root.join(vendor).join(name))
}

fn split_model_id(model_id: &str) -> Option<(&str, &str)> {
    let mut parts = model_id.split('/');
    let vendor = parts.next()?;
    let name = parts.next()?;
    if vendor.is_empty() || name.is_empty() || parts.next().is_some() {
        return None;
    }
    Some((vendor, name))
}

/// Async download stub — overrideable via the trait so tests don't go
/// to the network.
#[async_trait::async_trait]
pub trait FileFetcher: Send + Sync {
    async fn fetch(&self, url: &str) -> Result<Vec<u8>, String>;
}

/// Production HTTP fetcher backed by reqwest. Avoids shelling out to
/// the OS `curl` binary (which `wylde_check` rule 31 forbids inside
/// service crates) and keeps the network round-trip on tokio.
pub struct HttpFetcher {
    client: reqwest::Client,
}

impl HttpFetcher {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!("wylde-voice/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client }
    }
}

impl Default for HttpFetcher {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl FileFetcher for HttpFetcher {
    async fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| format!("http get {url}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!(
                "http get {url}: {} {}",
                resp.status().as_u16(),
                resp.status().canonical_reason().unwrap_or("")
            ));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("http body {url}: {e}"))?;
        Ok(bytes.to_vec())
    }
}

/// Spawn a background pull. Records the job in [`PullJobs::global`] so
/// the caller can return the `job_id` to the GUI immediately while the
/// download runs.
pub fn spawn_pull_job(model_id: String) -> String {
    spawn_pull_job_with(
        model_id,
        Config::get().wakeword_models_dir.clone(),
        Arc::new(HttpFetcher::new()),
    )
}

pub fn spawn_pull_job_with<F: FileFetcher + 'static>(
    model_id: String,
    models_root: PathBuf,
    fetcher: Arc<F>,
) -> String {
    let job_id = new_job_id();
    let jobs = PullJobs::global();
    let job_for_task = job_id.clone();
    let jobs_for_task = Arc::clone(&jobs);
    tokio::spawn(async move {
        jobs_for_task
            .record(&job_for_task, PullStatus::InProgress)
            .await;
        let result = run_pull(&model_id, &models_root, fetcher.as_ref()).await;
        let status = match result {
            Ok(bundle_dir) => PullStatus::Done { bundle_dir },
            Err(e) => PullStatus::Failed { error: e },
        };
        jobs_for_task.record(&job_for_task, status).await;
    });
    job_id
}

async fn run_pull(
    model_id: &str,
    models_root: &Path,
    fetcher: &dyn FileFetcher,
) -> Result<PathBuf, String> {
    let (base_url, classifier, files) =
        resolve_layout(model_id).ok_or_else(|| format!("invalid model id: {model_id}"))?;
    let bundle = bundle_dir_for(model_id, models_root)
        .ok_or_else(|| format!("can't resolve bundle dir for {model_id}"))?;
    std::fs::create_dir_all(&bundle).map_err(|e| format!("mkdir {}: {e}", bundle.display()))?;

    for file in &files {
        let dest = bundle.join(file);
        if dest.is_file() {
            // Idempotent re-pulls: skip files that are already in place.
            // The caller can delete the bundle to force a refetch.
            continue;
        }
        let url = format!("{}/{}", base_url.trim_end_matches('/'), file);
        tracing::info!("wylde-voice: wake-word fetch {} → {}", url, dest.display());
        let bytes = fetcher.fetch(&url).await.map_err(|e| {
            format!("fetch {} ({}): {e}", file, url)
        })?;
        // Atomic-write via a `.tmp` sibling so a half-downloaded file
        // never tricks the scanner into thinking the bundle is ready.
        let tmp = dest.with_extension("onnx.tmp");
        {
            let mut f = std::fs::File::create(&tmp)
                .map_err(|e| format!("create {}: {e}", tmp.display()))?;
            f.write_all(&bytes)
                .map_err(|e| format!("write {}: {e}", tmp.display()))?;
            f.sync_all()
                .map_err(|e| format!("fsync {}: {e}", tmp.display()))?;
        }
        std::fs::rename(&tmp, &dest)
            .map_err(|e| format!("rename {} → {}: {e}", tmp.display(), dest.display()))?;
    }
    let _ = classifier; // already in `files`; kept for readability above.
    Ok(bundle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;

    struct InMemoryFetcher {
        by_url: StdMutex<std::collections::HashMap<String, Vec<u8>>>,
        seen: StdMutex<Vec<String>>,
    }

    impl InMemoryFetcher {
        fn new() -> Self {
            Self {
                by_url: StdMutex::new(std::collections::HashMap::new()),
                seen: StdMutex::new(Vec::new()),
            }
        }

        fn install(&self, url: &str, body: &[u8]) {
            self.by_url
                .lock()
                .unwrap()
                .insert(url.to_owned(), body.to_vec());
        }
    }

    #[async_trait::async_trait]
    impl FileFetcher for InMemoryFetcher {
        async fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
            self.seen.lock().unwrap().push(url.to_owned());
            self.by_url
                .lock()
                .unwrap()
                .get(url)
                .cloned()
                .ok_or_else(|| format!("no stub for {url}"))
        }
    }

    /// Process-wide lock so the tests in this module don't fight over
    /// the env vars `resolve_layout` reads. Cargo runs tests on a thread
    /// pool by default; without serialisation the in-parallel mutations
    /// to `WYLDE_VOICE_WAKEWORD_URL_BASE` leak across cases.
    fn env_lock() -> &'static StdMutex<()> {
        static LOCK: std::sync::OnceLock<StdMutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| StdMutex::new(()))
    }

    fn fresh_layout_env() -> EnvGuard {
        EnvGuard::new()
    }

    /// Snapshot + restore for the two env vars `resolve_layout` reads.
    struct EnvGuard {
        _guard: std::sync::MutexGuard<'static, ()>,
        prior_base: Option<std::ffi::OsString>,
        prior_cls: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn new() -> Self {
            let guard = env_lock().lock().unwrap_or_else(|p| p.into_inner());
            let prior_base = std::env::var_os("WYLDE_VOICE_WAKEWORD_URL_BASE");
            let prior_cls = std::env::var_os("WYLDE_VOICE_WAKEWORD_CLASSIFIER_FILE");
            std::env::remove_var("WYLDE_VOICE_WAKEWORD_URL_BASE");
            std::env::remove_var("WYLDE_VOICE_WAKEWORD_CLASSIFIER_FILE");
            Self {
                _guard: guard,
                prior_base,
                prior_cls,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.prior_base.take() {
                Some(v) => std::env::set_var("WYLDE_VOICE_WAKEWORD_URL_BASE", v),
                None => std::env::remove_var("WYLDE_VOICE_WAKEWORD_URL_BASE"),
            }
            match self.prior_cls.take() {
                Some(v) => std::env::set_var("WYLDE_VOICE_WAKEWORD_CLASSIFIER_FILE", v),
                None => std::env::remove_var("WYLDE_VOICE_WAKEWORD_CLASSIFIER_FILE"),
            }
        }
    }

    #[test]
    fn resolve_layout_default() {
        let _g = fresh_layout_env();
        let (base, classifier, files) = resolve_layout("openWakeWord/hey-jarvis").unwrap();
        assert!(base.contains("dscripka"));
        assert_eq!(classifier, "hey_jarvis.onnx");
        assert_eq!(files.len(), 3);
        assert!(files.contains(&"melspectrogram.onnx".to_owned()));
        assert!(files.contains(&"embedding_model.onnx".to_owned()));
        assert!(files.contains(&"hey_jarvis.onnx".to_owned()));
    }

    #[test]
    fn resolve_layout_classifier_override() {
        let _g = fresh_layout_env();
        std::env::set_var("WYLDE_VOICE_WAKEWORD_CLASSIFIER_FILE", "custom.onnx");
        let (_, classifier, files) = resolve_layout("openWakeWord/alexa").unwrap();
        assert_eq!(classifier, "custom.onnx");
        assert!(files.contains(&"custom.onnx".to_owned()));
    }

    #[test]
    fn resolve_layout_rejects_malformed_id() {
        let _g = fresh_layout_env();
        assert!(resolve_layout("missing-slash").is_none());
        assert!(resolve_layout("a/b/c").is_none());
        assert!(resolve_layout("").is_none());
    }

    #[test]
    fn bundle_dir_layout_matches_scanner() {
        let _g = fresh_layout_env();
        let td = TempDir::new().unwrap();
        let dir = bundle_dir_for("openWakeWord/hey-jarvis", td.path()).unwrap();
        assert_eq!(
            dir,
            td.path().join("openWakeWord").join("hey-jarvis")
        );
    }

    #[tokio::test]
    async fn run_pull_writes_three_files() {
        let _g = fresh_layout_env();
        let td = TempDir::new().unwrap();
        std::env::set_var(
            "WYLDE_VOICE_WAKEWORD_URL_BASE",
            "https://example.test/models",
        );
        let fetcher = InMemoryFetcher::new();
        fetcher.install(
            "https://example.test/models/melspectrogram.onnx",
            b"MEL",
        );
        fetcher.install(
            "https://example.test/models/embedding_model.onnx",
            b"EMB",
        );
        fetcher.install("https://example.test/models/hey_jarvis.onnx", b"CLS");
        let bundle = run_pull("openWakeWord/hey-jarvis", td.path(), &fetcher)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(bundle.join("melspectrogram.onnx")).unwrap(),
            b"MEL"
        );
        assert_eq!(
            std::fs::read(bundle.join("embedding_model.onnx")).unwrap(),
            b"EMB"
        );
        assert_eq!(
            std::fs::read(bundle.join("hey_jarvis.onnx")).unwrap(),
            b"CLS"
        );
    }

    #[tokio::test]
    async fn run_pull_idempotent_skips_existing_files() {
        let _g = fresh_layout_env();
        let td = TempDir::new().unwrap();
        std::env::set_var(
            "WYLDE_VOICE_WAKEWORD_URL_BASE",
            "https://example.test/models",
        );
        let bundle = td.path().join("openWakeWord").join("hey-jarvis");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("melspectrogram.onnx"), b"OLD").unwrap();

        let fetcher = InMemoryFetcher::new();
        fetcher.install(
            "https://example.test/models/embedding_model.onnx",
            b"EMB",
        );
        fetcher.install("https://example.test/models/hey_jarvis.onnx", b"CLS");
        let _ = run_pull("openWakeWord/hey-jarvis", td.path(), &fetcher)
            .await
            .unwrap();
        // Pre-existing file untouched.
        assert_eq!(
            std::fs::read(bundle.join("melspectrogram.onnx")).unwrap(),
            b"OLD"
        );
        // Only 2 URLs hit (mel skipped).
        assert_eq!(fetcher.seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn run_pull_surfaces_fetch_error() {
        let _g = fresh_layout_env();
        let td = TempDir::new().unwrap();
        std::env::set_var(
            "WYLDE_VOICE_WAKEWORD_URL_BASE",
            "https://example.test/models",
        );
        // Fetcher is empty → every URL fails.
        let fetcher = InMemoryFetcher::new();
        let err = run_pull("openWakeWord/hey-jarvis", td.path(), &fetcher)
            .await
            .unwrap_err();
        assert!(err.contains("fetch"), "{err}");
    }

    #[tokio::test]
    async fn pull_jobs_tracks_inflight_then_done() {
        let _g = fresh_layout_env();
        let td = TempDir::new().unwrap();
        std::env::set_var(
            "WYLDE_VOICE_WAKEWORD_URL_BASE",
            "https://example.test/models",
        );
        let fetcher = Arc::new(InMemoryFetcher::new());
        fetcher.install(
            "https://example.test/models/melspectrogram.onnx",
            b"MEL",
        );
        fetcher.install(
            "https://example.test/models/embedding_model.onnx",
            b"EMB",
        );
        fetcher.install("https://example.test/models/hey_jarvis.onnx", b"CLS");
        let job = spawn_pull_job_with(
            "openWakeWord/hey-jarvis".to_owned(),
            td.path().to_path_buf(),
            fetcher,
        );
        // Wait for completion — bounded; the in-memory fetcher returns
        // immediately so the task should land well within 2 s.
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Some(PullStatus::Done { bundle_dir }) =
                PullJobs::global().status(&job).await
            {
                assert!(bundle_dir.join("hey_jarvis.onnx").is_file());
                break;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("pull job didn't complete in 2 s");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}

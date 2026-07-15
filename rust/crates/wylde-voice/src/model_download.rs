//! Rust model bootstrap (Slice 4 — `docs/plans/voice-rust-port.md`).
//!
//! Fetches the Whisper STT + Kokoro TTS model files the Rust crate's
//! `ort` loaders read, directly in Rust — removing the dependency on
//! `Voice/download_models.py`. Same shape as the wake-word downloader
//! (`crate::wakeword::download`): a `reqwest` client, atomic per-file
//! writes, an idempotent "check then fetch" pass, and a background job
//! the GUI can poll for progress.
//!
//! ## On-disk layout
//!
//! Files land in the standard HuggingFace hub cache so the existing
//! resolvers in `actions::transcribe` / `actions::synthesize` find them
//! unchanged:
//!
//! ```text
//! <hf_cache>/models--<repo--with--dashes>/snapshots/main/<files>
//! ```
//!
//! On Windows `<hf_cache>` defaults to `%USERPROFILE%\.cache\huggingface\hub`
//! (honouring `HUGGINGFACE_HUB_CACHE` / `HF_HOME` first, exactly like
//! `huggingface_hub`). The per-platform default mirrors what
//! `Voice/download_models.py` materialised into, so a half-migrated
//! install with some models already present is a no-op.
//!
//! ## Whisper: ONNX source vs. cache key
//!
//! The Rust path runs `ort` ONNX inference, so it needs the ONNX export
//! of Whisper (`onnx/encoder_model.onnx`, `onnx/decoder_model.onnx`,
//! `tokenizer.json`, `config.json`) — these live in the
//! `onnx-community/whisper-*` repos, not the stock `openai/whisper-*`
//! PyTorch repos. We therefore fetch from the ONNX-export repo but cache
//! under the *configured* `stt_model` directory (default
//! `openai/whisper-small`) so the resolvers — which key off `cfg.stt_model`
//! — find the files. The source repo is derived from `stt_model` (or set
//! explicitly via `WYLDE_VOICE_STT_DOWNLOAD_REPO`).
//!
//! ## Integrity
//!
//! HuggingFace serves the content SHA-256 of every git-LFS file in the
//! `X-Linked-Etag` header. When present we verify the downloaded bytes
//! against it before committing the file to disk — so a truncated or
//! corrupted weight file fails fast rather than poisoning the cache. (The
//! big `.onnx` / `.bin` weights are all LFS, so this covers everything
//! that matters; small git-blob files like `config.json` carry a git
//! SHA-1 etag we deliberately don't treat as a content hash.)
//!
//! ## Everything-Rust
//!
//! No Python wrapper, no helper script, no shell-out: `reqwest`
//! (rustls-TLS) for HTTP, `sha2` for the checksum, an in-house NumPy
//! `.npz` writer (`crate::synth::voices::write_voices_npz`) to assemble
//! the Kokoro voice bundle.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::synth::voices::{write_voices_npz, VOICE_STYLE_TOTAL_F32};

/// Kokoro repo id — identical to `Voice/download_models.py::KOKORO_REPO`
/// and what `actions::synthesize::first_kokoro_snapshot` resolves against.
pub const KOKORO_REPO: &str = "onnx-community/Kokoro-82M-v1.0-ONNX";

/// Per-voice `.bin` files inside the Kokoro repo we combine into
/// `voices.npz`. Mirrors `Voice/download_models.py::KOKORO_VOICE_NAMES`
/// exactly (29 entries incl. the base `af`) so the assembled bundle is a
/// byte-for-byte parity match for the Python output.
const KOKORO_VOICE_NAMES: &[&str] = &[
    "af",
    "af_alloy",
    "af_aoede",
    "af_bella",
    "af_heart",
    "af_jessica",
    "af_kore",
    "af_nicole",
    "af_nova",
    "af_river",
    "af_sarah",
    "af_sky",
    "am_adam",
    "am_echo",
    "am_eric",
    "am_fenrir",
    "am_liam",
    "am_michael",
    "am_onyx",
    "am_puck",
    "am_santa",
    "bf_alice",
    "bf_emma",
    "bf_isabella",
    "bf_lily",
    "bm_daniel",
    "bm_fable",
    "bm_george",
    "bm_lewis",
];

/// Whisper files to fetch from the ONNX-export repo. `dest_rel` mirrors
/// the onnx-community layout (`onnx/` subdir for the weights) so the
/// resolver's `["encoder_model.onnx", "onnx/encoder_model.onnx"]` probe
/// lands.
struct RepoFile {
    repo_path: &'static str,
    /// When true a 404 / fetch failure is logged and skipped rather than
    /// failing the whole bootstrap (some Whisper exports omit it).
    optional: bool,
}

const WHISPER_FILES: &[RepoFile] = &[
    RepoFile {
        repo_path: "config.json",
        optional: false,
    },
    RepoFile {
        repo_path: "generation_config.json",
        optional: true,
    },
    RepoFile {
        repo_path: "tokenizer.json",
        optional: false,
    },
    RepoFile {
        repo_path: "onnx/encoder_model.onnx",
        optional: false,
    },
    RepoFile {
        repo_path: "onnx/decoder_model.onnx",
        optional: false,
    },
];

/// Number of file fetches the progress counter ticks through:
/// every Whisper file + Kokoro's `model.onnx` + each voice `.bin`.
fn total_file_count() -> usize {
    WHISPER_FILES.len() + 1 + KOKORO_VOICE_NAMES.len()
}

// --------------------------------------------------------------------- //
// HTTP fetch (trait-injectable so tests never hit the network).         //
// --------------------------------------------------------------------- //

/// One fetched file: the body plus the server-advertised content SHA-256
/// (the git-LFS `X-Linked-Etag`), when the response carried one.
pub struct FetchedFile {
    pub bytes: Vec<u8>,
    pub sha256: Option<String>,
}

#[async_trait::async_trait]
pub trait ModelHttp: Send + Sync {
    async fn get(&self, url: &str) -> Result<FetchedFile, String>;
}

/// Production fetcher backed by `reqwest` (rustls-TLS, no shell-out —
/// same constraint that drove the wake-word downloader, `wylde_check`
/// rule 31).
pub struct ReqwestHttp {
    client: reqwest::Client,
}

impl ReqwestHttp {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!("wylde-voice/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client }
    }
}

impl Default for ReqwestHttp {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ModelHttp for ReqwestHttp {
    async fn get(&self, url: &str) -> Result<FetchedFile, String> {
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
        // git-LFS files advertise their content SHA-256 in X-Linked-Etag.
        let sha256 = resp
            .headers()
            .get("x-linked-etag")
            .or_else(|| resp.headers().get("etag"))
            .and_then(|v| v.to_str().ok())
            .map(normalize_etag)
            .filter(|s| is_sha256_hex(s));
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("http body {url}: {e}"))?
            .to_vec();
        Ok(FetchedFile { bytes, sha256 })
    }
}

fn normalize_etag(raw: &str) -> String {
    raw.trim()
        .trim_start_matches("W/")
        .trim_matches('"')
        .to_owned()
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn verify_sha256(bytes: &[u8], expected: &str) -> Result<(), String> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let got = hex::encode(hasher.finalize());
    if got.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(format!("sha256 mismatch: expected {expected}, got {got}"))
    }
}

// --------------------------------------------------------------------- //
// Path + repo resolution.                                               //
// --------------------------------------------------------------------- //

/// HuggingFace hub cache root — same resolution order as
/// `actions::models::hf_cache_root` and `huggingface_hub.constants`.
pub fn hf_cache_root() -> PathBuf {
    if let Some(p) = std::env::var_os("HUGGINGFACE_HUB_CACHE") {
        return PathBuf::from(p);
    }
    if let Some(p) = std::env::var_os("HF_HOME") {
        return PathBuf::from(p).join("hub");
    }
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        return PathBuf::from(home)
            .join(".cache")
            .join("huggingface")
            .join("hub");
    }
    PathBuf::from(".cache/huggingface/hub")
}

/// Base URL for HF file downloads. Honours `WYLDE_VOICE_HF_ENDPOINT` then
/// `HF_ENDPOINT` (the same var `huggingface_hub` reads) so an air-gapped
/// install can point at an internal mirror.
fn hf_endpoint() -> String {
    for key in ["WYLDE_VOICE_HF_ENDPOINT", "HF_ENDPOINT"] {
        if let Ok(v) = std::env::var(key) {
            if !v.is_empty() {
                return v.trim_end_matches('/').to_owned();
            }
        }
    }
    "https://huggingface.co".to_owned()
}

/// `models--<owner>--<name>` cache directory name for `repo`.
fn cache_dir_name(repo: &str) -> String {
    format!("models--{}", repo.replace('/', "--"))
}

/// The snapshot directory we materialise files into. We use a fixed
/// `main` snapshot name (HF uses a commit SHA; the resolvers just pick the
/// first dir under `snapshots/`, so the name is immaterial to load).
fn snapshot_dir(hf_root: &Path, repo: &str) -> PathBuf {
    hf_root
        .join(cache_dir_name(repo))
        .join("snapshots")
        .join("main")
}

/// Map the configured `stt_model` to the repo that actually ships the
/// ONNX export the Rust `ort` loader needs. Overridable via
/// `WYLDE_VOICE_STT_DOWNLOAD_REPO`.
pub fn whisper_source_repo(stt_model: &str) -> String {
    if let Ok(v) = std::env::var("WYLDE_VOICE_STT_DOWNLOAD_REPO") {
        if !v.is_empty() {
            return v;
        }
    }
    // Repos that already ship ONNX exports pass straight through.
    if stt_model.contains("onnx-community") || stt_model.contains("Xenova") {
        return stt_model.to_owned();
    }
    // `<owner>/whisper-small` → `onnx-community/whisper-small`.
    let name = stt_model.rsplit('/').next().unwrap_or(stt_model);
    format!("onnx-community/{name}")
}

// --------------------------------------------------------------------- //
// Core fetch logic.                                                     //
// --------------------------------------------------------------------- //

/// Fetch `url` into `dest`, verifying the SHA-256 when the server
/// advertised one, writing atomically via a `.tmp` sibling. Returns
/// `Ok(true)` when a download happened, `Ok(false)` when the file was
/// already present (idempotent skip).
async fn fetch_to(http: &dyn ModelHttp, url: &str, dest: &Path) -> Result<bool, String> {
    if dest.is_file() {
        let nonempty = std::fs::metadata(dest)
            .map(|m| m.len() > 0)
            .unwrap_or(false);
        if nonempty {
            return Ok(false);
        }
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    tracing::info!("wylde-voice: model fetch {} → {}", url, dest.display());
    let file = http.get(url).await?;
    if let Some(expected) = &file.sha256 {
        verify_sha256(&file.bytes, expected)?;
    }
    let tmp = dest.with_extension("tmp");
    std::fs::write(&tmp, &file.bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, dest)
        .map_err(|e| format!("rename {} → {}: {e}", tmp.display(), dest.display()))?;
    Ok(true)
}

async fn ensure_whisper(
    http: &dyn ModelHttp,
    hf_root: &Path,
    endpoint: &str,
    stt_model: &str,
    bump: &(dyn Fn() + Sync),
) -> Result<PathBuf, String> {
    let source = whisper_source_repo(stt_model);
    // Cache under the *configured* model id so the resolvers (keyed on
    // cfg.stt_model) find the files even though we fetch from the ONNX repo.
    let dest_dir = snapshot_dir(hf_root, stt_model);
    for rf in WHISPER_FILES {
        let url = format!("{endpoint}/{source}/resolve/main/{}", rf.repo_path);
        let dest = dest_dir.join(rf.repo_path);
        match fetch_to(http, &url, &dest).await {
            Ok(_) => {}
            Err(e) if rf.optional => {
                tracing::warn!(
                    "wylde-voice: optional whisper file {} skipped: {e}",
                    rf.repo_path
                );
            }
            Err(e) => return Err(format!("whisper {}: {e}", rf.repo_path)),
        }
        bump();
    }
    Ok(dest_dir)
}

async fn ensure_kokoro(
    http: &dyn ModelHttp,
    hf_root: &Path,
    endpoint: &str,
    bump: &(dyn Fn() + Sync),
) -> Result<PathBuf, String> {
    let dest_dir = snapshot_dir(hf_root, KOKORO_REPO);

    // The model graph.
    let model_url = format!("{endpoint}/{KOKORO_REPO}/resolve/main/onnx/model.onnx");
    fetch_to(http, &model_url, &dest_dir.join("onnx").join("model.onnx"))
        .await
        .map_err(|e| format!("kokoro model.onnx: {e}"))?;
    bump();

    // The voice catalogue, assembled into a single voices.npz. Skip the
    // whole fetch+assemble pass when the bundle is already present
    // (mirrors Voice/download_models.py::fetch_kokoro).
    let npz = dest_dir.join("voices.npz");
    if npz.is_file() {
        for _ in KOKORO_VOICE_NAMES {
            bump();
        }
        return Ok(dest_dir);
    }

    let mut voices: Vec<(String, Vec<u8>)> = Vec::with_capacity(KOKORO_VOICE_NAMES.len());
    for name in KOKORO_VOICE_NAMES {
        let url = format!("{endpoint}/{KOKORO_REPO}/resolve/main/voices/{name}.bin");
        match http.get(&url).await {
            Ok(file) => {
                if let Some(expected) = &file.sha256 {
                    verify_sha256(&file.bytes, expected)
                        .map_err(|e| format!("kokoro voice {name}: {e}"))?;
                }
                if file.bytes.len() == VOICE_STYLE_TOTAL_F32 * 4 {
                    voices.push(((*name).to_owned(), file.bytes));
                } else {
                    tracing::warn!(
                        "wylde-voice: kokoro voice {name} unexpected size {} (want {}); skipping",
                        file.bytes.len(),
                        VOICE_STYLE_TOTAL_F32 * 4
                    );
                }
            }
            Err(e) => tracing::warn!("wylde-voice: kokoro voice {name} fetch failed: {e}"),
        }
        bump();
    }

    if voices.is_empty() {
        return Err("no kokoro voice arrays fetched; voices.npz not built".to_owned());
    }
    write_voices_npz(&voices, &npz).map_err(|e| format!("assemble voices.npz: {e}"))?;
    Ok(dest_dir)
}

/// Result of a successful bootstrap — the two snapshot dirs now populated.
#[derive(Debug, Clone)]
pub struct EnsureOutcome {
    pub whisper_dir: PathBuf,
    pub kokoro_dir: PathBuf,
}

/// Ensure both Whisper + Kokoro model files are present, fetching only
/// what's missing. `bump` is invoked once per file processed so callers
/// can render progress.
pub async fn ensure_models(
    http: &dyn ModelHttp,
    hf_root: &Path,
    stt_model: &str,
    bump: &(dyn Fn() + Sync),
) -> Result<EnsureOutcome, String> {
    let endpoint = hf_endpoint();
    let whisper_dir = ensure_whisper(http, hf_root, &endpoint, stt_model, bump).await?;
    let kokoro_dir = ensure_kokoro(http, hf_root, &endpoint, bump).await?;
    Ok(EnsureOutcome {
        whisper_dir,
        kokoro_dir,
    })
}

// --------------------------------------------------------------------- //
// Background job tracking — mirrors wakeword::download::PullJobs.        //
// --------------------------------------------------------------------- //

/// Status of a background bootstrap job, polled via [`EnsureJobs::status`].
#[derive(Debug, Clone)]
pub enum EnsureStatus {
    InProgress {
        done: usize,
        total: usize,
    },
    Done {
        whisper_dir: PathBuf,
        kokoro_dir: PathBuf,
    },
    Failed {
        error: String,
    },
}

/// Process-wide store of in-flight + completed bootstrap jobs. Uses a
/// `std::sync::Mutex` (not tokio's) so the per-file progress callback can
/// update it from the async fetch loop without holding a lock across an
/// `.await`.
#[derive(Default)]
pub struct EnsureJobs {
    inner: Mutex<HashMap<String, EnsureStatus>>,
}

impl EnsureJobs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn global() -> Arc<EnsureJobs> {
        static SINGLETON: OnceLock<Arc<EnsureJobs>> = OnceLock::new();
        SINGLETON
            .get_or_init(|| Arc::new(EnsureJobs::new()))
            .clone()
    }

    fn record(&self, job_id: &str, status: EnsureStatus) {
        if let Ok(mut g) = self.inner.lock() {
            g.insert(job_id.to_owned(), status);
        }
    }

    pub fn status(&self, job_id: &str) -> Option<EnsureStatus> {
        self.inner.lock().ok().and_then(|g| g.get(job_id).cloned())
    }
}

/// 12-hex-char job id, same shape as the wake-word downloader.
fn new_job_id() -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    id.chars().take(12).collect()
}

/// Kick a background bootstrap using the configured model + the real
/// `reqwest` fetcher. Returns the job id immediately; poll
/// [`EnsureJobs::status`] (or `voice.download_status`) for progress.
pub fn spawn_ensure_job() -> String {
    spawn_ensure_job_with(
        Config::get().stt_model.clone(),
        hf_cache_root(),
        Arc::new(ReqwestHttp::new()),
    )
}

pub fn spawn_ensure_job_with<H: ModelHttp + 'static>(
    stt_model: String,
    hf_root: PathBuf,
    http: Arc<H>,
) -> String {
    let job_id = new_job_id();
    let jobs = EnsureJobs::global();
    let total = total_file_count();
    let job_for_task = job_id.clone();

    tokio::spawn(async move {
        jobs.record(&job_for_task, EnsureStatus::InProgress { done: 0, total });
        let done = Arc::new(AtomicUsize::new(0));
        let bump = {
            let done = Arc::clone(&done);
            let jobs = Arc::clone(&jobs);
            let job = job_for_task.clone();
            move || {
                let d = done.fetch_add(1, Ordering::SeqCst) + 1;
                jobs.record(&job, EnsureStatus::InProgress { done: d, total });
            }
        };
        let result = ensure_models(http.as_ref(), &hf_root, &stt_model, &bump).await;
        let status = match result {
            Ok(outcome) => EnsureStatus::Done {
                whisper_dir: outcome.whisper_dir,
                kokoro_dir: outcome.kokoro_dir,
            },
            Err(error) => EnsureStatus::Failed { error },
        };
        jobs.record(&job_for_task, status);
    });

    job_id
}

#[cfg(test)]
mod tests {
    // The async tests below hold `env_lock()` (a process-wide std Mutex) for
    // their whole body — deliberately across `.await` — to serialise tests
    // that mutate global env vars (`WYLDE_VOICE_*`) so concurrent test threads
    // can't race each other's `set_var`/`remove_var`. That is the intended
    // lifetime of the guard, so clippy's await-holding-lock warning is a false
    // positive here; allow it for the test module only (prod code is unaffected).
    #![allow(clippy::await_holding_lock)]

    use super::*;
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;

    /// In-memory fetcher: serves installed bodies, records the URLs hit,
    /// and can attach a SHA-256 to any URL to exercise verification.
    struct FakeHttp {
        bodies: StdMutex<HashMap<String, Vec<u8>>>,
        shas: StdMutex<HashMap<String, String>>,
        seen: StdMutex<Vec<String>>,
    }

    impl FakeHttp {
        fn new() -> Self {
            Self {
                bodies: StdMutex::new(HashMap::new()),
                shas: StdMutex::new(HashMap::new()),
                seen: StdMutex::new(Vec::new()),
            }
        }
        fn install(&self, url: &str, body: Vec<u8>) {
            self.bodies.lock().unwrap().insert(url.to_owned(), body);
        }
        fn install_with_sha(&self, url: &str, body: Vec<u8>, sha: &str) {
            self.shas
                .lock()
                .unwrap()
                .insert(url.to_owned(), sha.to_owned());
            self.install(url, body);
        }
        fn hit_count(&self, url: &str) -> usize {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|u| *u == url)
                .count()
        }
    }

    #[async_trait::async_trait]
    impl ModelHttp for FakeHttp {
        async fn get(&self, url: &str) -> Result<FetchedFile, String> {
            self.seen.lock().unwrap().push(url.to_owned());
            let body = self
                .bodies
                .lock()
                .unwrap()
                .get(url)
                .cloned()
                .ok_or_else(|| format!("no stub for {url}"))?;
            let sha256 = self.shas.lock().unwrap().get(url).cloned();
            Ok(FetchedFile {
                bytes: body,
                sha256,
            })
        }
    }

    /// Serialises the few tests that mutate `WYLDE_VOICE_HF_ENDPOINT` /
    /// `WYLDE_VOICE_STT_DOWNLOAD_REPO` so parallel cargo threads don't
    /// leak env into one another.
    fn env_lock() -> &'static StdMutex<()> {
        static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| StdMutex::new(()))
    }

    const ENDPOINT: &str = "https://hf.test";

    fn voice_buf(v: f32) -> Vec<u8> {
        let mut b = Vec::with_capacity(VOICE_STYLE_TOTAL_F32 * 4);
        for _ in 0..VOICE_STYLE_TOTAL_F32 {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b
    }

    fn install_all(http: &FakeHttp, source: &str) {
        for rf in WHISPER_FILES {
            http.install(
                &format!("{ENDPOINT}/{source}/resolve/main/{}", rf.repo_path),
                format!("DATA:{}", rf.repo_path).into_bytes(),
            );
        }
        http.install(
            &format!("{ENDPOINT}/{KOKORO_REPO}/resolve/main/onnx/model.onnx"),
            b"KOKORO-ONNX".to_vec(),
        );
        for (i, name) in KOKORO_VOICE_NAMES.iter().enumerate() {
            http.install(
                &format!("{ENDPOINT}/{KOKORO_REPO}/resolve/main/voices/{name}.bin"),
                voice_buf(i as f32 * 0.01),
            );
        }
    }

    #[test]
    fn whisper_source_repo_maps_openai_to_onnx_community() {
        let _g = env_lock().lock().unwrap_or_else(|p| p.into_inner());
        std::env::remove_var("WYLDE_VOICE_STT_DOWNLOAD_REPO");
        assert_eq!(
            whisper_source_repo("openai/whisper-small"),
            "onnx-community/whisper-small"
        );
        // Already an ONNX repo → pass through.
        assert_eq!(
            whisper_source_repo("onnx-community/whisper-base"),
            "onnx-community/whisper-base"
        );
        // Explicit override wins.
        std::env::set_var("WYLDE_VOICE_STT_DOWNLOAD_REPO", "myorg/custom-onnx");
        assert_eq!(
            whisper_source_repo("openai/whisper-small"),
            "myorg/custom-onnx"
        );
        std::env::remove_var("WYLDE_VOICE_STT_DOWNLOAD_REPO");
    }

    #[test]
    fn etag_normalisation_and_sha_detection() {
        assert_eq!(normalize_etag("W/\"abc\""), "abc");
        let sha = "e".repeat(64);
        assert!(is_sha256_hex(&sha));
        assert!(!is_sha256_hex("deadbeef")); // git sha-1-ish, too short
        assert!(verify_sha256(b"", &sha).is_err());
    }

    #[tokio::test]
    async fn ensure_models_writes_whisper_and_kokoro_into_cache() {
        let _g = env_lock().lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("WYLDE_VOICE_HF_ENDPOINT", ENDPOINT);
        std::env::remove_var("WYLDE_VOICE_STT_DOWNLOAD_REPO");
        let td = TempDir::new().unwrap();
        let http = FakeHttp::new();
        let source = whisper_source_repo("openai/whisper-small");
        install_all(&http, &source);

        let outcome = ensure_models(&http, td.path(), "openai/whisper-small", &|| {})
            .await
            .expect("bootstrap succeeds");

        // Whisper cached under the *configured* id, not the source repo.
        let enc = outcome.whisper_dir.join("onnx").join("encoder_model.onnx");
        assert!(enc.is_file(), "encoder at {}", enc.display());
        assert!(outcome.whisper_dir.join("tokenizer.json").is_file());
        assert!(outcome
            .whisper_dir
            .to_string_lossy()
            .contains("models--openai--whisper-small"));

        // Kokoro model + an assembled, loadable voices.npz.
        assert!(outcome.kokoro_dir.join("onnx").join("model.onnx").is_file());
        let npz = outcome.kokoro_dir.join("voices.npz");
        assert!(npz.is_file());
        let voices = crate::synth::voices::Voices::load(&npz).expect("voices.npz loads");
        assert_eq!(voices.len(), KOKORO_VOICE_NAMES.len());
        assert!(voices.get("af_heart").is_some());

        std::env::remove_var("WYLDE_VOICE_HF_ENDPOINT");
    }

    #[tokio::test]
    async fn ensure_models_is_idempotent_second_run_refetches_nothing() {
        let _g = env_lock().lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("WYLDE_VOICE_HF_ENDPOINT", ENDPOINT);
        std::env::remove_var("WYLDE_VOICE_STT_DOWNLOAD_REPO");
        let td = TempDir::new().unwrap();
        let http = FakeHttp::new();
        let source = whisper_source_repo("openai/whisper-small");
        install_all(&http, &source);

        ensure_models(&http, td.path(), "openai/whisper-small", &|| {})
            .await
            .unwrap();
        let enc_url = format!("{ENDPOINT}/{source}/resolve/main/onnx/encoder_model.onnx");
        let voice_url = format!("{ENDPOINT}/{KOKORO_REPO}/resolve/main/voices/af.bin");
        assert_eq!(http.hit_count(&enc_url), 1);
        assert_eq!(http.hit_count(&voice_url), 1);

        // Second pass: files (and voices.npz) already present → no new hits.
        ensure_models(&http, td.path(), "openai/whisper-small", &|| {})
            .await
            .unwrap();
        assert_eq!(http.hit_count(&enc_url), 1, "encoder not refetched");
        assert_eq!(http.hit_count(&voice_url), 1, "voice not refetched");

        std::env::remove_var("WYLDE_VOICE_HF_ENDPOINT");
    }

    #[tokio::test]
    async fn ensure_models_verifies_advertised_sha256() {
        let _g = env_lock().lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("WYLDE_VOICE_HF_ENDPOINT", ENDPOINT);
        std::env::remove_var("WYLDE_VOICE_STT_DOWNLOAD_REPO");
        let td = TempDir::new().unwrap();
        let http = FakeHttp::new();
        let source = whisper_source_repo("openai/whisper-small");
        install_all(&http, &source);

        // Poison the encoder's advertised hash → bootstrap must fail and
        // NOT write the corrupted file.
        let enc_url = format!("{ENDPOINT}/{source}/resolve/main/onnx/encoder_model.onnx");
        http.install_with_sha(&enc_url, b"corrupt".to_vec(), &"a".repeat(64));

        let err = ensure_models(&http, td.path(), "openai/whisper-small", &|| {})
            .await
            .unwrap_err();
        assert!(err.contains("sha256 mismatch"), "{err}");
        let enc = snapshot_dir(td.path(), "openai/whisper-small")
            .join("onnx")
            .join("encoder_model.onnx");
        assert!(!enc.is_file(), "corrupt file must not be committed");

        std::env::remove_var("WYLDE_VOICE_HF_ENDPOINT");
    }

    #[tokio::test]
    async fn spawn_ensure_job_tracks_progress_then_done() {
        let _g = env_lock().lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("WYLDE_VOICE_HF_ENDPOINT", ENDPOINT);
        std::env::remove_var("WYLDE_VOICE_STT_DOWNLOAD_REPO");
        let td = TempDir::new().unwrap();
        let http = Arc::new(FakeHttp::new());
        let source = whisper_source_repo("openai/whisper-small");
        install_all(&http, &source);

        let job = spawn_ensure_job_with(
            "openai/whisper-small".to_owned(),
            td.path().to_path_buf(),
            http,
        );

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match EnsureJobs::global().status(&job) {
                Some(EnsureStatus::Done { kokoro_dir, .. }) => {
                    assert!(kokoro_dir.join("voices.npz").is_file());
                    break;
                }
                Some(EnsureStatus::Failed { error }) => panic!("job failed: {error}"),
                _ => {}
            }
            if tokio::time::Instant::now() > deadline {
                panic!("ensure job didn't complete in 5 s");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        std::env::remove_var("WYLDE_VOICE_HF_ENDPOINT");
    }
}

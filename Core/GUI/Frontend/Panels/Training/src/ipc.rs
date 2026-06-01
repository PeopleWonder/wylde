//! Per-panel IPC helpers for the Training panel.
//!
//! Calls land on `\\.\pipe\wylde-trainer` directly — the Python Gateway
//! retired its `/api/training/*` routes in the "trainer surface moved to
//! chat-driven flows" decision (see `Gateway/routes/__init__.py`), and
//! the GUI is expected to talk to the trainer pipe one-on-one when the
//! verbs land.  Every call uses the same HTTP-shaped envelope the
//! Images panel does (`wylde_gui_pipe::call(SVC_TRAINER, verb, path,
//! body)`), so once the trainer pipe ships the routes below this panel
//! lights up with no further wiring.
//!
//! All result-bearing helpers return `Result<_, String>`; the panel
//! shows the error verbatim in its degraded-state strip so the user can
//! tell `pipe_unavailable` (the trainer service isn't running) from a
//! verb-level error.
//!
//! Routes used:
//!
//!   * `GET  /api/jobs`                — list every known training job
//!   * `POST /api/start_training`      — body = full `TrainingConfig`
//!   * `POST /api/jobs/{id}/stop`      — cancel a running job
//!   * `GET  /api/jobs/{id}/status`    — status payload incl. loss
//!     points
//!   * `GET  /api/datasets`            — list LLaMA-Factory datasets

use serde_json::{Map, Value};

/// Pipe service name.  `wylde_gui_pipe::call` strips the prefix and
/// resolves `\\.\pipe\wylde-trainer`.
pub const SVC_TRAINER: &str = "wylde-trainer";

async fn get(path: &str) -> Result<Value, String> {
    wylde_gui_pipe::call(SVC_TRAINER, "GET", path, None).await
}

async fn post(path: &str, body: Value) -> Result<Value, String> {
    wylde_gui_pipe::call(SVC_TRAINER, "POST", path, Some(body)).await
}

// ── Status enum ────────────────────────────────────────────────────────

/// Lifecycle state of a training job.  Mirrors what the Svelte page
/// expected the trainer to emit; unknown strings fall through to
/// `Unknown` rather than rejecting the row outright.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Stopped,
    Unknown,
}

impl JobStatus {
    /// Map the trainer's string state into the enum.  Named
    /// `from_label` rather than `from_str` to dodge the `FromStr` trait
    /// clash — the trainer never emits a `Result`-shaped parse, and we
    /// want fall-through-to-Unknown semantics for unknown strings.
    pub fn from_label(s: &str) -> Self {
        match s {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "stopped" => Self::Stopped,
            _ => Self::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
            Self::Unknown => "unknown",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Stopped)
    }
}

// ── Loss curve point ───────────────────────────────────────────────────

/// One point on the live loss curve.  `epoch` may be fractional when the
/// trainer logs sub-epoch checkpoints.  `lr` is the optimiser's current
/// learning rate, surfaced as a tooltip on the bar histogram.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LossPoint {
    pub step: u64,
    pub loss: f64,
    pub epoch: f64,
    pub lr: f64,
}

impl LossPoint {
    pub fn from_value(v: &Value) -> Self {
        Self {
            step: v.get("step").and_then(|x| x.as_u64()).unwrap_or(0),
            loss: v.get("loss").and_then(|x| x.as_f64()).unwrap_or(0.0),
            epoch: v.get("epoch").and_then(|x| x.as_f64()).unwrap_or(0.0),
            lr: v.get("lr").and_then(|x| x.as_f64()).unwrap_or(0.0),
        }
    }
}

// ── Training config ────────────────────────────────────────────────────

/// The set of hyperparameters the panel's start-new-training form
/// surfaces.  Keys mirror what the Svelte page sent — the trainer's
/// `start_training` handler is the source of truth, but the planner
/// payload and the form payload were always close.
///
/// Numeric fields that look like reasonable defaults are seeded here so
/// the form pre-fills without an extra round trip.  Sane defaults match
/// what LLaMA-Factory's `examples/lora_single_gpu/llama3_lora_sft.yaml`
/// uses for an 8 B / single-GPU LoRA run.
#[derive(Debug, Clone, PartialEq)]
pub struct TrainingConfig {
    pub base_model: String,
    pub dataset_name: String,
    pub finetuning_type: String,
    pub lora_rank: u32,
    pub lora_alpha: u32,
    pub lora_dropout: f64,
    pub batch_size: u32,
    pub grad_accum: u32,
    pub learning_rate: f64,
    pub num_epochs: f64,
    pub cutoff_len: u32,
    /// Optional checkpoint path the trainer resumes from.  Empty string
    /// means "start fresh"; the resume-from-this affordance fills this
    /// from a past run's `checkpoint_path`.
    pub resume_from_checkpoint: String,
}

impl Default for TrainingConfig {
    fn default() -> Self {
        Self {
            base_model: String::new(),
            dataset_name: String::new(),
            finetuning_type: "lora".to_owned(),
            lora_rank: 16,
            lora_alpha: 32,
            lora_dropout: 0.05,
            batch_size: 2,
            grad_accum: 8,
            learning_rate: 2e-4,
            num_epochs: 3.0,
            cutoff_len: 2048,
            resume_from_checkpoint: String::new(),
        }
    }
}

impl TrainingConfig {
    /// Best-effort validation against the form's numeric guardrails.
    /// Returns the first error message, or `Ok(())` if every field is
    /// within range.  The panel uses this to disable the Submit button
    /// before an empty round trip.
    pub fn validate(&self) -> Result<(), String> {
        if self.base_model.trim().is_empty() {
            return Err("Base model is required".into());
        }
        if self.dataset_name.trim().is_empty() {
            return Err("Dataset is required".into());
        }
        if self.lora_rank == 0 || self.lora_rank > 256 {
            return Err("LoRA rank must be between 1 and 256".into());
        }
        if self.lora_alpha == 0 || self.lora_alpha > 512 {
            return Err("LoRA alpha must be between 1 and 512".into());
        }
        if !(0.0..=0.9).contains(&self.lora_dropout) {
            return Err("LoRA dropout must be between 0.0 and 0.9".into());
        }
        if self.batch_size == 0 || self.batch_size > 128 {
            return Err("Batch size must be between 1 and 128".into());
        }
        if self.grad_accum == 0 || self.grad_accum > 64 {
            return Err("Gradient accumulation must be between 1 and 64".into());
        }
        if !(0.0..=1.0).contains(&self.learning_rate) || self.learning_rate == 0.0 {
            return Err("Learning rate must be > 0 and ≤ 1".into());
        }
        if self.num_epochs <= 0.0 || self.num_epochs > 50.0 {
            return Err("Epochs must be > 0 and ≤ 50".into());
        }
        if self.cutoff_len < 64 || self.cutoff_len > 32_768 {
            return Err("Cutoff length must be between 64 and 32768".into());
        }
        Ok(())
    }

    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("base_model".into(), Value::from(self.base_model.clone()));
        m.insert("dataset_name".into(), Value::from(self.dataset_name.clone()));
        m.insert("finetuning_type".into(), Value::from(self.finetuning_type.clone()));
        m.insert("lora_rank".into(), Value::from(self.lora_rank));
        m.insert("lora_alpha".into(), Value::from(self.lora_alpha));
        m.insert("lora_dropout".into(), Value::from(self.lora_dropout));
        m.insert("batch_size".into(), Value::from(self.batch_size));
        m.insert("grad_accum".into(), Value::from(self.grad_accum));
        m.insert("learning_rate".into(), Value::from(self.learning_rate));
        m.insert("num_epochs".into(), Value::from(self.num_epochs));
        m.insert("cutoff_len".into(), Value::from(self.cutoff_len));
        if !self.resume_from_checkpoint.is_empty() {
            m.insert(
                "resume_from_checkpoint".into(),
                Value::from(self.resume_from_checkpoint.clone()),
            );
        }
        Value::Object(m)
    }

    /// Re-hydrate a config from a past run's status payload.  Used by
    /// the resume-from-this affordance to pre-fill the start form.
    /// Missing keys fall back to `default()` values.
    pub fn from_value(v: &Value) -> Self {
        let mut cfg = Self::default();
        if let Some(s) = v.get("base_model").and_then(|x| x.as_str()) {
            cfg.base_model = s.to_owned();
        }
        if let Some(s) = v.get("dataset_name").and_then(|x| x.as_str()) {
            cfg.dataset_name = s.to_owned();
        }
        if let Some(s) = v.get("finetuning_type").and_then(|x| x.as_str()) {
            cfg.finetuning_type = s.to_owned();
        }
        if let Some(n) = v.get("lora_rank").and_then(|x| x.as_u64()) {
            cfg.lora_rank = n as u32;
        }
        if let Some(n) = v.get("lora_alpha").and_then(|x| x.as_u64()) {
            cfg.lora_alpha = n as u32;
        }
        if let Some(n) = v.get("lora_dropout").and_then(|x| x.as_f64()) {
            cfg.lora_dropout = n;
        }
        if let Some(n) = v.get("batch_size").and_then(|x| x.as_u64()) {
            cfg.batch_size = n as u32;
        }
        if let Some(n) = v.get("grad_accum").and_then(|x| x.as_u64()) {
            cfg.grad_accum = n as u32;
        }
        if let Some(n) = v.get("learning_rate").and_then(|x| x.as_f64()) {
            cfg.learning_rate = n;
        }
        if let Some(n) = v.get("num_epochs").and_then(|x| x.as_f64()) {
            cfg.num_epochs = n;
        }
        if let Some(n) = v.get("cutoff_len").and_then(|x| x.as_u64()) {
            cfg.cutoff_len = n as u32;
        }
        if let Some(s) = v.get("checkpoint_path").and_then(|x| x.as_str()) {
            cfg.resume_from_checkpoint = s.to_owned();
        }
        cfg
    }
}

// ── Job row ────────────────────────────────────────────────────────────

/// One row in the active / past runs strips.  Shape mirrors what the
/// Svelte page parsed out of the trainer's `/api/jobs` reply.  Unknown
/// fields are dropped to keep the in-panel projection narrow; loss
/// points come from the per-job `/status` route on expansion.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JobRow {
    pub job_id: String,
    pub status: String,
    /// 0–100.  Sourced from the trainer (it computes step/total) so the
    /// panel doesn't have to back-derive it from epochs.
    pub progress: f64,
    pub config: Value,
    /// Final loss for completed runs; the latest loss for running runs.
    pub final_loss: Option<f64>,
    /// Set when the run failed.  Surfaced in the row as a one-liner.
    pub error: Option<String>,
    /// Optional checkpoint path the trainer wrote during / at the end
    /// of this run.  Drives the resume-from-this button.
    pub checkpoint_path: Option<String>,
    /// Optional ETA in seconds for active runs.
    pub eta_seconds: Option<u64>,
    /// Optional duration in seconds for terminal runs.
    pub duration_seconds: Option<u64>,
    /// Current / final epoch — fractional when the trainer reports
    /// sub-epoch progress.
    pub current_epoch: Option<f64>,
    /// Total epochs the run was launched with.  Pulled from `config`
    /// when present so callers don't have to re-walk the config every
    /// time.
    pub total_epochs: Option<f64>,
}

impl JobRow {
    pub fn from_value(v: &Value) -> Self {
        let config = v.get("config").cloned().unwrap_or(Value::Null);
        let total_epochs = config.get("num_epochs").and_then(|x| x.as_f64());
        Self {
            job_id: v.get("job_id").and_then(|x| x.as_str()).unwrap_or_default().to_owned(),
            status: v
                .get("status")
                .and_then(|x| x.as_str())
                .unwrap_or("unknown")
                .to_owned(),
            progress: v.get("progress").and_then(|x| x.as_f64()).unwrap_or(0.0),
            final_loss: v.get("final_loss").and_then(|x| x.as_f64()),
            error: v
                .get("error")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_owned()),
            checkpoint_path: v
                .get("checkpoint_path")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_owned()),
            eta_seconds: v.get("eta_seconds").and_then(|x| x.as_u64()),
            duration_seconds: v.get("duration_seconds").and_then(|x| x.as_u64()),
            current_epoch: v.get("current_epoch").and_then(|x| x.as_f64()),
            total_epochs,
            config,
        }
    }

    pub fn status_enum(&self) -> JobStatus {
        JobStatus::from_label(&self.status)
    }

    pub fn base_model(&self) -> Option<&str> {
        self.config.get("base_model").and_then(|x| x.as_str())
    }

    pub fn dataset_name(&self) -> Option<&str> {
        self.config.get("dataset_name").and_then(|x| x.as_str())
    }
}

/// Project the `/api/jobs` envelope.  The Svelte page handled both
/// `{jobs: [...]}` and bare arrays — keep both shapes here so the
/// integration is forward-compatible with whichever flavour the trainer
/// pipe ships.
pub fn parse_jobs(v: &Value) -> Vec<JobRow> {
    let arr_opt = v
        .get("jobs")
        .and_then(|x| x.as_array())
        .cloned()
        .or_else(|| v.as_array().cloned());
    let Some(arr) = arr_opt else { return Vec::new(); };
    arr.iter().map(JobRow::from_value).collect()
}

pub async fn list_jobs() -> Result<Vec<JobRow>, String> {
    let v = get("/api/jobs").await?;
    Ok(parse_jobs(&v))
}

// ── Status (loss curve) ────────────────────────────────────────────────

/// `/api/jobs/{id}/status` reply.  Returns the same row shape as
/// `/api/jobs`, plus a `loss_history` array of [`LossPoint`]s.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JobStatus2 {
    pub row: JobRow,
    pub loss_history: Vec<LossPoint>,
}

pub fn parse_status(v: &Value) -> JobStatus2 {
    let row = JobRow::from_value(v);
    let loss_history = v
        .get("loss_history")
        .and_then(|x| x.as_array())
        .map(|arr| arr.iter().map(LossPoint::from_value).collect())
        .unwrap_or_default();
    JobStatus2 { row, loss_history }
}

pub async fn job_status(job_id: &str) -> Result<JobStatus2, String> {
    let path = format!("/api/jobs/{job_id}/status");
    let v = get(&path).await?;
    Ok(parse_status(&v))
}

// ── Start / stop ───────────────────────────────────────────────────────

/// Returned by `/api/start_training`.  The trainer assigns the job id.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StartOutcome {
    pub job_id: String,
}

pub fn parse_start(v: &Value) -> StartOutcome {
    StartOutcome {
        job_id: v.get("job_id").and_then(|x| x.as_str()).unwrap_or_default().to_owned(),
    }
}

pub async fn start_training(cfg: &TrainingConfig) -> Result<StartOutcome, String> {
    cfg.validate()?;
    let body = cfg.to_value();
    let v = post("/api/start_training", body).await?;
    Ok(parse_start(&v))
}

pub async fn stop_training(job_id: &str) -> Result<(), String> {
    if job_id.trim().is_empty() {
        return Err("bad_request: job_id is empty".into());
    }
    let path = format!("/api/jobs/{job_id}/stop");
    let _ = post(&path, Value::Null).await?;
    Ok(())
}

// ── Datasets ───────────────────────────────────────────────────────────

/// Minimal projection of a LLaMA-Factory dataset row.  The trainer's
/// `/api/datasets` reply is richer (sample counts, splits, format) but
/// only the name + a short description are surfaced on the form picker.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DatasetRow {
    pub name: String,
    pub display_name: String,
    pub sample_count: u64,
    pub format: String,
}

impl DatasetRow {
    pub fn from_value(v: &Value) -> Self {
        let name = v.get("name").and_then(|x| x.as_str()).unwrap_or_default().to_owned();
        Self {
            display_name: v
                .get("display_name")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(&name)
                .to_owned(),
            name,
            sample_count: v.get("sample_count").and_then(|x| x.as_u64()).unwrap_or(0),
            format: v.get("format").and_then(|x| x.as_str()).unwrap_or("").to_owned(),
        }
    }
}

pub fn parse_datasets(v: &Value) -> Vec<DatasetRow> {
    let arr_opt = v
        .get("datasets")
        .and_then(|x| x.as_array())
        .cloned()
        .or_else(|| v.as_array().cloned());
    let Some(arr) = arr_opt else { return Vec::new(); };
    arr.iter().map(DatasetRow::from_value).collect()
}

pub async fn list_datasets() -> Result<Vec<DatasetRow>, String> {
    let v = get("/api/datasets").await?;
    Ok(parse_datasets(&v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── JobStatus enum ────────────────────────────────────────────────

    #[test]
    fn job_status_round_trips_known_strings() {
        for (s, want) in [
            ("queued", JobStatus::Queued),
            ("running", JobStatus::Running),
            ("completed", JobStatus::Completed),
            ("failed", JobStatus::Failed),
            ("stopped", JobStatus::Stopped),
        ] {
            assert_eq!(JobStatus::from_label(s), want);
            assert_eq!(want.label(), s);
        }
    }

    #[test]
    fn job_status_unknown_falls_through() {
        assert_eq!(JobStatus::from_label("paused"), JobStatus::Unknown);
        assert_eq!(JobStatus::Unknown.label(), "unknown");
    }

    #[test]
    fn job_status_active_terminal_classification() {
        assert!(JobStatus::Running.is_active());
        assert!(JobStatus::Queued.is_active());
        assert!(!JobStatus::Completed.is_active());
        assert!(JobStatus::Completed.is_terminal());
        assert!(JobStatus::Failed.is_terminal());
        assert!(!JobStatus::Running.is_terminal());
        assert!(!JobStatus::Unknown.is_terminal());
    }

    // ── Config defaults + validation ──────────────────────────────────

    #[test]
    fn training_config_defaults_pass_validation_when_required_filled() {
        let mut cfg = TrainingConfig::default();
        // Defaults are intentionally NOT user-facing valid: base_model
        // + dataset are blank.
        assert!(cfg.validate().is_err());
        cfg.base_model = "meta-llama/Llama-3.2-3B".into();
        cfg.dataset_name = "alpaca".into();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_zero_lora_rank() {
        let mut cfg = TrainingConfig {
            base_model: "m".into(),
            dataset_name: "d".into(),
            ..TrainingConfig::default()
        };
        cfg.lora_rank = 0;
        assert!(cfg.validate().unwrap_err().contains("LoRA rank"));
    }

    #[test]
    fn validate_rejects_oversize_lora_rank() {
        let mut cfg = TrainingConfig {
            base_model: "m".into(),
            dataset_name: "d".into(),
            ..TrainingConfig::default()
        };
        cfg.lora_rank = 500;
        assert!(cfg.validate().unwrap_err().contains("LoRA rank"));
    }

    #[test]
    fn validate_rejects_high_dropout() {
        let mut cfg = TrainingConfig {
            base_model: "m".into(),
            dataset_name: "d".into(),
            ..TrainingConfig::default()
        };
        cfg.lora_dropout = 0.95;
        assert!(cfg.validate().unwrap_err().contains("dropout"));
    }

    #[test]
    fn validate_rejects_zero_learning_rate() {
        let mut cfg = TrainingConfig {
            base_model: "m".into(),
            dataset_name: "d".into(),
            ..TrainingConfig::default()
        };
        cfg.learning_rate = 0.0;
        assert!(cfg.validate().unwrap_err().contains("Learning rate"));
    }

    #[test]
    fn validate_rejects_huge_epochs() {
        let mut cfg = TrainingConfig {
            base_model: "m".into(),
            dataset_name: "d".into(),
            ..TrainingConfig::default()
        };
        cfg.num_epochs = 100.0;
        assert!(cfg.validate().unwrap_err().contains("Epochs"));
    }

    #[test]
    fn validate_rejects_short_cutoff() {
        let mut cfg = TrainingConfig {
            base_model: "m".into(),
            dataset_name: "d".into(),
            ..TrainingConfig::default()
        };
        cfg.cutoff_len = 32;
        assert!(cfg.validate().unwrap_err().contains("Cutoff"));
    }

    #[test]
    fn validate_rejects_empty_required() {
        let cfg = TrainingConfig::default();
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("Base model"));
    }

    // ── Config serialisation ──────────────────────────────────────────

    #[test]
    fn config_to_value_omits_empty_checkpoint() {
        let cfg = TrainingConfig {
            base_model: "m".into(),
            dataset_name: "d".into(),
            ..TrainingConfig::default()
        };
        let v = cfg.to_value();
        assert!(v.get("resume_from_checkpoint").is_none());
    }

    #[test]
    fn config_to_value_includes_checkpoint_when_set() {
        let cfg = TrainingConfig {
            base_model: "m".into(),
            dataset_name: "d".into(),
            resume_from_checkpoint: "/runs/foo/ckpt-42".into(),
            ..TrainingConfig::default()
        };
        let v = cfg.to_value();
        assert_eq!(
            v.get("resume_from_checkpoint").and_then(|x| x.as_str()),
            Some("/runs/foo/ckpt-42"),
        );
    }

    #[test]
    fn config_from_value_round_trips_known_keys() {
        let src = json!({
            "base_model": "mistralai/Mistral-7B",
            "dataset_name": "wylde_sft",
            "finetuning_type": "lora",
            "lora_rank": 32,
            "lora_alpha": 64,
            "lora_dropout": 0.1,
            "batch_size": 4,
            "grad_accum": 4,
            "learning_rate": 1.5e-4,
            "num_epochs": 2.5,
            "cutoff_len": 4096,
            "checkpoint_path": "/runs/wylde_sft/ckpt-1000",
        });
        let cfg = TrainingConfig::from_value(&src);
        assert_eq!(cfg.base_model, "mistralai/Mistral-7B");
        assert_eq!(cfg.dataset_name, "wylde_sft");
        assert_eq!(cfg.lora_rank, 32);
        assert_eq!(cfg.lora_alpha, 64);
        assert!((cfg.lora_dropout - 0.1).abs() < 1e-9);
        assert_eq!(cfg.batch_size, 4);
        assert_eq!(cfg.cutoff_len, 4096);
        assert_eq!(cfg.resume_from_checkpoint, "/runs/wylde_sft/ckpt-1000");
    }

    #[test]
    fn config_from_value_uses_defaults_for_missing_keys() {
        let src = json!({"base_model": "m", "dataset_name": "d"});
        let cfg = TrainingConfig::from_value(&src);
        let def = TrainingConfig::default();
        assert_eq!(cfg.lora_rank, def.lora_rank);
        assert_eq!(cfg.batch_size, def.batch_size);
    }

    // ── Job + status parsing ──────────────────────────────────────────

    #[test]
    fn parse_jobs_accepts_envelope_or_bare_array() {
        let envelope = json!({"jobs": [
            {"job_id": "a", "status": "running", "progress": 42.0,
             "config": {"base_model": "m", "dataset_name": "d", "num_epochs": 3.0}}
        ]});
        let bare = json!([
            {"job_id": "b", "status": "completed", "final_loss": 0.85,
             "config": {"base_model": "m2", "dataset_name": "d2"}}
        ]);
        let env_jobs = parse_jobs(&envelope);
        let bare_jobs = parse_jobs(&bare);
        assert_eq!(env_jobs.len(), 1);
        assert_eq!(env_jobs[0].job_id, "a");
        assert_eq!(env_jobs[0].status_enum(), JobStatus::Running);
        assert!((env_jobs[0].progress - 42.0).abs() < 1e-9);
        assert_eq!(env_jobs[0].total_epochs, Some(3.0));
        assert_eq!(env_jobs[0].base_model(), Some("m"));
        assert_eq!(env_jobs[0].dataset_name(), Some("d"));
        assert_eq!(bare_jobs.len(), 1);
        assert_eq!(bare_jobs[0].status_enum(), JobStatus::Completed);
        assert_eq!(bare_jobs[0].final_loss, Some(0.85));
    }

    #[test]
    fn parse_jobs_handles_empty() {
        assert!(parse_jobs(&json!({})).is_empty());
        assert!(parse_jobs(&json!({"jobs": []})).is_empty());
        assert!(parse_jobs(&json!([])).is_empty());
    }

    #[test]
    fn job_row_picks_known_optional_fields() {
        let v = json!({
            "job_id": "x",
            "status": "failed",
            "error": "OOM at step 1500",
            "checkpoint_path": "/runs/x/ckpt-1500",
            "duration_seconds": 3600,
            "current_epoch": 1.5,
        });
        let row = JobRow::from_value(&v);
        assert_eq!(row.status_enum(), JobStatus::Failed);
        assert_eq!(row.error.as_deref(), Some("OOM at step 1500"));
        assert_eq!(row.checkpoint_path.as_deref(), Some("/runs/x/ckpt-1500"));
        assert_eq!(row.duration_seconds, Some(3600));
        assert_eq!(row.current_epoch, Some(1.5));
    }

    #[test]
    fn job_row_drops_empty_error_string() {
        let v = json!({"job_id": "x", "status": "completed", "error": ""});
        let row = JobRow::from_value(&v);
        assert!(row.error.is_none());
    }

    #[test]
    fn parse_status_collects_loss_history() {
        let v = json!({
            "job_id": "z",
            "status": "running",
            "progress": 50.0,
            "loss_history": [
                {"step": 10, "loss": 2.5, "epoch": 0.1, "lr": 2e-4},
                {"step": 20, "loss": 1.8, "epoch": 0.2, "lr": 1.95e-4},
                {"step": 30, "loss": 1.5, "epoch": 0.3, "lr": 1.9e-4},
            ],
        });
        let st = parse_status(&v);
        assert_eq!(st.row.status_enum(), JobStatus::Running);
        assert_eq!(st.loss_history.len(), 3);
        assert_eq!(st.loss_history[1].step, 20);
        assert!((st.loss_history[2].loss - 1.5).abs() < 1e-9);
    }

    #[test]
    fn parse_status_no_history_returns_empty() {
        let v = json!({"job_id": "z", "status": "queued"});
        let st = parse_status(&v);
        assert!(st.loss_history.is_empty());
    }

    // ── Start outcome ─────────────────────────────────────────────────

    #[test]
    fn parse_start_picks_job_id() {
        let v = json!({"job_id": "deadbeef", "status": "queued"});
        assert_eq!(parse_start(&v).job_id, "deadbeef");
    }

    #[test]
    fn parse_start_handles_missing_id() {
        let v = json!({"status": "queued"});
        assert!(parse_start(&v).job_id.is_empty());
    }

    #[tokio::test]
    async fn start_training_rejects_invalid_config() {
        let cfg = TrainingConfig::default();
        let err = start_training(&cfg).await.unwrap_err();
        assert!(err.contains("Base model"));
    }

    #[tokio::test]
    async fn stop_training_rejects_empty_id() {
        let err = stop_training("   ").await.unwrap_err();
        assert!(err.contains("bad_request"));
    }

    // ── Datasets ──────────────────────────────────────────────────────

    #[test]
    fn parse_datasets_envelope_and_bare_array() {
        let envelope = json!({"datasets": [
            {"name": "alpaca", "display_name": "Alpaca", "sample_count": 50000, "format": "instruction"},
            {"name": "wylde_sft"},
        ]});
        let rows = parse_datasets(&envelope);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "alpaca");
        assert_eq!(rows[0].display_name, "Alpaca");
        assert_eq!(rows[0].sample_count, 50_000);
        // display_name falls back to name when blank/missing.
        assert_eq!(rows[1].display_name, "wylde_sft");

        let bare = json!([{"name": "x"}]);
        assert_eq!(parse_datasets(&bare).len(), 1);
    }

    #[test]
    fn parse_datasets_empty_envelope() {
        assert!(parse_datasets(&json!({})).is_empty());
    }
}

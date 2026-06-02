//! wylde-voice service entry point.
//!
//! Boots the manifest, registers the action surface (Slice 11.A: three
//! actions), opens the pipe at `\\.\pipe\wylde-voice`, and serves until
//! Ctrl-C. Same shape as `wylde-trainer/main.rs` and
//! `wylde-ollama/main.rs` — the Wylde user's standing pattern.
//!
//! Phase 11a foundation slice — see `crate` docstring for the
//! full slice plan.

use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use tracing::Level;
use wylde_shared::ipc;
use wylde_shared::logging::configure_logging;
use wylde_shared::manifest::ManifestWriter;

const SERVICE_NAME: &str = "wylde-voice";

#[tokio::main]
async fn main() -> Result<()> {
    configure_logging(Some(SERVICE_NAME), Level::INFO);
    tracing::info!("wylde-voice: starting (rust impl, slice 11.D)");

    let cfg = wylde_voice::config::Config::get();
    let manifest = ManifestWriter::write(
        SERVICE_NAME,
        None,
        "core",
        "Voice service — STT/TTS/wake-word primitives over \\\\.\\pipe\\wylde-voice. \
         Slice 11.A+/B/C/D: health, list_models, transcribe (full Whisper pipeline), \
         synthesize (Kokoro phoneme-path), streaming variants, cpal mic capture, \
         openWakeWord listener.",
        json!({
            "wylde_voice": {
                "actions": wylde_voice::service::all_actions(),
                "slice": "11.D",
                "stt_backend": cfg.stt_backend.as_str(),
                "stt_model": cfg.stt_model.clone(),
                "tts_voice": cfg.tts_voice.clone(),
                "ov_device": cfg.ov_device.clone(),
                "ov_cache_dir": cfg.ov_cache_dir.display().to_string(),
                "wakeword_model": cfg.wakeword_model.clone(),
                "wakeword_models_dir": cfg.wakeword_models_dir.display().to_string(),
            },
            "dashboard": {
                "label": "Voice",
                "icon": "mic",
                "color": "purple"
            },
        }),
        Some("rust:wylde-voice"),
    )?;
    let _heartbeat = manifest.start_heartbeat(Duration::from_secs(60));

    // Register actions on the process-wide registry. install() must
    // precede serve() so the registry is populated when the first
    // pipe client connects.
    wylde_voice::service::install();

    // ⚠️  Slice 11.A: action contract auto-write is INTENTIONALLY SKIPPED.
    //
    // Unlike every other Rust service in the mesh, wylde-voice's Rust
    // action surface (voice.health / voice.list_models / voice.transcribe
    // — lower-level primitives) does NOT match its Python predecessor
    // (voice.toggle / voice.start_session / etc — orchestration). The
    // contract file at `data/contracts/actions/wylde-voice.json` is
    // shared single-source state that wylde_check + the GUI's
    // gui_action_contract lint both read.
    //
    // While the strangler-fig is in flight (Slice 11.A through 11.E),
    // the Python contract MUST stay authoritative so the GUI's calls
    // to voice.toggle / voice.get_mode / etc. don't trigger the
    // gui_action_contract lint. When the harness migration ships in
    // Slice 11.E and the GUI is updated to call the new primitives,
    // re-enable `ipc::write_action_contract(SERVICE_NAME, &cfg.wylde_root)`.
    //
    // The Rust crate's own action surface is documented in
    // `wylde_voice::service::ALL_ACTIONS` + this main's manifest
    // `contributes.wylde_voice.actions`.

    // Slice 4 — opt-in first-run model bootstrap. When
    // `WYLDE_VOICE_AUTO_DOWNLOAD=1`, kick the Rust-native fetch of the
    // Whisper + Kokoro model files in the background so a clean install
    // self-provisions without `Voice/download_models.py`. Off by default
    // so boot stays fast + offline-friendly; the GUI / harness can also
    // trigger it on demand via the `voice.download_models` action.
    if env_flag("WYLDE_VOICE_AUTO_DOWNLOAD") {
        let job = wylde_voice::model_download::spawn_ensure_job();
        tracing::info!(
            "wylde-voice: auto model download started (job {job}); \
             poll voice.download_status for progress"
        );
    }

    tracing::info!(
        "wylde-voice: actions registered; opening pipe at \\\\.\\pipe\\wylde-voice"
    );

    let serve_fut = ipc::serve(SERVICE_NAME, None);
    tokio::select! {
        result = serve_fut => {
            if let Err(e) = result {
                tracing::error!("wylde-voice: serve() exited with error: {e}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("wylde-voice: ctrl-c received, shutting down");
        }
    }

    wylde_voice::service::stop();
    if let Err(e) = manifest.mark_stopped() {
        tracing::warn!("wylde-voice: mark_stopped failed: {e}");
    }
    Ok(())
}

/// Truthy env-var check (`1` / `true`, case-insensitive).
fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

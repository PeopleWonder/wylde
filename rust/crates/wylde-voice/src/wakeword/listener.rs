//! Background wake-word listener.
//!
//! Owns one [`crate::mic::MicCapture`] handle, a
//! [`super::pipeline::WakeWordPipeline`], and a worker thread that
//! pulls 1280-sample frames off the mic broadcast, runs the 3-stage
//! inference pipeline, and fans detection events out to subscribers.
//!
//! ## Cooldown
//!
//! Per-listener monotonic timestamp tracks the last detection. Frames
//! during the cooldown window still run inference (so the rolling
//! buffer stays warm) but score-over-threshold events are suppressed.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

use crate::mic::{MicCapture, WAKEWORD_FRAME_SAMPLES};
use crate::wakeword::pipeline::{WakeWordInferError, WakeWordPipeline};

/// A single detection event. Multiple subscribers receive their own
/// copy via the listener's broadcast channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WakeWordEvent {
    /// Monotonic milliseconds since the listener started — gives the
    /// orchestrator an ordering anchor without leaking wall-clock.
    pub elapsed_ms: u64,
    /// Classifier output for the detecting frame.
    pub score: f32,
    /// Threshold that triggered the event — useful for log-only modes
    /// where a downstream re-decides at a stricter bar.
    pub threshold: f32,
    /// Model name — same string Python's `state.config.wake_word_model`
    /// reports back to the GUI. Stored on the listener so callers know
    /// which wake-word fired.
    pub model: String,
}

/// Broadcast capacity for detection events. Wake-word events are slow
/// (≤ a few per minute even with chatter); 16 is plenty.
const EVENT_BUFFER: usize = 16;

pub struct WakeWordListener {
    events: broadcast::Sender<WakeWordEvent>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    _mic: Arc<MicCapture>,
    model_name: String,
    threshold: f32,
    cooldown_ms: u64,
}

impl WakeWordListener {
    /// Wire `mic` + `pipeline` together on a background thread. The
    /// listener holds an `Arc<MicCapture>` so the cpal stream survives
    /// for as long as the listener does, even if every other holder
    /// drops out (voice.mic.stop while wake-word is running).
    pub fn start(
        mic: Arc<MicCapture>,
        pipeline: Arc<WakeWordPipeline>,
        model_name: String,
    ) -> std::io::Result<Self> {
        let threshold = pipeline.threshold();
        let cooldown_ms = pipeline.cooldown_ms();
        let (events_tx, _) = broadcast::channel::<WakeWordEvent>(EVENT_BUFFER);
        let stop = Arc::new(AtomicBool::new(false));

        let mut chunks_rx = mic.subscribe();
        let events_tx_for_worker = events_tx.clone();
        let stop_for_worker = Arc::clone(&stop);
        let model_for_worker = model_name.clone();

        let worker = thread::Builder::new()
            .name("wylde-voice-wakeword".to_owned())
            .spawn(move || {
                run_listener_thread(
                    pipeline,
                    &mut chunks_rx,
                    events_tx_for_worker,
                    stop_for_worker,
                    cooldown_ms,
                    model_for_worker,
                );
            })?;

        Ok(Self {
            events: events_tx,
            stop,
            worker: Some(worker),
            _mic: mic,
            model_name,
            threshold,
            cooldown_ms,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<WakeWordEvent> {
        self.events.subscribe()
    }

    pub fn model_name(&self) -> &str {
        &self.model_name
    }

    pub fn threshold(&self) -> f32 {
        self.threshold
    }

    pub fn cooldown_ms(&self) -> u64 {
        self.cooldown_ms
    }

    /// Stop the listener — signal the worker thread, join it, and
    /// release the cpal capture. Idempotent.
    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.worker.take() {
            if let Err(e) = handle.join() {
                tracing::warn!("wylde-voice: wake-word worker join panicked: {e:?}");
            }
        }
        // `_mic: Arc<MicCapture>` drops here in the surrounding Drop
        // impl; the cpal stream tears down when the final Arc reference
        // dies.
    }
}

impl Drop for WakeWordListener {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

fn run_listener_thread(
    pipeline: Arc<WakeWordPipeline>,
    chunks_rx: &mut broadcast::Receiver<Arc<Vec<i16>>>,
    events_tx: broadcast::Sender<WakeWordEvent>,
    stop: Arc<AtomicBool>,
    cooldown_ms: u64,
    model_name: String,
) {
    let started_at = Instant::now();
    let mut last_fire: Option<Instant> = None;
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("wylde-voice: failed to build wake-word recv runtime: {e}");
            return;
        }
    };

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        // The receiver blocks an entire task on `recv().await`, so we
        // wrap it in a short `select!` against a sleep so we can check
        // the stop flag periodically.
        let next = runtime.block_on(async {
            tokio::select! {
                msg = chunks_rx.recv() => Some(msg),
                _ = tokio::time::sleep(Duration::from_millis(100)) => None,
            }
        });
        let chunk = match next {
            Some(Ok(c)) => c,
            Some(Err(RecvError::Lagged(skipped))) => {
                tracing::warn!(
                    "wylde-voice: wake-word listener lagged, skipped {skipped} chunks"
                );
                continue;
            }
            Some(Err(RecvError::Closed)) => {
                tracing::info!("wylde-voice: mic broadcast closed, stopping wake-word listener");
                break;
            }
            None => continue,
        };
        if chunk.len() != WAKEWORD_FRAME_SAMPLES {
            tracing::warn!(
                "wylde-voice: wake-word listener got odd-sized chunk ({} samples) — skipping",
                chunk.len()
            );
            continue;
        }

        let score = match pipeline.process_frame(&chunk) {
            Ok(Some(s)) => s,
            Ok(None) => continue, // warm-up
            Err(WakeWordInferError::WrongFrameSize(n)) => {
                tracing::warn!("wylde-voice: wake-word frame size mismatch ({n}) — skipping");
                continue;
            }
            Err(e) => {
                tracing::error!("wylde-voice: wake-word inference failed: {e}");
                continue;
            }
        };

        if score < pipeline.threshold() {
            continue;
        }
        if let Some(last) = last_fire {
            if last.elapsed() < Duration::from_millis(cooldown_ms) {
                continue;
            }
        }
        last_fire = Some(Instant::now());
        let event = WakeWordEvent {
            elapsed_ms: started_at.elapsed().as_millis() as u64,
            score,
            threshold: pipeline.threshold(),
            model: model_name.clone(),
        };
        if events_tx.send(event).is_err() {
            // No subscribers right now — that's fine, drop and continue.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_is_serialisable() {
        let e = WakeWordEvent {
            elapsed_ms: 1_234,
            score: 0.875,
            threshold: 0.5,
            model: "openWakeWord/hey-jarvis".to_owned(),
        };
        let json = serde_json::to_value(&e).expect("serialise");
        assert_eq!(json["elapsed_ms"], 1_234);
        // f32→f64 widening is exact for 0.875 (binary representable),
        // so the JSON Number compares cleanly without an epsilon.
        assert_eq!(json["score"], 0.875);
        assert_eq!(json["threshold"], 0.5);
        assert_eq!(json["model"], "openWakeWord/hey-jarvis");
    }
}

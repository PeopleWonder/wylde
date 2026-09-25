//! The FIM lane: latest request wins, per device.
//!
//! Autocomplete sends a new FIM request on almost every keystroke, and
//! only the newest one matters. [`FimLane::begin`] bumps the device's
//! generation, which supersedes every older in-flight FIM request from
//! that device; each request watches its [`FimTicket`] and stops as soon
//! as it is superseded. Stopping drops the upstream stream, which cancels
//! generation in `wylde-ollama` (with `evict_on_cancel: false`, so the model
//! stays loaded). Fast typing therefore never builds a queue in front of an
//! agent turn. Priority over chat comes from `wylde-ollama`'s FIM lease
//! priority (`fim: true`).

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::watch;

/// Per-device FIM generations.
#[derive(Default)]
pub struct FimLane {
    devices: Mutex<HashMap<String, watch::Sender<u64>>>,
}

/// One FIM request's claim on its device's lane.
pub struct FimTicket {
    generation: u64,
    rx: watch::Receiver<u64>,
}

impl FimLane {
    /// Start a FIM request for `device_id`, superseding its older ones.
    pub fn begin(&self, device_id: &str) -> FimTicket {
        let mut devices = self.devices.lock().expect("fim lane poisoned");
        let tx = devices
            .entry(device_id.to_owned())
            .or_insert_with(|| watch::channel(0).0);
        let generation = *tx.borrow() + 1;
        tx.send_replace(generation);
        FimTicket {
            generation,
            rx: tx.subscribe(),
        }
    }
}

impl FimTicket {
    /// Whether a newer request from the same device has started.
    pub fn is_superseded(&self) -> bool {
        *self.rx.borrow() != self.generation
    }

    /// Resolves once a newer request from the same device has started.
    pub async fn superseded(&mut self) {
        loop {
            if self.is_superseded() {
                return;
            }
            if self.rx.changed().await.is_err() {
                // The lane was dropped; nothing can supersede us now.
                std::future::pending::<()>().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn a_newer_request_supersedes_older_ones_on_the_same_device_only() {
        let lane = FimLane::default();
        let mut first = lane.begin("dev-a");
        let other = lane.begin("dev-b");
        assert!(!first.is_superseded());
        let second = lane.begin("dev-a");
        assert!(first.is_superseded());
        assert!(!second.is_superseded());
        assert!(!other.is_superseded(), "devices are independent");
        tokio::time::timeout(Duration::from_secs(1), first.superseded())
            .await
            .expect("superseded() resolves");
    }

    #[tokio::test]
    async fn superseded_waits_until_a_newer_request_arrives() {
        let lane = std::sync::Arc::new(FimLane::default());
        let mut ticket = lane.begin("dev");
        let pending = tokio::time::timeout(Duration::from_millis(50), ticket.superseded()).await;
        assert!(pending.is_err(), "no newer request yet");
        let l = lane.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _newer = l.begin("dev");
        });
        tokio::time::timeout(Duration::from_secs(1), ticket.superseded())
            .await
            .expect("woken by the newer request");
    }
}

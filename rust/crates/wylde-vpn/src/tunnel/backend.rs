//! Trait-shaped backend abstraction.
//!
//! The tunnel manager talks to a [`Backend`] for every side-effecting
//! call (create wintun adapter, configure peer, …). Two implementations:
//!
//! * [`RealBackend`] — production impl. Thin glue over [`super::datapath`].
//! * [`StubBackend`] — records every call to a `Vec<Op>` so unit tests
//!   can verify the manager's lifecycle without touching the OS. Used
//!   throughout [`super::state::tests`].

use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::datapath::TunnelParams;

/// Tunnel-lifecycle operations the manager performs. Trait-shaped so
/// tests can substitute a stub.
///
/// Methods are sync because every implementation needs to acquire OS
/// resources (a wintun adapter handle, etc.). The manager itself runs
/// in async contexts but calls into the backend via `spawn_blocking`
/// where it matters.
pub trait Backend: Send + Sync + 'static {
    /// Create the platform TUN device and start the boringtun-driven
    /// I/O workers. Returns a [`SessionHandle`] the manager keeps for
    /// the lifetime of the tunnel.
    fn start_tunnel(&self, params: TunnelParams) -> Result<SessionHandle>;

    /// Tear down a previously-started tunnel. Best-effort: errors are
    /// logged but do not block teardown — a half-dead tunnel still
    /// needs the wintun adapter unloaded.
    fn stop_tunnel(&self, session: SessionHandle) -> Result<()>;
}

/// Opaque handle to a started tunnel. Owned by the manager between
/// `start_tunnel` and `stop_tunnel`. The Real impl wraps platform
/// resources (wintun Adapter + Session, JoinHandles, shutdown Notify);
/// the Stub impl just carries an id.
pub struct SessionHandle {
    pub id: u64,
    /// Opaque payload — real backend stuffs an [`super::datapath::RunningTunnel`]
    /// here, stub backend leaves it `None`.
    pub inner: Option<Box<dyn std::any::Any + Send>>,
}

impl SessionHandle {
    pub fn new_with(id: u64, inner: Box<dyn std::any::Any + Send>) -> Self {
        Self {
            id,
            inner: Some(inner),
        }
    }

    pub fn empty(id: u64) -> Self {
        Self { id, inner: None }
    }
}

// ── Real backend ─────────────────────────────────────────────────────

pub struct RealBackend {
    next_id: parking_lot::Mutex<u64>,
}

impl RealBackend {
    pub fn new() -> Self {
        Self {
            next_id: parking_lot::Mutex::new(1),
        }
    }
}

impl Default for RealBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for RealBackend {
    fn start_tunnel(&self, params: TunnelParams) -> Result<SessionHandle> {
        let id = {
            let mut g = self.next_id.lock();
            let v = *g;
            *g += 1;
            v
        };
        let running = super::datapath::start(params)?;
        Ok(SessionHandle::new_with(id, Box::new(running)))
    }

    fn stop_tunnel(&self, session: SessionHandle) -> Result<()> {
        if let Some(inner) = session.inner {
            if let Ok(running) = inner.downcast::<super::datapath::RunningTunnel>() {
                super::datapath::stop(*running)?;
            }
        }
        Ok(())
    }
}

// ── Stub backend (unit tests) ────────────────────────────────────────

/// Operations the stub backend records. Used by unit tests to verify
/// the manager calls the backend in the right order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    StartTunnel {
        iface: String,
        endpoint: String,
        tunnel_addr: String,
    },
    StopTunnel {
        id: u64,
    },
}

pub struct StubBackend {
    pub ops: Arc<Mutex<Vec<Op>>>,
    pub fail_start: Arc<parking_lot::Mutex<Option<String>>>,
    next_id: parking_lot::Mutex<u64>,
}

impl StubBackend {
    pub fn new() -> Self {
        Self {
            ops: Arc::new(Mutex::new(Vec::new())),
            fail_start: Arc::new(parking_lot::Mutex::new(None)),
            next_id: parking_lot::Mutex::new(1),
        }
    }

    pub fn ops(&self) -> Vec<Op> {
        self.ops.lock().expect("stub ops lock").clone()
    }

    /// Test hook — set this to make the next `start_tunnel` fail. The
    /// flag is consumed (one-shot) so subsequent calls succeed.
    pub fn arm_start_failure(&self, msg: &str) {
        *self.fail_start.lock() = Some(msg.to_string());
    }
}

impl Default for StubBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for StubBackend {
    fn start_tunnel(&self, params: TunnelParams) -> Result<SessionHandle> {
        if let Some(msg) = self.fail_start.lock().take() {
            return Err(anyhow::anyhow!(msg));
        }
        let id = {
            let mut g = self.next_id.lock();
            let v = *g;
            *g += 1;
            v
        };
        self.ops
            .lock()
            .expect("stub ops lock")
            .push(Op::StartTunnel {
                iface: params.iface_name.clone(),
                endpoint: params.endpoint.clone(),
                tunnel_addr: params.tunnel_addr.clone(),
            });
        Ok(SessionHandle::empty(id))
    }

    fn stop_tunnel(&self, session: SessionHandle) -> Result<()> {
        self.ops
            .lock()
            .expect("stub ops lock")
            .push(Op::StopTunnel { id: session.id });
        Ok(())
    }
}

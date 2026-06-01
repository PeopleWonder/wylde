//! Spawning a service implementation as a child process.
//!
//! Both implementations are launched exactly the way the Lifecycle daemon
//! launches them in production:
//!
//! - Python: `<.venv>/Scripts/python.exe -m <module>` with cwd = repo root.
//! - Rust:   `<repo>/rust/target/release/<name>.exe` with cwd = repo root.
//!
//! cwd matters: the Python entry points resolve `Core.*` / `Gateway.*`
//! imports relative to the repo root, and both implementations write their
//! manifest under `<repo>/data/manifests`.

use std::process::{Child, Command, Stdio};

use crate::paths;

/// A running service process. Killed (and reaped) on drop so a panicking
/// test never leaks a child holding a port or a pipe.
pub struct Service {
    label: String,
    child: Child,
}

impl Service {
    /// Spawn `cmd`, tagging the handle with `label` for diagnostics.
    pub fn spawn(label: &str, mut cmd: Command) -> std::io::Result<Self> {
        let child = cmd.spawn()?;
        Ok(Self {
            label: label.to_string(),
            child,
        })
    }

    /// Human-readable label, e.g. `"python vram-broker"`.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Has the process already exited on its own? A service that dies
    /// during startup (bad import, port in use) should surface as a clear
    /// failure rather than a readiness timeout.
    pub fn has_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        // kill() is TerminateProcess on Windows: abrupt, but the services
        // hold only in-memory state (broker leases) or rewrite their
        // manifest on next boot, so an ungraceful stop is safe here.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Build a `python -m <module>` command on the repo's virtualenv
/// interpreter. stdout/stderr are discarded — the harness asserts on
/// observable request/response behaviour, not on logs.
pub fn python_module(module: &str) -> Command {
    let mut cmd = Command::new(paths::venv_python());
    cmd.arg("-m")
        .arg(module)
        .current_dir(paths::repo_root())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

/// Build a command to run a release Rust service binary.
pub fn rust_binary(name: &str) -> Command {
    let mut cmd = Command::new(paths::rust_release_bin(name));
    cmd.current_dir(paths::repo_root())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

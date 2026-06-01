//! Idempotent root-logger setup for Wylde Rust services.
//!
//! Mirrors `Core/shared/logging_setup.configure_logging` on the Python side:
//! one entry point, callable from anywhere, safe to call twice. The first
//! call installs a `tracing` formatter whose output matches the Python
//! format
//!
//! ```text
//! %(asctime)s [service] %(levelname)s %(name)s: %(message)s
//! ```
//!
//! so merged subprocess log output stays readable across the boundary.
//! Subsequent calls are no-ops. Noisy upstream targets (`hyper`, `h2`,
//! `tokio_util`) are clamped to WARN — the Rust equivalent of Python's
//! `urllib3` / `requests` quieting.

use std::fmt;
use std::sync::OnceLock;

use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::{
    format::Writer, time::FormatTime, FmtContext, FormatEvent, FormatFields,
};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::EnvFilter;

static CONFIGURED: OnceLock<()> = OnceLock::new();

struct WyldeTime;

impl FormatTime for WyldeTime {
    fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
        write!(
            w,
            "{}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f")
        )
    }
}

struct WyldeFormat {
    service: Option<String>,
}

impl<S, N> FormatEvent<S, N> for WyldeFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        WyldeTime.format_time(&mut writer)?;
        writer.write_char(' ')?;
        if let Some(svc) = self.service.as_deref() {
            write!(writer, "[{}] ", svc)?;
        }
        let meta = event.metadata();
        write!(writer, "{} {}: ", meta.level(), meta.target())?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

/// Configure the root tracing subscriber. The first call wins; later calls
/// are silent no-ops, matching the Python behaviour.
pub fn configure_logging(service: Option<&str>, level: Level) {
    if CONFIGURED.set(()).is_err() {
        // Already configured — still attest the phase so the manifest
        // records the call (mirrors the Python re-entrant path).
        crate::manifest::attest_phase("configure_logging");
        return;
    }
    let default = format!("{}", level).to_lowercase();
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&default))
        .add_directive("hyper=warn".parse().expect("static directive"))
        .add_directive("h2=warn".parse().expect("static directive"))
        .add_directive("tokio_util=warn".parse().expect("static directive"));
    let formatter = WyldeFormat {
        service: service.map(str::to_owned),
    };
    let _ = tracing_subscriber::fmt()
        .event_format(formatter)
        .with_env_filter(filter)
        .try_init();
    crate::manifest::attest_phase("configure_logging");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idempotent() {
        configure_logging(Some("test"), Level::INFO);
        configure_logging(Some("test"), Level::DEBUG);
    }
}

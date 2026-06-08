//! Failure-mode tests for the shared workspaces client (scope v2 §7.1).
//!
//! These exercise the resilience pipeline at the client surface without a
//! live service: a pre-tripped breaker fails fast, and a call to a
//! guaranteed-missing pipe surfaces a transport error of the right tier and
//! records a breaker failure. Exhaustive coverage of the individual
//! primitives (breaker state machine, retry backoff, timeout budgets, cache
//! TTL, error classification) lives in each module's own unit tests.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use wylde_workspaces_client::circuit_breaker::{BreakerDecision, CircuitBreaker};
use wylde_workspaces_client::error::ErrorTier;
use wylde_workspaces_client::WorkspacesClient;

/// A pipe name that is overwhelmingly unlikely to exist.
fn missing_service_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("wylde-workspaces-missing-{}-{}", std::process::id(), nanos)
}

#[tokio::test]
async fn breaker_open_fails_fast() {
    // Pre-trip the breaker for `ping` so the call never touches the pipe.
    let breaker = CircuitBreaker::with_params(5, Duration::from_secs(30));
    for _ in 0..5 {
        breaker.record_failure("ping");
    }
    assert_eq!(breaker.check("ping"), BreakerDecision::Open);

    let client = WorkspacesClient::with_breaker(missing_service_name(), breaker);

    let started = Instant::now();
    let err = client
        .ping()
        .await
        .expect_err("breaker is open → must fail");
    let elapsed = started.elapsed();

    assert_eq!(err.code, "breaker_open");
    assert_eq!(err.tier, ErrorTier::Degraded);
    assert!(!err.transport, "breaker-open is not a transport failure");
    // Fast: no 2s connect attempt happened.
    assert!(
        elapsed < Duration::from_millis(250),
        "breaker-open path took {elapsed:?}, expected fail-fast"
    );
}

#[tokio::test]
async fn missing_pipe_surfaces_transport_error() {
    // No service is listening on this pipe → the transport fails to connect.
    let client = WorkspacesClient::new(PathBuf::from(format!(
        r"\\.\pipe\{}",
        missing_service_name()
    )));

    let err = client
        .ping()
        .await
        .expect_err("no service → ping must fail");

    // Connect failures classify as Broken (service unreachable); a race that
    // surfaces a timeout instead would be Transient. Either is transport.
    assert!(
        matches!(err.tier, ErrorTier::Broken | ErrorTier::Transient),
        "unexpected tier: {:?} ({})",
        err.tier,
        err.code
    );
    assert!(err.transport, "a connect failure is a transport failure");

    // One exhausted operation → exactly one breaker failure recorded; below
    // the 5-failure threshold, so the breaker is still closed.
    assert_eq!(client.breaker().check("ping"), BreakerDecision::Closed);
}

#[tokio::test]
async fn unknown_verb_is_a_client_error() {
    let client = WorkspacesClient::for_service(missing_service_name());
    let err = client
        .call_verb("workspaces.not_in_table", serde_json::Value::Null, 1)
        .await
        .expect_err("verb not in the table → client error");
    assert_eq!(err.code, "unknown_verb");
    assert!(!err.transport);
}

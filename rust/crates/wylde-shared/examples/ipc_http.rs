//! Dev-only IPC client for the *HTTP-shaped* surface — send one
//! `<verb> <path>` request over a service's pipe and print the reply as
//! JSON. Sibling of `ipc_call.rs` (which speaks the `/__action__` verb
//! surface); this one exercises the [`HttpRouteTable`] dispatch added to
//! the shared pipe server.
//!
//! Lives in `wylde-shared` (a lib crate with no prebuild guard) so it
//! builds while the service stack is up.
//!
//! Usage:
//!   ipc_http <service> <verb> <path> [json-body | @path-to-json-file]
//!
//! Examples:
//!   ipc_http wylde-vpn GET /api/link/status
//!   ipc_http wylde-vpn GET /api/link/services
//!
//! Exit code: 0 on an ok reply, 1 on a service-level error, 2 on a
//! usage / payload-parse error.

use wylde_shared::ipc::send_with_verb;

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(service), Some(verb), Some(path)) = (args.next(), args.next(), args.next()) else {
        usage();
        std::process::exit(2);
    };
    let raw = args.next().unwrap_or_else(|| "null".to_string());
    let body_str = if let Some(p) = raw.strip_prefix('@') {
        match std::fs::read_to_string(p) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("could not read body file {p:?}: {e}");
                std::process::exit(2);
            }
        }
    } else {
        raw
    };
    let body: serde_json::Value = match serde_json::from_str(&body_str) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("body is not valid JSON: {e}");
            std::process::exit(2);
        }
    };

    // `send_with_verb` builds the exact envelope the GUI panels send
    // (`method = path`, `http_verb`).
    let reply = send_with_verb(
        &service,
        &path,
        &verb,
        body,
        std::time::Duration::from_secs(30),
    )
    .await;
    let out = serde_json::json!({
        "ok": reply.ok,
        "data": reply.data,
        "error": reply.error.as_ref().map(|e| serde_json::json!({
            "code": e.code,
            "message": e.message,
        })),
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&out).unwrap_or_else(|_| "{}".to_string())
    );
    if !reply.ok {
        std::process::exit(1);
    }
}

fn usage() {
    eprintln!("usage: ipc_http <service> <verb> <path> [json-body | @file]");
}

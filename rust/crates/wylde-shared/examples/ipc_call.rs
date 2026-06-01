//! Dev-only IPC client — send one pipe-action to a running Wylde
//! service and print the reply as JSON. Lives in `wylde-shared`
//! (a lib crate with no service binary) so it builds while the stack
//! is up — unlike the `wylde-*` service crates, whose prebuild guard
//! refuses to link over a running `.exe`.
//!
//! Usage:
//!   ipc_call <service> <action> [json-payload | @path-to-json-file]
//!
//! A payload that starts with `@` is read from the named file — the
//! robust way to pass JSON on Windows PowerShell, which otherwise
//! mangles embedded quotes and spaces in a native-command argument.
//!
//! Examples:
//!   ipc_call lifecycle service.shutdown_all
//!   ipc_call harness chat.run_turn @C:\tmp\turn.json
//!
//! Exit code is 0 on an ok reply, 1 on a service-level error, 2 on a
//! usage / payload-parse error.

use wylde_shared::ipc::send_action;

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let Some(service) = args.next() else {
        usage();
        std::process::exit(2);
    };
    let Some(action) = args.next() else {
        usage();
        std::process::exit(2);
    };
    let raw = args.next().unwrap_or_else(|| "{}".to_string());
    let payload_str = if let Some(path) = raw.strip_prefix('@') {
        match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("could not read payload file {path:?}: {e}");
                std::process::exit(2);
            }
        }
    } else {
        raw
    };
    let payload: serde_json::Value = match serde_json::from_str(&payload_str) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("payload is not valid JSON: {e}");
            std::process::exit(2);
        }
    };

    let reply = send_action(&service, &action, payload).await;
    let out = serde_json::json!({
        "ok": reply.ok,
        "data": reply.data,
        "error": reply.error.as_ref().map(|e| serde_json::json!({
            "code": e.code,
            "message": e.message,
            "details": e.details,
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
    eprintln!("usage: ipc_call <service> <action> [json-payload]");
}

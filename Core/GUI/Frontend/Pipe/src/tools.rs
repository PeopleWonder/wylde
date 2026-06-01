//! `tools.*` in-process dispatch.  Mirror of
//! `Core/GUI/src-tauri/src/pipe/tools.rs`.

use serde_json::Value;
use wylde_harness::HarnessApi;
use wylde_shared::ipc::Reply;

pub async fn dispatch<A: HarnessApi + ?Sized>(
    api: &A,
    verb: &str,
    payload: Value,
) -> Option<Reply> {
    match verb {
        "tools.list" => Some(api.tools_list(payload).await),
        "tools.run" => Some(api.tools_run(payload).await),
        _ => None,
    }
}

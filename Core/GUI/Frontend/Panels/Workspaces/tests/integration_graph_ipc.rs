//! Integration: the graph IPC data → panel-state path over a real named pipe.
//!
//! Stands up a mock `wylde-workspaces` server on a private, per-process fixture
//! pipe (never the production endpoint — see `PipeNameOverride` below)
//! speaking the exact msgpack wire format `wylde_gui_pipe::call` uses (4-byte
//! BE length prefix + a `{id, method, http_verb, data, meta}` request, a
//! `{ok, data}` reply). It answers `workspaces.list_mru` with a one-workspace
//! MRU and `workspaces.graph` with a small fixture corpus shaped exactly like
//! the service's `projection::project` output, then drives
//! `ipc::fetch_active_graph()` and asserts the resulting `GraphLoad` carries
//! the right workspace id + node/edge/cluster counts.
//!
//! No memgraph and no real service binary needed — this verifies the wire
//! contract and deserialisation the panel relies on. (Windows-only: the
//! transport is named-pipe-based, like the rest of the GUI.)

#![cfg(target_os = "windows")]

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

use wylde_gui_pipe::test_backend;
use wylde_panel_workspaces::graph::ipc;

/// The service the panel's graph verbs target. The fixture server binds a
/// PRIVATE pipe and routes this service name to it — see the override in the
/// test below.
const SERVICE: &str = "wylde-workspaces";

/// A fixture `workspaces.graph` reply mirroring the service projection: 4
/// workspace entities in one file dir + 2 synthesised external edge targets,
/// 3 edges (CALLS/IMPORTS/INHERITS), 1 cluster.
fn graph_fixture() -> Value {
    let node = |id: &str, kind: &str, file: &str| {
        json!({
            "id": id, "kind": kind, "name": id, "file": file,
            "line": 0, "position": { "x": 0.0, "y": 0.0, "z": 0.0 }, "style": {}
        })
    };
    json!({
        "nodes": [
            node("widget", "Module", "src/widget.rs"),
            node("alpha", "Function", "src/widget.rs"),
            node("beta", "Function", "src/widget.rs"),
            node("Widget", "Class", "src/widget.rs"),
            node("std::collections", "Module", ""),
            node("Render", "Class", "")
        ],
        "edges": [
            { "src": "alpha", "dst": "beta", "rel_type": "CALLS", "weight": 1.0 },
            { "src": "widget", "dst": "std::collections", "rel_type": "IMPORTS", "weight": 1.0 },
            { "src": "Widget", "dst": "Render", "rel_type": "INHERITS", "weight": 1.0 }
        ],
        "clusters": [
            { "id": "src", "member_ids": ["alpha", "beta", "widget", "Widget"],
              "parent_breadcrumb": ["src"], "zoom_threshold": 1.0 }
        ]
    })
}

/// Read one length-prefixed msgpack frame.
async fn read_frame(server: &mut NamedPipeServer) -> Value {
    let mut hdr = [0u8; 4];
    server.read_exact(&mut hdr).await.unwrap();
    let n = u32::from_be_bytes(hdr) as usize;
    let mut buf = vec![0u8; n];
    server.read_exact(&mut buf).await.unwrap();
    rmp_serde::from_slice(&buf).unwrap()
}

/// Write a `{ok:true, data}` reply as a length-prefixed msgpack frame.
async fn write_reply(server: &mut NamedPipeServer, data: Value) {
    let reply = json!({ "ok": true, "data": data });
    let body = rmp_serde::to_vec_named(&reply).unwrap();
    server
        .write_all(&(body.len() as u32).to_be_bytes())
        .await
        .unwrap();
    server.write_all(&body).await.unwrap();
    server.flush().await.unwrap();
}

/// Handle one already-connected instance: decode the requested action, reply.
async fn handle(mut server: NamedPipeServer) {
    let env = read_frame(&mut server).await;
    let action = env
        .get("data")
        .and_then(|d| d.get("action"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let data = match action.as_str() {
        "workspaces.list_mru" => {
            json!({ "workspaces": [{ "id": "demo", "folder": "C:/ws/demo" }] })
        }
        "workspaces.graph" => graph_fixture(),
        other => panic!("unexpected action {other}"),
    };
    write_reply(&mut server, data).await;
}

/// Accept-loop server: always keeps the *next* pipe instance created before
/// handling the current one, so a client's follow-up call never races a
/// not-yet-created pipe. Serves exactly `count` connections then returns.
/// `initial` is created in the test thread (before the first client call) so
/// the pipe exists up front.
async fn run_server(initial: NamedPipeServer, pipe: String, count: usize) {
    let mut listener = initial;
    for _ in 0..count {
        listener.connect().await.unwrap();
        let connected = listener;
        // Create the next waiting instance BEFORE handling this one.
        listener = ServerOptions::new().create(&pipe).unwrap();
        handle(connected).await;
    }
}

#[tokio::test]
async fn fetch_active_graph_loads_fixture_over_the_pipe() {
    // The GUI runtime stashes a tokio handle for `wylde_gui_pipe::call`; in a
    // bare test there is none, so calls run inline on this runtime — which is
    // exactly what we want.

    // Own a PRIVATE pipe, never the production one. Binding
    // `\\.\pipe\wylde-workspaces` here claimed the live service's endpoint, so
    // this test failed with ERROR_ACCESS_DENIED on any machine actually running
    // Wylde — and passed in CI, which never runs the stack (#75). The override
    // re-points `wylde_gui_pipe::call` at the fixture for the life of the guard.
    let pipe = test_backend::unique_pipe_name(SERVICE);
    let _route = test_backend::PipeNameOverride::install(SERVICE, &pipe);

    // Create the first pipe instance up front so the client's first connect
    // can't lose a race with the server task starting.
    let initial = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&pipe)
        .expect("fixture pipe should be free — it is unique to this process");
    // `fetch_active_graph` makes two sequential calls (list_mru, then graph).
    let server = tokio::spawn(run_server(initial, pipe, 2));

    let load = ipc::fetch_active_graph()
        .await
        .expect("graph load should succeed over the mock pipe");

    assert_eq!(load.workspace_id.as_deref(), Some("demo"));
    assert_eq!(load.graph.nodes.len(), 6, "4 entities + 2 external targets");
    assert_eq!(load.graph.edges.len(), 3);
    assert_eq!(load.graph.clusters.len(), 1);

    // Spot-check the deserialised model matches the wire kinds/rels.
    use wylde_panel_workspaces::graph::model::{NodeKind, RelType};
    let widget = load.graph.node_by_id("widget").unwrap();
    assert_eq!(widget.kind, NodeKind::Module);
    assert!(load
        .graph
        .edges
        .iter()
        .any(|e| e.rel_type == RelType::Inherits));

    server.await.unwrap();
}

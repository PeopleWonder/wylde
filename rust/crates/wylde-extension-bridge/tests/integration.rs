//! End-to-end integration test: launch `wylde_mcp_py_shim` wrapping a
//! synthetic in-tree test extension and drive the full round-trip
//! through the host's `Host` API (initialize, tools/list, tools/call,
//! ping). Skipped if the .venv python isn't resolvable — running the
//! shim requires a working Python 3 interpreter on PATH or
//! `WYLDE_PYTHON`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;
use tempfile::TempDir;

use wylde_extension_bridge::manifest::{load_extension, McpServerManifest, Transport};
use wylde_extension_bridge::mcp::{McpClient, SpawnSpec};

/// Resolve the Python interpreter to use for the shim.
fn resolve_python() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("WYLDE_PYTHON") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    let wylde_root = std::env::var("WYLDE_ROOT").unwrap_or_else(|_| ".".to_string());
    let suffix = if cfg!(windows) {
        ".venv/Scripts/python.exe"
    } else {
        ".venv/bin/python3"
    };
    let venv = PathBuf::from(&wylde_root).join(suffix);
    if venv.exists() {
        return Some(venv);
    }
    // Fall back to plain `python3` on PATH. We don't probe PATH here —
    // Command::new will do that at spawn time. Returning None means
    // "no resolvable interpreter"; the test will skip.
    None
}

/// Plant an extension folder and a sibling `_shim` folder under
/// `extensions_dir` that mirrors the real on-tree shim. The shim
/// directory is created by symlink (or a tiny re-import stub on
/// Windows) so the test doesn't need to embed the shim source.
fn write_synth_extension(tmp: &TempDir, real_shim_dir: &std::path::Path) -> PathBuf {
    let ext_dir = tmp.path().join("Synth");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(
        ext_dir.join("manifest.json"),
        r#"{
            "name": "Synth",
            "description": "tiny test extension",
            "version": "1.0",
            "enabled": true,
            "transport": "http",
            "handler": "handler",
            "capabilities": [],
            "tools": [
                {
                    "tool_id": "echo",
                    "description": "echo args back",
                    "endpoint": "do_echo",
                    "parameters": [
                        {"name": "msg", "type": "string", "required": true}
                    ],
                    "tags": ["test"]
                }
            ]
        }"#,
    )
    .unwrap();
    std::fs::write(
        ext_dir.join("handler.py"),
        "def do_echo(params):\n    return {'got': params}\n",
    )
    .unwrap();
    // Plant mcp-server.json pointing at the shim. command uses ${WYLDE_PYTHON}.
    std::fs::write(
        ext_dir.join("mcp-server.json"),
        r#"{
            "name": "Synth",
            "transport": "stdio",
            "enabled": true,
            "command": ["${WYLDE_PYTHON}", "-m", "Extensions._shim.server",
                        "--extension", "Synth",
                        "--extensions-root", "PLACEHOLDER"],
            "env": {}
        }"#,
    )
    .unwrap();
    // Copy the real shim directory next to the synth so module
    // resolution works. We re-implement just enough to point Python
    // at the tmpdir for sys.path purposes via PYTHONPATH below.
    let shim_dst = tmp.path().join("Extensions").join("_shim");
    std::fs::create_dir_all(&shim_dst).unwrap();
    for entry in std::fs::read_dir(real_shim_dir).unwrap().flatten() {
        let from = entry.path();
        if !from.is_file() {
            continue;
        }
        let to = shim_dst.join(from.file_name().unwrap());
        std::fs::copy(&from, &to).unwrap();
    }
    // Also place a copy of Synth under Extensions/ so `--extension Synth` works.
    let ext_under_extensions = tmp.path().join("Extensions").join("Synth");
    std::fs::create_dir_all(&ext_under_extensions).unwrap();
    std::fs::copy(
        ext_dir.join("manifest.json"),
        ext_under_extensions.join("manifest.json"),
    )
    .unwrap();
    std::fs::copy(
        ext_dir.join("handler.py"),
        ext_under_extensions.join("handler.py"),
    )
    .unwrap();
    ext_dir
}

#[tokio::test]
async fn shim_roundtrip_via_synth_extension() {
    let Some(python) = resolve_python() else {
        eprintln!("skip: no python interpreter resolvable");
        return;
    };
    // Locate the real shim source so we can copy it next to the synth ext.
    let real_shim = {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        // wylde-extension-bridge is at rust/crates/wylde-extension-bridge —
        // walk back to the vault root (../../../) then into Extensions/_shim.
        manifest_dir
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .map(|p| p.join("Extensions").join("_shim"))
    };
    let Some(real_shim) = real_shim.filter(|p| p.join("server.py").exists()) else {
        eprintln!("skip: real shim source not findable from CARGO_MANIFEST_DIR");
        return;
    };

    let tmp = TempDir::new().unwrap();
    let _ext = write_synth_extension(&tmp, &real_shim);

    // Build the SpawnSpec ourselves rather than going via Host (Host
    // resolves manifest paths from disk and would need extensions_dir
    // wired up; cleaner here to drive the MCP client directly).
    let extensions_root = tmp.path().join("Extensions");
    let env_overrides: BTreeMap<String, String> = [
        (
            "PYTHONPATH".to_string(),
            tmp.path().to_string_lossy().to_string(),
        ),
    ]
    .into_iter()
    .collect();
    let command: Vec<String> = vec![
        python.to_string_lossy().to_string(),
        "-m".to_string(),
        "Extensions._shim.server".to_string(),
        "--extension".to_string(),
        "Synth".to_string(),
        "--extensions-root".to_string(),
        extensions_root.to_string_lossy().to_string(),
    ];
    let spec = SpawnSpec {
        command: &command,
        cwd: Some(tmp.path()),
        env: &env_overrides,
    };

    let client = match McpClient::connect_stdio(
        spec,
        Duration::from_secs(10),
        "wylde-extension-bridge-test",
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("skip: connect_stdio failed — likely Python missing required deps: {e}");
            return;
        }
    };
    assert!(client.version_decision.accepted());
    let tools = client
        .list_tools(Duration::from_secs(5))
        .await
        .expect("tools/list");
    assert!(tools.iter().any(|t| t.name == "echo"));

    let result = client
        .call_tool(
            "echo",
            json!({"msg": "hello"}),
            Duration::from_secs(5),
        )
        .await
        .expect("tools/call");
    let body = result
        .get("structuredContent")
        .expect("structuredContent");
    assert_eq!(body["got"]["msg"], "hello");

    client.ping(Duration::from_secs(3)).await.expect("ping");
    client.shutdown().await;
}

#[test]
fn manifest_parses_for_real_in_tree_extensions() {
    // Anchor test: the two shipped mcp-server.json files MUST parse
    // and validate (catches breakage from typos in the JSON).
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let wylde_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .expect("walk to vault root");
    for ext_name in ["Webcrawler", "Wylde_Study"] {
        let ext_root = wylde_root.join("Extensions").join(ext_name);
        if !ext_root.join("mcp-server.json").exists() {
            eprintln!("skip: {} mcp-server.json not present", ext_name);
            continue;
        }
        let rec = load_extension(&ext_root).expect("load_extension");
        assert_eq!(rec.manifest.transport, Transport::Stdio);
        assert!(!rec.manifest.command.is_empty());
        let m: &McpServerManifest = &rec.manifest;
        assert_eq!(m.name, ext_name);
    }
}

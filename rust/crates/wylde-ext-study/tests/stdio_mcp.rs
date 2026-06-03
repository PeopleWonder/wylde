//! Integration test — spawn the built `wylde-ext-study` binary and drive the
//! MCP stdio protocol the `wylde-extension-bridge` host speaks.
//!
//! Sends `initialize` then `tools/list` over stdin, reads the newline-framed
//! JSON-RPC replies off stdout, and asserts the five Study tools surface with
//! object input schemas. Stays entirely on the protocol surface — no harness
//! pipe is involved, so this needs no live `wylde-harness` (tool *handlers*
//! are unit-tested against a mock client in `src/tools.rs`).

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

/// Path to the binary cargo built for this test (`CARGO_BIN_EXE_<name>`).
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_wylde-ext-study")
}

#[test]
fn stdio_initialize_then_tools_list() {
    let mut child = Command::new(bin())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn wylde-ext-study");

    let mut stdin = child.stdin.take().expect("child stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("child stdout"));

    // 1) initialize
    writeln!(
        stdin,
        "{}",
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} })
    )
    .unwrap();
    stdin.flush().unwrap();

    let init = read_frame(&mut stdout);
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(init["result"]["serverInfo"]["name"], "wylde-ext-study");

    // 2) tools/list
    writeln!(
        stdin,
        "{}",
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} })
    )
    .unwrap();
    stdin.flush().unwrap();

    let list = read_frame(&mut stdout);
    assert_eq!(list["id"], 2);
    let tools = list["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert_eq!(
        names,
        [
            "study_index_page",
            "study_query",
            "study_summarize",
            "study_explain",
            "study_flashcards"
        ],
        "the five Python Study tool names must surface verbatim"
    );
    for t in tools {
        assert_eq!(t["inputSchema"]["type"], "object", "tool {t:?} needs object schema");
    }

    // Closing stdin (EOF) tells the server loop to exit cleanly.
    drop(stdin);
    let status = child.wait().expect("wait for child");
    assert!(status.success(), "server should exit 0 on stdin EOF");
}

/// Read one newline-delimited JSON-RPC frame, skipping any blank lines.
fn read_frame(stdout: &mut impl BufRead) -> Value {
    loop {
        let mut line = String::new();
        let n = stdout.read_line(&mut line).expect("read stdout");
        assert_ne!(n, 0, "unexpected EOF before a JSON-RPC frame arrived");
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        return serde_json::from_str(trimmed)
            .unwrap_or_else(|e| panic!("non-JSON frame {trimmed:?}: {e}"));
    }
}

//! Drive the real binary over stdio the way an MCP client does.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

#[test]
fn mcp_session_over_stdio() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("orion.toml");
    let audit = dir.path().join("audit.jsonl");
    std::fs::write(
        &config,
        format!(
            "[tools]\nfs_write = \"allow\"\n[fs]\nwrite_roots = [{root:?}]\n[audit]\npath = {audit:?}\n",
            root = dir.path().to_str().unwrap(),
            audit = audit.to_str().unwrap(),
        ),
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_orion"))
        .args(["--config", config.to_str().unwrap(), "serve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut next_id = 0;
    let mut rpc = |method: &str, params: Value| -> Value {
        next_id += 1;
        writeln!(stdin, "{}", json!({ "jsonrpc": "2.0", "id": next_id, "method": method, "params": params })).unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["id"], next_id);
        reply
    };

    let init = rpc("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "0" } }));
    assert_eq!(init["result"]["serverInfo"]["name"], "orion");

    let file = dir.path().join("note.txt");
    let wrote = rpc("tools/call", json!({ "name": "fs_write", "arguments": { "path": file, "content": "from the model" } }));
    assert_eq!(wrote["result"]["isError"], false, "{wrote}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "from the model");

    let denied = rpc("tools/call", json!({ "name": "fs_write", "arguments": { "path": "/etc/orion-test", "content": "x" } }));
    assert_eq!(denied["result"]["isError"], true);

    drop(stdin);
    assert!(child.wait().unwrap().success());

    let log = std::fs::read_to_string(&audit).unwrap();
    let entries: Vec<Value> = log.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["ok"], true);
    assert_eq!(entries[1]["ok"], false);
}

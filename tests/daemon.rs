//! The daemon and the `orion serve` bridge, as separate processes.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

#[test]
fn serve_bridges_to_a_running_daemon() {
    let home = tempfile::tempdir().unwrap();
    let run = home.path().join("run");
    std::fs::create_dir_all(home.path().join(".config/orion")).unwrap();
    std::fs::write(
        home.path().join(".config/orion/orion.toml"),
        "[sandbox]\nmode = \"off\"\n",
    )
    .unwrap();
    let env = |c: &mut Command| {
        c.env("HOME", home.path())
            .env("XDG_RUNTIME_DIR", &run)
            .env_remove("ORION_CONFIG");
    };

    let mut daemon = Command::new(env!("CARGO_BIN_EXE_orion"));
    env(&mut daemon);
    let mut daemon = daemon.arg("daemon").stderr(Stdio::null()).spawn().unwrap();
    let socket = run.join("orion/orion.sock");
    let start = Instant::now();
    while !socket.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "daemon did not start"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let mut serve = Command::new(env!("CARGO_BIN_EXE_orion"));
    env(&mut serve);
    let mut serve = serve
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = serve.stdin.take().unwrap();
    let mut stdout = BufReader::new(serve.stdout.take().unwrap());
    let mut rpc = |id: u64, method: &str, params: Value| -> Value {
        writeln!(
            stdin,
            "{}",
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
        )
        .unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };

    rpc(
        1,
        "initialize",
        json!({ "clientInfo": { "name": "bridge-test" } }),
    );
    let r = rpc(
        2,
        "tools/call",
        json!({ "name": "system_info", "arguments": {} }),
    );
    assert_eq!(r["result"]["isError"], false, "{r}");
    // Only the daemon knows `orion/status`: proof the call went through it.
    let st = rpc(3, "orion/status", json!({}));
    assert_eq!(st["result"]["tool_calls"], 1, "{st}");

    drop(stdin);
    let _ = serve.wait(); // exits when its stdin closes
    let _ = daemon.kill();
    let _ = daemon.wait();
    let log = std::fs::read_to_string(home.path().join(".local/state/orion/audit.jsonl")).unwrap();
    assert!(log.contains("\"caller\":\"bridge-test (pid "), "{log}");
}

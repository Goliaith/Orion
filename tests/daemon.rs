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
    let daemon = daemon.arg("daemon").stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    // Stopped however the test ends: a daemon left running would keep
    // cargo waiting for the test's output forever.
    struct Reap(std::process::Child);
    impl Drop for Reap {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let daemon = Reap(daemon);
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
    // Through the daemon, agents are offered tools that need approval too.
    let list = rpc(4, "tools/list", json!({}));
    let fs_write = list["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == "fs_write").cloned();
    assert!(fs_write.is_some_and(|t| t["description"].as_str().unwrap().contains("waits for a person")), "{list}");
    // With nobody approving, a call that needs it is refused, and says why.
    let w = rpc(5, "tools/call", json!({ "name": "fs_write", "arguments": { "path": "~/orion-workspace/x.txt", "content": "x" } }));
    assert_eq!(w["result"]["isError"], true, "{w}");
    assert!(w["result"]["content"][0]["text"].as_str().unwrap().contains("nobody is approving"), "{w}");
    // But an agent may never approve, not even its own calls.
    for (id, method) in [(6, "orion/approvals/self"), (7, "orion/approvals/watch"), (8, "orion/approvals/answer")] {
        let r = rpc(id, method, json!({ "id": 1, "approve": true }));
        assert!(r["error"]["message"].as_str().unwrap_or_default().contains("can't answer approval requests"), "{method}: {r}");
    }
    // The daemon's watch: what needs attention, as a list (maybe empty).
    let h = rpc(9, "orion/health", json!({}));
    assert!(h["result"]["findings"].is_array(), "{h}");

    drop(stdin);
    let _ = serve.wait(); // exits when its stdin closes
    drop(daemon);
    let log = std::fs::read_to_string(home.path().join(".local/state/orion/audit.jsonl")).unwrap();
    assert!(log.contains("\"caller\":\"bridge-test (pid "), "{log}");
}

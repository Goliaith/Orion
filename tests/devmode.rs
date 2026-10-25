//! Developer mode end to end: the mode switch, both dev shells in real
//! bubblewrap, what each can and can't reach, approvals, checkpoints and
//! rollback. One test, since it sets HOME for the whole process.

use std::cell::Cell;
use std::os::unix::net::UnixListener;
use std::path::Path;

use orion::dev::{self, Mode};
use orion::{Approver, Orion, Policy, Tool};
use serde_json::{json, Value};

struct Yes(Cell<u32>);

impl Approver for Yes {
    fn approve(&self, _tool: &dyn Tool, _args: &Value) -> bool {
        self.0.set(self.0.get() + 1);
        true
    }
}

fn run(o: &Orion, yes: &Yes, args: Value) -> Value {
    o.call("dev_shell", args.clone(), Some(yes)).unwrap_or_else(|e| panic!("{args}: {e}"))
}

fn out(v: &Value) -> &str {
    v["output"].as_str().unwrap()
}

#[test]
fn developer_mode() {
    // Not under /tmp, which the sandbox replaces with its own.
    let home = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let h = home.path().canonicalize().unwrap();
    let runtime = h.join("run");
    std::fs::create_dir_all(runtime.join("orion")).unwrap();
    // SAFETY: the only test in this binary, before any thread starts.
    unsafe {
        std::env::set_var("HOME", &h);
        std::env::set_var("XDG_RUNTIME_DIR", &runtime);
        std::env::remove_var("XDG_STATE_HOME");
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::remove_var("ORION_CONFIG");
    }
    if !orion::sandbox::bubblewrap_works() {
        assert!(std::env::var_os("ORION_CI_SANDBOX").is_none(), "bubblewrap must work here");
        eprintln!("skipped: bubblewrap can't make a sandbox here");
        return;
    }
    for d in [".ssh", ".cargo", ".local/state/orion"] {
        std::fs::create_dir_all(h.join(d)).unwrap();
    }
    std::fs::write(h.join(".ssh/id_test"), "secret key").unwrap();
    // Stand-ins for the daemon's socket and the session bus.
    let _daemon = UnixListener::bind(runtime.join("orion/orion.sock")).unwrap();
    let _bus = UnixListener::bind(runtime.join("bus")).unwrap();

    let audit = h.join("audit.jsonl");
    let policy = Policy::from_toml(&format!("[audit]\npath = {:?}\n[dev]\ncaches = [\"~/.cargo\"]\n", audit.to_str().unwrap())).unwrap();
    let o = Orion::new(policy.clone());
    let yes = Yes(Cell::new(0));

    // Off by default, and nothing opens.
    assert_eq!(dev::mode(), Mode::Safe);
    let e = o.call("dev_shell", json!({ "command": "true" }), Some(&yes)).unwrap_err().to_string();
    assert!(e.contains("developer mode is off"), "{e}");
    assert_eq!(yes.0.get(), 0);

    // Sandboxed.
    dev::switch(&policy, Mode::Sandboxed, Some("1h")).unwrap();
    let root = h.join("orion-dev");
    assert!(root.is_dir());
    let r = run(&o, &yes, json!({ "command": "mkdir app && cd app && echo one > f && export X=7" }));
    assert_eq!((r["exit_code"].as_i64(), yes.0.get()), (Some(0), 1), "{r}");
    assert_eq!(r["cwd"].as_str(), Some(root.join("app").to_str().unwrap()));
    let shell = r["shell"].as_str().unwrap().to_string();
    let first = r["checkpoint"].as_u64().unwrap();

    // State carries over, and commands to an open shell don't ask.
    let r = run(&o, &yes, json!({ "shell": shell, "command": "echo $X; cat f; echo oops >&2; false" }));
    assert_eq!((out(&r), r["exit_code"].as_i64(), yes.0.get()), ("7\none\noops\n", Some(1), 1), "{r}");

    // Writable: the dev root and the caches. Not: the rest of home, keys, Orion's state.
    let r = run(&o, &yes, json!({ "shell": shell, "command":
        "touch ~/.cargo/ok && echo cache; touch ~/outside 2>/dev/null || echo no-home; cat ~/.ssh/id_test 2>/dev/null || echo no-key; \
         ls -A ~/.local/state/orion | wc -l; [ -S $XDG_RUNTIME_DIR/orion/orion.sock ] || echo no-daemon; \
         [ -S $XDG_RUNTIME_DIR/bus ] || echo no-bus; readlink /proc/self/ns/net" }));
    let lines: Vec<&str> = out(&r).lines().collect();
    assert_eq!(&lines[..6], ["cache", "no-home", "no-key", "0", "no-daemon", "no-bus"], "{r}");
    assert_eq!(lines[6], std::fs::read_link("/proc/self/ns/net").unwrap().to_str().unwrap(), "the network is on");
    assert!(h.join(".cargo/ok").exists() && !h.join("outside").exists());

    // A command still running is followed with dev_shell_read.
    let r = run(&o, &yes, json!({ "shell": shell, "command": "sleep 1; echo late", "wait_secs": 0 }));
    assert_eq!((r["running"].as_bool(), r["exit_code"].as_i64()), (Some(true), None), "{r}");
    let busy = o.call("dev_shell", json!({ "shell": shell, "command": "true" }), Some(&yes)).unwrap_err().to_string();
    assert!(busy.contains("still running"), "{busy}");
    let r = o.call("dev_shell_read", json!({ "shell": shell, "wait_secs": 20 }), None).unwrap();
    assert_eq!((out(&r), r["exit_code"].as_i64()), ("late\n", Some(0)), "{r}");

    // Undo what a command did to the dev root.
    run(&o, &yes, json!({ "shell": shell, "command": "echo broken > f; mkdir junk" }));
    let history = o.call("dev_history", json!({}), None).unwrap();
    assert!(history["checkpoints"].as_array().unwrap().len() >= 4, "{history}");
    let r = o.call("dev_rollback", json!({ "id": first + 1 }), Some(&yes)).unwrap();
    assert!(r["restored"].as_u64().unwrap() >= 1, "{r}");
    assert_eq!(std::fs::read_to_string(root.join("app/f")).unwrap(), "one\n");
    assert!(!root.join("app/junk").exists());

    // Back to safe: the shell is gone.
    dev::switch(&policy, Mode::Safe, None).unwrap();
    let gone = o.call("dev_shell", json!({ "shell": shell, "command": "true" }), Some(&yes)).unwrap_err().to_string();
    assert!(gone.contains("developer mode is off"), "{gone}");

    // Unsandboxed: your files and keys, but not Orion's, nor its sockets,
    // nor the user bus, nor the machine's other processes.
    dev::switch(&policy, Mode::Unsandboxed, None).unwrap();
    let asked = yes.0.get();
    let r = run(&o, &yes, json!({ "command":
        "cat ~/.ssh/id_test; echo; touch ~/outside && echo home; touch ~/.ssh/new 2>/dev/null || echo ssh-read-only; \
         ls -A ~/.local/state/orion | wc -l; [ -S $XDG_RUNTIME_DIR/orion/orion.sock ] || echo no-daemon; \
         [ -S $XDG_RUNTIME_DIR/bus ] || echo no-bus; ls /proc | grep -c '^[0-9]'" }));
    assert_eq!(yes.0.get(), asked + 1);
    let lines: Vec<&str> = out(&r).lines().collect();
    assert_eq!(&lines[..6], ["secret key", "home", "ssh-read-only", "0", "no-daemon", "no-bus"], "{r}");
    assert!(lines[6].parse::<u32>().unwrap() < 10, "only the shell's own processes: {r}");
    assert_eq!(r["mode"], "unsandboxed");
    assert!(h.join("outside").exists() && !h.join(".ssh/new").exists());
    // Opening an unsandboxed shell is never covered by "always for this session".
    let tool = o.tools().find(|(t, _)| t.name() == "dev_shell").unwrap().0;
    assert!(tool.every_time(&json!({ "command": "true" })));
    assert!(!tool.every_time(&json!({ "command": "true", "shell": r["shell"] })));
    dev::switch(&policy, Mode::Safe, None).unwrap();

    // Every command is in the audit log.
    let log = std::fs::read_to_string(&audit).unwrap();
    assert!(log.lines().filter(|l| l.contains("\"tool\":\"dev_shell\"")).count() >= 7, "{log}");
    assert!(log.contains("UNSANDBOXED dev shell"), "the approval's wording is logged");
    assert!(!Path::new(&h.join(".local/state/orion/dev-checkpoints")).read_dir().unwrap().next().is_none());
}

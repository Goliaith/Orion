//! Orion is the interface between an AI model and a Linux machine.
//!
//! The machine is exposed as a set of typed [`Tool`]s. Every call goes through
//! a [`Policy`] (allow, ask or deny, per tool and per risk level, with
//! filesystem and command scopes) and is written to an append-only audit log.
//! The tools are served over the Model Context Protocol (see [`mcp`]), so any
//! MCP-capable agent, whether hosted or local, can drive the machine through the
//! same contract. [`chat`] is the human-facing front-end on top of it.

pub mod approvals;
pub mod audit;
pub mod chat;
pub mod daemon;
pub mod dev;
pub mod doctor;
pub mod envfile;
pub mod helper;
pub mod interrupt;
pub mod mcp;
pub mod policy;
pub mod remote;
pub mod sandbox;
pub mod seccomp;
pub mod setup;
pub mod tool;
pub mod tools;
pub mod trust;
pub mod tuning;
pub mod voice;
pub mod watch;

use std::fmt;
use std::time::Instant;

use serde_json::Value;

pub use audit::Audit;
pub use policy::{Decision, Policy};
pub use tool::{Preapproval, Risk, Tool};

/// Why a tool call did not produce a result.
/// The failure a tool reports in its result, when it ran but what it
/// drove did not succeed: `"ok": false` (a package manager that failed
/// partway), with its `"error"`.
pub fn reported_failure(value: &Value) -> Option<String> {
    if value.get("ok") != Some(&Value::Bool(false)) {
        return None;
    }
    let why = value.get("error").and_then(Value::as_str).unwrap_or("the tool reported a failure");
    Some(why.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or(why).trim().to_string())
}

#[derive(Debug)]
pub enum CallError {
    Unknown(String),
    /// Refused by the policy or by the human; the message says which.
    Denied(String),
    Failed(anyhow::Error),
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallError::Unknown(name) => write!(f, "unknown tool: {name}"),
            CallError::Denied(reason) => f.write_str(reason),
            CallError::Failed(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for CallError {}

/// Answers `ask` decisions. Implemented by whatever has a human attached.
pub trait Approver {
    fn approve(&self, tool: &dyn Tool, args: &Value) -> bool;

    /// Why the last call wasn't approved, when it wasn't a person saying
    /// no (nobody was there to ask, say).
    fn why_not(&self) -> Option<String> {
        None
    }
}

/// The tool registry with its policy and audit log attached.
pub struct Orion {
    policy: Policy,
    tools: Vec<Box<dyn Tool>>,
    audit: Audit,
    /// The daemon, when calls go through it.
    remote: Option<remote::Remote>,
    /// Why they don't, when they could have.
    remote_note: Option<String>,
}

impl Orion {
    pub fn new(policy: Policy) -> Self {
        let audit = Audit::new(policy.audit_path());
        Self { tools: tools::builtin(), policy, audit, remote: None, remote_note: None }
    }

    /// Call tools through the daemon from now on, if it runs with this
    /// policy (`source`) and version; otherwise keep calling them here.
    pub fn through_daemon(mut self, source: Option<&std::path::Path>, client: &str) -> Self {
        match remote::Remote::connect(&self.policy, source, client) {
            Ok(r) => self.remote = Some(r),
            Err(why) => self.remote_note = Some(why),
        }
        self
    }

    /// Where tool calls run: "through the daemon (pid N)", or "in this
    /// process" with why, for status displays.
    pub fn runs_tools(&self) -> String {
        match (&self.remote, &self.remote_note) {
            (Some(r), _) if r.alive() => format!("through the daemon (pid {})", r.pid()),
            (Some(_), _) => "in this process (the daemon went away)".into(),
            (None, Some(why)) if why != "no daemon is running" && !why.contains("\"never\"") => format!("in this process ({why})"),
            _ => "in this process".into(),
        }
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// Every registered tool with the decision the policy makes for it.
    pub fn tools(&self) -> impl Iterator<Item = (&dyn Tool, Decision)> {
        self.tools
            .iter()
            .map(|t| (t.as_ref(), self.policy.decide(t.name(), t.risk())))
    }

    /// Call a tool as the AI. `approver` answers `ask` decisions; without
    /// one they are denied.
    pub fn call(&self, name: &str, args: Value, approver: Option<&dyn Approver>) -> Result<Value, CallError> {
        self.call_as(name, args, approver, None)
    }

    /// [`call`](Self::call), naming the caller in the audit log.
    pub fn call_as(
        &self,
        name: &str,
        args: Value,
        approver: Option<&dyn Approver>,
        caller: Option<&str>,
    ) -> Result<Value, CallError> {
        if let Some(r) = self.remote.as_ref().filter(|r| r.alive()) {
            return r.call(self, name, args, approver, caller);
        }
        let tool = self
            .tools
            .iter()
            .find(|t| t.name() == name)
            .ok_or_else(|| CallError::Unknown(name.to_string()))?;
        let decision = self.policy.decide(name, tool.risk());
        let mut approved = None;
        let precheck = || tool.precheck(&self.policy, &args).map_err(|e| format!("{e:#}"));
        let verdict = match (decision, tool.preapproval(&self.policy, &args)) {
            (Decision::Deny, _) => Err(format!("tool {name} is denied by the Orion policy")),
            // Refused before the checks, which can be slow (a dry run).
            (Decision::Allow, Preapproval::Refused(reason)) => Err(reason),
            (_, _) if let Err(e) = precheck() => Err(e),
            (d, p) if p.unasked(d) => Ok(()),
            _ => match approver {
                Some(approver) => {
                    let yes = approver.approve(tool.as_ref(), &args);
                    approved = Some(yes);
                    if yes {
                        Ok(())
                    } else {
                        Err(approver.why_not().unwrap_or_else(|| format!("the user declined this {name} call")))
                    }
                }
                None => Err(format!("tool {name} needs interactive approval, which is not available here")),
            },
        };
        // Shown as it was before the call: sys_edit's diff is gone after it.
        let shown = tool.redact(&args);
        let start = Instant::now();
        let result = match verdict {
            Ok(()) => tool.call(&self.policy, args.clone()).map_err(CallError::Failed),
            Err(reason) => Err(CallError::Denied(reason)),
        };
        self.audit.record(audit::Entry {
            tool: name,
            caller,
            decision,
            approved,
            args: &shown,
            error: match &result {
                Err(e) => Some(e.to_string()),
                Ok(value) => reported_failure(value),
            },
            duration: start.elapsed(),
        });
        result
    }

    /// Turn a call away before the policy is consulted (the person asking
    /// may not use it), and record that in the audit log.
    pub fn refuse(&self, name: &str, args: &Value, reason: &str, caller: Option<&str>) -> CallError {
        self.audit.record(audit::Entry {
            tool: name,
            caller,
            decision: Decision::Deny,
            approved: None,
            args,
            error: Some(reason.to_string()),
            duration: std::time::Duration::ZERO,
        });
        CallError::Denied(reason.to_string())
    }

    /// Record a call that something other than a tool handled (the chat's
    /// memory), so the audit log is complete.
    pub fn record(&self, name: &str, args: &Value, caller: Option<&str>, error: Option<&str>) {
        self.audit.record(audit::Entry {
            tool: name,
            caller,
            decision: Decision::Allow,
            approved: None,
            args,
            error: error.map(str::to_string),
            duration: std::time::Duration::ZERO,
        });
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use serde_json::json;

    use super::*;

    #[test]
    fn reported_failures() {
        assert_eq!(reported_failure(&json!({ "ok": true })), None);
        assert_eq!(reported_failure(&json!({ "files": [] })), None);
        let failed = json!({ "ok": false, "error": "Reading package lists...\nE: Could not get lock\n\n" });
        assert_eq!(reported_failure(&failed).as_deref(), Some("E: Could not get lock"));
        assert_eq!(reported_failure(&json!({ "ok": false })).as_deref(), Some("the tool reported a failure"));
    }

    struct Answer {
        yes: bool,
        asked: Cell<u32>,
    }

    impl Approver for Answer {
        fn approve(&self, _tool: &dyn Tool, _args: &Value) -> bool {
            self.asked.set(self.asked.get() + 1);
            self.yes
        }
    }

    fn orion(policy: &str) -> Orion {
        Orion::new(Policy::from_toml(&format!("{policy}\n[audit]\nenabled = false\n")).unwrap())
    }

    #[test]
    fn ask_needs_a_human() {
        let o = orion("[proc]\nallowed_commands = [\"true\"]\n");
        let run = |cmd: &str, a: Option<&dyn Approver>| o.call("proc_run", json!({ "command": cmd }), a);

        // Pre-approved commands run without a prompt, even with nobody there.
        assert!(run("true", None).is_ok());
        assert!(matches!(run("echo", None), Err(CallError::Denied(_))));

        let yes = Answer { yes: true, asked: Cell::new(0) };
        assert!(run("echo", Some(&yes)).is_ok());
        assert!(run("true", Some(&yes)).is_ok());
        assert_eq!(yes.asked.get(), 1);

        let no = Answer { yes: false, asked: Cell::new(0) };
        assert!(matches!(run("echo", Some(&no)), Err(CallError::Denied(r)) if r.contains("declined")));
    }

    #[test]
    fn prechecks_run_before_prompting() {
        let o = orion("[fs]\nwrite_roots = []\n");
        let yes = Answer { yes: true, asked: Cell::new(0) };
        let r = o.call("fs_write", json!({ "path": "/tmp/nope.txt", "content": "x" }), Some(&yes));
        assert!(r.unwrap_err().to_string().contains("outside the writable roots"));
        assert_eq!(yes.asked.get(), 0);
    }

    #[test]
    fn some_calls_always_ask() {
        // Even with proc_run allowed and the command pre-approved.
        let o = orion("[tools]\nproc_run = \"allow\"\n[proc]\nallowed_commands = [\"*\"]\n");
        let outside = json!({ "command": "true", "sandbox": false });
        assert!(matches!(o.call("proc_run", outside.clone(), None), Err(CallError::Denied(r)) if r.contains("needs interactive approval")));
        let yes = Answer { yes: true, asked: Cell::new(0) };
        let done = o.call("proc_run", outside, Some(&yes)).unwrap();
        assert_eq!((yes.asked.get(), done["sandbox"].as_str()), (1, Some("off")));
        assert!(o.call("proc_run", json!({ "command": "true" }), Some(&yes)).is_ok());
        assert_eq!(yes.asked.get(), 1, "sandboxed, it stays pre-approved");

        let off = orion("[proc]\nunsandboxed = false\n");
        let r = off.call("proc_run", json!({ "command": "true", "sandbox": false }), Some(&yes));
        assert!(r.unwrap_err().to_string().contains("unsandboxed = false"));
    }

    #[test]
    fn allow_and_deny_never_prompt() {
        let o = orion("[tools]\nproc_run = \"allow\"\nsystem_info = \"deny\"\n[proc]\nallowed_commands = [\"true\"]\n");
        let yes = Answer { yes: true, asked: Cell::new(0) };
        assert!(o.call("proc_run", json!({ "command": "true" }), Some(&yes)).is_ok());
        assert!(o.call("proc_run", json!({ "command": "echo" }), Some(&yes)).is_err());
        assert!(o.call("system_info", json!({}), Some(&yes)).is_err());
        assert_eq!(yes.asked.get(), 0);
    }
}

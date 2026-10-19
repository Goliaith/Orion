//! Orion is the interface between an AI model and a Linux machine.
//!
//! The machine is exposed as a set of typed [`Tool`]s. Every call goes through
//! a [`Policy`] (allow, ask or deny, per tool and per risk level, with
//! filesystem and command scopes) and is written to an append-only audit log.
//! The tools are served over the Model Context Protocol (see [`mcp`]), so any
//! MCP-capable agent, whether hosted or local, can drive the machine through the
//! same contract. [`chat`] is the human-facing front-end on top of it.

pub mod audit;
pub mod chat;
pub mod mcp;
pub mod policy;
pub mod tool;
pub mod tools;

use std::fmt;
use std::time::Instant;

use serde_json::Value;

pub use audit::Audit;
pub use policy::{Decision, Policy};
pub use tool::{Preapproval, Risk, Tool};

/// Why a tool call did not produce a result.
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
}

/// The tool registry with its policy and audit log attached.
pub struct Orion {
    policy: Policy,
    tools: Vec<Box<dyn Tool>>,
    audit: Audit,
}

impl Orion {
    pub fn new(policy: Policy) -> Self {
        let audit = Audit::new(policy.audit_path());
        Self { tools: tools::builtin(), policy, audit }
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
            (_, _) if let Err(e) = precheck() => Err(e),
            (Decision::Allow, Preapproval::Refused(reason)) => Err(reason),
            (Decision::Allow, _) | (Decision::Ask, Preapproval::Granted) => Ok(()),
            (Decision::Ask, _) => match approver {
                Some(approver) => {
                    let yes = approver.approve(tool.as_ref(), &args);
                    approved = Some(yes);
                    if yes { Ok(()) } else { Err(format!("the user declined this {name} call")) }
                }
                None => Err(format!("tool {name} needs interactive approval, which is not available here")),
            },
        };
        let start = Instant::now();
        let result = match verdict {
            Ok(()) => tool.call(&self.policy, args.clone()).map_err(CallError::Failed),
            Err(reason) => Err(CallError::Denied(reason)),
        };
        self.audit.record(audit::Entry {
            tool: name,
            decision,
            approved,
            args: &args,
            error: result.as_ref().err().map(ToString::to_string),
            duration: start.elapsed(),
        });
        result
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use serde_json::json;

    use super::*;

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
    fn allow_and_deny_never_prompt() {
        let o = orion("[tools]\nproc_run = \"allow\"\nsystem_info = \"deny\"\n[proc]\nallowed_commands = [\"true\"]\n");
        let yes = Answer { yes: true, asked: Cell::new(0) };
        assert!(o.call("proc_run", json!({ "command": "true" }), Some(&yes)).is_ok());
        assert!(o.call("proc_run", json!({ "command": "echo" }), Some(&yes)).is_err());
        assert!(o.call("system_info", json!({}), Some(&yes)).is_err());
        assert_eq!(yes.asked.get(), 0);
    }
}

//! Orion is the interface between an AI model and a Linux machine.
//!
//! The machine is exposed as a set of typed [`Tool`]s. Every call goes through
//! a [`Policy`] (allow or deny, per tool and per risk level, with filesystem and
//! command scopes) and is written to an append-only audit log. The tools are
//! served over the Model Context Protocol (see [`mcp`]), so any MCP-capable
//! agent, whether hosted or local, can drive the machine through the same contract.

pub mod audit;
pub mod mcp;
pub mod policy;
pub mod tool;
pub mod tools;

use std::fmt;
use std::time::Instant;

use serde_json::Value;

pub use audit::Audit;
pub use policy::{Decision, Policy};
pub use tool::{Risk, Tool};

/// Why a tool call did not produce a result.
#[derive(Debug)]
pub enum CallError {
    Unknown(String),
    Denied(String),
    Failed(anyhow::Error),
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallError::Unknown(name) => write!(f, "unknown tool: {name}"),
            CallError::Denied(name) => write!(f, "tool {name} is denied by the Orion policy"),
            CallError::Failed(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for CallError {}

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

    pub fn call(&self, name: &str, args: Value) -> Result<Value, CallError> {
        let tool = self
            .tools
            .iter()
            .find(|t| t.name() == name)
            .ok_or_else(|| CallError::Unknown(name.to_string()))?;
        let decision = self.policy.decide(name, tool.risk());
        let start = Instant::now();
        let result = match decision {
            Decision::Allow => tool.call(&self.policy, args.clone()).map_err(CallError::Failed),
            Decision::Deny => Err(CallError::Denied(name.to_string())),
        };
        self.audit.record(audit::Entry {
            tool: name,
            decision,
            args: &args,
            error: result.as_ref().err().map(ToString::to_string),
            duration: start.elapsed(),
        });
        result
    }
}

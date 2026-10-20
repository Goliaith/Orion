use anyhow::{anyhow, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::policy::Policy;

/// How much a tool can change the machine. The policy's per-risk defaults
/// apply to any tool that has no explicit entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    /// Observes state only.
    Read,
    /// Changes the machine: files, services, loaded models.
    Write,
    /// Runs arbitrary programs.
    Execute,
}

impl Risk {
    pub fn as_str(self) -> &'static str {
        match self {
            Risk::Read => "read",
            Risk::Write => "write",
            Risk::Execute => "execute",
        }
    }
}

/// A capability exposed to the AI.
///
/// Names must match `^[a-zA-Z0-9_-]{1,64}$` so they pass through every model
/// API unchanged.
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn risk(&self) -> Risk;
    /// JSON Schema for the `arguments` object.
    fn input_schema(&self) -> Value;
    fn call(&self, policy: &Policy, args: Value) -> Result<Value>;

    /// Whether the policy pre-approves this particular call, for tools whose
    /// policy depends on their arguments (such as `proc_run`'s command list).
    fn preapproval(&self, _policy: &Policy, _args: &Value) -> Preapproval {
        Preapproval::NotApplicable
    }

    /// The arguments as they may be shown and logged: secrets (a Wi-Fi
    /// password) blanked out. Used for the audit log, approval prompts and
    /// tool-call displays.
    fn redact(&self, args: &Value) -> Value {
        args.clone()
    }

    /// Hard checks that no approval can override (such as path scopes). They
    /// run before the user is asked, so nobody approves a call that would fail.
    fn precheck(&self, _policy: &Policy, _args: &Value) -> Result<()> {
        Ok(())
    }
}

pub enum Preapproval {
    /// The tool has no per-call rule; the tool-level decision stands.
    NotApplicable,
    /// Runs under `allow` or `ask` without a prompt.
    Granted,
    /// Refused under `allow`; under `ask` a human can still approve it.
    Refused(String),
}

/// Deserialize tool arguments; a missing/null argument object counts as `{}`.
pub fn parse_args<T: DeserializeOwned>(args: Value) -> Result<T> {
    let args = if args.is_null() { Value::Object(Default::default()) } else { args };
    serde_json::from_value(args).map_err(|e| anyhow!("invalid arguments: {e}"))
}

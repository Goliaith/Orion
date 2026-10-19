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
    /// Modifies files.
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
}

/// Deserialize tool arguments; a missing/null argument object counts as `{}`.
pub fn parse_args<T: DeserializeOwned>(args: Value) -> Result<T> {
    let args = if args.is_null() { Value::Object(Default::default()) } else { args };
    serde_json::from_value(args).map_err(|e| anyhow!("invalid arguments: {e}"))
}

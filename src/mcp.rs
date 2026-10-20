//! Model Context Protocol server: newline-delimited JSON-RPC 2.0 over stdio.
//!
//! Only the tools capability is implemented. Served on its own (`orion
//! serve --direct`) there is no human to answer approval prompts, so only
//! `allow` tools are listed. Through the daemon, a person can approve from
//! `orion approvals`, a notification or the shell, so `ask` tools are
//! listed too (see [`crate::approvals`]).

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use serde_json::{json, Value};

use crate::policy::Decision;
use crate::tool::Risk;
use crate::{Approver, CallError, Orion};

/// Newest first; the first entry is offered when the client asks for an
/// unknown version.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "Orion exposes this Linux machine as tools. A local policy governs every call \
and every call is audited. File paths must be absolute or start with ~/. proc_run runs a single program \
without a shell. If a tool is missing from the list, the policy has disabled it or requires interactive \
approval and nobody can approve here. Tools that say they ask for approval wait until a person answers; \
if the answer is no, don't retry the same call.";

const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const PARSE_ERROR: i64 = -32700;

/// The current Orion, swappable while connections are open (the daemon
/// reloads its policy this way).
pub type Shared = Arc<RwLock<Arc<Orion>>>;

/// Extra methods a host adds (the daemon's `orion/status`).
pub type Extension = Arc<dyn Fn(&str, &Value) -> Option<Value> + Send + Sync>;

pub struct Server {
    orion: Shared,
    /// Who is calling, for the audit log: the client's name from
    /// `initialize`, plus whatever the host knows (a process id).
    peer: String,
    client: Mutex<Option<String>>,
    extension: Option<Extension>,
    /// List `ask` tools too: a person can approve them.
    offer_ask: bool,
}

impl Server {
    pub fn new(orion: Orion) -> Self {
        Self::shared(Arc::new(RwLock::new(Arc::new(orion))), String::new(), None)
    }

    pub fn shared(orion: Shared, peer: String, extension: Option<Extension>) -> Self {
        Self { orion, peer, client: Mutex::new(None), extension, offer_ask: false }
    }

    /// Also list the tools that need approval (someone can give it).
    pub fn offering_ask(mut self, yes: bool) -> Self {
        self.offer_ask = yes;
        self
    }

    fn orion(&self) -> Arc<Orion> {
        self.orion.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn caller(&self) -> Option<String> {
        let client = self.client.lock().unwrap_or_else(|e| e.into_inner()).clone();
        match (client, self.peer.as_str()) {
            (Some(c), "") => Some(c),
            (Some(c), p) => Some(format!("{c} ({p})")),
            (None, "") => None,
            (None, p) => Some(p.to_string()),
        }
    }

    /// Serve until `input` reaches EOF.
    pub fn serve(&self, input: impl BufRead, mut output: impl Write) -> Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let reply = match serde_json::from_str::<Value>(&line) {
                Ok(msg) => self.handle(msg),
                Err(e) => Some(error(Value::Null, PARSE_ERROR, &format!("parse error: {e}"))),
            };
            if let Some(reply) = reply {
                serde_json::to_writer(&mut output, &reply)?;
                output.write_all(b"\n")?;
                output.flush()?;
            }
        }
        Ok(())
    }

    /// Handle one message; `None` for notifications and responses.
    pub fn handle(&self, msg: Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            // A response to a request we never send, or garbage.
            return match id {
                Some(id) if msg.get("result").is_none() && msg.get("error").is_none() => {
                    Some(error(id, INVALID_REQUEST, "missing method"))
                }
                _ => None,
            };
        };
        let id = id?;
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        tracing::debug!(method, "request");
        let result = match method {
            "initialize" => Ok(self.initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(self.list_tools()),
            "tools/call" => self.call_tool(&params, None),
            other => match self.extension.as_ref().and_then(|x| x(other, &params)) {
                Some(v) => Ok(v),
                None => Err((METHOD_NOT_FOUND, format!("method not found: {method}"))),
            },
        };
        Some(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => error(id, code, &message),
        })
    }

    fn initialize(&self, params: &Value) -> Value {
        if let Some(name) = params.pointer("/clientInfo/name").and_then(Value::as_str) {
            let name: String = name.chars().filter(|c| !c.is_control()).take(64).collect();
            *self.client.lock().unwrap_or_else(|e| e.into_inner()) = Some(name);
        }
        let requested = params.get("protocolVersion").and_then(Value::as_str);
        let version = requested
            .and_then(|v| PROTOCOL_VERSIONS.iter().find(|s| **s == v))
            .unwrap_or(&PROTOCOL_VERSIONS[0]);
        json!({
            "protocolVersion": version,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "orion", "version": env!("CARGO_PKG_VERSION") },
            "instructions": INSTRUCTIONS,
        })
    }

    fn list_tools(&self) -> Value {
        let orion = self.orion();
        let tools: Vec<Value> = orion
            .tools()
            .filter(|(_, d)| *d == Decision::Allow || (self.offer_ask && *d == Decision::Ask))
            .map(|(t, d)| {
                let description = if d == Decision::Ask {
                    format!("{} (Each call waits for a person to approve it.)", t.description())
                } else {
                    t.description().to_string()
                };
                json!({
                    "name": t.name(),
                    "description": description,
                    "inputSchema": t.input_schema(),
                    "annotations": {
                        "readOnlyHint": t.risk() == Risk::Read,
                        "destructiveHint": t.risk() != Risk::Read,
                        "openWorldHint": false,
                    },
                })
            })
            .collect();
        json!({ "tools": tools })
    }

    /// Who a call is from, for the audit log: `_meta["orion/caller"]` (a
    /// chat names the person, say) via the connection's own name.
    pub fn call_caller(&self, params: &Value) -> Option<String> {
        let named = params.pointer("/_meta/orion~1caller").and_then(Value::as_str);
        let named = named.map(|n| n.chars().filter(|c| !c.is_control()).take(80).collect::<String>());
        match (named, self.caller()) {
            (Some(n), Some(c)) => Some(format!("{n} via {c}")),
            (n, c) => n.or(c),
        }
    }

    /// `tools/call`, with `approver` answering `ask` decisions. The result
    /// says in `_meta["orion/error"]` whether a failed call was refused or
    /// went wrong, for clients that care (a chat through the daemon).
    pub fn call_tool(&self, params: &Value, approver: Option<&dyn Approver>) -> Result<Value, (i64, String)> {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Err((INVALID_PARAMS, "missing tool name".into()));
        };
        let args = params.get("arguments").cloned().unwrap_or(Value::Null);
        let failed = |kind: &str, text: String| {
            json!({ "content": [{ "type": "text", "text": text }], "isError": true, "_meta": { "orion/error": kind } })
        };
        match self.orion().call_as(name, args, approver, self.call_caller(params).as_deref()) {
            Ok(value) => {
                let text = serde_json::to_string_pretty(&value).unwrap_or_default();
                Ok(json!({ "content": [{ "type": "text", "text": text }], "isError": false }))
            }
            Err(e @ CallError::Unknown(_)) => Err((INVALID_PARAMS, e.to_string())),
            Err(e @ CallError::Denied(_)) => Ok(failed("denied", e.to_string())),
            Err(CallError::Failed(e)) if crate::interrupt::is_interrupt(&e) => Ok(failed("interrupted", format!("{e:#}"))),
            Err(e) => Ok(failed("failed", e.to_string())),
        }
    }
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Policy;

    fn server(policy: &str) -> Server {
        Server::new(Orion::new(Policy::from_toml(&format!("{policy}\n[audit]\nenabled = false\n")).unwrap()))
    }

    fn request(s: &Server, method: &str, params: Value) -> Value {
        s.handle(json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })).unwrap()
    }

    #[test]
    fn negotiates_version() {
        let s = server("");
        let r = request(&s, "initialize", json!({ "protocolVersion": "2025-03-26" }));
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        let r = request(&s, "initialize", json!({ "protocolVersion": "1999-01-01" }));
        assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
        assert!(s.handle(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).is_none());
    }

    #[test]
    fn lists_only_allowed_tools() {
        let s = server("[tools]\ngpu_info = \"deny\"\nfs_write = \"allow\"\n");
        let r = request(&s, "tools/list", Value::Null);
        let names: Vec<&str> = r["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"system_info"));
        assert!(names.contains(&"fs_write"));
        assert!(!names.contains(&"gpu_info"));
        assert!(!names.contains(&"proc_run"));
    }

    #[test]
    fn call_results() {
        let s = server("");
        let ok = request(&s, "tools/call", json!({ "name": "system_info", "arguments": {} }));
        assert_eq!(ok["result"]["isError"], false);
        let text = ok["result"]["content"][0]["text"].as_str().unwrap();
        assert!(serde_json::from_str::<Value>(text).unwrap()["arch"].is_string());

        let denied = request(&s, "tools/call", json!({ "name": "proc_run", "arguments": { "command": "id" } }));
        assert_eq!(denied["result"]["isError"], true);

        let unknown = request(&s, "tools/call", json!({ "name": "nope" }));
        assert_eq!(unknown["error"]["code"], INVALID_PARAMS);

        let missing = request(&s, "resources/list", Value::Null);
        assert_eq!(missing["error"]["code"], METHOD_NOT_FOUND);
    }
}

//! `orion chat`: the human-facing front-end. You talk to Claude in the
//! terminal; Claude works on the machine through the same policy-checked
//! tools that `orion serve` exposes, and `ask` decisions come to you as
//! approval prompts.

pub mod claude;
pub mod terminal;

use std::cell::RefCell;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{Approver, Decision, Orion, Policy, Tool};
use claude::{Client, Event, Usage};

/// `[chat]` in orion.toml.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChatConfig {
    pub model: String,
    /// `low`, `medium`, `high`, `xhigh` or `max`.
    pub effort: String,
    pub max_tokens: u32,
    /// `updates` shows the model's progress notes between tool calls,
    /// `summarized` shows a summary of its reasoning, `omitted` shows nothing.
    pub thinking_display: String,
    /// Let the API retry a declined request on another model.
    pub fallbacks: bool,
    /// Model calls per user message before the turn is stopped.
    pub max_tool_rounds: usize,
    pub base_url: String,
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            model: "claude-opus-5-5".into(),
            effort: "high".into(),
            max_tokens: 64000,
            thinking_display: "updates".into(),
            fallbacks: true,
            max_tool_rounds: 50,
            base_url: "https://api.anthropic.com".into(),
        }
    }
}

/// What the session needs from whoever is showing it.
pub trait Ui {
    fn text(&mut self, text: &str);
    fn thinking(&mut self, text: &str);
    fn tool_start(&mut self, name: &str, input: &Value);
    fn tool_end(&mut self, name: &str, result: &Result<Value, String>, took: Duration);
    fn notice(&mut self, text: &str);
    /// Answer an `ask` decision.
    fn approve(&mut self, tool: &dyn Tool, args: &Value) -> bool;
}

struct UiApprover<'a>(RefCell<&'a mut dyn Ui>);

impl Approver for UiApprover<'_> {
    fn approve(&self, tool: &dyn Tool, args: &Value) -> bool {
        self.0.borrow_mut().approve(tool, args)
    }
}

/// One conversation. The history is append-only, so thinking blocks and the
/// prompt cache stay valid from turn to turn.
pub struct Session<'a> {
    orion: &'a Orion,
    client: Client,
    pub config: ChatConfig,
    system: String,
    tools: Vec<Value>,
    messages: Vec<Value>,
    pub usage: Usage,
}

impl<'a> Session<'a> {
    pub fn new(orion: &'a Orion, client: Client) -> Self {
        let config = orion.policy().chat.clone();
        // Denied tools are never offered. `ask` tools are, because a human is here.
        let tools = orion
            .tools()
            .filter(|(_, d)| *d != Decision::Deny)
            .map(|(t, _)| {
                json!({
                    "name": t.name(),
                    "description": t.description(),
                    "input_schema": t.input_schema(),
                    "eager_input_streaming": true,
                })
            })
            .collect();
        Self { orion, client, config, system: system_prompt(orion.policy()), tools, messages: Vec::new(), usage: Usage::default() }
    }

    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }

    pub fn clear(&mut self) {
        self.messages.clear();
    }

    fn request_body(&self) -> Value {
        let mut body = json!({
            "model": self.config.model,
            "max_tokens": self.config.max_tokens,
            "stream": true,
            "system": self.system,
            "tools": self.tools,
            "messages": self.messages,
            "thinking": { "type": "adaptive", "display": self.config.thinking_display },
            "output_config": { "effort": self.config.effort },
            // Caches the growing conversation prefix between requests.
            "cache_control": { "type": "ephemeral" },
        });
        if self.config.fallbacks {
            body["fallbacks"] = json!("default");
        }
        body
    }

    fn betas(&self) -> Vec<&'static str> {
        let mut betas = Vec::new();
        if self.config.fallbacks {
            betas.push("server-side-fallback-2026-07-01");
        }
        if self.config.thinking_display == "updates" {
            betas.push("thinking-display-updates-2026-08-18");
        }
        betas
    }

    /// Send one user message and run the tool loop until the model is done.
    pub fn turn(&mut self, input: &str, ui: &mut dyn Ui) -> Result<()> {
        self.messages.push(json!({ "role": "user", "content": input }));
        for _ in 0..self.config.max_tool_rounds {
            let msg = self.client.stream(&self.request_body(), &self.betas(), &mut |ev| match ev {
                Event::Text(t) => ui.text(t),
                Event::Thinking(t) => ui.thinking(t),
                Event::ToolUse(_) => {}
            })?;
            self.usage.add(&msg.usage);
            let stop = msg.stop_reason.as_deref().unwrap_or_default();
            if stop == "refusal" {
                // A partial reply is discarded, not kept in the history.
                ui.notice("The model declined this request.");
                return Ok(());
            }

            let content = echo_content(msg.content);
            let calls: Vec<Value> = content.iter().filter(|b| b["type"] == "tool_use").cloned().collect();
            if !content.is_empty() {
                self.messages.push(json!({ "role": "assistant", "content": content }));
            }
            if calls.is_empty() {
                if stop == "max_tokens" {
                    ui.notice("The reply hit max_tokens and was cut off.");
                }
                return Ok(());
            }

            // Every tool_use gets a result, all in a single user message.
            let results: Vec<Value> = calls
                .iter()
                .map(|call| {
                    let id = call["id"].as_str().unwrap_or_default();
                    let (text, is_error) = if let Some(raw) = msg.invalid_tool_inputs.get(id) {
                        (json!({ "INVALID_JSON": raw }).to_string(), true)
                    } else if stop == "max_tokens" {
                        ("Not run: the tool input was cut off by max_tokens.".to_string(), true)
                    } else {
                        self.run_tool(call, ui)
                    };
                    json!({ "type": "tool_result", "tool_use_id": id, "content": text, "is_error": is_error })
                })
                .collect();
            self.messages.push(json!({ "role": "user", "content": results }));
        }
        ui.notice(&format!("Stopped after {} model calls in one turn.", self.config.max_tool_rounds));
        Ok(())
    }

    fn run_tool(&self, call: &Value, ui: &mut dyn Ui) -> (String, bool) {
        let name = call["name"].as_str().unwrap_or_default();
        let input = call["input"].clone();
        ui.tool_start(name, &input);
        let start = Instant::now();
        let result = {
            let approver = UiApprover(RefCell::new(&mut *ui));
            self.orion.call(name, input, Some(&approver)).map_err(|e| e.to_string())
        };
        ui.tool_end(name, &result, start.elapsed());
        match result {
            Ok(v) => (serde_json::to_string_pretty(&v).unwrap_or_default(), false),
            Err(e) => (e, true),
        }
    }
}

/// The blocks of a response that go back into the history. After a
/// mid-response server-side fallback, only text survives from before the last
/// `fallback` marker; the declined model's tool calls and thinking are dropped
/// (and so are never run).
fn echo_content(content: Vec<Value>) -> Vec<Value> {
    let Some(boundary) = content.iter().rposition(|b| b["type"] == "fallback") else { return content };
    content
        .into_iter()
        .enumerate()
        .filter(|(i, b)| *i >= boundary || b["type"] == "text")
        .map(|(_, b)| b)
        .collect()
}

fn system_prompt(policy: &Policy) -> String {
    let list = |items: Vec<String>| if items.is_empty() { "none".to_string() } else { items.join(", ") };
    let paths = |roots: &[std::path::PathBuf]| list(roots.iter().map(|r| r.display().to_string()).collect());
    let read = |p: &str| std::fs::read_to_string(p).map(|s| s.trim().to_string()).unwrap_or_default();
    let host = read("/proc/sys/kernel/hostname");
    let os = read("/etc/os-release")
        .lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim_matches('"').to_string())
        .unwrap_or_else(|| "Linux".into());
    format!(
        "You are the resident AI of Orion, an AI-first operating system. You are running on the \
machine \"{host}\" ({os}) and talking with its owner in a terminal.

You act on the machine only through the tools provided. The owner's policy checks every call, and \
every call is written to an audit log. Some calls stop and ask the owner for approval. If they \
decline one, don't try to get around it; ask what they would prefer instead.

File paths must be absolute or start with ~/. proc_run starts one program with arguments and \
no shell, so pipes, globs and redirects don't work. Look before you change things, and say what \
you are about to change. Keep replies short and plain: this is a terminal, so use little or no \
Markdown.

Policy limits: you can read under {read}. You can write under {write}; writes elsewhere fail and \
no approval can override that. Programs that run without asking: {commands}.",
        read = paths(&policy.fs.read_roots),
        write = paths(&policy.fs.write_roots),
        commands = list(policy.proc.allowed_commands.clone()),
    )
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    use super::*;
    use crate::Policy;

    #[derive(Default)]
    struct Recorder {
        out: String,
        approvals: u32,
    }

    impl Ui for Recorder {
        fn text(&mut self, text: &str) {
            self.out.push_str(text);
        }
        fn thinking(&mut self, text: &str) {
            self.out.push_str(&format!("[{text}]"));
        }
        fn tool_start(&mut self, name: &str, _input: &Value) {
            self.out.push_str(&format!("<{name}>"));
        }
        fn tool_end(&mut self, _name: &str, result: &Result<Value, String>, _took: Duration) {
            self.out.push_str(if result.is_ok() { "<ok>" } else { "<err>" });
        }
        fn notice(&mut self, text: &str) {
            self.out.push_str(&format!("!{text}"));
        }
        fn approve(&mut self, _tool: &dyn Tool, _args: &Value) -> bool {
            self.approvals += 1;
            false
        }
    }

    fn sse(events: &[Value]) -> String {
        events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())).collect()
    }

    /// Serve each canned SSE body in turn; send each request body back.
    fn fake_api(responses: Vec<String>) -> (String, mpsc::Receiver<Value>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for body in responses {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut len = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                }
                let mut req = vec![0; len];
                reader.read_exact(&mut req).unwrap();
                tx.send(serde_json::from_slice(&req).unwrap()).unwrap();
                let mut stream = stream;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        (url, rx)
    }

    #[test]
    fn tool_loop_round_trip() {
        let first = sse(&[
            json!({"type":"message_start","message":{"model":"claude-opus-5-5","usage":{"input_tokens":100}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Checking the host."}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"abc"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"system_info","input":{}}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t2","name":"fs_write","input":{}}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"/tmp/orion-test-root/x\",\"content\":\"hi\"}"}}),
            json!({"type":"content_block_stop","index":2}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":20}}),
            json!({"type":"message_stop"}),
        ]);
        let second = sse(&[
            json!({"type":"message_start","message":{"usage":{"input_tokens":300}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Done."}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}),
            json!({"type":"message_stop"}),
        ]);
        let (url, requests) = fake_api(vec![first, second]);
        let orion =
            Orion::new(Policy::from_toml("[fs]\nwrite_roots = [\"/tmp/orion-test-root\"]\n[audit]\nenabled = false\n").unwrap());
        let mut session = Session::new(&orion, Client::with_api_key(&url, "test"));
        let mut ui = Recorder::default();
        session.turn("what machine is this?", &mut ui).unwrap();

        assert_eq!(ui.out, "[Checking the host.]<system_info><ok><fs_write><err>Done.");
        assert_eq!(ui.approvals, 1, "fs_write is `ask` by default");
        assert_eq!(session.usage.input_tokens, 400);
        assert_eq!(session.usage.output_tokens, 25);

        let req1 = requests.recv().unwrap();
        assert_eq!(req1["model"], "claude-opus-5-5");
        assert_eq!(req1["fallbacks"], "default");
        let offered: Vec<&str> = req1["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(offered.contains(&"proc_run") && offered.contains(&"fs_write"));

        let req2 = requests.recv().unwrap();
        let msgs = req2["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        // The thinking block goes back unchanged, signature included.
        assert_eq!(msgs[1]["content"][0], json!({"type":"thinking","thinking":"Checking the host.","signature":"abc"}));
        let results = msgs[2]["content"].as_array().unwrap();
        assert_eq!(results[0]["tool_use_id"], "t1");
        assert_eq!(results[0]["is_error"], false);
        assert_eq!(results[1]["is_error"], true);
        assert!(results[1]["content"].as_str().unwrap().contains("declined"));
        assert_eq!(session.messages.len(), 4);
    }

    #[test]
    fn fallback_echo_drops_declined_partial() {
        let content = vec![
            json!({"type":"thinking","thinking":"","signature":"a"}),
            json!({"type":"text","text":"Partial"}),
            json!({"type":"tool_use","id":"t1","name":"fs_read","input":{}}),
            json!({"type":"fallback","from":{"model":"x"},"to":{"model":"y"}}),
            json!({"type":"thinking","thinking":"","signature":"b"}),
            json!({"type":"text","text":" continued"}),
        ];
        let kept: Vec<Value> = echo_content(content);
        let types: Vec<&str> = kept.iter().map(|b| b["type"].as_str().unwrap()).collect();
        assert_eq!(types, ["text", "fallback", "thinking", "text"]);
        assert_eq!(kept[2]["signature"], "b");
    }
}

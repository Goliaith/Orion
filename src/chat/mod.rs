//! `orion chat`: the human-facing front-end. You talk to a model in the
//! terminal; it works on the machine through the same policy-checked tools
//! that `orion serve` exposes, and `ask` decisions come to you as approval
//! prompts.
//!
//! The model is behind [`Backend`]: [`claude`] for the Claude API, [`local`]
//! for models served on this machine (Ollama, llama.cpp, vLLM, LM Studio).
//! In `auto` mode the [`router`] picks one for each message, and the
//! [`Session`] keeps both in one conversation.

mod agent;
mod classifier;
pub mod claude;
pub(crate) mod http;
mod machine;
pub mod local;
pub mod router;
mod session;
pub mod shell;
pub mod store;
pub mod terminal;
mod tui;

pub use session::Session;

use std::cell::RefCell;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Approver, Decision, Orion, Policy, Tool};

/// `[chat]` in orion.toml.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChatConfig {
    /// `auto` routes each message; `claude` or `local` pins one.
    #[serde(rename = "provider")]
    pub mode: Mode,
    /// Model calls per user message before the turn is stopped.
    pub max_tool_rounds: usize,
    /// Show reasoning and progress notes (Ctrl-O toggles it in the chat).
    pub show_reasoning: bool,
    /// Save conversations after every message so they can be resumed.
    pub save_conversations: bool,
    /// `tui` (full screen) or `plain` (line by line). Plain is also used
    /// whenever input or output is not a terminal.
    pub interface: Interface,
    pub claude: claude::ClaudeConfig,
    pub local: local::LocalConfig,
    pub router: router::RouterConfig,
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Auto,
            max_tool_rounds: 50,
            show_reasoning: true,
            save_conversations: true,
            interface: Interface::Tui,
            claude: Default::default(),
            local: Default::default(),
            router: Default::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Interface {
    #[default]
    Tui,
    Plain,
}

/// Where the model behind a message comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Route each message to Claude or the local model.
    #[default]
    Auto,
    Claude,
    Local,
}

/// What the UI hears while a reply streams in.
pub enum Event<'a> {
    Text(&'a str),
    /// Reasoning or progress notes, shown apart from the answer.
    Thinking(&'a str),
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
    }
}

/// A tool as offered to the model.
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Value,
}

pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
    /// The raw arguments, when they were not a valid JSON object. Such a
    /// call is answered with an error and never run.
    pub invalid_input: Option<String>,
}

pub struct ToolResult {
    pub id: String,
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Stop {
    /// The model finished its reply.
    Done,
    /// The model wants the tool calls run.
    ToolUse,
    /// The reply hit the output limit; any tool calls in it may be cut off.
    Truncated,
    /// The model declined. Nothing from this reply was kept in the history.
    Refused,
}

pub struct Reply {
    pub calls: Vec<ToolCall>,
    pub stop: Stop,
}

/// A model and the conversation history in that model's own format. A
/// backend appends its replies to the history itself, so the session never
/// rewrites what the model said.
pub trait Backend: Send {
    /// One line for the banner, such as the model and its settings.
    fn describe(&self) -> String;
    fn model(&self) -> String;
    fn set_model(&mut self, model: &str);
    fn set_effort(&mut self, _effort: &str) -> Result<(), String> {
        Err("this backend has no effort setting".into())
    }
    fn list_models(&self) -> Result<Vec<String>> {
        anyhow::bail!("this backend cannot list models")
    }
    fn push_user(&mut self, text: &str);
    /// Record an assistant turn this backend did not produce (one the other
    /// model handled), after the matching `push_user`.
    fn push_assistant_text(&mut self, text: &str);
    fn respond(&mut self, on: &mut dyn FnMut(Event)) -> Result<Reply>;
    /// Answers to every call of the last reply, in order.
    fn push_tool_results(&mut self, results: Vec<ToolResult>);
    fn clear(&mut self);
    /// The conversation history in this backend's wire format, for saving.
    fn export_history(&self) -> Value;
    /// Replace the history with one from `export_history`.
    fn import_history(&mut self, history: Value) -> Result<()>;
    fn usage(&self) -> Usage;
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
    /// Which model is taking the message, and why.
    fn route(&mut self, _to: &str, _model: &str, _reason: &str) {}
    /// A yes/no question that is not about a tool call.
    fn confirm(&mut self, _question: &str) -> bool {
        false
    }
}

struct UiApprover<'a>(RefCell<&'a mut dyn Ui>);

impl Approver for UiApprover<'_> {
    fn approve(&self, tool: &dyn Tool, args: &Value) -> bool {
        self.0.borrow_mut().approve(tool, args)
    }
}

/// The tools a model in chat is offered. Denied tools never are. `ask`
/// tools are, because a human is here to answer.
pub fn offered_tools(orion: &Orion) -> Vec<ToolSpec> {
    orion
        .tools()
        .filter(|(_, d)| *d != Decision::Deny)
        .map(|(t, _)| ToolSpec { name: t.name(), description: t.description(), schema: t.input_schema() })
        .collect()
}

/// Build the backend for `provider`, optionally overriding its model.
pub fn connect(orion: &Orion, provider: Provider, model: Option<&str>) -> Result<Box<dyn Backend>> {
    let config = &orion.policy().chat;
    let system = system_prompt(orion.policy());
    let tools = offered_tools(orion);
    Ok(match provider {
        Provider::Claude => {
            let mut b = claude::ClaudeBackend::from_env(config.claude.clone(), system, &tools)?;
            if let Some(m) = model {
                b.set_model(m);
            }
            Box::new(b)
        }
        Provider::Local => {
            let mut c = config.local.clone();
            if let Some(m) = model {
                c.model = m.to_string();
            }
            Box::new(local::LocalBackend::connect(c, system, &tools)?)
        }
    })
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
pub(crate) mod test_support {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    use super::*;

    /// Records what a session shows, and declines every approval.
    #[derive(Default)]
    pub struct Recorder {
        pub out: String,
        pub approvals: u32,
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
        fn route(&mut self, to: &str, _model: &str, reason: &str) {
            self.out.push_str(&format!("→{to}({reason})"));
        }
    }

    pub fn orion() -> Orion {
        Orion::new(
            Policy::from_toml("[fs]\nwrite_roots = [\"/tmp/orion-test-root\"]\n[audit]\nenabled = false\n").unwrap(),
        )
    }

    /// A server that answers each request with the next canned body (SSE or
    /// JSON), and sends each request's path and JSON body back.
    pub fn fake_server(responses: Vec<String>) -> (String, mpsc::Receiver<(String, Value)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for body in responses {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                let path = request_line.split_whitespace().nth(1).unwrap_or_default().to_string();
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
                tx.send((path, serde_json::from_slice(&req).unwrap_or(Value::Null))).unwrap();
                // "STATUS 400 {...}" answers with that status.
                let (status, body) = match body.strip_prefix("STATUS ") {
                    Some(rest) => (rest[..3].to_string(), rest[4..].to_string()),
                    None => ("200".to_string(), body),
                };
                let kind = if body.starts_with('{') { "application/json" } else { "text/event-stream" };
                let mut stream = stream;
                write!(
                    stream,
                    "HTTP/1.1 {status} X\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        (url, rx)
    }
}

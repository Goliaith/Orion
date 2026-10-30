//! The local backend: models served on this machine through the OpenAI-style
//! `/v1/chat/completions` API that Ollama, llama.cpp's `llama-server`, vLLM
//! and LM Studio all provide.

use std::io::BufRead;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::cloud::{self, CloudConfig, CloudRouter, Pick, Target};
use super::http::{secret_env, sse_data, Http, RateLimited, StatusError};
use super::{Backend, Event, Reply, Stop, ToolCall, ToolResult, ToolSpec, Usage};

/// `[chat.local]` in orion.toml.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalConfig {
    /// The server's OpenAI-compatible base URL, ending in `/v1`. Defaults:
    /// Ollama :11434, llama-server :8080, vLLM :8000, LM Studio :1234.
    pub base_url: String,
    /// Empty means the first model the server lists.
    pub model: String,
    /// Output limit per reply; unset leaves it to the server.
    pub max_tokens: Option<u32>,
    pub temperature: Option<f64>,
    /// Environment variable holding an API key, for servers that want one.
    pub api_key_env: String,
    /// How hard a reasoning model thinks before answering, sent as
    /// `reasoning_effort`: Gemini takes `none` (2.5 Flash and Flash-Lite
    /// only), `minimal`, `low`, `medium` or `high`; OpenAI-style servers
    /// `low`, `medium` or `high`. Unset leaves it to the server.
    pub reasoning_effort: Option<String>,
    /// Whether the server runs on this machine (or this network), so it may
    /// see private messages, notes and files. Unset: decided from
    /// `base_url` (loopback and private-network addresses are).
    pub on_machine: Option<bool>,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:11434/v1".into(),
            model: String::new(),
            max_tokens: None,
            temperature: None,
            api_key_env: String::new(),
            reasoning_effort: None,
            on_machine: None,
        }
    }
}

impl LocalConfig {
    /// Whether what is private may be sent to this server. A hosted API
    /// behind the local backend (Gemini) is not on this machine.
    /// Through a proxy, it isn't: the proxy sees everything.
    pub fn on_machine(&self) -> bool {
        self.on_machine.unwrap_or_else(|| is_on_machine(&self.base_url) && !super::http::proxied(&self.base_url))
    }
}

/// Whether `url`'s host is this machine or a private network: localhost,
/// loopback, RFC 1918, link-local or unique-local addresses. Names other
/// than localhost are taken to be elsewhere.
pub fn is_on_machine(url: &str) -> bool {
    use std::net::IpAddr;
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority.rsplit('@').next().unwrap_or_default();
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    }
    .to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified(),
        Ok(IpAddr::V6(ip)) => {
            let first = ip.segments()[0];
            ip.is_loopback() || ip.is_unspecified() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
        Err(_) => false,
    }
}

/// The bearer token header, for servers configured with `api_key_env`.
pub(super) fn auth_headers(config: &LocalConfig) -> Result<Vec<(String, String)>> {
    if config.api_key_env.is_empty() {
        return Ok(Vec::new());
    }
    let key = secret_env(&config.api_key_env)?
        .with_context(|| format!("{} is not set (chat.local.api_key_env)", config.api_key_env))?;
    Ok(vec![("authorization".into(), format!("Bearer {key}"))])
}

pub struct LocalBackend {
    http: Http,
    config: LocalConfig,
    tools: Vec<Value>,
    /// Starts with the system message; append-only.
    messages: Vec<Value>,
    usage: Usage,
    next_call_id: u64,
    /// Other cloud models to switch to when this one is rate limited; only
    /// for a hosted API.
    cloud: Option<CloudRouter>,
    /// Which model wrote each assistant message, by index, once the router
    /// has switched; the rest are `config.model`'s.
    made_by: Vec<(usize, String)>,
}

impl LocalBackend {
    /// Check that the server answers, and pick a model if none is set.
    pub fn connect(mut config: LocalConfig, system: String, tools: &[ToolSpec]) -> Result<Self> {
        let mut backend = Self {
            http: Http::new(&config.base_url, auth_headers(&config)?),
            tools: tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": { "name": t.name, "description": t.description, "parameters": t.schema },
                    })
                })
                .collect(),
            messages: vec![json!({ "role": "system", "content": system })],
            usage: Usage::default(),
            next_call_id: 0,
            config: LocalConfig::default(),
            cloud: None,
            made_by: Vec::new(),
        };
        if config.model.is_empty() {
            let models = backend.list_models().with_context(|| format!("asking {} for its models", config.base_url))?;
            let Some(first) = models.into_iter().next() else {
                bail!("{} has no models; install one first (for Ollama: `ollama pull qwen3`)", config.base_url);
            };
            config.model = first;
        }
        backend.config = config;
        Ok(backend)
    }

    /// Switch to the other cloud models in `cloud` when this one is rate
    /// limited. Nothing changes for a server on this machine, or when there
    /// is no other model with a key.
    pub fn with_cloud(mut self, cloud: &CloudConfig) -> Self {
        if self.config.on_machine() {
            return self;
        }
        let lineup = cloud::lineup(&self.config, cloud, &cloud::key_is_set);
        let targets = lineup
            .into_iter()
            .enumerate()
            .filter_map(|(i, entry)| {
                let http = if i == 0 {
                    self.http.clone()
                } else {
                    let key = secret_env(&entry.api_key_env).ok().flatten()?;
                    Http::new(&entry.base_url, vec![("authorization".into(), format!("Bearer {key}"))])
                };
                Some(Target::new(entry, http))
            })
            .collect();
        self.cloud = CloudRouter::new(targets, std::time::Duration::from_secs(cloud.cooldown_secs));
        self
    }

    /// The cloud router, when there is one.
    pub fn cloud(&self) -> Option<&CloudRouter> {
        self.cloud.as_ref()
    }

    fn list_models_with(http: &Http) -> Result<Vec<String>> {
        let v = http.get_json("/models")?;
        let mut ids: Vec<String> = v["data"]
            .as_array()
            .map(|a| a.iter().filter_map(|m| m["id"].as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        ids.sort();
        Ok(ids)
    }

    fn request_body(&self) -> Value {
        self.request_body_for(&self.config.model, self.config.reasoning_effort.as_deref(), Value::from(self.messages.clone()))
    }

    fn request_body_for(&self, model: &str, effort: Option<&str>, messages: Value) -> Value {
        let mut body = json!({
            "model": model,
            "messages": messages,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if !self.tools.is_empty() {
            body["tools"] = json!(self.tools);
        }
        if let Some(n) = self.config.max_tokens {
            body["max_tokens"] = json!(n);
        }
        if let Some(t) = self.config.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(effort) = effort.filter(|e| !e.is_empty()) {
            body["reasoning_effort"] = json!(effort);
        }
        body
    }

    /// Send the conversation to the configured model or, through the cloud
    /// router, to the best one that isn't rate limited. Returns the stream
    /// and the model answering.
    fn send(&mut self, on: &mut dyn FnMut(Event)) -> Result<(Box<dyn BufRead>, String)> {
        let Some(mut router) = self.cloud.take() else {
            let reader = self
                .http
                .post_stream("/chat/completions", &self.request_body(), &[], on)
                .with_context(|| format!("calling the local model {}", self.config.model))?;
            return Ok((Box::new(reader), self.config.model.clone()));
        };
        let result = self.send_routed(&mut router, on);
        self.cloud = Some(router);
        result
    }

    fn send_routed(&mut self, router: &mut CloudRouter, on: &mut dyn FnMut(Event)) -> Result<(Box<dyn BufRead>, String)> {
        let last = self.made_by.last().map_or(self.config.model.as_str(), |(_, m)| m.as_str()).to_string();
        let mut tried = Vec::new();
        loop {
            let (i, wait) = match router.pick(&tried) {
                Pick::Try(i) => (i, false),
                Pick::Wait(i) => (i, true),
                Pick::None => bail!("no cloud model can take the request (see `orion doctor`)"),
            };
            let target = &router.targets[i];
            let model = target.entry.model.clone();
            if wait {
                on(Event::Notice(&format!("Every cloud model is rate limited; waiting for {}.", target.label())));
            } else if model != last && tried.is_empty() && i == 0 {
                on(Event::Notice(&format!("{} is free again; back to it.", target.label())));
            }
            let messages = cloud::messages_for(&self.messages, &self.made_by, &self.config.model, &model, target.is_gemini());
            let body = self.request_body_for(&model, target.entry.reasoning_effort.as_deref(), Value::from(messages));
            let http = target.http.clone().handing_off(!wait);
            let err = match http.post_stream("/chat/completions", &body, &[], on) {
                Ok(reader) => return Ok((Box::new(reader), model)),
                Err(e) => e,
            };
            tried.push(i);
            let label = router.targets[i].label();
            let next = match router.pick(&tried) {
                Pick::Try(n) => Some(router.targets[n].label()),
                _ => None,
            };
            if let Some(limit) = err.downcast_ref::<RateLimited>() {
                let secs = router.limited(i, limit.retry_after).as_secs();
                if let Some(next) = next {
                    on(Event::Notice(&format!("{label} is rate limited for {secs}s; switching to {next}.")));
                }
                continue;
            }
            // A key turned down or a model that isn't there won't get
            // better this session; anything else is the server's trouble.
            let lasting = err.downcast_ref::<StatusError>().is_some_and(|s| matches!(s.code, 401 | 403 | 404));
            if !lasting || router.targets.iter().enumerate().all(|(n, t)| n == i || t.dead.is_some()) {
                return Err(err.context(format!("calling {label}")));
            }
            let why = format!("{err:#}");
            let then = next.map(|n| format!("; switching to {n}")).unwrap_or_default();
            on(Event::Notice(&format!("{label} can't be used this session ({why}){then}.")));
            router.failed(i, why);
        }
    }
}

impl Backend for LocalBackend {
    fn describe(&self) -> String {
        let here = format!("{} · local at {}", self.config.model, self.http.base());
        let Some(router) = &self.cloud else { return here };
        let others = router.targets.len() - 1;
        match router.next().filter(|t| t.entry.model != self.config.model) {
            Some(t) => format!("{} · standing in for {} (rate limited) · cloud router, {others} more", t.label(), self.config.model),
            None => format!("{here} · cloud router, {others} more"),
        }
    }

    fn model(&self) -> String {
        self.config.model.clone()
    }

    fn set_model(&mut self, model: &str) {
        // Another model the cloud router knows, by name or as
        // "provider:model": it becomes the one asked first, wherever it is.
        if let Some(router) = self.cloud.as_mut() {
            let named = |t: &Target| {
                model == t.entry.model
                    || model.split_once(':').is_some_and(|(p, m)| p.eq_ignore_ascii_case(&t.entry.provider) && m == t.entry.model)
            };
            if let Some(i) = router.targets.iter().skip(1).position(named).map(|i| i + 1) {
                // Replies so far keep their author once `config.model` changes.
                let old = self.config.model.clone();
                for (n, m) in self.messages.iter().enumerate() {
                    if m["role"] == "assistant" && !self.made_by.iter().any(|(at, _)| *at == n) {
                        self.made_by.push((n, old.clone()));
                    }
                }
                self.made_by.sort_by_key(|(at, _)| *at);
                let target = router.targets.remove(i);
                self.config.model = target.entry.model.clone();
                self.config.base_url = target.entry.base_url.clone();
                self.config.api_key_env = target.entry.api_key_env.clone();
                self.config.reasoning_effort = target.entry.reasoning_effort.clone();
                self.http = target.http.clone().handing_off(false);
                router.targets.insert(0, target);
                return;
            }
        }
        self.config.model = model.to_string();
        if let Some(home) = self.cloud.as_mut().and_then(|r| r.targets.first_mut()) {
            home.entry.model = model.to_string();
        }
    }

    fn list_models(&self) -> Result<Vec<String>> {
        let mut ids = Self::list_models_with(&self.http)?;
        // The cloud router's other models, as `/model local` takes them.
        if let Some(router) = &self.cloud {
            ids.extend(router.targets.iter().skip(1).map(|t| format!("{}:{}", t.entry.provider, t.entry.model)));
        }
        Ok(ids)
    }

    fn push_user(&mut self, text: &str) {
        self.messages.push(json!({ "role": "user", "content": text }));
    }

    fn push_assistant_text(&mut self, text: &str) {
        self.messages.push(json!({ "role": "assistant", "content": text }));
    }

    fn respond(&mut self, on: &mut dyn FnMut(Event)) -> Result<Reply> {
        let (reader, model) = self.send(on)?;
        let done = parse_stream(reader, on)?;
        self.usage.add(&done.usage);
        if done.finish.as_deref() == Some("content_filter") {
            return Ok(Reply { calls: Vec::new(), stop: Stop::Refused });
        }

        let mut calls = Vec::new();
        let mut wire_calls = Vec::new();
        for c in done.calls {
            let id = c.id.unwrap_or_else(|| {
                self.next_call_id += 1;
                format!("call_{}", self.next_call_id)
            });
            let (input, invalid_input) = match c.arguments.trim() {
                "" => (json!({}), None),
                raw => match serde_json::from_str::<Value>(raw) {
                    Ok(v @ Value::Object(_)) => (v, None),
                    _ => (json!({}), Some(raw.to_string())),
                },
            };
            // Servers re-parse the arguments in the history, so broken ones go back as {}.
            let arguments = if invalid_input.is_some() { "{}".to_string() } else { c.arguments };
            let mut wire = json!({
                "id": id,
                "type": "function",
                "function": { "name": c.name, "arguments": arguments },
            });
            if let Some(extra) = c.extra_content {
                wire["extra_content"] = extra;
            }
            wire_calls.push(wire);
            calls.push(ToolCall { id, name: c.name, input, invalid_input });
        }
        let mut message = json!({ "role": "assistant", "content": done.content });
        if !wire_calls.is_empty() {
            message["tool_calls"] = json!(wire_calls);
        }
        if self.cloud.is_some() && (model != self.config.model || !self.made_by.is_empty()) {
            self.made_by.push((self.messages.len(), model));
        }
        self.messages.push(message);

        let stop = match done.finish.as_deref() {
            Some("length") => Stop::Truncated,
            // Some servers report "stop" even when they return tool calls.
            _ if !calls.is_empty() => Stop::ToolUse,
            _ => Stop::Done,
        };
        Ok(Reply { calls, stop })
    }

    fn push_tool_results(&mut self, results: Vec<ToolResult>) {
        for r in results {
            let content = if r.is_error { format!("Error: {}", r.content) } else { r.content };
            self.messages.push(json!({ "role": "tool", "tool_call_id": r.id, "content": content }));
        }
    }

    fn clear(&mut self) {
        self.messages.truncate(1);
        self.made_by.clear();
    }

    fn set_system(&mut self, system: &str) {
        self.messages[0] = json!({ "role": "system", "content": system });
    }

    fn export_history(&self) -> Value {
        // The system prompt is rebuilt on resume, so only the rest is saved.
        Value::Array(self.messages[1..].to_vec())
    }

    fn import_history(&mut self, history: Value) -> Result<()> {
        let Value::Array(messages) = history else { bail!("saved local history is not a list") };
        self.messages.truncate(1);
        self.made_by.clear();
        self.next_call_id = messages.len() as u64;
        self.messages.extend(messages);
        Ok(())
    }

    fn usage(&self) -> Usage {
        self.usage
    }
}

#[derive(Debug, Default)]
struct PartialCall {
    id: Option<String>,
    name: String,
    arguments: String,
    /// Opaque data the server wants back with the call, such as Gemini's
    /// `{"google": {"thought_signature": …}}`.
    extra_content: Option<Value>,
}

#[derive(Debug, Default)]
struct Completion {
    /// The visible answer, with any `<think>` sections removed.
    content: String,
    calls: Vec<PartialCall>,
    finish: Option<String>,
    usage: Usage,
}

/// Assemble a streamed chat completion.
fn parse_stream(reader: impl BufRead, on: &mut dyn FnMut(Event)) -> Result<Completion> {
    let mut done = Completion::default();
    let mut think = ThinkSplitter::default();
    fn emit(on: &mut dyn FnMut(Event), done: &mut Completion, pieces: Vec<(bool, String)>) {
        for (thinking, text) in pieces {
            if thinking {
                on(Event::Thinking(&text));
            } else {
                // Drop the blank lines models put between reasoning and answer.
                let text = if done.content.is_empty() { text.trim_start().to_string() } else { text };
                if !text.is_empty() {
                    on(Event::Text(&text));
                    done.content.push_str(&text);
                }
            }
        }
    }
    for data in sse_data(reader) {
        let data = data?;
        if data == "[DONE]" {
            break;
        }
        let chunk: Value = serde_json::from_str(&data).context("malformed stream chunk")?;
        if let Some(err) = chunk.get("error") {
            let msg = err.get("message").and_then(Value::as_str).or(err.as_str()).unwrap_or("unknown");
            bail!("local model error: {msg}");
        }
        if let Some(u) = chunk.get("usage").filter(|u| u.is_object()) {
            done.usage.input_tokens = u["prompt_tokens"].as_u64().unwrap_or(0);
            done.usage.output_tokens = u["completion_tokens"].as_u64().unwrap_or(0);
        }
        let Some(choice) = chunk["choices"].get(0) else { continue };
        let delta = &choice["delta"];
        // llama.cpp and vLLM say reasoning_content; Ollama says reasoning.
        for key in ["reasoning_content", "reasoning"] {
            if let Some(t) = delta[key].as_str().filter(|t| !t.is_empty()) {
                on(Event::Thinking(t));
            }
        }
        if let Some(t) = delta["content"].as_str() {
            let pieces = think.feed(t);
            emit(on, &mut done, pieces);
        }
        for tc in delta["tool_calls"].as_array().into_iter().flatten() {
            let index = match tc["index"].as_u64() {
                Some(i) => i as usize,
                // Without an index, a new id starts a new call.
                None if tc["id"].is_string() || done.calls.is_empty() => done.calls.len(),
                None => done.calls.len() - 1,
            };
            if done.calls.len() <= index {
                done.calls.resize_with(index + 1, PartialCall::default);
            }
            let call = &mut done.calls[index];
            if let Some(id) = tc["id"].as_str().filter(|s| !s.is_empty()) {
                call.id = Some(id.to_string());
            }
            if let Some(extra) = tc.get("extra_content").filter(|v| !v.is_null()) {
                call.extra_content = Some(extra.clone());
            }
            let f = &tc["function"];
            if let Some(name) = f["name"].as_str() {
                call.name.push_str(name);
            }
            match &f["arguments"] {
                Value::String(s) => call.arguments.push_str(s),
                Value::Null => {}
                // A few servers send the arguments as an object.
                other => call.arguments = other.to_string(),
            }
        }
        if let Some(r) = choice["finish_reason"].as_str() {
            done.finish = Some(r.to_string());
        }
    }
    let rest = think.finish();
    emit(on, &mut done, rest);
    done.calls.retain(|c| !c.name.is_empty());
    Ok(done)
}

/// Separates `<think>…</think>` sections, which many local reasoning models
/// write inline, from the answer, even when a tag is split across chunks.
#[derive(Default)]
struct ThinkSplitter {
    in_think: bool,
    pending: String,
}

impl ThinkSplitter {
    /// Pieces of text, each flagged as thinking or not.
    fn feed(&mut self, chunk: &str) -> Vec<(bool, String)> {
        self.pending.push_str(chunk);
        let mut out = Vec::new();
        loop {
            let tag = if self.in_think { "</think>" } else { "<think>" };
            if let Some(pos) = self.pending.find(tag) {
                let before: String = self.pending.drain(..pos).collect();
                self.pending.drain(..tag.len());
                if !before.is_empty() {
                    out.push((self.in_think, before));
                }
                self.in_think = !self.in_think;
                continue;
            }
            // Hold back a suffix that could be the start of the tag.
            let keep = (1..tag.len()).rev().find(|&k| self.pending.ends_with(&tag[..k])).unwrap_or(0);
            let emit_len = self.pending.len() - keep;
            if emit_len > 0 {
                out.push((self.in_think, self.pending.drain(..emit_len).collect()));
            }
            return out;
        }
    }

    fn finish(&mut self) -> Vec<(bool, String)> {
        if self.pending.is_empty() {
            Vec::new()
        } else {
            vec![(self.in_think, std::mem::take(&mut self.pending))]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{fake_server, orion, Recorder};
    use super::super::{offered_tools, Mode, Session};
    use super::*;

    fn chunks(deltas: &[Value]) -> String {
        let mut s: String = deltas.iter().map(|d| format!("data: {d}\n\n")).collect();
        s.push_str("data: [DONE]\n\n");
        s
    }

    fn delta(d: Value) -> Value {
        json!({ "choices": [{ "index": 0, "delta": d, "finish_reason": null }] })
    }

    fn finish(reason: &str) -> Value {
        json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": reason }] })
    }

    #[test]
    fn splits_think_tags_across_chunks() {
        let mut s = ThinkSplitter::default();
        let mut all = Vec::new();
        for c in ["<th", "ink>plan", " it</th", "ink>\n\nAnswer <b>", "x</b>"] {
            all.extend(s.feed(c));
        }
        all.extend(s.finish());
        let thinking: String = all.iter().filter(|p| p.0).map(|p| p.1.as_str()).collect();
        let text: String = all.iter().filter(|p| !p.0).map(|p| p.1.as_str()).collect();
        assert_eq!(thinking, "plan it");
        assert_eq!(text, "\n\nAnswer <b>x</b>");
    }

    #[test]
    fn assembles_parallel_tool_calls() {
        let stream = chunks(&[
            delta(json!({"role":"assistant","reasoning_content":"Need two lookups."})),
            delta(json!({"tool_calls":[{"index":0,"id":"a","type":"function","function":{"name":"system_info","arguments":""}}]})),
            delta(json!({"tool_calls":[{"index":1,"id":"b","type":"function","function":{"name":"fs_list","arguments":"{\"pa"}}]})),
            delta(json!({"tool_calls":[{"index":0,"function":{"arguments":"{}"}}]})),
            delta(json!({"tool_calls":[{"index":1,"function":{"arguments":"th\":\"~\"}"}}]})),
            finish("tool_calls"),
            json!({"choices":[],"usage":{"prompt_tokens":50,"completion_tokens":9}}),
        ]);
        let mut thoughts = String::new();
        let done = parse_stream(stream.as_bytes(), &mut |e| {
            if let Event::Thinking(t) = e {
                thoughts.push_str(t)
            }
        })
        .unwrap();
        assert_eq!(thoughts, "Need two lookups.");
        assert_eq!(done.finish.as_deref(), Some("tool_calls"));
        assert_eq!((done.usage.input_tokens, done.usage.output_tokens), (50, 9));
        assert_eq!(done.calls.len(), 2);
        assert_eq!((done.calls[0].name.as_str(), done.calls[0].arguments.as_str()), ("system_info", "{}"));
        assert_eq!(done.calls[1].arguments, r#"{"path":"~"}"#);
    }

    #[test]
    fn tolerates_ollama_style_whole_calls() {
        // One chunk per call, no index, arguments as an object.
        let stream = chunks(&[
            delta(json!({"content":"","tool_calls":[{"id":"x1","function":{"name":"proc_list","arguments":{"sort":"memory"}}}]})),
            finish("stop"),
        ]);
        let done = parse_stream(stream.as_bytes(), &mut |_| {}).unwrap();
        assert_eq!(done.calls.len(), 1);
        assert_eq!(serde_json::from_str::<Value>(&done.calls[0].arguments).unwrap(), json!({"sort":"memory"}));
    }

    /// A local backend at `home` whose cloud router can switch to `other`.
    fn routed(home: &str, other: &str) -> LocalBackend {
        let orion = orion();
        let config = LocalConfig { base_url: format!("{home}/v1"), model: "home-model".into(), on_machine: Some(false), ..Default::default() };
        let mut backend = LocalBackend::connect(config, "system".into(), &offered_tools(&orion)).unwrap();
        let entry = |model: &str, url: String| cloud::Entry {
            provider: "test".into(),
            model: model.into(),
            base_url: url,
            api_key_env: String::new(),
            reasoning_effort: None,
            speed: 3,
            quality: 3,
        };
        let targets = vec![
            Target::new(entry("home-model", format!("{home}/v1")), backend.http.clone()),
            Target::new(entry("other-model", format!("{other}/v1")), Http::new(&format!("{other}/v1"), Vec::new())),
        ];
        backend.cloud = CloudRouter::new(targets, std::time::Duration::from_secs(60));
        backend
    }

    fn limited(secs: u32) -> String {
        format!(r#"STATUS 429 {{"error":{{"code":429,"details":[{{"retryDelay":"{secs}s"}}]}}}}"#)
    }

    #[test]
    fn rate_limited_requests_switch_models_instead_of_waiting() {
        let call = chunks(&[
            delta(json!({"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"system_info","arguments":"{}"}}]})),
            finish("tool_calls"),
        ]);
        let answer = chunks(&[delta(json!({"content":"Fine."})), finish("stop")]);
        let (home, home_reqs) = fake_server(vec![limited(30)]);
        let (other, other_reqs) = fake_server(vec![call, answer]);
        let orion = orion();
        let backend = routed(&home, &other);
        let mut session = Session::with_backends(&orion, Mode::Local, None, Some(Box::new(backend)));
        let mut ui = Recorder::default();
        session.turn("how is it?", &mut ui).unwrap();

        assert!(ui.out.contains("!home-model (the local model server) is rate limited for 30s; switching to other-model"), "{}", ui.out);
        assert!(!ui.out.contains("waiting"), "no wait: {}", ui.out);
        assert!(ui.out.ends_with("Fine."), "{}", ui.out);
        assert_eq!(home_reqs.recv().unwrap().1["model"], "home-model");
        // The whole turn stays on the other model while home cools down,
        // tool call and result included.
        let (_, first) = other_reqs.recv().unwrap();
        let (_, second) = other_reqs.recv().unwrap();
        assert_eq!(first["model"], "other-model");
        assert_eq!(second["messages"][2]["tool_calls"][0]["id"], "c1");
        assert_eq!(second["messages"][3]["role"], "tool");
        assert!(home_reqs.try_recv().is_err());
    }

    #[test]
    fn waits_as_before_when_every_model_is_limited() {
        let answer = chunks(&[delta(json!({"content":"Fine."})), finish("stop")]);
        let (home, home_reqs) = fake_server(vec![limited(0), answer]);
        let (other, _other_reqs) = fake_server(vec![limited(0)]);
        let orion = orion();
        let mut session = Session::with_backends(&orion, Mode::Local, None, Some(Box::new(routed(&home, &other))));
        let mut ui = Recorder::default();
        session.turn("hi", &mut ui).unwrap();
        assert!(ui.out.contains("switching to other-model"), "{}", ui.out);
        assert!(ui.out.contains("!Every cloud model is rate limited; waiting for home-model"), "{}", ui.out);
        assert!(ui.out.ends_with("Fine."), "{}", ui.out);
        assert_eq!(home_reqs.iter().take(2).count(), 2);
    }

    #[test]
    fn model_switches_to_another_cloud_model_and_back() {
        let answer = || chunks(&[delta(json!({"content":"Fine."})), finish("stop")]);
        let (home, home_reqs) = fake_server(vec![answer(), answer()]);
        let (other, other_reqs) = fake_server(vec![answer()]);
        let orion = orion();
        let mut session = Session::with_backends(&orion, Mode::Local, None, Some(Box::new(routed(&home, &other))));
        let mut ui = Recorder::default();
        session.turn("hi", &mut ui).unwrap();
        let local = super::super::Provider::Local;
        session.backend_mut(local).unwrap().set_model("test:other-model");
        assert!(session.backend(local).unwrap().describe().starts_with("other-model · local at "), "{}", session.backend(local).unwrap().describe());
        session.turn("and now?", &mut ui).unwrap();
        session.backend_mut(local).unwrap().set_model("home-model");
        session.turn("back?", &mut ui).unwrap();
        assert!(!ui.out.contains("rate limited"), "{}", ui.out);
        assert_eq!(home_reqs.recv().unwrap().1["model"], "home-model");
        assert_eq!(other_reqs.recv().unwrap().1["model"], "other-model");
        assert_eq!(home_reqs.recv().unwrap().1["model"], "home-model");
    }

    #[test]
    fn a_fallback_that_is_turned_down_is_left_out() {
        let answer = chunks(&[delta(json!({"content":"Fine."})), finish("stop")]);
        let (home, _home_reqs) = fake_server(vec![limited(0), answer]);
        let (other, _other_reqs) = fake_server(vec![r#"STATUS 401 {"error":{"message":"bad key"}}"#.to_string()]);
        let orion = orion();
        let mut session = Session::with_backends(&orion, Mode::Local, None, Some(Box::new(routed(&home, &other))));
        let mut ui = Recorder::default();
        session.turn("hi", &mut ui).unwrap();
        assert!(ui.out.contains("!Every cloud model is rate limited; waiting for home-model"), "{}", ui.out);
        let backend = session.backend(super::super::Provider::Local).unwrap();
        assert!(backend.describe().contains("cloud router, 1 more"), "{}", backend.describe());
    }

    #[test]
    fn keeps_gemini_thought_signatures() {
        let signature = json!({"google": {"thought_signature": "c2ln"}});
        let first = chunks(&[
            delta(json!({"tool_calls":[{"index":0,"id":"g1","type":"function","extra_content":signature,
                "function":{"name":"system_info","arguments":"{}"}}]})),
            finish("tool_calls"),
        ]);
        let second = chunks(&[delta(json!({"content":"Fine."})), finish("stop")]);
        let (url, requests) = fake_server(vec![first, second]);
        let orion = orion();
        let config = LocalConfig {
            base_url: format!("{url}/v1"),
            model: "gemini".into(),
            reasoning_effort: Some("low".into()),
            ..Default::default()
        };
        let backend = LocalBackend::connect(config, "system".into(), &offered_tools(&orion)).unwrap();
        let mut session = Session::with_backends(&orion, Mode::Local, None, Some(Box::new(backend)));
        session.turn("how is it?", &mut Recorder::default()).unwrap();

        let (_, req1) = requests.recv().unwrap();
        assert_eq!(req1["reasoning_effort"], "low");
        let (_, req2) = requests.recv().unwrap();
        assert_eq!(req2["messages"][2]["tool_calls"][0]["extra_content"], signature);
    }

    #[test]
    fn stream_errors_surface() {
        let stream = "data: {\"error\":{\"message\":\"model ran out of memory\"}}\n\n";
        let err = parse_stream(stream.as_bytes(), &mut |_| {}).unwrap_err();
        assert!(err.to_string().contains("out of memory"));
    }

    #[test]
    fn tool_loop_round_trip() {
        let models = json!({"object":"list","data":[{"id":"qwen3:8b"},{"id":"llama3.2:3b"}]}).to_string();
        let first = chunks(&[
            delta(json!({"content":"<think>Check the host.</think>"})),
            delta(json!({"tool_calls":[{"index":0,"id":"c1","function":{"name":"system_info","arguments":"{}"}}]})),
            delta(json!({"tool_calls":[{"index":1,"id":"c2","function":{"name":"fs_read","arguments":"{\"path\":"}}]})),
            finish("tool_calls"),
        ]);
        let second = chunks(&[delta(json!({"content":"\n\nIt is a Linux box."})), finish("stop")]);
        let (url, requests) = fake_server(vec![models, first, second]);
        let orion = orion();
        let config = LocalConfig { base_url: format!("{url}/v1"), ..Default::default() };
        let backend = LocalBackend::connect(config, "system".into(), &offered_tools(&orion)).unwrap();
        assert!(backend.describe().starts_with("llama3.2:3b"), "picks the first listed model, sorted");

        let mut session = Session::with_backends(&orion, Mode::Local, None, Some(Box::new(backend)));
        let mut ui = Recorder::default();
        session.turn("what is this?", &mut ui).unwrap();
        assert_eq!(ui.out, "[Check the host.]<system_info><ok>It is a Linux box.");

        assert_eq!(requests.recv().unwrap().0, "/v1/models");
        let (path, req1) = requests.recv().unwrap();
        assert_eq!(path, "/v1/chat/completions");
        assert_eq!(req1["model"], "llama3.2:3b");
        assert_eq!(req1["messages"][0]["role"], "system");
        assert!(req1["tools"].as_array().unwrap().iter().any(|t| t["function"]["name"] == "proc_run"));

        let (_, req2) = requests.recv().unwrap();
        let msgs = req2["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 5, "system, user, assistant, two tool results");
        assert_eq!(msgs[2]["content"], "", "thinking is not sent back");
        assert_eq!(msgs[2]["tool_calls"][1]["function"]["arguments"], "{}");
        assert_eq!(msgs[3]["tool_call_id"], "c1");
        assert_eq!(msgs[4]["tool_call_id"], "c2");
        assert!(msgs[4]["content"].as_str().unwrap().contains("INVALID_JSON"));
    }

    #[test]
    fn knows_which_servers_are_on_this_machine() {
        for url in [
            "http://127.0.0.1:11434/v1",
            "http://localhost:8080/v1",
            "http://[::1]:8000/v1",
            "http://192.168.1.20:11434/v1",
            "http://10.0.0.5/v1",
            "https://user:pw@172.20.1.1:1234/v1",
        ] {
            assert!(is_on_machine(url), "{url}");
        }
        for url in ["https://generativelanguage.googleapis.com/v1beta/openai", "https://api.openai.com/v1", "http://8.8.8.8/v1", "http://ollama.example.com:11434/v1"] {
            assert!(!is_on_machine(url), "{url}");
        }
        let gemini = LocalConfig { base_url: "https://generativelanguage.googleapis.com/v1beta/openai".into(), ..Default::default() };
        assert!(!gemini.on_machine());
        assert!(LocalConfig { on_machine: Some(true), ..gemini }.on_machine());
        assert!(LocalConfig::default().on_machine());
    }
}

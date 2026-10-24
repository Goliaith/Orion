//! The model-based router: the local model judges whether a message needs
//! Claude. It runs on this machine, so routing never sends a message to
//! Claude just to decide where to send it.

use std::cell::Cell;
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use super::http::{Http, StatusError};
use super::local::{auth_headers, LocalConfig};
use super::router::{Classifier, Verdict};
use super::Provider;

pub struct LocalClassifier {
    http: Http,
    model: String,
    prefer: Provider,
    timeout: Duration,
    /// `[chat.local] reasoning_effort`, for servers that think by default.
    reasoning_effort: Option<String>,
    /// Set once the server has rejected the optional request fields, so they
    /// are not sent again.
    minimal: Cell<bool>,
}

impl LocalClassifier {
    pub fn new(config: &LocalConfig, model: String, prefer: Provider, timeout: Duration) -> Result<Self> {
        let http = Http::new(&config.base_url, auth_headers(config)?);
        let reasoning_effort = config.reasoning_effort.clone().filter(|e| !e.is_empty());
        Ok(Self { http, model, prefer, timeout, reasoning_effort, minimal: Cell::new(false) })
    }

    fn body(&self, text: &str, previous: Option<(Provider, &str)>, minimal: bool) -> Value {
        let mut body = json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": instructions(self.prefer) },
                { "role": "user", "content": user_message(text, previous) },
            ],
            "temperature": 0,
            "max_tokens": 200,
            "stream": false,
        });
        if !minimal {
            // JSON mode, and no reasoning pass for models that think
            // (llama.cpp and vLLM read chat_template_kwargs; others ignore it).
            body["response_format"] = json!({ "type": "json_object" });
            body["chat_template_kwargs"] = json!({ "enable_thinking": false });
            // Servers that ignore those (Gemini) take this.
            if let Some(effort) = &self.reasoning_effort {
                body["reasoning_effort"] = json!(effort);
            }
        }
        body
    }
}

impl Classifier for LocalClassifier {
    fn name(&self) -> String {
        self.model.clone()
    }

    fn classify(&self, text: &str, previous: Option<(Provider, &str)>) -> Result<Verdict> {
        let post = |minimal| self.http.post_json("/chat/completions", &self.body(text, previous, minimal), self.timeout);
        let reply = match post(self.minimal.get()) {
            Err(e) if !self.minimal.get() && is_client_error(&e) => {
                self.minimal.set(true);
                post(true)?
            }
            other => other?,
        };
        let content = reply
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("no message in the classifier's reply"))?;
        parse_verdict(content)
    }
}

fn is_client_error(e: &anyhow::Error) -> bool {
    e.downcast_ref::<StatusError>().is_some_and(|s| (400..500).contains(&s.code) && s.code != 404)
}

fn instructions(prefer: Provider) -> String {
    let unsure = match prefer {
        Provider::Local => "local",
        Provider::Claude => "claude",
    };
    format!(
        "You route requests inside Orion, an AI operating system, to one of two assistants. Both \
can use tools on this computer: read files, list processes, run programs.

local: a small model running on this computer. Fast and free. Good for quick lookups, short \
factual questions, single commands, simple file or system checks, and casual chat.

claude: a frontier model in the cloud. Use it for writing or debugging code, planning, tasks \
with several steps, analysis, careful writing, explanations that need real reasoning, and \
anything high-stakes where a mistake would be costly.

Judge only how hard the request is; privacy is handled elsewhere. If you are unsure, choose \
{unsure}. Reply with only a JSON object, for example:
{{\"route\": \"local\", \"reason\": \"single disk usage check\"}}
The reason must be at most eight words."
    )
}

fn user_message(text: &str, previous: Option<(Provider, &str)>) -> String {
    let clip = |s: &str, max: usize| s.chars().take(max).collect::<String>();
    match previous {
        Some((p, prev)) => {
            let who = match p {
                Provider::Claude => "claude",
                Provider::Local => "local",
            };
            format!("Previous message (answered by {who}):\n{}\n\nNew message to route:\n{}", clip(prev, 500), clip(text, 4000))
        }
        None => format!("Message to route:\n{}", clip(text, 4000)),
    }
}

/// Read a verdict from the model's reply: JSON (possibly fenced, or after
/// a `<think>` section), else a bare "claude" or "local".
pub fn parse_verdict(content: &str) -> Result<Verdict> {
    let mut text = content.to_string();
    while let (Some(a), Some(b)) = (text.find("<think>"), text.find("</think>")) {
        if b < a {
            break;
        }
        text.replace_range(a..b + "</think>".len(), "");
    }
    let json = text.find('{').zip(text.rfind('}')).and_then(|(a, b)| serde_json::from_str::<Value>(&text[a..=b]).ok());
    let (route, reason) = match &json {
        Some(v) => (
            v["route"].as_str().unwrap_or_default().to_lowercase(),
            v["reason"].as_str().unwrap_or_default().trim().to_string(),
        ),
        None => (text.trim().to_lowercase(), String::new()),
    };
    let provider = match (route.contains("claude"), route.contains("local")) {
        (true, false) => Provider::Claude,
        (false, true) => Provider::Local,
        _ => return Err(anyhow!("unclear classifier reply: {:?}", content.chars().take(80).collect::<String>())),
    };
    let reason: String = if reason.is_empty() { "no reason given".into() } else { reason.chars().take(60).collect() };
    Ok(Verdict { provider, reason })
}

#[cfg(test)]
mod tests {
    use super::super::test_support::fake_server;
    use super::*;

    #[test]
    fn parses_replies() {
        let v = parse_verdict(r#"{"route": "claude", "reason": "multi-step refactor"}"#).unwrap();
        assert_eq!(v, Verdict { provider: Provider::Claude, reason: "multi-step refactor".into() });
        let v = parse_verdict("<think>easy one</think>\n```json\n{\"route\":\"LOCAL\",\"reason\":\"quick check\"}\n```").unwrap();
        assert_eq!(v.provider, Provider::Local);
        assert_eq!(parse_verdict("claude").unwrap().provider, Provider::Claude);
        assert!(parse_verdict("either would do").is_err());
        assert!(parse_verdict(r#"{"route": "local or claude"}"#).is_err());
    }

    #[test]
    fn asks_the_local_server() {
        let reply = json!({"choices":[{"message":{"role":"assistant","content":"{\"route\":\"claude\",\"reason\":\"needs planning\"}"}}]});
        let (url, requests) = fake_server(vec![reply.to_string()]);
        let config = LocalConfig { base_url: format!("{url}/v1"), ..Default::default() };
        let c = LocalClassifier::new(&config, "qwen3:1.7b".into(), Provider::Local, Duration::from_secs(5)).unwrap();
        let v = c.classify("plan my migration", Some((Provider::Local, "how full is my disk?"))).unwrap();
        assert_eq!(v, Verdict { provider: Provider::Claude, reason: "needs planning".into() });

        let (path, body) = requests.recv().unwrap();
        assert_eq!(path, "/v1/chat/completions");
        assert_eq!(body["model"], "qwen3:1.7b");
        assert_eq!(body["temperature"], 0);
        assert_eq!(body["response_format"]["type"], "json_object");
        let user = body["messages"][1]["content"].as_str().unwrap();
        assert!(user.contains("answered by local") && user.ends_with("plan my migration"), "{user}");
        assert!(body["messages"][0]["content"].as_str().unwrap().contains("choose local"));
    }

    #[test]
    fn retries_without_optional_fields_when_rejected() {
        let ok = json!({"choices":[{"message":{"content":"{\"route\":\"local\",\"reason\":\"simple\"}"}}]});
        let rejected = format!("STATUS 400 {}", json!({"error":{"message":"unknown field response_format"}}));
        let (url, requests) = fake_server(vec![rejected, ok.to_string(), ok.to_string()]);
        let config = LocalConfig { base_url: format!("{url}/v1"), ..Default::default() };
        let c = LocalClassifier::new(&config, "m".into(), Provider::Local, Duration::from_secs(5)).unwrap();
        assert_eq!(c.classify("uptime?", None).unwrap().provider, Provider::Local);
        assert!(requests.recv().unwrap().1.get("response_format").is_some());
        assert!(requests.recv().unwrap().1.get("response_format").is_none());
        c.classify("uptime?", None).unwrap();
        assert!(requests.recv().unwrap().1.get("response_format").is_none(), "remembered");
    }
}

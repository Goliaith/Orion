//! HTTP plumbing shared by the model backends.

use std::io::{BufRead, BufReader};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

use crate::interrupt::{self, ChannelReader};

const MAX_RETRIES: u32 = 3;

#[derive(Clone)]
pub struct Http {
    agent: ureq::Agent,
    base: String,
    headers: Vec<(String, String)>,
}

impl Http {
    /// `headers` go on every request.
    pub fn new(base_url: &str, headers: Vec<(String, String)>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .try_proxy_from_env(!bypass_proxy(base_url))
            .timeout_connect(Duration::from_secs(30))
            // Long thinking, or a local server loading a model, can leave a
            // stream quiet for a while; this only trips on a dead connection.
            .timeout_read(Duration::from_secs(600))
            .build();
        Self { agent, base: base_url.trim_end_matches('/').to_string(), headers }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn request(&self, method: &str, path: &str, extra: &[(String, String)]) -> ureq::Request {
        let mut req = self.agent.request(method, &format!("{}{path}", self.base));
        for (k, v) in &self.headers {
            req = req.set(k, v);
        }
        for (k, v) in extra {
            req = req.set(k, v);
        }
        req
    }

    /// POST a JSON body and return the response body for streaming. Retries
    /// rate limits, overload and dropped connections that happen before the
    /// response starts; a server that refuses connections fails at once.
    /// Ctrl-C interrupts both the wait for the response and its reads.
    pub fn post_stream(&self, path: &str, body: &Value, extra: &[(&str, String)]) -> Result<impl BufRead> {
        let body = serde_json::to_string(body)?;
        let extra: Vec<(String, String)> = extra.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        let (http, path) = (self.clone(), path.to_string());
        let reader = interrupt::wait(move || {
            let mut attempt = 0;
            loop {
                let req = http.request("POST", &path, &extra).set("content-type", "application/json");
                let wait = match req.send_string(&body) {
                    Ok(resp) => return Ok(resp.into_reader()),
                    Err(ureq::Error::Status(code, resp)) if retryable(code) && attempt < MAX_RETRIES => {
                        resp.header("retry-after").and_then(|v| v.parse::<u64>().ok()).map(Duration::from_secs)
                    }
                    Err(e) => return Err(http.describe(e)),
                };
                attempt += 1;
                thread::sleep(wait.unwrap_or(Duration::from_secs(1 << attempt)).min(Duration::from_secs(60)));
                // The caller may have given up on us in the meantime.
                interrupt::check()?;
            }
        })?;
        Ok(BufReader::new(ChannelReader::spawn(reader)))
    }

    /// One quick POST with a JSON reply: no retries, and a timeout of its own.
    pub fn post_json(&self, path: &str, body: &Value, timeout: Duration) -> Result<Value> {
        let body = serde_json::to_string(body)?;
        let (http, path) = (self.clone(), path.to_string());
        interrupt::wait(move || {
            let resp = http
                .request("POST", &path, &[])
                .timeout(timeout)
                .set("content-type", "application/json")
                .send_string(&body)
                .map_err(|e| http.describe(e))?;
            let text = resp.into_string().context("reading the response")?;
            serde_json::from_str(&text).context("malformed JSON response")
        })
    }

    /// One POST of raw bytes (audio, multipart forms); returns the raw reply.
    pub fn post_bytes(&self, path: &str, content_type: &str, body: Vec<u8>, timeout: Duration) -> Result<Vec<u8>> {
        let (http, path, content_type) = (self.clone(), path.to_string(), content_type.to_string());
        interrupt::wait(move || {
            let resp = http
                .request("POST", &path, &[])
                .timeout(timeout)
                .set("content-type", &content_type)
                .send_bytes(&body)
                .map_err(|e| http.describe(e))?;
            let mut out = Vec::new();
            std::io::Read::read_to_end(&mut resp.into_reader(), &mut out).context("reading the response")?;
            Ok(out)
        })
    }

    pub fn get_json(&self, path: &str) -> Result<Value> {
        let (http, path) = (self.clone(), path.to_string());
        interrupt::wait(move || {
            let resp = http.request("GET", &path, &[]).call().map_err(|e| http.describe(e))?;
            let text = resp.into_string().context("reading the response")?;
            serde_json::from_str(&text).context("malformed JSON response")
        })
    }

    fn describe(&self, e: ureq::Error) -> anyhow::Error {
        match e {
            ureq::Error::Status(code, resp) => {
                let text = resp.into_string().unwrap_or_default();
                anyhow::Error::new(StatusError { base: self.base.clone(), code, message: error_message(&text) })
            }
            ureq::Error::Transport(t) if t.kind() == ureq::ErrorKind::ConnectionFailed => {
                anyhow!("cannot connect to {}: is the server running?", self.base)
            }
            ureq::Error::Transport(t) => anyhow!(t).context(format!("calling {}", self.base)),
        }
    }
}

/// The server answered with an error status.
#[derive(Debug)]
pub struct StatusError {
    pub base: String,
    pub code: u16,
    pub message: String,
}

impl std::fmt::Display for StatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} returned {}: {}", self.base, self.code, self.message)
    }
}

impl std::error::Error for StatusError {}

fn retryable(code: u16) -> bool {
    matches!(code, 408 | 409 | 429 | 500..=599)
}

/// The message out of an error body: `{"error": {"message": ...}}` (Claude,
/// OpenAI style) or `{"error": "..."}` (Ollama).
pub fn error_message(body: &str) -> String {
    let v: Option<Value> = serde_json::from_str(body).ok();
    v.as_ref()
        .and_then(|v| v.pointer("/error/message").or_else(|| v.get("error")))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| body.trim().to_string())
}

/// Loopback hosts and hosts matched by `NO_PROXY` (which ureq ignores) are
/// reached directly.
pub fn bypass_proxy(base_url: &str) -> bool {
    let rest = base_url.split_once("://").map_or(base_url, |(_, r)| r);
    let authority = rest.split('/').next().unwrap_or_default();
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    }
    .to_ascii_lowercase();
    if host == "localhost" || host == "::1" || host.starts_with("127.") {
        return true;
    }
    let no_proxy = non_empty_env("NO_PROXY").or_else(|| non_empty_env("no_proxy")).unwrap_or_default();
    no_proxy.split(',').map(|p| p.trim().trim_start_matches('.').to_ascii_lowercase()).any(|p| {
        p == "*" || (!p.is_empty() && (host == p || host.ends_with(&format!(".{p}"))))
    })
}

pub fn non_empty_env(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|v| !v.trim().is_empty())
}

/// Iterate the `data:` payloads of a Server-Sent Events stream.
pub fn sse_data(reader: impl BufRead) -> impl Iterator<Item = Result<String>> {
    reader.lines().filter_map(|line| match line {
        Ok(line) => line.strip_prefix("data:").map(|d| Ok(d.trim().to_string())),
        Err(e) if e.get_ref().is_some_and(|inner| inner.is::<interrupt::Interrupted>()) => {
            Some(Err(interrupt::Interrupted.into()))
        }
        Err(e) => Some(Err(anyhow!(e).context("reading the response stream"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_skips_the_proxy() {
        assert!(bypass_proxy("http://127.0.0.1:8080"));
        assert!(bypass_proxy("http://localhost/v1"));
        assert!(bypass_proxy("http://[::1]:9"));
    }

    #[test]
    fn error_shapes() {
        assert_eq!(error_message(r#"{"type":"error","error":{"type":"x","message":"bad key"}}"#), "bad key");
        assert_eq!(error_message(r#"{"error":"model 'q' not found"}"#), "model 'q' not found");
        assert_eq!(error_message("plain text\n"), "plain text");
    }

    #[test]
    fn refused_connection_is_explained() {
        // Port 9 (discard) on loopback is closed in test environments.
        let http = Http::new("http://127.0.0.1:9", Vec::new());
        let err = http.get_json("/models").unwrap_err().to_string();
        assert!(err.contains("is the server running"), "{err}");
    }
}

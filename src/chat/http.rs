//! HTTP plumbing shared by the model backends.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use super::Event;
use crate::interrupt::{self, ChannelReader};

const MAX_RETRIES: u32 = 3;

#[derive(Clone)]
pub struct Http {
    agent: ureq::Agent,
    base: String,
    headers: Vec<(String, String)>,
    /// Give a rate limit back to the caller as [`RateLimited`] instead of
    /// waiting it out, so the cloud router can switch models.
    hand_off: bool,
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
        Self { agent, base: base_url.trim_end_matches('/').to_string(), headers, hand_off: false }
    }

    /// Rate limits fail at once with [`RateLimited`] rather than being waited out.
    pub fn handing_off(mut self, yes: bool) -> Self {
        self.hand_off = yes;
        self
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
    /// response starts, telling `on` about each [`Wait`] (and when few
    /// requests are left); a server that refuses connections fails at once.
    /// Ctrl-C interrupts both the waits and the reads.
    pub fn post_stream(
        &self,
        path: &str,
        body: &Value,
        extra: &[(&str, String)],
        on: &mut dyn FnMut(Event),
    ) -> Result<impl BufRead> {
        let model = body["model"].as_str().unwrap_or_default().to_string();
        let body = serde_json::to_string(body)?;
        let extra: Vec<(String, String)> = extra.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        let mut attempt = 0;
        let reader = loop {
            let (http, path, extra, body) = (self.clone(), path.to_string(), extra.clone(), body.clone());
            quota::sent(&self.base);
            let sent = interrupt::wait(move || {
                let req = http.request("POST", &path, &extra).set("content-type", "application/json");
                match req.send_string(&body) {
                    Ok(resp) => {
                        quota::heard(&http.base, &resp);
                        Ok(Ok(resp.into_reader()))
                    }
                    Err(ureq::Error::Status(code, resp)) if retryable(code) && attempt < MAX_RETRIES => {
                        quota::heard(&http.base, &resp);
                        let header = resp.header("retry-after").and_then(|v| v.trim().parse::<u64>().ok());
                        let asked = header.or_else(|| retry_delay(&resp.into_string().unwrap_or_default()));
                        Ok(Err((code, asked.map(Duration::from_secs))))
                    }
                    Err(e) => Err(http.describe(e)),
                }
            })?;
            let (code, asked) = match sent {
                Ok(reader) => {
                    if let Some(low) = quota::running_low(&self.base) {
                        on(Event::Notice(&low));
                    }
                    break reader;
                }
                Err(retry) => retry,
            };
            if code == 429 && self.hand_off {
                return Err(RateLimited { service: service(&self.base), model, retry_after: asked }.into());
            }
            if code == 429 && attempt == 0 {
                crate::tuning::Signals::rate_limited(&self.base, &model);
            }
            attempt += 1;
            let wait = asked.unwrap_or(Duration::from_secs(1 << attempt)).min(Duration::from_secs(60));
            let w = Wait { service: service(&self.base), code, secs: wait.as_secs().max(1), attempt, quota: quota::describe(&self.base) };
            on(Event::Wait(&w));
            interrupt::wait(move || {
                thread::sleep(wait);
                Ok(())
            })?;
        };
        Ok(BufReader::new(ChannelReader::spawn(reader)))
    }

    /// One quick POST with a JSON reply: no retries, and a timeout of its own.
    pub fn post_json(&self, path: &str, body: &Value, timeout: Duration) -> Result<Value> {
        let body = serde_json::to_string(body)?;
        // The classifier's requests count against the same limit.
        quota::sent(&self.base);
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
        self.get_json_within(path, Duration::from_secs(600))
    }

    /// GET with a JSON reply, giving up after `timeout`.
    pub fn get_json_within(&self, path: &str, timeout: Duration) -> Result<Value> {
        let (http, path) = (self.clone(), path.to_string());
        interrupt::wait(move || {
            let resp = http.request("GET", &path, &[]).timeout(timeout).call().map_err(|e| http.describe(e))?;
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

/// A rate limit handed back instead of waited out (see [`Http::handing_off`]).
#[derive(Debug)]
pub struct RateLimited {
    pub service: String,
    pub model: String,
    /// How long the server asked for, if it said.
    pub retry_after: Option<Duration>,
}

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} rate limited {}", self.service, self.model)
    }
}

impl std::error::Error for RateLimited {}

fn retryable(code: u16) -> bool {
    matches!(code, 408 | 409 | 429 | 500..=599)
}

/// A retry being waited out, for the user to see: a free tier's rate limit
/// otherwise looks like a hang.
#[derive(Debug, Clone, PartialEq)]
pub struct Wait {
    /// Who is making us wait: "Gemini", "Claude", "the local model server".
    pub service: String,
    pub code: u16,
    pub secs: u64,
    /// Which retry this is, of [`MAX_RETRIES`].
    pub attempt: u32,
    /// What Orion knows of the allowance: "0 of 50 requests left this
    /// minute", or how many it sent in the last minute.
    pub quota: Option<String>,
}

impl Wait {
    /// What is being waited on, for a status line: "waiting on Gemini's rate limit".
    pub fn label(&self) -> String {
        match self.code {
            429 => format!("waiting on {}'s rate limit", self.service),
            503 | 529 => format!("waiting on {} (busy)", self.service),
            code => format!("waiting on {} (it answered {code})", self.service),
        }
    }
}

impl std::fmt::Display for Wait {
    /// "waiting on Gemini's rate limit (17s, retry 1 of 3) · 15 requests in the last minute"
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}s, retry {} of {MAX_RETRIES})", self.label(), self.secs, self.attempt)?;
        if let Some(q) = &self.quota {
            write!(f, " · {q}")?;
        }
        Ok(())
    }
}

/// The name people know a model API by, from its base URL.
pub fn service(base: &str) -> String {
    let rest = base.split_once("://").map_or(base, |(_, r)| r);
    let host = rest.split(['/', ':']).next().unwrap_or_default().to_ascii_lowercase();
    const KNOWN: [(&str, &str); 8] = [
        ("generativelanguage.googleapis.com", "Gemini"),
        ("anthropic.com", "Claude"),
        ("openai.com", "OpenAI"),
        ("openrouter.ai", "OpenRouter"),
        ("groq.com", "Groq"),
        ("mistral.ai", "Mistral"),
        ("deepseek.com", "DeepSeek"),
        ("together.xyz", "Together"),
    ];
    if let Some((_, name)) = KNOWN.iter().find(|(h, _)| host == *h || host.ends_with(&format!(".{h}"))) {
        return name.to_string();
    }
    if super::local::is_on_machine(base) {
        return "the local model server".into();
    }
    host
}

/// What each API has said about its allowance, and how often Orion has
/// called it lately. Shared by every client of the same base URL (a
/// backend and the classifier count against one limit).
mod quota {
    use super::*;

    #[derive(Default)]
    struct Quota {
        sent: VecDeque<Instant>,
        /// Requests left and the limit, from the last response's headers.
        left: Option<(u64, u64)>,
        /// Whether that limit is known to be per minute.
        per_minute: bool,
        warned: Option<Instant>,
    }

    static QUOTAS: Mutex<BTreeMap<String, Quota>> = Mutex::new(BTreeMap::new());
    const MINUTE: Duration = Duration::from_secs(60);

    fn with<T>(base: &str, f: impl FnOnce(&mut Quota) -> T) -> T {
        let mut all = QUOTAS.lock().unwrap_or_else(|e| e.into_inner());
        let q = all.entry(base.to_string()).or_default();
        while q.sent.front().is_some_and(|t| t.elapsed() > MINUTE) {
            q.sent.pop_front();
        }
        f(q)
    }

    pub fn sent(base: &str) {
        with(base, |q| q.sent.push_back(Instant::now()));
    }

    /// Read the allowance headers: Anthropic's `anthropic-ratelimit-requests-*`
    /// and the OpenAI-style `x-ratelimit-*-requests` (both per minute there;
    /// other providers that send the latter may count per day).
    pub fn heard(base: &str, resp: &ureq::Response) {
        let num = |h: &str| resp.header(h).and_then(|v| v.trim().parse::<u64>().ok());
        let found = num("anthropic-ratelimit-requests-remaining")
            .zip(num("anthropic-ratelimit-requests-limit"))
            .map(|l| (l, true))
            .or_else(|| {
                let per_minute = matches!(service(base).as_str(), "OpenAI");
                num("x-ratelimit-remaining-requests").zip(num("x-ratelimit-limit-requests")).map(|l| (l, per_minute))
            });
        if let Some((left, per_minute)) = found {
            with(base, |q| {
                q.left = Some(left);
                q.per_minute = per_minute;
            });
        }
    }

    fn text(q: &Quota) -> Option<String> {
        if let Some((left, limit)) = q.left {
            let window = if q.per_minute { " this minute" } else { "" };
            return Some(format!("{left} of {limit} requests left{window}"));
        }
        match q.sent.len() {
            0 => None,
            1 => Some("1 request in the last minute".into()),
            n => Some(format!("{n} requests in the last minute")),
        }
    }

    pub fn describe(base: &str) -> Option<String> {
        with(base, |q| text(q))
    }

    /// A note when the allowance is nearly used up, at most once a minute.
    pub fn running_low(base: &str) -> Option<String> {
        with(base, |q| {
            let (left, limit) = q.left?;
            let low = left <= (limit / 10).max(1);
            if !low || q.warned.is_some_and(|t| t.elapsed() < MINUTE) {
                return None;
            }
            q.warned = Some(Instant::now());
            Some(format!("{}: {}; replies may wait soon", service(base), text(q)?))
        })
    }

    #[cfg(test)]
    pub fn reset(base: &str) {
        QUOTAS.lock().unwrap().remove(base);
    }
}

/// The wait a 429 body asks for, when there's no `retry-after` header:
/// Google's APIs send `"retryDelay": "17s"` in the error details.
fn retry_delay(body: &str) -> Option<u64> {
    fn find(v: &Value) -> Option<u64> {
        match v {
            Value::Object(m) => m
                .get("retryDelay")
                .and_then(Value::as_str)
                .and_then(|d| d.trim().strip_suffix('s'))
                .and_then(|d| d.parse::<f64>().ok())
                .map(|d| d.ceil() as u64)
                .or_else(|| m.values().find_map(find)),
            Value::Array(a) => a.iter().find_map(find),
            _ => None,
        }
    }
    find(&serde_json::from_str(body).ok()?)
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

/// Whether requests to `base_url` go through a proxy from the environment.
pub fn proxied(base_url: &str) -> bool {
    !bypass_proxy(base_url)
        && ["ALL_PROXY", "all_proxy", "HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"]
            .iter()
            .any(|v| non_empty_env(v).is_some())
}

pub fn non_empty_env(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|v| !v.trim().is_empty())
}

/// A credential from the environment, cleaned: keys never contain spaces or
/// invisible characters, so any that came along with a paste (a line
/// ending, a non-breaking or zero-width space, a byte-order mark) are
/// dropped wherever they are. Errors never include the value.
pub fn secret_env(var: &str) -> Result<Option<String>> {
    let Some(value) = non_empty_env(var) else { return Ok(None) };
    clean_secret(&value).map_err(|e| anyhow!("{var} {e}"))
}

/// A pasted key without the whitespace and invisible characters that came
/// along with it; `None` if nothing is left. The error never includes it.
pub fn clean_secret(value: &str) -> Result<Option<String>> {
    let value: String = value.chars().filter(|&c| !invisible(c)).collect();
    if value.is_empty() {
        return Ok(None);
    }
    if !value.bytes().all(|b| b.is_ascii_graphic()) {
        bail!("has a character that can't be part of a key (an accent, a curly quote?); copy the key again");
    }
    Ok(Some(value))
}

/// Whitespace, control and zero-width characters.
fn invisible(c: char) -> bool {
    c.is_whitespace() || c.is_control() || matches!(c, '\u{200b}'..='\u{200f}' | '\u{2060}' | '\u{feff}' | '\u{ad}')
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
    use serde_json::json;

    #[test]
    fn secret_env_cleans_and_never_echoes_the_key() {
        std::env::set_var("ORION_TEST_KEY_OK", " abc.DEF-123\r\n");
        assert_eq!(secret_env("ORION_TEST_KEY_OK").unwrap().as_deref(), Some("abc.DEF-123"));
        std::env::set_var("ORION_TEST_KEY_HIDDEN", "\u{feff}abc\u{a0}DEF\u{200b}-1 2\t3");
        assert_eq!(secret_env("ORION_TEST_KEY_HIDDEN").unwrap().as_deref(), Some("abcDEF-123"));
        std::env::set_var("ORION_TEST_KEY_BAD", "abc\u{201c}secret");
        let err = secret_env("ORION_TEST_KEY_BAD").unwrap_err().to_string();
        assert!(err.contains("ORION_TEST_KEY_BAD") && !err.contains("secret"), "{err}");
        assert_eq!(secret_env("ORION_TEST_KEY_UNSET").unwrap(), None);
    }

    #[test]
    fn retries_say_why_and_how_long() {
        let gemini = r#"[{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","details":[
            {"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"16.4s"}]}}]"#;
        assert_eq!(retry_delay(gemini), Some(17));
        assert_eq!(retry_delay(r#"{"error":{"message":"slow down"}}"#), None);
        let w = |code, secs, attempt, quota: Option<&str>| {
            Wait { service: "Gemini".into(), code, secs, attempt, quota: quota.map(str::to_string) }.to_string()
        };
        assert_eq!(w(429, 17, 1, Some("15 requests in the last minute")), "waiting on Gemini's rate limit (17s, retry 1 of 3) · 15 requests in the last minute");
        assert_eq!(w(503, 2, 2, None), "waiting on Gemini (busy) (2s, retry 2 of 3)");
        assert_eq!(service("https://generativelanguage.googleapis.com/v1beta/openai"), "Gemini");
        assert_eq!(service("https://api.anthropic.com"), "Claude");
        assert_eq!(service("http://127.0.0.1:11434/v1"), "the local model server");
        assert_eq!(service("https://llm.example.net/v1"), "llm.example.net");
    }

    #[test]
    fn rate_limits_are_retried_out_loud() {
        // A server that rate-limits once, then answers.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        thread::spawn(move || {
            for (i, conn) in listener.incoming().take(2).enumerate() {
                let mut conn = conn.unwrap();
                let mut buf = [0u8; 4096];
                let _ = std::io::Read::read(&mut conn, &mut buf);
                let reply = if i == 0 {
                    "HTTP/1.1 429 Too Many Requests\r\nretry-after: 0\r\nx-ratelimit-remaining-requests: 0\r\nx-ratelimit-limit-requests: 15\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}"
                } else {
                    "HTTP/1.1 200 OK\r\nx-ratelimit-remaining-requests: 1\r\nx-ratelimit-limit-requests: 15\r\ncontent-length: 8\r\nconnection: close\r\n\r\ndata: ok\n"
                };
                std::io::Write::write_all(&mut conn, reply.as_bytes()).unwrap();
            }
        });
        quota::reset(&url);
        let mut seen = Vec::new();
        let reader = Http::new(&url, Vec::new()).post_stream("/x", &json!({}), &[], &mut |e| {
            seen.push(match e {
                Event::Wait(w) => format!("wait: {w}"),
                Event::Notice(n) => format!("notice: {n}"),
                _ => "other".into(),
            })
        });
        let data: Vec<String> = sse_data(reader.unwrap()).map(Result::unwrap).collect();
        assert_eq!(data, ["ok"]);
        assert_eq!(
            seen,
            [
                "wait: waiting on the local model server's rate limit (1s, retry 1 of 3) · 0 of 15 requests left",
                "notice: the local model server: 1 of 15 requests left; replies may wait soon",
            ]
        );
    }

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

//! HTTP plumbing shared by the model backends.

use std::io::{BufRead, BufReader};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
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
    /// response starts, telling `notice` how long each wait is; a server that
    /// refuses connections fails at once. Ctrl-C interrupts both the waits
    /// and the reads.
    pub fn post_stream(
        &self,
        path: &str,
        body: &Value,
        extra: &[(&str, String)],
        notice: &mut dyn FnMut(&str),
    ) -> Result<impl BufRead> {
        let model = body["model"].as_str().unwrap_or_default().to_string();
        let body = serde_json::to_string(body)?;
        let extra: Vec<(String, String)> = extra.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        let mut attempt = 0;
        let reader = loop {
            let (http, path, extra, body) = (self.clone(), path.to_string(), extra.clone(), body.clone());
            let sent = interrupt::wait(move || {
                let req = http.request("POST", &path, &extra).set("content-type", "application/json");
                match req.send_string(&body) {
                    Ok(resp) => Ok(Ok(resp.into_reader())),
                    Err(ureq::Error::Status(code, resp)) if retryable(code) && attempt < MAX_RETRIES => {
                        let header = resp.header("retry-after").and_then(|v| v.trim().parse::<u64>().ok());
                        let asked = header.or_else(|| retry_delay(&resp.into_string().unwrap_or_default()));
                        Ok(Err((code, asked.map(Duration::from_secs))))
                    }
                    Err(e) => Err(http.describe(e)),
                }
            })?;
            let (code, asked) = match sent {
                Ok(reader) => break reader,
                Err(retry) => retry,
            };
            if code == 429 && attempt == 0 {
                crate::tuning::Signals::rate_limited(&self.base, &model);
            }
            attempt += 1;
            let wait = asked.unwrap_or(Duration::from_secs(1 << attempt)).min(Duration::from_secs(60));
            notice(&retry_notice(code, wait, attempt));
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

fn retryable(code: u16) -> bool {
    matches!(code, 408 | 409 | 429 | 500..=599)
}

/// What a retry is waiting for, for the user: a free tier's rate limit
/// otherwise looks like a hang.
fn retry_notice(code: u16, wait: Duration, attempt: u32) -> String {
    let why = match code {
        429 => "rate limited".to_string(),
        503 | 529 => "the server is busy".to_string(),
        _ => format!("the server answered {code}"),
    };
    format!("{why}, retrying in {}s ({attempt} of {MAX_RETRIES})", wait.as_secs().max(1))
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
        assert_eq!(retry_notice(429, Duration::from_secs(17), 1), "rate limited, retrying in 17s (1 of 3)");
        assert_eq!(retry_notice(503, Duration::from_secs(2), 2), "the server is busy, retrying in 2s (2 of 3)");
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
                    "HTTP/1.1 429 Too Many Requests\r\nretry-after: 0\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}"
                } else {
                    "HTTP/1.1 200 OK\r\ncontent-length: 8\r\nconnection: close\r\n\r\ndata: ok\n"
                };
                std::io::Write::write_all(&mut conn, reply.as_bytes()).unwrap();
            }
        });
        let mut notices = Vec::new();
        let reader = Http::new(&url, Vec::new()).post_stream("/x", &json!({}), &[], &mut |n| notices.push(n.to_string()));
        let data: Vec<String> = sse_data(reader.unwrap()).map(Result::unwrap).collect();
        assert_eq!(data, ["ok"]);
        assert_eq!(notices, ["rate limited, retrying in 1s (1 of 3)"]);
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

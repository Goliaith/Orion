//! Speech to text: an OpenAI-compatible transcription server, or a command.

use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use super::{shell, shell_quote, TempPath, VoiceConfig};
use crate::chat::http::{secret_env, Http};

#[derive(Clone)]
pub enum Transcriber {
    Http { http: Http, model: String, language: String, timeout: Duration },
    Command(String),
}

impl Transcriber {
    pub fn from_config(config: &VoiceConfig) -> Result<Self> {
        if !config.transcribe_command.is_empty() {
            return Ok(Self::Command(config.transcribe_command.clone()));
        }
        if config.transcribe_url.is_empty() {
            bail!("set voice.transcribe_url or voice.transcribe_command");
        }
        let mut headers = Vec::new();
        if !config.api_key_env.is_empty() {
            let key = secret_env(&config.api_key_env)?.with_context(|| format!("{} is not set", config.api_key_env))?;
            headers.push(("authorization".to_string(), format!("Bearer {key}")));
        }
        Ok(Self::Http {
            http: Http::new(&config.transcribe_url, headers),
            model: config.transcribe_model.clone(),
            language: config.language.clone(),
            timeout: Duration::from_secs(config.transcribe_timeout_secs.max(1)),
        })
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Http { http, model, .. } => format!("{model} at {}", http.base()),
            Self::Command(c) => c.split_whitespace().next().unwrap_or_default().to_string(),
        }
    }

    /// Whether audio stays on this machine.
    pub fn is_local(&self) -> bool {
        match self {
            Self::Http { http, .. } => {
                let rest = http.base().split_once("://").map_or(http.base(), |(_, r)| r);
                let host = rest.split('/').next().unwrap_or_default();
                let host = host.strip_prefix('[').map_or_else(|| host.split(':').next().unwrap_or_default(), |h| h.split(']').next().unwrap_or_default());
                host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
            }
            Self::Command(_) => true,
        }
    }

    /// The words in a WAV recording.
    pub fn transcribe(&self, wav: &[u8]) -> Result<String> {
        let start = Instant::now();
        let raw = match self {
            Self::Http { http, model, language, timeout } => {
                let mut fields = vec![("model", model.as_str()), ("response_format", "json")];
                if !language.is_empty() {
                    fields.push(("language", language.as_str()));
                }
                let (content_type, body) = multipart(&fields, "file", "speech.wav", "audio/wav", wav);
                let reply = http.post_bytes("/audio/transcriptions", &content_type, body, *timeout)?;
                let v: Value = serde_json::from_slice(&reply)
                    .map_err(|_| anyhow!("unexpected reply: {}", String::from_utf8_lossy(&reply).chars().take(120).collect::<String>()))?;
                v["text"].as_str().context("the reply has no text")?.to_string()
            }
            Self::Command(template) => {
                let file = TempPath::new("wav");
                std::fs::write(&file.0, wav)?;
                let command = template.replace("{wav}", &shell_quote(&file.0.to_string_lossy()));
                let out = shell(&command)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .output()
                    .with_context(|| format!("running {}", self.describe()))?;
                if !out.status.success() {
                    let err = String::from_utf8_lossy(&out.stderr);
                    bail!("{} failed: {}", self.describe(), err.lines().last().unwrap_or("").trim());
                }
                String::from_utf8_lossy(&out.stdout).into_owned()
            }
        };
        // How long it took, so Orion can tell when it is too slow.
        crate::tuning::Signals::transcribed(&self.describe(), wav_secs(wav), start.elapsed());
        let text = clean(&raw);
        if text.is_empty() {
            bail!("heard nothing that could be transcribed");
        }
        Ok(text)
    }
}

/// How long a WAV recording lasts, from its header.
fn wav_secs(wav: &[u8]) -> f32 {
    let (Some(fmt), true) = (wav.get(20..36), wav.starts_with(b"RIFF") && wav.len() > 44) else { return 0.0 };
    let channels = u16::from_le_bytes([fmt[2], fmt[3]]) as f32;
    let rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]) as f32;
    let bits = u16::from_le_bytes([fmt[14], fmt[15]]) as f32;
    let per_sec = channels * rate * bits / 8.0;
    if per_sec > 0.0 { (wav.len() - 44) as f32 / per_sec } else { 0.0 }
}

/// One line of plain text, without the markers whisper models emit for
/// non-speech ("[BLANK_AUDIO]", "(music)").
fn clean(raw: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in raw.chars() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn multipart(fields: &[(&str, &str)], file_field: &str, file_name: &str, file_type: &str, data: &[u8]) -> (String, Vec<u8>) {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let boundary = format!("orion-{:x}-{nanos:x}", std::process::id());
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes());
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{file_field}\"; filename=\"{file_name}\"\r\nContent-Type: {file_type}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::test_support::fake_server;

    #[test]
    fn cleans_transcripts() {
        assert_eq!(clean(" [BLANK_AUDIO]\n how full is\n my disk? (coughs) "), "how full is my disk?");
        assert_eq!(clean("[BLANK_AUDIO]"), "");
    }

    #[test]
    fn measures_recordings() {
        assert_eq!(wav_secs(&super::super::wav(&vec![0; 32_000], 16_000)), 2.0);
        assert_eq!(wav_secs(b"RIFFfake"), 0.0);
    }

    #[test]
    fn transcribes_with_a_command() {
        let t = Transcriber::Command("test -s {wav} && echo ' how full is my disk?'".into());
        assert_eq!(t.transcribe(&super::super::wav(&[1, 2, 3], 16_000)).unwrap(), "how full is my disk?");
        let t = Transcriber::Command("echo 'model not found' >&2; exit 2".into());
        assert!(t.transcribe(b"x").unwrap_err().to_string().contains("model not found"));
    }

    #[test]
    fn transcribes_over_http() {
        let (url, requests) = fake_server(vec![r#"{"text":" Hello Orion. "}"#.to_string()]);
        let config = VoiceConfig { transcribe_url: format!("{url}/v1"), language: "en".into(), ..Default::default() };
        let t = Transcriber::from_config(&config).unwrap();
        assert_eq!(t.transcribe(b"RIFFfake").unwrap(), "Hello Orion.");
        let (path, _) = requests.recv().unwrap();
        assert_eq!(path, "/v1/audio/transcriptions");
    }

    #[test]
    fn multipart_shape() {
        let (ct, body) = multipart(&[("model", "m")], "file", "a.wav", "audio/wav", b"DATA");
        let boundary = ct.split("boundary=").nth(1).unwrap();
        let text = String::from_utf8(body).unwrap();
        assert!(text.starts_with(&format!("--{boundary}\r\n")));
        assert!(text.contains("name=\"model\"\r\n\r\nm\r\n"));
        assert!(text.contains("filename=\"a.wav\"\r\nContent-Type: audio/wav\r\n\r\nDATA\r\n"));
        assert!(text.ends_with(&format!("--{boundary}--\r\n")));
    }
}

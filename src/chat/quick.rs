//! The fast path: requests simple enough that Orion answers them by calling
//! one tool itself, without asking a model. "Volume up", "brightness 40%",
//! "status" and "open firefox" take a tool call instead of a model round
//! trip (or two), which matters most when the model is a rate-limited
//! hosted API.
//!
//! Only exact, unambiguous phrasings match; anything else goes to a model
//! as before. The call goes through the same policy, approvals and audit
//! log as a model's would.

use serde_json::{json, Value};

/// How far "volume up" or "brighter" moves, in percent.
const STEP: i64 = 10;

/// A request the fast path can answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Quick {
    pub tool: &'static str,
    pub args: Value,
    what: What,
}

#[derive(Debug, Clone, PartialEq)]
enum What {
    Volume,
    Brightness,
    Status,
    /// The app or what is being opened, as the user wrote it.
    Open(String),
}

impl Quick {
    /// What it is, for the route line: "simple request: volume".
    pub fn kind(&self) -> &'static str {
        match self.what {
            What::Volume => "volume",
            What::Brightness => "brightness",
            What::Status => "status",
            What::Open(_) => "open",
        }
    }

    /// The reply, from what the tool returned.
    pub fn reply(&self, result: &Value) -> String {
        match &self.what {
            What::Volume => {
                let volume = result["volume"].as_i64();
                match (result["mute"].as_bool(), volume) {
                    (Some(true), _) => "Muted.".into(),
                    (_, Some(v)) if self.args.get("mute") == Some(&json!(false)) => format!("Unmuted; the volume is {v}%."),
                    (_, Some(v)) => format!("The volume is {v}%."),
                    _ => "Done.".into(),
                }
            }
            What::Brightness => match result.pointer("/brightness/percent").and_then(Value::as_f64) {
                Some(p) => format!("The brightness is {}%.", p.round()),
                None => "Done.".into(),
            },
            What::Status => status_line(result),
            What::Open(what) => format!("Opened {what}."),
        }
    }

    /// The reply when the call failed or was turned down.
    pub fn failed(&self, error: &str) -> String {
        let what = match &self.what {
            What::Volume => "change the volume".to_string(),
            What::Brightness => "change the brightness".to_string(),
            What::Status => "check the machine".to_string(),
            What::Open(what) => format!("open {what}"),
        };
        format!("Couldn't {what}: {error}")
    }
}

/// The request `text` makes, if it's one the fast path takes. `app` names
/// the installed app a name refers to, if exactly one.
pub fn parse(text: &str, app: &dyn Fn(&str) -> Option<String>) -> Option<Quick> {
    // A spoken request starts with who said it.
    let text = text.split_once(" (by voice): ").map_or(text, |(_, rest)| rest);
    if text.contains('\n') {
        return None;
    }
    let words: Vec<&str> = text
        .split_whitespace()
        .map(|w| w.trim_end_matches(['.', '!', ',', '?']))
        .filter(|w| !w.is_empty())
        .collect();
    let lower: Vec<String> = words.iter().map(|w| w.to_lowercase()).collect();
    let mut start = 0;
    let mut end = lower.len();
    // Politeness and the wake word aren't part of the request.
    let lead = ["hey", "ok", "okay", "orion", "please", "can", "could", "would", "will", "you"];
    while start < end && lead.contains(&lower[start].trim_end_matches(',')) {
        start += 1;
    }
    while end > start && matches!(lower[end - 1].as_str(), "please" | "thanks" | "now") {
        end -= 1;
    }
    let low: Vec<&str> = lower[start..end].iter().map(String::as_str).collect();
    let orig = &words[start..end];
    if low.is_empty() || low.len() > 8 {
        return None;
    }
    volume(&low)
        .map(|args| Quick { tool: "audio_set", args, what: What::Volume })
        .or_else(|| brightness(&low).map(|args| Quick { tool: "display_set", args, what: What::Brightness }))
        .or_else(|| status(&low).then(|| Quick { tool: "system_info", args: json!({}), what: What::Status }))
        .or_else(|| open(&low, orig, app))
}

/// Drop leading filler: "turn the volume up" → "volume up".
fn strip<'a, 'b>(mut w: &'a [&'b str], filler: &[&str]) -> &'a [&'b str] {
    while let [first, rest @ ..] = w {
        if !filler.contains(first) {
            break;
        }
        w = rest;
    }
    w
}

/// "40", "40%", "40 percent" → 40.
fn percent(w: &[&str]) -> Option<i64> {
    match w {
        [n] => n.strip_suffix('%').unwrap_or(n).parse().ok(),
        [n, "%" | "percent"] => n.parse().ok(),
        _ => None,
    }
}

/// A change: "up", "up by 20", "down 5%". `None` when it isn't one.
fn step(w: &[&str], up: &[&str], down: &[&str]) -> Option<i64> {
    let (sign, rest) = match w {
        [d, rest @ ..] if up.contains(d) => (1, rest),
        [d, rest @ ..] if down.contains(d) => (-1, rest),
        _ => return None,
    };
    let by = match strip(rest, &["by", "a", "bit", "little"]) {
        [] => STEP,
        n => percent(n)?,
    };
    (1..=100).contains(&by).then_some(sign * by)
}

fn volume(w: &[&str]) -> Option<Value> {
    let nouns = ["volume", "sound", "audio", "speakers"];
    match strip(w, &["turn", "set", "the", "my"]) {
        ["mute", rest @ ..] if strip(rest, &["the", "my"]).iter().all(|x| nouns.contains(x)) => return Some(json!({ "mute": true })),
        ["unmute", rest @ ..] if strip(rest, &["the", "my"]).iter().all(|x| nouns.contains(x)) => {
            return Some(json!({ "mute": false }))
        }
        ["louder"] => return Some(json!({ "change": STEP })),
        ["quieter" | "softer"] => return Some(json!({ "change": -STEP })),
        _ => {}
    }
    let w = strip(w, &["turn", "set", "the", "my"]);
    let [noun, rest @ ..] = w else { return None };
    if !nouns[..2].contains(noun) {
        return None;
    }
    // "volume up", "volume down by 20"; "volume to 40", "volume 40%".
    if let Some(change) = step(rest, &["up", "higher"], &["down", "lower"]) {
        return Some(json!({ "change": change }));
    }
    let v = percent(strip(rest, &["to", "at"]))?;
    (0..=100).contains(&v).then(|| json!({ "volume": v }))
}

fn brightness(w: &[&str]) -> Option<Value> {
    match strip(w, &["make", "the", "screen"]) {
        ["brighter"] => return Some(json!({ "change_brightness": STEP })),
        ["dimmer" | "darker"] => return Some(json!({ "change_brightness": -STEP })),
        _ => {}
    }
    if let ["dim", rest @ ..] = w {
        if strip(rest, &["the", "my"]) == ["screen"] {
            return Some(json!({ "change_brightness": -STEP }));
        }
    }
    let w = strip(w, &["turn", "set", "the", "screen", "display"]);
    let ["brightness", rest @ ..] = w else { return None };
    if let Some(change) = step(rest, &["up", "higher"], &["down", "lower"]) {
        return Some(json!({ "change_brightness": change }));
    }
    let v = percent(strip(rest, &["to", "at"]))?;
    (1..=100).contains(&v).then(|| json!({ "brightness": v }))
}

fn status(w: &[&str]) -> bool {
    matches!(
        strip(w, &["show", "the", "my"]),
        ["status"] | ["system", "status"] | ["system", "info"] | ["machine", "status"] | ["computer", "status"]
    )
}

fn open(low: &[&str], orig: &[&str], app: &dyn Fn(&str) -> Option<String>) -> Option<Quick> {
    let (["open" | "launch" | "start", rest @ ..], [_, orig_rest @ ..]) = (low, orig) else { return None };
    let skip = rest.len() - strip(rest, &["the", "up"]).len();
    let (rest, orig_rest) = (&rest[skip..], &orig_rest[skip..]);
    // A link or a path is opened in its default app, as written.
    if let [target] = orig_rest {
        let t = target.to_lowercase();
        if t.starts_with("http://") || t.starts_with("https://") || target.starts_with('/') || target.starts_with("~/") {
            return Some(Quick { tool: "app_open", args: json!({ "targets": [target] }), what: What::Open(target.to_string()) });
        }
    }
    // An app's name: a word or two, naming exactly one installed app.
    let name = match rest {
        [.., "app"] => &orig_rest[..orig_rest.len() - 1],
        _ => orig_rest,
    };
    if name.is_empty() || name.len() > 3 || rest.iter().any(|w| ["my", "a", "file", "folder", "new", "and", "with", "in"].contains(w)) {
        return None;
    }
    let name = name.join(" ");
    let found = app(&name)?;
    Some(Quick { tool: "app_open", args: json!({ "app": found }), what: What::Open(found) })
}

/// `system_info` in a line: "host: Fedora 42, up 3h 12m, load 0.52, 5.1 of 16 GB memory free".
fn status_line(v: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(os) = v["os"].as_str() {
        parts.push(os.to_string());
    }
    if let Some(up) = v["uptime_secs"].as_f64() {
        let mins = (up / 60.0) as u64;
        let (d, h, m) = (mins / 1440, mins / 60 % 24, mins % 60);
        parts.push(match (d, h) {
            (0, 0) => format!("up {m}m"),
            (0, _) => format!("up {h}h {m}m"),
            _ => format!("up {d}d {h}h"),
        });
    }
    if let Some(load) = v["load_average"][0].as_f64() {
        let cores = v.pointer("/cpu/logical_cores").and_then(Value::as_u64);
        parts.push(match cores {
            Some(c) => format!("load {load:.2} on {c} cores"),
            None => format!("load {load:.2}"),
        });
    }
    let gb = |k: &str| v.pointer(&format!("/memory_kb/{k}")).and_then(Value::as_f64).map(|kb| kb / 1_048_576.0);
    if let (Some(free), Some(total)) = (gb("available"), gb("total")) {
        parts.push(format!("{free:.1} of {total:.0} GB memory free"));
    }
    let host = v["hostname"].as_str().unwrap_or("This machine");
    if parts.is_empty() {
        return format!("{host} is up.");
    }
    format!("{host}: {}.", parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apps(name: &str) -> Option<String> {
        match name.to_lowercase().as_str() {
            "firefox" => Some("Firefox".into()),
            "files" | "nautilus" => Some("Files".into()),
            _ => None,
        }
    }

    fn args(text: &str) -> Option<(&'static str, Value)> {
        parse(text, &apps).map(|q| (q.tool, q.args))
    }

    #[test]
    fn volume_requests() {
        assert_eq!(args("volume up"), Some(("audio_set", json!({ "change": 10 }))));
        assert_eq!(args("Turn the volume down."), Some(("audio_set", json!({ "change": -10 }))));
        assert_eq!(args("turn the volume up by 25%"), Some(("audio_set", json!({ "change": 25 }))));
        assert_eq!(args("set volume to 40"), Some(("audio_set", json!({ "volume": 40 }))));
        assert_eq!(args("volume 30 percent"), Some(("audio_set", json!({ "volume": 30 }))));
        assert_eq!(args("Orion, mute"), Some(("audio_set", json!({ "mute": true }))));
        assert_eq!(args("unmute the sound please"), Some(("audio_set", json!({ "mute": false }))));
        assert_eq!(args("louder"), Some(("audio_set", json!({ "change": 10 }))));
        assert_eq!(args("Ana (by voice): quieter"), Some(("audio_set", json!({ "change": -10 }))));
        // Anything more than that is for a model.
        assert_eq!(args("volume 150"), None);
        assert_eq!(args("mute firefox"), None);
        assert_eq!(args("turn the volume up on the HDMI output"), None);
        assert_eq!(args("why is the volume so low?"), None);
    }

    #[test]
    fn brightness_requests() {
        assert_eq!(args("brightness up"), Some(("display_set", json!({ "change_brightness": 10 }))));
        assert_eq!(args("set the screen brightness to 60%"), Some(("display_set", json!({ "brightness": 60 }))));
        assert_eq!(args("brightness down 5"), Some(("display_set", json!({ "change_brightness": -5 }))));
        assert_eq!(args("dim the screen"), Some(("display_set", json!({ "change_brightness": -10 }))));
        assert_eq!(args("brighter"), Some(("display_set", json!({ "change_brightness": 10 }))));
        assert_eq!(args("brightness 0"), None, "a dark panel is not a quick fix");
        assert_eq!(args("brightness on my second monitor"), None);
    }

    #[test]
    fn status_and_open() {
        assert_eq!(args("status"), Some(("system_info", json!({}))));
        assert_eq!(args("show system status"), Some(("system_info", json!({}))));
        assert_eq!(args("status of nginx"), None);
        assert_eq!(args("open firefox"), Some(("app_open", json!({ "app": "Firefox" }))));
        assert_eq!(args("please launch the Files app"), Some(("app_open", json!({ "app": "Files" }))));
        assert_eq!(args("open https://example.com/A"), Some(("app_open", json!({ "targets": ["https://example.com/A"] }))));
        assert_eq!(args("open ~/Notes.txt"), Some(("app_open", json!({ "targets": ["~/Notes.txt"] }))));
        assert_eq!(args("open steam"), None, "not installed: a model can offer to install it");
        assert_eq!(args("open my latest download"), None);
        assert_eq!(args("open a new firefox window"), None);
        assert_eq!(args("start the backup service"), None);
    }

    #[test]
    fn replies() {
        let q = parse("volume up", &apps).unwrap();
        assert_eq!(q.reply(&json!({ "volume": 55, "mute": false })), "The volume is 55%.");
        let q = parse("unmute", &apps).unwrap();
        assert_eq!(q.reply(&json!({ "volume": 30, "mute": false })), "Unmuted; the volume is 30%.");
        let q = parse("brightness 40", &apps).unwrap();
        assert_eq!(q.reply(&json!({ "ok": true, "brightness": { "percent": 40.0 } })), "The brightness is 40%.");
        let q = parse("status", &apps).unwrap();
        let info = json!({ "hostname": "box", "os": "Fedora Linux 42", "uptime_secs": 11_520.0, "load_average": [0.52, 0.4, 0.3],
                           "cpu": { "logical_cores": 8 }, "memory_kb": { "total": 16_777_216, "available": 5_347_737 } });
        assert_eq!(q.reply(&info), "box: Fedora Linux 42, up 3h 12m, load 0.52 on 8 cores, 5.1 of 16 GB memory free.");
        let q = parse("open firefox", &apps).unwrap();
        assert_eq!(q.reply(&json!({ "ok": true })), "Opened Firefox.");
        assert_eq!(q.failed("the user declined this app_open call"), "Couldn't open Firefox: the user declined this app_open call");
    }
}

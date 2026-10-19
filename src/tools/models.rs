//! Local inference servers and the GPU memory they share: what each server
//! offers and has loaded, and loading, unloading and pulling models on
//! Ollama within a VRAM budget.
//!
//! The budget is the GPUs' memory minus `[models] vram_reserve_mb` each,
//! less what every process already uses. A model is only loaded when it
//! fits, or when `evict` may first unload the models that will expire
//! soonest (the least recently used).

use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::chat::http::Http;
use crate::policy::Policy;
use crate::tool::{parse_args, Risk, Tool};

const MIB: u64 = 1 << 20;
/// The usual ports of Ollama, llama.cpp's server, vLLM and LM Studio.
const USUAL_PORTS: [u16; 4] = [11434, 8080, 8000, 1234];

#[derive(Debug, Clone, PartialEq)]
enum Kind {
    Ollama(String),
    /// Any OpenAI-compatible server; `owned_by` says which, roughly.
    OpenAi(String),
}

struct Server {
    url: String,
    kind: Kind,
    http: Http,
}

#[derive(Debug, Clone)]
struct Model {
    name: String,
    size: u64,
    details: Value,
}

#[derive(Debug, Clone)]
struct Loaded {
    name: String,
    size: u64,
    vram: u64,
    expires_at: String,
}

/// `http://host:port`, without `/v1` or a trailing slash.
fn normalize(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    url.strip_suffix("/v1").unwrap_or(url).to_string()
}

/// Servers to look at: configured ones first (they are reported even when
/// down), then the usual local ports (only when something answers).
fn candidates(policy: &Policy) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut add = |url: String, configured: bool| {
        if !url.is_empty() && !out.iter().any(|(u, _)| *u == url) {
            out.push((url, configured));
        }
    };
    for s in &policy.models.servers {
        add(normalize(s), true);
    }
    add(normalize(&policy.chat.local.base_url), true);
    if policy.models.discover {
        for port in USUAL_PORTS {
            add(format!("http://127.0.0.1:{port}"), false);
        }
    }
    out
}

fn host_port(url: &str) -> Option<SocketAddr> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split('/').next()?;
    let with_port = if authority.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) {
        authority.to_string()
    } else {
        format!("{authority}:{}", if url.starts_with("https") { 443 } else { 80 })
    };
    with_port.to_socket_addrs().ok()?.next()
}

fn is_local(url: &str) -> bool {
    host_port(url).is_some_and(|a| a.ip().is_loopback())
}

fn probe(url: &str) -> Result<Server> {
    let addr = host_port(url).ok_or_else(|| anyhow!("can't resolve {url}"))?;
    TcpStream::connect_timeout(&addr, Duration::from_millis(if addr.ip().is_loopback() { 300 } else { 2000 }))
        .map_err(|e| anyhow!("{url} doesn't answer: {e}"))?;
    let http = Http::new(url, Vec::new());
    let quick = Duration::from_secs(3);
    if let Ok(v) = http.get_json_within("/api/version", quick) {
        if let Some(version) = v["version"].as_str() {
            return Ok(Server { url: url.into(), kind: Kind::Ollama(version.into()), http });
        }
    }
    let models = http.get_json_within("/v1/models", quick).with_context(|| format!("{url} is not an inference server"))?;
    let owner = models["data"][0]["owned_by"].as_str().unwrap_or_default();
    let kind = match owner {
        "llamacpp" => "llama.cpp",
        "vllm" => "vLLM",
        _ if url.ends_with(":1234") => "LM Studio",
        _ => "OpenAI-compatible",
    };
    Ok(Server { url: url.into(), kind: Kind::OpenAi(kind.into()), http })
}

fn servers(policy: &Policy) -> (Vec<Server>, Vec<Value>) {
    let mut up = Vec::new();
    let mut down = Vec::new();
    for (url, configured) in candidates(policy) {
        match probe(&url) {
            Ok(s) => up.push(s),
            Err(e) if configured => down.push(json!({ "url": url, "error": format!("{e:#}") })),
            Err(_) => {}
        }
    }
    (up, down)
}

impl Server {
    fn available(&self) -> Result<Vec<Model>> {
        match &self.kind {
            Kind::Ollama(_) => {
                let v = self.http.get_json_within("/api/tags", Duration::from_secs(10))?;
                Ok(v["models"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|m| Model {
                        name: m["name"].as_str().unwrap_or_default().into(),
                        size: m["size"].as_u64().unwrap_or(0),
                        details: m["details"].clone(),
                    })
                    .collect())
            }
            Kind::OpenAi(_) => {
                let v = self.http.get_json_within("/v1/models", Duration::from_secs(10))?;
                Ok(v["data"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|m| Model { name: m["id"].as_str().unwrap_or_default().into(), size: 0, details: Value::Null })
                    .collect())
            }
        }
    }

    fn loaded(&self) -> Result<Vec<Loaded>> {
        let Kind::Ollama(_) = self.kind else { return Ok(Vec::new()) };
        let v = self.http.get_json_within("/api/ps", Duration::from_secs(10))?;
        Ok(v["models"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|m| Loaded {
                name: m["name"].as_str().unwrap_or_default().into(),
                size: m["size"].as_u64().unwrap_or(0),
                vram: m["size_vram"].as_u64().unwrap_or(0),
                expires_at: m["expires_at"].as_str().unwrap_or_default().into(),
            })
            .collect())
    }

    fn kind_name(&self) -> String {
        match &self.kind {
            Kind::Ollama(_) => "Ollama".into(),
            Kind::OpenAi(k) => k.clone(),
        }
    }

    /// Ollama loads on an empty generate request and unloads on keep_alive 0.
    fn keep_alive(&self, model: &str, keep_alive: Value, timeout: Duration) -> Result<()> {
        self.http
            .post_json("/api/generate", &json!({ "model": model, "keep_alive": keep_alive, "stream": false }), timeout)
            .map(drop)
            .with_context(|| format!("{} on {}", model, self.url))
    }
}

/// `qwen3` and `qwen3:latest` are the same model.
fn same_model(a: &str, b: &str) -> bool {
    let tag = |s: &str| if s.contains(':') { s.to_string() } else { format!("{s}:latest") };
    tag(a) == tag(b)
}

/// One GPU's memory, in MiB.
struct Card {
    index: u64,
    name: String,
    total: u64,
    used: u64,
    processes: Value,
}

fn cards() -> Vec<Card> {
    let g = super::gpu_status();
    g["nvidia"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            Some(Card {
                index: c["index"].as_u64()?,
                name: c["name"].as_str().unwrap_or("GPU").into(),
                total: c["memory_total_mib"].as_u64()?,
                used: c["memory_used_mib"].as_u64()?,
                processes: c["processes"].clone(),
            })
        })
        .collect()
}

/// MiB a model may still take: over all GPUs, total less the reserve less
/// what is in use. `None` when there are no GPU readings to go by.
fn free_mib(cards: &[Card], reserve: u64) -> Option<u64> {
    (!cards.is_empty()).then(|| cards.iter().map(|c| c.total.saturating_sub(reserve).saturating_sub(c.used)).sum())
}

/// What loading needs, roughly: the weights plus room for the KV cache and
/// compute buffers.
fn need_mib(size_bytes: u64) -> u64 {
    (size_bytes as f64 * 1.15 / MIB as f64).ceil() as u64 + 512
}

/// Which loaded models to unload first so that `need` MiB fit in `free`:
/// none if it fits; with `evict`, those expiring soonest. An error says
/// what is in the way.
fn plan_load(need: u64, free: Option<u64>, loaded: &[Loaded], target: &str, evict: bool) -> Result<Vec<String>> {
    let Some(free) = free else { return Ok(Vec::new()) };
    if need <= free {
        return Ok(Vec::new());
    }
    let mut others: Vec<&Loaded> = loaded.iter().filter(|l| !same_model(&l.name, target)).collect();
    others.sort_by(|a, b| a.expires_at.cmp(&b.expires_at));
    let in_the_way = || {
        others.iter().map(|l| format!("{} ({} MiB)", l.name, l.vram / MIB)).collect::<Vec<_>>().join(", ")
    };
    if !evict {
        let hint = if others.is_empty() {
            "nothing Orion can unload is loaded; lower models.vram_reserve_mb or free GPU memory".to_string()
        } else {
            format!("unload {} first, or pass evict: true", in_the_way())
        };
        bail!("{target} needs about {need} MiB of VRAM and {free} MiB are free within the budget: {hint}");
    }
    let (mut room, mut out) = (free, Vec::new());
    for l in &others {
        if room >= need {
            break;
        }
        room += l.vram / MIB;
        out.push(l.name.clone());
    }
    ensure!(
        room >= need,
        "{target} needs about {need} MiB of VRAM; even after unloading {} only {room} MiB would be free",
        in_the_way()
    );
    Ok(out)
}

/// Models loaded on the Ollama servers Orion knows: (name, VRAM MiB), for
/// status displays.
pub fn loaded_models(policy: &Policy) -> Vec<(String, u64)> {
    servers(policy)
        .0
        .iter()
        .flat_map(|s| s.loaded().unwrap_or_default())
        .map(|l| (l.name, l.vram / MIB))
        .collect()
}

fn server_schema() -> Value {
    json!({ "type": "string", "description": "The server's URL, as model_status lists it. Default: the first Ollama server that has the model." })
}

pub struct ModelStatus;

impl Tool for ModelStatus {
    fn name(&self) -> &'static str {
        "model_status"
    }

    fn description(&self) -> &'static str {
        "Show the local inference servers (Ollama, llama.cpp, vLLM, LM Studio) with the models each offers \
         and has loaded, and the GPUs' memory: what each process uses and how much is free for models \
         within Orion's VRAM budget. Check this before loading a model."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct NoArgs {}
        let NoArgs {} = parse_args(args)?;
        let reserve = policy.models.vram_reserve_mb;
        let cards = cards();
        let listeners = super::network::listening();
        let (up, down) = servers(policy);
        let servers: Vec<Value> = up
            .iter()
            .map(|s| {
                let mut v = json!({ "url": s.url, "kind": s.kind_name() });
                if let Kind::Ollama(version) = &s.kind {
                    v["version"] = json!(version);
                }
                if is_local(&s.url) {
                    let port = host_port(&s.url).map(|a| a.port());
                    if let Some(l) = listeners.iter().find(|l| Some(l.port) == port && l.proto == "tcp") {
                        v["pid"] = json!(l.pid);
                        v["process"] = json!(l.process);
                    }
                }
                match s.available() {
                    Ok(models) => {
                        v["models"] = models
                            .iter()
                            .map(|m| {
                                let mut e = json!({ "name": m.name });
                                if m.size > 0 {
                                    e["size_mib"] = json!(m.size / MIB);
                                    e["needs_vram_mib"] = json!(need_mib(m.size));
                                    e["parameters"] = m.details["parameter_size"].clone();
                                    e["quantization"] = m.details["quantization_level"].clone();
                                }
                                e
                            })
                            .collect()
                    }
                    Err(e) => v["error"] = json!(format!("{e:#}")),
                }
                if let Ok(loaded) = s.loaded() {
                    v["loaded"] = loaded
                        .iter()
                        .map(|l| {
                            json!({
                                "name": l.name,
                                "vram_mib": l.vram / MIB,
                                "on_gpu_pct": (l.size > 0).then(|| (l.vram * 100 / l.size).min(100)),
                                "expires_at": l.expires_at,
                            })
                        })
                        .collect();
                }
                if let Kind::OpenAi(_) = s.kind {
                    v["note"] = json!("serves a fixed model; start, stop or switch it as a service (svc_control)");
                }
                v
            })
            .collect();
        let gpus: Vec<Value> = cards
            .iter()
            .map(|c| {
                json!({
                    "index": c.index, "name": c.name, "total_mib": c.total, "used_mib": c.used, "reserve_mib": reserve,
                    "free_for_models_mib": c.total.saturating_sub(reserve).saturating_sub(c.used),
                    "processes": c.processes,
                })
            })
            .collect();
        Ok(json!({
            "gpus": gpus,
            "vram_free_for_models_mib": free_mib(&cards, reserve),
            "vram_note": if cards.is_empty() { Some("no NVIDIA readings (nvidia-smi); loads are not budgeted") } else { None },
            "servers": servers,
            "unreachable": down,
        }))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoadArgs {
    model: String,
    server: Option<String>,
    keep_alive: Option<String>,
    #[serde(default)]
    evict: bool,
}

/// The Ollama server to act on: the one named (which must be one Orion
/// knows), or the first that `has` the model.
fn pick(policy: &Policy, server: Option<&str>, has: impl Fn(&Server) -> bool) -> Result<Server> {
    let known = candidates(policy);
    if let Some(url) = server {
        let url = normalize(url);
        ensure!(
            known.iter().any(|(u, _)| *u == url),
            "{url} is not an inference server Orion knows; add it to models.servers"
        );
        let s = probe(&url)?;
        ensure!(
            matches!(s.kind, Kind::Ollama(_)),
            "{url} is a {} server, which serves a fixed model: start, stop or switch it as a service (svc_control)",
            s.kind_name()
        );
        return Ok(s);
    }
    let (up, _) = servers(policy);
    let mut ollama = up.into_iter().filter(|s| matches!(s.kind, Kind::Ollama(_))).peekable();
    ensure!(ollama.peek().is_some(), "no Ollama server is running (model_status shows what is)");
    ollama.find(|s| has(s)).ok_or_else(|| anyhow!("no running Ollama server has that model (model_status lists them; model_pull fetches one)"))
}

struct LoadPlan {
    server: Server,
    model: Model,
    evict: Vec<String>,
    need: u64,
    free: Option<u64>,
}

fn plan(policy: &Policy, a: &LoadArgs) -> Result<LoadPlan> {
    let has = |s: &Server| s.available().is_ok_and(|ms| ms.iter().any(|m| same_model(&m.name, &a.model)));
    let server = pick(policy, a.server.as_deref(), has)?;
    let model = server
        .available()?
        .into_iter()
        .find(|m| same_model(&m.name, &a.model))
        .ok_or_else(|| anyhow!("{} doesn't have {}; model_pull fetches it", server.url, a.model))?;
    let loaded = server.loaded()?;
    let need = need_mib(model.size);
    let (evict, free) = if loaded.iter().any(|l| same_model(&l.name, &model.name)) || !is_local(&server.url) {
        (Vec::new(), None)
    } else {
        let free = free_mib(&cards(), policy.models.vram_reserve_mb);
        (plan_load(need, free, &loaded, &model.name, a.evict)?, free)
    };
    Ok(LoadPlan { server, model, evict, need, free })
}

pub struct ModelLoad;

impl Tool for ModelLoad {
    fn name(&self) -> &'static str {
        "model_load"
    }

    fn description(&self) -> &'static str {
        "Load a model into GPU memory on an Ollama server, so it answers without a cold start, and keep it \
         there for keep_alive (such as \"30m\", \"2h\", or \"-1\" for good). It is refused if it doesn't fit in \
         the VRAM budget, unless evict is set: then the models expiring soonest are unloaded first."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "model": { "type": "string", "description": "e.g. \"qwen3:8b\"" },
                "server": server_schema(),
                "keep_alive": { "type": "string", "description": "How long it stays loaded after last use. Default from the policy." },
                "evict": { "type": "boolean", "default": false, "description": "Unload other models if that is what it takes to fit." }
            },
            "required": ["model"],
            "additionalProperties": false
        })
    }

    fn precheck(&self, policy: &Policy, args: &Value) -> Result<()> {
        plan(policy, &parse_args(args.clone())?).map(drop)
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: LoadArgs = parse_args(args)?;
        let keep_alive = a.keep_alive.clone().unwrap_or_else(|| policy.models.keep_alive.clone());
        ensure!(
            keep_alive == "-1" || keep_alive.chars().all(|c| c.is_ascii_alphanumeric() || c == '.'),
            "keep_alive is a duration like \"30m\" or \"-1\", not {keep_alive:?}"
        );
        let p = plan(policy, &a)?;
        for name in &p.evict {
            p.server.keep_alive(name, json!(0), Duration::from_secs(60))?;
        }
        let ka: Value = keep_alive.parse::<i64>().map_or(json!(keep_alive), |n| json!(n));
        p.server.keep_alive(&p.model.name, ka, Duration::from_secs(900))?;
        let now = p.server.loaded()?.into_iter().find(|l| same_model(&l.name, &p.model.name));
        Ok(json!({
            "model": p.model.name,
            "server": p.server.url,
            "loaded": now.is_some(),
            "vram_mib": now.as_ref().map(|l| l.vram / MIB),
            "on_gpu_pct": now.as_ref().filter(|l| l.size > 0).map(|l| (l.vram * 100 / l.size).min(100)),
            "expires_at": now.map(|l| l.expires_at),
            "estimated_need_mib": p.need,
            "free_before_mib": p.free,
            "unloaded": p.evict,
        }))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnloadArgs {
    model: String,
    server: Option<String>,
}

pub struct ModelUnload;

impl Tool for ModelUnload {
    fn name(&self) -> &'static str {
        "model_unload"
    }

    fn description(&self) -> &'static str {
        "Unload a model from an Ollama server, freeing its GPU memory. It stays on disk and loads again on its next use."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "model": { "type": "string" }, "server": server_schema() },
            "required": ["model"],
            "additionalProperties": false
        })
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: UnloadArgs = parse_args(args)?;
        let is_loaded = |s: &Server| s.loaded().is_ok_and(|ls| ls.iter().any(|l| same_model(&l.name, &a.model)));
        let server = pick(policy, a.server.as_deref(), is_loaded)
            .map_err(|e| if a.server.is_none() { anyhow!("{} is not loaded on any Ollama server", a.model) } else { e })?;
        let Some(entry) = server.loaded()?.into_iter().find(|l| same_model(&l.name, &a.model)) else {
            return Ok(json!({ "model": a.model, "server": server.url, "was_loaded": false }));
        };
        server.keep_alive(&entry.name, json!(0), Duration::from_secs(60))?;
        Ok(json!({ "model": entry.name, "server": server.url, "was_loaded": true, "freed_vram_mib": entry.vram / MIB }))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PullArgs {
    model: String,
    server: Option<String>,
}

pub struct ModelPull;

impl Tool for ModelPull {
    fn name(&self) -> &'static str {
        "model_pull"
    }

    fn description(&self) -> &'static str {
        "Download a model onto an Ollama server from the Ollama library, such as \"qwen3:8b\". Large models take \
         a while; this returns when the download is done."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "model": { "type": "string" }, "server": server_schema() },
            "required": ["model"],
            "additionalProperties": false
        })
    }

    fn precheck(&self, _policy: &Policy, args: &Value) -> Result<()> {
        let a: PullArgs = parse_args(args.clone())?;
        let ok = a.model.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
            && a.model.len() <= 200
            && a.model.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:/".contains(c));
        ensure!(ok, "{:?} is not a model name", a.model);
        Ok(())
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        self.precheck(policy, &args)?;
        let a: PullArgs = parse_args(args)?;
        let server = pick(policy, a.server.as_deref(), |_| true)?;
        let r = server
            .http
            .post_json("/api/pull", &json!({ "model": a.model, "stream": false }), Duration::from_secs(4 * 3600))
            .with_context(|| format!("pulling {} on {}", a.model, server.url))?;
        let size = server.available()?.into_iter().find(|m| same_model(&m.name, &a.model)).map(|m| m.size);
        Ok(json!({
            "model": a.model,
            "server": server.url,
            "status": r["status"],
            "size_mib": size.map(|s| s / MIB),
            "needs_vram_mib": size.map(need_mib),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use super::*;

    fn loaded(name: &str, vram_mib: u64, expires: &str) -> Loaded {
        Loaded { name: name.into(), size: vram_mib * MIB, vram: vram_mib * MIB, expires_at: expires.into() }
    }

    #[test]
    fn budgets_vram() {
        let ls = [loaded("big:70b", 40000, "2026-10-19T12:30:00Z"), loaded("small:8b", 6000, "2026-10-19T12:05:00Z")];
        // Fits, or no readings to go by: nothing to unload.
        assert!(plan_load(5000, Some(8000), &ls, "x", false).unwrap().is_empty());
        assert!(plan_load(99999, None, &ls, "x", false).unwrap().is_empty());
        // Doesn't fit: says what is in the way, soonest-expiring first.
        let err = plan_load(10000, Some(2000), &ls, "x", false).unwrap_err().to_string();
        assert!(err.contains("unload small:8b (6000 MiB), big:70b (40000 MiB) first"), "{err}");
        // With evict, unloads only as much as it needs, in that order.
        assert_eq!(plan_load(7000, Some(2000), &ls, "x", true).unwrap(), ["small:8b"]);
        assert_eq!(plan_load(20000, Some(2000), &ls, "x", true).unwrap(), ["small:8b", "big:70b"]);
        assert!(plan_load(90000, Some(2000), &ls, "x", true).is_err());
        // The model itself never counts as in the way.
        assert!(plan_load(7000, Some(2000), &ls[1..], "small:8b", true).is_err());

        assert_eq!(need_mib(4 * 1024 * MIB), 4711 + 512);
        assert!(same_model("qwen3", "qwen3:latest") && !same_model("qwen3", "qwen3:8b"));
        assert_eq!(normalize("http://127.0.0.1:11434/v1/"), "http://127.0.0.1:11434");
    }

    /// A small stand-in for Ollama: two models on disk, loading and
    /// unloading through /api/generate.
    fn fake_ollama() -> (String, Arc<Mutex<Vec<Value>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(Mutex::new(Vec::<Value>::new()));
        let log = calls.clone();
        std::thread::spawn(move || {
            let mut resident: Vec<String> = vec!["small:8b".into()];
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                let path = line.split_whitespace().nth(1).unwrap_or_default().to_string();
                let mut length = 0;
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let reply = match path.as_str() {
                    "/api/version" => json!({ "version": "0.9.0" }),
                    "/api/tags" => json!({ "models": [
                        { "name": "small:8b", "size": 5 * 1024 * MIB, "details": { "parameter_size": "8B", "quantization_level": "Q4_K_M" } },
                        { "name": "qwen3:latest", "size": 2 * 1024 * MIB, "details": {} },
                    ]}),
                    "/api/ps" => json!({ "models": resident.iter().map(|n| json!({
                        "name": n, "size": 1000 * MIB, "size_vram": 1000 * MIB, "expires_at": "2026-10-19T12:00:00Z",
                    })).collect::<Vec<_>>() }),
                    "/api/generate" => {
                        let name = body["model"].as_str().unwrap_or_default().to_string();
                        resident.retain(|n| *n != name);
                        if body["keep_alive"] != 0 {
                            resident.push(name);
                        }
                        log.lock().unwrap().push(body);
                        json!({ "done": true })
                    }
                    _ => json!({ "error": "not found" }),
                };
                let text = reply.to_string();
                let mut s = stream;
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                    text.len()
                );
            }
        });
        (url, calls)
    }

    #[test]
    fn manages_an_ollama_server() {
        let (url, calls) = fake_ollama();
        let p = Policy::from_toml(&format!(
            "[models]\nservers = [\"{url}/v1\"]\ndiscover = false\nkeep_alive = \"10m\"\n[chat.local]\nbase_url = \"{url}/v1\"\n"
        ))
        .unwrap();

        let st = ModelStatus.call(&p, json!({})).unwrap();
        let s = &st["servers"][0];
        assert_eq!(s["kind"], "Ollama");
        assert_eq!(s["models"][0]["needs_vram_mib"], need_mib(5 * 1024 * MIB));
        assert_eq!(s["loaded"][0]["name"], "small:8b");
        assert_eq!(s["process"].as_str(), std::fs::read_to_string("/proc/self/comm").ok().as_deref().map(str::trim));

        let r = ModelLoad.call(&p, json!({ "model": "qwen3" })).unwrap();
        assert_eq!(r["model"], "qwen3:latest");
        assert_eq!(r["loaded"], true);
        assert_eq!(calls.lock().unwrap().last().unwrap()["keep_alive"], "10m");

        let r = ModelUnload.call(&p, json!({ "model": "small:8b" })).unwrap();
        assert_eq!(r["was_loaded"], true);
        assert_eq!(r["freed_vram_mib"], 1000);
        assert_eq!(calls.lock().unwrap().last().unwrap()["keep_alive"], 0);

        // Only servers Orion knows, only models the server has.
        let err = ModelLoad.precheck(&p, &json!({ "model": "qwen3", "server": "http://10.0.0.9:11434" })).unwrap_err();
        assert!(err.to_string().contains("not an inference server Orion knows"), "{err}");
        let err = ModelLoad.precheck(&p, &json!({ "model": "nope:1b" })).unwrap_err();
        assert!(err.to_string().contains("no running Ollama server has that model"), "{err}");
        assert!(ModelLoad.call(&p, json!({ "model": "qwen3", "keep_alive": "1h; rm" })).is_err());
        assert!(ModelPull.precheck(&p, &json!({ "model": "--insecure" })).is_err());
    }
}

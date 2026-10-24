//! Sound: the outputs (speakers, headphones, HDMI) and inputs (microphones)
//! of PipeWire or PulseAudio, their volume, and which one is the default.
//! Everything goes through `pactl`, which both speak, and its JSON output
//! (pactl 16 and later).

use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::session;
use crate::policy::Policy;
use crate::tool::{parse_args, Preapproval, Risk, Tool};

/// The loudest volume Orion sets: above 100% PipeWire amplifies, and
/// distorts soon after.
const MAX_VOLUME: i64 = 150;

/// One output or input.
#[derive(Debug, Clone, PartialEq)]
struct Device {
    name: String,
    description: String,
    /// Average over the channels, in percent.
    volume: i64,
    mute: bool,
    state: String,
    port: Option<String>,
}

impl Device {
    fn json(&self, default: &str) -> Value {
        json!({
            "name": self.name, "description": self.description, "volume": self.volume, "mute": self.mute,
            "state": self.state.to_lowercase(), "port": self.port, "default": self.name == default,
        })
    }
}

fn percent(v: &Value) -> Option<i64> {
    v["value_percent"].as_str()?.trim_end_matches('%').trim().parse().ok()
}

/// `pactl -f json list sinks|sources`, without the monitors of outputs
/// (they look like inputs, but only echo what plays).
fn parse_devices(text: &str) -> Result<Vec<Device>> {
    let list: Vec<Value> = serde_json::from_str(text).context("pactl's device list")?;
    Ok(list
        .iter()
        .filter(|d| d["properties"]["device.class"] != "monitor" && d["monitor_of_sink"].is_null())
        .filter_map(|d| {
            let volumes: Vec<i64> = d["volume"].as_object()?.values().filter_map(percent).collect();
            let port = d["active_port"].as_str().and_then(|p| {
                d["ports"].as_array()?.iter().find(|x| x["name"] == p).and_then(|x| x["description"].as_str()).map(String::from)
            });
            Some(Device {
                name: d["name"].as_str()?.to_string(),
                description: d["description"].as_str().unwrap_or_default().to_string(),
                volume: if volumes.is_empty() { 0 } else { volumes.iter().sum::<i64>() / volumes.len() as i64 },
                mute: d["mute"].as_bool().unwrap_or(false),
                state: d["state"].as_str().unwrap_or_default().to_string(),
                port,
            })
        })
        .collect())
}

/// What plays: `pactl -f json list sink-inputs`.
fn parse_streams(text: &str, outputs: &[Device], sinks: &Value) -> Vec<Value> {
    let Ok(list) = serde_json::from_str::<Vec<Value>>(text) else { return Vec::new() };
    let sink_name = |index: &Value| {
        sinks.as_array().and_then(|s| s.iter().find(|d| d["index"] == *index)).and_then(|d| d["name"].as_str()).map(String::from)
    };
    list.iter()
        .map(|s| {
            let volumes: Vec<i64> = s["volume"].as_object().map(|o| o.values().filter_map(percent).collect()).unwrap_or_default();
            let output = sink_name(&s["sink"]);
            json!({
                "app": s["properties"]["application.name"].as_str().or(s["properties"]["media.name"].as_str()),
                "volume": if volumes.is_empty() { Value::Null } else { json!(volumes.iter().sum::<i64>() / volumes.len() as i64) },
                "mute": s["mute"],
                "output": output.as_ref().and_then(|o| outputs.iter().find(|d| &d.name == o)).map(|d| d.description.clone()).or(output),
            })
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
enum Kind {
    #[default]
    Output,
    Input,
}

impl Kind {
    fn noun(self) -> &'static str {
        match self {
            Kind::Output => "sink",
            Kind::Input => "source",
        }
    }
}

struct Sound {
    server: String,
    default_output: String,
    default_input: String,
    outputs: Vec<Device>,
    inputs: Vec<Device>,
    sinks: Value,
}

fn pactl(args: &[&str]) -> Result<String> {
    session::output("pactl", args, 10).map_err(|e| {
        let e = format!("{e:#}");
        if e.contains("No such file") || e.contains("not found") {
            anyhow::anyhow!("pactl isn't installed (pulseaudio-utils, or libpulse on Arch)")
        } else if e.contains("Connection refused") || e.contains("Connection failure") {
            anyhow::anyhow!("no sound server is running in this session (PipeWire or PulseAudio)")
        } else {
            anyhow::anyhow!("{e}")
        }
    })
}

fn sound() -> Result<Sound> {
    let info: Value = serde_json::from_str(&pactl(&["-f", "json", "info"])?)
        .context("pactl printed no JSON: it needs to be version 16 or newer")?;
    let sinks_text = pactl(&["-f", "json", "list", "sinks"])?;
    Ok(Sound {
        server: info["server_name"].as_str().unwrap_or_default().to_string(),
        default_output: info["default_sink_name"].as_str().unwrap_or_default().to_string(),
        default_input: info["default_source_name"].as_str().unwrap_or_default().to_string(),
        outputs: parse_devices(&sinks_text)?,
        inputs: parse_devices(&pactl(&["-f", "json", "list", "sources"])?)?,
        sinks: serde_json::from_str(&sinks_text).unwrap_or(Value::Null),
    })
}

/// The device `target` names: its exact name, else the one whose
/// description contains it; "default" (or nothing) is the default.
fn find<'a>(devices: &'a [Device], default: &str, target: Option<&str>) -> Result<&'a Device> {
    let target = target.unwrap_or("default");
    let wanted = if target == "default" { default } else { target };
    if let Some(d) = devices.iter().find(|d| d.name == wanted) {
        return Ok(d);
    }
    let low = target.to_lowercase();
    let found: Vec<&Device> = devices.iter().filter(|d| d.description.to_lowercase().contains(&low)).collect();
    match found.as_slice() {
        [d] => Ok(d),
        [] => bail!("no device is called {target:?}; there are: {}", names(devices)),
        _ => bail!("{target:?} could be {}; name one", found.iter().map(|d| format!("{:?}", d.description)).collect::<Vec<_>>().join(" or ")),
    }
}

fn names(devices: &[Device]) -> String {
    devices.iter().map(|d| format!("{:?}", d.description)).collect::<Vec<_>>().join(", ")
}

pub struct AudioStatus;

impl Tool for AudioStatus {
    fn name(&self) -> &'static str {
        "audio_status"
    }

    fn description(&self) -> &'static str {
        "Sound on this machine: every output (speakers, headphones, HDMI, Bluetooth) and input (microphones) \
         with its volume, whether it is muted, and which is the default; and what is playing now."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    fn call(&self, _policy: &Policy, _args: Value) -> Result<Value> {
        let s = sound()?;
        let playing = parse_streams(&pactl(&["-f", "json", "list", "sink-inputs"]).unwrap_or_default(), &s.outputs, &s.sinks);
        Ok(json!({
            "server": s.server,
            "outputs": s.outputs.iter().map(|d| d.json(&s.default_output)).collect::<Vec<_>>(),
            "inputs": s.inputs.iter().map(|d| d.json(&s.default_input)).collect::<Vec<_>>(),
            "playing": playing,
        }))
    }
}

pub struct AudioSet;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetArgs {
    #[serde(default)]
    kind: Kind,
    device: Option<String>,
    volume: Option<i64>,
    change: Option<i64>,
    mute: Option<bool>,
    #[serde(default)]
    make_default: bool,
}

fn check(a: &SetArgs) -> Result<()> {
    ensure!(a.volume.is_none() || a.change.is_none(), "give volume or change, not both");
    ensure!(a.volume.is_some() || a.change.is_some() || a.mute.is_some() || a.make_default, "say what to change: volume, change, mute or make_default");
    if let Some(v) = a.volume {
        ensure!((0..=MAX_VOLUME).contains(&v), "volume goes from 0 to {MAX_VOLUME} (percent)");
    }
    if let Some(c) = a.change {
        ensure!((-100..=100).contains(&c), "change goes from -100 to 100 (percent)");
    }
    Ok(())
}

impl Tool for AudioSet {
    fn name(&self) -> &'static str {
        "audio_set"
    }

    fn description(&self) -> &'static str {
        "Change the sound: set an output's or input's volume (percent, or up and down by some), mute or unmute \
         it, or make it the default (where new sound plays, or which microphone records). The device is the \
         default unless named, by its name or part of its description (\"HDMI\", \"headphones\")."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kind": { "type": "string", "enum": ["output", "input"], "default": "output" },
                "device": { "type": "string", "description": "A device's name or part of its description. Default: the default device." },
                "volume": { "type": "integer", "minimum": 0, "maximum": MAX_VOLUME, "description": "Set the volume, in percent (above 100 amplifies)." },
                "change": { "type": "integer", "minimum": -100, "maximum": 100, "description": "Turn the volume up (or down, negative) by this many percent." },
                "mute": { "type": "boolean" },
                "make_default": { "type": "boolean", "description": "Make this device the default." }
            },
            "additionalProperties": false
        })
    }

    fn preapproval(&self, policy: &Policy, _args: &Value) -> Preapproval {
        policy.desktop.preapproval("volume")
    }

    fn precheck(&self, _policy: &Policy, args: &Value) -> Result<()> {
        check(&parse_args(args.clone())?)
    }

    fn call(&self, _policy: &Policy, args: Value) -> Result<Value> {
        let a: SetArgs = parse_args(args)?;
        check(&a)?;
        let s = sound()?;
        let (devices, default) = match a.kind {
            Kind::Output => (&s.outputs, &s.default_output),
            Kind::Input => (&s.inputs, &s.default_input),
        };
        let d = find(devices, default, a.device.as_deref())?.clone();
        let noun = a.kind.noun();
        if let Some(target) = a.volume.or(a.change.map(|c| d.volume + c)) {
            let v = target.clamp(0, MAX_VOLUME);
            pactl(&[&format!("set-{noun}-volume"), &d.name, &format!("{v}%")])?;
        }
        if let Some(m) = a.mute {
            pactl(&[&format!("set-{noun}-mute"), &d.name, if m { "1" } else { "0" }])?;
        }
        if a.make_default {
            pactl(&[&format!("set-default-{noun}"), &d.name])?;
        }
        let after = sound()?;
        let (devices, default) = match a.kind {
            Kind::Output => (&after.outputs, &after.default_output),
            Kind::Input => (&after.inputs, &after.default_input),
        };
        let now = devices.iter().find(|x| x.name == d.name).context("the device went away")?;
        Ok(now.json(default))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SINKS: &str = r#"[
      {"index":45,"state":"RUNNING","name":"alsa_output.pci-0000_00_1f.3.analog-stereo","description":"Built-in Audio Analog Stereo",
       "mute":false,"volume":{"front-left":{"value":42598,"value_percent":"65%","db":"-11.23 dB"},"front-right":{"value":45875,"value_percent":"70%","db":"-9.29 dB"}},
       "properties":{"device.class":"sound"},"ports":[{"name":"analog-output-headphones","description":"Headphones"},{"name":"analog-output-speaker","description":"Speakers"}],
       "active_port":"analog-output-headphones"},
      {"index":52,"state":"SUSPENDED","name":"alsa_output.hdmi","description":"HDMI / DisplayPort","mute":true,
       "volume":{"mono":{"value_percent":"100%"}},"properties":{},"ports":[],"active_port":null}
    ]"#;

    #[test]
    fn reads_devices_and_streams() {
        let out = parse_devices(SINKS).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].volume, out[0].port.as_deref(), out[1].mute), (67, Some("Headphones"), true));
        let sources = r#"[{"index":1,"name":"alsa_output.hdmi.monitor","description":"Monitor of HDMI","monitor_of_sink":52,"volume":{},"properties":{"device.class":"monitor"}},
                          {"index":2,"name":"alsa_input.mic","description":"Built-in Microphone","mute":false,"volume":{"mono":{"value_percent":"40%"}},"properties":{}}]"#;
        let ins = parse_devices(sources).unwrap();
        assert_eq!(ins.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["alsa_input.mic"]);
        let streams = r#"[{"sink":45,"mute":false,"volume":{"front-left":{"value_percent":"100%"}},"properties":{"application.name":"Firefox"}}]"#;
        let playing = parse_streams(streams, &out, &serde_json::from_str(SINKS).unwrap());
        assert_eq!(playing[0]["app"], "Firefox");
        assert_eq!(playing[0]["output"], "Built-in Audio Analog Stereo");

        let default = &out[0].name;
        assert_eq!(find(&out, default, None).unwrap().name, *default);
        assert_eq!(find(&out, default, Some("hdmi")).unwrap().name, "alsa_output.hdmi");
        assert!(format!("{:#}", find(&out, default, Some("bluetooth")).unwrap_err()).contains("Built-in Audio"));

        let a = |v: Value| check(&serde_json::from_value(v).unwrap());
        assert!(a(json!({})).is_err());
        assert!(a(json!({ "volume": 50, "change": 5 })).is_err());
        assert!(a(json!({ "volume": 500 })).is_err());
        assert!(a(json!({ "change": -10, "mute": false })).is_ok());
    }
}
